//! Frame-rate conversion and motion interpolation.
//!
//! Three modes, cheapest to most expensive:
//!
//! - [`InterpolateMode::FramerateOnly`] — plain `fps=` conform: resamples
//!   to a constant frame rate without inventing frames. Fast, lossy only
//!   in the usual CFR-resampling sense.
//! - [`InterpolateMode::Mpdecimate`] — `mpdecimate,fps=`: drops
//!   near-duplicate frames first, then conforms to the target rate.
//!   Use to *reduce* redundant frames / motion judder from duplicated
//!   captures; the `fps=` tail still emits CFR output.
//! - [`InterpolateMode::Minterpolate`] — ffmpeg's `minterpolate` filter:
//!   motion-compensated frame interpolation that synthesizes in-between
//!   frames. Significantly more CPU than a plain re-encode; guarded by
//!   [`MAX_OUTPUT_FRAMES`] and the `target <= source * 4` diminishing-
//!   returns check below.
//!
//! All modes re-encode video through the standard H.264/AAC path
//! ([`h264_aac_args`]) and preserve the input's container family via
//! [`ext_for_format`], falling back to `mp4`.
//!
//! # Limit policy
//!
//! [`check_limits`] covers `max_input_bytes` / `max_duration_secs` /
//! `max_pixels` on the probed input. [`EngineLimits`] has no frame-count
//! field, so the interpolated *output* frame estimate
//! (`duration_secs × target_fps`) is bounded by the engine-internal
//! [`MAX_OUTPUT_FRAMES`] cap — checked before ffmpeg runs so an
//! over-long/high-fps request fails fast instead of grinding for hours.

use std::sync::OnceLock;

use crate::crates::core::engine::EngineLimits;
use crate::crates::video_engine::error::{VideoError, VideoResult};
use crate::crates::video_engine::model::VideoInfo;
use crate::crates::video_engine::ops::probe::probe;
use crate::crates::video_engine::ops::{
    COMMAND_TIMEOUT, FFMPEG, PROBE_TIMEOUT, TempDir, check_input, check_limits, ext_for_format,
    find_tool, h264_aac_args, read_output, run_tool, tool_failure, write_input,
};

/// Engine-internal cap on estimated output frames
/// (`duration_secs × target_fps`). `EngineLimits` carries no frame-count
/// bound, and interpolation cost scales with output frames, so this is
/// enforced as a [`VideoError::LimitExceeded`] before ffmpeg runs.
const MAX_OUTPUT_FRAMES: u64 = 100_000;

/// Highest accepted `target_fps`. Above this the per-frame cost of
/// interpolation is rarely justified and CFR conform gains nothing.
const MAX_TARGET_FPS: f64 = 120.0;

/// Highest accepted `target_fps` multiplier over the *source* rate for
/// [`InterpolateMode::Minterpolate`]. Beyond 4× the synthesized frames
/// are mostly guesswork and the CPU cost grows linearly — rejected as
/// [`VideoError::InvalidInput`].
const MAX_MINTERPOLATE_UPSAMPLE: f64 = 4.0;

/// Which frame-rate conversion strategy [`interpolate_video`] applies.
#[derive(Debug, Clone, PartialEq)]
pub enum InterpolateMode {
    /// `minterpolate` — motion-compensated interpolation synthesizing
    /// intermediate frames. Requires the `minterpolate` filter in the
    /// ffmpeg build; see [`MinterpolateOptions`] for tuning.
    Minterpolate(MinterpolateOptions),
    /// `mpdecimate,fps=` — drop near-duplicate frames, then conform to
    /// `target_fps`. The "reduce redundant frames" direction: fewer
    /// unique frames in, same CFR cadence out.
    Mpdecimate,
    /// `fps=` only — cheap constant-frame-rate conform, no frame
    /// synthesis or dropping analysis.
    FramerateOnly,
}

