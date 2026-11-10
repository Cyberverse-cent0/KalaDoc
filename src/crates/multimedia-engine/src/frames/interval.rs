//! Frame extraction at intervals, fixed frame rates, or on scene
//! changes.
//!
//! [`frames_interval`] writes the input buffer to a temp file, runs
//! `ffmpeg` with an `image2` sequence muxer (`<prefix>_%04d.<ext>`),
//! and reads every produced frame back under
//! [`MediaLimits::max_output_bytes`] / `max_output_files` and
//! [`IntervalOptions::max_frames`]. The first frame becomes
//! [`MediaOutput::primary`]; the rest land in `extra`.
//!
//! Schedule mapping:
//!
//! - [`FrameSchedule::EverySecs`] — `-vf fps=1/interval` (one frame
//!   per `interval` seconds of source time).
//! - [`FrameSchedule::Fps`] — `-vf fps=N` (resample to N fps).
//! - [`FrameSchedule::SceneChanges`] — `-vf select=gt(scene,0.4)`
//!   with VFR output, keeping only frames whose scene score exceeds
//!   the threshold.

use std::ffi::OsString;

use serde::{Deserialize, Serialize};

use crate::crates::multimedia_engine::error::{MediaError, MediaResult};
use crate::crates::multimedia_engine::model::{MediaFile, MediaLimits, MediaOutput};
use crate::crates::multimedia_engine::runtime::discovery::Runtime;
use crate::crates::multimedia_engine::runtime::process;
use crate::crates::multimedia_engine::runtime::tempdir::TempDir;

/// Scene-score threshold for [`FrameSchedule::SceneChanges`].
const SCENE_THRESHOLD: f64 = 0.4;

/// How frames are selected from the source video.
#[derive(Debug, Clone, Copy, PartialEq, Serialize, Deserialize)]
pub enum FrameSchedule {
    /// One frame every `interval` seconds of source time
    /// (e.g. `EverySecs(0.5)` → a frame twice per second).
    EverySecs(f64),
    /// Resample to `fps` frames per second.
    Fps(f64),
    /// Keep only frames whose scene-change score exceeds a fixed
    /// threshold — useful for thumbnailing highlights.
    SceneChanges,
}

/// Options for [`frames_interval`].
#[derive(Debug, Clone)]
pub struct IntervalOptions {
    /// Frame selection schedule.
    pub schedule: FrameSchedule,
    /// Image format / encoder token, without a leading dot
    /// (e.g. `"png"`, `"jpg"`, `"webp"`, `"bmp"`). Also used as the
    /// file extension unless remapped (see [`image_codec`]).
    /// Defaults to `"png"`.
    pub output_format: String,
    /// File-name prefix for the image2 pattern
    /// (`<prefix>_%04d.<ext>`). Defaults to `"frame"`.
    pub prefix: String,
    /// Optional output width in pixels (`-vf scale`). `None` keeps the
    /// source width; combine with `height: None` for height-only.
    pub width: Option<u32>,
    /// Optional output height in pixels.
    pub height: Option<u32>,
    /// Only extract frames at or after this many seconds into the
    /// input (`-ss`, input seeking).
    pub start_secs: Option<f64>,
    /// Only extract frames before this many seconds into the input.
    /// With `start_secs` set, the range is `[start, end)`; otherwise
    /// it is `[0, end)`.
    pub end_secs: Option<f64>,
    /// Maximum number of frames to produce. Exceeding it — like
    /// exceeding `MediaLimits::max_output_files` — is a
    /// [`MediaError::LimitExceeded`], not a silent truncation.
    pub max_frames: Option<u32>,
}

impl Default for IntervalOptions {
    fn default() -> Self {
        Self {
            schedule: FrameSchedule::EverySecs(1.0),
            output_format: "png".to_string(),
            prefix: "frame".to_string(),
            width: None,
            height: None,
            start_secs: None,
            end_secs: None,
            max_frames: None,
        }
    }
}

