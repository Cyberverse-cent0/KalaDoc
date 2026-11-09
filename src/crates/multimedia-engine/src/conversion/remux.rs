//! Container remuxing — change the output container **without**
//! re-encoding the media streams.
//!
//! [`remux`] runs `ffmpeg -c copy` (stream copy) against a temp copy
//! of the input and returns the repackaged bytes. Because no codec
//! work happens, remuxing is fast and lossless — but it can only move
//! streams into containers that accept their codec parameters
//! (timestamps, codec extradata, pixel format, ...). An incompatible
//! stream makes ffmpeg fail; that surfaces as [`MediaError::ToolFailed`].
//!
//! # Codec overrides
//!
//! [`RemuxOptions::video_codec`] / [`RemuxOptions::audio_codec`] map
//! to ffmpeg's `-c:v` / `-c:a`. The default `"copy"` preserves the
//! encoded bitstream. Passing a real encoder name (e.g. `"aac"`,
//! `"libx264"`) **re-encodes that stream** — remux then degrades into
//! a partial transcode, which is slower and lossy for that stream.
//! Use this only when the target container requires a different codec
//! (e.g. forcing AAC audio for broad MP4 compatibility). Unknown codec
//! names are rejected up front via the runtime capability registry
//! when they are not `"copy"`/`"none"`.

use serde::{Deserialize, Serialize};

use crate::crates::multimedia_engine::error::{MediaError, MediaResult};
use crate::crates::multimedia_engine::model::{MediaLimits, MediaOutput};
use crate::crates::multimedia_engine::runtime::discovery::Runtime;
use crate::crates::multimedia_engine::runtime::process;
use crate::crates::multimedia_engine::runtime::tempdir::TempDir;

/// Options for [`remux`].
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct RemuxOptions {
    /// Output container name — a muxer name or common alias
    /// (`"mkv"`, `"mp4"`, `"webm"`, `"mov"`, `"avi"`, `"ts"`, `"ogg"`,
    /// `"wav"`, `"flac"`, `"mp3"`, `"aac"`, ...). Validated against the
    /// detected FFmpeg build's muxer list; unknown names return
    /// [`MediaError::Unsupported`].
    pub output_format: String,
    /// `-c:v` codec specifier. `None` or `"copy"` copies the video
    /// stream unchanged; a real encoder name re-encodes video.
    pub video_codec: Option<String>,
    /// `-c:a` codec specifier. `None` or `"copy"` copies the audio
    /// stream unchanged; a real encoder name re-encodes audio.
    pub audio_codec: Option<String>,
    /// MP4-family only: write the `moov` atom up front
    /// (`-movflags +faststart`) so the file streams without a full
    /// download. Ignored for non-MP4 containers (ffmpeg would reject
    /// the flag); a note is added to `MediaOutput::warnings` when the
    /// flag is set but not applicable.
    pub faststart: bool,
}

/// Map a user-facing container name/alias to the ffmpeg muxer name.
/// Returns `None` when `name` is empty or contains unsafe characters
/// (muxer names are plain `[a-z0-9_]`; anything else cannot be a real
/// muxer and would only reach ffmpeg as a bad `-f` value).
fn muxer_name(format: &str) -> Option<String> {
    let normalized = format.trim().trim_start_matches('.').to_lowercase();
    if normalized.is_empty()
        || !normalized
            .chars()
            .all(|c| c.is_ascii_lowercase() || c.is_ascii_digit() || c == '_')
    {
        return None;
    }
    let muxer = match normalized.as_str() {
        "mkv" | "matroska" => "matroska",
        "ts" | "mpegts" => "mpegts",
        "aac" | "adts" => "adts",
        "m4a" | "ipod" => "ipod",
        other => other,
    };
    Some(muxer.to_string())
}

/// Whether `muxer` belongs to the ISO-BMFF/QuickTime family that
/// accepts `-movflags`.
fn is_mp4_family(muxer: &str) -> bool {
    matches!(muxer, "mp4" | "mov" | "ipod")
}

