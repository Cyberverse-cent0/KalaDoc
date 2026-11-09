//! Cover art / attached-picture management for audio containers.
//!
//! [`add_cover_art`] embeds an image (JPEG/PNG decoded to MJPEG) as
//! an `attached_pic` video stream in mp3/flac/m4a/ogg files without
//! re-encoding the audio. [`extract_cover_art`] finds the
//! attached-picture stream via ffprobe and copies it out as an image
//! file. [`remove_cover_art`] re-muxes the container with that one
//! stream excluded.
//!
//! All three ops are stream-copy operations — audio bits are never
//! decoded or re-encoded.

use std::ffi::OsString;
use std::path::PathBuf;

use crate::crates::multimedia_engine::error::{MediaError, MediaResult};
use crate::crates::multimedia_engine::model::{MediaLimits, MediaOutput};
use crate::crates::multimedia_engine::probe::types::{MediaStream, ProbeReport};
use crate::crates::multimedia_engine::runtime::discovery::Runtime;
use crate::crates::multimedia_engine::runtime::process;
use crate::crates::multimedia_engine::runtime::tempdir::TempDir;

/// Containers that accept an attached picture via this op.
/// ffmpeg muxer name for a supported cover-art container extension.
fn muxer_for<'a>(ext: &'a str) -> &'a str {
    match ext {
        "m4a" => "ipod",
        "mp3" => "mp3",
        "flac" => "flac",
        "ogg" => "ogg",
        other => other,
    }
}

/// Sniff a cover-art-capable audio container extension from magic
/// bytes. Returns `None` for anything else — callers turn that into
/// [`MediaError::InvalidInput`].
fn sniff_audio_ext(input: &[u8]) -> Option<&'static str> {
    if input.len() >= 4 {
        match &input[..4] {
            [b'f', b'L', b'a', b'C'] => return Some("flac"),
            [b'O', b'g', b'g', b'S'] => return Some("ogg"),
            [b'I', b'D', b'3', _] => return Some("mp3"),
            _ => {}
        }
        // ISO BMFF: bytes 4..8 are "ftyp"; brands M4A /M4B mark
        // audio-only MP4 files (iTunes-style m4a).
        if input.len() >= 12 && &input[4..8] == b"ftyp" {
            return match &input[8..12] {
                b"M4A " | b"M4B " => Some("m4a"),
                _ => None,
            };
        }
        // MPEG audio frame sync (MP3 without ID3).
        if input[0] == 0xFF && (input[1] & 0xE0) == 0xE0 {
            return Some("mp3");
        }
    }
    None
}

/// Best-effort generic extension for the temp input file so ffprobe
/// and ffmpeg get a demuxer hint. Not restricted to audio containers.
fn sniff_ext(input: &[u8]) -> &'static str {
    sniff_audio_ext(input).unwrap_or_else(|| {
        if input.len() >= 12 && &input[4..8] == b"ftyp" {
            "mp4"
        } else {
            "bin"
        }
    })
}

/// Reject inputs over `max_input_bytes`.
fn check_input_size(input: &[u8], limits: &MediaLimits) -> MediaResult<()> {
    if let Some(max) = limits.max_input_bytes
        && (input.len() as u64) > max
    {
        return Err(MediaError::LimitExceeded(format!(
            "input is {} bytes, exceeding max_input_bytes {max}",
            input.len()
        )));
    }
    Ok(())
}

/// Write `input` into `dir` with a sniffed extension and return its path.
fn write_input(dir: &TempDir, input: &[u8]) -> MediaResult<PathBuf> {
    let name = format!("input.{}", sniff_ext(input));
    dir.write(&name, input)
}

/// Run ffprobe `-show_streams` against `path` and return the streams.
fn probe_streams(
    path: &PathBuf,
    limits: &MediaLimits,
    rt: &Runtime,
) -> MediaResult<Vec<MediaStream>> {
    let args = process::args(&[
        "-v",
        "error",
        "-print_format",
        "json",
        "-show_streams",
        path.to_str()
            .ok_or_else(|| MediaError::InvalidInput("non-UTF-8 temp path".to_string()))?,
    ]);
    let out = process::run_checked("ffprobe", &rt.ffprobe, &args, None, limits)?;
    let report: ProbeReport = serde_json::from_slice(&out.stdout)
        .map_err(|e| MediaError::InvalidInput(format!("ffprobe JSON parse failed: {e}")))?;
    Ok(report.streams)
}

