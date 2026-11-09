//! Single-frame extraction at a timestamp.
//!
//! [`frame_at`] writes the input buffer to a temp file and runs
//! `ffmpeg -ss <t> -accurate_seek -i <in> ... -frames:v 1 <out>`,
//! producing one still image (`png`, `jpeg`, `webp` or `bmp`).
//!
//! `-ss` is placed **before** `-i` so ffmpeg seeks by keyframe/index
//! instead of decoding the whole file; `-accurate_seek` (the default
//! for decoded output, made explicit here) then decodes and discards
//! frames up to the exact timestamp, so the emitted frame is the one
//! at — not merely near — `at_secs`.
//!
//! Output formats map to FFmpeg's `image2` family of muxers and are
//! validated against [`Runtime::capabilities`] before invoking the
//! tool — a build without the encoder (e.g. `webp` when libwebp is
//! absent) yields [`MediaError::Unsupported`], never a silent
//! fallback.

use std::ffi::OsString;
use std::path::PathBuf;

use serde::{Deserialize, Serialize};

use crate::crates::multimedia_engine::error::{MediaError, MediaResult};
use crate::crates::multimedia_engine::model::{MediaLimits, MediaOutput};
use crate::crates::multimedia_engine::runtime::discovery::Runtime;
use crate::crates::multimedia_engine::runtime::process;
use crate::crates::multimedia_engine::runtime::tempdir::TempDir;

/// Options for [`frame_at`].
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct FrameOptions {
    /// Seek position in seconds from the start of the input.
    /// Must be finite and `>= 0.0`. Values past the end yield the
    /// last decodable frame or an ffmpeg error, depending on the
    /// container.
    pub at_secs: f64,
    /// Output image format: `"png"`, `"jpeg"`, `"webp"` or `"bmp"`
    /// (case-insensitive, leading dot tolerated).
    pub output_format: String,
    /// Target width in pixels for `-vf scale`. `None` keeps the
    /// source width. When only one of `width`/`height` is set, the
    /// other dimension is `-1` (auto, preserving aspect ratio).
    pub width: Option<u32>,
    /// Target height in pixels for `-vf scale`. `None` keeps the
    /// source height. See [`Self::width`].
    pub height: Option<u32>,
    /// Encoder quality hint, applied to `jpeg` (`-q:v`, 1..=31
    /// qscale mapped from 0..=100 where higher = better) and `webp`
    /// (`-quality`, 0..=100). Ignored for `png`/`bmp`.
    pub quality: Option<u8>,
}

impl Default for FrameOptions {
    fn default() -> Self {
        Self {
            at_secs: 0.0,
            output_format: "png".to_string(),
            width: None,
            height: None,
            quality: None,
        }
    }
}

/// Normalized output format: lowercased extension, canonical name and
/// the FFmpeg encoder candidates (first available in the build wins)
/// plus the image2-family muxer it requires.
struct FormatSpec {
    /// Canonical lowercase format name ("jpeg", not "jpg").
    canonical: &'static str,
    /// File extension for the output name.
    extension: &'static str,
    /// Preferred encoder names, in preference order.
    encoders: &'static [&'static str],
    /// MIME type for the produced [`MediaFile`].
    content_type: &'static str,
}

/// Resolve `output_format` to a [`FormatSpec`], or `Unsupported`.
///
/// The list is hardcoded because the accepted set is part of this
/// API's contract (not "whatever image2 supports"); availability of
/// each encoder in the detected build is checked separately via
/// [`Runtime::capabilities`].
fn format_spec(output_format: &str) -> MediaResult<FormatSpec> {
    let fmt = output_format
        .trim()
        .trim_start_matches('.')
        .to_ascii_lowercase();
    let spec = match fmt.as_str() {
        "png" => FormatSpec {
            canonical: "png",
            extension: "png",
            encoders: &["png"],
            content_type: "image/png",
        },
        "jpeg" | "jpg" | "jpe" => FormatSpec {
            canonical: "jpeg",
            extension: "jpg",
            encoders: &["mjpeg", "jpeg"],
            content_type: "image/jpeg",
        },
        "webp" => FormatSpec {
            canonical: "webp",
            extension: "webp",
            encoders: &["libwebp", "webp"],
            content_type: "image/webp",
        },
        "bmp" | "dib" => FormatSpec {
            canonical: "bmp",
            extension: "bmp",
            encoders: &["bmp"],
            content_type: "image/bmp",
        },
        other => {
            return Err(MediaError::Unsupported(format!(
                "unsupported frame output format '{other}' \
                 (expected png, jpeg, webp or bmp)"
            )));
        }
    };
    Ok(spec)
}

