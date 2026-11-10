//! Delivery packaging: HLS, MPEG-DASH, and faststart MP4.
//!
//! Multi-file outputs return [`MediaOutput`] — segment/playlist
//! names are deterministic (`index.m3u8` + `seg_%03d.ts`,
//! `manifest.mpd` + `chunk-*.m4s`/`init-*.m4s`).

use std::fs;
use std::path::PathBuf;
use std::sync::OnceLock;

use crate::crates::core::engine::EngineLimits;
use crate::crates::video_engine::error::{VideoError, VideoResult};
use crate::crates::video_engine::model::{MediaFile, MediaOutput};
use crate::crates::video_engine::ops::probe::probe;
use crate::crates::video_engine::ops::{
    COMMAND_TIMEOUT, FFMPEG, PROBE_TIMEOUT, TempDir, check_input, check_limits, ext_for_format,
    failure_detail, find_tool, h264_encoder, read_output, run_tool, tool_failure, write_input,
};

/// Default cap on packaged output artifacts when
/// `limits.max_outputs` is unset: 500 segments/playlists files is far
/// beyond any sane VOD package.
const DEFAULT_MAX_OUTPUTS: u32 = 500;

/// Segment/playlist target duration in seconds for HLS and DASH.
const SEGMENT_SECONDS: &str = "6";

/// Input written to the scratch dir plus the probed metadata needed
/// to build codec arguments.
struct PreparedInput {
    dir: TempDir,
    input_path: PathBuf,
    info: crate::crates::video_engine::model::VideoInfo,
}

/// Validate `input` against the hard cap and `limits`, probe it, and
/// stage it inside a fresh [`TempDir`]. Returns the scratch dir (keep
/// it alive for the tool run), the staged path, and the probe result.
fn prepare(input: &[u8], limits: &EngineLimits, tag: &str) -> VideoResult<PreparedInput> {
    let input_len = check_input(input)?;
    let info = probe(input)?;
    check_limits(&info, input_len, limits, false)?;
    let dir = TempDir::new(tag)?;
    let input_ext = info
        .format_name
        .as_deref()
        .and_then(ext_for_format)
        .unwrap_or("bin");
    let input_path = write_input(&dir, &format!("input.{input_ext}"), input)?;
    Ok(PreparedInput {
        dir,
        input_path,
        info,
    })
}

/// Resolve the ffmpeg binary once per call site.
fn ffmpeg_program() -> VideoResult<PathBuf> {
    find_tool(FFMPEG)
}

/// Whether this ffmpeg build lists `name` among its muxers. The list
/// is fetched once and cached; an unreadable list yields `false` and
/// callers surface the ffmpeg error honestly.
fn muxer_available(name: &str) -> bool {
    static MUXERS: OnceLock<Vec<String>> = OnceLock::new();
    let list = MUXERS.get_or_init(|| {
        let Ok(program) = find_tool(FFMPEG) else {
            return Vec::new();
        };
        let args = vec!["-hide_banner".to_string(), "-muxers".to_string()];
        match run_tool(FFMPEG, &program, &args, PROBE_TIMEOUT) {
            Ok(output) if output.status.success() => {
                String::from_utf8_lossy(&output.stdout)
                    .lines()
                    // Lines look like "  E  hls   Apple HTTP Live
                    // Streaming" — flags token, then the muxer name.
                    .filter_map(|line| line.split_whitespace().nth(1))
                    .map(str::to_string)
                    .collect()
            }
            _ => Vec::new(),
        }
    });
    list.iter().any(|m| m == name)
}

/// Shared codec argument block for packaging: always re-encode to
/// H.264 + yuv420p video and AAC audio so the TS/fMP4 segments are
/// playable everywhere. `has_video`/`has_audio` select `-an`/`-vn`
/// for single-kind inputs; an input with neither is rejected.
fn package_codec_args(has_video: bool, has_audio: bool, op: &str) -> VideoResult<Vec<String>> {
    if !has_video && !has_audio {
        return Err(VideoError::InvalidInput(format!(
            "{op} requires at least one audio or video stream"
        )));
    }
    let mut args = Vec::new();
    if has_video {
        let enc = h264_encoder().ok_or_else(|| VideoError::ExternalTool {
            tool: FFMPEG.to_string(),
            detail: "packaging requires an H.264 encoder (libx264 or libopenh264); \
                     this ffmpeg build provides neither"
                .to_string(),
        })?;
        args.extend([
            "-c:v".to_string(),
            enc.to_string(),
            "-pix_fmt".to_string(),
            "yuv420p".to_string(),
        ]);
        if has_audio {
            args.extend(["-c:a".to_string(), "aac".to_string()]);
        } else {
            args.push("-an".to_string());
        }
    } else {
        // Audio-only HLS/DASH: map audio only.
        args.extend(["-vn".to_string(), "-c:a".to_string(), "aac".to_string()]);
    }
    Ok(args)
}

