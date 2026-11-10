//! Remux / repackage a media file without re-encoding.
//!
//! [`stream_copy`] is the "copy everything, change nothing but the
//! container" op: `ffmpeg -i <in> -map 0 -c copy <out>`. It is the
//! building block for container changes (mp4 → mkv, mkv → mp4, ...),
//! fast repackaging, and repairing files whose streams are already
//! in their final codec.
//!
//! Stream copy is cheap but *not* universally compatible: a codec
//! that one container accepts may be rejected by another's muxer
//! (e.g. copying an mp3 audio stream into an IVF/WebM target, or an
//! mp4-only codec like `mov_text` into MPEG-TS). Those failures are
//! reported honestly as [`MediaError::ToolFailed`] — there is no
//! fallback re-encode here by design.
//!
//! The only capability gate is the output container's muxer.

use std::ffi::OsString;

use serde::{Deserialize, Serialize};

use crate::crates::multimedia_engine::error::{MediaError, MediaResult};
use crate::crates::multimedia_engine::model::{MediaLimits, MediaOutput};
use crate::crates::multimedia_engine::runtime::discovery::Runtime;
use crate::crates::multimedia_engine::runtime::{process, tempdir::TempDir};

/// Options for [`stream_copy`].
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct StreamCopyOptions {
    /// Output container extension without a leading dot
    /// (`"mp4"`, `"mkv"`, `"webm"`, `"ts"`, ...). Becomes
    /// `output.<format>` — choosing a different extension *is* the
    /// container change. `None` keeps the input's sniffed container.
    #[serde(default)]
    pub output_format: Option<String>,
    /// Map every stream from the input (`-map 0`, default `true`).
    /// `false` lets ffmpeg's default stream selection pick one stream
    /// per type — useful to drop data/attachment streams.
    #[serde(default = "default_true")]
    pub map_all_streams: bool,
    /// Copy chapter markers (default `true`). `false` adds
    /// `-map_chapters -1`.
    #[serde(default = "default_true")]
    pub copy_chapters: bool,
    /// Copy container-level metadata (default `true`). `false` adds
    /// `-map_metadata -1`.
    #[serde(default = "default_true")]
    pub copy_metadata: bool,
}

fn default_true() -> bool {
    true
}

/// Remux `input` into a (possibly different) container, copying all
/// streams bit-exactly.
///
/// Runs `ffmpeg -hide_banner -loglevel error -nostdin -y -i <in>
/// [-map 0] -c copy [-map_chapters 0|-1] [-map_metadata 0|-1]
/// output.<ext>` inside a temp dir and returns the produced file as
/// the primary [`MediaFile`](crate::crates::multimedia_engine::MediaFile).
///
/// # Errors
///
/// - [`MediaError::InvalidInput`] — empty input or invalid
///   `output_format`.
/// - [`MediaError::LimitExceeded`] — input exceeds
///   `max_input_bytes`, or the output exceeds `max_output_bytes`.
/// - [`MediaError::Unsupported`] — the output muxer is not in this
///   FFmpeg build.
/// - [`MediaError::ToolFailed`] — ffmpeg rejected the remux, e.g. a
///   stream codec the target container cannot hold.
pub fn stream_copy(
    input: &[u8],
    options: &StreamCopyOptions,
    limits: &MediaLimits,
    rt: &Runtime,
) -> MediaResult<MediaOutput> {
    if input.is_empty() {
        return Err(MediaError::InvalidInput("empty input buffer".to_string()));
    }
    if let Some(max) = limits.max_input_bytes
        && (input.len() as u64) > max
    {
        return Err(MediaError::LimitExceeded(format!(
            "input is {} bytes, exceeding max_input_bytes {max}",
            input.len()
        )));
    }

    // Capability gate before spawning: `-c copy` needs no encoder,
    // only the output muxer.
    let caps = rt.capabilities()?;
    let output_ext = resolve_output_ext(input, options.output_format.as_deref())?;
    let muxer = muxer_name_for(&output_ext);
    if !caps.has_muxer(muxer) {
        return Err(MediaError::Unsupported(format!(
            "muxer '{muxer}' (for .{output_ext}) is not available in this FFmpeg build"
        )));
    }

    let dir = TempDir::new("stream-copy")?;
    let input_path = dir.write(&format!("input.{}", sniff_extension(input)), input)?;
    let output_name = format!("output.{output_ext}");
    let output_path = dir.file(&output_name);

    let mut argv: Vec<OsString> = vec![
        OsString::from("-hide_banner"),
        OsString::from("-loglevel"),
        OsString::from("error"),
        OsString::from("-nostdin"),
        OsString::from("-y"),
        OsString::from("-i"),
        input_path.into_os_string(),
    ];
    if options.map_all_streams {
        argv.push(OsString::from("-map"));
        argv.push(OsString::from("0"));
    }
    argv.push(OsString::from("-c"));
    argv.push(OsString::from("copy"));
    argv.push(OsString::from("-map_chapters"));
    argv.push(OsString::from(if options.copy_chapters {
        "0"
    } else {
        "-1"
    }));
    argv.push(OsString::from("-map_metadata"));
    argv.push(OsString::from(if options.copy_metadata {
        "0"
    } else {
        "-1"
    }));
    argv.push(output_path.into_os_string());

    process::run_checked("ffmpeg", &rt.ffmpeg, &argv, None, limits)?;

    let bytes = dir.read_limited(&output_name, limits.max_output_bytes)?;
    if bytes.is_empty() {
        return Err(MediaError::ToolFailed {
            tool: "ffmpeg",
            status: "empty output".to_string(),
            detail: format!("ffmpeg produced an empty {output_name}"),
        });
    }

    Ok(MediaOutput {
        primary: Some(crate::crates::multimedia_engine::model::MediaFile {
            name: output_name,
            bytes,
            content_type: content_type_for(&output_ext),
        }),
        extra: Vec::new(),
        warnings: Vec::new(),
    })
}

