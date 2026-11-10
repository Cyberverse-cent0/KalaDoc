//! Scene-change detection and optional split-at-scenes for video.
//!
//! # Detection approach
//!
//! Detection runs one `ffmpeg` pass:
//!
//! ```text
//! ffmpeg -i INPUT -map 0:v:0 -vf "select='gt(scene,T)',showinfo" -f null -
//! ```
//!
//! FFmpeg's `select` filter computes a per-frame "scene score" in
//! `0.0..=1.0` by comparing each frame to its predecessor; frames whose
//! score exceeds `T` are the only ones that reach `showinfo`, which
//! prints one `pts_time:` line per surviving frame to **stderr**. Each
//! parsed `pts_time` is therefore a scene-change timestamp — this is
//! the same mechanism behind the classic
//! `select='gt(scene,0.4)',showinfo` recipe, and it needs no second
//! pass or metadata plumbing. (`scdet` is the alternative filter; it
//! requires a two-pass workflow — `sc_pass=1` to gather scores, then
//! `sc_pass=2` to mark frames — so `select` is simpler and preferred
//! when both are available. We still verify `select`/`showinfo` exist
//! via [`Runtime::capabilities`] and return
//! [`MediaError::Unsupported`] otherwise.)
//!
//! # Known limitation
//!
//! `runtime::process` keeps only the last 4 KiB of stderr in
//! [`ProcOutput`]. `showinfo` output is sparse (one line per detected
//! scene, ~200 bytes), so thousands of scene changes could push early
//! `pts_time` lines out of the tail. When the captured stderr was
//! tail-truncated (it is prefixed with `…`) a warning is added to the
//! report; typical inputs are unaffected.
//!
//! # Splitting
//!
//! With [`SceneOptions::split`], each interval between consecutive
//! scene boundaries (`0`, each scene time, `duration`) is cut with
//! `ffmpeg -ss START -t LEN -i INPUT -map 0 -c copy`. Stream copy is
//! used for speed — **segment boundaries snap to the nearest
//! keyframe**, so cuts may land earlier/later than the detected scene
//! time on GOP-structured codecs. Segment files are returned in
//! [`SceneReport::segments`] (the `MediaOutput.extra` role); a
//! per-segment failure produces a warning rather than aborting the
//! whole report.

use std::ffi::OsString;
use std::path::PathBuf;

use crate::crates::multimedia_engine::error::{MediaError, MediaResult};
use crate::crates::multimedia_engine::model::{MediaFile, MediaInput, MediaLimits};
use crate::crates::multimedia_engine::runtime::discovery::Runtime;
use crate::crates::multimedia_engine::runtime::process;
use crate::crates::multimedia_engine::runtime::tempdir::TempDir;

/// Default scene-score threshold. `0.0` flags nearly every frame;
/// `1.0` effectively disables detection. `0.3` is the commonly cited
/// starting point for `select='gt(scene,T)'`.
const DEFAULT_THRESHOLD: f32 = 0.3;

/// Default minimum spacing between reported scene boundaries.
/// Scenes closer together than this are merged into one.
const DEFAULT_MIN_SEGMENT_SECS: f64 = 0.5;

/// Options for [`detect_scenes`].
#[derive(Debug, Clone)]
pub struct SceneOptions {
    /// Scene-score threshold in `0.0..=1.0`, mapped to
    /// `select='gt(scene,threshold)'`. Values outside the range are
    /// clamped (a warning is recorded).
    pub threshold: f32,
    /// Merge scene boundaries closer together than this many seconds.
    /// `0.0` keeps every detected boundary.
    pub min_segment_secs: f64,
    /// When `true`, also produce one segment file per scene interval
    /// (stream copy — see module docs for the keyframe caveat).
    pub split: bool,
}

impl Default for SceneOptions {
    fn default() -> Self {
        Self {
            threshold: DEFAULT_THRESHOLD,
            min_segment_secs: DEFAULT_MIN_SEGMENT_SECS,
            split: false,
        }
    }
}