/// Extract frames from `input` according to `options`.
///
/// Runs `ffmpeg -hide_banner -loglevel error -nostdin -y` with an
/// `image2` muxer writing `<prefix>_%04d.<ext>` into a temp dir, then
/// collects the files in numeric order. The first frame is the
/// primary output.
///
/// # Errors
///
/// - [`MediaError::InvalidInput`] — empty/oversized input, non-positive
///   or non-finite schedule/range values, unsafe `prefix` /
///   `output_format`, zero `width`/`height`, `end <= start`.
/// - [`MediaError::Unsupported`] — the requested image encoder is not
///   in this FFmpeg build's capability set.
/// - [`MediaError::LimitExceeded`] — produced frames exceed
///   `options.max_frames` or `limits.max_output_files`, or total
///   output bytes exceed `limits.max_output_bytes`.
/// - [`MediaError::ToolFailed`] — ffmpeg failed or produced no frames.
pub fn frames_interval(
    input: &[u8],
    options: &IntervalOptions,
    limits: &MediaLimits,
    rt: &Runtime,
) -> MediaResult<MediaOutput> {
    if input.is_empty() {
        return Err(MediaError::InvalidInput("empty input buffer".to_string()));
    }
    if let Some(max) = limits.max_input_bytes
        && (input.len() as u64) > max
    {
        return Err(MediaError::LimitExceeded(format!(
            "input is {} bytes, exceeding max_input_bytes {max}",
            input.len()
        )));
    }

    // --- validate schedule -------------------------------------------------
    match options.schedule {
        FrameSchedule::EverySecs(interval) => {
            if !interval.is_finite() || interval <= 0.0 {
                return Err(MediaError::InvalidInput(format!(
                    "EverySecs interval must be a positive finite number, got {interval}"
                )));
            }
        }
        FrameSchedule::Fps(fps) => {
            if !fps.is_finite() || fps <= 0.0 {
                return Err(MediaError::InvalidInput(format!(
                    "Fps must be a positive finite number, got {fps}"
                )));
            }
        }
        FrameSchedule::SceneChanges => {}
    }

    // --- validate time range ------------------------------------------------
    let start = options.start_secs.unwrap_or(0.0);
    if let Some(s) = options.start_secs
        && (!s.is_finite() || s < 0.0)
    {
        return Err(MediaError::InvalidInput(format!(
            "start_secs must be a non-negative finite number, got {s}"
        )));
    }
    if let Some(e) = options.end_secs {
        if !e.is_finite() || e < 0.0 {
            return Err(MediaError::InvalidInput(format!(
                "end_secs must be a non-negative finite number, got {e}"
            )));
        }
        if e <= start {
            return Err(MediaError::InvalidInput(format!(
                "end_secs ({e}) must be greater than start_secs ({start})"
            )));
        }
    }

    // --- validate geometry ---------------------------------------------------
    for (name, dim) in [("width", options.width), ("height", options.height)] {
        if let Some(d) = dim
            && d == 0
        {
            return Err(MediaError::InvalidInput(format!(
                "{name} must be greater than 0"
            )));
        }
    }

    // --- sanitize names used in the output pattern ---------------------------
    let prefix = sanitize_name_part(options.prefix.trim(), "prefix")?;
    let format = sanitize_name_part(
        options.output_format.trim().trim_start_matches('.'),
        "output_format",
    )?;
    let (codec, ext) = image_codec(&format, rt)?;

    let dir = TempDir::new("frames-interval")?;
    let input_name = "input.bin";
    dir.write(input_name, input)?;

    // Image2 pattern; `%04d` zero-pads and still expands past 9999.
    let pattern_name = format!("{prefix}_%04d.{ext}");
    let pattern_path = dir.file(&pattern_name);

    // Frame-count bound: request one more than allowed so exceeding
    // the cap is detectable after collection.
    let frame_cap = match (options.max_frames, limits.max_output_files) {
        (None, None) => None,
        (a, b) => {
            let allowed = [a.map(u64::from), b.map(u64::from)]
                .into_iter()
                .flatten()
                .min()
                .expect("at least one cap is set");
            Some(allowed.saturating_add(1))
        }
    };

    // --- build the filter chain ----------------------------------------------
    let mut filters: Vec<String> = Vec::new();
    match (options.width, options.height) {
        (Some(w), Some(h)) => filters.push(format!("scale={w}:{h}")),
        (Some(w), None) => filters.push(format!("scale={w}:-2")),
        (None, Some(h)) => filters.push(format!("scale=-2:{h}")),
        (None, None) => {}
    }
    match options.schedule {
        FrameSchedule::EverySecs(interval) => {
            filters.push(format!("fps={}", format_fps(1.0 / interval)));
        }
        FrameSchedule::Fps(fps) => {
            filters.push(format!("fps={}", format_fps(fps)));
        }
        FrameSchedule::SceneChanges => {
            filters.push(format!("select=gt(scene,{SCENE_THRESHOLD})"));
        }
    }
    let filter_chain = filters.join(",");

    // --- argv -----------------------------------------------------------------
    let mut argv: Vec<OsString> = vec![
        OsString::from("-hide_banner"),
        OsString::from("-loglevel"),
        OsString::from("error"),
        OsString::from("-nostdin"),
        OsString::from("-y"),
    ];
    if start > 0.0 {
        argv.push(OsString::from("-ss"));
        argv.push(OsString::from(format_secs(start)));
    }
    argv.push(OsString::from("-i"));
    argv.push(dir.file(input_name).into_os_string());

    // Output-side options.
    argv.push(OsString::from("-map"));
    argv.push(OsString::from("0:v:0"));
    if !filter_chain.is_empty() {
        argv.push(OsString::from("-vf"));
        argv.push(OsString::from(filter_chain));
    }
    if matches!(options.schedule, FrameSchedule::SceneChanges) {
        // `select` produces variable-frame-rate output; without this
        // ffmpeg warns about non-monotonic DTS and may duplicate/drop.
        argv.push(OsString::from("-fps_mode"));
        argv.push(OsString::from("vfr"));
    }
    if let Some(e) = options.end_secs {
        let duration = e - start;
        argv.push(OsString::from("-t"));
        argv.push(OsString::from(format_secs(duration)));
    }
    if let Some(cap) = frame_cap {
        argv.push(OsString::from("-frames:v"));
        argv.push(OsString::from(cap.to_string()));
    }
    argv.push(OsString::from("-an"));
    argv.push(OsString::from("-c:v"));
    argv.push(OsString::from(codec));
    argv.push(OsString::from("-f"));
    argv.push(OsString::from("image2"));
    argv.push(pattern_path.into_os_string());

    let out = process::run_checked("ffmpeg", &rt.ffmpeg, &argv, None, limits)?;

    // --- collect produced frames ----------------------------------------------
    let files = collect_frames(dir.path(), &prefix, &ext)?;
    if files.is_empty() {
        return Err(MediaError::ToolFailed {
            tool: "ffmpeg",
            status: "no frames produced".to_string(),
            detail: format!(
                "ffmpeg produced no {pattern_name} files{}",
                if out.stderr.is_empty() {
                    String::new()
                } else {
                    format!(": {}", out.stderr)
                }
            ),
        });
    }

    // Enforce the frame-count caps explicitly (the -frames:v bound is
    // allowed+1, so count > allowed means the cap was exceeded).
    let allowed_frames = match (options.max_frames, limits.max_output_files) {
        (None, None) => u64::MAX,
        (a, b) => [a.map(u64::from), b.map(u64::from)]
            .into_iter()
            .flatten()
            .min()
            .expect("at least one cap is set"),
    };
    if (files.len() as u64) > allowed_frames {
        return Err(MediaError::LimitExceeded(format!(
            "frame extraction produced {} frames, exceeding the cap of {allowed_frames}",
            files.len()
        )));
    }

    // Read frames back under the byte cap; primary = first frame.
    let mut media_files: Vec<MediaFile> = Vec::with_capacity(files.len());
    let mut total_bytes: u64 = 0;
    for name in &files {
        let remaining = limits
            .max_output_bytes
            .map(|max| max.saturating_sub(total_bytes));
        let bytes = match remaining {
            Some(0) => {
                return Err(MediaError::LimitExceeded(format!(
                    "frame outputs exceed max_output_bytes {}",
                    limits.max_output_bytes.unwrap_or_default()
                )));
            }
            budget => dir.read_limited(name, budget)?,
        };
        total_bytes = total_bytes.saturating_add(bytes.len() as u64);
        media_files.push(MediaFile {
            name: name.clone(),
            bytes,
            content_type: content_type_for(&ext),
        });
    }

    let mut iter = media_files.into_iter();
    let primary = iter.next();
    let mut result = MediaOutput {
        primary,
        extra: iter.collect(),
        warnings: Vec::new(),
    };
    if !out.stderr.is_empty() {
        result.warnings.push(out.stderr);
    }
    Ok(result)
}

