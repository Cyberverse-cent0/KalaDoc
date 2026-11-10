//! Video stabilization: remove handheld shake via ffmpeg's
//! `vidstabdetect`/`vidstabtransform` two-pass filters (libvidstab)
//! or the single-pass `deshake` filter as a fallback.
//!
//! # Algorithm selection
//!
//! [`StabilizeParams::algo`] is explicit: [`StabilizeAlgo::Vidstab`]
//! runs the two-pass libvidstab pipeline, [`StabilizeAlgo::Deshake`]
//! runs `deshake`. There is no silent downgrade — asking for vidstab
//! on an ffmpeg build without libvidstab fails with
//! [`VideoError::ExternalTool`] naming the missing filters.
//! [`stabilize_auto`] is the convenience entry point that picks
//! vidstab when available, then deshake, then errors.
//!
//! # Parameter mapping
//!
//! - **vidstab**: `shakiness` (1–10) → `vidstabdetect=shakiness`
//!   (1 = little/fast motion, 10 = very strong/quick). `smoothing`
//!   (1–100) → `vidstabtransform=smoothing` (the low-pass window
//!   length, in frames*2+1 units; range 0–1000 — our 1–100 input is
//!   passed through clamped). `vidstabtransform` also gets
//!   `optzoom=1` so ffmpeg computes the optimal static zoom that
//!   hides the empty borders left by `crop=keep` (the default
//!   cropping mode).
//! - **deshake**: there is no smoothing parameter, so `smoothing` is
//!   ignored. `shakiness` is approximated onto the motion-search
//!   radius: `rx = ry = round(shakiness * 5)` clamped to [16, 64]
//!   and rounded to a multiple of `blocksize` (16), which deshake
//!   requires. A larger radius lets the filter
//!   find and cancel bigger translations, which is the closest
//!   deshake analogue of "shakiness". `x/y/w/h = -1` keeps the
//!   automatic full-frame search region; `edge=original` (1) fills
//!   vacated borders with the original image; `blocksize=16` is a
//!   mid-range block size (default 8, max 128).
//!
//! Both paths fully decode + re-encode through the standard
//! H.264/AAC tail ([`h264_aac_args`]) and preserve the input's
//! container extension via [`ext_for_format`], falling back to
//! `mp4`. Output dimensions and duration match the input — the
//! vidstab zoom only scales the *sampled* region, not the canvas.
//!
//! # Limit policy
//!
//! [`check_limits`] runs with `check_pixels = true` after
//! [`probe`], before any ffmpeg invocation: stabilization decodes
//! every frame twice (vidstab) or once (deshake), so the input-side
//! `max_input_bytes`/`max_duration_secs`/`max_pixels` bounds are
//! enforced up front.

use std::path::PathBuf;
use std::sync::OnceLock;

use crate::crates::core::engine::EngineLimits;
use crate::crates::video_engine::error::{VideoError, VideoResult};
use crate::crates::video_engine::ops::probe::probe;
use crate::crates::video_engine::ops::{
    COMMAND_TIMEOUT, FFMPEG, PROBE_TIMEOUT, TempDir, check_input, check_limits, ext_for_format,
    find_tool, h264_aac_args, read_output, run_tool, tool_failure, write_input,
};

/// Which stabilization backend to use.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum StabilizeAlgo {
    /// Two-pass libvidstab pipeline (`vidstabdetect` +
    /// `vidstabtransform`). Higher quality, requires an ffmpeg build
    /// with `--enable-libvidstab`.
    #[default]
    Vidstab,
    /// Single-pass `deshake` filter. Always present in ffmpeg ≥ 2.x;
    /// weaker than vidstab but needs no intermediate transform file.
    Deshake,
}

/// Stabilization parameters. `shakiness` and `smoothing` are validated
/// to the documented ranges (1–10 and 1–100); out-of-range or
/// non-finite values are rejected with [`VideoError::InvalidInput`]
/// rather than silently clamped.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct StabilizeParams {
    /// How shaky the source is: 1 = little/fast motion …
    /// 10 = very strong/quick motion. Maps to
    /// `vidstabdetect=shakiness`, or to deshake's `rx`/`ry` search
    /// radius (see module docs).
    pub shakiness: f64,
    /// Smoothing strength: 1 = minimal … 100 = strongest. Maps to
    /// `vidstabtransform=smoothing`; ignored by deshake.
    pub smoothing: f64,
    /// Backend selection. Use [`stabilize_auto`] if you want the
    /// engine to pick the best available filter instead.
    pub algo: StabilizeAlgo,
}

