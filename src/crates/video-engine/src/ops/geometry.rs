//! Basic video geometry: resize/scale, crop, pad, rotate, flip.
//!
//! All ops re-encode (geometry filters are lossy by nature) using the
//! engine's standard H.264/AAC output path ([`h264_aac_args`]) and
//! preserve the input's container family via [`ext_for_format`],
//! falling back to `mp4` when the probed `format_name` is unknown.
//!
//! Inputs without a video stream (audio-only media) are rejected with
//! [`VideoError::InvalidInput`] — there is no frame to transform.
//!
//! # Limit policy
//!
//! [`check_limits`] is called with `check_pixels = true`, so
//! `max_pixels` bounds the *probed input* frame (decompression-bomb
//! guard). Where an op computes exact output dimensions up front —
//! `resize` (box upper bound), `crop`, `pad` — `max_pixels` is
//! additionally enforced on those output dims via
//! [`check_output_pixels`]. `rotate`'s output canvas is computed by
//! ffmpeg for arbitrary angles and `flip` preserves dimensions, so no
//! separate output check applies there; the input-frame bound still
//! holds.

use std::path::PathBuf;

use crate::crates::core::engine::EngineLimits;
use crate::crates::video_engine::error::{VideoError, VideoResult};
use crate::crates::video_engine::model::FitMode;
use crate::crates::video_engine::ops::probe::probe;
use crate::crates::video_engine::ops::{
    COMMAND_TIMEOUT, FFMPEG, TempDir, check_input, check_limits, ext_for_format, find_tool,
    h264_aac_args, read_output, run_tool, tool_failure, write_input,
};

/// Probed-and-staged input shared by every geometry op: validated
/// video dimensions, a scratch dir holding `input.bin`, and the output
/// container extension.
struct Prepared {
    width: u32,
    height: u32,
    dir: TempDir,
    input_path: PathBuf,
    ext: String,
}

impl Prepared {
    /// Run ffmpeg with `-vf <filter>` plus the standard H.264/AAC
    /// encode tail and read the produced file back as bytes.
    fn execute(&self, filter: &str) -> VideoResult<Vec<u8>> {
        let program = find_tool(FFMPEG)?;
        let out_path = self.dir.file(&format!("out.{}", self.ext));
        let mut args = vec![
            "-y".to_string(),
            "-v".to_string(),
            "error".to_string(),
            "-i".to_string(),
            self.input_path.to_string_lossy().into_owned(),
            "-vf".to_string(),
            filter.to_string(),
        ];
        args.extend(h264_aac_args()?);
        args.push(out_path.to_string_lossy().into_owned());

        let output = run_tool(FFMPEG, &program, &args, COMMAND_TIMEOUT)?;
        if !output.status.success() {
            return Err(tool_failure(FFMPEG, &output));
        }
        read_output(&out_path)
    }
}

/// Probe `input`, enforce the input-side limits, and stage it in a
/// [`TempDir`]. Fails with [`VideoError::InvalidInput`] when the media
/// has no video stream or the stream reports no dimensions.
fn prepare(input: &[u8], limits: &EngineLimits, tag: &str) -> VideoResult<Prepared> {
    let input_len = check_input(input)?;
    find_tool(FFMPEG)?;
    let info = probe(input)?;
    if !info.has_video {
        return Err(VideoError::InvalidInput(
            "geometry operations require a video stream".to_string(),
        ));
    }
    let (Some(width), Some(height)) = (info.width, info.height) else {
        return Err(VideoError::InvalidInput(
            "video stream reports no dimensions".to_string(),
        ));
    };
    // max_pixels bounds the probed input frame here; ops that know
    // their output dims call check_output_pixels additionally.
    check_limits(&info, input_len, limits, true)?;
    let ext = info
        .format_name
        .as_deref()
        .and_then(ext_for_format)
        .unwrap_or("mp4")
        .to_string();
    let dir = TempDir::new(tag)?;
    let input_path = write_input(&dir, "input.bin", input)?;
    Ok(Prepared {
        width,
        height,
        dir,
        input_path,
        ext,
    })
}

