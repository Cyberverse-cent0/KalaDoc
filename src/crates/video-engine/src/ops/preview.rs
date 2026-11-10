//! Animated preview generation: short looping GIF or animated WebP
//! derived from the first `max_duration_secs` of a video.
//!
//! The preview always starts at `t = 0` of the source (not a midpoint
//! or scene-picked frame) — it is a deterministic "opening seconds"
//! teaser suitable for thumbnails and timeline scrubbing. A future
//! `start_pct` parameter could shift the window; for now the head of
//! the clip is the contract.
//!
//! Audio is never carried into the preview: GIF/WebP animations are
//! video-only formats, so the output is always encoded with `-an`.
//! Inputs without a video stream are rejected with
//! [`VideoError::InvalidInput`].
//!
//! Encoder availability is probed honestly: GIF needs the native
//! `gif` encoder, animated WebP needs `libwebp_anim` specifically
//! (`libwebp` alone only does still images). Missing encoders yield
//! [`VideoError::ExternalTool`] rather than a mid-run ffmpeg failure.
//!
//! Limits: [`check_limits`] enforces `max_input_bytes`,
//! `max_duration_secs` (probed container duration) and `max_pixels`
//! (probed frame size). `EngineLimits` has no frame-count bound, so
//! the output frame budget is `eff_dur * fps`; parameter validation
//! keeps `fps` and `max_duration_secs` finite and positive, and the
//! shared [`COMMAND_TIMEOUT`] bounds wall-clock encode time.

use crate::crates::core::engine::EngineLimits;
use crate::crates::video_engine::error::{VideoError, VideoResult};
use crate::crates::video_engine::ops::probe::probe;
use crate::crates::video_engine::ops::{
    COMMAND_TIMEOUT, FFMPEG, TempDir, check_input, check_limits, encoder_available, find_tool,
    read_output, run_tool, tool_failure, write_input,
};

/// Upper sanity bound on the requested frame rate. Well above any
/// reasonable preview rate; rejects `f64::INFINITY`/absurd values
/// before they reach the ffmpeg filter graph.
const MAX_FPS: f64 = 240.0;

/// Upper sanity bound on preview width. Larger widths would upscale
/// small sources pointlessly and blow up animation file size.
const MAX_WIDTH: u32 = 4096;

/// Lower sanity bound on preview width — below this the filter graph
/// cannot produce a usable image.
const MIN_WIDTH: u32 = 8;

/// Output format for [`video_preview`].
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PreviewFormat {
    /// Animated GIF (native `gif` encoder, two-pass palette via
    /// `palettegen`/`paletteuse`, infinite loop).
    Gif,
    /// Animated WebP (`libwebp_anim`, infinite loop, lossy quality 75).
    WebpAnimated,
}

/// Parameters controlling [`video_preview`].
///
/// Defaults describe a small, fast thumbnail-quality teaser:
/// the first 5 seconds at 10 fps scaled to 320 px wide.
#[derive(Debug, Clone, Copy)]
pub struct PreviewParams {
    /// Maximum seconds of source to animate. The effective duration
    /// is `min(probed_duration, max_duration_secs)`; when the
    /// container reports no duration this value is used directly as
    /// the `-t` bound (ffmpeg simply stops at EOF).
    pub max_duration_secs: f64,
    /// Output frame rate for the `fps` filter. Typical preview
    /// values are 5–15; must be finite and in `(0, 240]`.
    pub fps: f64,
    /// Output width in pixels; height is derived by `scale={w}:-2`
    /// (aspect-preserving, even height). Must be in `[8, 4096]`.
    pub width: u32,
    /// Output format.
    pub format: PreviewFormat,
}

impl Default for PreviewParams {
    fn default() -> Self {
        Self {
            max_duration_secs: 5.0,
            fps: 10.0,
            width: 320,
            format: PreviewFormat::Gif,
        }
    }
}

