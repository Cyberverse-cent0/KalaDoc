//! Chapter inspection and editing: list embedded chapters, replace the
//! chapter table, remove all chapters, and shift chapter timestamps.
//!
//! Chapters live in the container's metadata, so every editing op is a
//! stream-copy remux (`-c copy`) — no re-encode, no quality loss.
//!
//! - [`chapters_list`] reads the chapter table via `ffprobe
//!   -show_chapters` (already part of [`probe_json`]).
//! - [`chapters_set`] writes an `ffmetadata` sidecar (`;FFMETADATA1`
//!   with `[CHAPTER]` blocks on a `TIMEBASE=1/1000` clock) and remuxes
//!   with `-map 0 -map_metadata 1 -c copy`. Only ISO-BMFF (`mp4`/`mov`)
//!   and Matroska (`mkv`) outputs are accepted — those are the muxers
//!   that reliably round-trip ffmetadata chapter tables.
//! - [`chapters_remove`] drops the table with `-map_chapters -1 -c
//!   copy` (verified against `ffmpeg -h full`).
//! - [`chapters_shift`] is [`chapters_list`] → offset →
//!   [`chapters_set`], so it inherits the same container restriction.
//!
//! Chapter times are millisecond-precision integers throughout
//! ([`ChapterInfo::start_ms`]/[`ChapterInfo::end_ms`]); the ffmetadata
//! `TIMEBASE=1/1000` clock makes the write side lossless, and the read
//! side parses both ffprobe's decimal-seconds strings and
//! `HH:MM:SS.mmm` forms defensively.

use serde_json::Value;

use crate::crates::core::engine::EngineLimits;
use crate::crates::video_engine::error::{VideoError, VideoResult};
use crate::crates::video_engine::ops::probe::probe;
use crate::crates::video_engine::ops::{
    COMMAND_TIMEOUT, FFMPEG, TempDir, check_input, check_limits, ext_for_format, failure_detail,
    find_tool, probe_json, read_output, run_tool, tool_failure, write_input,
};

/// One embedded chapter marker with millisecond-precision bounds.
///
/// Distinct from [`crate::crates::video_engine::model::ChapterInfo`],
/// which reports probe-time seconds (`start_secs`/`end_secs`) for the
/// `VideoInfo` snapshot; this type is the editing API's currency.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ChapterInfo {
    /// Chapter identifier as reported by ffprobe (`id`), when present.
    pub id: Option<u64>,
    /// Chapter start time in milliseconds.
    pub start_ms: i64,
    /// Chapter end time in milliseconds.
    pub end_ms: i64,
    /// Chapter title from the chapter's `title` tag, when present.
    pub title: Option<String>,
}

/// Output containers [`chapters_set`] accepts. `ext_for_format` maps
/// the whole ISO-BMFF family (`mov,mp4,m4a,...`) to `"mp4"`, so `"mov"`
/// never appears here — `"mp4"` and `"mkv"` cover the requirement.
const CHAPTER_SET_EXTS: &[&str] = &["mp4", "mkv"];

/// Parse an ffprobe chapter time into milliseconds. ffprobe normally
/// emits `start_time`/`end_time` as decimal-second strings
/// (`"1.500000"`), but demuxers and ffprobe versions have also produced
/// `HH:MM:SS.mmm`; both forms — plus bare JSON numbers — are handled.
fn time_to_ms(value: &Value) -> Option<i64> {
    match value {
        Value::Number(n) => n.as_f64().map(|s| (s * 1000.0).round() as i64),
        Value::String(s) => {
            let s = s.trim();
            if s.contains(':') {
                // HH:MM:SS[.mmm] (hours may exceed 99; seconds may be
                // fractional).
                let mut parts = s.rsplitn(3, ':');
                let secs = parts.next()?.parse::<f64>().ok()?;
                let mins = parts.next()?.parse::<i64>().ok()?;
                let hours = parts.next()?.parse::<i64>().ok()?;
                Some(((hours * 3600 + mins * 60) as f64 * 1000.0 + secs * 1000.0).round() as i64)
            } else {
                s.parse::<f64>()
                    .ok()
                    .map(|sec| (sec * 1000.0).round() as i64)
            }
        }
        _ => None,
    }
}