/// Enforce `limits.max_pixels` on computed output dimensions.
fn check_output_pixels(limits: &EngineLimits, width: u64, height: u64) -> VideoResult<()> {
    if let Some(max) = limits.max_pixels {
        let pixels = width.saturating_mul(height);
        if pixels > max {
            return Err(VideoError::LimitExceeded(format!(
                "output frame is {width}x{height} = {pixels} pixels, \
                 exceeding max_pixels {max}"
            )));
        }
    }
    Ok(())
}

/// Compute the dimension on the "other" axis that preserves the input
/// aspect ratio. `target` is the requested size on one axis,
/// `in_target_axis`/`in_other_axis` the probed input sizes on the same
/// and opposite axes. Result is clamped to `[1, u32::MAX]`.
fn preserve_aspect(target: u32, in_target_axis: u32, in_other_axis: u32) -> u32 {
    let v = (u64::from(target) * u64::from(in_other_axis) + u64::from(in_target_axis) / 2)
        / u64::from(in_target_axis);
    v.clamp(1, u64::from(u32::MAX)) as u32
}

/// Build the `-vf` filtergraph for [`resize`]. All expressions avoid
/// `,` inside filter arguments, so no escaping is needed beyond the
/// filtergraph's own `,`-separated filter list.
fn resize_filter(w: u32, h: u32, fit: FitMode) -> String {
    match fit {
        // Exact box, aspect ignored.
        FitMode::Stretch => format!("scale={w}:{h}"),
        // Largest centered image fitting inside the box; output may be
        // smaller than the box on one axis.
        FitMode::Fit => format!("scale={w}:{h}:force_original_aspect_ratio=decrease"),
        // Fit inside the box, then pad the remainder (letterbox).
        FitMode::Contain => format!(
            "scale={w}:{h}:force_original_aspect_ratio=decrease,\
             pad={w}:{h}:(ow-iw)/2:(oh-ih)/2"
        ),
        // Fill the box (may overflow), then center-crop to it.
        FitMode::Cover => format!(
            "scale={w}:{h}:force_original_aspect_ratio=increase,\
             crop={w}:{h}:(iw-ow)/2:(ih-oh)/2"
        ),
    }
}

/// Resize `input` to fit `width`×`height` per `fit`
/// ([`FitMode::Fit`]/`Cover`/`Contain`/`Stretch`). `0` in either
/// dimension means "auto" (preserve aspect). Output keeps the input
/// container; re-encodes via the standard H.264/AAC path.
pub(crate) fn resize(
    input: &[u8],
    width: u32,
    height: u32,
    fit: FitMode,
    limits: &EngineLimits,
) -> VideoResult<Vec<u8>> {
    if width == 0 && height == 0 {
        return Err(VideoError::InvalidInput(
            "resize requires at least one non-zero dimension".to_string(),
        ));
    }
    let ctx = prepare(input, limits, "resize")?;

    // Resolve "auto" dimensions against the probed aspect ratio; once
    // both dims are concrete the aspect is already preserved, so every
    // FitMode reduces to the same exact scale.
    let (w, h) = match (width, height) {
        (0, h) => (preserve_aspect(h, ctx.height, ctx.width), h),
        (w, 0) => (w, preserve_aspect(w, ctx.width, ctx.height)),
        (w, h) => (w, h),
    };
    // Fit's output is bounded by the box; the other modes are exact.
    // Using the box dims is a conservative upper bound either way.
    check_output_pixels(limits, u64::from(w), u64::from(h))?;

    ctx.execute(&resize_filter(w, h, fit))
}