/// Collect every regular file ffmpeg wrote into `dir` as a
/// [`MediaOutput`], sorted by name with `manifest_name` first.
/// `input_name` (the staged input file) is skipped. Enforces
/// `limits.max_outputs` (default [`DEFAULT_MAX_OUTPUTS`]) before any
/// bytes are read back.
fn collect_outputs(
    dir: &TempDir,
    input_path: &PathBuf,
    manifest_name: &str,
    limits: &EngineLimits,
) -> VideoResult<MediaOutput> {
    let input_name = input_path
        .file_name()
        .map(|n| n.to_string_lossy().into_owned())
        .unwrap_or_default();
    let max = limits.max_outputs.unwrap_or(DEFAULT_MAX_OUTPUTS) as usize;

    let mut names: Vec<String> = Vec::new();
    for entry in fs::read_dir(dir.file(""))? {
        let entry = entry?;
        if !entry.file_type()?.is_file() {
            continue;
        }
        let name = entry.file_name().to_string_lossy().into_owned();
        // Skip the staged input and any dotfiles/ffmpeg scratch.
        if name == input_name || name.starts_with('.') {
            continue;
        }
        names.push(name);
    }
    names.sort();
    if names.len() > max {
        return Err(VideoError::LimitExceeded(format!(
            "packaging produced {} files, exceeding max_outputs {max}",
            names.len()
        )));
    }
    // Manifest/playlist first for callers that stream files in order.
    names.sort_by_key(|n| (n != manifest_name, n.clone()));

    let mut files = Vec::with_capacity(names.len());
    for name in names {
        let bytes = read_output(&dir.file(&name))?;
        files.push(MediaFile { name, bytes });
    }
    Ok(MediaOutput { files })
}

/// Package `input` as an HLS VOD stream: `index.m3u8` +
/// `seg_%03d.ts` MPEG-TS segments (6s target). Streams are always
/// re-encoded to H.264/AAC because arbitrary input codecs cannot go
/// into TS; video-less inputs produce an audio-only playlist, inputs
/// without audio get `-an`.
pub(crate) fn package_hls(input: &[u8], limits: &EngineLimits) -> VideoResult<MediaOutput> {
    let prepared = prepare(input, limits, "hls")?;
    if !muxer_available("hls") {
        return Err(VideoError::ExternalTool {
            tool: FFMPEG.to_string(),
            detail: "ffmpeg build has no 'hls' muxer".to_string(),
        });
    }
    let program = ffmpeg_program()?;

    let mut args = vec![
        "-y".to_string(),
        "-v".to_string(),
        "error".to_string(),
        "-i".to_string(),
        prepared.input_path.to_string_lossy().into_owned(),
    ];
    args.extend(package_codec_args(
        prepared.info.has_video,
        prepared.info.has_audio,
        "package_hls",
    )?);
    args.extend([
        "-f".to_string(),
        "hls".to_string(),
        "-hls_time".to_string(),
        SEGMENT_SECONDS.to_string(),
        "-hls_playlist_type".to_string(),
        "vod".to_string(),
        // Absolute pattern keeps segments inside the TempDir; the
        // playlist still references bare `seg_NNN.ts` names.
        "-hls_segment_filename".to_string(),
        prepared
            .dir
            .file("seg_%03d.ts")
            .to_string_lossy()
            .into_owned(),
        prepared
            .dir
            .file("index.m3u8")
            .to_string_lossy()
            .into_owned(),
    ]);

    let output = run_tool(FFMPEG, &program, &args, COMMAND_TIMEOUT)?;
    if !output.status.success() {
        return Err(tool_failure(FFMPEG, &output));
    }
    collect_outputs(&prepared.dir, &prepared.input_path, "index.m3u8", limits)
}