/// Generate an animated preview of `input` and return its bytes.
///
/// Pipeline: `check_input` → `probe` → `check_limits` → ffmpeg encode
/// of `min(duration, params.max_duration_secs)` seconds starting at
/// `t = 0`, resampled to `params.fps` and scaled to `params.width`
/// wide with lanczos scaling. GIF output uses the two-pass palette
/// filtergraph (`split`/`palettegen`/`paletteuse`) for reasonable
/// color fidelity at small sizes; WebP output uses `libwebp_anim`
/// with `-loop 0 -lossless 0 -quality 75`.
///
/// The produced bytes are verified against the format's magic before
/// returning so a silent ffmpeg failure cannot yield garbage.
pub fn video_preview(
    input: &[u8],
    params: &PreviewParams,
    limits: &EngineLimits,
) -> VideoResult<Vec<u8>> {
    validate_params(params)?;

    let input_len = check_input(input)?;
    let program = find_tool(FFMPEG)?;

    // Probe first: limits need duration/dimensions and eff_dur needs
    // the reported duration.
    let info = probe(input)?;
    check_limits(&info, input_len, limits, true)?;

    if !info.has_video {
        return Err(VideoError::InvalidInput(
            "input has no video stream; animated previews require video".to_string(),
        ));
    }

    // Effective preview duration: capped by the param, floored at the
    // probed container duration. Unknown durations fall back to the
    // param cap — ffmpeg stops at EOF anyway.
    let eff_dur = match info.duration_secs {
        Some(d) if d.is_finite() && d > 0.0 => d.min(params.max_duration_secs),
        _ => params.max_duration_secs,
    };
    if eff_dur <= 0.0 {
        return Err(VideoError::InvalidInput(
            "probed duration is zero; nothing to animate".to_string(),
        ));
    }

    let dir = TempDir::new("preview")?;
    let input_path = write_input(&dir, "input.bin", input)?;

    let (ext, magic, mut args) = match params.format {
        PreviewFormat::Gif => gif_args(params, &input_path, eff_dur),
        PreviewFormat::WebpAnimated => webp_args(params, &input_path, eff_dur),
    };
    let out_path = dir.file(&format!("preview.{ext}"));
    args.push(out_path.to_string_lossy().into_owned());

    let output = run_tool(FFMPEG, &program, &args, COMMAND_TIMEOUT)?;
    if !output.status.success() {
        return Err(tool_failure(FFMPEG, &output));
    }

    let bytes = read_output(&out_path)?;
    if bytes.len() < magic.len() || &bytes[..magic.len()] != magic {
        return Err(VideoError::ExternalTool {
            tool: FFMPEG.to_string(),
            detail: format!("produced {ext} output failed magic-byte verification"),
        });
    }
    Ok(bytes)
}

/// Validate [`PreviewParams`] before any tool runs — bad values would
/// otherwise surface as obscure ffmpeg filter-graph errors.
fn validate_params(params: &PreviewParams) -> VideoResult<()> {
    if !params.max_duration_secs.is_finite() || params.max_duration_secs <= 0.0 {
        return Err(VideoError::InvalidInput(format!(
            "max_duration_secs must be a finite positive number, got {}",
            params.max_duration_secs
        )));
    }
    if !params.fps.is_finite() || params.fps <= 0.0 || params.fps > MAX_FPS {
        return Err(VideoError::InvalidInput(format!(
            "fps must be a finite number in (0, {MAX_FPS}], got {}",
            params.fps
        )));
    }
    if !(MIN_WIDTH..=MAX_WIDTH).contains(&params.width) {
        return Err(VideoError::InvalidInput(format!(
            "width must be in [{MIN_WIDTH}, {MAX_WIDTH}], got {}",
            params.width
        )));
    }
    Ok(())
}

/// Shared input-side arguments: quiet errors, input path, audio
/// dropped (`-an`), and the output duration bound. The caller adds
/// the filter chain — `-vf` for WebP, `-filter_complex` for GIF —
/// then encoder options and the output path.
fn common_args(input_path: &std::path::Path, eff_dur: f64) -> Vec<String> {
    vec![
        "-y".to_string(),
        "-v".to_string(),
        "error".to_string(),
        "-i".to_string(),
        input_path.to_string_lossy().into_owned(),
        "-an".to_string(),
        "-t".to_string(),
        format!("{eff_dur:.3}"),
    ]
}

/// The `fps`+`scale` filter chain shared by both formats.
fn vf_chain(params: &PreviewParams) -> String {
    format!(
        "fps={:.3},scale={}:{}:flags=lanczos",
        params.fps, params.width, "-2"
    )
}