/// Map one ffprobe `chapters[]` element to [`ChapterInfo`]. Missing or
/// unparsable times default to `0` (matching `ops::probe`'s lenient
/// chapter parsing); a missing/empty `title` tag stays `None`.
fn chapter_from_json(c: &Value) -> ChapterInfo {
    let title = c
        .get("tags")
        .and_then(Value::as_object)
        .and_then(|t| t.get("title"))
        .and_then(|v| match v {
            Value::String(s) if !s.is_empty() => Some(s.clone()),
            Value::Number(n) => Some(n.to_string()),
            _ => None,
        });
    ChapterInfo {
        id: c.get("id").and_then(Value::as_u64),
        start_ms: c.get("start_time").and_then(time_to_ms).unwrap_or(0),
        end_ms: c.get("end_time").and_then(time_to_ms).unwrap_or(0),
        title,
    }
}

/// List the chapters embedded in `input`, in container order.
///
/// Returns an empty `Vec` when the container carries no chapters —
/// that is a valid state, not an error. The buffer is written to a
/// scratch file for `ffprobe`; a non-media input is
/// [`VideoError::InvalidInput`] via [`probe_json`].
pub(crate) fn chapters_list(input: &[u8]) -> VideoResult<Vec<ChapterInfo>> {
    check_input(input)?;
    let dir = TempDir::new("chapters-list")?;
    let input_path = write_input(&dir, "input.bin", input)?;
    let json = probe_json(&input_path)?;
    Ok(json
        .get("chapters")
        .and_then(Value::as_array)
        .map(|chs| chs.iter().map(chapter_from_json).collect())
        .unwrap_or_default())
}

/// Validate a caller-supplied chapter title for the ffmetadata
/// sidecar: NUL/CR/LF cannot cross the line-based format, and a
/// leading `;` would be parsed as a comment marker.
fn sanitize_title(title: &str) -> VideoResult<&str> {
    if title.contains(['\0', '\r', '\n']) {
        return Err(VideoError::InvalidInput(
            "chapter titles must not contain NUL, CR or LF characters".to_string(),
        ));
    }
    Ok(title)
}

/// Validate the chapter table for [`chapters_set`]: non-empty, every
/// chapter non-empty (`end > start`, non-negative start), strictly
/// sorted by start, and non-overlapping (`start >= previous end`).
fn validate_chapters(chapters: &[(i64, i64, &str)]) -> VideoResult<()> {
    if chapters.is_empty() {
        return Err(VideoError::InvalidInput(
            "chapters_set requires at least one chapter; use chapters_remove to clear".to_string(),
        ));
    }
    let mut prev_end: Option<i64> = None;
    for (idx, (start, end, title)) in chapters.iter().enumerate() {
        sanitize_title(title)?;
        if *start < 0 {
            return Err(VideoError::InvalidInput(format!(
                "chapter {idx} start {start}ms is negative"
            )));
        }
        if *end <= *start {
            return Err(VideoError::InvalidInput(format!(
                "chapter {idx} is empty or inverted: [{start}ms, {end}ms)"
            )));
        }
        if let Some(pe) = prev_end {
            if start < &pe {
                return Err(VideoError::InvalidInput(format!(
                    "chapters must be sorted and non-overlapping: chapter {idx} starts at \
                     {start}ms before previous chapter ends at {pe}ms"
                )));
            }
        }
        prev_end = Some(*end);
    }
    Ok(())
}

/// Serialize the chapter table as an `ffmetadata` document on a
/// millisecond clock. Empty titles omit the `title=` line entirely so
/// ffprobe reports `None` rather than an empty string.
fn ffmetadata(chapters: &[(i64, i64, &str)]) -> String {
    let mut out = String::from(";FFMETADATA1\n");
    for (start, end, title) in chapters {
        out.push_str(&format!(
            "[CHAPTER]\nTIMEBASE=1/1000\nSTART={start}\nEND={end}\n"
        ));
        if !title.is_empty() {
            out.push_str(&format!("title={title}\n"));
        }
    }
    out
}

/// Whether a non-zero ffmpeg exit looks like an honest container/muxer
/// rejection rather than a tool fault (mirrors `remux`/`metadata_set`).
fn is_container_rejection(detail: &str) -> bool {
    let lower = detail.to_lowercase();
    lower.contains("could not find tag")
        || lower.contains("muxer")
        || lower.contains("codec")
        || lower.contains("could not write header")
        || lower.contains("invalid argument")
        || lower.contains("not supported")
}

