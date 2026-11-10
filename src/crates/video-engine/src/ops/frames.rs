//! Frame extraction: timed intervals, fixed fps, all frames, and
//! keyframe-only pulls — emitted as a [`MediaOutput`] of named image
//! files.
//!
//! Every selector funnels through the same pipeline: `check_input` →
//! probe → `check_limits` → stage in a [`TempDir`] → one ffmpeg
//! `image2` sequence run → sorted, capped [`MediaFile`] collection.
//! ffmpeg 7.x is assumed: `-fps_mode passthrough` (the `-vsync 0`
//! spelling is deprecated) preserves every decoded frame for the
//! `all`/`keyframes_only` selectors instead of resampling to CFR.

use std::fs;
use std::path::{Path, PathBuf};

use crate::crates::core::engine::EngineLimits;
use crate::crates::video_engine::error::{VideoError, VideoResult};
use crate::crates::video_engine::model::{FramesSpec, MediaFile, MediaOutput, VideoInfo};
use crate::crates::video_engine::ops::probe::probe;
use crate::crates::video_engine::ops::{
    COMMAND_TIMEOUT, FFMPEG, TempDir, check_input, check_limits, checked_ext, encoder_available,
    find_tool, read_output, run_tool, tool_failure, write_input,
};

/// Image formats accepted for frame output. Anything else is
/// [`VideoError::InvalidInput`] before ffmpeg runs.
const FRAME_FORMATS: &[&str] = &["png", "jpg", "jpeg", "webp", "bmp"];

/// Cap on produced frame files when `limits.max_outputs` is unset —
/// matches the documented "default cap 10000 frames".
const DEFAULT_MAX_FRAMES: u32 = 10_000;

/// ffmpeg encoder name the `image2` muxer needs for a normalized
/// frame extension. `unreachable!` is safe: callers validate `ext`
/// against [`FRAME_FORMATS`] first.
fn image_encoder(ext: &str) -> &'static str {
    match ext {
        "png" => "png",
        "jpg" | "jpeg" => "mjpeg",
        "webp" => "libwebp",
        "bmp" => "bmp",
        _ => unreachable!("image_encoder called with unvalidated ext '{ext}'"),
    }
}

/// Format an f64 for an ffmpeg filter argument: fixed 9 decimals with
/// trailing zeros trimmed, so tiny rates stay non-zero (`0.000000001`)
/// while integers print bare (`30`, not `30.000000000`). Values that
/// round to zero at this precision are rejected by callers first.
fn fmt_f64(v: f64) -> String {
    let s = format!("{v:.9}");
    let trimmed = s.trim_end_matches('0').trim_end_matches('.');
    if trimmed.is_empty() || trimmed == "-" {
        "0".to_string()
    } else {
        trimmed.to_string()
    }
}

/// Rough upper bound on frames a selector will emit, used only for an
/// early [`EngineLimits::max_outputs`] check — the authoritative cap
/// is applied to the collected files. `estimate_fps` is the output
/// cadence (`None` for `all`-mode when the probed fps is unknown and
/// for keyframe-only pulls, whose count cannot be predicted).
fn estimated_frames(info: &VideoInfo, estimate_fps: Option<f64>) -> Option<u64> {
    let (Some(duration), Some(fps)) = (info.duration_secs, estimate_fps) else {
        return None;
    };
    if !duration.is_finite() || duration < 0.0 || !fps.is_finite() || fps <= 0.0 {
        return None;
    }
    Some((duration * fps).ceil() as u64 + 1)
}

/// Early `max_outputs` check against [`estimated_frames`].
fn check_frame_budget(
    info: &VideoInfo,
    estimate_fps: Option<f64>,
    limits: &EngineLimits,
) -> VideoResult<()> {
    let max = limits.max_outputs.unwrap_or(DEFAULT_MAX_FRAMES);
    if let Some(est) = estimated_frames(info, estimate_fps)
        && est > max as u64
    {
        return Err(VideoError::LimitExceeded(format!(
            "frame extraction would produce ~{est} frames, exceeding max_outputs {max}"
        )));
    }
    Ok(())
}