/// Crop `input` to the rectangle `(x, y, w, h)` in source pixels.
/// The rectangle must lie fully inside the probed frame — partial or
/// overflowing crops are rejected rather than silently clamped.
pub(crate) fn crop(
    input: &[u8],
    x: u32,
    y: u32,
    w: u32,
    h: u32,
    limits: &EngineLimits,
) -> VideoResult<Vec<u8>> {
    if w == 0 || h == 0 {
        return Err(VideoError::InvalidInput(format!(
            "crop width and height must be positive, got {w}x{h}"
        )));
    }
    let ctx = prepare(input, limits, "crop")?;
    if u64::from(x) + u64::from(w) > u64::from(ctx.width)
        || u64::from(y) + u64::from(h) > u64::from(ctx.height)
    {
        return Err(VideoError::InvalidInput(format!(
            "crop rectangle ({x}, {y}, {w}x{h}) exceeds the \
             {}x{} input frame",
            ctx.width, ctx.height
        )));
    }
    check_output_pixels(limits, u64::from(w), u64::from(h))?;
    ctx.execute(&format!("crop={w}:{h}:{x}:{y}"))
}

/// Characters allowed in a pad color spec — anything else could break
/// the `:`-separated filter argument list, so it is rejected up front.
fn valid_pad_color(color: &str) -> bool {
    !color.is_empty()
        && color
            .chars()
            .all(|c| c.is_ascii_alphanumeric() || matches!(c, '#' | 'x' | 'X' | '.' | '_' | '-'))
}

/// Pad `input` with `color` (e.g. `"black"`, `"0xRRGGBB"`) by the
/// given pixel amounts on each side. Output frame is
/// `(width + left + right)` × `(height + top + bottom)`.
pub(crate) fn pad(
    input: &[u8],
    left: u32,
    right: u32,
    top: u32,
    bottom: u32,
    color: Option<&str>,
    limits: &EngineLimits,
) -> VideoResult<Vec<u8>> {
    let color = color.unwrap_or("black");
    if !valid_pad_color(color) {
        return Err(VideoError::InvalidInput(format!(
            "invalid pad color '{color}'"
        )));
    }
    let ctx = prepare(input, limits, "pad")?;
    let out_w = u64::from(ctx.width) + u64::from(left) + u64::from(right);
    let out_h = u64::from(ctx.height) + u64::from(top) + u64::from(bottom);
    check_output_pixels(limits, out_w, out_h)?;
    ctx.execute(&format!("pad={out_w}:{out_h}:{left}:{top}:{color}"))
}

/// Rotate `input` by `degrees` (arbitrary angle; 90/180/270 should
/// prefer the lossless `transpose` path in `geometry_adv`). Fill
/// color defaults to black. ffmpeg's `rotate` filter expands the
/// output canvas automatically for non-multiple-of-90 angles.
pub(crate) fn rotate(input: &[u8], degrees: f64, limits: &EngineLimits) -> VideoResult<Vec<u8>> {
    if !degrees.is_finite() {
        return Err(VideoError::InvalidInput(format!(
            "degrees must be a finite number, got {degrees}"
        )));
    }
    let ctx = prepare(input, limits, "rotate")?;
    let radians = degrees.to_radians();
    // ffmpeg's rotate defaults ow/oh to the input size, clipping
    // right-angle rotations; pass the rotated bounding box explicitly
    // (rounded up to even for yuv420p).
    let (w, h) = (f64::from(ctx.width), f64::from(ctx.height));
    let sin = radians.sin().abs();
    let cos = radians.cos().abs();
    let out_w = ((w * cos + h * sin).ceil() as u64 + 1) & !1;
    let out_h = ((w * sin + h * cos).ceil() as u64 + 1) & !1;
    check_output_pixels(limits, out_w, out_h)?;
    // Six decimals (~0.00006°) is far below pixel precision.
    ctx.execute(&format!(
        "rotate={radians:.6}:ow={out_w}:oh={out_h}:fillcolor=black"
    ))
}

