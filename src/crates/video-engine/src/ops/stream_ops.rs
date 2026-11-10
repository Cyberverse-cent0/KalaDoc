//! Stream-level container surgery: select/map streams, merge audio
//! from a separate source, and edit/strip metadata including cover
//! art.
//!
//! All operations are stream-copy remuxes (`ffmpeg -c copy`) except
//! [`merge_av`], which copies the video stream but re-encodes the
//! audio track into a codec the target container accepts. Stream
//! selection uses explicit `-map 0:{index}` arguments built from
//! `ffprobe`'s per-stream `index` field, so demuxer default-selection
//! quirks (dropped attached pictures, non-default audio) never apply.

use std::collections::BTreeMap;
use std::path::PathBuf;

use serde_json::Value;

use crate::crates::core::engine::EngineLimits;
use crate::crates::video_engine::error::{VideoError, VideoResult};
use crate::crates::video_engine::model::StreamFilter;
use crate::crates::video_engine::ops::probe::probe;
use crate::crates::video_engine::ops::remux::REMUX_TARGETS;
use crate::crates::video_engine::ops::{
    COMMAND_TIMEOUT, FFMPEG, ProcOutput, TempDir, check_input, check_limits, checked_ext,
    encoder_available, ext_for_format, failure_detail, find_tool, normalize_ext, probe_json,
    read_output, run_tool, write_input,
};

/// Explicit ffmpeg muxer name for `ext` — extension guessing is
/// ambiguous for several targets (`.m4a` could pick `mp4`, `.aac` has
/// no unique muxer, `.ts` must be `mpegts`), so we always pass `-f`.
fn muxer_for(ext: &str) -> &str {
    match ext {
        "mkv" => "matroska",
        "m4a" => "ipod",
        "aac" => "adts",
        "ts" | "mpegts" => "mpegts",
        other => other,
    }
}

/// Whether a non-zero ffmpeg exit looks like an honest "this codec
/// cannot go in that container" rejection rather than a tool fault.
fn is_container_rejection(detail: &str) -> bool {
    let lower = detail.to_lowercase();
    lower.contains("could not find tag")
        || lower.contains("muxer")
        || lower.contains("codec")
        || lower.contains("could not write header")
        || lower.contains("invalid argument")
}

/// Map a failed ffmpeg run to `InvalidInput` when it is a
/// container/codec compatibility rejection, otherwise keep the honest
/// `ExternalTool`.
fn remux_failure(output: &ProcOutput) -> VideoError {
    let detail = failure_detail(output, "ffmpeg exited unsuccessfully");
    if is_container_rejection(&detail) {
        VideoError::InvalidInput(format!("ffmpeg rejected the stream mapping: {detail}"))
    } else {
        VideoError::ExternalTool {
            tool: FFMPEG.to_string(),
            detail,
        }
    }
}

/// Write `input` into `dir` under a sniffable extension derived from
/// the probed container name, then return its path.
fn stage_input(dir: &TempDir, name: &str, input: &[u8], json: &Value) -> VideoResult<PathBuf> {
    let ext = format_name_of(json)
        .as_deref()
        .and_then(ext_for_format)
        .unwrap_or("bin");
    write_input(dir, &format!("{name}.{ext}"), input)
}

/// The probed `format_name` (e.g. `"mov,mp4,m4a,..."`) of an input.
fn format_name_of(json: &Value) -> Option<String> {
    json.get("format")
        .and_then(|f| f.get("format_name"))
        .and_then(Value::as_str)
        .map(str::to_string)
}

fn streams_of(json: &Value) -> &[Value] {
    json.get("streams")
        .and_then(Value::as_array)
        .map(Vec::as_slice)
        .unwrap_or(&[])
}

fn stream_codec_type(s: &Value) -> &str {
    s.get("codec_type").and_then(Value::as_str).unwrap_or("")
}

/// ffprobe's absolute stream index — the number `-map 0:{index}`
/// expects.
fn stream_index(s: &Value) -> Option<u64> {
    s.get("index").and_then(Value::as_u64)
}

/// Whether the stream is an attached picture (cover art exposed as a
/// video stream). Such streams must never win "the video stream" slot.
fn is_attached_pic(s: &Value) -> bool {
    s.get("disposition")
        .and_then(|d| d.get("attached_pic"))
        .and_then(Value::as_i64)
        == Some(1)
}