/// List `<prefix>_*.ext` files in `dir`, sorted by the numeric
/// sequence component (so `frame_10000.png` sorts after
/// `frame_9999.png`).
fn collect_frames(dir: &std::path::Path, prefix: &str, ext: &str) -> MediaResult<Vec<String>> {
    let suffix = format!(".{ext}");
    let mut found: Vec<(u64, String)> = Vec::new();
    for entry in std::fs::read_dir(dir)? {
        let entry = entry?;
        let name = entry.file_name().to_string_lossy().into_owned();
        let Some(rest) = name
            .strip_prefix(&format!("{prefix}_"))
            .and_then(|r| r.strip_suffix(&suffix))
        else {
            continue;
        };
        if entry.file_type()?.is_file()
            && let Ok(num) = rest.parse::<u64>()
        {
            found.push((num, name));
        }
    }
    found.sort_by_key(|(num, _)| *num);
    Ok(found.into_iter().map(|(_, name)| name).collect())
}

/// Map an `output_format` token to `(ffmpeg encoder, file extension)`.
///
/// Common image formats are aliased to their real encoder names
/// (`jpg` → `mjpeg`, `webp` → `libwebp`); anything else must itself
/// be an encoder in this FFmpeg build or we return
/// [`MediaError::Unsupported`] — never a silent fallback.
fn image_codec(format: &str, rt: &Runtime) -> MediaResult<(String, String)> {
    let lower = format.to_ascii_lowercase();
    let (codec, ext) = match lower.as_str() {
        "png" => ("png", "png"),
        "jpg" | "jpeg" => ("mjpeg", "jpg"),
        "webp" => ("libwebp", "webp"),
        "bmp" => ("bmp", "bmp"),
        "tif" | "tiff" => ("tiff", "tiff"),
        other => (other, other),
    };
    let codec = codec.to_string();
    let ext = ext.to_string();
    if !rt.capabilities()?.has_encoder(&codec) {
        return Err(MediaError::Unsupported(format!(
            "image encoder '{codec}' (from output_format '{format}') is not available in this FFmpeg build"
        )));
    }
    Ok((codec, ext))
}