/// Package `input` as MPEG-DASH: `manifest.mpd` +
/// `init-stream*.m4s`/`chunk-stream*.m4s` segments (6s target).
/// Streams are re-encoded to H.264/AAC; audio-only inputs emit a
/// single audio AdaptationSet (`-vn`), video-only inputs get `-an`.
pub(crate) fn package_dash(input: &[u8], limits: &EngineLimits) -> VideoResult<MediaOutput> {
    let prepared = prepare(input, limits, "dash")?;
    if !muxer_available("dash") {
        return Err(VideoError::ExternalTool {
            tool: FFMPEG.to_string(),
            detail: "ffmpeg build has no 'dash' muxer".to_string(),
        });
    }
    let program = ffmpeg_program()?;

    let mut args = vec![
        "-y".to_string(),
        "-v".to_string(),
        "error".to_string(),
        "-i".to_string(),
        prepared.input_path.to_string_lossy().into_owned(),
    ];
    args.extend(package_codec_args(
        prepared.info.has_video,
        prepared.info.has_audio,
        "package_dash",
    )?);
    args.extend([
        "-f".to_string(),
        "dash".to_string(),
        "-seg_duration".to_string(),
        SEGMENT_SECONDS.to_string(),
        prepared
            .dir
            .file("manifest.mpd")
            .to_string_lossy()
            .into_owned(),
    ]);

    let output = run_tool(FFMPEG, &program, &args, COMMAND_TIMEOUT)?;
    if !output.status.success() {
        let detail = failure_detail(&output, "ffmpeg exited unsuccessfully");
        // Codec/container rejections are input problems; anything
        // else is a tool failure.
        let lower = detail.to_lowercase();
        if lower.contains("codec")
            || lower.contains("muxer")
            || lower.contains("could not write header")
        {
            return Err(VideoError::InvalidInput(format!(
                "cannot package as DASH: {detail}"
            )));
        }
        return Err(tool_failure(FFMPEG, &output));
    }
    collect_outputs(&prepared.dir, &prepared.input_path, "manifest.mpd", limits)
}

/// ISO-BMFF-family demuxer names that `faststart` accepts. ffprobe
/// reports e.g. `"mov,mp4,m4a,3gp,3g2,mj2"`; any listed token wins.
const MP4_FAMILY_TOKENS: &[&str] = &["mp4", "mov", "m4a", "3gp", "mj2"];