/// Resolve the output container extension: explicit `output_format`
/// when given (validated), otherwise the input's sniffed container.
fn resolve_output_ext(input: &[u8], output_format: Option<&str>) -> MediaResult<String> {
    if let Some(fmt) = output_format {
        let fmt = fmt.trim().trim_start_matches('.').to_ascii_lowercase();
        if fmt.is_empty() || !fmt.chars().all(|c| c.is_ascii_alphanumeric()) {
            return Err(MediaError::InvalidInput(format!(
                "invalid output_format {fmt:?}"
            )));
        }
        return Ok(fmt);
    }
    Ok(sniff_container(input).to_string())
}

/// Map a file extension to the ffmpeg muxer name for the capability
/// check (`.mkv` → `matroska`, `.ts` → `mpegts`, ...).
fn muxer_name_for(ext: &str) -> &str {
    match ext {
        "mkv" => "matroska",
        "m4a" => "ipod",
        "ts" | "mts" | "m2ts" => "mpegts",
        "mpga" | "oga" => "ogg",
        other => other,
    }
}

/// Best-effort container guess from magic bytes for the default
/// output extension.
fn sniff_container(data: &[u8]) -> &'static str {
    if data.len() >= 12 && &data[4..8] == b"ftyp" {
        return if &data[8..12] == b"qt  " {
            "mov"
        } else {
            "mp4"
        };
    }
    if data.starts_with(&[0x1A, 0x45, 0xDF, 0xA3]) {
        return "mkv";
    }
    if data.starts_with(b"RIFF") && data.len() >= 12 {
        return match &data[8..12] {
            b"WAVE" => "wav",
            b"AVI " => "avi",
            _ => "mp4",
        };
    }
    if data.starts_with(b"OggS") {
        return "ogg";
    }
    if data.starts_with(b"fLaC") {
        return "flac";
    }
    if data.starts_with(b"ID3") || (data.len() >= 2 && data[0] == 0xFF && (data[1] & 0xE0) == 0xE0)
    {
        return "mp3";
    }
    "mp4"
}

/// Best-effort extension for the temp *input* file.
fn sniff_extension(data: &[u8]) -> &'static str {
    if data.len() >= 12 && &data[4..8] == b"ftyp" {
        return if &data[8..12] == b"qt  " {
            "mov"
        } else {
            "mp4"
        };
    }
    if data.starts_with(&[0x1A, 0x45, 0xDF, 0xA3]) {
        return "mkv";
    }
    if data.starts_with(b"RIFF") && data.len() >= 12 {
        return match &data[8..12] {
            b"WAVE" => "wav",
            b"AVI " => "avi",
            _ => "bin",
        };
    }
    if data.starts_with(b"OggS") {
        return "ogg";
    }
    if data.starts_with(b"fLaC") {
        return "flac";
    }
    if data.starts_with(b"ID3") || (data.len() >= 2 && data[0] == 0xFF && (data[1] & 0xE0) == 0xE0)
    {
        return "mp3";
    }
    "bin"
}