/// Probe `input` for limits + container facts, pick the scratch
/// extensions, and return `(info_format_name, input_ext, out_ext)`.
/// Shared preamble for the editing ops.
fn probe_for_remux(
    input: &[u8],
    input_len: u64,
    limits: &EngineLimits,
    allowed_out: Option<&[&str]>,
    op: &str,
) -> VideoResult<(String, String, String)> {
    let info = probe(input)?;
    check_limits(&info, input_len, limits, false)?;
    let format_name = info.format_name.clone().unwrap_or_default();
    let out_ext = ext_for_format(&format_name).ok_or_else(|| {
        VideoError::InvalidInput(format!(
            "{op}: cannot determine an output container for format_name '{format_name}'"
        ))
    })?;
    if let Some(allowed) = allowed_out {
        if !allowed.contains(&out_ext) {
            return Err(VideoError::InvalidInput(format!(
                "{op} only supports mp4/mov/mkv outputs; input format '{format_name}' \
                 would produce '{out_ext}'"
            )));
        }
    }
    let input_ext = ext_for_format(&format_name).unwrap_or("bin").to_string();
    Ok((format_name, input_ext, out_ext.to_string()))
}

/// Replace the chapter table of `input` with `chapters`
/// (`(start_ms, end_ms, title)` triples), returning the remuxed media.
///
/// The table is written to a temp `ffmetadata` file and applied with
/// `ffmpeg -i in -i chapters.ffmeta -map 0 -map_metadata 1 -c copy
/// out.ext`. `map_metadata 1` carries the sidecar's `[CHAPTER]` blocks
/// into the output container's chapter table (verified against ffmpeg
/// 7.1.5 for both mp4 and mkv).
///
/// The output container is derived from the probed `format_name` via
/// [`ext_for_format`] and restricted to `mp4`/`mkv` — chapters in other
/// containers (MPEG-TS, AVI, ...) are not round-tripped reliably, so
/// they are rejected with [`VideoError::InvalidInput`] rather than
/// silently dropped.
///
/// `chapters` must be sorted by start, non-overlapping, and every
/// chapter must satisfy `0 <= start < end`; violations are
/// [`VideoError::InvalidInput`]. Use [`chapters_remove`] to clear the
/// table — an empty `chapters` slice is rejected here.
pub(crate) fn chapters_set(
    input: &[u8],
    chapters: &[(i64, i64, &str)],
    limits: &EngineLimits,
) -> VideoResult<Vec<u8>> {
    let input_len = check_input(input)?;
    validate_chapters(chapters)?;
    let program = find_tool(FFMPEG)?;
    let (_format_name, input_ext, out_ext) = probe_for_remux(
        input,
        input_len,
        limits,
        Some(CHAPTER_SET_EXTS),
        "chapters_set",
    )?;

    let dir = TempDir::new("chapters-set")?;
    let input_path = write_input(&dir, &format!("input.{input_ext}"), input)?;
    let meta_path = dir.file("chapters.ffmeta");
    std::fs::write(&meta_path, ffmetadata(chapters)).map_err(VideoError::Io)?;
    let out_path = dir.file(&format!("out.{out_ext}"));

    let args = vec![
        "-y".to_string(),
        "-v".to_string(),
        "error".to_string(),
        "-i".to_string(),
        input_path.to_string_lossy().into_owned(),
        // Second input: the ffmetadata sidecar. `-map_metadata 1`
        // imports its chapters (and any global tags) into the output.
        "-i".to_string(),
        meta_path.to_string_lossy().into_owned(),
        "-map".to_string(),
        "0".to_string(),
        "-map_metadata".to_string(),
        "1".to_string(),
        // Force chapters from the ffmetadata input — without this the
        // muxer may keep the input file's existing chapter table.
        "-map_chapters".to_string(),
        "1".to_string(),
        "-c".to_string(),
        "copy".to_string(),
        out_path.to_string_lossy().into_owned(),
    ];

    let output = run_tool(FFMPEG, &program, &args, COMMAND_TIMEOUT)?;
    if !output.status.success() {
        let detail = failure_detail(&output, "ffmpeg exited unsuccessfully");
        if is_container_rejection(&detail) {
            return Err(VideoError::InvalidInput(format!(
                "container '{out_ext}' rejected the chapter table: {detail}"
            )));
        }
        return Err(tool_failure(FFMPEG, &output));
    }
    read_output(&out_path)
}

