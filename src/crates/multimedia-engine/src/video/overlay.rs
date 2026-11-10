//! Overlay an image (logo, watermark, bug) onto a video stream.
//!
//! [`overlay_image`] adds a second input — the overlay image — and
//! composites it with ffmpeg's `overlay` filter at a configurable
//! position. The video is re-encoded because pixels change; audio is
//! stream-copied.
//!
//! Positions are expressed as ffmpeg `overlay=x:y` expressions:
//! corner modes pin the image `margin` pixels from the edges,
//! `Center` centers it, `Tile` repeats it across the frame.

use std::ffi::OsString;

use serde::{Deserialize, Serialize};

use crate::crates::multimedia_engine::error::{MediaError, MediaResult};
use crate::crates::multimedia_engine::model::{MediaLimits, MediaOutput};
use crate::crates::multimedia_engine::runtime::discovery::Runtime;
use crate::crates::multimedia_engine::runtime::process;
use crate::crates::multimedia_engine::runtime::tempdir::TempDir;

/// Where on the frame the overlay image is placed.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
pub enum OverlayPosition {
    #[default]
    TopLeft,
    TopRight,
    BottomLeft,
    BottomRight,
    Center,
    /// Repeat the image across the whole frame (watermark tiling).
    Tile,
}

/// Options for [`overlay_image`].
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct OverlayOptions {
    /// Placement on the frame.
    pub position: OverlayPosition,
    /// Margin in pixels from the edges for corner positions
    /// (ignored for `Center`/`Tile`). Default `10`.
    #[serde(default = "default_margin")]
    pub margin: u32,
    /// Scale the overlay to this fraction of the main frame's
    /// smaller dimension before placing (`0.1` = 10%). `None` uses
    /// the image's native size (clamped to the frame).
    pub scale: Option<f64>,
    /// Overall opacity `0.0..=1.0` applied via the `format=rgba,
    /// colorchannelmixer=aa=` chain. Default `1.0`.
    #[serde(default = "default_opaque")]
    pub opacity: f64,
    /// Output container extension (no dot). `None` keeps the
    /// input's sniffed container.
    pub output_format: Option<String>,
}

fn default_margin() -> u32 {
    10
}
fn default_opaque() -> f64 {
    1.0
}

impl Default for OverlayOptions {
    fn default() -> Self {
        Self {
            position: OverlayPosition::TopLeft,
            margin: default_margin(),
            scale: None,
            opacity: default_opaque(),
            output_format: None,
        }
    }
}

/// Best-effort container sniff for the media input.
fn sniff_media_ext(input: &[u8]) -> &'static str {
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
    }
    "mp4"
}

/// Sniff the overlay image format for its temp extension.
fn sniff_image_ext(data: &[u8]) -> &'static str {
    if data.starts_with(b"\x89PNG") {
        "png"
    } else if data.starts_with(b"\xFF\xD8\xFF") {
        "jpg"
    } else if data.starts_with(b"GIF8") {
        "gif"
    } else if data.len() >= 12 && &data[8..12] == b"WEBP" {
        "webp"
    } else {
        "png"
    }
}

fn check_size(input: &[u8], limits: &MediaLimits, which: &str) -> MediaResult<()> {
    if let Some(max) = limits.max_input_bytes
        && (input.len() as u64) > max
    {
        return Err(MediaError::LimitExceeded(format!(
            "{which} input is {} bytes, exceeding max_input_bytes {max}",
            input.len()
        )));
    }
    Ok(())
}

/// Build the overlay filter chain: optional scale, optional alpha
/// multiply, then `overlay=x:y` or `tile`.
fn overlay_filter(opts: &OverlayOptions) -> String {
    let mut parts: Vec<String> = Vec::new();

    // Scale the overlay relative to the main frame.
    let scaled = match opts.scale {
        Some(f) if f.is_finite() && f > 0.0 && f <= 1.0 => {
            format!("scale=w='min(iw,main_w*{f})':h='min(ih,main_h*{f})'")
        }
        Some(_) => String::new(), // invalid scale rejected earlier
        None => String::new(),
    };

    // Opacity via alpha channel multiply.
    let alpha = if (opts.opacity - 1.0).abs() > f64::EPSILON {
        format!(",colorchannelmixer=aa={}", opts.opacity)
    } else {
        String::new()
    };

    match opts.position {
        OverlayPosition::Tile => {
            parts.push(format!("[1:v]{scaled}{alpha},format=rgba[tiled]"));
            parts.push("[0:v][tiled]overlay=0:0:shortest=1[vout]".to_string());
        }
        pos => {
            let (x, y) = match pos {
                OverlayPosition::TopLeft => {
                    (format!("{}", opts.margin), format!("{}", opts.margin))
                }
                OverlayPosition::TopRight => ("W-w-10".to_string(), format!("{}", opts.margin)),
                OverlayPosition::BottomLeft => (format!("{}", opts.margin), "H-h-10".to_string()),
                OverlayPosition::BottomRight => ("W-w-10".to_string(), "H-h-10".to_string()),
                OverlayPosition::Center => ("(W-w)/2".to_string(), "(H-h)/2".to_string()),
                OverlayPosition::Tile => unreachable!(),
            };
            // Fix margin inside expressions for right/bottom.
            let x = if matches!(
                pos,
                OverlayPosition::TopRight | OverlayPosition::BottomRight
            ) {
                format!("W-w-{}", opts.margin)
            } else {
                x
            };
            let y = if matches!(
                pos,
                OverlayPosition::BottomLeft | OverlayPosition::BottomRight
            ) {
                format!("H-h-{}", opts.margin)
            } else {
                y
            };
            parts.push(format!("[1:v]{scaled}{alpha},format=rgba[ov]"));
            parts.push(format!("[0:v][ov]overlay={x}:{y}[vout]"));
        }
    }
    parts.join(";")
}

