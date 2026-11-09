//! Metadata stripping for privacy — remux without tags or chapters.
//!
//! [`strip_metadata`] runs `ffmpeg -map 0 -c copy` with metadata
//! mapping disabled, producing a byte-different but stream-identical
//! file: same media, no container tags, (optionally) no chapters and
//! no per-stream tags.
//!
//! # What gets stripped
//!
//! | Option | ffmpeg args | Effect |
//! |--------|-------------|--------|
//! | `format_tags` | `-map_metadata -1` | Container tags (title, artist, encoder, ...) |
//! | `stream_tags` | `-map_metadata:s:<idx> -1` per stream | Per-stream tags (language, handler_name, title) |
//! | `chapters` | `-map_chapters -1` | Chapter markers |
//!
//! Stream tags require probing the input first so each stream index
//! gets its own `-map_metadata:s:<idx> -1` argument — ffmpeg has no
//! "all streams" wildcard for metadata mapping.

use serde::{Deserialize, Serialize};

use crate::crates::multimedia_engine::error::{MediaError, MediaResult};
use crate::crates::multimedia_engine::model::{MediaLimits, MediaOutput};
use crate::crates::multimedia_engine::probe::types::ProbeReport;
use crate::crates::multimedia_engine::runtime::discovery::Runtime;
use crate::crates::multimedia_engine::runtime::process;
use crate::crates::multimedia_engine::runtime::tempdir::TempDir;

/// Options for [`strip_metadata`]. All fields default to `true` —
/// calling with `StripOptions::default()` removes everything.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct StripOptions {
    /// Strip container/format-level tags (`-map_metadata -1`).
    #[serde(default = "default_true")]
    pub format_tags: bool,
    /// Strip per-stream tags (`-map_metadata:s:<idx> -1` for every
    /// stream found by ffprobe).
    #[serde(default = "default_true")]
    pub stream_tags: bool,
    /// Strip chapter markers (`-map_chapters -1`).
    #[serde(default = "default_true")]
    pub chapters: bool,
}

fn default_true() -> bool {
    true
}

impl Default for StripOptions {
    fn default() -> Self {
        Self {
            format_tags: true,
            stream_tags: true,
            chapters: true,
        }
    }
}

/// Best-effort container sniff for temp file extensions.
fn sniff_extension(input: &[u8]) -> &'static str {
    if input.len() >= 4 {
        match &input[..4] {
            [0x1A, 0x45, 0xDF, 0xA3] => return "mkv",
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
            _ => {}
        }
        if input.len() >= 8 && &input[4..8] == b"ftyp" {
            return "mp4";
        }
        if input.starts_with(b"\x89PNG") {
            return "png";
        }
        if input.starts_with(b"\xFF\xD8\xFF") {
            return "jpg";
        }
        if input[0] == 0xFF && (input[1] & 0xE0) == 0xE0 {
            return "mp3";
        }
    }
    "bin"
}

/// Probe the staged input for stream indices (needed for per-stream
/// metadata mapping).
fn probe_stream_indices(
    input_path: &std::path::Path,
    limits: &MediaLimits,
    rt: &Runtime,
) -> MediaResult<Vec<u32>> {
    let args = process::args(&[
        "-v",
        "error",
        "-print_format",
        "json",
        "-show_streams",
        input_path.to_str().unwrap_or("input"),
    ]);
    let out = process::run("ffprobe", &rt.ffprobe, &args, None, limits)?;
    if !out.success || out.stdout.is_empty() {
        return Err(MediaError::InvalidInput(
            "ffprobe produced no stream list".to_string(),
        ));
    }
    let report: ProbeReport = serde_json::from_slice(&out.stdout)
        .map_err(|e| MediaError::InvalidInput(format!("ffprobe JSON parse failed: {e}")))?;
    Ok(report
        .streams
        .iter()
        .enumerate()
        .map(|(pos, s)| s.index.unwrap_or(pos as u32))
        .collect())
}