/// Rewrite an MP4/MOV-family input with the `moov` atom moved to the
/// front (`-movflags +faststart`) so progressive download can start
/// playback before the file finishes. Non-MP4-family containers
/// (mkv, avi, ts, ...) are rejected as [`VideoError::InvalidInput`].
pub(crate) fn faststart(input: &[u8], limits: &EngineLimits) -> VideoResult<Vec<u8>> {
    let input_len = check_input(input)?;
    let info = probe(input)?;
    check_limits(&info, input_len, limits, false)?;

    let format_name = info.format_name.as_deref().unwrap_or("");
    let is_mp4_family = format_name
        .split(',')
        .map(str::trim)
        .any(|t| MP4_FAMILY_TOKENS.contains(&t));
    if !is_mp4_family {
        return Err(VideoError::InvalidInput(format!(
            "faststart requires an MP4/MOV-family container, got format_name '{format_name}'"
        )));
    }

    let program = ffmpeg_program()?;
    let dir = TempDir::new("faststart")?;
    let input_ext = info
        .format_name
        .as_deref()
        .and_then(ext_for_format)
        .unwrap_or("mp4");
    let input_path = write_input(&dir, &format!("input.{input_ext}"), input)?;
    let out_path = dir.file("out.mp4");

    let args = vec![
        "-y".to_string(),
        "-v".to_string(),
        "error".to_string(),
        "-i".to_string(),
        input_path.to_string_lossy().into_owned(),
        "-c".to_string(),
        "copy".to_string(),
        "-movflags".to_string(),
        "+faststart".to_string(),
        out_path.to_string_lossy().into_owned(),
    ];
    let output = run_tool(FFMPEG, &program, &args, COMMAND_TIMEOUT)?;
    if !output.status.success() {
        return Err(tool_failure(FFMPEG, &output));
    }
    read_output(&out_path)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::crates::video_engine::error::VideoError;
    use crate::crates::video_engine::ops::{FFPROBE, tool_available};

    fn tools_available() -> bool {
        tool_available(FFMPEG) && tool_available(FFPROBE)
    }

    /// Build a 2s 320x240@10fps clip with a 440 Hz sine track.
    /// Returns `None` when ffmpeg cannot produce it.
    fn make_av_fixture() -> Option<Vec<u8>> {
        let program = find_tool(FFMPEG).ok()?;
        let dir = TempDir::new("pkg-test").ok()?;
        let out = dir.file("fixture.mp4");
        let args = vec![
            "-y".to_string(),
            "-v".to_string(),
            "error".to_string(),
            "-f".to_string(),
            "lavfi".to_string(),
            "-i".to_string(),
            "testsrc=duration=2:size=320x240:rate=10".to_string(),
            "-f".to_string(),
            "lavfi".to_string(),
            "-i".to_string(),
            "sine=frequency=440:duration=2".to_string(),
            "-pix_fmt".to_string(),
            "yuv420p".to_string(),
            "-shortest".to_string(),
            out.to_string_lossy().into_owned(),
        ];
        let output = run_tool(FFMPEG, &program, &args, COMMAND_TIMEOUT).ok()?;
        if !output.status.success() {
            return None;
        }
        fs::read(out).ok()
    }

    /// Build a 2s Matroska fixture (any codec — faststart must reject
    /// it on container grounds before touching codecs).
    fn make_mkv_fixture() -> Option<Vec<u8>> {
        let program = find_tool(FFMPEG).ok()?;
        let dir = TempDir::new("pkg-test-mkv").ok()?;
        let out = dir.file("fixture.mkv");
        let args = vec![
            "-y".to_string(),
            "-v".to_string(),
            "error".to_string(),
            "-f".to_string(),
            "lavfi".to_string(),
            "-i".to_string(),
            "testsrc=duration=2:size=320x240:rate=10".to_string(),
            "-f".to_string(),
            "lavfi".to_string(),
            "-i".to_string(),
            "sine=frequency=440:duration=2".to_string(),
            "-pix_fmt".to_string(),
            "yuv420p".to_string(),
            "-shortest".to_string(),
            out.to_string_lossy().into_owned(),
        ];
        let output = run_tool(FFMPEG, &program, &args, COMMAND_TIMEOUT).ok()?;
        if !output.status.success() {
            return None;
        }
        fs::read(out).ok()
    }

    #[test]
    fn hls_produces_playlist_and_ts_segments() {
        if !tools_available() || h264_encoder().is_none() || !muxer_available("hls") {
            return;
        }
        let Some(fixture) = make_av_fixture() else {
            return;
        };
        let out = package_hls(&fixture, &EngineLimits::default()).unwrap();
        let names: Vec<&str> = out.files.iter().map(|f| f.name.as_str()).collect();
        assert!(names.contains(&"index.m3u8"), "files: {names:?}");
        assert!(
            names.iter().any(|n| n.ends_with(".ts")),
            "no .ts segment in {names:?}"
        );
        // Playlist references segment files by bare name.
        let m3u8 = out.files.iter().find(|f| f.name == "index.m3u8").unwrap();
        let text = String::from_utf8_lossy(&m3u8.bytes);
        assert!(text.contains("#EXTM3U"), "playlist head: {text}");
        assert!(text.contains(".ts"), "playlist lacks .ts refs: {text}");
        // Manifest is first in the files vec.
        assert_eq!(out.files[0].name, "index.m3u8");
    }

    #[test]
    fn dash_produces_manifest_and_segments() {
        if !tools_available() || h264_encoder().is_none() || !muxer_available("dash") {
            return;
        }
        let Some(fixture) = make_av_fixture() else {
            return;
        };
        let out = package_dash(&fixture, &EngineLimits::default()).unwrap();
        let names: Vec<&str> = out.files.iter().map(|f| f.name.as_str()).collect();
        assert!(names.contains(&"manifest.mpd"), "files: {names:?}");
        assert!(
            names.iter().any(|n| n.ends_with(".m4s")),
            "no .m4s segment in {names:?}"
        );
        let mpd = out.files.iter().find(|f| f.name == "manifest.mpd").unwrap();
        let text = String::from_utf8_lossy(&mpd.bytes);
        assert!(text.contains("<MPD"), "manifest head: {text}");
        assert_eq!(out.files[0].name, "manifest.mpd");
    }

    #[test]
    fn faststart_rewrites_mp4_and_preserves_duration() {
        if !tools_available() {
            return;
        }
        let Some(fixture) = make_av_fixture() else {
            return;
        };
        let src = probe(&fixture).unwrap();
        let out = faststart(&fixture, &EngineLimits::default()).unwrap();
        assert!(!out.is_empty());
        // moov-first MP4: the 'moov' atom appears before 'mdat'.
        let text = String::from_utf8_lossy(&out);
        let _ = text; // binary-safe check below instead
        let moov = find_subslice(&out, b"moov");
        let mdat = find_subslice(&out, b"mdat");
        assert!(moov.is_some(), "no moov atom found");
        assert!(mdat.is_some(), "no mdat atom found");
        assert!(moov < mdat, "moov atom is not before mdat");

        let dst = probe(&out).unwrap();
        let (Some(a), Some(b)) = (src.duration_secs, dst.duration_secs) else {
            return;
        };
        assert!((a - b).abs() < 0.5, "duration changed: {a:.3}s -> {b:.3}s");
    }

    #[test]
    fn faststart_rejects_mkv() {
        if !tools_available() {
            return;
        }
        let Some(mkv) = make_mkv_fixture() else {
            return;
        };
        let err = faststart(&mkv, &EngineLimits::default()).unwrap_err();
        assert!(
            matches!(err, VideoError::InvalidInput(_)),
            "expected InvalidInput, got {err:?}"
        );
    }

    /// Naive substring search for a 4-byte atom name.
    fn find_subslice(haystack: &[u8], needle: &[u8]) -> Option<usize> {
        haystack.windows(needle.len()).position(|w| w == needle)
    }
}
