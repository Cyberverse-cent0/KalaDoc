//! Video compositing ops: picture-in-picture, side-by-side, and
//! split-screen.
//!
//! All three ops re-encode through the standard H.264/AAC path
//! ([`h264_aac_args`] — libx264 → libopenh264 fallback) and preserve
//! the *first/base* input's container family via [`ext_for_format`],
//! falling back to `mp4` when the probed `format_name` cannot be
//! named. Audio comes from the base/first input only (`-map "0:a?"`);
//! secondary-input audio is dropped — see each op's docs.
//!
//! NOTE: this module is not yet declared in `ops/mod.rs`; the engine
//! coordinator wires the `pub(crate) mod composite;` declaration and
//! `VideoEngine` dispatch separately.

use crate::crates::core::engine::EngineLimits;
use crate::crates::video_engine::error::{VideoError, VideoResult};
use crate::crates::video_engine::model::VideoInfo;
use crate::crates::video_engine::ops::probe::probe;
use crate::crates::video_engine::ops::{
    COMMAND_TIMEOUT, FFMPEG, TempDir, check_input, check_limits, ext_for_format, find_tool,
    h264_aac_args, read_output, run_tool, tool_failure, write_input,
};

/// Where a picture-in-picture overlay is anchored, as fractions of the
/// *free* canvas (canvas minus overlay size): `0.0` = left/top edge,
/// `1.0` = flush against the right/bottom edge, `0.5` = centered.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct OverlayPosition {
    /// Horizontal anchor, `0.0..=1.0`.
    pub x_pct: f64,
    /// Vertical anchor, `0.0..=1.0`.
    pub y_pct: f64,
}

/// Parameters for [`picture_in_picture`].
#[derive(Debug, Clone, PartialEq)]
pub struct PictureInPictureParams {
    /// Base video the overlay is composited onto. Supplies the output
    /// canvas dimensions, the container family, and the audio track.
    pub base: Vec<u8>,
    /// Overlay video shown in the corner. Scaled per `overlay_scale`;
    /// **its audio is dropped** (only `base` audio is mapped).
    pub overlay: Vec<u8>,
    /// Anchor position on the base canvas.
    pub position: OverlayPosition,
    /// Overlay width as a fraction of the *base width*
    /// (`0.25` = quarter of base width). Must be finite and `> 0`;
    /// height follows the overlay's aspect ratio.
    pub overlay_scale: f64,
    /// Optional `(start_secs, end_secs)` visibility window. `start`
    /// must be finite and `>= 0`. When `end` is non-finite or
    /// `<= start` the overlay stays on from `start` onward
    /// (`gte(t,start)`); otherwise `between(t,start,end)` is used.
    /// `None` shows the overlay for the whole video.
    pub window: Option<(f64, f64)>,
}

/// Arrangement for [`side_by_side`].
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Orientation {
    /// Left input beside the right input (`hstack`).
    Horizontal,
    /// Top input above the bottom input (`vstack`).
    Vertical,
}

/// Parameters for [`side_by_side`].
#[derive(Debug, Clone, PartialEq)]
pub struct SideBySideParams {
    /// Left (horizontal) or top (vertical) input. Supplies the audio
    /// track and the output container family.
    pub left: Vec<u8>,
    /// Right (horizontal) or bottom (vertical) input. Its audio is
    /// dropped.
    pub right: Vec<u8>,
    /// Stacking direction.
    pub orientation: Orientation,
}

/// Parameters for [`split_screen`].
#[derive(Debug, Clone, PartialEq)]
pub struct SplitScreenParams {
    /// Top half of the output. Supplies the audio track and the output
    /// container family.
    pub top: Vec<u8>,
    /// Bottom half of the output. Its audio is dropped. Must have the
    /// same width as `top`.
    pub bottom: Vec<u8>,
}

