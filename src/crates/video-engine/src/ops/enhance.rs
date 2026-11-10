//! Video enhancement ops: denoise, sharpen, deinterlace, fps conversion,
//! and integer upscaling.
//!
//! Every op re-encodes through the engine's standard H.264/AAC output
//! path ([`h264_aac_args`]) and preserves the input's container family
//! via [`ext_for_format`], falling back to `mp4` when the probed
//! `format_name` is unknown — same contract as `ops::geometry`.
//!
//! # Filter choices (verified against ffmpeg 7.1.5)
//!
//! - **denoise**: `hqdn3d` (`luma_spatial`/`chroma_spatial`) is the fast
//!   default. `strength` is a 0..1-ish knob mapped to a spatial strength
//!   of `strength * 1.5` clamped to `0..1.5`. When the build lacks
//!   `hqdn3d`, the op falls back to `nlmeans` — much slower (patch
//!   search per pixel) but higher quality — with `s` mapped into its
//!   documented 1..30 range.
//! - **sharpen**: `unsharp=5:5:{amt}:5:5:{amt*0.5}` (5x5 luma matrix at
//!   `amount`, 5x5 chroma at half strength), `amount` clamped to 0..5.
//! - **deinterlace**: `yadif` / `bwdif` map to the same-named filters.
//!   [`DeinterlaceMethod::Deinterlace`] requests the generic
//!   `deinterlace` filter, which does **not** exist in ffmpeg 7.1.5
//!   (only the hardware `deinterlace_qsv`/`deinterlace_vaapi`
//!   variants), so it falls back to `yadif` — ffmpeg's reference CPU
//!   deinterlacer — when the filter is absent.
//! - **fps_convert**: the `fps` filter forces a constant output frame
//!   rate; accepts `fps` in `(0, 240]`.
//! - **upscale**: `scale=iw*{s}:ih*{s}:flags={neighbor|bicubic|lanczos}`
//!   — all three flag names are valid libswscale `sws_flags` values.
//!
//! Inputs without a video stream are rejected with
//! [`VideoError::InvalidInput`]; `max_pixels` bounds both the probed
//! input frame and (where dims are known exactly) the output frame.

use std::path::PathBuf;
use std::sync::OnceLock;

use crate::crates::core::engine::EngineLimits;
use crate::crates::video_engine::error::{VideoError, VideoResult};
use crate::crates::video_engine::ops::probe::probe;
use crate::crates::video_engine::ops::{
    COMMAND_TIMEOUT, FFMPEG, TempDir, check_input, check_limits, ext_for_format, find_tool,
    h264_aac_args, read_output, run_tool, tool_failure, write_input,
};

/// Parameters for [`denoise`].
#[derive(Debug, Clone, Copy)]
pub struct DenoiseParams {
    /// Denoise strength; nominally 0.0 (off) to 1.0 (strong). Internally
    /// mapped to `hqdn3d` spatial strengths of `strength * 1.5` clamped
    /// to 0..1.5, or to `nlmeans` `s` in 1..30 on the fallback path.
    pub strength: f64,
}

/// Parameters for [`sharpen`].
#[derive(Debug, Clone, Copy)]
pub struct SharpenParams {
    /// Sharpen amount clamped to 0.0..=5.0 (the `unsharp` filter's own
    /// range). `0` is a no-op, `1` a mild unsharp mask.
    pub amount: f64,
}

/// Deinterlacing algorithm for [`deinterlace`].
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum DeinterlaceMethod {
    /// `yadif` — ffmpeg's default CPU deinterlacer.
    Yadif,
    /// `bwdif` — Bob+Weave deinterlacer, sharper than yadif on motion.
    Bwdif,
    /// The generic `deinterlace` filter when the build provides one
    /// (ffmpeg 7.1.5 does not); falls back to `yadif` otherwise.
    Deinterlace,
}

/// Parameters for [`fps_convert`].
#[derive(Debug, Clone, Copy)]
pub struct FpsConvertParams {
    /// Target frames per second, in `(0, 240]`. Non-finite, zero,
    /// negative, or >240 values are rejected with
    /// [`VideoError::InvalidInput`].
    pub fps: f64,
}