/// Extract one frame at `options.at_secs` seconds.
///
/// Runs `ffmpeg -ss <t> -accurate_seek -i <in> [-vf scale=w:h]
/// [-c:v <enc>] [quality opts] -frames:v 1 <out>` and reads the
/// produced image back under [`MediaLimits::max_output_bytes`].
///
/// # Errors
///
/// - [`MediaError::InvalidInput`] — empty/oversized input, negative
///   or non-finite `at_secs`, zero `width`/`height`.
/// - [`MediaError::Unsupported`] — unknown `output_format`, or the
///   needed image encoder/muxer is absent from this FFmpeg build.
/// - [`MediaError::ToolFailed`] / [`MediaError::Timeout`] — ffmpeg
///   rejected the input or produced no frame (e.g. seek past the end
///   of a stream with no decodable frame).
pub fn frame_at(
    input: &[u8],
    options: &FrameOptions,
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
    if !options.at_secs.is_finite() || options.at_secs < 0.0 {
        return Err(MediaError::InvalidInput(format!(
            "at_secs must be a finite number >= 0, got {}",
            options.at_secs
        )));
    }
    for (name, dim) in [("width", options.width), ("height", options.height)] {
        if dim == Some(0) {
            return Err(MediaError::InvalidInput(format!(
                "{name} must be > 0 when set"
            )));
        }
    }

    let spec = format_spec(&options.output_format)?;

    // Honest capabilities: the image2 muxer and at least one encoder
    // for the format must exist in this build.
    let caps = rt.capabilities()?;
    if !caps.has_muxer("image2") {
        return Err(MediaError::Unsupported(
            "image2 muxer is not available in this FFmpeg build".to_string(),
        ));
    }
    let encoder = spec
        .encoders
        .iter()
        .find(|enc| caps.has_encoder(enc))
        .copied()
        .ok_or_else(|| {
            MediaError::Unsupported(format!(
                "no {} image encoder ({}) in this FFmpeg build",
                spec.canonical,
                spec.encoders.join("/")
            ))
        })?;

    let dir = TempDir::new("frame-at")?;
    let input_name = "input.bin";
    // Neutral extension — ffmpeg probes by content; see transcode.rs.
    dir.write(input_name, input)?;

    let output_name = format!("frame.{}", spec.extension);
    let output_path: PathBuf = dir.file(&output_name);

    let mut argv: Vec<OsString> = vec![
        OsString::from("-hide_banner"),
        OsString::from("-loglevel"),
        OsString::from("error"),
        OsString::from("-nostdin"),
        OsString::from("-y"),
        // Input-side options: fast seek to the nearest seek point,
        // then decode-and-discard to the exact timestamp.
        OsString::from("-ss"),
        OsString::from(format_secs(options.at_secs)),
        OsString::from("-accurate_seek"),
        OsString::from("-i"),
        dir.file(input_name).into_os_string(),
    ];

    if options.width.is_some() || options.height.is_some() {
        // `-1` keeps the aspect ratio for the unset dimension; `-2`
        // rounds to even, which some encoders require — but image2
        // encoders accept odd sizes, so plain `-1` suffices.
        let w = dim_expr(options.width);
        let h = dim_expr(options.height);
        argv.push(OsString::from("-vf"));
        argv.push(OsString::from(format!("scale={w}:{h}")));
    }

    argv.push(OsString::from("-c:v"));
    argv.push(OsString::from(encoder));

    if let Some(q) = options.quality {
        match spec.canonical {
            // mjpeg `-q:v` is a qscale where lower = better; map the
            // 0..=100 "higher = better" API onto 31..=2.
            "jpeg" => {
                let qscale = 31 - (q as u32 * 29) / 100;
                argv.push(OsString::from("-q:v"));
                argv.push(OsString::from(qscale.max(2).to_string()));
            }
            // libwebp/webp `-quality` is already 0..=100.
            "webp" => {
                argv.push(OsString::from("-quality"));
                argv.push(OsString::from(q.to_string()));
            }
            // png/bmp have no quality knob (png uses -compression_level
            // 0..=9; out of scope for a u8 0..=100 API).
            _ => {}
        }
    }

    argv.push(OsString::from("-frames:v"));
    argv.push(OsString::from("1"));
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

    let mut result = MediaOutput::single(output_name, bytes);
    if let Some(file) = result.primary.as_mut() {
        file.content_type = Some(spec.content_type.to_string());
    }
    Ok(result)
}

/// Scale dimension expression: the set value, or `-1` (auto /
/// preserve aspect) when `None`.
fn dim_expr(dim: Option<u32>) -> String {
    dim.map(|d| d.to_string())
        .unwrap_or_else(|| "-1".to_string())
}