/// Mirror `input` horizontally or vertically.
pub(crate) fn flip(input: &[u8], horizontal: bool, limits: &EngineLimits) -> VideoResult<Vec<u8>> {
    let ctx = prepare(input, limits, "flip")?;
    ctx.execute(if horizontal { "hflip" } else { "vflip" })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::crates::video_engine::ops::{FFPROBE, find_tool};

    /// Encode a ~1s 160x120 H.264+AAC clip via lavfi `testsrc` +
    /// `sine`. Returns `None` when ffmpeg/ffprobe or the openh264
    /// encoder is unavailable so tests skip cleanly.
    fn fixture() -> Option<Vec<u8>> {
        let ffmpeg = find_tool(FFMPEG).ok()?;
        find_tool(FFPROBE).ok()?;
        let dir = TempDir::new("geom-test").ok()?;
        let out = dir.file("fixture.mp4");
        let args = vec![
            "-y".to_string(),
            "-v".to_string(),
            "error".to_string(),
            "-f".to_string(),
            "lavfi".to_string(),
            "-i".to_string(),
            "testsrc=size=160x120:rate=10:duration=1".to_string(),
            "-f".to_string(),
            "lavfi".to_string(),
            "-i".to_string(),
            "sine=frequency=440:duration=1".to_string(),
            "-c:v".to_string(),
            "libopenh264".to_string(),
            "-pix_fmt".to_string(),
            "yuv420p".to_string(),
            "-c:a".to_string(),
            "aac".to_string(),
            "-shortest".to_string(),
            out.to_string_lossy().into_owned(),
        ];
        let output = run_tool(FFMPEG, &ffmpeg, &args, COMMAND_TIMEOUT).ok()?;
        if !output.status.success() {
            return None;
        }
        read_output(&out).ok()
    }

    fn dims(bytes: &[u8]) -> (u32, u32) {
        let info = probe(bytes).expect("output must probe");
        (info.width.expect("width"), info.height.expect("height"))
    }

    #[test]
    fn resize_fit_stays_within_box() {
        let Some(fx) = fixture() else { return };
        let out =
            resize(&fx, 80, 60, FitMode::Fit, &EngineLimits::default()).expect("resize failed");
        let (w, h) = dims(&out);
        assert!(w > 0 && h > 0 && w <= 80 && h <= 60, "got {w}x{h}");
    }

    #[test]
    fn crop_produces_exact_dims() {
        let Some(fx) = fixture() else { return };
        let out = crop(&fx, 10, 20, 80, 60, &EngineLimits::default()).expect("crop failed");
        assert_eq!(dims(&out), (80, 60));
    }

    #[test]
    fn pad_adds_requested_border() {
        let Some(fx) = fixture() else { return };
        let out = pad(&fx, 5, 7, 11, 13, None, &EngineLimits::default()).expect("pad failed");
        assert_eq!(dims(&out), (172, 144));
    }

    #[test]
    fn flip_preserves_dims() {
        let Some(fx) = fixture() else { return };
        let out = flip(&fx, true, &EngineLimits::default()).expect("flip failed");
        assert_eq!(dims(&out), (160, 120));
    }

    #[test]
    fn rotate_90_swaps_dims() {
        let Some(fx) = fixture() else { return };
        let out = rotate(&fx, 90.0, &EngineLimits::default()).expect("rotate failed");
        let (w, h) = dims(&out);
        // ffmpeg's rotate expands the canvas by rounding; allow a few px.
        assert!(
            (i64::from(w) - 120).abs() <= 3 && (i64::from(h) - 160).abs() <= 3,
            "expected ~120x160, got {w}x{h}"
        );
    }

    #[test]
    fn invalid_crop_is_rejected() {
        let Some(fx) = fixture() else { return };
        // x + w overflows the 160px width.
        assert!(matches!(
            crop(&fx, 150, 0, 20, 20, &EngineLimits::default()),
            Err(VideoError::InvalidInput(_))
        ));
        // Zero-size crop.
        assert!(matches!(
            crop(&fx, 0, 0, 0, 20, &EngineLimits::default()),
            Err(VideoError::InvalidInput(_))
        ));
    }

    #[test]
    fn resize_both_zero_is_rejected() {
        let Some(fx) = fixture() else { return };
        assert!(matches!(
            resize(&fx, 0, 0, FitMode::Fit, &EngineLimits::default()),
            Err(VideoError::InvalidInput(_))
        ));
    }
}