/// Best-effort container sniffing from magic bytes, used only to pick
/// a plausible temp-file extension for the input. ffmpeg re-probes
/// the content itself; a wrong guess is harmless, a missing one just
/// makes demuxer selection slightly less certain.
fn sniff_extension(input: &[u8]) -> &'static str {
    if input.len() >= 4 {
        match &input[..4] {
            [0x1A, 0x45, 0xDF, 0xA3] => return "mkv", // EBML
            [b'O', b'g', b'g', b'S'] => return "ogg",
            [b'f', b'L', b'a', b'C'] => return "flac",
            [b'R', b'I', b'F', b'F'] => {
                if input.len() >= 12 && &input[8..12] == b"WAVE" {
                    return "wav";
                }
                if input.len() >= 12 && &input[8..12] == b"AVI " {
                    return "avi";
                }
                return "bin";
            }
            [b'I', b'D', b'3', _] => return "mp3",
            [b'#', b'!', b'A', b'M'] => return "amr",
            _ => {}
        }
        // ISO-BMFF: bytes 4..8 are "ftyp".
        if input.len() >= 8 && &input[4..8] == b"ftyp" {
            // Peek at the major brand for a more precise guess.
            let brand = &input[8..12.min(input.len())];
            return match brand {
                b"qt  " => "mov",
                b"M4A " | b"M4B " => "m4a",
                _ => "mp4",
            };
        }
        // JPEG/PNG/GIF/WebP — some "media" inputs are still images.
        if input.starts_with(b"\x89PNG") {
            return "png";
        }
        if input.starts_with(b"\xFF\xD8\xFF") {
            return "jpg";
        }
        if input.starts_with(b"GIF8") {
            return "gif";
        }
        if input.len() >= 12 && &input[8..12] == b"WEBP" {
            return "webp";
        }
        // MPEG audio frame sync (MP3 without ID3).
        if input[0] == 0xFF && (input[1] & 0xE0) == 0xE0 {
            return "mp3";
        }
    }
    "bin"
}

