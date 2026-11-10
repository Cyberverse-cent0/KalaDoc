//! Contact sheets and scene-aware keyframe extraction.
//!
//! Both operations decode the input once through an ffmpeg filter
//! chain and emit still images:
//!
//! - [`contact_sheet`] resamples the video to `cols*rows` evenly
//!   spaced frames (`fps=frames/duration`, or `fps=1/interval` when
//!   the caller pins an interval), scales each to `thumb_width` px
//!   wide, and tiles them into a single PNG via the `tile` filter.
//! - [`scene_keyframes`] keeps only frames whose `scene` score
//!   exceeds `threshold` (`select='gt(scene\,T)'`) and writes them
//!   as a numbered `scene_NNNNNN.png` sequence.
//!
//! `tile`'s `layout` option requires a concrete `WxH` grid — ffmpeg
//! 7.x rejects `tile=10x-1`-style "auto rows" (`Unable to parse
//! option value ... as image size`), so callers always pass an
//! explicit grid.

use std::fs;
use std::path::Path;

use crate::crates::core::engine::EngineLimits;
use crate::crates::video_engine::error::{VideoError, VideoResult};
use crate::crates::video_engine::model::{MediaFile, MediaOutput};
use crate::crates::video_engine::ops::probe::probe;
use crate::crates::video_engine::ops::{
    COMMAND_TIMEOUT, FFMPEG, TempDir, check_input, check_limits, find_tool, read_output, run_tool,
    tool_failure, write_input,
};

/// PNG signature — verified on produced bytes so a silent ffmpeg
/// failure cannot return garbage.
const PNG_MAGIC: &[u8; 8] = b"\x89PNG\r\n\x1a\n";

/// Sanity bounds for the contact-sheet grid and thumbnail width.
/// `cols`/`rows` beyond 20 produce sheets nobody can read; thumb
/// widths outside 16–4096 are almost certainly caller bugs.
const MAX_GRID_DIM: u32 = 20;
const MIN_THUMB_WIDTH: u32 = 16;
const MAX_THUMB_WIDTH: u32 = 4096;

/// Shared prologue: validate the buffer, resolve ffmpeg, probe the
/// media and enforce `limits`. Returns `(ffmpeg path, probed info,
/// input length, scratch dir)` — the caller writes `input` into `dir`
/// itself because the two ops use different scratch filenames.
fn prepare(
    input: &[u8],
    limits: &EngineLimits,
    tag: &str,
) -> VideoResult<(
    std::path::PathBuf,
    crate::crates::video_engine::model::VideoInfo,
    u64,
    TempDir,
)> {
    let input_len = check_input(input)?;
    let program = find_tool(FFMPEG)?;
    let info = probe(input)?;
    // Sheet/keyframe outputs are small thumbnails; the decompression-
    // bomb pixel guard is not meaningful here (pixels=false).
    check_limits(&info, input_len, limits, false)?;
    if !info.has_video {
        return Err(VideoError::InvalidInput(
            "input has no video stream; cannot build frames".to_string(),
        ));
    }
    let dir = TempDir::new(tag)?;
    Ok((program, info, input_len, dir))
}

/// Best available duration: container `format.duration`, falling
/// back to the primary video stream's own `duration`. `None` when
/// neither is reported — callers that need even sampling must treat
/// that as [`VideoError::InvalidInput`].
fn media_duration(info: &crate::crates::video_engine::model::VideoInfo) -> Option<f64> {
    info.duration_secs
        .or_else(|| {
            info.streams
                .iter()
                .find(|s| s.codec_type == "video" && !s.attached_pic)
                .and_then(|s| s.duration_secs)
        })
        .filter(|d| d.is_finite() && *d > 0.0)
}

/// Validate the shared sheet geometry arguments.
fn check_sheet_args(cols: u32, rows: u32, thumb_width: u32, op: &str) -> VideoResult<()> {
    if cols == 0 || cols > MAX_GRID_DIM || rows == 0 || rows > MAX_GRID_DIM {
        return Err(VideoError::InvalidInput(format!(
            "{op} grid must be 1..={MAX_GRID_DIM} in each dimension, got {cols}x{rows}"
        )));
    }
    if !(MIN_THUMB_WIDTH..=MAX_THUMB_WIDTH).contains(&thumb_width) {
        return Err(VideoError::InvalidInput(format!(
            "{op} thumb_width must be {MIN_THUMB_WIDTH}..={MAX_THUMB_WIDTH}, got {thumb_width}"
        )));
    }
    Ok(())
}