/// Remove every embedded chapter from `input`, returning the remuxed
/// media (`ffmpeg -map 0 -map_chapters -1 -c copy`).
///
/// `-map_chapters -1` disables chapter mapping for the output — the
/// option is present in `ffmpeg -h full` output on this build. Works
/// for any container [`ext_for_format`] can name; inputs without
/// chapters are remuxed unchanged.
pub(crate) fn chapters_remove(input: &[u8], limits: &EngineLimits) -> VideoResult<Vec<u8>> {
    let input_len = check_input(input)?;
    let program = find_tool(FFMPEG)?;
    let (_format_name, input_ext, out_ext) =
        probe_for_remux(input, input_len, limits, None, "chapters_remove")?;

    let dir = TempDir::new("chapters-remove")?;
    let input_path = write_input(&dir, &format!("input.{input_ext}"), input)?;
    let out_path = dir.file(&format!("out.{out_ext}"));

    let args = vec![
        "-y".to_string(),
        "-v".to_string(),
        "error".to_string(),
        "-i".to_string(),
        input_path.to_string_lossy().into_owned(),
        "-map".to_string(),
        "0".to_string(),
        "-map_chapters".to_string(),
        "-1".to_string(),
        "-c".to_string(),
        "copy".to_string(),
        out_path.to_string_lossy().into_owned(),
    ];

    let output = run_tool(FFMPEG, &program, &args, COMMAND_TIMEOUT)?;
    if !output.status.success() {
        let detail = failure_detail(&output, "ffmpeg exited unsuccessfully");
        if is_container_rejection(&detail) {
            return Err(VideoError::InvalidInput(format!(
                "container '{out_ext}' rejected chapter removal: {detail}"
            )));
        }
        return Err(tool_failure(FFMPEG, &output));
    }
    read_output(&out_path)
}