/// Absolute indices of every stream of `codec_type`, in demuxer
/// order. `skip_attached_pic` excludes cover-art pseudo-video streams.
fn indices_of(streams: &[Value], codec_type: &str, skip_attached_pic: bool) -> Vec<u64> {
    streams
        .iter()
        .filter(|s| stream_codec_type(s) == codec_type)
        .filter(|s| !(skip_attached_pic && is_attached_pic(s)))
        .filter_map(stream_index)
        .collect()
}

/// Whether the input carries at least one real video stream (an
/// attached picture alone does not count).
fn has_real_video(json: &Value) -> bool {
    streams_of(json)
        .iter()
        .any(|s| stream_codec_type(s) == "video" && !is_attached_pic(s))
}

/// Whether the input carries at least one audio stream.
fn has_audio(json: &Value) -> bool {
    streams_of(json)
        .iter()
        .any(|s| stream_codec_type(s) == "audio")
}

/// Resolve an ordinal filter against the available per-type indices.
/// `None` keeps every stream of that type; `Some(list)` keeps only the
/// listed 0-based ordinals *within that type* (translated to absolute
/// stream indices). Out-of-range ordinals are `InvalidInput`.
fn resolve_ordinals(
    ordinals: Option<&Vec<u32>>,
    available: &[u64],
    what: &str,
) -> VideoResult<Vec<u64>> {
    match ordinals {
        None => Ok(available.to_vec()),
        Some(list) => {
            for &o in list {
                if o as usize >= available.len() {
                    return Err(VideoError::InvalidInput(format!(
                        "{what} ordinal {o} is out of range: input has {} {what} stream(s)",
                        available.len()
                    )));
                }
            }
            Ok(list.iter().map(|&o| available[o as usize]).collect())
        }
    }
}

/// Keep only the streams allowed by `filter`, remuxing into
/// `target_ext` (stream copy — codecs must be compatible with the
/// target container).
///
/// Mapping rules: at most the first real video stream (cover-art
/// attached pictures never count), then audio, then subtitle streams
/// selected by 0-based per-type ordinals (`None` = all of that type).
/// Attachment and data streams are *never* mapped: [`StreamFilter`]
/// has no field that can name them, so `drop_attachments` is accepted
/// for API compatibility but selection is opt-in regardless —
/// attachments are always dropped. A selection with zero streams is
/// `InvalidInput`; illegal codec/container pairs surface ffmpeg's own
/// muxer error as `InvalidInput`.
pub(crate) fn select_streams(
    input: &[u8],
    filter: &StreamFilter,
    target_ext: &str,
    limits: &EngineLimits,
) -> VideoResult<Vec<u8>> {
    let input_len = check_input(input)?;
    let ext = checked_ext(target_ext, REMUX_TARGETS, "select_streams")?;
    let program = find_tool(FFMPEG)?;

    let info = probe(input)?;
    check_limits(&info, input_len, limits, false)?;

    let dir = TempDir::new("select")?;
    let json = {
        let p = write_input(&dir, "input.bin", input)?;
        probe_json(&p)?
    };
    let streams = streams_of(&json);

    let video = indices_of(streams, "video", true);
    let audio = indices_of(streams, "audio", false);
    let subs = indices_of(streams, "subtitle", false);

    let mut maps: Vec<u64> = Vec::new();
    if filter.keep_video {
        if let Some(&idx) = video.first() {
            maps.push(idx);
        }
    }
    for idx in resolve_ordinals(filter.audio_ordinals.as_ref(), &audio, "audio")? {
        if !maps.contains(&idx) {
            maps.push(idx);
        }
    }
    for idx in resolve_ordinals(filter.subtitle_ordinals.as_ref(), &subs, "subtitle")? {
        if !maps.contains(&idx) {
            maps.push(idx);
        }
    }
    if maps.is_empty() {
        return Err(VideoError::InvalidInput(
            "stream filter selects no streams".to_string(),
        ));
    }

    let input_path = stage_input(&dir, "input", input, &json)?;
    let out_path = dir.file(&format!("out.{ext}"));

    let mut args = vec![
        "-y".to_string(),
        "-v".to_string(),
        "error".to_string(),
        "-i".to_string(),
        input_path.to_string_lossy().into_owned(),
    ];
    // Map order: video first, then audio, then subtitles — within each
    // group indices stay in demuxer (ascending) order.
    for idx in maps {
        args.extend(["-map".to_string(), format!("0:{idx}")]);
    }
    args.extend([
        "-c".to_string(),
        "copy".to_string(),
        "-f".to_string(),
        muxer_for(&ext).to_string(),
    ]);
    args.push(out_path.to_string_lossy().into_owned());

    let output = run_tool(FFMPEG, &program, &args, COMMAND_TIMEOUT)?;
    if !output.status.success() {
        return Err(remux_failure(&output));
    }
    read_output(&out_path)
}