/// Tuning knobs for [`InterpolateMode::Minterpolate`], mapped onto
/// ffmpeg's `minterpolate` AVOptions. `None` leaves the filter default.
///
/// ffmpeg's numbering (verified against `ffmpeg -h filter=minterpolate`
/// on 7.1.x):
///
/// - `mi_mode`: `0` = dup (duplicate frames), `1` = blend, `2` = mci
///   (motion-compensated interpolation, the default and the expensive
///   one).
/// - `me_mode`: `0` = bidir (bidirectional motion estimation, cheaper),
///   `1` = bilat (bilateral, the filter default).
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct MinterpolateOptions {
    /// `mi_mode` — motion interpolation mode, `0..=2`.
    pub mi_mode: Option<u8>,
    /// `me_mode` — motion estimation mode, `0..=1`.
    pub me_mode: Option<u8>,
    /// `vsbmc` — variable-size block motion compensation (`0`/`1`).
    pub vsbmc: bool,
}

/// Parameters for [`interpolate_video`].
#[derive(Debug, Clone, PartialEq)]
pub struct InterpolateParams {
    /// Output frame rate in frames per second. Must be finite and in
    /// `1.0..=120.0`.
    pub target_fps: f64,
    /// Conversion strategy.
    pub mode: InterpolateMode,
}

/// Format an fps value for a filtergraph: up to 6 decimals, trailing
/// zeros trimmed (`20.0` → `"20"`, `23.976` → `"23.976"`).
fn fmt_fps(fps: f64) -> String {
    let s = format!("{fps:.6}");
    let s = s.trim_end_matches('0').trim_end_matches('.');
    if s.is_empty() {
        "0".to_string()
    } else {
        s.to_string()
    }
}

/// Build the `-vf` filtergraph for `params`. Validates mode-specific
/// option ranges and the `target > source × 4` minterpolate guard.
/// `source_fps` is the probed input rate, when known.
fn build_filter(params: &InterpolateParams, source_fps: Option<f64>) -> VideoResult<String> {
    let target = params.target_fps;
    if !target.is_finite() || !(1.0..=MAX_TARGET_FPS).contains(&target) {
        return Err(VideoError::InvalidInput(format!(
            "target_fps must be finite and within 1.0..={MAX_TARGET_FPS}, got {target}"
        )));
    }
    match &params.mode {
        InterpolateMode::FramerateOnly => Ok(format!("fps={}", fmt_fps(target))),
        InterpolateMode::Mpdecimate => Ok(format!("mpdecimate,fps={}", fmt_fps(target))),
        InterpolateMode::Minterpolate(opts) => {
            if let Some(mi) = opts.mi_mode {
                if mi > 2 {
                    return Err(VideoError::InvalidInput(format!(
                        "mi_mode must be 0..=2 (dup/blend/mci), got {mi}"
                    )));
                }
            }
            if let Some(me) = opts.me_mode {
                if me > 1 {
                    return Err(VideoError::InvalidInput(format!(
                        "me_mode must be 0..=1 (bidir/bilat), got {me}"
                    )));
                }
            }
            // Diminishing-returns + cost guard: synthesizing more than
            // 4× the source cadence produces mostly invented frames.
            if let Some(src) = source_fps {
                if src > 0.0 && target > src * MAX_MINTERPOLATE_UPSAMPLE {
                    return Err(VideoError::InvalidInput(format!(
                        "target_fps {target} exceeds 4× the source rate {src:.3}; \
                         interpolation beyond that is not useful and is very expensive"
                    )));
                }
            }
            let mut f = format!("minterpolate=fps={}", fmt_fps(target));
            if let Some(mi) = opts.mi_mode {
                f.push_str(&format!(":mi_mode={mi}"));
            }
            if let Some(me) = opts.me_mode {
                f.push_str(&format!(":me_mode={me}"));
            }
            f.push_str(&format!(":vsbmc={}", u8::from(opts.vsbmc)));
            Ok(f)
        }
    }
}

/// Estimated output frame count for the frame-count guard. All three
/// modes emit at most `duration × target_fps` frames (mpdecimate can
/// only reduce below that before `fps=` resamples up to it).
fn estimated_output_frames(info: &VideoInfo, target_fps: f64) -> Option<u64> {
    info.duration_secs
        .map(|d| (d * target_fps).ceil() as u64)
        .filter(|&n| n > 0)
}