/// Whether a stream is flagged as an attached picture (cover art).
fn is_attached_pic(s: &MediaStream) -> bool {
    s.disposition_attached_pic == Some(true) || s.disposition.attached_pic.is_some_and(|v| v != 0)
}

/// The attached-picture stream, when present. Attached pics are
/// `codec_type == "video"` streams; in some containers (Matroska)
/// they surface as `codec_type == "attachment"` instead.
fn attached_pic_stream(streams: &[MediaStream]) -> Option<&MediaStream> {
    streams.iter().find(|s| {
        is_attached_pic(s)
            || (s.kind() == Some("video")
                && s.codec_name
                    .as_deref()
                    .is_some_and(|c| matches!(c, "mjpeg" | "png" | "bmp" | "gif" | "tiff")))
    })
}

/// Map a cover-art codec to an image file extension for extraction.
fn image_ext_for_codec(codec: Option<&str>) -> &'static str {
    match codec {
        Some("png") => "png",
        Some("bmp") => "bmp",
        Some("gif") => "gif",
        Some("tiff") => "tiff",
        _ => "jpg",
    }
}

/// Embed `image` as cover art in `input`, returning the remuxed file.
///
/// The output container is chosen by sniffing `input`: mp3, flac, m4a
/// or ogg. Anything else returns [`MediaError::InvalidInput`].
///
/// # Errors
///
/// - [`MediaError::InvalidInput`] when the input does not sniff as an
///   mp3/flac/m4a/ogg container.
/// - [`MediaError::Unsupported`] when this FFmpeg build lacks the
///   `mjpeg` encoder or the target muxer.
/// - [`MediaError::LimitExceeded`] / [`MediaError::ToolFailed`] /
///   [`MediaError::MissingTool`] / [`MediaError::Timeout`] / [`MediaError::Io`]
///   as usual.
pub fn add_cover_art(
    input: &[u8],
    image: &[u8],
    limits: &MediaLimits,
    rt: &Runtime,
) -> MediaResult<MediaOutput> {
    check_input_size(input, limits)?;
    check_input_size(image, limits)?;

    let ext = sniff_audio_ext(input).ok_or_else(|| {
        MediaError::InvalidInput(format!(
            "input is not an mp3/flac/m4a/ogg container; cannot attach cover art"
        ))
    })?;
    let muxer = muxer_for(ext);

    let caps = rt.capabilities()?;
    if !caps.has_muxer(muxer) {
        return Err(MediaError::Unsupported(format!(
            "container {ext:?} (muxer {muxer:?}) is not available in this FFmpeg build"
        )));
    }
    if !caps.has_encoder("mjpeg") {
        return Err(MediaError::Unsupported(
            "mjpeg encoder is not available in this FFmpeg build".to_string(),
        ));
    }

    let dir = TempDir::new("cover-art-add")?;
    let in_path = write_input(&dir, input)?;
    let img_name = format!("cover.{}", sniff_image_ext(image));
    let img_path = dir.write(&img_name, image)?;

    let out_name = format!("output.{ext}");
    let out_path = dir.file(&out_name);

    let mut argv: Vec<OsString> = ["-hide_banner", "-loglevel", "error", "-nostdin", "-y", "-i"]
        .iter()
        .map(OsString::from)
        .collect();
    argv.push(in_path.into_os_string());
    argv.push("-i".into());
    argv.push(img_path.into_os_string());
    argv.extend(
        [
            "-map",
            "0",
            "-map",
            "1",
            "-c",
            "copy",
            "-c:v",
            "mjpeg",
            "-disposition:v:0",
            "attached_pic",
            "-metadata:s:v",
            "title=Album cover",
            "-f",
            muxer,
        ]
        .iter()
        .map(OsString::from),
    );
    // ID3v2.3 is the de-facto standard for mp3 tags; other muxers
    // would reject or ignore the option, so only pass it for mp3.
    if ext == "mp3" {
        argv.extend(["-id3v2_version".into(), "3".into()]);
    }
    argv.push(out_path.into_os_string());

    let out = process::run_checked("ffmpeg", &rt.ffmpeg, &argv, None, limits)?;
    let mut warnings = Vec::new();
    if !out.stderr.is_empty() {
        warnings.push(out.stderr);
    }

    let bytes = dir.read_limited(&out_name, limits.max_output_bytes)?;
    if bytes.is_empty() {
        return Err(MediaError::ToolFailed {
            tool: "ffmpeg",
            status: "empty output".to_string(),
            detail: "add_cover_art produced a zero-byte file".to_string(),
        });
    }

    let mut result = MediaOutput::single(out_name, bytes);
    result.warnings = warnings;
    Ok(result)
}