/// Probe `bytes`, enforce `limits`, and require a primary video stream
/// with known dimensions. `role` only decorates error messages.
fn checked_video_input(bytes: &[u8], limits: &EngineLimits, role: &str) -> VideoResult<VideoInfo> {
    let input_len = check_input(bytes)?;
    let info = probe(bytes)?;
    check_limits(&info, input_len, limits, true)?;
    if !info.has_video {
        return Err(VideoError::InvalidInput(format!(
            "{role} input has no video stream to composite"
        )));
    }
    if info.width.is_none() || info.height.is_none() {
        return Err(VideoError::InvalidInput(format!(
            "{role} input video stream reports no dimensions"
        )));
    }
    Ok(info)
}

/// Output extension: the first/base input's container family via
/// [`ext_for_format`], `mp4` when unnameable.
fn output_ext(info: &VideoInfo) -> String {
    info.format_name
        .as_deref()
        .and_then(ext_for_format)
        .unwrap_or("mp4")
        .to_string()
}

/// Shared tail: run ffmpeg, require success, read the output file.
fn run_encode(
    program: &std::path::Path,
    args: &[String],
    out_path: &std::path::Path,
) -> VideoResult<Vec<u8>> {
    let output = run_tool(FFMPEG, program, args, COMMAND_TIMEOUT)?;
    if !output.status.success() {
        return Err(tool_failure(FFMPEG, &output));
    }
    read_output(out_path)
}

/// Format a finite f64 for a filtergraph: up to 6 decimals, trailing
/// zeros trimmed (`0.5` → `"0.5"`, `2.0` → `"2"`).
fn fmt_num(v: f64) -> String {
    let s = format!("{v:.6}");
    let s = s.trim_end_matches('0').trim_end_matches('.');
    if s.is_empty() || s == "-0" {
        "0".to_string()
    } else {
        s.to_string()
    }
}

/// Validate a `0.0..=1.0` fraction parameter.
fn checked_unit(value: f64, name: &str) -> VideoResult<f64> {
    if !value.is_finite() || !(0.0..=1.0).contains(&value) {
        return Err(VideoError::InvalidInput(format!(
            "{name} must be a finite number in 0.0..=1.0, got {value}"
        )));
    }
    Ok(value)
}