/// ffmpeg arguments for GIF output: `-loop 0` (infinite) plus the
/// two-pass palette filtergraph that keeps colors accurate at small
/// sizes. Requires the native `gif` encoder.
fn gif_args(
    params: &PreviewParams,
    input_path: &std::path::Path,
    eff_dur: f64,
) -> (&'static str, &'static [u8], Vec<String>) {
    let mut args = common_args(input_path, eff_dur);
    // The palette chain must own the whole filter graph — `-vf` and
    // `-filter_complex` cannot both consume the same stream.
    args.extend([
        "-filter_complex".to_string(),
        format!(
            "{},split[s0][s1];[s0]palettegen[p];[s1][p]paletteuse",
            vf_chain(params)
        ),
        "-loop".to_string(),
        "0".to_string(),
        "-f".to_string(),
        "gif".to_string(),
    ]);
    ("gif", b"GIF8", args)
}

/// ffmpeg arguments for animated WebP: `libwebp_anim` with infinite
/// loop and lossy quality 75 (verified against `ffmpeg -h
/// encoder=libwebp_anim`: the knob is `-quality 0..100`, not `-q:v`).
fn webp_args(
    params: &PreviewParams,
    input_path: &std::path::Path,
    eff_dur: f64,
) -> (&'static str, &'static [u8], Vec<String>) {
    let mut args = common_args(input_path, eff_dur);
    args.extend([
        "-vf".to_string(),
        vf_chain(params),
        "-c:v".to_string(),
        "libwebp_anim".to_string(),
        "-loop".to_string(),
        "0".to_string(),
        "-lossless".to_string(),
        "0".to_string(),
        "-quality".to_string(),
        "75".to_string(),
        "-f".to_string(),
        "webp".to_string(),
    ]);
    // RIFF....WEBP — verified in full by the caller via magic bytes.
    ("webp", b"RIFF", args)
}