/// Audio encoder candidates per target container, in preference order.
/// The first encoder this ffmpeg build actually has wins; containers
/// with no usable encoder fail honestly rather than emitting a codec
/// the muxer will reject.
fn audio_codec_candidates(ext: &str) -> &'static [&'static str] {
    match ext {
        // ISO-BMFF family: AAC is the only broadly-safe choice.
        "mp4" | "mov" | "m4a" => &["aac"],
        // Matroska accepts almost anything; Opus preferred, AAC fine.
        "mkv" => &["libopus", "aac"],
        // WebM mandates VP8/VP9/AV1 video + Vorbis/Opus audio.
        "webm" => &["libopus", "libvorbis"],
        // Ogg family: Opus preferred, FLAC/Vorbis also legal.
        "ogg" | "opus" | "oga" => &["libopus", "flac", "libvorbis"],
        "wav" => &["pcm_s16le"],
        "flac" => &["flac"],
        "mp3" => &["libmp3lame"],
        "adts" | "aac" => &["aac"],
        "ts" | "mpegts" => &["aac", "mp2"],
        "flv" => &["aac", "libmp3lame"],
        "avi" => &["libmp3lame", "pcm_s16le"],
        _ => &["aac"],
    }
}

/// Pick the first available audio encoder for `ext`.
fn audio_codec_for(ext: &str) -> VideoResult<String> {
    let candidates = audio_codec_candidates(ext);
    for cand in candidates {
        if encoder_available(cand) {
            return Ok((*cand).to_string());
        }
    }
    Err(VideoError::ExternalTool {
        tool: FFMPEG.to_string(),
        detail: format!(
            "no usable audio encoder for '{ext}' output (tried: {})",
            candidates.join(", ")
        ),
    })
}

/// Merge `audio` (any audio-bearing file) into `video` as its audio
/// track — the video stream(s) are copied bit-for-bit, the audio track
/// is re-encoded to a codec the target container accepts. Any audio
/// already inside `video` is dropped (only `0:v` is mapped). Output
/// container follows `target_ext`, or — when empty or identical to the
/// video's own container extension — the video input's probed family.
pub(crate) fn merge_av(
    video: &[u8],
    audio: &[u8],
    target_ext: &str,
    limits: &EngineLimits,
) -> VideoResult<Vec<u8>> {
    let video_len = check_input(video)?;
    let audio_len = check_input(audio)?;
    let program = find_tool(FFMPEG)?;

    let vinfo = probe(video)?;
    check_limits(&vinfo, video_len, limits, false)?;
    let ainfo = probe(audio)?;
    check_limits(&ainfo, audio_len, limits, false)?;

    let dir = TempDir::new("mergeav")?;
    let vjson = {
        let p = write_input(&dir, "video.bin", video)?;
        probe_json(&p)?
    };
    let ajson = {
        let p = write_input(&dir, "audio.bin", audio)?;
        probe_json(&p)?
    };

    if !has_real_video(&vjson) {
        return Err(VideoError::InvalidInput(
            "merge_av requires a video stream in the video input".to_string(),
        ));
    }
    if !has_audio(&ajson) {
        return Err(VideoError::InvalidInput(
            "merge_av requires an audio stream in the audio input".to_string(),
        ));
    }

    // Resolve the output container: an explicit target wins unless it
    // merely restates the video's own extension; otherwise derive from
    // the probed format family.
    let video_ext = format_name_of(&vjson).as_deref().and_then(ext_for_format);
    let ext = {
        let norm = normalize_ext(target_ext);
        if norm.is_empty() || Some(norm.as_str()) == video_ext {
            video_ext
                .ok_or_else(|| {
                    VideoError::InvalidInput(
                        "cannot determine output container for merge_av".to_string(),
                    )
                })?
                .to_string()
        } else {
            checked_ext(&norm, REMUX_TARGETS, "merge_av")?
        }
    };

    let audio_codec = audio_codec_for(&ext)?;

    let video_path = stage_input(&dir, "video", video, &vjson)?;
    let audio_path = stage_input(&dir, "audio", audio, &ajson)?;
    let out_path = dir.file(&format!("out.{ext}"));

    let mut args = vec![
        "-y".to_string(),
        "-v".to_string(),
        "error".to_string(),
        "-i".to_string(),
        video_path.to_string_lossy().into_owned(),
        "-i".to_string(),
        audio_path.to_string_lossy().into_owned(),
        // Video streams of input 0 only — its existing audio is
        // dropped. All audio streams of input 1 are kept. `?` makes
        // either side optional at the ffmpeg level (we pre-validated).
        "-map".to_string(),
        "0:v?".to_string(),
        "-map".to_string(),
        "1:a?".to_string(),
        "-c:v".to_string(),
        "copy".to_string(),
        "-c:a".to_string(),
        audio_codec,
        "-f".to_string(),
        muxer_for(&ext).to_string(),
    ];
    if ext == "mp4" || ext == "mov" {
        args.extend(["-movflags".to_string(), "+faststart".to_string()]);
    }
    args.push(out_path.to_string_lossy().into_owned());

    let output = run_tool(FFMPEG, &program, &args, COMMAND_TIMEOUT)?;
    if !output.status.success() {
        return Err(remux_failure(&output));
    }
    read_output(&out_path)
}

