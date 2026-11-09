//! Subtitle format conversion between sidecar formats.
//!
//! [`convert_subtitles`] transcodes a subtitle file from one
//! sidecar format to another using ffmpeg's subtitle codecs —
//! SRT ↔ ASS/SSA ↔ WebVTT. Unlike [`super::extract`] (which pulls
//! subtitles *out of* a media container), this op takes a bare
//! subtitle file as input.
//!
//! # Codec mapping
//!
//! | Format | ffmpeg decoder/encoder | Muxer |
//! |--------|------------------------|--------|
//! | SRT    | `subrip` (srt muxer)    | `srt`  |
//! | ASS    | `ass`                  | `ass`  |
//! | VTT    | `webvtt`               | `webvtt` |
//!
//! `Txt` reuses the SRT codec (plain text has no dedicated ffmpeg
//! subtitle codec — same convention as `extract::SubtitleFormat`).

use serde::{Deserialize, Serialize};

use crate::crates::multimedia_engine::error::{MediaError, MediaResult};
use crate::crates::multimedia_engine::model::{MediaLimits, MediaOutput};
use crate::crates::multimedia_engine::runtime::discovery::Runtime;
use crate::crates::multimedia_engine::runtime::process;
use crate::crates::multimedia_engine::runtime::tempdir::TempDir;

/// Sidecar subtitle formats for [`convert_subtitles`]. Mirrors
/// [`super::extract::SubtitleFormat`] — kept as a separate type so
/// this op's contract doesn't depend on the extract module.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
pub enum SubtitleFormat {
    /// SubRip `.srt`.
    #[default]
    Srt,
    /// Advanced SubStation Alpha `.ass`/`.ssa`.
    Ass,
    /// WebVTT `.vtt`.
    Vtt,
    /// Plain text `.txt` (SRT codec, `.txt` output).
    Txt,
}

impl SubtitleFormat {
    /// FFmpeg subtitle codec name.
    fn codec(self) -> &'static str {
        match self {
            Self::Srt | Self::Txt => "subrip",
            Self::Ass => "ass",
            Self::Vtt => "webvtt",
        }
    }

    /// FFmpeg demuxer name for decoding this format.
    fn demuxer(self) -> &'static str {
        match self {
            Self::Srt | Self::Txt => "srt",
            Self::Ass => "ass",
            Self::Vtt => "webvtt",
        }
    }

    /// FFmpeg muxer name for the output.
    fn muxer(self) -> &'static str {
        match self {
            Self::Srt | Self::Txt => "srt",
            Self::Ass => "ass",
            Self::Vtt => "webvtt",
        }
    }

    /// Output file extension (no dot).
    fn extension(self) -> &'static str {
        match self {
            Self::Srt => "srt",
            Self::Ass => "ass",
            Self::Vtt => "vtt",
            Self::Txt => "txt",
        }
    }

    /// MIME type.
    fn content_type(self) -> &'static str {
        match self {
            Self::Srt => "application/x-subrip",
            Self::Ass => "text/x-ssa",
            Self::Vtt => "text/vtt",
            Self::Txt => "text/plain",
        }
    }
}

/// Options for [`convert_subtitles`].
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ConvertOptions {
    /// Format of the input file. Required — subtitle content is
    /// ambiguous without it (SRT and VTT are both plain text).
    pub input_format: SubtitleFormat,
    /// Desired output format.
    pub output_format: SubtitleFormat,
}

impl Default for ConvertOptions {
    fn default() -> Self {
        Self {
            input_format: SubtitleFormat::Srt,
            output_format: SubtitleFormat::Vtt,
        }
    }
}