/// Collect the `prefix%06d.ext` files ffmpeg wrote next to
/// `out_pattern`, sorted by name, capped at `max_outputs`
/// ([`DEFAULT_MAX_FRAMES`] when unset). An empty result is an honest
/// error — ffmpeg exits 0 with no output when a seek lands past the
/// last frame or the input has no video stream.
fn collect_sequence(
    out_pattern: &Path,
    prefix: &str,
    ext: &str,
    limits: &EngineLimits,
) -> VideoResult<Vec<MediaFile>> {
    let dir_path = out_pattern.parent().unwrap_or_else(|| Path::new("."));
    let suffix = format!(".{ext}");
    let mut paths: Vec<PathBuf> = fs::read_dir(dir_path)?
        .filter_map(|entry| entry.ok().map(|e| e.path()))
        .filter(|p| {
            p.file_name()
                .and_then(|n| n.to_str())
                .is_some_and(|n| n.starts_with(prefix) && n.ends_with(&suffix))
        })
        .collect();
    // Full paths share the tempdir prefix, so path order == name order.
    paths.sort();

    let max = limits.max_outputs.unwrap_or(DEFAULT_MAX_FRAMES) as usize;
    if paths.len() > max {
        return Err(VideoError::LimitExceeded(format!(
            "frame extraction produced {} files, exceeding max_outputs {max}",
            paths.len()
        )));
    }
    if paths.is_empty() {
        return Err(VideoError::ExternalTool {
            tool: FFMPEG.to_string(),
            detail: "no frames produced (input may have no video stream or the \
                     requested range is empty)"
                .to_string(),
        });
    }

    let mut files = Vec::with_capacity(paths.len());
    for path in paths {
        let name = path
            .file_name()
            .map(|n| n.to_string_lossy().into_owned())
            .unwrap_or_default();
        let bytes = read_output(&path)?;
        if bytes.is_empty() {
            return Err(VideoError::ExternalTool {
                tool: FFMPEG.to_string(),
                detail: format!("frame file '{name}' is empty"),
            });
        }
        files.push(MediaFile { name, bytes });
    }
    Ok(files)
}

/// Shared runner: validate the image encoder, stage the input, run one
/// ffmpeg `image2` sequence command, and collect the numbered frames.
///
/// - `vf`: optional `-vf` filtergraph string (already escaped for
///   ffmpeg's filter parser — args are passed without a shell).
/// - `passthrough`: add `-fps_mode passthrough` so every decoded
///   frame is muxed exactly once (required for `all` and
///   keyframe-only selectors; wrong for `fps=` resampling).
/// - `start_number`: image2 first-frame number (`None` = muxer
///   default 1).
/// - `estimate_fps`: output cadence for the early budget check.
#[allow(clippy::too_many_arguments)]
fn run_sequence(
    input: &[u8],
    _input_len: u64,
    info: &VideoInfo,
    limits: &EngineLimits,
    ext: &str,
    vf: Option<String>,
    passthrough: bool,
    start_number: Option<u64>,
    prefix: &str,
    estimate_fps: Option<f64>,
) -> VideoResult<MediaOutput> {
    // Fail honestly up front when this ffmpeg build lacks the image
    // encoder the extension needs (e.g. webp without libwebp).
    let encoder = image_encoder(ext);
    if !encoder_available(encoder) {
        return Err(VideoError::ExternalTool {
            tool: FFMPEG.to_string(),
            detail: format!(
                "frame format '{ext}' requires the '{encoder}' encoder, which \
                 this ffmpeg build does not provide"
            ),
        });
    }
    check_frame_budget(info, estimate_fps, limits)?;

    let program = find_tool(FFMPEG)?;
    let dir = TempDir::new("frames")?;
    // Stage with the probed container extension when known so ffmpeg's
    // demuxer probing gets the friendliest hint; `.bin` still works
    // (ffmpeg probes content), matching thumbnail.rs.
    let input_ext = info
        .format_name
        .as_deref()
        .and_then(crate::crates::video_engine::ops::ext_for_format)
        .unwrap_or("bin");
    let input_path = write_input(&dir, &format!("input.{input_ext}"), input)?;
    let out_pattern = dir.file(&format!("{prefix}%06d.{ext}"));

    let mut args = vec![
        "-y".to_string(),
        "-v".to_string(),
        "error".to_string(),
        "-i".to_string(),
        input_path.to_string_lossy().into_owned(),
    ];
    if let Some(filter) = vf {
        args.push("-vf".to_string());
        args.push(filter);
    }
    if passthrough {
        args.push("-fps_mode".to_string());
        args.push("passthrough".to_string());
    }
    if let Some(n) = start_number {
        args.push("-start_number".to_string());
        args.push(n.to_string());
    }
    args.extend([
        "-an".to_string(),
        "-f".to_string(),
        "image2".to_string(),
        out_pattern.to_string_lossy().into_owned(),
    ]);

    let output = run_tool(FFMPEG, &program, &args, COMMAND_TIMEOUT)?;
    if !output.status.success() {
        return Err(tool_failure(FFMPEG, &output));
    }

    let files = collect_sequence(&out_pattern, prefix, ext, limits)?;
    Ok(MediaOutput { files })
}