/// Format seconds for `-ss` without scientific notation or excess
/// precision (microsecond resolution is plenty for frame accuracy).
fn format_secs(secs: f64) -> String {
    let rounded = (secs * 1_000_000.0).round() / 1_000_000.0;
    let s = format!("{rounded:.6}");
    s.trim_end_matches('0').trim_end_matches('.').to_string()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::crates::multimedia_engine::runtime::process::run;

    fn runtime() -> Runtime {
        Runtime::discover().expect("ffmpeg/ffprobe must be on PATH")
    }

    fn limits() -> MediaLimits {
        MediaLimits::default()
    }

    /// Generate a 2-second 160x120 testsrc mp4 via lavfi.
    fn sample_input() -> Vec<u8> {
        let rt = runtime();
        let dir = TempDir::new("frame-at-test-in").expect("tempdir");
        let out_path: PathBuf = dir.file("sample.mp4");
        let argv = process::args(&[
            "-hide_banner",
            "-loglevel",
            "error",
            "-nostdin",
            "-y",
            "-f",
            "lavfi",
            "-i",
            "testsrc=duration=2:size=160x120:rate=10",
            "-c:v",
            "mpeg4",
            "-pix_fmt",
            "yuv420p",
            out_path.to_str().expect("utf8 path"),
        ]);
        run("ffmpeg", &rt.ffmpeg, &argv, None, &limits()).expect("generate sample");
        std::fs::read(&out_path).expect("read sample")
    }

    #[test]
    fn frame_at_extracts_png_with_magic_bytes() {
        let rt = runtime();
        let input = sample_input();
        assert!(!input.is_empty());

        let options = FrameOptions {
            at_secs: 1.0,
            output_format: "png".to_string(),
            ..Default::default()
        };
        let out = frame_at(&input, &options, &limits(), &rt).expect("frame_at png");
        let file = out.primary.expect("primary output");
        assert_eq!(file.name, "frame.png");
        assert_eq!(file.content_type.as_deref(), Some("image/png"));
        // PNG magic: 89 50 4E 47 0D 0A 1A 0A
        assert_eq!(&file.bytes[..8], b"\x89PNG\r\n\x1a\n");
        // A 160x120 testsrc frame compresses to well over 1 KiB.
        assert!(
            file.bytes.len() > 1024,
            "suspiciously small: {}",
            file.bytes.len()
        );
    }

    #[test]
    fn frame_at_extracts_jpeg() {
        let rt = runtime();
        let input = sample_input();

        let options = FrameOptions {
            at_secs: 1.5,
            output_format: "jpeg".to_string(),
            quality: Some(90),
            ..Default::default()
        };
        let out = frame_at(&input, &options, &limits(), &rt).expect("frame_at jpeg");
        let file = out.primary.expect("primary output");
        assert_eq!(file.name, "frame.jpg");
        assert_eq!(file.content_type.as_deref(), Some("image/jpeg"));
        // JPEG SOI marker.
        assert_eq!(&file.bytes[..2], b"\xff\xd8");
        assert!(
            file.bytes.len() > 512,
            "suspiciously small: {}",
            file.bytes.len()
        );
    }

    #[test]
    fn frame_at_scales_output() {
        let rt = runtime();
        let input = sample_input();

        let options = FrameOptions {
            at_secs: 0.5,
            output_format: "png".to_string(),
            width: Some(80),
            height: Some(60),
            ..Default::default()
        };
        let out = frame_at(&input, &options, &limits(), &rt).expect("frame_at scaled");
        let file = out.primary.expect("primary output");
        // IHDR width/height are big-endian u32 at offsets 16 and 20.
        assert!(file.bytes.len() > 24);
        let w = u32::from_be_bytes([
            file.bytes[16],
            file.bytes[17],
            file.bytes[18],
            file.bytes[19],
        ]);
        let h = u32::from_be_bytes([
            file.bytes[20],
            file.bytes[21],
            file.bytes[22],
            file.bytes[23],
        ]);
        assert_eq!((w, h), (80, 60), "unexpected PNG dimensions {w}x{h}");
    }

    #[test]
    fn frame_at_rejects_bad_input() {
        let rt = runtime();
        let input = sample_input();

        // Unsupported format.
        let options = FrameOptions {
            output_format: "tiff".to_string(),
            ..Default::default()
        };
        let err = frame_at(&input, &options, &limits(), &rt).expect_err("must fail");
        assert!(matches!(err, MediaError::Unsupported(_)), "got {err:?}");

        // Negative timestamp.
        let options = FrameOptions {
            at_secs: -1.0,
            ..Default::default()
        };
        let err = frame_at(&input, &options, &limits(), &rt).expect_err("must fail");
        assert!(matches!(err, MediaError::InvalidInput(_)), "got {err:?}");

        // Zero width.
        let options = FrameOptions {
            width: Some(0),
            ..Default::default()
        };
        let err = frame_at(&input, &options, &limits(), &rt).expect_err("must fail");
        assert!(matches!(err, MediaError::InvalidInput(_)), "got {err:?}");
    }
}
