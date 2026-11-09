//! Remove selected time ranges from a video, keeping everything else.
//!
//! [`remove_sections`] computes the *complement* of the requested
//! removal ranges within `[0, duration]` (duration is probed via
//! ffprobe), then rebuilds the file from the surviving segments:
//!
//! - **Re-encode path** (`reencode: true`, the default): each keep
//!   segment is extracted with `ffmpeg -ss <s> -to <e> -i in ...`
//!   re-encoded to a uniform intermediate codec, then the segments
//!   are joined with the concat demuxer (`-c copy`). Frame-accurate
//!   cuts; required because stream-copy cuts only land on keyframes.
//! - **Stream-copy path** (`reencode: false`): the same segment
//!   pipeline but with `-c copy` throughout. **Limitation:** cuts
//!   snap to the nearest keyframe before the requested boundary, so
//!   kept segments may include up to one GOP of extra material at
//!   each cut and output duration is approximate. Audio is cut on
//!   packet boundaries. Use only when keyframe-aligned cuts are
//!   acceptable.
//!
//! A single keep segment skips the concat step entirely. Only the
//! first video stream and first audio stream are mapped — attached
//! subtitles, data and cover-art streams are dropped (re-encoding
//! them per segment is unreliable across containers).

use std::ffi::OsString;
use std::path::PathBuf;

use crate::crates::multimedia_engine::error::{MediaError, MediaResult};
use crate::crates::multimedia_engine::model::{MediaLimits, MediaOutput};
use crate::crates::multimedia_engine::runtime::discovery::Runtime;
use crate::crates::multimedia_engine::runtime::process;
use crate::crates::multimedia_engine::runtime::tempdir::TempDir;

/// Options for [`remove_sections`].
#[derive(Debug, Clone)]
pub struct RemoveSectionsOptions {
    /// Ranges to remove, as `(start_secs, end_secs)`. Entries are
    /// sorted and merged internally, so order and overlap do not
    /// matter. Each range must satisfy `0 <= start < end`; `end` is
    /// clamped to the probed duration and `start` must lie inside it.
    pub remove: Vec<(f64, f64)>,
    /// Output container extension (`"mp4"`, `"mkv"`, `"webm"`, ...).
    /// `None` keeps the input's container family (mp4/mov → `mp4`,
    /// matroska → `mkv`, mpegts → `ts`, otherwise `mkv`).
    pub output_format: Option<String>,
    /// Re-encode kept segments for frame-accurate cuts (default
    /// `true`). `false` uses stream copy — faster, lossless, but cuts
    /// snap to keyframes (see module docs).
    pub reencode: bool,
}

impl Default for RemoveSectionsOptions {
    fn default() -> Self {
        Self {
            remove: Vec::new(),
            output_format: None,
            reencode: true,
        }
    }
}

/// Minimum keep-segment length in seconds. Shorter slivers are
/// dropped — a sub-frame segment cannot produce meaningful output
/// and would make ffmpeg fail with an empty file.
const MIN_KEEP_SECS: f64 = 0.002;

/// One normalized `(start, end)` range in seconds.
type Range = (f64, f64);

/// Remove `options.remove` ranges from `input`, returning the edited
/// video as a single-file [`MediaOutput`].
pub fn remove_sections(
    input: &[u8],
    options: &RemoveSectionsOptions,
    limits: &MediaLimits,
    rt: &Runtime,
) -> MediaResult<MediaOutput> {
    if let Some(max) = limits.max_input_bytes
        && input.len() as u64 > max
    {
        return Err(MediaError::LimitExceeded(format!(
            "input is {} bytes, exceeding max_input_bytes {max}",
            input.len()
        )));
    }

    let tmp = TempDir::new("remove-sections")?;
    // ffmpeg/ffprobe probe by content; a neutral name is fine for input.
    let input_path = tmp.write("input.bin", input)?;

    let duration = probe_duration(&input_path, limits, rt)?;
    let removes = normalize_ranges(&options.remove, duration)?;
    let keeps = keep_segments(&removes, duration);
    if keeps.is_empty() {
        return Err(MediaError::InvalidInput(
            "removal ranges cover the entire video; nothing to keep".to_string(),
        ));
    }

    let ext = output_extension(&input_path, options.output_format.as_deref(), limits, rt)?;
    let out_name = format!("output.{ext}");
    let out_path = tmp.file(&out_name);

    if keeps.len() == 1 {
        // Single survivor: encode/copy straight to the output name,
        // no concat needed.
        let (s, e) = keeps[0];
        run_segment(
            &input_path,
            s,
            e,
            &out_path,
            &ext,
            options.reencode,
            limits,
            rt,
        )?;
    } else {
        let mut seg_paths = Vec::with_capacity(keeps.len());
        for (i, &(s, e)) in keeps.iter().enumerate() {
            let seg_name = format!("seg{i}.{ext}");
            let seg_path = tmp.file(&seg_name);
            run_segment(
                &input_path,
                s,
                e,
                &seg_path,
                &ext,
                options.reencode,
                limits,
                rt,
            )?;
            seg_paths.push(seg_path);
        }
        concat_segments(&seg_paths, &out_path, limits, rt)?;
    }

    let bytes = tmp.read_limited(&out_name, limits.max_output_bytes)?;
    let mut output = MediaOutput::single(out_name, bytes);
    output.primary.as_mut().map(|f| {
        f.content_type = Some(content_type_for(&ext).to_string());
    });
    if !options.reencode {
        output.warnings.push(
            "stream-copy mode: cuts snap to the nearest keyframe; \
             output duration is approximate"
                .to_string(),
        );
    }
    Ok(output)
}