/// Whether this ffmpeg build ships the `minterpolate` filter. Cached —
/// the filter list is static per binary.
fn minterpolate_available() -> bool {
    static AVAILABLE: OnceLock<bool> = OnceLock::new();
    *AVAILABLE.get_or_init(|| {
        let Ok(program) = find_tool(FFMPEG) else {
            return false;
        };
        let args = vec!["-hide_banner".to_string(), "-filters".to_string()];
        match run_tool(FFMPEG, &program, &args, PROBE_TIMEOUT) {
            Ok(output) if output.status.success() => {
                let text = String::from_utf8_lossy(&output.stdout);
                text.lines()
                    .any(|l| l.split_whitespace().nth(2) == Some("minterpolate"))
                    || text.contains("minterpolate")
            }
            _ => false,
        }
    })
}

/// Convert `input`'s frame rate per `params`, returning the re-encoded
/// media bytes (same container family as the input, H.264/AAC streams).
///
/// Fails with [`VideoError::InvalidInput`] for non-media input, missing
/// video stream, or out-of-range parameters; [`VideoError::LimitExceeded`]
/// when `limits` or the internal output-frame cap would be exceeded;
/// [`VideoError::ExternalTool`] when ffmpeg lacks the `minterpolate`
/// filter or exits non-zero.
pub fn interpolate_video(
    input: &[u8],
    params: &InterpolateParams,
    limits: &EngineLimits,
) -> VideoResult<Vec<u8>> {
    let input_len = check_input(input)?;
    find_tool(FFMPEG)?;
    let info = probe(input)?;
    if !info.has_video {
        return Err(VideoError::InvalidInput(
            "frame-rate conversion requires a video stream".to_string(),
        ));
    }
    check_limits(&info, input_len, limits, true)?;

    // Expensive-mode prerequisites before touching disk: the filter must
    // exist in this ffmpeg build.
    if matches!(params.mode, InterpolateMode::Minterpolate(_)) && !minterpolate_available() {
        return Err(VideoError::ExternalTool {
            tool: FFMPEG.to_string(),
            detail: "minterpolate filter is not available in this ffmpeg build".to_string(),
        });
    }

    let filter = build_filter(params, info.fps)?;

    // Output-frame guard, before spending any CPU: interpolation cost is
    // proportional to the frames produced.
    if let Some(frames) = estimated_output_frames(&info, params.target_fps) {
        if frames > MAX_OUTPUT_FRAMES {
            return Err(VideoError::LimitExceeded(format!(
                "estimated output is ~{frames} frames \
                 (duration × target_fps), exceeding the {MAX_OUTPUT_FRAMES}-frame \
                 interpolation cap"
            )));
        }
    }

    let ext = info
        .format_name
        .as_deref()
        .and_then(ext_for_format)
        .unwrap_or("mp4")
        .to_string();
    let dir = TempDir::new("interpolate")?;
    let input_path = write_input(&dir, "input.bin", input)?;
    let out_path = dir.file(&format!("out.{ext}"));

    let program = find_tool(FFMPEG)?;
    let mut args = vec![
        "-y".to_string(),
        "-v".to_string(),
        "error".to_string(),
        "-i".to_string(),
        input_path.to_string_lossy().into_owned(),
        "-vf".to_string(),
        filter,
    ];
    // Video is re-encoded; audio is re-encoded to AAC for container
    // safety (a filtergraph pass can otherwise carry an incompatible
    // codec into the muxer).
    args.extend(h264_aac_args()?);
    args.push(out_path.to_string_lossy().into_owned());

    let output = run_tool(FFMPEG, &program, &args, COMMAND_TIMEOUT)?;
    if !output.status.success() {
        return Err(tool_failure(FFMPEG, &output));
    }
    read_output(&out_path)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::crates::video_engine::ops::{FFPROBE, h264_encoder};

    /// Encode a tiny lavfi clip. `rate`/`duration`/`size` are small so
    /// `minterpolate` stays well under the test budget.
    /// `still` produces a static source (all frames near-identical) for
    /// the mpdecimate path.
    fn fixture(rate: u32, duration: f64, still: bool) -> Option<Vec<u8>> {
        let ffmpeg = find_tool(FFMPEG).ok()?;
        find_tool(FFPROBE).ok()?;
        let enc = h264_encoder()?;
        let dir = TempDir::new("interp-test").ok()?;
        let out = dir.file("fixture.mp4");
        let source = if still {
            format!("color=c=gray:size=128x96:rate={rate}:duration={duration}")
        } else {
            format!("testsrc=size=128x96:rate={rate}:duration={duration}")
        };
        let args = vec![
            "-y".to_string(),
            "-v".to_string(),
            "error".to_string(),
            "-f".to_string(),
            "lavfi".to_string(),
            "-i".to_string(),
            source,
            "-f".to_string(),
            "lavfi".to_string(),
            "-i".to_string(),
            format!("sine=frequency=440:duration={duration}"),
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

    fn out_fps(bytes: &[u8]) -> f64 {
        let info = probe(bytes).expect("output must probe");
        info.fps.expect("output must report a frame rate")
    }

    #[test]
    fn framerate_only_conforms_to_target() {
        let Some(fx) = fixture(10, 0.5, false) else {
            return;
        };
        let params = InterpolateParams {
            target_fps: 20.0,
            mode: InterpolateMode::FramerateOnly,
        };
        let out = interpolate_video(&fx, &params, &EngineLimits::default()).expect("conform");
        let fps = out_fps(&out);
        assert!((fps - 20.0).abs() < 0.5, "expected ~20fps, got {fps}");
    }

    #[test]
    fn mpdecimate_runs_and_conforms() {
        // Static source: every frame is a near-duplicate, so mpdecimate
        // drops almost all of them before fps= resamples back to CFR.
        let Some(fx) = fixture(10, 0.5, true) else {
            return;
        };
        let params = InterpolateParams {
            target_fps: 20.0,
            mode: InterpolateMode::Mpdecimate,
        };
        let out = interpolate_video(&fx, &params, &EngineLimits::default()).expect("mpdecimate");
        let fps = out_fps(&out);
        assert!((fps - 20.0).abs() < 0.5, "expected ~20fps, got {fps}");
    }

    #[test]
    fn minterpolate_produces_target_fps() {
        if !minterpolate_available() {
            return;
        }
        // 0.5s at 128x96: 10 source frames → ~10 output frames; cheap.
        let Some(fx) = fixture(10, 0.5, false) else {
            return;
        };
        let params = InterpolateParams {
            target_fps: 20.0,
            mode: InterpolateMode::Minterpolate(MinterpolateOptions {
                mi_mode: Some(2),
                me_mode: Some(0),
                vsbmc: true,
            }),
        };
        let out = interpolate_video(&fx, &params, &EngineLimits::default()).expect("minterpolate");
        let fps = out_fps(&out);
        assert!((fps - 20.0).abs() < 0.5, "expected ~20fps, got {fps}");
    }

    #[test]
    fn rejects_bad_target_fps() {
        let Some(fx) = fixture(10, 0.5, false) else {
            return;
        };
        for bad in [0.0, -5.0, 500.0, f64::NAN] {
            let params = InterpolateParams {
                target_fps: bad,
                mode: InterpolateMode::FramerateOnly,
            };
            assert!(
                interpolate_video(&fx, &params, &EngineLimits::default()).is_err(),
                "target_fps {bad} must be rejected"
            );
        }
    }

    #[test]
    fn minterpolate_rejects_excessive_upsample() {
        let Some(fx) = fixture(10, 0.5, false) else {
            return;
        };
        // 10fps source → 41fps target is beyond the 4× guard (40fps).
        let params = InterpolateParams {
            target_fps: 41.0,
            mode: InterpolateMode::Minterpolate(MinterpolateOptions::default()),
        };
        assert!(interpolate_video(&fx, &params, &EngineLimits::default()).is_err());
    }

    #[test]
    fn rejects_bad_mi_mode() {
        let params = InterpolateParams {
            target_fps: 20.0,
            mode: InterpolateMode::Minterpolate(MinterpolateOptions {
                mi_mode: Some(9),
                ..Default::default()
            }),
        };
        assert!(build_filter(&params, Some(10.0)).is_err());
    }

    #[test]
    fn rejects_non_media() {
        let params = InterpolateParams {
            target_fps: 20.0,
            mode: InterpolateMode::FramerateOnly,
        };
        assert!(interpolate_video(b"not media", &params, &EngineLimits::default()).is_err());
        assert!(interpolate_video(&[], &params, &EngineLimits::default()).is_err());
    }
}