/// Result of scene detection.
#[derive(Debug, Clone, Default)]
pub struct SceneReport {
    /// Scene-change timestamps in seconds, ascending. Does **not**
    /// include `0.0` — each entry is the start of a new scene.
    pub scene_times: Vec<f64>,
    /// Number of detected scenes (`scene_times.len()`).
    pub count: usize,
    /// Input duration in seconds.
    pub duration_secs: f64,
    /// Per-scene segment files, populated only when
    /// [`SceneOptions::split`] was set. Ordered by start time; there
    /// are `count + 1` entries when every segment succeeded.
    pub segments: Vec<MediaFile>,
    /// Non-fatal diagnostics (clamped threshold, truncated stderr,
    /// failed segment cuts, ...).
    pub warnings: Vec<String>,
}

/// Detect scene changes in `input`.
///
/// `input` is the media bytes plus a display name whose extension is
/// reused for the temp file (and for segment naming when splitting).
/// `limits` bounds input size, the ffmpeg/ffprobe timeouts, read-back
/// bytes and the number of produced segment files.
pub fn detect_scenes(
    input: &MediaInput,
    options: &SceneOptions,
    limits: &MediaLimits,
    rt: &Runtime,
) -> MediaResult<SceneReport> {
    let mut report = SceneReport::default();
    let mut warnings = Vec::new();

    // --- input size cap -------------------------------------------------
    if let Some(max) = limits.max_input_bytes
        && input.data.len() as u64 > max
    {
        return Err(MediaError::LimitExceeded(format!(
            "input {} is {} bytes, exceeding max_input_bytes {max}",
            input.name,
            input.data.len()
        )));
    }

    // --- capability check ----------------------------------------------
    let caps = rt.capabilities()?;
    for filter in ["select", "showinfo"] {
        if !caps.has_filter(filter) {
            return Err(MediaError::Unsupported(format!(
                "scene detection requires the '{filter}' filter, not present in this FFmpeg build"
            )));
        }
    }

    // --- threshold -------------------------------------------------------
    let mut threshold = options.threshold;
    if !(0.0..=1.0).contains(&threshold) {
        warnings.push(format!(
            "threshold {} outside 0.0..=1.0; clamped",
            options.threshold
        ));
        threshold = threshold.clamp(0.0, 1.0);
    }
    let min_segment = options.min_segment_secs.max(0.0);

    // --- stage input -----------------------------------------------------
    let tmp = TempDir::new("scenes")?;
    let ext = input_ext(&input.name);
    let input_path = tmp.write(&format!("input.{ext}"), &input.data)?;

    // --- detection pass --------------------------------------------------
    let vf = format!("select='gt(scene,{threshold})',showinfo");
    let ffmpeg_args: Vec<OsString> = process::args(&[
        "-hide_banner",
        "-i",
        &input_path.to_string_lossy(),
        "-map",
        "0:v:0",
        "-vf",
        &vf,
        "-f",
        "null",
        "-",
    ]);
    let out = process::run_checked("ffmpeg", &rt.ffmpeg, &ffmpeg_args, None, limits)?;

    if out.stderr.starts_with('…') {
        warnings.push(
            "ffmpeg stderr was tail-truncated; with very many scene changes \
             early timestamps may be missing"
                .to_string(),
        );
    }

    // --- parse timestamps -------------------------------------------------
    let mut scene_times = parse_pts_times(&out.stderr);
    scene_times.sort_by(|a, b| a.partial_cmp(b).unwrap_or(std::cmp::Ordering::Equal));
    scene_times.dedup_by(|a, b| (*a - *b).abs() < f64::EPSILON);

    // --- duration ----------------------------------------------------------
    let duration_secs = probe_duration(&input_path, rt, limits)
        .or_else(|_| {
            // Fall back to the last reported frame time.
            Ok::<f64, MediaError>(scene_times.last().copied().unwrap_or(0.0))
        })
        .unwrap_or(0.0);

    // Drop boundaries at/past the end and non-positive times.
    scene_times.retain(|&t| t > 0.0 && (duration_secs <= 0.0 || t < duration_secs));

    // --- merge close scenes -------------------------------------------------
    let scene_times = merge_close(scene_times, min_segment);
    report.count = scene_times.len();
    report.scene_times = scene_times;
    report.duration_secs = duration_secs;

    // --- optional split ------------------------------------------------------
    if options.split && report.count > 0 {
        report.segments = split_segments(
            &input_path,
            &report.scene_times,
            duration_secs,
            ext,
            limits,
            rt,
            &mut warnings,
        )?;
    }

    report.warnings = warnings;
    Ok(report)
}