impl Default for StabilizeParams {
    /// vidstab defaults: `shakiness=5`, `smoothing=15` (both are
    /// ffmpeg's own defaults for the corresponding options).
    fn default() -> Self {
        Self {
            shakiness: 5.0,
            smoothing: 15.0,
            algo: StabilizeAlgo::Vidstab,
        }
    }
}

impl StabilizeParams {
    /// Parameters for the deshake fallback path.
    pub fn deshake(shakiness: f64, smoothing: f64) -> Self {
        Self {
            shakiness,
            smoothing,
            algo: StabilizeAlgo::Deshake,
        }
    }

    /// Same strengths, explicit vidstab backend (same as
    /// [`StabilizeParams::default`] field values but named).
    pub fn vidstab(shakiness: f64, smoothing: f64) -> Self {
        Self {
            shakiness,
            smoothing,
            algo: StabilizeAlgo::Vidstab,
        }
    }

    /// Reject non-finite or out-of-range strengths.
    fn validate(&self) -> VideoResult<()> {
        for (name, value, lo, hi) in [
            ("shakiness", self.shakiness, 1.0, 10.0),
            ("smoothing", self.smoothing, 1.0, 100.0),
        ] {
            if !value.is_finite() || !(lo..=hi).contains(&value) {
                return Err(VideoError::InvalidInput(format!(
                    "stabilize {name} must be in [{lo}, {hi}], got {value}"
                )));
            }
        }
        Ok(())
    }
}

/// Probed-and-staged input shared by both stabilization paths.
struct Prepared {
    dir: TempDir,
    input_path: PathBuf,
    ext: String,
}

/// Probe `input`, enforce input-side limits, and stage it in a
/// [`TempDir`]. Fails with [`VideoError::InvalidInput`] when the media
/// has no video stream or reports no dimensions — there are no frames
/// to stabilize.
fn prepare(input: &[u8], limits: &EngineLimits, tag: &str) -> VideoResult<Prepared> {
    let input_len = check_input(input)?;
    find_tool(FFMPEG)?;
    let info = probe(input)?;
    if !info.has_video {
        return Err(VideoError::InvalidInput(
            "stabilization requires a video stream".to_string(),
        ));
    }
    if info.width.is_none() || info.height.is_none() {
        return Err(VideoError::InvalidInput(
            "video stream reports no dimensions".to_string(),
        ));
    }
    // Stabilization decodes the whole clip (twice for vidstab), so the
    // input-side bounds are the meaningful guard here.
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
        dir,
        input_path,
        ext,
    })
}

/// Whether `ffmpeg` was built with the named avfilter. The filter
/// list is fetched once via `ffmpeg -hide_banner -filters` and cached;
/// if the probe fails the filter is reported absent (callers then
/// surface the requirement error honestly).
fn filter_available(name: &str) -> bool {
    static FILTERS: OnceLock<Vec<String>> = OnceLock::new();
    let list = FILTERS.get_or_init(|| {
        let Ok(program) = find_tool(FFMPEG) else {
            return Vec::new();
        };
        let args = vec!["-hide_banner".to_string(), "-filters".to_string()];
        match run_tool(FFMPEG, &program, &args, PROBE_TIMEOUT) {
            Ok(output) if output.status.success() => String::from_utf8_lossy(&output.stdout)
                .lines()
                // " TSC name  AVOptions ..." — the filter name is the
                // second whitespace-separated token.
                .filter_map(|line| line.split_whitespace().nth(1))
                .map(str::to_string)
                .collect(),
            _ => Vec::new(),
        }
    });
    list.iter().any(|f| f == name)
}

fn vidstab_available() -> bool {
    filter_available("vidstabdetect") && filter_available("vidstabtransform")
}

fn deshake_available() -> bool {
    filter_available("deshake")
}

/// Error for a missing stabilization backend, naming the filters the
/// ffmpeg build must provide.
fn missing_backend(detail: &str) -> VideoError {
    VideoError::ExternalTool {
        tool: FFMPEG.to_string(),
        detail: format!(
            "video stabilization {detail}: ffmpeg needs libvidstab \
             (vidstabdetect + vidstabtransform filters) or the deshake \
             filter; neither is available in this build"
        ),
    }
}