/// Composite `params.overlay` onto `params.base` as a picture-in-picture
/// window.
///
/// The output canvas is exactly the base video's dimensions; the
/// overlay is scaled to `overlay_scale × base_width` (aspect ratio
/// preserved, both dims rounded to integers ≥ 1 px) and anchored at
/// `(x_pct, y_pct)` of the *remaining* canvas — `(0,0)` top-left,
/// `(1,1)` bottom-right flush, `(0.5,0.5)` centered.
///
/// `window` optionally delays the overlay: the graph applies
/// `setpts=PTS-STARTPTS+start/TB` to the overlay stream and gates the
/// `overlay` filter with `enable='between(t,start,end)'` (or
/// `gte(t,start)` when there is no finite end). With `window: None`
/// the overlay is visible for the whole video.
///
/// Audio: only the base input's audio is mapped (`-map "0:a?"`,
/// re-encoded to AAC); **overlay audio is intentionally dropped**.
/// `-shortest` bounds the output to the shortest stream.
pub fn picture_in_picture(
    params: &PictureInPictureParams,
    limits: &EngineLimits,
) -> VideoResult<Vec<u8>> {
    let base_info = checked_video_input(&params.base, limits, "base")?;
    let overlay_info = checked_video_input(&params.overlay, limits, "overlay")?;

    let scale = params.overlay_scale;
    if !scale.is_finite() || scale <= 0.0 {
        return Err(VideoError::InvalidInput(format!(
            "overlay_scale must be a finite number > 0, got {scale}"
        )));
    }
    let xpct = checked_unit(params.position.x_pct, "position.x_pct")?;
    let ypct = checked_unit(params.position.y_pct, "position.y_pct")?;

    let (Some(w), Some(h)) = (base_info.width, base_info.height) else {
        return Err(VideoError::InvalidInput(
            "base input video stream reports no dimensions".to_string(),
        ));
    };
    let (Some(ow), Some(oh)) = (overlay_info.width, overlay_info.height) else {
        return Err(VideoError::InvalidInput(
            "overlay input video stream reports no dimensions".to_string(),
        ));
    };

    // Overlay width = scale × base width; height preserves the
    // overlay's aspect ratio. Both rounded to ints, clamped ≥ 1 px.
    let tw = ((scale * w as f64).round() as i64).clamp(1, i64::from(w));
    let th = ((tw as f64 * f64::from(oh) / f64::from(ow)).round() as i64).clamp(1, i64::from(h));
    if tw < 1 || th < 1 {
        return Err(VideoError::InvalidInput(
            "overlay_scale is too small to produce a visible overlay".to_string(),
        ));
    }

    // Anchor within the free canvas: (W - tw) * pct.
    let x = ((i64::from(w) - tw) as f64 * xpct).round() as i64;
    let y = ((i64::from(h) - th) as f64 * ypct).round() as i64;

    // Optional visibility window.
    let (start, enable) = match params.window {
        Some((s, e)) => {
            if !s.is_finite() || s < 0.0 {
                return Err(VideoError::InvalidInput(format!(
                    "window start must be a finite number >= 0, got {s}"
                )));
            }
            let expr = if e.is_finite() && e > s {
                format!("between(t,{},{})", fmt_num(s), fmt_num(e))
            } else {
                format!("gte(t,{})", fmt_num(s))
            };
            (s, Some(expr))
        }
        None => (0.0, None),
    };

    let mut graph = format!(
        "[0:v]setpts=PTS-STARTPTS[base];\
         [1:v]scale={tw}:{th},setpts=PTS-STARTPTS+{start}/TB[ov];\
         [base][ov]overlay=x={x}:y={y}",
        start = fmt_num(start),
    );
    if let Some(expr) = enable {
        graph.push_str(&format!(":enable='{expr}'"));
    }
    graph.push_str("[v]");

    let program = find_tool(FFMPEG)?;
    let ext = output_ext(&base_info);
    let dir = TempDir::new("pip")?;
    let base_path = write_input(&dir, "base.bin", &params.base)?;
    let overlay_path = write_input(&dir, "overlay.bin", &params.overlay)?;
    let out_path = dir.file(&format!("out.{ext}"));

    let mut args = vec![
        "-y".to_string(),
        "-v".to_string(),
        "error".to_string(),
        "-i".to_string(),
        base_path.to_string_lossy().into_owned(),
        "-i".to_string(),
        overlay_path.to_string_lossy().into_owned(),
        "-filter_complex".to_string(),
        graph,
        "-map".to_string(),
        "[v]".to_string(),
        "-map".to_string(),
        "0:a?".to_string(),
    ];
    args.extend(h264_aac_args()?);
    args.push("-shortest".to_string());
    args.push(out_path.to_string_lossy().into_owned());

    run_encode(&program, &args, &out_path)
}

/// Round `v` up to the next even integer (yuv420p safety).
fn even_up(v: u64) -> u64 {
    v.div_ceil(2) * 2
}