/// Maximum length of a metadata value accepted by [`set_metadata`].
const MAX_TAG_VALUE_CHARS: usize = 256;

/// Whether `key` is a safe ffmpeg metadata key: ASCII alphanumerics
/// plus `_`, `.`, `-`. Anything else (whitespace, `=`, quotes, ...) is
/// rejected so a crafted key cannot inject extra ffmpeg options.
fn valid_tag_key(key: &str) -> bool {
    !key.is_empty()
        && key
            .chars()
            .all(|c| c.is_ascii_alphanumeric() || matches!(c, '_' | '.' | '-'))
}

/// Set container/stream tags. Keys are normalized lowercase;
/// reserved keys (`encoder`) may be rejected. Streams' existing tags
/// are preserved unless overridden.
///
/// Implementation: `-map_metadata 0` keeps the input's existing global
/// tags (ffmpeg's default anyway, made explicit), then one
/// `-metadata key=value` pair per requested tag overrides/sets the
/// format-level tag. Keys must match `[a-zA-Z0-9_.-]+` and values are
/// capped at 256 chars — violations are `InvalidInput`. All streams
/// are stream-copied (`-map 0 -c copy`) into the input's own container
/// family; unrecognized containers are `InvalidInput`.
pub(crate) fn set_metadata(
    input: &[u8],
    tags: &BTreeMap<String, String>,
    limits: &EngineLimits,
) -> VideoResult<Vec<u8>> {
    let input_len = check_input(input)?;
    let program = find_tool(FFMPEG)?;

    let info = probe(input)?;
    check_limits(&info, input_len, limits, false)?;

    for (k, v) in tags {
        if !valid_tag_key(k) {
            return Err(VideoError::InvalidInput(format!(
                "invalid metadata key '{k}': allowed characters are [a-zA-Z0-9_.-]"
            )));
        }
        if v.chars().count() > MAX_TAG_VALUE_CHARS {
            return Err(VideoError::InvalidInput(format!(
                "metadata value for '{k}' exceeds {MAX_TAG_VALUE_CHARS} characters"
            )));
        }
    }

    let dir = TempDir::new("setmeta")?;
    let json = {
        let p = write_input(&dir, "input.bin", input)?;
        probe_json(&p)?
    };
    let ext = format_name_of(&json)
        .as_deref()
        .and_then(ext_for_format)
        .ok_or_else(|| {
            VideoError::InvalidInput(
                "cannot determine output container for set_metadata".to_string(),
            )
        })?;
    let input_path = stage_input(&dir, "input", input, &json)?;
    let out_path = dir.file(&format!("out.{ext}"));

    let mut args = vec![
        "-y".to_string(),
        "-v".to_string(),
        "error".to_string(),
        "-i".to_string(),
        input_path.to_string_lossy().into_owned(),
        "-map".to_string(),
        "0".to_string(),
        // Preserve existing global tags, then apply overrides. Later
        // -metadata pairs win over -map_metadata for the same key.
        "-map_metadata".to_string(),
        "0".to_string(),
    ];
    for (k, v) in tags {
        args.extend(["-metadata".to_string(), format!("{k}={v}")]);
    }
    args.extend([
        "-c".to_string(),
        "copy".to_string(),
        "-f".to_string(),
        muxer_for(ext).to_string(),
    ]);
    if ext == "mp4" || ext == "mov" {
        args.extend(["-movflags".to_string(), "+faststart".to_string()]);
    }
    args.push(out_path.to_string_lossy().into_owned());

    let output = run_tool(FFMPEG, &program, &args, COMMAND_TIMEOUT)?;
    if !output.status.success() {
        return Err(remux_failure(&output));
    }
    read_output(&out_path)
}