/// Best-effort MIME type for the produced container.
fn content_type_for(ext: &str) -> Option<String> {
    let mime = match ext {
        "mp4" | "m4v" | "m4a" => "video/mp4",
        "mkv" => "video/x-matroska",
        "webm" => "video/webm",
        "mov" => "video/quicktime",
        "avi" => "video/x-msvideo",
        "mp3" => "audio/mpeg",
        "aac" | "adts" => "audio/aac",
        "wav" => "audio/wav",
        "ogg" | "oga" | "opus" => "audio/ogg",
        "flac" => "audio/flac",
        "ts" | "mts" | "m2ts" => "video/mp2t",
        _ => return None,
    };
    Some(mime.to_string())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::crates::multimedia_engine::probe::inspect::inspect;
    use crate::crates::multimedia_engine::runtime::discovery;

    fn runtime() -> Runtime {
        Runtime::discover().expect("ffmpeg/ffprobe must be on PATH")
    }

    fn limits() -> MediaLimits {
        MediaLimits::default()
    }

    /// 1s 64x64 mpeg4 video + aac audio mp4 (native encoders only).
    fn make_av_mp4() -> Option<Vec<u8>> {
        let ffmpeg = discovery::find_tool("ffmpeg")?;
        let dir = TempDir::new("scopy-test-in").ok()?;
        let out_path = dir.file("av.mp4");
        let argv = process::args(&[
            "-hide_banner",
            "-loglevel",
            "error",
            "-nostdin",
            "-y",
            "-f",
            "lavfi",
            "-i",
            "testsrc=size=64x64:rate=10:duration=1",
            "-f",
            "lavfi",
            "-i",
            "sine=frequency=440:duration=1",
            "-c:v",
            "mpeg4",
            "-pix_fmt",
            "yuv420p",
            "-c:a",
            "aac",
            "-shortest",
            out_path.to_str()?,
        ]);
        let out = process::run("ffmpeg", &ffmpeg, &argv, None, &limits()).ok()?;
        if !out.success {
            return None;
        }
        std::fs::read(&out_path).ok()
    }

    #[test]
    fn stream_copy_same_container_is_lossless_remux() {
        let Some(mp4) = make_av_mp4() else {
            eprintln!("skipping: ffmpeg lavfi/mpeg4/aac unavailable");
            return;
        };
        let rt = runtime();
        let out =
            stream_copy(&mp4, &StreamCopyOptions::default(), &limits(), &rt).expect("stream_copy");
        let file = out.primary.expect("primary output");
        assert_eq!(file.name, "output.mp4");
        assert!(!file.bytes.is_empty());

        let info = inspect(&file.bytes, &limits()).expect("probe output");
        assert_eq!(info.video_streams, 1);
        assert_eq!(info.audio_streams, 1);
        let dur = info.duration_secs.expect("duration");
        assert!((dur - 1.0).abs() < 0.3, "duration {dur}");
    }

    #[test]
    fn stream_copy_changes_container_to_mkv() {
        let Some(mp4) = make_av_mp4() else {
            eprintln!("skipping: ffmpeg lavfi/mpeg4/aac unavailable");
            return;
        };
        let rt = runtime();
        let caps = match rt.capabilities() {
            Ok(c) => c,
            Err(_) => return,
        };
        if !caps.has_muxer("matroska") {
            eprintln!("skipping: matroska muxer unavailable");
            return;
        }
        let options = StreamCopyOptions {
            output_format: Some("mkv".to_string()),
            ..Default::default()
        };
        let out = stream_copy(&mp4, &options, &limits(), &rt).expect("copy to mkv");
        let file = out.primary.expect("primary output");
        assert_eq!(file.name, "output.mkv");

        let info = inspect(&file.bytes, &limits()).expect("probe mkv");
        assert!(
            info.format_name
                .as_deref()
                .is_some_and(|n| n.contains("matroska") || n.contains("webm")),
            "unexpected format {:?}",
            info.format_name
        );
        assert_eq!(info.video_streams, 1);
        assert_eq!(info.audio_streams, 1);
    }

    #[test]
    fn stream_copy_rejects_empty_and_bad_format() {
        let rt = runtime();
        let err =
            stream_copy(&[], &StreamCopyOptions::default(), &limits(), &rt).expect_err("must fail");
        assert!(matches!(err, MediaError::InvalidInput(_)), "{err:?}");

        let options = StreamCopyOptions {
            output_format: Some("not a format!".to_string()),
            ..Default::default()
        };
        let err = stream_copy(&[0u8; 64], &options, &limits(), &rt).expect_err("must fail");
        assert!(matches!(err, MediaError::InvalidInput(_)), "{err:?}");
    }
}