fn check_size(input: &[u8], limits: &MediaLimits) -> MediaResult<()> {
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

/// Convert `input` from `options.input_format` to
/// `options.output_format`.
///
/// Runs `ffmpeg -f <in_demuxer> -i input.ext -c:s <out_codec> -f
/// <out_muxer> output.ext`.
///
/// # Errors
///
/// - [`MediaError::LimitExceeded`] when `input` exceeds
///   `limits.max_input_bytes` or output exceeds
///   `limits.max_output_bytes`.
/// - [`MediaError::Unsupported`] when the needed subtitle codec or
///   muxer is absent from this FFmpeg build.
/// - [`MediaError::ToolFailed`] when ffmpeg cannot decode the input
///   (malformed timestamps, unknown format, ...).
pub fn convert_subtitles(
    input: &[u8],
    options: &ConvertOptions,
    limits: &MediaLimits,
    rt: &Runtime,
) -> MediaResult<MediaOutput> {
    check_size(input, limits)?;
    if input.is_empty() {
        return Err(MediaError::InvalidInput("empty subtitle input".to_string()));
    }

    let caps = rt.capabilities()?;
    let in_demuxer = options.input_format.demuxer();
    let out_codec = options.output_format.codec();
    let out_muxer = options.output_format.muxer();

    if !caps.has_demuxer(in_demuxer) {
        return Err(MediaError::Unsupported(format!(
            "subtitle demuxer {in_demuxer:?} is not available in this FFmpeg build"
        )));
    }
    if !caps.has_encoder(out_codec) {
        return Err(MediaError::Unsupported(format!(
            "subtitle encoder {out_codec:?} is not available in this FFmpeg build"
        )));
    }
    if !caps.has_muxer(out_muxer) {
        return Err(MediaError::Unsupported(format!(
            "subtitle muxer {out_muxer:?} is not available in this FFmpeg build"
        )));
    }

    let dir = TempDir::new("subconv")?;
    let in_ext = options.input_format.extension();
    let in_name = format!("input.{in_ext}");
    let in_path = dir.write(&in_name, input)?;

    let out_ext = options.output_format.extension();
    let out_name = format!("output.{out_ext}");
    let out_path = dir.file(&out_name);

    let args = process::args(&[
        "-hide_banner",
        "-loglevel",
        "error",
        "-nostdin",
        "-y",
        "-f",
        in_demuxer,
        "-i",
        in_path.to_str().unwrap_or("input"),
        "-c:s",
        out_codec,
        "-f",
        out_muxer,
        out_path.to_str().unwrap_or("output"),
    ]);
    let out = process::run_checked("ffmpeg", &rt.ffmpeg, &args, None, limits)?;

    let bytes = dir.read_limited(&out_name, limits.max_output_bytes)?;
    if bytes.is_empty() {
        return Err(MediaError::ToolFailed {
            tool: "ffmpeg",
            status: "empty output".to_string(),
            detail: "subtitle conversion produced a zero-byte file".to_string(),
        });
    }

    let mut result = MediaOutput::single(out_name, bytes);
    if let Some(primary) = result.primary.as_mut() {
        primary.content_type = Some(options.output_format.content_type().to_string());
    }
    if !out.stderr.is_empty() {
        result.warnings.push(out.stderr);
    }
    Ok(result)
}

#[cfg(test)]
mod tests {
    use super::*;

    const SRT: &str = "1\n00:00:00,000 --> 00:00:01,000\nHello\n\n\
                        2\n00:00:01,000 --> 00:00:02,000\nWorld\n";

    #[test]
    fn convert_srt_to_vtt() {
        let rt = match Runtime::discover() {
            Ok(rt) => rt,
            Err(_) => {
                eprintln!("skipping: ffmpeg/ffprobe not on PATH");
                return;
            }
        };
        let caps = match rt.capabilities() {
            Ok(c) => c,
            Err(_) => return,
        };
        if !(caps.has_demuxer("srt") && caps.has_encoder("webvtt") && caps.has_muxer("webvtt")) {
            eprintln!("skipping: srt/webvtt support unavailable");
            return;
        }

        let opts = ConvertOptions {
            input_format: SubtitleFormat::Srt,
            output_format: SubtitleFormat::Vtt,
        };
        let out = convert_subtitles(SRT.as_bytes(), &opts, &MediaLimits::default(), &rt)
            .expect("convert failed");
        let primary = out.primary.expect("primary");
        assert_eq!(primary.name, "output.vtt");
        let text = String::from_utf8_lossy(&primary.bytes);
        assert!(text.starts_with("WEBVTT"), "unexpected vtt:\n{text}");
        assert!(text.contains("Hello"), "missing cue text:\n{text}");
    }

    #[test]
    fn convert_rejects_empty_input() {
        let rt = match Runtime::discover() {
            Ok(rt) => rt,
            Err(_) => return,
        };
        let err = convert_subtitles(
            b"",
            &ConvertOptions::default(),
            &MediaLimits::default(),
            &rt,
        )
        .expect_err("must fail");
        assert!(matches!(err, MediaError::InvalidInput(_)));
    }
}