/// Extract frames per `spec` into `frames_000001.<ext>`-style named
/// files. `spec.format` selects the image format (`png`, `jpg`,
/// `jpeg`, `webp`, `bmp` — anything else is [`VideoError::InvalidInput`]).
///
/// Exactly one selector must be set on `spec`: `all` (every decoded
/// frame, `-fps_mode passthrough`), `every_secs` (one frame per N
/// seconds via `fps=1/N`), `fps` (one frame per tick via `fps=N`), or
/// `keyframes_only` (`select='eq(pict_type\,I)'` + passthrough). Zero
/// or multiple selectors are [`VideoError::InvalidInput`].
///
/// Enforces `limits.max_input_bytes`/`max_duration_secs` via
/// `check_limits`, `limits.max_pixels` on the probed frame, and
/// `limits.max_outputs` (default cap 10000 frames) both as an early
/// estimate and on the collected files.
pub(crate) fn extract_frames(
    input: &[u8],
    spec: &FramesSpec,
    limits: &EngineLimits,
) -> VideoResult<MediaOutput> {
    let input_len = check_input(input)?;
    let info = probe(input)?;
    check_limits(&info, input_len, limits, true)?;

    let ext = checked_ext(&spec.format, FRAME_FORMATS, "extract_frames")?;

    let selectors = u8::from(spec.all)
        + u8::from(spec.every_secs.is_some())
        + u8::from(spec.fps.is_some())
        + u8::from(spec.keyframes_only);
    if selectors == 0 {
        return Err(VideoError::InvalidInput(
            "FramesSpec selects no frames: set exactly one of all, every_secs, \
             fps, or keyframes_only"
                .to_string(),
        ));
    }
    if selectors > 1 {
        return Err(VideoError::InvalidInput(
            "FramesSpec selectors are mutually exclusive: set exactly one of \
             all, every_secs, fps, or keyframes_only"
                .to_string(),
        ));
    }

    if spec.all {
        return run_sequence(
            input, input_len, &info, limits, &ext, None, true, None, "frames_", info.fps,
        );
    }

    if spec.keyframes_only {
        // Backslash-escaped comma is ffmpeg filter syntax (not shell);
        // passthrough keeps the sparse I-frame cadence instead of
        // duplicating frames to fill CFR gaps.
        return run_sequence(
            input,
            input_len,
            &info,
            limits,
            &ext,
            Some("select='eq(pict_type\\,I)'".to_string()),
            true,
            None,
            "frames_",
            None,
        );
    }

    // Interval/fps selectors resample through the `fps` filter; the
    // muxer keeps its default timing mode so the filter's CFR output
    // is emitted once per tick.
    let out_fps = if let Some(fps) = spec.fps {
        if !fps.is_finite() || fps <= 0.0 {
            return Err(VideoError::InvalidInput(format!(
                "FramesSpec.fps must be a finite positive number, got {fps}"
            )));
        }
        fps
    } else {
        let secs = spec.every_secs.expect("selector count checked above");
        if !secs.is_finite() || secs <= 0.0 {
            return Err(VideoError::InvalidInput(format!(
                "FramesSpec.every_secs must be a finite positive number, got {secs}"
            )));
        }
        1.0 / secs
    };
    if out_fps <= 0.0 || !out_fps.is_finite() || format!("{out_fps:.9}") == "0.000000000" {
        return Err(VideoError::InvalidInput(format!(
            "frame rate {out_fps} is too small to express for ffmpeg"
        )));
    }

    run_sequence(
        input,
        input_len,
        &info,
        limits,
        &ext,
        Some(format!("fps={}", fmt_f64(out_fps))),
        false,
        None,
        "frames_",
        Some(out_fps),
    )
}