/// Container extension to use for the staged input / segment files.
/// Whitelisted extensions pass through; anything else becomes `mp4`
/// (ffmpeg probes the staged input by content anyway, so the temp
/// extension only matters for the *output* segments).
fn input_ext(name: &str) -> &'static str {
    let ext = PathBuf::from(name)
        .extension()
        .and_then(|e| e.to_str())
        .map(|e| e.to_ascii_lowercase());
    match ext.as_deref() {
        Some("mp4") | Some("m4v") => "mp4",
        Some("mov") => "mov",
        Some("mkv") => "mkv",
        Some("avi") => "avi",
        Some("webm") => "webm",
        _ => "mp4",
    }
}

/// Parse every `pts_time:` value out of `showinfo` stderr lines.
fn parse_pts_times(stderr: &str) -> Vec<f64> {
    stderr
        .lines()
        .filter_map(|line| {
            let idx = line.find("pts_time:")?;
            let rest = &line[idx + "pts_time:".len()..];
            rest.split_whitespace().next()?.parse::<f64>().ok()
        })
        .collect()
}

/// Container duration via ffprobe; `Err` when unavailable so callers
/// can fall back.
fn probe_duration(path: &std::path::Path, rt: &Runtime, limits: &MediaLimits) -> MediaResult<f64> {
    let args: Vec<OsString> = process::args(&[
        "-v",
        "error",
        "-show_entries",
        "format=duration",
        "-of",
        "default=noprint_wrappers=1:nokey=1",
        &path.to_string_lossy(),
    ]);
    let out = process::run_checked("ffprobe", &rt.ffprobe, &args, None, limits)?;
    let text = String::from_utf8_lossy(&out.stdout);
    text.trim()
        .parse::<f64>()
        .map_err(|_| MediaError::InvalidInput(format!("unparseable ffprobe duration: {text:?}")))
}

/// Drop boundaries closer than `min_segment` to the previously kept
/// one (first boundary always kept).
fn merge_close(times: Vec<f64>, min_segment: f64) -> Vec<f64> {
    let mut kept: Vec<f64> = Vec::with_capacity(times.len());
    for t in times {
        if kept.last().is_none_or(|&last| t - last >= min_segment) {
            kept.push(t);
        }
    }
    kept
}

