//! Advanced geometry: letterbox/pillarbox, autocrop, transpose,
//! aspect-ratio conversion (portrait↔landscape).
//!
//! Like [`super::geometry`], every op re-encodes through the standard
//! H.264/AAC path ([`h264_aac_args`], which resolves `libx264` then
//! `libopenh264`) and preserves the input's container family via
//! [`ext_for_format`]. Inputs without a video stream are rejected with
//! [`VideoError::InvalidInput`].
//!
//! # Limit policy
//!
//! [`check_limits`] bounds the probed *input* frame (`max_pixels`,
//! decompression-bomb guard). Where output dimensions are known up
//! front — letterbox/pillarbox targets, transpose's swapped dims,
//! autocrop's detected rect — `max_pixels` is additionally enforced on
//! them.

use std::path::PathBuf;

use crate::crates::core::engine::EngineLimits;
use crate::crates::video_engine::error::{VideoError, VideoResult};
use crate::crates::video_engine::ops::probe::probe;
use crate::crates::video_engine::ops::{
    COMMAND_TIMEOUT, FFMPEG, TempDir, check_input, check_limits, ext_for_format, find_tool,
    h264_aac_args, read_output, run_tool, tool_failure, write_input,
};

/// Probed-and-staged input shared by the geometry-adv ops.
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
/// [`TempDir`]. Mirrors `geometry::prepare`.
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

/// Scale `input` to `width`×`height` and pad (letterbox) to exactly
/// that frame, preserving aspect ratio.
///
/// The filtergraph is `scale=<w>:<h>:force_original_aspect_ratio=decrease`
/// followed by `pad=<w>:<h>:(ow-iw)/2:(oh-iw)/2:black`-style centering —
/// the same proven pair `geometry::resize` uses for [`FitMode::Contain`].
/// The scale expression deliberately does *not* self-reference `ow`/`oh`
/// (`scale=iw*min(ow/iw\,oh/ih):...` fails: ffmpeg rejects
/// self-referencing width expressions), and no `,` appears inside any
/// filter argument so no escaping is needed. `pad`'s `ow`/`oh` are the
/// requested output dims and `iw`/`ih` the scaled frame, so the output
/// is exactly `width`×`height` with black bars.
pub(crate) fn letterbox(
    input: &[u8],
    width: u32,
    height: u32,
    limits: &EngineLimits,
) -> VideoResult<Vec<u8>> {
    fit_pad(input, width, height, limits, "letterbox")
}

/// Same as [`letterbox`] but pillarbox is the same operation for
/// landscape content — kept as a separate entry point for API
/// symmetry (both produce a `width`×`height` frame with bars).
pub(crate) fn pillarbox(
    input: &[u8],
    width: u32,
    height: u32,
    limits: &EngineLimits,
) -> VideoResult<Vec<u8>> {
    fit_pad(input, width, height, limits, "pillarbox")
}

/// Shared implementation for [`letterbox`]/[`pillarbox`].
fn fit_pad(
    input: &[u8],
    width: u32,
    height: u32,
    limits: &EngineLimits,
    tag: &str,
) -> VideoResult<Vec<u8>> {
    if width == 0 || height == 0 {
        return Err(VideoError::InvalidInput(format!(
            "{tag} requires positive width and height, got {width}x{height}"
        )));
    }
    // The H.264/yuv420p encode path needs even output dimensions.
    if width % 2 == 1 || height % 2 == 1 {
        return Err(VideoError::InvalidInput(format!(
            "{tag} target {width}x{height} must be even for yuv420p encoding"
        )));
    }
    let ctx = prepare(input, limits, tag)?;
    check_output_pixels(limits, u64::from(width), u64::from(height))?;
    ctx.execute(&format!(
        "scale={width}:{height}:force_original_aspect_ratio=decrease,\
         pad={width}:{height}:(ow-iw)/2:(oh-ih)/2:black"
    ))
}

/// Parse the last `crop=w:h:x:y` rectangle reported by ffmpeg's
/// `cropdetect` filter on stderr. Returns `None` when no crop line was
/// emitted (the filter only logs once its internal stability counter
/// is satisfied, so short clips may produce none).
fn parse_last_crop(stderr: &[u8]) -> Option<(u32, u32, u32, u32)> {
    let text = String::from_utf8_lossy(stderr);
    let mut found = None;
    for line in text.lines() {
        let Some(pos) = line.find("crop=") else {
            continue;
        };
        let spec = &line[pos + "crop=".len()..];
        let spec = spec.split_whitespace().next().unwrap_or(spec);
        let parts: Vec<&str> = spec.split(':').collect();
        if parts.len() == 4 {
            if let (Ok(w), Ok(h), Ok(x), Ok(y)) = (
                parts[0].parse::<u32>(),
                parts[1].parse::<u32>(),
                parts[2].parse::<u32>(),
                parts[3].parse::<u32>(),
            ) {
                found = Some((w, h, x, y));
            }
        }
    }
    found
}