/// Lay `left` and `right` out side by side (or stacked) per
/// `params.orientation`.
///
/// Each input is scaled preserving aspect ratio into a box occupying
/// *half the canvas* along the stacking axis — for `Horizontal` each
/// gets `canvas_w/2` where `canvas_w ≈ left_w + right_w`; for
/// `Vertical` each gets `canvas_h/2` where `canvas_h ≈ top_h +
/// bottom_h` — then padded (letterboxed, centered, black) to the
/// common cross-axis size so `hstack`/`vstack` succeeds. Output dims
/// are therefore ≈ `(w1 + w2) × max_scaled_height` horizontally and
/// `max_scaled_width × (h1 + h2)` vertically.
///
/// Audio comes from `left`/`top` (`-map "0:a?"`, re-encoded to AAC);
/// the second input's audio is dropped. The output container matches
/// the first input's probed format (`mp4` fallback).
pub fn side_by_side(params: &SideBySideParams, limits: &EngineLimits) -> VideoResult<Vec<u8>> {
    let a_info = checked_video_input(&params.left, limits, "first")?;
    let b_info = checked_video_input(&params.right, limits, "second")?;
    let (Some(aw), Some(ah)) = (a_info.width, a_info.height) else {
        return Err(VideoError::InvalidInput(
            "first input video stream reports no dimensions".to_string(),
        ));
    };
    let (Some(bw), Some(bh)) = (b_info.width, b_info.height) else {
        return Err(VideoError::InvalidInput(
            "second input video stream reports no dimensions".to_string(),
        ));
    };

    // Per-input scaled extent along the *cross* axis, given the
    // half-canvas box, so we can size the pad box up front.
    // Horizontal: box = half_w × hmax.  Vertical: box = wmax × half_h.
    let (box_w, box_h, stack) = match params.orientation {
        Orientation::Horizontal => {
            let half_w = even_up(u64::from(aw) + u64::from(bw)) / 2;
            // Scaled height of each input at width half_w (AR kept):
            // h_i = ah_i * half_w / aw_i, rounded up then made even.
            let ha = even_up(u64::from(ah) * half_w / u64::from(aw).max(1));
            let hb = even_up(u64::from(bh) * half_w / u64::from(bw).max(1));
            (half_w, ha.max(hb), "hstack")
        }
        Orientation::Vertical => {
            let half_h = even_up(u64::from(ah) + u64::from(bh)) / 2;
            let wa = even_up(u64::from(aw) * half_h / u64::from(ah).max(1));
            let wb = even_up(u64::from(bw) * half_h / u64::from(bh).max(1));
            (wa.max(wb), half_h, "vstack")
        }
    };
    if box_w == 0 || box_h == 0 {
        return Err(VideoError::InvalidInput(
            "composited dimensions computed to zero".to_string(),
        ));
    }

    // `scale=W:H:force_original_aspect_ratio=decrease` guarantees the
    // scaled frame fits inside the box; `pad` centers it on black.
    let graph = format!(
        "[0:v]scale={box_w}:{box_h}:force_original_aspect_ratio=decrease,\
         pad={box_w}:{box_h}:(ow-iw)/2:(oh-ih)/2:color=black[p0];\
         [1:v]scale={box_w}:{box_h}:force_original_aspect_ratio=decrease,\
         pad={box_w}:{box_h}:(ow-iw)/2:(oh-ih)/2:color=black[p1];\
         [p0][p1]{stack}=inputs=2[v]"
    );

    let program = find_tool(FFMPEG)?;
    let ext = output_ext(&a_info);
    let dir = TempDir::new("sidebyside")?;
    let a_path = write_input(&dir, "left.bin", &params.left)?;
    let b_path = write_input(&dir, "right.bin", &params.right)?;
    let out_path = dir.file(&format!("out.{ext}"));

    let mut args = vec![
        "-y".to_string(),
        "-v".to_string(),
        "error".to_string(),
        "-i".to_string(),
        a_path.to_string_lossy().into_owned(),
        "-i".to_string(),
        b_path.to_string_lossy().into_owned(),
        "-filter_complex".to_string(),
        graph,
        "-map".to_string(),
        "[v]".to_string(),
        "-map".to_string(),
        "0:a?".to_string(),
    ];
    args.extend(h264_aac_args()?);
    args.push("-shortest".to_string());
    args.push(out_path.to_string_lossy().into_owned());

    run_encode(&program, &args, &out_path)
}