/// Cut one segment file per scene interval using stream copy.
fn split_segments(
    input_path: &std::path::Path,
    scene_times: &[f64],
    duration: f64,
    ext: &'static str,
    limits: &MediaLimits,
    rt: &Runtime,
    warnings: &mut Vec<String>,
) -> MediaResult<Vec<MediaFile>> {
    // Boundaries: 0, each scene time, duration.
    let mut bounds = Vec::with_capacity(scene_times.len() + 2);
    bounds.push(0.0f64);
    bounds.extend_from_slice(scene_times);
    if duration > 0.0 {
        bounds.push(duration);
    }

    let segment_count = bounds.len().saturating_sub(1);
    if let Some(max) = limits.max_output_files
        && segment_count as u32 > max
    {
        return Err(MediaError::LimitExceeded(format!(
            "split would produce {segment_count} segment files, exceeding max_output_files {max}"
        )));
    }

    let tmp = TempDir::new("scenes-split")?;
    let input_str = input_path.to_string_lossy().into_owned();
    let mut files = Vec::new();

    for i in 0..segment_count {
        let start = bounds[i];
        let end = bounds[i + 1];
        let len = end - start;
        // Skip degenerate intervals (e.g. scene at t≈duration).
        if len <= 0.0 {
            continue;
        }
        let name = format!("scene_{:03}.{ext}", i + 1);
        let out_path = tmp.file(&name);
        let out_str = out_path.to_string_lossy().into_owned();

        let args: Vec<OsString> = process::args(&[
            "-hide_banner",
            "-loglevel",
            "error",
            "-y",
            "-ss",
            &format!("{start:.3}"),
            "-i",
            &input_str,
            "-t",
            &format!("{len:.3}"),
            "-map",
            "0",
            "-c",
            "copy",
            "-avoid_negative_ts",
            "make_zero",
            &out_str,
        ]);

        match process::run_checked("ffmpeg", &rt.ffmpeg, &args, None, limits) {
            Ok(_) => {
                let bytes = tmp.read_limited(&name, limits.max_output_bytes)?;
                files.push(MediaFile {
                    name,
                    bytes,
                    content_type: None,
                });
            }
            Err(e) => warnings.push(format!(
                "segment {i} ({start:.3}s–{end:.3}s) failed to cut: {e}"
            )),
        }
    }

    Ok(files)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::crates::multimedia_engine::runtime::discovery::Runtime;
    use std::process::Command;

    /// Build a 4 s test clip: 2 s of solid red (`color`) hard-cut to
    /// 2 s of `testsrc2`. `testsrc2` alone changes patterns smoothly
    /// and may not cross the scene threshold, so the concat cut is
    /// the reliable trigger. Encoded as mpeg4/avi — universally
    /// available, no libx264 dependency.
    fn make_test_clip() -> Vec<u8> {
        let tmp = TempDir::new("scenes-test").unwrap();
        let out_path = tmp.file("clip.avi");
        let status = Command::new("ffmpeg")
            .args([
                "-hide_banner",
                "-loglevel",
                "error",
                "-y",
                "-f",
                "lavfi",
                "-i",
                "color=c=red:s=320x240:d=2:r=25",
                "-f",
                "lavfi",
                "-i",
                "testsrc2=s=320x240:d=2:r=25",
                "-filter_complex",
                "[0:v][1:v]concat=n=2:v=1[v]",
                "-map",
                "[v]",
                "-c:v",
                "mpeg4",
                "-q:v",
                "5",
            ])
            .arg(&out_path)
            .status()
            .unwrap();
        assert!(status.success(), "test clip generation failed");
        std::fs::read(&out_path).unwrap()
    }

    fn test_input(data: Vec<u8>) -> MediaInput {
        MediaInput {
            name: "clip.avi".to_string(),
            data,
        }
    }

    fn test_rt() -> Runtime {
        Runtime::discover().expect("ffmpeg/ffprobe must be on PATH")
    }

    #[test]
    fn detects_scene_in_concat_clip() {
        let rt = test_rt();
        let input = test_input(make_test_clip());
        let report = detect_scenes(
            &input,
            &SceneOptions {
                threshold: 0.3,
                min_segment_secs: 0.0,
                split: false,
            },
            &MediaLimits::default(),
            &rt,
        )
        .expect("detect_scenes failed");

        assert!(
            report.count >= 1,
            "expected >=1 scene, got {:?} (warnings: {:?})",
            report.scene_times,
            report.warnings
        );
        // The hard cut is at 2.0 s; allow frame-level slop.
        let cut = report
            .scene_times
            .iter()
            .copied()
            .min_by(|a, b| (a - 2.0).abs().partial_cmp(&(b - 2.0).abs()).unwrap())
            .unwrap();
        assert!(
            (cut - 2.0).abs() <= 0.2,
            "scene at {cut}s, expected ~2.0s (times: {:?})",
            report.scene_times
        );
        assert!(
            (report.duration_secs - 4.0).abs() <= 0.2,
            "duration {} != ~4.0",
            report.duration_secs
        );
        assert!(report.segments.is_empty());
    }

    #[test]
    fn min_segment_merges_and_split_produces_segments() {
        let rt = test_rt();
        let input = test_input(make_test_clip());

        // First: with a large min_segment_secs the single cut must be
        // merged away relative to nothing — verify merge logic on a
        // synthetic list instead (unit-level), then split for real.
        let merged = merge_close(vec![1.0, 1.2, 3.0, 3.1, 6.0], 0.5);
        assert_eq!(merged, vec![1.0, 3.0, 6.0]);

        let report = detect_scenes(
            &input,
            &SceneOptions {
                threshold: 0.3,
                min_segment_secs: 0.0,
                split: true,
            },
            &MediaLimits::default(),
            &rt,
        )
        .expect("detect_scenes(split) failed");

        assert!(report.count >= 1);
        // One segment per interval: [0..t1], [t1..t2], ..., [tn..dur].
        assert_eq!(
            report.segments.len(),
            report.count + 1,
            "expected count+1 segments, warnings: {:?}",
            report.warnings
        );
        for seg in &report.segments {
            assert!(!seg.bytes.is_empty(), "empty segment {}", seg.name);
            assert!(seg.name.starts_with("scene_"));
        }
    }
}