/// Format an ffmpeg frame rate. Rust's `Display` for f64 emits the
/// shortest round-trip decimal, which `av_parse_video_rate` accepts
/// (`-r 29.97`-style values are the same code path).
fn fps_arg(rate: f64) -> String {
    format!("{rate}")
}

/// Run ffmpeg producing a single PNG still; non-zero exit →
/// [`tool_failure`], missing/invalid PNG magic → [`VideoError::ExternalTool`].
fn run_png(
    program: &std::path::Path,
    input_path: &Path,
    filter: &str,
    out_path: &Path,
) -> VideoResult<Vec<u8>> {
    let args = vec![
        "-y".to_string(),
        "-hide_banner".to_string(),
        "-v".to_string(),
        "error".to_string(),
        "-i".to_string(),
        input_path.to_string_lossy().into_owned(),
        "-vf".to_string(),
        filter.to_string(),
        "-frames:v".to_string(),
        "1".to_string(),
        "-f".to_string(),
        "image2".to_string(),
        out_path.to_string_lossy().into_owned(),
    ];
    let output = run_tool(FFMPEG, program, &args, COMMAND_TIMEOUT)?;
    if !output.status.success() {
        return Err(tool_failure(FFMPEG, &output));
    }
    let png = read_output(out_path)?;
    if png.len() < PNG_MAGIC.len() || &png[..PNG_MAGIC.len()] != PNG_MAGIC {
        return Err(VideoError::ExternalTool {
            tool: FFMPEG.to_string(),
            detail: "no video frame produced (input may have no video stream)".to_string(),
        });
    }
    Ok(png)
}

/// Build a single PNG contact sheet: sample `cols*rows` frames evenly
/// across the probed duration (or every `interval_secs` when given),
/// scale each to `thumb_width` px wide preserving aspect
/// (`scale={tw}:-2` keeps height even for yuv420p-friendly output),
/// and tile into a `cols`×`rows` grid — a single PNG.
///
/// Grid cells with no sampled frame (short video, large interval)
/// stay black; the `tile` filter pads the tail of the grid.
pub(crate) fn contact_sheet(
    input: &[u8],
    cols: u32,
    rows: u32,
    thumb_width: u32,
    interval_secs: Option<f64>,
    limits: &EngineLimits,
) -> VideoResult<Vec<u8>> {
    check_sheet_args(cols, rows, thumb_width, "contact_sheet")?;
    if let Some(t) = interval_secs {
        if !t.is_finite() || t <= 0.0 {
            return Err(VideoError::InvalidInput(format!(
                "interval_secs must be a finite positive number, got {t}"
            )));
        }
    }
    let (program, info, _input_len, dir) = prepare(input, limits, "sheet")?;
    let input_path = write_input(&dir, "input.bin", input)?;

    // Sampling rate: one frame per `interval_secs`, else spread
    // cols*rows frames evenly over the duration via fps=frames/dur.
    let rate = match interval_secs {
        Some(t) => 1.0 / t,
        None => {
            let dur = media_duration(&info).ok_or_else(|| {
                VideoError::InvalidInput(
                    "input reports no duration; cannot sample frames evenly".to_string(),
                )
            })?;
            (cols as f64 * rows as f64) / dur
        }
    };
    if !rate.is_finite() || rate <= 0.0 {
        return Err(VideoError::InvalidInput(format!(
            "computed sample rate {rate} is not usable"
        )));
    }

    let filter = format!(
        "fps={},scale={thumb_width}:-2,tile={cols}x{rows}",
        fps_arg(rate)
    );
    let out_path = dir.file("sheet.png");
    run_png(&program, &input_path, &filter, &out_path)
}