/// Sniff an image extension for the cover input temp file. ffmpeg
/// probes the real codec anyway; the extension only guides demuxer
/// selection.
fn sniff_image_ext(image: &[u8]) -> &'static str {
    if image.starts_with(b"\x89PNG") {
        "png"
    } else if image.starts_with(b"GIF8") {
        "gif"
    } else if image.starts_with(b"BM") {
        "bmp"
    } else {
        "jpg"
    }
}

/// Extract the attached picture from `input`, if one exists.
///
/// Returns `Ok(None)` when the file has no attached-picture stream.
/// The extracted image is stream-copied (no re-encode) and returned
/// as a [`MediaOutput`] whose primary file is `cover.<ext>` with the
/// extension matching the stored image codec (`jpg`, `png`, ...).
///
/// # Errors
///
/// - [`MediaError::InvalidInput`] when ffprobe cannot parse the input.
/// - [`MediaError::Unsupported`] when this FFmpeg build lacks the
///   `image2` muxer needed to write the extracted image.
/// - [`MediaError::LimitExceeded`] / [`MediaError::ToolFailed`] /
///   [`MediaError::MissingTool`] / [`MediaError::Timeout`] / [`MediaError::Io`]
///   as usual.
pub fn extract_cover_art(
    input: &[u8],
    limits: &MediaLimits,
    rt: &Runtime,
) -> MediaResult<Option<MediaOutput>> {
    check_input_size(input, limits)?;

    let dir = TempDir::new("cover-art-extract")?;
    let in_path = write_input(&dir, input)?;
    let streams = probe_streams(&in_path, limits, rt)?;

    let Some(pic) = attached_pic_stream(&streams) else {
        return Ok(None);
    };

    let caps = rt.capabilities()?;
    if !caps.has_muxer("image2") {
        return Err(MediaError::Unsupported(
            "image2 muxer is not available in this FFmpeg build".to_string(),
        ));
    }

    let img_ext = image_ext_for_codec(pic.codec_name.as_deref());
    let out_name = format!("cover.{img_ext}");
    let out_path = dir.file(&out_name);

    // Map the exact probed stream index when known; fall back to the
    // first video stream (attached pics are the only video stream in
    // audio containers).
    let map_spec = pic
        .index
        .map(|i| format!("0:{i}"))
        .unwrap_or_else(|| "0:v".to_string());

    let argv = process::args(&[
        "-hide_banner",
        "-loglevel",
        "error",
        "-nostdin",
        "-y",
        "-i",
        in_path.to_str().unwrap_or_default(),
        "-map",
        &map_spec,
        "-c",
        "copy",
        "-frames:v",
        "1",
        "-f",
        "image2",
        out_path.to_str().unwrap_or_default(),
    ]);
    let out = process::run_checked("ffmpeg", &rt.ffmpeg, &argv, None, limits)?;

    let bytes = dir.read_limited(&out_name, limits.max_output_bytes)?;
    if bytes.is_empty() {
        return Err(MediaError::ToolFailed {
            tool: "ffmpeg",
            status: "empty output".to_string(),
            detail: "cover art extraction produced a zero-byte file".to_string(),
        });
    }

    let mut warnings = Vec::new();
    if !out.stderr.is_empty() {
        warnings.push(out.stderr);
    }
    let mut result = MediaOutput::single(out_name, bytes);
    result.warnings = warnings;
    Ok(Some(result))
}