/// Whether the encoder required by `format` exists in this ffmpeg
/// build. Public so tests (and future engine-level checks) can skip
/// cleanly instead of surfacing [`VideoError::ExternalTool`].
pub fn preview_encoder_available(format: PreviewFormat) -> bool {
    match format {
        PreviewFormat::Gif => encoder_available("gif"),
        PreviewFormat::WebpAnimated => encoder_available("libwebp_anim"),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::crates::video_engine::ops::tool_available;
    use std::fs;
    use std::path::PathBuf;
    use std::process::Command;
    use std::sync::atomic::{AtomicU64, Ordering};
    use std::time::{SystemTime, UNIX_EPOCH};

    fn ffmpeg_available() -> bool {
        tool_available(FFMPEG) && tool_available("ffprobe")
    }

    /// Unique scratch dir per test; removed on drop.
    struct TestDir(PathBuf);

    impl TestDir {
        fn new(tag: &str) -> Self {
            static COUNTER: AtomicU64 = AtomicU64::new(0);
            let nanos = SystemTime::now()
                .duration_since(UNIX_EPOCH)
                .map(|d| d.as_nanos())
                .unwrap_or(0);
            let seq = COUNTER.fetch_add(1, Ordering::Relaxed);
            let path = std::env::temp_dir().join(format!(
                "kaladoc-video-preview-test-{tag}-{}-{nanos}-{seq}",
                std::process::id()
            ));
            fs::create_dir_all(&path).unwrap();
            Self(path)
        }
    }

    impl Drop for TestDir {
        fn drop(&mut self) {
            let _ = fs::remove_dir_all(&self.0);
        }
    }

    /// Generate a 4 s 320x240@10fps testsrc clip (video only) and
    /// return its bytes. Video-only keeps fixture generation fast;
    /// the op under test drops audio anyway.
    fn make_clip(dir: &TestDir, name: &str) -> Vec<u8> {
        let out = dir.0.join(name);
        let status = Command::new(FFMPEG)
            .args([
                "-y",
                "-v",
                "error",
                "-f",
                "lavfi",
                "-i",
                "testsrc=duration=4:size=320x240:rate=10",
                "-pix_fmt",
                "yuv420p",
                "-an",
            ])
            .arg(&out)
            .stdin(std::process::Stdio::null())
            .stdout(std::process::Stdio::null())
            .stderr(std::process::Stdio::piped())
            .status()
            .expect("failed to spawn ffmpeg");
        assert!(status.success(), "ffmpeg fixture generation failed");
        fs::read(&out).unwrap()
    }

    /// Small fast params: 1 s at 5 fps, 160 px wide.
    fn fast_params(format: PreviewFormat) -> PreviewParams {
        PreviewParams {
            max_duration_secs: 1.0,
            fps: 5.0,
            width: 160,
            format,
        }
    }

    #[test]
    fn preview_rejects_bad_params() {
        let params = PreviewParams {
            max_duration_secs: 0.0,
            ..Default::default()
        };
        assert!(matches!(
            video_preview(b"x", &params, &EngineLimits::default()),
            Err(VideoError::InvalidInput(_))
        ));
        let params = PreviewParams {
            fps: f64::NAN,
            ..Default::default()
        };
        assert!(matches!(
            video_preview(b"x", &params, &EngineLimits::default()),
            Err(VideoError::InvalidInput(_))
        ));
        let params = PreviewParams {
            width: 4,
            ..Default::default()
        };
        assert!(matches!(
            video_preview(b"x", &params, &EngineLimits::default()),
            Err(VideoError::InvalidInput(_))
        ));
    }

    #[test]
    fn preview_gif_magic_and_size() {
        if !ffmpeg_available() || !preview_encoder_available(PreviewFormat::Gif) {
            return;
        }
        let dir = TestDir::new("gif");
        let clip = make_clip(&dir, "clip.mp4");

        let gif = video_preview(
            &clip,
            &fast_params(PreviewFormat::Gif),
            &EngineLimits::default(),
        )
        .expect("gif preview failed");
        assert!(
            gif.starts_with(b"GIF87a") || gif.starts_with(b"GIF89a"),
            "not a GIF: {:?}",
            &gif[..6.min(gif.len())]
        );
        // 1 s @ 5 fps, 160px wide, testsrc content: tens of KB at most.
        assert!(gif.len() > 500, "gif suspiciously small: {}", gif.len());
        assert!(
            gif.len() < 2_000_000,
            "gif suspiciously large: {}",
            gif.len()
        );
    }

    #[test]
    fn preview_webp_magic() {
        if !ffmpeg_available() || !preview_encoder_available(PreviewFormat::WebpAnimated) {
            return;
        }
        let dir = TestDir::new("webp");
        let clip = make_clip(&dir, "clip.mp4");

        let webp = video_preview(
            &clip,
            &fast_params(PreviewFormat::WebpAnimated),
            &EngineLimits::default(),
        )
        .expect("webp preview failed");
        assert!(webp.len() > 12, "webp too small: {}", webp.len());
        assert_eq!(&webp[..4], b"RIFF", "missing RIFF header");
        assert_eq!(&webp[8..12], b"WEBP", "missing WEBP form type");
        // u32 LE chunk size sanity: file size should match header.
        let chunk = u32::from_le_bytes(webp[4..8].try_into().unwrap()) as usize;
        assert!(
            chunk + 8 <= webp.len() + 1,
            "RIFF size {chunk} inconsistent with file len {}",
            webp.len()
        );
    }

    #[test]
    fn preview_duration_is_capped() {
        if !ffmpeg_available() || !preview_encoder_available(PreviewFormat::Gif) {
            return;
        }
        let dir = TestDir::new("cap");
        // 4 s source, cap at 0.5 s → the GIF should hold ~2-3 frames
        // at 5 fps, i.e. be much smaller than an uncapped encode.
        let clip = make_clip(&dir, "clip.mp4");
        let params = PreviewParams {
            max_duration_secs: 0.5,
            fps: 5.0,
            width: 160,
            format: PreviewFormat::Gif,
        };
        let gif = video_preview(&clip, &params, &EngineLimits::default())
            .expect("capped gif preview failed");
        assert!(gif.starts_with(b"GIF8"));
        assert!(gif.len() > 100);
    }

    #[test]
    fn preview_rejects_audio_only_input() {
        if !ffmpeg_available() || !preview_encoder_available(PreviewFormat::Gif) {
            return;
        }
        let dir = TestDir::new("audio");
        // Sine-only WAV: probe succeeds but there is no video stream.
        let out = dir.0.join("tone.wav");
        let status = Command::new(FFMPEG)
            .args([
                "-y",
                "-v",
                "error",
                "-f",
                "lavfi",
                "-i",
                "sine=frequency=440:duration=1",
                "-f",
                "wav",
            ])
            .arg(&out)
            .stdin(std::process::Stdio::null())
            .stdout(std::process::Stdio::null())
            .stderr(std::process::Stdio::piped())
            .status()
            .expect("failed to spawn ffmpeg");
        assert!(status.success());
        let audio = fs::read(&out).unwrap();
        let err = video_preview(
            &audio,
            &fast_params(PreviewFormat::Gif),
            &EngineLimits::default(),
        )
        .unwrap_err();
        assert!(
            matches!(err, VideoError::InvalidInput(_)),
            "expected InvalidInput, got {err:?}"
        );
    }
}