/// Probe the container duration (seconds) via `ffprobe -show_format`.
fn probe_duration(
    input_path: &std::path::Path,
    limits: &MediaLimits,
    rt: &Runtime,
) -> MediaResult<f64> {
    let out = process::run_checked(
        "ffprobe",
        &rt.ffprobe,
        &[
            OsString::from("-v"),
            OsString::from("error"),
            OsString::from("-show_format"),
            OsString::from("-of"),
            OsString::from("json"),
            input_path.as_os_str().to_os_string(),
        ],
        None,
        limits,
    )?;
    let report: serde_json::Value = serde_json::from_slice(&out.stdout)
        .map_err(|e| MediaError::InvalidInput(format!("ffprobe JSON parse failed: {e}")))?;
    let duration = report
        .get("format")
        .and_then(|f| f.get("duration"))
        .and_then(|d| d.as_str())
        .and_then(|d| d.parse::<f64>().ok())
        .or_else(|| {
            // Some containers omit format.duration; fall back to the
            // longest stream duration.
            report
                .get("streams")
                .and_then(|s| s.as_array())
                .into_iter()
                .flatten()
                .filter_map(|s| s.get("duration")?.as_str()?.parse::<f64>().ok())
                .reduce(f64::max)
        });
    duration
        .filter(|d| d.is_finite() && *d > 0.0)
        .ok_or_else(|| MediaError::InvalidInput("could not determine media duration".to_string()))
}

/// Validate, sort and merge removal ranges against `duration`.
/// Returns `InvalidInput` on malformed ranges or ranges entirely
/// outside the media.
fn normalize_ranges(remove: &[(f64, f64)], duration: f64) -> MediaResult<Vec<Range>> {
    let mut ranges: Vec<Range> = Vec::with_capacity(remove.len());
    for &(s, e) in remove {
        if !s.is_finite() || !e.is_finite() {
            return Err(MediaError::InvalidInput(format!(
                "non-finite removal range ({s}, {e})"
            )));
        }
        if s < 0.0 {
            return Err(MediaError::InvalidInput(format!(
                "removal range start {s} is negative"
            )));
        }
        if e <= s {
            return Err(MediaError::InvalidInput(format!(
                "removal range ({s}, {e}) is empty or inverted"
            )));
        }
        if s >= duration {
            return Err(MediaError::InvalidInput(format!(
                "removal range start {s} is beyond media duration {duration:.3}s"
            )));
        }
        // Clamp the tail to the real duration.
        ranges.push((s, e.min(duration)));
    }
    ranges.sort_by(|a, b| a.0.total_cmp(&b.0));
    // Merge overlapping or touching ranges.
    let mut merged: Vec<Range> = Vec::with_capacity(ranges.len());
    for (s, e) in ranges {
        if let Some(last) = merged.last_mut()
            && s <= last.1 + f64::EPSILON
        {
            last.1 = last.1.max(e);
            continue;
        }
        merged.push((s, e));
    }
    Ok(merged)
}

/// Complement of `removes` within `[0, duration]`, dropping slivers
/// shorter than [`MIN_KEEP_SECS`].
fn keep_segments(removes: &[Range], duration: f64) -> Vec<Range> {
    let mut keeps = Vec::with_capacity(removes.len() + 1);
    let mut cursor = 0.0f64;
    for &(s, e) in removes {
        if s - cursor > MIN_KEEP_SECS {
            keeps.push((cursor, s));
        }
        cursor = cursor.max(e);
    }
    if duration - cursor > MIN_KEEP_SECS {
        keeps.push((cursor, duration));
    }
    keeps
}