/// Strip metadata from `input` and return the re-muxed media.
///
/// # Errors
///
/// - [`MediaError::LimitExceeded`] when `input` exceeds
///   `limits.max_input_bytes` or the output exceeds
///   `limits.max_output_bytes`.
/// - [`MediaError::ToolFailed`] when ffmpeg cannot re-mux.
pub fn strip_metadata(
    input: &[u8],
    options: &StripOptions,
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

    let ext = sniff_extension(input);
    let dir = TempDir::new("strip")?;
    let in_name = format!("input.{ext}");
    let in_path = dir.write(&in_name, input)?;

    let mut argv: Vec<std::ffi::OsString> = vec![
        "-hide_banner".into(),
        "-loglevel".into(),
        "error".into(),
        "-nostdin".into(),
        "-y".into(),
        "-i".into(),
        in_path.into_os_string(),
        "-map".into(),
        "0".into(),
        "-c".into(),
        "copy".into(),
    ];

    if options.format_tags {
        argv.extend(["-map_metadata".into(), "-1".into()]);
    }
    if options.chapters {
        argv.extend(["-map_chapters".into(), "-1".into()]);
    }
    if options.stream_tags {
        let indices = probe_stream_indices(&dir.file(&in_name), limits, rt)?;
        for idx in indices {
            argv.push(format!("-map_metadata:s:{idx}").into());
            argv.push("-1".into());
        }
    }

    let out_name = format!("output.{ext}");
    argv.push(dir.file(&out_name).into_os_string());

    let out = process::run_checked("ffmpeg", &rt.ffmpeg, &argv, None, limits)?;

    let bytes = dir.read_limited(&out_name, limits.max_output_bytes)?;
    if bytes.is_empty() {
        return Err(MediaError::ToolFailed {
            tool: "ffmpeg",
            status: "empty output".to_string(),
            detail: "metadata strip produced a zero-byte file".to_string(),
        });
    }

    let mut result = MediaOutput::single(out_name, bytes);
    if !out.stderr.is_empty() {
        result.warnings.push(out.stderr);
    }
    Ok(result)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::crates::multimedia_engine::runtime::process;

    /// mp4 with a title tag + a chapter, for strip verification.
    fn tagged_mp4() -> Option<Vec<u8>> {
        let rt = Runtime::discover().ok()?;
        let dir = TempDir::new("strip-test").ok()?;
        let media = dir.file("media.mp4");
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
            "libx264",
            "-pix_fmt",
            "yuv420p",
            "-c:a",
            "aac",
            "-shortest",
            "-metadata",
            "title=Secret Title",
            media.to_str()?,
        ]);
        process::run_checked("ffmpeg", &rt.ffmpeg, &args, None, &MediaLimits::default()).ok()?;
        std::fs::read(&media).ok()
    }

    #[test]
    fn strip_removes_format_tags() {
        let Some(input) = tagged_mp4() else {
            eprintln!("skipping: ffmpeg lavfi/x264/aac unavailable");
            return;
        };
        let rt = Runtime::discover().unwrap();
        let out = strip_metadata(
            &input,
            &StripOptions::default(),
            &MediaLimits::default(),
            &rt,
        )
        .expect("strip_metadata failed");
        let stripped = &out.primary.as_ref().unwrap().bytes;

        // Re-probe: format tags should be gone.
        let dir = TempDir::new("strip-verify").unwrap();
        let p = dir.file("out.mp4");
        std::fs::write(&p, stripped).unwrap();
        let args = process::args(&[
            "-v",
            "error",
            "-print_format",
            "json",
            "-show_format",
            p.to_str().unwrap(),
        ]);
        let probe = process::run("ffprobe", &rt.ffprobe, &args, None, &MediaLimits::default())
            .expect("ffprobe failed");
        let report: ProbeReport = serde_json::from_slice(&probe.stdout).expect("ffprobe json");
        let title = report
            .format
            .as_ref()
            .and_then(|f| f.tags.get("title"))
            .cloned();
        assert_ne!(
            title.as_deref(),
            Some("Secret Title"),
            "title survived strip"
        );
    }

    #[test]
    fn strip_rejects_oversized_input() {
        let rt = match Runtime::discover() {
            Ok(rt) => rt,
            Err(_) => return,
        };
        let limits = MediaLimits {
            max_input_bytes: Some(4),
            ..MediaLimits::default()
        };
        assert!(strip_metadata(b"way too long", &StripOptions::default(), &limits, &rt).is_err());
    }
}