/// Extract every frame as a numbered image sequence
/// (`frame_000000.png`, `frame_000001.png`, ...) via
/// `-fps_mode passthrough -start_number 0` so VFR inputs keep their
/// exact frame count and numbering starts at zero.
///
/// `format` is validated against the same allowlist as
/// [`extract_frames`]. Same guards: `limits.max_input_bytes`,
/// `max_duration_secs`, `max_pixels`, and `max_outputs`
/// (default cap 10000 frames).
pub(crate) fn video_to_image_sequence(
    input: &[u8],
    format: &str,
    limits: &EngineLimits,
) -> VideoResult<MediaOutput> {
    let input_len = check_input(input)?;
    let info = probe(input)?;
    check_limits(&info, input_len, limits, true)?;

    let ext = checked_ext(format, FRAME_FORMATS, "video_to_image_sequence")?;

    run_sequence(
        input,
        input_len,
        &info,
        limits,
        &ext,
        None,
        true,
        Some(0),
        "frame_",
        info.fps,
    )
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::crates::video_engine::ops::{FFPROBE, tool_available};

    fn tools_available() -> bool {
        tool_available(FFMPEG) && tool_available(FFPROBE)
    }

    /// Build a 2s 320x240@30fps clip (~60 frames). Video-only: frame
    /// extraction needs no audio track and no H.264 encoder (the
    /// default mpeg4 encoder always exists).
    fn make_fixture() -> Option<Vec<u8>> {
        let program = find_tool(FFMPEG).ok()?;
        let dir = TempDir::new("frames-test").ok()?;
        let out = dir.file("fixture.mp4");
        let args = vec![
            "-y".to_string(),
            "-v".to_string(),
            "error".to_string(),
            "-f".to_string(),
            "lavfi".to_string(),
            "-i".to_string(),
            "testsrc=duration=2:size=320x240:rate=30".to_string(),
            "-pix_fmt".to_string(),
            "yuv420p".to_string(),
            "-an".to_string(),
            out.to_string_lossy().into_owned(),
        ];
        let output = run_tool(FFMPEG, &program, &args, COMMAND_TIMEOUT).ok()?;
        if !output.status.success() {
            return None;
        }
        fs::read(out).ok()
    }

    fn spec_fps(fps: f64, format: &str) -> FramesSpec {
        FramesSpec {
            all: false,
            every_secs: None,
            fps: Some(fps),
            keyframes_only: false,
            format: format.to_string(),
        }
    }

    const PNG_MAGIC: &[u8; 8] = b"\x89PNG\r\n\x1a\n";

    #[test]
    fn fps_selector_produces_expected_count() {
        if !tools_available() {
            return;
        }
        let Some(fixture) = make_fixture() else {
            return;
        };
        let out =
            extract_frames(&fixture, &spec_fps(5.0, "png"), &EngineLimits::default()).unwrap();
        // 2s @ 5fps → ~10 frames (container rounding tolerated).
        assert!(
            (8..=12).contains(&out.files.len()),
            "expected ~10 frames, got {}",
            out.files.len()
        );
        assert!(out.files[0].name.starts_with("frames_"));
        assert!(out.files[0].name.ends_with(".png"));
        assert_eq!(&out.files[0].bytes[..8], PNG_MAGIC, "not a PNG");
        // Names are zero-padded and sorted.
        assert!(out.files.windows(2).all(|w| w[0].name < w[1].name));
    }

    #[test]
    fn every_secs_selector_produces_sparse_frames() {
        if !tools_available() {
            return;
        }
        let Some(fixture) = make_fixture() else {
            return;
        };
        let spec = FramesSpec {
            all: false,
            every_secs: Some(1.0),
            fps: None,
            keyframes_only: false,
            format: "jpg".to_string(),
        };
        let out = extract_frames(&fixture, &spec, &EngineLimits::default()).unwrap();
        // 2s @ 1fps → ~2-3 frames.
        assert!(
            (2..=4).contains(&out.files.len()),
            "expected ~2-3 frames, got {}",
            out.files.len()
        );
        assert!(out.files[0].name.ends_with(".jpg"));
        // JPEG SOI marker.
        assert_eq!(&out.files[0].bytes[..2], b"\xff\xd8");
    }

    #[test]
    fn keyframes_only_produces_at_least_one() {
        if !tools_available() {
            return;
        }
        let Some(fixture) = make_fixture() else {
            return;
        };
        let spec = FramesSpec {
            all: false,
            every_secs: None,
            fps: None,
            keyframes_only: true,
            format: "png".to_string(),
        };
        let out = extract_frames(&fixture, &spec, &EngineLimits::default()).unwrap();
        assert!(!out.files.is_empty(), "no keyframes extracted");
        assert_eq!(&out.files[0].bytes[..8], PNG_MAGIC);
    }

    #[test]
    fn all_selector_matches_source_frame_count() {
        if !tools_available() {
            return;
        }
        let Some(fixture) = make_fixture() else {
            return;
        };
        let spec = FramesSpec {
            all: true,
            every_secs: None,
            fps: None,
            keyframes_only: false,
            format: "png".to_string(),
        };
        let out = extract_frames(&fixture, &spec, &EngineLimits::default()).unwrap();
        // 2s @ 30fps → ~60 frames.
        assert!(
            (55..=65).contains(&out.files.len()),
            "expected ~60 frames, got {}",
            out.files.len()
        );
    }

    #[test]
    fn image_sequence_numbers_from_zero() {
        if !tools_available() {
            return;
        }
        let Some(fixture) = make_fixture() else {
            return;
        };
        let out = video_to_image_sequence(&fixture, "png", &EngineLimits::default()).unwrap();
        assert!(
            (55..=65).contains(&out.files.len()),
            "expected ~60 frames, got {}",
            out.files.len()
        );
        assert_eq!(out.files[0].name, "frame_000000.png");
        assert_eq!(&out.files[0].bytes[..8], PNG_MAGIC);
    }

    #[test]
    fn invalid_format_and_selectors_rejected() {
        let fixture = vec![0u8; 16];
        // Bad format is rejected before probing/ffmpeg.
        let bad = FramesSpec {
            all: false,
            every_secs: None,
            fps: Some(1.0),
            keyframes_only: false,
            format: "tiff".to_string(),
        };
        assert!(matches!(
            extract_frames(&fixture, &bad, &EngineLimits::default()),
            Err(VideoError::InvalidInput(_))
        ));
        // No selector set.
        let none = FramesSpec {
            all: false,
            every_secs: None,
            fps: None,
            keyframes_only: false,
            format: "png".to_string(),
        };
        assert!(matches!(
            extract_frames(&fixture, &none, &EngineLimits::default()),
            Err(VideoError::InvalidInput(_))
        ));
        // Two selectors set.
        let both = FramesSpec {
            all: true,
            every_secs: None,
            fps: Some(1.0),
            keyframes_only: false,
            format: "png".to_string(),
        };
        assert!(matches!(
            extract_frames(&fixture, &both, &EngineLimits::default()),
            Err(VideoError::InvalidInput(_))
        ));
    }

    #[test]
    fn max_outputs_caps_sequence() {
        if !tools_available() {
            return;
        }
        let Some(fixture) = make_fixture() else {
            return;
        };
        let limits = EngineLimits {
            max_outputs: Some(5),
            ..EngineLimits::default()
        };
        let err = video_to_image_sequence(&fixture, "png", &limits)
            .err()
            .expect("expected LimitExceeded");
        assert!(
            matches!(err, VideoError::LimitExceeded(_)),
            "expected LimitExceeded, got {err:?}"
        );
    }
}