/// Sanitize a user-supplied token used inside the image2 output
/// pattern. Allows ASCII alphanumerics plus `-`, `_`, `.`; rejects
/// path separators, `%` (image2 pattern escape), and `.`/`..`.
fn sanitize_name_part(value: &str, field: &str) -> MediaResult<String> {
    if value.is_empty() {
        return Err(MediaError::InvalidInput(format!(
            "{field} must not be empty"
        )));
    }
    if value == "." || value == ".." {
        return Err(MediaError::InvalidInput(format!(
            "{field} '{value}' is not a valid file-name component"
        )));
    }
    if !value
        .chars()
        .all(|c| c.is_ascii_alphanumeric() || matches!(c, '-' | '_' | '.'))
    {
        return Err(MediaError::InvalidInput(format!(
            "{field} '{value}' contains characters unsafe for a file name"
        )));
    }
    Ok(value.to_string())
}

/// Format an fps value compactly (`"30"` not `"30.0"`).
fn format_fps(fps: f64) -> String {
    if fps.fract() == 0.0 {
        format!("{}", fps as i64)
    } else {
        let rounded = (fps * 1000.0).round() / 1000.0;
        format!("{rounded}")
    }
}

/// Format seconds compactly for `-ss` / `-t`.
fn format_secs(secs: f64) -> String {
    let rounded = (secs * 1000.0).round() / 1000.0;
    if rounded.fract() == 0.0 {
        format!("{}", rounded as i64)
    } else {
        format!("{rounded}")
    }
}