/// Run one ffmpeg pass over `prepared` and read back the produced
/// file. `extra` args are inserted between `-i` and the encode tail.
fn run_pass(prepared: &Prepared, extra: &[String], tag: &str) -> VideoResult<Vec<u8>> {
    let program = find_tool(FFMPEG)?;
    let out_path = prepared.dir.file(&format!("{tag}.{}", prepared.ext));
    let mut args = vec![
        "-y".to_string(),
        "-v".to_string(),
        "error".to_string(),
        "-i".to_string(),
        prepared.input_path.to_string_lossy().into_owned(),
    ];
    args.extend_from_slice(extra);
    args.extend(h264_aac_args()?);
    args.push(out_path.to_string_lossy().into_owned());

    let output = run_tool(FFMPEG, &program, &args, COMMAND_TIMEOUT)?;
    if !output.status.success() {
        return Err(tool_failure(FFMPEG, &output));
    }
    read_output(&out_path)
}

/// Two-pass vidstab stabilization. Pass 1 (`vidstabdetect`) analyzes
/// the clip and writes the camera-motion transform file; pass 2
/// (`vidstabtransform`) applies the smoothed inverse motion.
fn stabilize_vidstab(prepared: &Prepared, params: &StabilizeParams) -> VideoResult<Vec<u8>> {
    // vidstabdetect's `shakiness` is an int option (1–10); the input
    // is already range-checked, so round to the nearest int.
    let shakiness = params.shakiness.round() as i64;
    let trf_path = prepared.dir.file("transforms.trf");
    let trf = trf_path.to_string_lossy().into_owned();

    let program = find_tool(FFMPEG)?;
    let detect = format!("vidstabdetect=shakiness={shakiness}:result={trf}");
    let pass1_args = vec![
        "-y".to_string(),
        "-v".to_string(),
        "error".to_string(),
        "-i".to_string(),
        prepared.input_path.to_string_lossy().into_owned(),
        "-vf".to_string(),
        detect,
        // Decode-only analysis pass: null muxer, no output file.
        "-f".to_string(),
        "null".to_string(),
        "-".to_string(),
    ];
    let output = run_tool(FFMPEG, &program, &pass1_args, COMMAND_TIMEOUT)?;
    if !output.status.success() {
        return Err(tool_failure(FFMPEG, &output));
    }

    // vidstabtransform `smoothing` is frames*2+1 (0–1000); our
    // 1–100 param maps through directly. `optzoom=1` computes the
    // optimal static zoom so the `keep` crop border does not show;
    // `zoom=0` leaves no additional user zoom.
    let smoothing = params.smoothing.round().clamp(0.0, 1000.0) as i64;
    let transform = format!("vidstabtransform=input={trf}:smoothing={smoothing}:zoom=0:optzoom=1");
    run_pass(
        prepared,
        &["-vf".to_string(), transform],
        "stabilized-vidstab",
    )
}

/// Single-pass deshake stabilization. `shakiness` maps to the
/// `rx`/`ry` motion-search radius (see module docs); `smoothing` has
/// no deshake equivalent and is ignored.
fn stabilize_deshake(prepared: &Prepared, params: &StabilizeParams) -> VideoResult<Vec<u8>> {
    // rx/ry range 0–64 and must be a multiple of blocksize (16);
    // shakiness 1–10 → radius 5–50 rounded to the nearest block.
    let raw = (params.shakiness * 5.0).round().clamp(16.0, 64.0) as i64;
    let radius = ((raw + 8) / 16) * 16;
    let filter = format!("deshake=x=-1:y=-1:w=-1:h=-1:rx={radius}:ry={radius}:edge=1:blocksize=16");
    run_pass(prepared, &["-vf".to_string(), filter], "stabilized-deshake")
}

/// Stabilize `input` with the backend selected by
/// `params.algo`. Fails with [`VideoError::ExternalTool`] when the
/// requested filters are absent from this ffmpeg build — there is no
/// fallback for an explicit algorithm choice (use
/// [`stabilize_auto`] for automatic selection).
pub(crate) fn stabilize_video(
    input: &[u8],
    params: &StabilizeParams,
    limits: &EngineLimits,
) -> VideoResult<Vec<u8>> {
    params.validate()?;
    let prepared = prepare(input, limits, "stabilize")?;
    match params.algo {
        StabilizeAlgo::Vidstab => {
            if !vidstab_available() {
                return Err(missing_backend("requested vidstabdetect/vidstabtransform"));
            }
            stabilize_vidstab(&prepared, params)
        }
        StabilizeAlgo::Deshake => {
            if !deshake_available() {
                return Err(missing_backend("requested deshake"));
            }
            stabilize_deshake(&prepared, params)
        }
    }
}