/// Extract one PNG frame at each detected scene change
/// (`select='gt(scene\,threshold)'`), named `scene_000001.png`,
/// `scene_000002.png`, ... in presentation order.
///
/// The frame at `pts=0` is *not* a scene change and is excluded —
/// a video with no cuts yields an empty [`MediaOutput`], never an
/// error. `limits.max_outputs` caps the produced file count.
pub(crate) fn scene_keyframes(
    input: &[u8],
    threshold: f64,
    limits: &EngineLimits,
) -> VideoResult<MediaOutput> {
    if !threshold.is_finite() || threshold < 0.0 {
        return Err(VideoError::InvalidInput(format!(
            "scene threshold must be a finite non-negative number, got {threshold}"
        )));
    }
    let (program, _info, _input_len, dir) = prepare(input, limits, "scenes")?;
    let input_path = write_input(&dir, "input.bin", input)?;

    // `select` keeps only frames whose scene score beats the
    // threshold; `fps_mode passthrough` prevents the muxer from
    // duplicating frames to fill CFR gaps between scene hits.
    let filter = format!("select='gt(scene\\,{threshold})'");
    let pattern = dir.file("scene_%06d.png");
    let args = vec![
        "-y".to_string(),
        "-hide_banner".to_string(),
        "-v".to_string(),
        "error".to_string(),
        "-i".to_string(),
        input_path.to_string_lossy().into_owned(),
        "-vf".to_string(),
        filter,
        "-fps_mode".to_string(),
        "passthrough".to_string(),
        "-start_number".to_string(),
        "1".to_string(),
        "-f".to_string(),
        "image2".to_string(),
        pattern.to_string_lossy().into_owned(),
    ];
    let output = run_tool(FFMPEG, &program, &args, COMMAND_TIMEOUT)?;
    if !output.status.success() {
        return Err(tool_failure(FFMPEG, &output));
    }

    // `TempDir.path` is module-private but visible to this child
    // module of `ops`.
    let mut names: Vec<String> = fs::read_dir(&dir.path)
        .map_err(VideoError::Io)?
        .filter_map(|e| e.ok())
        .filter_map(|e| {
            let name = e.file_name().to_string_lossy().into_owned();
            (name.starts_with("scene_") && name.ends_with(".png")).then_some(name)
        })
        .collect();
    names.sort();
    if let Some(max) = limits.max_outputs {
        if names.len() as u64 > max as u64 {
            return Err(VideoError::LimitExceeded(format!(
                "scene detection produced {} frames, exceeding max_outputs {max}",
                names.len()
            )));
        }
    }

    let mut files = Vec::with_capacity(names.len());
    for name in names {
        let bytes = read_output(&dir.file(&name))?;
        files.push(MediaFile { name, bytes });
    }
    Ok(MediaOutput { files })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::crates::video_engine::ops::{FFPROBE, find_tool};

    fn tools_available() -> bool {
        find_tool(FFMPEG).is_ok() && find_tool(FFPROBE).is_ok()
    }

    /// Encode a fixture with ffmpeg's native `mpeg4` encoder (no
    /// libx264 dependency): 3s of 160x120 testsrc in an AVI.
    fn video_fixture() -> Option<Vec<u8>> {
        if !tools_available() {
            return None;
        }
        let dir = TempDir::new("contact-sheet-test").ok()?;
        let out = dir.file("fixture.avi");
        let program = find_tool(FFMPEG).ok()?;
        let args = vec![
            "-y".to_string(),
            "-v".to_string(),
            "error".to_string(),
            "-f".to_string(),
            "lavfi".to_string(),
            "-i".to_string(),
            "testsrc=size=160x120:rate=10:duration=3".to_string(),
            "-c:v".to_string(),
            "mpeg4".to_string(),
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

    /// Two-shot fixture: testsrc → testsrc2 hard cut at ~1.5s.
    fn scene_fixture() -> Option<Vec<u8>> {
        if !tools_available() {
            return None;
        }
        let dir = TempDir::new("scene-test").ok()?;
        let out = dir.file("scene.avi");
        let program = find_tool(FFMPEG).ok()?;
        let args = vec![
            "-y".to_string(),
            "-v".to_string(),
            "error".to_string(),
            "-f".to_string(),
            "lavfi".to_string(),
            "-i".to_string(),
            "testsrc=duration=1.5:size=160x120:rate=10".to_string(),
            "-f".to_string(),
            "lavfi".to_string(),
            "-i".to_string(),
            "testsrc2=duration=1.5:size=160x120:rate=10".to_string(),
            "-filter_complex".to_string(),
            "[0:v][1:v]concat=n=2:v=1[v]".to_string(),
            "-map".to_string(),
            "[v]".to_string(),
            "-c:v".to_string(),
            "mpeg4".to_string(),
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

    /// Read PNG IHDR dimensions (bytes 16..24, big-endian u32s).
    fn png_dims(png: &[u8]) -> (u32, u32) {
        assert!(png.len() > 24 && &png[..8] == PNG_MAGIC, "not a PNG");
        let w = u32::from_be_bytes([png[16], png[17], png[18], png[19]]);
        let h = u32::from_be_bytes([png[20], png[21], png[22], png[23]]);
        (w, h)
    }

    #[test]
    fn contact_sheet_tiles_grid() {
        let Some(fx) = video_fixture() else { return };
        let png = contact_sheet(&fx, 3, 2, 80, None, &EngineLimits::default())
            .expect("contact_sheet failed");
        // 160x120 → scale=80:-2 → 80x60 cells; tile 3x2 → 240x120.
        let (w, h) = png_dims(&png);
        assert_eq!((w, h), (240, 120), "sheet dims {w}x{h}");
    }

    #[test]
    fn contact_sheet_interval_sampling() {
        let Some(fx) = video_fixture() else { return };
        // interval 1s over 3s → ~3 frames into a 2x2 grid; tile still
        // emits one 160x120-ish PNG (cells padded black).
        let png = contact_sheet(&fx, 2, 2, 80, Some(1.0), &EngineLimits::default())
            .expect("contact_sheet interval failed");
        let (w, h) = png_dims(&png);
        assert_eq!((w, h), (160, 120), "sheet dims {w}x{h}");
    }

    #[test]
    fn contact_sheet_rejects_bad_args() {
        let Some(fx) = video_fixture() else { return };
        let limits = EngineLimits::default();
        assert!(matches!(
            contact_sheet(&fx, 0, 2, 80, None, &limits),
            Err(VideoError::InvalidInput(_))
        ));
        assert!(matches!(
            contact_sheet(&fx, 3, 2, 4, None, &limits),
            Err(VideoError::InvalidInput(_))
        ));
        assert!(matches!(
            contact_sheet(&fx, 3, 2, 80, Some(-1.0), &limits),
            Err(VideoError::InvalidInput(_))
        ));
        assert!(matches!(
            contact_sheet(b"not media", 3, 2, 80, None, &limits),
            Err(VideoError::InvalidInput(_))
        ));
    }

    #[test]
    fn scene_keyframes_finds_cut() {
        let Some(fx) = scene_fixture() else { return };
        let out =
            scene_keyframes(&fx, 0.3, &EngineLimits::default()).expect("scene_keyframes failed");
        assert!(!out.files.is_empty(), "expected >=1 scene frame");
        for f in &out.files {
            assert!(f.name.starts_with("scene_") && f.name.ends_with(".png"));
            assert!(f.bytes.len() > PNG_MAGIC.len());
            assert_eq!(&f.bytes[..8], PNG_MAGIC, "{} not a PNG", f.name);
        }
    }

    #[test]
    fn scene_keyframes_no_cut_is_empty_not_error() {
        let Some(fx) = video_fixture() else { return };
        // testsrc has no hard cuts; a high threshold must yield zero
        // files rather than an error.
        let out =
            scene_keyframes(&fx, 0.99, &EngineLimits::default()).expect("scene_keyframes failed");
        assert!(out.files.is_empty());
    }

    #[test]
    fn scene_keyframes_rejects_bad_threshold() {
        let Some(fx) = video_fixture() else { return };
        assert!(matches!(
            scene_keyframes(&fx, -0.5, &EngineLimits::default()),
            Err(VideoError::InvalidInput(_))
        ));
        assert!(matches!(
            scene_keyframes(&fx, f64::NAN, &EngineLimits::default()),
            Err(VideoError::InvalidInput(_))
        ));
    }
}