/// Stack `top` above `bottom` into a single full-width frame
/// (`vstack`). Both inputs are rescaled to the shared width
/// (`scale={w}:-2`, aspect preserved, even height) and concatenated
/// vertically — the output is approximately `w × (h_top + h_bottom)`.
///
/// The inputs **must report the same width**; mismatched widths are
/// [`VideoError::InvalidInput`] rather than silently stretching one
/// side. Audio comes from `top` (`-map "0:a?"`, re-encoded to AAC);
/// `bottom` audio is dropped. Output container matches `top`'s probed
/// format (`mp4` fallback).
pub fn split_screen(params: &SplitScreenParams, limits: &EngineLimits) -> VideoResult<Vec<u8>> {
    let top_info = checked_video_input(&params.top, limits, "top")?;
    let bottom_info = checked_video_input(&params.bottom, limits, "bottom")?;
    let (Some(tw), Some(_)) = (top_info.width, top_info.height) else {
        return Err(VideoError::InvalidInput(
            "top input video stream reports no dimensions".to_string(),
        ));
    };
    let Some(bw) = bottom_info.width else {
        return Err(VideoError::InvalidInput(
            "bottom input video stream reports no dimensions".to_string(),
        ));
    };
    if tw != bw {
        return Err(VideoError::InvalidInput(format!(
            "split_screen requires equal widths, got top={tw}px bottom={bw}px"
        )));
    }

    let graph = format!(
        "[0:v]scale={tw}:-2[v0];\
         [1:v]scale={tw}:-2[v1];\
         [v0][v1]vstack=inputs=2[v]"
    );

    let program = find_tool(FFMPEG)?;
    let ext = output_ext(&top_info);
    let dir = TempDir::new("splitscreen")?;
    let top_path = write_input(&dir, "top.bin", &params.top)?;
    let bottom_path = write_input(&dir, "bottom.bin", &params.bottom)?;
    let out_path = dir.file(&format!("out.{ext}"));

    let mut args = vec![
        "-y".to_string(),
        "-v".to_string(),
        "error".to_string(),
        "-i".to_string(),
        top_path.to_string_lossy().into_owned(),
        "-i".to_string(),
        bottom_path.to_string_lossy().into_owned(),
        "-filter_complex".to_string(),
        graph,
        "-map".to_string(),
        "[v]".to_string(),
        "-map".to_string(),
        "0:a?".to_string(),
    ];
    args.extend(h264_aac_args()?);
    args.push("-shortest".to_string());
    args.push(out_path.to_string_lossy().into_owned());

    run_encode(&program, &args, &out_path)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::crates::video_engine::ops::{FFPROBE, h264_encoder};

    fn tools_available() -> bool {
        find_tool(FFMPEG).is_ok() && find_tool(FFPROBE).is_ok() && h264_encoder().is_some()
    }

    /// ~1s testsrc clip with a sine audio track, `size` like "160x120".
    /// `source` is the lavfi source name (`testsrc`, `testsrc2`, ...).
    fn make_clip(tag: &str, source: &str, size: &str) -> Option<Vec<u8>> {
        let ffmpeg = find_tool(FFMPEG).ok()?;
        let enc = h264_encoder()?;
        let dir = TempDir::new(tag).ok()?;
        let out = dir.file("clip.mp4");
        let args = vec![
            "-y".to_string(),
            "-v".to_string(),
            "error".to_string(),
            "-f".to_string(),
            "lavfi".to_string(),
            "-i".to_string(),
            format!("{source}=size={size}:rate=10:duration=1"),
            "-f".to_string(),
            "lavfi".to_string(),
            "-i".to_string(),
            "sine=frequency=440:duration=1".to_string(),
            "-c:v".to_string(),
            enc.to_string(),
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

    #[test]
    fn pip_output_matches_base_dims() {
        if !tools_available() {
            return;
        }
        let (Some(base), Some(ov)) = (
            make_clip("pip-base", "testsrc", "160x120"),
            make_clip("pip-ov", "testsrc2", "320x240"),
        ) else {
            return;
        };
        let params = PictureInPictureParams {
            base,
            overlay: ov,
            position: OverlayPosition {
                x_pct: 1.0,
                y_pct: 1.0,
            },
            overlay_scale: 0.5,
            window: Some((0.0, 0.5)),
        };
        let out = picture_in_picture(&params, &EngineLimits::default()).expect("pip failed");
        assert!(!out.is_empty());
        let info = probe(&out).expect("output must probe");
        assert_eq!(info.width, Some(160));
        assert_eq!(info.height, Some(120));
        assert!(info.has_audio, "base audio should be preserved");
    }

    #[test]
    fn pip_validates_params() {
        if !tools_available() {
            return;
        }
        let (Some(base), Some(ov)) = (
            make_clip("pipv-base", "testsrc", "160x120"),
            make_clip("pipv-ov", "testsrc2", "320x240"),
        ) else {
            return;
        };
        let good = PictureInPictureParams {
            base: base.clone(),
            overlay: ov.clone(),
            position: OverlayPosition {
                x_pct: 0.0,
                y_pct: 0.0,
            },
            overlay_scale: 0.5,
            window: None,
        };
        for (scale, x, y) in [
            (0.0, 0.0, 0.0),
            (-1.0, 0.0, 0.0),
            (f64::NAN, 0.0, 0.0),
            (0.5, 1.5, 0.0),
            (0.5, 0.0, f64::NAN),
        ] {
            let mut p = good.clone();
            p.overlay_scale = scale;
            p.position = OverlayPosition { x_pct: x, y_pct: y };
            assert!(
                picture_in_picture(&p, &EngineLimits::default()).is_err(),
                "scale={scale} pos=({x},{y}) must be rejected"
            );
        }
        // Garbage overlay bytes fail via ffprobe.
        let mut p = good.clone();
        p.overlay = b"junk".to_vec();
        assert!(picture_in_picture(&p, &EngineLimits::default()).is_err());
    }

    #[test]
    fn side_by_side_horizontal_combines_widths() {
        if !tools_available() {
            return;
        }
        let (Some(left), Some(right)) = (
            make_clip("sbs-l", "testsrc", "160x120"),
            make_clip("sbs-r", "testsrc2", "320x240"),
        ) else {
            return;
        };
        let params = SideBySideParams {
            left,
            right,
            orientation: Orientation::Horizontal,
        };
        let out = side_by_side(&params, &EngineLimits::default()).expect("side_by_side failed");
        let info = probe(&out).expect("output must probe");
        // half_w = (160+320)/2 = 240; both scale to 240x180 → 480x180.
        assert_eq!(info.width, Some(480));
        assert_eq!(info.height, Some(180));
        assert!(info.has_audio);
    }

    #[test]
    fn side_by_side_vertical_stacks_heights() {
        if !tools_available() {
            return;
        }
        let (Some(top), Some(bottom)) = (
            make_clip("sbsv-t", "testsrc", "160x120"),
            make_clip("sbsv-b", "testsrc2", "320x240"),
        ) else {
            return;
        };
        let params = SideBySideParams {
            left: top,
            right: bottom,
            orientation: Orientation::Vertical,
        };
        let out = side_by_side(&params, &EngineLimits::default()).expect("side_by_side failed");
        let info = probe(&out).expect("output must probe");
        // half_h = (120+240)/2 = 180; scaled widths both 240 → 240x360.
        assert_eq!(info.width, Some(240));
        assert_eq!(info.height, Some(360));
    }

    #[test]
    fn split_screen_stacks_and_rejects_mismatch() {
        if !tools_available() {
            return;
        }
        let (Some(a), Some(b), Some(wide)) = (
            make_clip("ss-a", "testsrc", "160x120"),
            make_clip("ss-b", "testsrc2", "160x120"),
            make_clip("ss-wide", "testsrc2", "320x240"),
        ) else {
            return;
        };
        let params = SplitScreenParams {
            top: a.clone(),
            bottom: b,
        };
        let out = split_screen(&params, &EngineLimits::default()).expect("split_screen failed");
        let info = probe(&out).expect("output must probe");
        // scale=160:-2 keeps 4:3 → each 160x120, vstack → 160x240.
        assert_eq!(info.width, Some(160));
        assert_eq!(info.height, Some(240));
        assert!(info.has_audio);

        let bad = SplitScreenParams {
            top: a,
            bottom: wide,
        };
        assert!(matches!(
            split_screen(&bad, &EngineLimits::default()),
            Err(VideoError::InvalidInput(_))
        ));
    }
}