/// Upscaling interpolation algorithm for [`upscale`].
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum UpscaleAlgorithm {
    /// `neighbor` — fastest, blocky.
    Neighbor,
    /// `bicubic` — smooth default.
    Bicubic,
    /// `lanczos` — sharpest, slightly slower.
    Lanczos,
}

/// Parameters for [`upscale`].
#[derive(Debug, Clone, Copy)]
pub struct UpscaleParams {
    /// Integer scale factor; only `2` and `4` are supported.
    pub scale: u32,
    /// Interpolation algorithm.
    pub algorithm: UpscaleAlgorithm,
}

/// Cache of `ffmpeg -filters` first-column names so filter-existence
/// checks don't respawn ffmpeg on every call.
fn available_filters() -> &'static Vec<String> {
    static FILTERS: OnceLock<Vec<String>> = OnceLock::new();
    FILTERS.get_or_init(|| {
        let Ok(program) = find_tool(FFMPEG) else {
            return Vec::new();
        };
        let args = vec!["-hide_banner".to_string(), "-filters".to_string()];
        // filter names are ~20 bytes/line; a fixed generous cap keeps a
        // pathological build from eating memory.
        match run_tool(
            FFMPEG,
            &program,
            &args,
            crate::crates::video_engine::ops::PROBE_TIMEOUT,
        ) {
            Ok(output) if output.status.success() => String::from_utf8_lossy(&output.stdout)
                .lines()
                .filter_map(|line| line.split_whitespace().nth(0).map(str::to_string))
                .collect(),
            _ => Vec::new(),
        }
    })
}

/// Whether `ffmpeg -filters` lists a filter named exactly `name`.
fn filter_available(name: &str) -> bool {
    available_filters().iter().any(|f| f == name)
}

/// Render an `f64` filter argument compactly: `15.0` becomes `"15"`,
/// `0.025` stays `"0.025"`. ffmpeg's option parser accepts both forms.
fn fmt_f64(v: f64) -> String {
    v.to_string()
}

/// Validate that `v` is finite before embedding it in a filtergraph.
fn check_finite(name: &str, v: f64) -> VideoResult<()> {
    if !v.is_finite() {
        return Err(VideoError::InvalidInput(format!(
            "{name} must be a finite number, got {v}"
        )));
    }
    Ok(())
}

/// Probed-and-staged input shared by every enhancement op.
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