/// Detect and remove black borders via ffmpeg's `cropdetect` filter;
/// returns the cropped video. Fails honestly when no crop is found.
///
/// Two passes: pass 1 runs `cropdetect` to `/dev/null` (`-f null -`) at
/// default log level — the filter reports `crop=w:h:x:y` on stderr once
/// per stabilized frame, and the *last* value is authoritative. Pass 2
/// applies `crop=w:h:x:y` and re-encodes.
///
/// The filter uses `round=2` rather than the documented `24:16:0`
/// default round of 16: `cropdetect` quantizes reported dimensions to a
/// multiple of `round`, so `round=16` reports a *smaller* rect (e.g.
/// `160:112:0:4` for a borderless 160x120 frame — verified on ffmpeg
/// 7.1.5) and would crop content that has no border. `round=2` keeps
/// dimensions even (required by yuv420p) and reports the true full
/// frame (`160:120:0:0`) for borderless input.
pub(crate) fn autocrop(input: &[u8], limits: &EngineLimits) -> VideoResult<Vec<u8>> {
    let input_len = check_input(input)?;
    let program = find_tool(FFMPEG)?;
    let info = probe(input)?;
    if !info.has_video {
        return Err(VideoError::InvalidInput(
            "autocrop requires a video stream".to_string(),
        ));
    }
    let (Some(in_w), Some(in_h)) = (info.width, info.height) else {
        return Err(VideoError::InvalidInput(
            "video stream reports no dimensions".to_string(),
        ));
    };
    check_limits(&info, input_len, limits, true)?;
    let ext = info
        .format_name
        .as_deref()
        .and_then(ext_for_format)
        .unwrap_or("mp4")
        .to_string();

    let dir = TempDir::new("autocrop")?;
    let input_path = write_input(&dir, "input.bin", input)?;

    // Pass 1: detection only. Default log level (info) is required —
    // cropdetect's `crop=` lines are AV_LOG_INFO and `-v error` would
    // suppress them.
    let detect_args = vec![
        "-y".to_string(),
        "-i".to_string(),
        input_path.to_string_lossy().into_owned(),
        "-vf".to_string(),
        "cropdetect=limit=24:round=2:reset=0".to_string(),
        "-f".to_string(),
        "null".to_string(),
        "-".to_string(),
    ];
    let detect = run_tool(FFMPEG, &program, &detect_args, COMMAND_TIMEOUT)?;
    if !detect.status.success() {
        return Err(tool_failure(FFMPEG, &detect));
    }
    let Some((w, h, x, y)) = parse_last_crop(&detect.stderr) else {
        return Err(VideoError::InvalidInput(
            "no cropable border detected".to_string(),
        ));
    };
    if w == 0 || h == 0 {
        return Err(VideoError::InvalidInput(
            "cropdetect reported a degenerate crop".to_string(),
        ));
    }
    // The crop must lie inside the probed frame.
    if u64::from(x) + u64::from(w) > u64::from(in_w)
        || u64::from(y) + u64::from(h) > u64::from(in_h)
    {
        return Err(VideoError::InvalidInput(format!(
            "cropdetect rect ({x}, {y}, {w}x{h}) exceeds the {in_w}x{in_h} input frame"
        )));
    }
    if w == in_w && h == in_h {
        return Err(VideoError::InvalidInput(
            "no cropable border detected".to_string(),
        ));
    }
    check_output_pixels(limits, u64::from(w), u64::from(h))?;

    // Pass 2: apply the crop and re-encode.
    let ctx = Prepared {
        width: in_w,
        height: in_h,
        dir,
        input_path,
        ext,
    };
    ctx.execute(&format!("crop={w}:{h}:{x}:{y}"))
}

/// ffmpeg `transpose` values, verified on ffmpeg 7.1.5 with an
/// asymmetric red|blue test pattern: `1` rotates 90° clockwise
/// (top-left output pixel comes from bottom-left input), `2` rotates
/// 90° counter-clockwise. Output dimensions always swap w↔h.
fn transpose_value(clockwise: bool) -> &'static str {
    if clockwise { "1" } else { "2" }
}

/// Rotate 90° clockwise (`clockwise = true`) or counter-clockwise
/// using the `transpose` filter, re-encoded via H.264/AAC. Output
/// dimensions are the input's swapped.
pub(crate) fn transpose(
    input: &[u8],
    clockwise: bool,
    limits: &EngineLimits,
) -> VideoResult<Vec<u8>> {
    let ctx = prepare(input, limits, "transpose")?;
    check_output_pixels(limits, u64::from(ctx.height), u64::from(ctx.width))?;
    ctx.execute(&format!("transpose={}", transpose_value(clockwise)))
}