/// Stabilize `input` with the best available backend: vidstab when
/// both libvidstab filters exist, deshake otherwise. `params.algo`
/// is ignored — this entry point exists precisely to pick the
/// algorithm. Strengths are still taken from `params`
/// (`shakiness`/`smoothing`); deshake ignores `smoothing`.
pub(crate) fn stabilize_auto(
    input: &[u8],
    params: &StabilizeParams,
    limits: &EngineLimits,
) -> VideoResult<Vec<u8>> {
    params.validate()?;
    if vidstab_available() {
        let prepared = prepare(input, limits, "stabilize")?;
        return stabilize_vidstab(&prepared, params);
    }
    if deshake_available() {
        let prepared = prepare(input, limits, "stabilize")?;
        return stabilize_deshake(&prepared, params);
    }
    Err(missing_backend("no stabilization filter available"))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::crates::video_engine::ops::{FFPROBE, h264_encoder};

    /// Encode a ~1s 160x120 H.264+AAC clip via lavfi `testsrc` +
    /// `sine` (the fixture has no real shake, but the stabilization
    /// filters still run end-to-end). Returns `None` when ffmpeg,
    /// ffprobe, or an H.264 encoder is unavailable so tests skip.
    fn fixture() -> Option<Vec<u8>> {
        let ffmpeg = find_tool(FFMPEG).ok()?;
        find_tool(FFPROBE).ok()?;
        let dir = TempDir::new("stab-test").ok()?;
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
            h264_encoder()?.to_string(),
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

    fn info(bytes: &[u8]) -> VideoInfo {
        probe(bytes).expect("output must probe")
    }

    #[test]
    fn deshake_preserves_dims_and_duration() {
        let Some(fx) = fixture() else { return };
        if !deshake_available() {
            return;
        }
        let params = StabilizeParams::deshake(5.0, 15.0);
        let out = stabilize_video(&fx, &params, &EngineLimits::default())
            .expect("deshake stabilization failed");
        assert!(!out.is_empty());
        let (src, dst) = (info(&fx), info(&out));
        assert_eq!((src.width, src.height), (dst.width, dst.height));
        let (Some(sd), Some(dd)) = (src.duration_secs, dst.duration_secs) else {
            return;
        };
        assert!((sd - dd).abs() < 0.5, "duration changed: {sd} -> {dd}");
    }

    #[test]
    fn vidstab_runs_when_available() {
        let Some(fx) = fixture() else { return };
        if !vidstab_available() {
            // Filter absent on this ffmpeg build — skip rather than
            // fake a pass.
            return;
        }
        let params = StabilizeParams::vidstab(5.0, 15.0);
        let out = stabilize_video(&fx, &params, &EngineLimits::default())
            .expect("vidstab stabilization failed");
        assert!(!out.is_empty());
        let dst = info(&out);
        assert_eq!((dst.width, dst.height), (Some(160), Some(120)));
    }

    #[test]
    fn auto_picks_an_available_backend() {
        let Some(fx) = fixture() else { return };
        if !vidstab_available() && !deshake_available() {
            return;
        }
        let out = stabilize_auto(&fx, &StabilizeParams::default(), &EngineLimits::default())
            .expect("auto stabilization failed");
        assert!(!out.is_empty());
    }

    #[test]
    fn invalid_params_are_rejected() {
        let Some(fx) = fixture() else { return };
        for bad in [
            StabilizeParams {
                shakiness: 0.0,
                ..StabilizeParams::default()
            },
            StabilizeParams {
                shakiness: 11.0,
                ..StabilizeParams::default()
            },
            StabilizeParams {
                smoothing: f64::NAN,
                ..StabilizeParams::default()
            },
            StabilizeParams {
                smoothing: 101.0,
                ..StabilizeParams::default()
            },
        ] {
            assert!(matches!(
                stabilize_video(&fx, &bad, &EngineLimits::default()),
                Err(VideoError::InvalidInput(_))
            ));
        }
    }

    #[test]
    fn explicit_vidstab_errors_without_libvidstab() {
        let Some(fx) = fixture() else { return };
        if vidstab_available() {
            return;
        }
        assert!(matches!(
            stabilize_video(&fx, &StabilizeParams::default(), &EngineLimits::default()),
            Err(VideoError::ExternalTool { .. })
        ));
    }
}