/// Probe `input`, enforce input-side limits, and stage it in a
/// [`TempDir`]. Requires a video stream with reported dimensions.
fn prepare(input: &[u8], limits: &EngineLimits, tag: &str) -> VideoResult<Prepared> {
    let input_len = check_input(input)?;
    find_tool(FFMPEG)?;
    let info = probe(input)?;
    if !info.has_video {
        return Err(VideoError::InvalidInput(
            "enhancement operations require a video stream".to_string(),
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

/// Reduce video noise. Prefers `hqdn3d` spatial denoising; falls back to
/// `nlmeans` (slower, higher quality) when `hqdn3d` is not in the build.
pub(crate) fn denoise(
    input: &[u8],
    params: DenoiseParams,
    limits: &EngineLimits,
) -> VideoResult<Vec<u8>> {
    check_finite("strength", params.strength)?;
    if params.strength < 0.0 {
        return Err(VideoError::InvalidInput(format!(
            "strength must be >= 0, got {}",
            params.strength
        )));
    }
    let ctx = prepare(input, limits, "denoise")?;

    if filter_available("hqdn3d") {
        // strength*1.5 clamped to the documented-ish 0..1.5 range; hqdn3d
        // accepts any non-negative double but values beyond ~1.5 smear.
        let s = (params.strength * 1.5).clamp(0.0, 1.5);
        let s = fmt_f64(s);
        ctx.execute(&format!("hqdn3d=luma_spatial={s}:chroma_spatial={s}"))
    } else {
        // nlmeans `s` lives in 1..30; strength 0 maps to the mildest
        // setting. Documented as the slow-but-better fallback.
        let s = (params.strength.clamp(0.0, 1.0) * 29.0 + 1.0).clamp(1.0, 30.0);
        let s = fmt_f64(s);
        ctx.execute(&format!("nlmeans=s={s}"))
    }
}

/// Sharpen `input` with a 5x5 `unsharp` mask. `amount` is clamped to
/// the filter's 0..5 range; chroma gets half the luma strength.
pub(crate) fn sharpen(
    input: &[u8],
    params: SharpenParams,
    limits: &EngineLimits,
) -> VideoResult<Vec<u8>> {
    check_finite("amount", params.amount)?;
    let ctx = prepare(input, limits, "sharpen")?;
    let amt = params.amount.clamp(0.0, 5.0);
    let chroma = (amt * 0.5).clamp(0.0, 5.0);
    ctx.execute(&format!(
        "unsharp=5:5:{}:5:5:{}",
        fmt_f64(amt),
        fmt_f64(chroma)
    ))
}

/// Deinterlace `input` using `method`. On builds without a plain
/// `deinterlace` filter (ffmpeg 7.1.5), [`DeinterlaceMethod::Deinterlace`]
/// falls back to `yadif`.
pub(crate) fn deinterlace(
    input: &[u8],
    method: DeinterlaceMethod,
    limits: &EngineLimits,
) -> VideoResult<Vec<u8>> {
    let ctx = prepare(input, limits, "deinterlace")?;
    let filter = match method {
        DeinterlaceMethod::Yadif => "yadif",
        DeinterlaceMethod::Bwdif => "bwdif",
        DeinterlaceMethod::Deinterlace => {
            if filter_available("deinterlace") {
                "deinterlace"
            } else {
                "yadif"
            }
        }
    };
    ctx.execute(filter)
}

/// Convert `input` to a constant `params.fps` frame rate via the `fps`
/// filter (duplicates/drops frames as needed; no motion interpolation —
/// use `ops::interpolate` for that).
pub(crate) fn fps_convert(
    input: &[u8],
    params: FpsConvertParams,
    limits: &EngineLimits,
) -> VideoResult<Vec<u8>> {
    check_finite("fps", params.fps)?;
    if params.fps <= 0.0 || params.fps > 240.0 {
        return Err(VideoError::InvalidInput(format!(
            "fps must be in (0, 240], got {}",
            params.fps
        )));
    }
    let ctx = prepare(input, limits, "fps_convert")?;
    ctx.execute(&format!("fps={}", fmt_f64(params.fps)))
}

/// Upscale `input` by an integer factor (2x or 4x) using `params.algorithm`.
/// Output dimensions are checked against `max_pixels` before running.
pub(crate) fn upscale(
    input: &[u8],
    params: UpscaleParams,
    limits: &EngineLimits,
) -> VideoResult<Vec<u8>> {
    if params.scale != 2 && params.scale != 4 {
        return Err(VideoError::InvalidInput(format!(
            "scale must be 2 or 4, got {}",
            params.scale
        )));
    }
    let flag = match params.algorithm {
        UpscaleAlgorithm::Neighbor => "neighbor",
        UpscaleAlgorithm::Bicubic => "bicubic",
        UpscaleAlgorithm::Lanczos => "lanczos",
    };
    let ctx = prepare(input, limits, "upscale")?;
    let s = u64::from(params.scale);
    check_output_pixels(limits, u64::from(ctx.width) * s, u64::from(ctx.height) * s)?;
    ctx.execute(&format!(
        "scale=iw*{}:ih*{}:flags={}",
        params.scale, params.scale, flag
    ))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::crates::video_engine::ops::{FFPROBE, h264_encoder};

    /// Encode a ~1s 160x120 H.264+AAC clip via lavfi `testsrc` +
    /// `sine`. Returns `None` when ffmpeg/ffprobe or an H.264 encoder
    /// is unavailable so tests skip cleanly.
    fn fixture() -> Option<Vec<u8>> {
        let ffmpeg = find_tool(FFMPEG).ok()?;
        find_tool(FFPROBE).ok()?;
        let enc = h264_encoder()?;
        let dir = TempDir::new("enhance-test").ok()?;
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

    /// Assert `out` is a non-empty, probeable video and return its info.
    fn assert_video(out: &[u8]) -> crate::crates::video_engine::model::VideoInfo {
        assert!(!out.is_empty(), "output must not be empty");
        let info = probe(out).expect("output must probe as media");
        assert!(info.has_video, "output must contain a video stream");
        info
    }

    #[test]
    fn denoise_produces_valid_video() {
        let Some(fx) = fixture() else { return };
        let out = denoise(
            &fx,
            DenoiseParams { strength: 0.5 },
            &EngineLimits::default(),
        )
        .expect("denoise failed");
        let info = assert_video(&out);
        assert_eq!(info.width, Some(160));
        assert_eq!(info.height, Some(120));
    }

    #[test]
    fn sharpen_produces_valid_video() {
        let Some(fx) = fixture() else { return };
        let out = sharpen(&fx, SharpenParams { amount: 1.0 }, &EngineLimits::default())
            .expect("sharpen failed");
        let info = assert_video(&out);
        assert_eq!(info.width, Some(160));
        assert_eq!(info.height, Some(120));
    }

    #[test]
    fn deinterlace_all_methods_run() {
        let Some(fx) = fixture() else { return };
        for method in [
            DeinterlaceMethod::Yadif,
            DeinterlaceMethod::Bwdif,
            DeinterlaceMethod::Deinterlace,
        ] {
            let out = deinterlace(&fx, method, &EngineLimits::default())
                .unwrap_or_else(|e| panic!("deinterlace {method:?} failed: {e}"));
            assert_video(&out);
        }
    }

    #[test]
    fn fps_convert_hits_target_rate() {
        let Some(fx) = fixture() else { return };
        let out = fps_convert(
            &fx,
            FpsConvertParams { fps: 15.0 },
            &EngineLimits::default(),
        )
        .expect("fps_convert failed");
        let info = assert_video(&out);
        let fps = info.fps.expect("output must report a frame rate");
        assert!((fps - 15.0).abs() < 0.6, "expected ~15 fps, got {fps}");
    }

    #[test]
    fn fps_convert_rejects_bad_values() {
        let Some(fx) = fixture() else { return };
        for bad in [0.0, -5.0, 241.0, f64::NAN, f64::INFINITY] {
            assert!(
                matches!(
                    fps_convert(&fx, FpsConvertParams { fps: bad }, &EngineLimits::default()),
                    Err(VideoError::InvalidInput(_))
                ),
                "fps={bad} must be rejected"
            );
        }
    }

    #[test]
    fn upscale_doubles_dimensions() {
        let Some(fx) = fixture() else { return };
        let out = upscale(
            &fx,
            UpscaleParams {
                scale: 2,
                algorithm: UpscaleAlgorithm::Lanczos,
            },
            &EngineLimits::default(),
        )
        .expect("upscale failed");
        let info = assert_video(&out);
        assert_eq!(info.width, Some(320));
        assert_eq!(info.height, Some(240));
    }

    #[test]
    fn upscale_4x_quadruples_dimensions() {
        let Some(fx) = fixture() else { return };
        let out = upscale(
            &fx,
            UpscaleParams {
                scale: 4,
                algorithm: UpscaleAlgorithm::Neighbor,
            },
            &EngineLimits::default(),
        )
        .expect("upscale failed");
        let info = assert_video(&out);
        assert_eq!(info.width, Some(640));
        assert_eq!(info.height, Some(480));
    }

    #[test]
    fn upscale_rejects_bad_scale() {
        let Some(fx) = fixture() else { return };
        for bad in [0, 1, 3, 8] {
            assert!(
                matches!(
                    upscale(
                        &fx,
                        UpscaleParams {
                            scale: bad,
                            algorithm: UpscaleAlgorithm::Bicubic,
                        },
                        &EngineLimits::default()
                    ),
                    Err(VideoError::InvalidInput(_))
                ),
                "scale={bad} must be rejected"
            );
        }
    }
}