/// Convert between portrait and landscape orientation by rotating
/// 90° and swapping to the requested orientation.
///
/// Orientation is decided from the probed frame: `width > height` is
/// landscape (a square frame counts as landscape). When the input
/// already matches the requested orientation the input bytes are
/// returned unchanged — a documented pass-through that still validates
/// the input (probe + limits) before deciding.
pub(crate) fn orient(input: &[u8], landscape: bool, limits: &EngineLimits) -> VideoResult<Vec<u8>> {
    let ctx = prepare(input, limits, "orient")?;
    let is_landscape = ctx.width >= ctx.height;
    if is_landscape == landscape {
        return Ok(input.to_vec());
    }
    check_output_pixels(limits, u64::from(ctx.height), u64::from(ctx.width))?;
    // Either 90° direction swaps the orientation; clockwise is used
    // uniformly so the operation is deterministic.
    ctx.execute("transpose=1")
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::crates::video_engine::ops::probe::probe;
    use crate::crates::video_engine::ops::{FFPROBE, find_tool};

    /// Encode a ~1s 160x120 H.264+AAC clip via lavfi `testsrc` +
    /// `sine`. Returns `None` when ffmpeg/ffprobe or the openh264
    /// encoder is unavailable so tests skip cleanly.
    fn fixture() -> Option<Vec<u8>> {
        let ffmpeg = find_tool(FFMPEG).ok()?;
        find_tool(FFPROBE).ok()?;
        let dir = TempDir::new("geoadv-test").ok()?;
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
    fn letterbox_produces_exact_dims() {
        let Some(fx) = fixture() else { return };
        let out = letterbox(&fx, 320, 240, &EngineLimits::default()).expect("letterbox failed");
        assert_eq!(dims(&out), (320, 240));
    }

    #[test]
    fn pillarbox_produces_exact_dims() {
        let Some(fx) = fixture() else { return };
        let out = pillarbox(&fx, 200, 200, &EngineLimits::default()).expect("pillarbox failed");
        assert_eq!(dims(&out), (200, 200));
    }

    #[test]
    fn letterbox_rejects_zero_and_odd_dims() {
        let Some(fx) = fixture() else { return };
        assert!(matches!(
            letterbox(&fx, 0, 100, &EngineLimits::default()),
            Err(VideoError::InvalidInput(_))
        ));
        assert!(matches!(
            letterbox(&fx, 101, 100, &EngineLimits::default()),
            Err(VideoError::InvalidInput(_))
        ));
    }

    #[test]
    fn transpose_swaps_dims_both_directions() {
        let Some(fx) = fixture() else { return };
        for cw in [true, false] {
            let out = transpose(&fx, cw, &EngineLimits::default()).expect("transpose failed");
            assert_eq!(dims(&out), (120, 160));
        }
    }

    #[test]
    fn orient_rotates_to_requested_orientation() {
        let Some(fx) = fixture() else { return };
        // 160x120 is landscape: portrait request rotates, landscape
        // request passes the input through unchanged.
        let portrait = orient(&fx, false, &EngineLimits::default()).expect("orient failed");
        assert_eq!(dims(&portrait), (120, 160));
        let landscape = orient(&fx, true, &EngineLimits::default()).expect("orient failed");
        assert_eq!(landscape, fx, "matching orientation must pass through");
        // And a portrait input converts back to landscape.
        let back = orient(&portrait, true, &EngineLimits::default()).expect("orient failed");
        assert_eq!(dims(&back), (160, 120));
    }

    #[test]
    fn autocrop_without_border_is_invalid() {
        let Some(fx) = fixture() else { return };
        // Borderless testsrc: cropdetect reports the full frame.
        assert!(matches!(
            autocrop(&fx, &EngineLimits::default()),
            Err(VideoError::InvalidInput(_))
        ));
    }

    #[test]
    fn autocrop_removes_black_border() {
        let Some(fx) = fixture() else { return };
        // Pad the fixture with a 20px black border, then autocrop it
        // back to ~160x120.
        let ffmpeg = find_tool(FFMPEG).unwrap();
        let dir = TempDir::new("geoadv-border").unwrap();
        let src = dir.file("src.mp4");
        let bordered = dir.file("bordered.mp4");
        let input_path = dir.file("input.bin");
        std::fs::write(&input_path, &fx).unwrap();
        let pad_args = vec![
            "-y".to_string(),
            "-v".to_string(),
            "error".to_string(),
            "-i".to_string(),
            input_path.to_string_lossy().into_owned(),
            "-vf".to_string(),
            "pad=200:160:20:20:black".to_string(),
            "-c:v".to_string(),
            "libopenh264".to_string(),
            "-pix_fmt".to_string(),
            "yuv420p".to_string(),
            "-c:a".to_string(),
            "aac".to_string(),
            bordered.to_string_lossy().into_owned(),
        ];
        let output = run_tool(FFMPEG, &ffmpeg, &pad_args, COMMAND_TIMEOUT).unwrap();
        if !output.status.success() {
            return;
        }
        let _ = src;
        let bordered_bytes = read_output(&bordered).unwrap();
        assert_eq!(dims(&bordered_bytes), (200, 160));
        let cropped = autocrop(&bordered_bytes, &EngineLimits::default()).expect("autocrop failed");
        assert_eq!(dims(&cropped), (160, 120));
    }
}