/// Choose the output container extension.
fn output_extension(
    input_path: &std::path::Path,
    requested: Option<&str>,
    limits: &MediaLimits,
    rt: &Runtime,
) -> MediaResult<String> {
    if let Some(fmt) = requested {
        let ext = fmt.trim().trim_start_matches('.').to_ascii_lowercase();
        if ext.is_empty() || !ext.chars().all(|c| c.is_ascii_alphanumeric()) {
            return Err(MediaError::InvalidInput(format!(
                "invalid output_format {fmt:?}"
            )));
        }
        return Ok(ext);
    }
    // Probe the input container name to pick a compatible output.
    let out = process::run_checked(
        "ffprobe",
        &rt.ffprobe,
        &[
            OsString::from("-v"),
            OsString::from("error"),
            OsString::from("-show_entries"),
            OsString::from("format=format_name"),
            OsString::from("-of"),
            OsString::from("default=noprint_wrappers=1:nokey=1"),
            input_path.as_os_str().to_os_string(),
        ],
        None,
        limits,
    )?;
    let names = String::from_utf8_lossy(&out.stdout).to_ascii_lowercase();
    Ok(
        if names.contains("mp4")
            || names.contains("mov")
            || names.contains("m4a")
            || names.contains("3gp")
        {
            "mp4".to_string()
        } else if names.contains("matroska") {
            "mkv".to_string()
        } else if names.contains("webm") {
            "webm".to_string()
        } else if names.contains("mpegts") || names.contains("mpegtsraw") {
            "ts".to_string()
        } else if names.contains("avi") {
            "avi".to_string()
        } else {
            // Safe generic default: mp4 accepts h264/aac from every
            // common source and is the most interoperable container.
            "mp4".to_string()
        },
    )
}

/// Encode (or copy) one keep segment `[s, e)` to `out_path`.
fn run_segment(
    input_path: &std::path::Path,
    s: f64,
    e: f64,
    out_path: &std::path::Path,
    ext: &str,
    reencode: bool,
    limits: &MediaLimits,
    rt: &Runtime,
) -> MediaResult<()> {
    let mut argv: Vec<OsString> = vec![
        OsString::from("-y"),
        OsString::from("-hide_banner"),
        OsString::from("-loglevel"),
        OsString::from("error"),
        // Input-side seek/read window: frame-accurate with re-encode.
        OsString::from("-ss"),
        OsString::from(format!("{s:.3}")),
        OsString::from("-to"),
        OsString::from(format!("{e:.3}")),
        OsString::from("-i"),
        input_path.as_os_str().to_os_string(),
        // First video stream + optional first audio stream only.
        OsString::from("-map"),
        OsString::from("0:v:0"),
        OsString::from("-map"),
        OsString::from("0:a:0?"),
    ];
    if reencode {
        argv.extend(video_encode_args(rt)?);
        argv.extend([
            OsString::from("-c:a"),
            OsString::from("aac"),
            OsString::from("-b:a"),
            OsString::from("128k"),
        ]);
    } else {
        argv.extend([OsString::from("-c"), OsString::from("copy")]);
    }
    // mp4-family outputs need a compatible pixel format.
    if reencode && matches!(ext, "mp4" | "mov" | "m4v" | "3gp") {
        argv.extend([OsString::from("-pix_fmt"), OsString::from("yuv420p")]);
    }
    argv.push(out_path.as_os_str().to_os_string());
    process::run_checked("ffmpeg", &rt.ffmpeg, &argv, None, limits)?;
    Ok(())
}

/// Pick a video encoder this FFmpeg build actually has. Preference
/// order: libx264 (best quality/size), libopenh264 (common in
/// GPL-free builds), mpeg4 (universal fallback). Returns
/// [`MediaError::Unsupported`] when none exist — never silently
/// substitutes an audio-only or image codec.
fn video_encode_args(rt: &Runtime) -> MediaResult<Vec<OsString>> {
    let caps = rt.capabilities()?;
    if caps.has_encoder("libx264") {
        return Ok(vec![
            OsString::from("-c:v"),
            OsString::from("libx264"),
            OsString::from("-preset"),
            OsString::from("veryfast"),
            OsString::from("-crf"),
            OsString::from("23"),
        ]);
    }
    if caps.has_encoder("libopenh264") {
        return Ok(vec![
            OsString::from("-c:v"),
            OsString::from("libopenh264"),
            OsString::from("-b:v"),
            OsString::from("4M"),
        ]);
    }
    if caps.has_encoder("mpeg4") {
        return Ok(vec![
            OsString::from("-c:v"),
            OsString::from("mpeg4"),
            OsString::from("-q:v"),
            OsString::from("5"),
        ]);
    }
    Err(MediaError::Unsupported(
        "no usable video encoder (libx264, libopenh264 or mpeg4) in this FFmpeg build".to_string(),
    ))
}