/// Remove the attached picture from `input` by re-muxing every stream
/// except the attached-picture stream.
///
/// When no attached picture is present the input is returned
/// unchanged (a warning notes nothing was removed).
///
/// # Errors
///
/// - [`MediaError::InvalidInput`] when ffprobe cannot parse the input.
/// - [`MediaError::Unsupported`] when the output muxer is unavailable.
/// - [`MediaError::LimitExceeded`] / [`MediaError::ToolFailed`] /
///   [`MediaError::MissingTool`] / [`MediaError::Timeout`] / [`MediaError::Io`]
///   as usual.
pub fn remove_cover_art(
    input: &[u8],
    limits: &MediaLimits,
    rt: &Runtime,
) -> MediaResult<MediaOutput> {
    check_input_size(input, limits)?;

    let ext = sniff_audio_ext(input).ok_or_else(|| {
        MediaError::InvalidInput("input is not an mp3/flac/m4a/ogg container".to_string())
    })?;
    let muxer = muxer_for(ext);

    let caps = rt.capabilities()?;
    if !caps.has_muxer(muxer) {
        return Err(MediaError::Unsupported(format!(
            "container {ext:?} (muxer {muxer:?}) is not available in this FFmpeg build"
        )));
    }

    let dir = TempDir::new("cover-art-remove")?;
    let in_path = write_input(&dir, input)?;
    let streams = probe_streams(&in_path, limits, rt)?;

    let Some(pic) = attached_pic_stream(&streams) else {
        // Nothing to remove — hand the input back unchanged.
        let out_name = format!("output.{ext}");
        let mut result = MediaOutput::single(out_name, input.to_vec());
        result
            .warnings
            .push("no attached picture found; input returned unchanged".to_string());
        return Ok(result);
    };

    let out_name = format!("output.{ext}");
    let out_path = dir.file(&out_name);

    let map_spec = pic
        .index
        .map(|i| format!("0:{i}"))
        .unwrap_or_else(|| "0:v".to_string());

    let argv = process::args(&[
        "-hide_banner",
        "-loglevel",
        "error",
        "-nostdin",
        "-y",
        "-i",
        in_path.to_str().unwrap_or_default(),
        "-map",
        "0",
        "-map",
        &format!("-{map_spec}"),
        "-c",
        "copy",
        "-f",
        muxer,
    ]);
    let mut argv = argv;
    if ext == "mp3" {
        argv.extend(["-id3v2_version".into(), "3".into()]);
    }
    argv.push(OsString::from(out_path.to_str().unwrap_or_default()));

    let out = process::run_checked("ffmpeg", &rt.ffmpeg, &argv, None, limits)?;
    let mut warnings = Vec::new();
    if !out.stderr.is_empty() {
        warnings.push(out.stderr);
    }

    let bytes = dir.read_limited(&out_name, limits.max_output_bytes)?;
    if bytes.is_empty() {
        return Err(MediaError::ToolFailed {
            tool: "ffmpeg",
            status: "empty output".to_string(),
            detail: "remove_cover_art produced a zero-byte file".to_string(),
        });
    }

    let mut result = MediaOutput::single(out_name, bytes);
    result.warnings = warnings;
    Ok(result)
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Generate a tiny mp3 (1s 440Hz sine, libmp3lame) via lavfi.
    /// Returns `None` when ffmpeg or the encoder is unavailable so
    /// tests skip gracefully on minimal builds.
    fn make_mp3() -> Option<Vec<u8>> {
        let rt = Runtime::discover().ok()?;
        let dir = TempDir::new("cover-art-test").ok()?;
        let out = dir.file("sample.mp3");
        let args = process::args(&[
            "-hide_banner",
            "-loglevel",
            "error",
            "-nostdin",
            "-y",
            "-f",
            "lavfi",
            "-i",
            "sine=frequency=440:duration=1",
            "-c:a",
            "libmp3lame",
            "-b:a",
            "64k",
            out.to_str()?,
        ]);
        let limits = MediaLimits::default();
        let res = process::run_checked("ffmpeg", &rt.ffmpeg, &args, None, &limits).ok()?;
        let _ = res;
        std::fs::read(&out).ok().filter(|b| !b.is_empty())
    }

    /// Generate a small JPEG via lavfi (64x64 red frame → mjpeg).
    fn make_jpg() -> Option<Vec<u8>> {
        let rt = Runtime::discover().ok()?;
        let dir = TempDir::new("cover-art-test").ok()?;
        let out = dir.file("cover.jpg");
        let args = process::args(&[
            "-hide_banner",
            "-loglevel",
            "error",
            "-nostdin",
            "-y",
            "-f",
            "lavfi",
            "-i",
            "color=c=red:size=64x64:duration=1",
            "-frames:v",
            "1",
            out.to_str()?,
        ]);
        let limits = MediaLimits::default();
        process::run_checked("ffmpeg", &rt.ffmpeg, &args, None, &limits).ok()?;
        std::fs::read(&out).ok().filter(|b| !b.is_empty())
    }

    #[test]
    fn add_then_extract_roundtrips_jpeg() {
        let (Some(mp3), Some(jpg)) = (make_mp3(), make_jpg()) else {
            eprintln!("skipping: ffmpeg lavfi/libmp3lame/mjpeg unavailable");
            return;
        };
        let rt = Runtime::discover().expect("ffmpeg/ffprobe on PATH");
        let limits = MediaLimits::default();

        let added = add_cover_art(&mp3, &jpg, &limits, &rt).expect("add_cover_art");
        let added_bytes = &added.primary.as_ref().expect("primary").bytes;
        assert_eq!(added.primary.as_ref().unwrap().name, "output.mp3");
        assert!(!added_bytes.is_empty());

        let extracted = extract_cover_art(added_bytes, &limits, &rt)
            .expect("extract_cover_art")
            .expect("attached pic should exist after add");
        let img = &extracted.primary.as_ref().expect("primary").bytes;
        assert!(
            img.starts_with(&[0xFF, 0xD8, 0xFF]),
            "extracted cover is not a JPEG"
        );
    }

    #[test]
    fn remove_cover_art_drops_attached_pic() {
        let (Some(mp3), Some(jpg)) = (make_mp3(), make_jpg()) else {
            eprintln!("skipping: ffmpeg lavfi/libmp3lame/mjpeg unavailable");
            return;
        };
        let rt = Runtime::discover().expect("ffmpeg/ffprobe on PATH");
        let limits = MediaLimits::default();

        let added = add_cover_art(&mp3, &jpg, &limits, &rt).expect("add_cover_art");
        let added_bytes = added.primary.as_ref().unwrap().bytes.clone();

        let removed = remove_cover_art(&added_bytes, &limits, &rt).expect("remove_cover_art");
        let removed_bytes = &removed.primary.as_ref().expect("primary").bytes;
        assert_eq!(removed.primary.as_ref().unwrap().name, "output.mp3");
        assert!(!removed_bytes.is_empty());

        let still = extract_cover_art(removed_bytes, &limits, &rt)
            .expect("extract_cover_art")
            .map(|o| o.primary.unwrap().bytes);
        assert!(still.is_none(), "attached pic survived removal");
    }

    #[test]
    fn extract_returns_none_for_plain_mp3() {
        let Some(mp3) = make_mp3() else {
            eprintln!("skipping: ffmpeg lavfi/libmp3lame unavailable");
            return;
        };
        let rt = Runtime::discover().expect("ffmpeg/ffprobe on PATH");
        let out = extract_cover_art(&mp3, &MediaLimits::default(), &rt).expect("extract_cover_art");
        assert!(out.is_none(), "plain mp3 should have no cover art");
    }

    #[test]
    fn add_rejects_non_audio_container() {
        let rt = Runtime::discover().expect("ffmpeg/ffprobe on PATH");
        // RIFF/WAVE magic — a valid media file but not a cover-art target.
        let wav_like = b"RIFF\x24\x00\x00\x00WAVEfmt ".to_vec();
        let jpg = make_jpg().unwrap_or_else(|| vec![0xFF, 0xD8, 0xFF, 0xE0, 0, 0]);
        let err = add_cover_art(&wav_like, &jpg, &MediaLimits::default(), &rt)
            .expect_err("wav input must be rejected");
        assert!(
            matches!(err, MediaError::InvalidInput(_)),
            "unexpected error: {err:?}"
        );
    }
}