/// Best-effort MIME type for common image extensions.
fn content_type_for(ext: &str) -> Option<String> {
    let mime = match ext.to_ascii_lowercase().as_str() {
        "png" => "image/png",
        "jpg" | "jpeg" => "image/jpeg",
        "webp" => "image/webp",
        "bmp" => "image/bmp",
        "gif" => "image/gif",
        "tif" | "tiff" => "image/tiff",
        _ => return None,
    };
    Some(mime.to_string())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::crates::multimedia_engine::runtime::process::run;
    use std::path::PathBuf;

    fn runtime() -> Runtime {
        Runtime::discover().expect("ffmpeg/ffprobe must be on PATH")
    }

    fn limits() -> MediaLimits {
        MediaLimits::default()
    }

    /// Generate a `duration_secs`-long `fps`-fps testsrc mp4 via lavfi.
    fn sample_input(duration_secs: u32, fps: u32) -> Vec<u8> {
        let rt = runtime();
        let dir = TempDir::new("frames-interval-test-in").expect("tempdir");
        let out_path: PathBuf = dir.file("sample.mp4");
        let argv = process::args(&[
            "-hide_banner",
            "-loglevel",
            "error",
            "-nostdin",
            "-y",
            "-f",
            "lavfi",
            "-i",
            &format!("testsrc=duration={duration_secs}:size=160x120:rate={fps}"),
            "-c:v",
            "mpeg4",
            "-pix_fmt",
            "yuv420p",
            out_path.to_str().expect("utf8 path"),
        ]);
        run("ffmpeg", &rt.ffmpeg, &argv, None, &limits()).expect("generate sample");
        std::fs::read(&out_path).expect("read sample")
    }

    fn frame_count(out: &MediaOutput) -> usize {
        out.files().count()
    }

    #[test]
    fn every_secs_extracts_expected_count() {
        let rt = runtime();
        // 2 s @ 10 fps → EverySecs(0.5) ⇒ fps=2 ⇒ ~4 frames.
        let input = sample_input(2, 10);
        let options = IntervalOptions {
            schedule: FrameSchedule::EverySecs(0.5),
            ..Default::default()
        };
        let out = frames_interval(&input, &options, &limits(), &rt).expect("extract");
        let count = frame_count(&out);
        assert!((3..=6).contains(&count), "expected ~4 frames, got {count}");

        let primary = out.primary.expect("primary frame");
        assert!(primary.name.starts_with("frame_"), "name: {}", primary.name);
        assert!(primary.name.ends_with(".png"), "name: {}", primary.name);
        assert!(!primary.bytes.is_empty());
        assert_eq!(primary.content_type.as_deref(), Some("image/png"));
        // Remaining frames are all present and non-empty.
        for f in &out.extra {
            assert!(!f.bytes.is_empty());
            assert!(f.name.ends_with(".png"));
        }
        assert_eq!(out.extra.len() + 1, count);
    }

    #[test]
    fn fps_schedule_resamples() {
        let rt = runtime();
        // 1 s @ 10 fps → Fps(2) ⇒ ~2 frames.
        let input = sample_input(1, 10);
        let options = IntervalOptions {
            schedule: FrameSchedule::Fps(2.0),
            ..Default::default()
        };
        let out = frames_interval(&input, &options, &limits(), &rt).expect("extract");
        let count = frame_count(&out);
        assert!((1..=3).contains(&count), "expected ~2 frames, got {count}");
    }

    #[test]
    fn max_frames_exceeding_is_limit_exceeded() {
        let rt = runtime();
        // 2 s @ 10 fps, EverySecs(0.1) ⇒ ~20 frames; cap at 3.
        let input = sample_input(2, 10);
        let options = IntervalOptions {
            schedule: FrameSchedule::EverySecs(0.1),
            max_frames: Some(3),
            ..Default::default()
        };
        let err =
            frames_interval(&input, &options, &limits(), &rt).expect_err("must exceed max_frames");
        match err {
            MediaError::LimitExceeded(msg) => {
                assert!(msg.contains("frame"), "msg: {msg}");
            }
            other => panic!("expected LimitExceeded, got {other:?}"),
        }
    }

    #[test]
    fn invalid_schedule_rejected() {
        let rt = runtime();
        let input = sample_input(1, 10);
        for schedule in [FrameSchedule::EverySecs(0.0), FrameSchedule::Fps(-1.0)] {
            let options = IntervalOptions {
                schedule,
                ..Default::default()
            };
            let err = frames_interval(&input, &options, &limits(), &rt)
                .expect_err("must reject invalid schedule");
            assert!(
                matches!(err, MediaError::InvalidInput(_)),
                "expected InvalidInput, got {err:?}"
            );
        }
    }
}