/// Overlay `overlay_image` bytes onto `input`'s video at `options.position`.
///
/// # Errors
///
/// - [`MediaError::InvalidInput`] — empty inputs, non-finite or
///   out-of-range `scale`/`opacity`.
/// - [`MediaError::LimitExceeded`] — inputs exceed
///   `limits.max_input_bytes`, output exceeds `max_output_bytes`.
/// - [`MediaError::Unsupported`] — `overlay` filter or chosen video
///   encoder missing from this build.
/// - [`MediaError::ToolFailed`] — undecodable media or overlay image.
pub fn overlay_image(
    input: &[u8],
    overlay_image: &[u8],
    options: &OverlayOptions,
    limits: &MediaLimits,
    rt: &Runtime,
) -> MediaResult<MediaOutput> {
    check_size(input, limits, "media")?;
    check_size(overlay_image, limits, "overlay")?;
    if input.is_empty() || overlay_image.is_empty() {
        return Err(MediaError::InvalidInput(
            "media and overlay inputs must be non-empty".to_string(),
        ));
    }
    if let Some(s) = options.scale
        && (!s.is_finite() || s <= 0.0 || s > 1.0)
    {
        return Err(MediaError::InvalidInput(format!(
            "scale must be in (0.0, 1.0], got {s}"
        )));
    }
    if !options.opacity.is_finite() || !(0.0..=1.0).contains(&options.opacity) {
        return Err(MediaError::InvalidInput(format!(
            "opacity must be in [0.0, 1.0], got {}",
            options.opacity
        )));
    }

    let caps = rt.capabilities()?;
    if !caps.has_filter("overlay") {
        return Err(MediaError::Unsupported(
            "filter \"overlay\" is not available in this FFmpeg build".to_string(),
        ));
    }

    let out_ext = options
        .output_format
        .as_deref()
        .map(|s| s.trim().trim_start_matches('.').to_ascii_lowercase())
        .filter(|s| !s.is_empty())
        .unwrap_or_else(|| sniff_media_ext(input).to_string());
    let video_codec = match out_ext.as_str() {
        "webm" => "libvpx-vp9",
        "gif" => "gif",
        _ => "libx264",
    };
    if !caps.has_encoder(video_codec) {
        return Err(MediaError::Unsupported(format!(
            "video encoder {video_codec:?} is not available in this FFmpeg build"
        )));
    }

    let dir = TempDir::new("overlay")?;
    let in_ext = sniff_media_ext(input);
    let in_name = format!("input.{in_ext}");
    dir.write(&in_name, input)?;

    let img_ext = sniff_image_ext(overlay_image);
    let img_name = format!("overlay.{img_ext}");
    let img_path = dir.file(&img_name);
    dir.write(&img_name, overlay_image)?;

    let filter = overlay_filter(options);
    let out_name = format!("output.{out_ext}");

    let mut argv: Vec<OsString> = vec![
        "-hide_banner".into(),
        "-loglevel".into(),
        "error".into(),
        "-nostdin".into(),
        "-y".into(),
        "-i".into(),
        dir.file(&in_name).into_os_string(),
        "-i".into(),
        img_path.into_os_string(),
        "-filter_complex".into(),
        filter.into(),
        "-map".into(),
        "[vout]".into(),
        "-map".into(),
        "0:a?".into(),
        "-c:v".into(),
        video_codec.into(),
    ];
    match out_ext.as_str() {
        "webm" => argv.extend(["-c:a".into(), "libopus".into()]),
        "gif" => argv.extend(["-c:a".into(), "copy".into()]),
        _ => argv.extend(["-c:a".into(), "aac".into()]),
    }
    argv.push(dir.file(&out_name).into_os_string());

    let out = process::run_checked("ffmpeg", &rt.ffmpeg, &argv, None, limits)?;

    let bytes = dir.read_limited(&out_name, limits.max_output_bytes)?;
    if bytes.is_empty() {
        return Err(MediaError::ToolFailed {
            tool: "ffmpeg",
            status: "empty output".to_string(),
            detail: "overlay produced a zero-byte file".to_string(),
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

    #[test]
    fn overlay_filter_center_uses_centered_expr() {
        let f = overlay_filter(&OverlayOptions {
            position: OverlayPosition::Center,
            ..Default::default()
        });
        assert!(f.contains("overlay=(W-w)/2:(H-h)/2"), "got {f}");
    }

    #[test]
    fn overlay_rejects_bad_opacity() {
        let rt = match Runtime::discover() {
            Ok(rt) => rt,
            Err(_) => return,
        };
        let opts = OverlayOptions {
            opacity: 1.5,
            ..Default::default()
        };
        assert!(
            overlay_image(
                b"v".as_slice(),
                b"i".as_slice(),
                &opts,
                &MediaLimits::default(),
                &rt
            )
            .is_err()
        );
    }
}