/// Shift every chapter in `input` by `offset_ms` milliseconds
/// (negative offsets move earlier), returning the remuxed media.
///
/// Implemented as [`chapters_list`] → offset → [`chapters_set`], so it
/// inherits the mp4/mkv output restriction. Note that ffmpeg's
/// ISO-BMFF muxer clamps the *first* chapter's start to 0 — mp4/mov
/// outputs cannot represent a leading gap before chapter 1, so on
/// those containers only the first chapter's start shift is lossy;
/// Matroska preserves all times exactly. `start_ms` is clamped to
/// `>= 0`; `end_ms` is clamped to `>= 0` and then to `> start_ms` (a
/// 1 ms minimum chapter) so the shifted table always passes
/// [`chapters_set`] validation — chapters that would collapse onto or
/// before zero are shortened rather than rejected.
///
/// Inputs with no chapters are returned unchanged (`Ok(input.to_vec())`)
/// — there is nothing to shift and no reason to remux.
pub(crate) fn chapters_shift(
    input: &[u8],
    offset_ms: i64,
    limits: &EngineLimits,
) -> VideoResult<Vec<u8>> {
    let chapters = chapters_list(input)?;
    if chapters.is_empty() {
        return Ok(input.to_vec());
    }
    let shifted: Vec<(i64, i64, &str)> = chapters
        .iter()
        .map(|c| {
            let start = (c.start_ms + offset_ms).max(0);
            // End clamps at >= start + 1ms: a chapter must be non-empty.
            let end = (c.end_ms + offset_ms).max(start + 1);
            (start, end, c.title.as_deref().unwrap_or(""))
        })
        .collect();
    chapters_set(input, &shifted, limits)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::crates::video_engine::ops::{FFPROBE, h264_encoder};
    use std::fs;

    fn tools_available() -> bool {
        find_tool(FFMPEG).is_ok() && find_tool(FFPROBE).is_ok()
    }

    /// Tiny 3s H.264+AAC clip in the requested container, built via
    /// lavfi. 3s gives `chapters_shift(+1000)` headroom inside the
    /// media. Returns `None` when ffmpeg or an H.264 encoder is
    /// unavailable.
    fn make_fixture(ext: &str) -> Option<Vec<u8>> {
        let program = find_tool(FFMPEG).ok()?;
        let enc = h264_encoder()?;
        let dir = TempDir::new("chapters-test").ok()?;
        let out = dir.file(&format!("fixture.{ext}"));
        let args = vec![
            "-y".to_string(),
            "-v".to_string(),
            "error".to_string(),
            "-f".to_string(),
            "lavfi".to_string(),
            "-i".to_string(),
            "testsrc=duration=3:size=160x120:rate=10".to_string(),
            "-f".to_string(),
            "lavfi".to_string(),
            "-i".to_string(),
            "sine=frequency=440:duration=3".to_string(),
            "-c:v".to_string(),
            enc.to_string(),
            "-pix_fmt".to_string(),
            "yuv420p".to_string(),
            "-c:a".to_string(),
            "aac".to_string(),
            "-shortest".to_string(),
            out.to_string_lossy().into_owned(),
        ];
        let output = run_tool(FFMPEG, &program, &args, COMMAND_TIMEOUT).ok()?;
        if !output.status.success() {
            return None;
        }
        fs::read(out).ok()
    }

    /// Two-chapter table used across the set/list/shift tests.
    fn two_chapters() -> Vec<(i64, i64, &'static str)> {
        vec![(0, 1000, "Intro"), (1000, 2000, "Main")]
    }

    /// Assert `actual` chapter times match `expected` (ms) within
    /// `tol` — containers may quantize to their own timescale.
    fn assert_times(actual: &[ChapterInfo], expected: &[(i64, i64)], tol: i64) {
        assert_eq!(actual.len(), expected.len(), "chapter count mismatch");
        for (c, (es, ee)) in actual.iter().zip(expected.iter()) {
            assert!(
                (c.start_ms - es).abs() <= tol,
                "start {} != {} (tol {tol})",
                c.start_ms,
                es
            );
            assert!(
                (c.end_ms - ee).abs() <= tol,
                "end {} != {} (tol {tol})",
                c.end_ms,
                ee
            );
        }
    }

    #[test]
    fn list_empty_when_no_chapters() {
        if !tools_available() {
            return;
        }
        let Some(fixture) = make_fixture("mp4") else {
            return;
        };
        assert!(chapters_list(&fixture).unwrap().is_empty());
    }

    #[test]
    fn set_then_list_roundtrips_titles_mp4() {
        if !tools_available() {
            return;
        }
        let Some(fixture) = make_fixture("mp4") else {
            return;
        };
        let chapters: Vec<(i64, i64, &str)> = two_chapters();
        let out = chapters_set(&fixture, &chapters, &EngineLimits::default()).unwrap();
        let listed = chapters_list(&out).unwrap();
        assert_eq!(listed.len(), 2);
        assert_eq!(listed[0].title.as_deref(), Some("Intro"));
        assert_eq!(listed[1].title.as_deref(), Some("Main"));
        assert_times(&listed, &[(0, 1000), (1000, 2000)], 10);
    }

    #[test]
    fn set_then_list_roundtrips_titles_mkv() {
        if !tools_available() {
            return;
        }
        let Some(fixture) = make_fixture("mkv") else {
            return;
        };
        let chapters: Vec<(i64, i64, &str)> = two_chapters();
        let out = chapters_set(&fixture, &chapters, &EngineLimits::default()).unwrap();
        let listed = chapters_list(&out).unwrap();
        assert_eq!(listed.len(), 2);
        assert_eq!(listed[0].title.as_deref(), Some("Intro"));
        assert_eq!(listed[1].title.as_deref(), Some("Main"));
        assert_times(&listed, &[(0, 1000), (1000, 2000)], 10);
    }

    #[test]
    fn remove_empties_chapter_table() {
        if !tools_available() {
            return;
        }
        let Some(fixture) = make_fixture("mp4") else {
            return;
        };
        let chapters: Vec<(i64, i64, &str)> = two_chapters();
        let with_chapters = chapters_set(&fixture, &chapters, &EngineLimits::default()).unwrap();
        assert_eq!(chapters_list(&with_chapters).unwrap().len(), 2);
        let stripped = chapters_remove(&with_chapters, &EngineLimits::default()).unwrap();
        assert!(
            chapters_list(&stripped).unwrap().is_empty(),
            "chapters_remove should leave an empty chapter table"
        );
    }

    #[test]
    fn shift_plus_1000_moves_starts_by_one_second() {
        if !tools_available() {
            return;
        }
        // mkv fixture: the ISO-BMFF muxer clamps the first chapter's
        // start to 0, which would mask the shift under test.
        let Some(fixture) = make_fixture("mkv") else {
            return;
        };
        let chapters: Vec<(i64, i64, &str)> = two_chapters();
        let with_chapters = chapters_set(&fixture, &chapters, &EngineLimits::default()).unwrap();
        let shifted = chapters_shift(&with_chapters, 1000, &EngineLimits::default()).unwrap();
        let listed = chapters_list(&shifted).unwrap();
        assert_eq!(listed.len(), 2);
        assert_times(&listed, &[(1000, 2000), (2000, 3000)], 10);
        // Titles survive the list→set round-trip.
        assert_eq!(listed[0].title.as_deref(), Some("Intro"));
        assert_eq!(listed[1].title.as_deref(), Some("Main"));
    }

    #[test]
    fn shift_negative_clamps_start_at_zero() {
        if !tools_available() {
            return;
        }
        let Some(fixture) = make_fixture("mp4") else {
            return;
        };
        let chapters: Vec<(i64, i64, &str)> = two_chapters();
        let with_chapters = chapters_set(&fixture, &chapters, &EngineLimits::default()).unwrap();
        // Shift -500ms: chapter 0 [0,1000) → clamped start 0, end 500;
        // chapter 1 [1000,2000) → [500,1500).
        let shifted = chapters_shift(&with_chapters, -500, &EngineLimits::default()).unwrap();
        let listed = chapters_list(&shifted).unwrap();
        assert_eq!(listed.len(), 2);
        assert!(listed[0].start_ms >= 0);
        assert_times(&listed, &[(0, 500), (500, 1500)], 10);
    }

    #[test]
    fn set_rejects_unsorted_overlapping_empty() {
        if !tools_available() {
            return;
        }
        let Some(fixture) = make_fixture("mp4") else {
            return;
        };
        // Overlapping.
        assert!(matches!(
            chapters_set(
                &fixture,
                &[(0, 1500, "a"), (1000, 2000, "b")],
                &EngineLimits::default()
            ),
            Err(VideoError::InvalidInput(_))
        ));
        // Unsorted.
        assert!(matches!(
            chapters_set(
                &fixture,
                &[(1000, 2000, "b"), (0, 1000, "a")],
                &EngineLimits::default()
            ),
            Err(VideoError::InvalidInput(_))
        ));
        // Empty chapter (end <= start).
        assert!(matches!(
            chapters_set(&fixture, &[(0, 0, "a")], &EngineLimits::default()),
            Err(VideoError::InvalidInput(_))
        ));
        // Negative start.
        assert!(matches!(
            chapters_set(&fixture, &[(-1, 100, "a")], &EngineLimits::default()),
            Err(VideoError::InvalidInput(_))
        ));
        // Empty table → use chapters_remove instead.
        assert!(matches!(
            chapters_set(&fixture, &[], &EngineLimits::default()),
            Err(VideoError::InvalidInput(_))
        ));
        // Title with newline.
        assert!(matches!(
            chapters_set(&fixture, &[(0, 100, "a\nb")], &EngineLimits::default()),
            Err(VideoError::InvalidInput(_))
        ));
    }

    #[test]
    fn list_rejects_garbage_input() {
        if !tools_available() {
            return;
        }
        let garbage = b"this is definitely not a media file".to_vec();
        assert!(matches!(
            chapters_list(&garbage),
            Err(VideoError::InvalidInput(_))
        ));
    }

    #[test]
    fn time_to_ms_parses_probe_and_hhmmss_forms() {
        // ffprobe decimal-seconds string.
        assert_eq!(time_to_ms(&Value::from("1.500000")), Some(1500));
        // HH:MM:SS.mmm form.
        assert_eq!(time_to_ms(&Value::from("00:01:30.250")), Some(90_250));
        // Bare JSON number (seconds).
        assert_eq!(time_to_ms(&Value::from(2.0)), Some(2000));
        // Garbage → None.
        assert_eq!(time_to_ms(&Value::from("nope")), None);
    }
}