/// Join segment files with the concat demuxer, stream-copying.
fn concat_segments(
    seg_paths: &[PathBuf],
    out_path: &std::path::Path,
    limits: &MediaLimits,
    rt: &Runtime,
) -> MediaResult<()> {
    // The concat demuxer reads `file '<path>'` lines; our temp paths
    // never contain quotes, but escape defensively anyway.
    let mut list = String::new();
    for p in seg_paths {
        let s = p.display().to_string().replace('\'', "'\\''");
        list.push_str(&format!("file '{s}'\n"));
    }
    let list_path = out_path.with_file_name("concat.txt");
    std::fs::write(&list_path, list)?;

    process::run_checked(
        "ffmpeg",
        &rt.ffmpeg,
        &[
            OsString::from("-y"),
            OsString::from("-hide_banner"),
            OsString::from("-loglevel"),
            OsString::from("error"),
            OsString::from("-f"),
            OsString::from("concat"),
            OsString::from("-safe"),
            OsString::from("0"),
            OsString::from("-i"),
            list_path.as_os_str().to_os_string(),
            OsString::from("-c"),
            OsString::from("copy"),
            out_path.as_os_str().to_os_string(),
        ],
        None,
        limits,
    )?;
    Ok(())
}

/// MIME type for the produced container, when known.
fn content_type_for(ext: &str) -> &'static str {
    match ext {
        "mp4" | "m4v" | "mov" | "3gp" => "video/mp4",
        "mkv" => "video/x-matroska",
        "webm" => "video/webm",
        "ts" => "video/mp2t",
        "avi" => "video/x-msvideo",
        "mp3" => "audio/mpeg",
        "wav" => "audio/wav",
        "flac" => "audio/flac",
        "ogg" | "oga" | "opus" => "audio/ogg",
        _ => "application/octet-stream",
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::crates::multimedia_engine::runtime::discovery::Runtime;

    /// Build a 4-second 160x120@10fps clip with a 440Hz sine track.
    fn test_clip() -> Vec<u8> {
        let tmp = TempDir::new("remove-sections-test").unwrap();
        let out = tmp.file("clip.mp4");
        let status = std::process::Command::new("ffmpeg")
            .args([
                "-y",
                "-hide_banner",
                "-loglevel",
                "error",
                "-f",
                "lavfi",
                "-i",
                "testsrc=duration=4:size=160x120:rate=10",
                "-f",
                "lavfi",
                "-i",
                "sine=frequency=440:duration=4",
                "-c:v",
                "libopenh264",
                "-g",
                "10",
                "-c:a",
                "aac",
                "-shortest",
            ])
            .arg(&out)
            .status()
            .expect("ffmpeg must be installed for tests");
        assert!(status.success(), "test clip generation failed");
        std::fs::read(&out).unwrap()
    }

    fn probe_secs(path: &std::path::Path) -> f64 {
        let out = std::process::Command::new("ffprobe")
            .args([
                "-v",
                "error",
                "-show_entries",
                "format=duration",
                "-of",
                "default=noprint_wrappers=1:nokey=1",
            ])
            .arg(path)
            .output()
            .unwrap();
        String::from_utf8_lossy(&out.stdout)
            .trim()
            .parse::<f64>()
            .unwrap()
    }

    #[test]
    fn removes_middle_second() {
        let rt = Runtime::discover().unwrap();
        let limits = MediaLimits::default();
        let input = test_clip();
        let opts = RemoveSectionsOptions {
            remove: vec![(1.0, 3.0)],
            ..Default::default()
        };
        let result = remove_sections(&input, &opts, &limits, &rt).unwrap();
        let primary = result.primary.expect("primary output");
        assert!(primary.name.ends_with(".mp4"));

        // Write output to a temp file and measure its duration.
        let tmp = TempDir::new("remove-sections-verify").unwrap();
        let out_path = tmp.write("out.mp4", &primary.bytes).unwrap();
        let duration = probe_secs(&out_path);
        assert!(
            (duration - 2.0).abs() < 0.35,
            "expected ~2s after removing 1-3s of a 4s clip, got {duration}s"
        );
    }

    #[test]
    fn rejects_invalid_ranges() {
        let rt = Runtime::discover().unwrap();
        let limits = MediaLimits::default();
        let input = test_clip();

        // Inverted range.
        let opts = RemoveSectionsOptions {
            remove: vec![(3.0, 1.0)],
            ..Default::default()
        };
        assert!(matches!(
            remove_sections(&input, &opts, &limits, &rt),
            Err(MediaError::InvalidInput(_))
        ));

        // Start beyond the probed duration.
        let opts = RemoveSectionsOptions {
            remove: vec![(10.0, 12.0)],
            ..Default::default()
        };
        assert!(matches!(
            remove_sections(&input, &opts, &limits, &rt),
            Err(MediaError::InvalidInput(_))
        ));

        // Removing everything leaves no keep segments.
        let opts = RemoveSectionsOptions {
            remove: vec![(0.0, 4.0)],
            ..Default::default()
        };
        assert!(matches!(
            remove_sections(&input, &opts, &limits, &rt),
            Err(MediaError::InvalidInput(_))
        ));
    }
}