/// Remux `input` into `options.output_format` without re-encoding.
///
/// # Arguments
///
/// - `input` — complete media file bytes.
/// - `options` — see [`RemuxOptions`].
/// - `limits` — per-op byte/time limits (input cap, output read-back
///   cap, timeout).
/// - `rt` — the discovered FFmpeg [`Runtime`]; its capability
///   registry is consulted to validate the requested muxer and any
///   non-copy codec overrides before ffmpeg runs.
///
/// # Errors
///
/// - [`MediaError::LimitExceeded`] when `input` exceeds
///   `limits.max_input_bytes` or the produced file exceeds
///   `limits.max_output_bytes`.
/// - [`MediaError::Unsupported`] when `output_format` names no muxer
///   in this FFmpeg build, or a codec override names no encoder.
/// - [`MediaError::ToolFailed`] when ffmpeg cannot remux the input
///   into the requested container (incompatible stream parameters).
/// - [`MediaError::MissingTool`] / [`MediaError::Timeout`] / [`MediaError::Io`]
///   for tool and filesystem failures.
pub fn remux(
    input: &[u8],
    options: &RemuxOptions,
    limits: &MediaLimits,
    rt: &Runtime,
) -> MediaResult<MediaOutput> {
    if let Some(max) = limits.max_input_bytes
        && (input.len() as u64) > max
    {
        return Err(MediaError::LimitExceeded(format!(
            "input is {} bytes, exceeding max_input_bytes {max}",
            input.len()
        )));
    }

    let caps = rt.capabilities()?;

    let muxer = muxer_name(&options.output_format).ok_or_else(|| {
        MediaError::Unsupported(format!(
            "invalid output container name {:?}",
            options.output_format
        ))
    })?;
    if !caps.has_muxer(&muxer) {
        return Err(MediaError::Unsupported(format!(
            "container {:?} (muxer {muxer:?}) is not available in this FFmpeg build",
            options.output_format
        )));
    }

    // Codec overrides: "copy" (default) and "none" need no encoder;
    // anything else must be a real encoder in this build — a missing
    // one would otherwise only fail deep inside ffmpeg.
    for (kind, codec) in [
        ("video", options.video_codec.as_deref()),
        ("audio", options.audio_codec.as_deref()),
    ] {
        if let Some(name) = codec
            && name != "copy"
            && name != "none"
            && !caps.has_encoder(name)
        {
            return Err(MediaError::Unsupported(format!(
                "{kind} codec {name:?} is not an encoder in this FFmpeg build"
            )));
        }
    }

    let dir = TempDir::new("remux")?;
    let in_ext = sniff_extension(input);
    let in_name = format!("input.{in_ext}");
    dir.write(&in_name, input)?;

    let out_name = format!(
        "output.{}",
        options
            .output_format
            .trim()
            .trim_start_matches('.')
            .to_lowercase()
    );
    let out_path = dir.file(&out_name);

    let mut argv: Vec<std::ffi::OsString> = vec![
        "-hide_banner".into(),
        "-loglevel".into(),
        "error".into(),
        "-nostdin".into(),
        "-y".into(),
        "-i".into(),
        dir.file(&in_name).into_os_string(),
        "-c".into(),
        "copy".into(),
    ];
    if let Some(v) = &options.video_codec
        && v != "copy"
    {
        argv.extend(["-c:v".into(), v.clone().into()]);
    }
    if let Some(a) = &options.audio_codec
        && a != "copy"
    {
        argv.extend(["-c:a".into(), a.clone().into()]);
    }
    argv.extend(["-f".into(), muxer.clone().into()]);
    let mut warnings = Vec::new();
    if options.faststart {
        if is_mp4_family(&muxer) {
            argv.extend(["-movflags".into(), "+faststart".into()]);
        } else {
            warnings.push(format!(
                "faststart requested but container {muxer:?} is not MP4-family; flag ignored"
            ));
        }
    }
    argv.push(out_path.into_os_string());

    let out = process::run_checked("ffmpeg", &rt.ffmpeg, &argv, None, limits)?;
    if !out.stderr.is_empty() {
        warnings.push(out.stderr);
    }

    let bytes = dir.read_limited(&out_name, limits.max_output_bytes)?;
    if bytes.is_empty() {
        return Err(MediaError::ToolFailed {
            tool: "ffmpeg",
            status: "empty output".to_string(),
            detail: "remux produced a zero-byte file".to_string(),
        });
    }

    let mut result = MediaOutput::single(out_name, bytes);
    result.warnings = warnings;
    Ok(result)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::crates::multimedia_engine::runtime::discovery::Runtime;
    use crate::crates::multimedia_engine::runtime::process;

    /// Generate a tiny H.264/AAC mp4 via lavfi for use as remux input.
    fn tiny_mp4() -> Vec<u8> {
        let rt = Runtime::discover().expect("ffmpeg/ffprobe on PATH");
        let dir = TempDir::new("remux-test").unwrap();
        let out = dir.file("fixture.mp4");
        let args = process::args(&[
            "-hide_banner",
            "-loglevel",
            "error",
            "-nostdin",
            "-y",
            "-f",
            "lavfi",
            "-i",
            "testsrc=duration=1:size=64x64:rate=10",
            "-f",
            "lavfi",
            "-i",
            "sine=frequency=440:duration=1",
            "-c:v",
            "libopenh264",
            "-c:a",
            "aac",
            "-shortest",
            out.to_str().unwrap(),
        ]);
        process::run_checked("ffmpeg", &rt.ffmpeg, &args, None, &MediaLimits::default())
            .expect("fixture encode failed");
        let bytes = std::fs::read(&out).unwrap();
        assert!(!bytes.is_empty());
        bytes
    }

    #[test]
    fn remux_mp4_to_mkv_has_ebml_magic() {
        let rt = Runtime::discover().unwrap();
        let input = tiny_mp4();
        let opts = RemuxOptions {
            output_format: "mkv".into(),
            ..Default::default()
        };
        let out = remux(&input, &opts, &MediaLimits::default(), &rt).unwrap();
        let bytes = &out.primary.as_ref().unwrap().bytes;
        assert_eq!(out.primary.as_ref().unwrap().name, "output.mkv");
        // Matroska/WebM files begin with the EBML header 0x1A45DFA3.
        assert_eq!(&bytes[..4], &[0x1A, 0x45, 0xDF, 0xA3]);
    }

    #[test]
    fn remux_mp4_to_mp4_faststart_moves_moov_first() {
        let rt = Runtime::discover().unwrap();
        let input = tiny_mp4();
        let opts = RemuxOptions {
            output_format: "mp4".into(),
            faststart: true,
            ..Default::default()
        };
        let out = remux(&input, &opts, &MediaLimits::default(), &rt).unwrap();
        let bytes = &out.primary.as_ref().unwrap().bytes;
        assert_eq!(out.primary.as_ref().unwrap().name, "output.mp4");
        let moov = bytes
            .windows(4)
            .position(|w| w == b"moov")
            .expect("mp4 has a moov atom");
        let mdat = bytes
            .windows(4)
            .position(|w| w == b"mdat")
            .expect("mp4 has an mdat atom");
        assert!(
            moov < mdat,
            "faststart should place moov before mdat (moov={moov}, mdat={mdat})"
        );
    }

    #[test]
    fn remux_rejects_unknown_container_and_bad_codec() {
        let rt = Runtime::discover().unwrap();
        let input = tiny_mp4();
        let bad_format = RemuxOptions {
            output_format: "definitely-not-a-container".into(),
            ..Default::default()
        };
        match remux(&input, &bad_format, &MediaLimits::default(), &rt) {
            Err(MediaError::Unsupported(_)) => {}
            other => panic!("expected Unsupported, got {:?}", other.map(|_| ())),
        }

        let bad_codec = RemuxOptions {
            output_format: "mkv".into(),
            audio_codec: Some("no-such-codec-xyz".into()),
            ..Default::default()
        };
        match remux(&input, &bad_codec, &MediaLimits::default(), &rt) {
            Err(MediaError::Unsupported(_)) => {}
            other => panic!("expected Unsupported, got {:?}", other.map(|_| ())),
        }
    }
}