/// Strip all metadata (container tags and per-stream tags) while
/// keeping media streams; remuxes into `target_ext`.
///
/// `-map_metadata -1` clears global tag copying; the `-map_metadata:s:*`
/// variants clear per-stream-type metadata (video, audio, subtitle,
/// data). `-1` as the metadata source means "no input", i.e. drop.
pub(crate) fn strip_metadata(
    input: &[u8],
    target_ext: &str,
    limits: &EngineLimits,
) -> VideoResult<Vec<u8>> {
    let input_len = check_input(input)?;
    let ext = checked_ext(target_ext, REMUX_TARGETS, "strip_metadata")?;
    let program = find_tool(FFMPEG)?;

    let info = probe(input)?;
    check_limits(&info, input_len, limits, false)?;

    let dir = TempDir::new("stripmeta")?;
    let json = {
        let p = write_input(&dir, "input.bin", input)?;
        probe_json(&p)?
    };
    let input_path = stage_input(&dir, "input", input, &json)?;
    let out_path = dir.file(&format!("out.{ext}"));

    let mut args = vec![
        "-y".to_string(),
        "-v".to_string(),
        "error".to_string(),
        "-i".to_string(),
        input_path.to_string_lossy().into_owned(),
        "-map".to_string(),
        "0".to_string(),
        "-map_metadata".to_string(),
        "-1".to_string(),
        "-map_metadata:s:v".to_string(),
        "-1".to_string(),
        "-map_metadata:s:a".to_string(),
        "-1".to_string(),
        "-map_metadata:s:s".to_string(),
        "-1".to_string(),
        "-map_metadata:s:d".to_string(),
        "-1".to_string(),
        // bitexact suppresses the muxer-written `encoder=Lavf...` tag
        // so genuinely no metadata survives.
        "-fflags".to_string(),
        "+bitexact".to_string(),
        "-c".to_string(),
        "copy".to_string(),
        "-f".to_string(),
        muxer_for(&ext).to_string(),
    ];
    if ext == "mp4" || ext == "mov" {
        args.extend(["-movflags".to_string(), "+faststart".to_string()]);
    }
    args.push(out_path.to_string_lossy().into_owned());

    let output = run_tool(FFMPEG, &program, &args, COMMAND_TIMEOUT)?;
    if !output.status.success() {
        return Err(remux_failure(&output));
    }
    read_output(&out_path)
}

/// Image magic-byte sniff for the cover input. Only PNG and JPEG are
/// accepted — those are the two formats every attached-picture
/// container (ID3v2 APIC, FLAC PICTURE, MP4 covr, Ogg annotations)
/// handles reliably.
fn sniff_cover_ext(cover: &[u8]) -> Option<&'static str> {
    if cover.starts_with(b"\x89PNG\r\n\x1a\n") {
        Some("png")
    } else if cover.starts_with(b"\xff\xd8\xff") {
        Some("jpg")
    } else {
        None
    }
}

/// Muxer to use for cover-art output, chosen from the input's probed
/// `format_name` tokens. Only audio containers that support attached
/// pictures qualify; video containers are `InvalidInput`.
fn cover_muxer(format_name: &str) -> Option<&'static str> {
    let lower = format_name.to_ascii_lowercase();
    let tokens: Vec<&str> = lower.split(',').map(str::trim).collect();
    for t in &tokens {
        match *t {
            "mp3" => return Some("mp3"),
            "flac" => return Some("flac"),
            "ogg" => return Some("ogg"),
            "opus" => return Some("opus"),
            _ => {}
        }
    }
    // ISO-BMFF family is only eligible for audio-only files (m4a);
    // the caller additionally checks there is no video stream.
    const BMFF: &[&str] = &["mov", "mp4", "m4a", "3gp", "3g2", "mj2"];
    if tokens.iter().any(|t| BMFF.contains(t)) {
        return Some("mp4");
    }
    None
}

/// Attach or replace the cover art (`cover` is a PNG or JPEG image).
///
/// Supported inputs are audio containers with attached-picture
/// support: MP3 (ID3v2 APIC), FLAC (METADATA_BLOCK_PICTURE),
/// Ogg/Opus, and audio-only ISO-BMFF files (m4a → covr). Video
/// containers — and unrecognized formats — are `InvalidInput`. The
/// cover is mapped as a second input and stream-copied with the
/// `attached_pic` disposition so muxers store it as artwork rather
/// than a playable video track.
pub(crate) fn set_cover_art(
    input: &[u8],
    cover: &[u8],
    limits: &EngineLimits,
) -> VideoResult<Vec<u8>> {
    let input_len = check_input(input)?;
    check_input(cover)?;
    let program = find_tool(FFMPEG)?;

    let info = probe(input)?;
    check_limits(&info, input_len, limits, false)?;
    if info.has_video {
        return Err(VideoError::InvalidInput(
            "set_cover_art supports audio containers only (mp3, flac, ogg, \
             opus, m4a); the input already contains a video stream"
                .to_string(),
        ));
    }
    let Some(cover_ext) = sniff_cover_ext(cover) else {
        return Err(VideoError::InvalidInput(
            "cover art must be a PNG or JPEG image".to_string(),
        ));
    };

    let dir = TempDir::new("coverart")?;
    let json = {
        let p = write_input(&dir, "input.bin", input)?;
        probe_json(&p)?
    };
    let Some(muxer) = format_name_of(&json).as_deref().and_then(cover_muxer) else {
        return Err(VideoError::InvalidInput(
            "set_cover_art supports mp3, flac, ogg, opus and audio-only mp4/m4a \
             containers"
                .to_string(),
        ));
    };

    let input_path = stage_input(&dir, "input", input, &json)?;
    let cover_path = write_input(&dir, &format!("cover.{cover_ext}"), cover)?;
    let out_ext = match muxer {
        "matroska" => "mkv",
        "ipod" => "m4a",
        other => other,
    };
    let out_path = dir.file(&format!("out.{out_ext}"));

    let mut args = vec![
        "-y".to_string(),
        "-v".to_string(),
        "error".to_string(),
        "-i".to_string(),
        input_path.to_string_lossy().into_owned(),
        "-i".to_string(),
        cover_path.to_string_lossy().into_owned(),
        // Everything from the audio input plus the cover image.
        "-map".to_string(),
        "0".to_string(),
        "-map".to_string(),
        "1".to_string(),
        "-c".to_string(),
        "copy".to_string(),
        // Mark the image stream as attached art so muxers store it as
        // a picture block / covr atom instead of a video track.
        "-disposition:v".to_string(),
        "attached_pic".to_string(),
    ];
    if muxer == "mp3" {
        // ID3v2.3 writes APIC frames most compatibly.
        args.extend(["-id3v2_version".to_string(), "3".to_string()]);
    }
    args.extend([
        "-f".to_string(),
        muxer.to_string(),
        out_path.to_string_lossy().into_owned(),
    ]);

    let output = run_tool(FFMPEG, &program, &args, COMMAND_TIMEOUT)?;
    if !output.status.success() {
        return Err(remux_failure(&output));
    }
    read_output(&out_path)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::crates::video_engine::ops::{FFPROBE, find_tool};
    use std::fs;

    fn tools_available() -> bool {
        find_tool(FFMPEG).is_ok() && find_tool(FFPROBE).is_ok()
    }

    /// Tiny 1s H.264+AAC mp4 via lavfi (has both audio and video).
    fn make_av_fixture() -> Option<Vec<u8>> {
        let program = find_tool(FFMPEG).ok()?;
        let enc = crate::crates::video_engine::ops::h264_encoder()?;
        let dir = TempDir::new("so-test-av").ok()?;
        let out = dir.file("fixture.mp4");
        let args = vec![
            "-y".to_string(),
            "-v".to_string(),
            "error".to_string(),
            "-f".to_string(),
            "lavfi".to_string(),
            "-i".to_string(),
            "testsrc=duration=1:size=160x120:rate=10".to_string(),
            "-f".to_string(),
            "lavfi".to_string(),
            "-i".to_string(),
            "sine=frequency=440:duration=1".to_string(),
            "-c:v".to_string(),
            enc.to_string(),
            "-pix_fmt".to_string(),
            "yuv420p".to_string(),
            "-c:a".to_string(),
            "aac".to_string(),
            "-shortest".to_string(),
            out.to_string_lossy().into_owned(),
        ];
        let output = run_tool(FFMPEG, &program, &args, COMMAND_TIMEOUT).ok()?;
        if !output.status.success() {
            return None;
        }
        fs::read(out).ok()
    }

    /// Tiny 1s mono WAV (audio-only input for merge_av).
    fn make_wav_fixture() -> Option<Vec<u8>> {
        let program = find_tool(FFMPEG).ok()?;
        let dir = TempDir::new("so-test-wav").ok()?;
        let out = dir.file("tone.wav");
        let args = vec![
            "-y".to_string(),
            "-v".to_string(),
            "error".to_string(),
            "-f".to_string(),
            "lavfi".to_string(),
            "-i".to_string(),
            "sine=frequency=880:duration=1".to_string(),
            "-c:a".to_string(),
            "pcm_s16le".to_string(),
            out.to_string_lossy().into_owned(),
        ];
        let output = run_tool(FFMPEG, &program, &args, COMMAND_TIMEOUT).ok()?;
        if !output.status.success() {
            return None;
        }
        fs::read(out).ok()
    }

    /// Tiny MP3 via lavfi (needs libmp3lame); `None` when unavailable.
    fn make_mp3_fixture() -> Option<Vec<u8>> {
        let program = find_tool(FFMPEG).ok()?;
        if !encoder_available("libmp3lame") {
            return None;
        }
        let dir = TempDir::new("so-test-mp3").ok()?;
        let out = dir.file("tone.mp3");
        let args = vec![
            "-y".to_string(),
            "-v".to_string(),
            "error".to_string(),
            "-f".to_string(),
            "lavfi".to_string(),
            "-i".to_string(),
            "sine=frequency=660:duration=1".to_string(),
            "-c:a".to_string(),
            "libmp3lame".to_string(),
            out.to_string_lossy().into_owned(),
        ];
        let output = run_tool(FFMPEG, &program, &args, COMMAND_TIMEOUT).ok()?;
        if !output.status.success() {
            return None;
        }
        fs::read(out).ok()
    }

    /// 64x64 red PNG via lavfi.
    fn make_png_fixture() -> Option<Vec<u8>> {
        let program = find_tool(FFMPEG).ok()?;
        let dir = TempDir::new("so-test-png").ok()?;
        let out = dir.file("cover.png");
        let args = vec![
            "-y".to_string(),
            "-v".to_string(),
            "error".to_string(),
            "-f".to_string(),
            "lavfi".to_string(),
            "-i".to_string(),
            "color=red:size=64x64:duration=0.1:rate=1".to_string(),
            "-frames:v".to_string(),
            "1".to_string(),
            out.to_string_lossy().into_owned(),
        ];
        let output = run_tool(FFMPEG, &program, &args, COMMAND_TIMEOUT).ok()?;
        if !output.status.success() {
            return None;
        }
        fs::read(out).ok()
    }

    fn no_video_filter() -> StreamFilter {
        StreamFilter {
            keep_video: false,
            audio_ordinals: None,
            subtitle_ordinals: None,
            drop_attachments: true,
        }
    }

    #[test]
    fn select_streams_drop_video_keeps_audio() {
        if !tools_available() {
            return;
        }
        let Some(fixture) = make_av_fixture() else {
            return;
        };
        let out = select_streams(
            &fixture,
            &no_video_filter(),
            "mp4",
            &EngineLimits::default(),
        )
        .unwrap();
        let info = probe(&out).unwrap();
        assert!(info.has_audio, "expected audio in output");
        assert!(!info.has_video, "expected no video in output");
    }

    #[test]
    fn select_streams_bogus_ordinal_is_invalid_input() {
        if !tools_available() {
            return;
        }
        let Some(fixture) = make_av_fixture() else {
            return;
        };
        let filter = StreamFilter {
            keep_video: true,
            audio_ordinals: Some(vec![42]),
            subtitle_ordinals: None,
            drop_attachments: true,
        };
        assert!(matches!(
            select_streams(&fixture, &filter, "mp4", &EngineLimits::default()),
            Err(VideoError::InvalidInput(_))
        ));
    }

    #[test]
    fn merge_av_combines_video_and_wav_audio() {
        if !tools_available() {
            return;
        }
        let (Some(video), Some(audio)) = (make_av_fixture(), make_wav_fixture()) else {
            return;
        };
        let out = merge_av(&video, &audio, "", &EngineLimits::default()).unwrap();
        let info = probe(&out).unwrap();
        assert!(info.has_video, "expected video stream in merged output");
        assert!(info.has_audio, "expected audio stream in merged output");
        assert_eq!(info.acodec.as_deref(), Some("aac"));
    }

    #[test]
    fn merge_av_rejects_audio_only_video_input() {
        if !tools_available() {
            return;
        }
        let (Some(video), Some(audio)) = (make_wav_fixture(), make_wav_fixture()) else {
            return;
        };
        assert!(matches!(
            merge_av(&video, &audio, "mp4", &EngineLimits::default()),
            Err(VideoError::InvalidInput(_))
        ));
    }

    #[test]
    fn set_metadata_writes_title_tag() {
        if !tools_available() {
            return;
        }
        let Some(fixture) = make_av_fixture() else {
            return;
        };
        let mut tags = BTreeMap::new();
        tags.insert("title".to_string(), "KalaDoc Test".to_string());
        let out = set_metadata(&fixture, &tags, &EngineLimits::default()).unwrap();
        let info = probe(&out).unwrap();
        assert_eq!(
            info.tags.get("title").map(String::as_str),
            Some("KalaDoc Test")
        );
    }

    #[test]
    fn set_metadata_rejects_bad_key() {
        if !tools_available() {
            return;
        }
        let Some(fixture) = make_av_fixture() else {
            return;
        };
        let mut tags = BTreeMap::new();
        tags.insert("bad key;rm".to_string(), "x".to_string());
        assert!(matches!(
            set_metadata(&fixture, &tags, &EngineLimits::default()),
            Err(VideoError::InvalidInput(_))
        ));
    }

    #[test]
    fn strip_metadata_removes_tags() {
        if !tools_available() {
            return;
        }
        let Some(fixture) = make_av_fixture() else {
            return;
        };
        let mut tags = BTreeMap::new();
        tags.insert("title".to_string(), "To Be Stripped".to_string());
        let tagged = set_metadata(&fixture, &tags, &EngineLimits::default()).unwrap();
        assert!(!probe(&tagged).unwrap().tags.is_empty());
        let out = strip_metadata(&tagged, "mp4", &EngineLimits::default()).unwrap();
        let info = probe(&out).unwrap();
        // User metadata is gone. Container-intrinsic atoms ffprobe
        // surfaces as tags (mp4 major_brand etc.) may remain — those
        // are not metadata we wrote.
        assert!(
            !info.tags.contains_key("title") && !info.tags.contains_key("encoder"),
            "expected user tags stripped, got {:?}",
            info.tags
        );
    }

    #[test]
    fn set_cover_art_attaches_png_to_mp3() {
        if !tools_available() {
            return;
        }
        let (Some(mp3), Some(png)) = (make_mp3_fixture(), make_png_fixture()) else {
            return;
        };
        let out = set_cover_art(&mp3, &png, &EngineLimits::default()).unwrap();
        // The cover appears as an attached-pic video stream.
        let dir = TempDir::new("so-test-cover-probe").unwrap();
        let path = write_input(&dir, "out.mp3", &out).unwrap();
        let json = probe_json(&path).unwrap();
        let has_attached_pic = streams_of(&json)
            .iter()
            .any(|s| stream_codec_type(s) == "video" && is_attached_pic(s));
        assert!(has_attached_pic, "expected an attached_pic video stream");
    }
}
