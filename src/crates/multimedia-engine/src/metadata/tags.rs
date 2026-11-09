//! Container/stream metadata (tag) editing.
//!
//! [`edit_tags`] re-muxes the input with `ffmpeg -c copy` while adding
//! or deleting metadata tags — no re-encoding, so it is fast and
//! lossless for the media streams. [`read_tags`] runs `ffprobe
//! -show_format -show_streams -of json` and folds the result into a
//! [`TagReport`].
//!
//! # ffmpeg tag syntax
//!
//! Format (container) tags map to `-metadata key=value`; stream tags
//! map to `-metadata:s:<index> key=value` where `<index>` is the
//! stream index as reported by ffprobe/ffmpeg. Passing an empty value
//! (`key=`) deletes the tag. Tag keys are passed verbatim — ffmpeg
//! treats them case-insensitively for well-known keys but preserves
//! case for unknown ones, so callers should pick one casing and use
//! it consistently.
//!
//! # Common tag keys
//!
//! | Key       | Meaning                              |
//! |-----------|--------------------------------------|
//! | `title`   | Track/clip title                     |
//! | `artist`  | Artist/performer                     |
//! | `album`   | Album or collection name             |
//! | `genre`   | Genre                                |
//! | `date`    | Release/recording date (`YYYY`/`YYYY-MM-DD`) |
//! | `comment` | Free-form comment                    |
//! | `encoder` | Writing-application tag (often set automatically by ffmpeg) |
//!
//! Container support varies: MP4/MOV honour `title`, `artist`,
//! `album`, `genre`, `date`, `comment`, `encoder` (stored as `©nam`,
//! `©ART`, ...); Matroska accepts arbitrary keys; MP3 uses ID3v2
//! frames (`TIT2`, `TPE1`, ... mapped from the same names).
//! Unsupported keys are silently ignored by some muxers rather than
//! failing — [`read_tags`] after editing is the reliable check.

use std::collections::BTreeMap;
use std::ffi::OsString;

use serde::{Deserialize, Serialize};

use crate::crates::multimedia_engine::error::{MediaError, MediaResult};
use crate::crates::multimedia_engine::model::{MediaLimits, MediaOutput};
use crate::crates::multimedia_engine::probe::types::ProbeReport;
use crate::crates::multimedia_engine::runtime::discovery::Runtime;
use crate::crates::multimedia_engine::runtime::process;
use crate::crates::multimedia_engine::runtime::tempdir::TempDir;

/// One metadata edit: set `key` to `value`, or delete it when
/// `value` is `None`.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct TagEdit {
    /// Tag key (`"title"`, `"artist"`, `"album"`, `"genre"`, `"date"`,
    /// `"comment"`, `"encoder"`, or any container-specific key).
    /// Must be non-empty and must not contain `=` or control
    /// characters.
    pub key: String,
    /// New tag value. `None` deletes the tag (emitted as
    /// `-metadata key=`); `Some("")` is treated the same way because
    /// ffmpeg interprets an empty value as deletion.
    pub value: Option<String>,
}

/// Options for [`edit_tags`].
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct TagOptions {
    /// Container/format-level tag edits, applied with
    /// `-metadata key=value`.
    pub format_tags: Vec<TagEdit>,
    /// Per-stream tag edits: `(stream_index, edit)` pairs applied
    /// with `-metadata:s:<stream_index> key=value`. The index is the
    /// ffmpeg/ffprobe stream index (as in [`TagReport::streams`]).
    pub stream_tags: Vec<(u32, TagEdit)>,
}

/// Read-back view of all tags in a media file.
#[derive(Debug, Clone, Default, Serialize)]
pub struct TagReport {
    /// Format/container tags (`title`, `artist`, `encoder`, ...).
    pub format: BTreeMap<String, String>,
    /// Per-stream tags, as `(stream_index, tags)` pairs in stream
    /// order. Streams with no tags appear with an empty map.
    pub streams: Vec<(u32, BTreeMap<String, String>)>,
}

/// Validate a tag key: non-empty, no `=` (it would corrupt the
/// `key=value` argument), no control characters or NUL.
fn check_key(key: &str) -> MediaResult<()> {
    if key.is_empty() || key.contains('=') || key.chars().any(|c| c.is_control()) {
        return Err(MediaError::InvalidInput(format!(
            "invalid tag key {key:?}: must be non-empty and contain no '=' or control characters"
        )));
    }
    Ok(())
}

/// Build the `key=value` argument for one [`TagEdit`]. `None` and
/// `Some("")` both yield `key=` — ffmpeg treats an empty value as a
/// deletion.
fn tag_arg(edit: &TagEdit) -> String {
    match &edit.value {
        Some(v) if !v.is_empty() => format!("{}={v}", edit.key),
        _ => format!("{}=", edit.key),
    }
}

/// Best-effort container sniffing from magic bytes, used only to pick
/// a plausible temp-file extension for the input and output (the
/// output keeps the input's container because we re-mux in place).
/// ffmpeg re-probes the content itself; a wrong guess is harmless.
fn sniff_extension(input: &[u8]) -> &'static str {
    if input.len() >= 4 {
        match &input[..4] {
            [0x1A, 0x45, 0xDF, 0xA3] => return "mkv", // EBML
            [b'O', b'g', b'g', b'S'] => return "ogg",
            [b'f', b'L', b'a', b'C'] => return "flac",
            [b'R', b'I', b'F', b'F'] => {
                if input.len() >= 12 && &input[8..12] == b"WAVE" {
                    return "wav";
                }
                if input.len() >= 12 && &input[8..12] == b"AVI " {
                    return "avi";
                }
                return "bin";
            }
            [b'I', b'D', b'3', _] => return "mp3",
            _ => {}
        }
        // ISO-BMFF: bytes 4..8 are "ftyp".
        if input.len() >= 8 && &input[4..8] == b"ftyp" {
            let brand = &input[8..12.min(input.len())];
            return match brand {
                b"qt  " => "mov",
                b"M4A " | b"M4B " => "m4a",
                _ => "mp4",
            };
        }
        if input.starts_with(b"\x89PNG") {
            return "png";
        }
        if input.starts_with(b"\xFF\xD8\xFF") {
            return "jpg";
        }
        if input.starts_with(b"GIF8") {
            return "gif";
        }
        if input.len() >= 12 && &input[8..12] == b"WEBP" {
            return "webp";
        }
        if input[0] == 0xFF && (input[1] & 0xE0) == 0xE0 {
            return "mp3";
        }
    }
    "bin"
}

/// Apply [`TagOptions`] to `input` and return the re-muxed media.
///
/// Runs `ffmpeg -map 0 -c copy` plus one `-metadata` / `-metadata:s:<idx>`
/// argument per [`TagEdit`]. Stream indices refer to the input stream
/// order; out-of-range indices make ffmpeg fail with
/// [`MediaError::ToolFailed`].
///
/// # Arguments
///
/// - `input` — complete media file bytes.
/// - `options` — see [`TagOptions`].
/// - `limits` — per-op byte/time limits.
/// - `rt` — the discovered FFmpeg [`Runtime`]; `rt.ffmpeg` is used.
///
/// # Errors
///
/// - [`MediaError::LimitExceeded`] when `input` exceeds
///   `limits.max_input_bytes` or the produced file exceeds
///   `limits.max_output_bytes`.
/// - [`MediaError::InvalidInput`] for malformed tag keys.
/// - [`MediaError::ToolFailed`] when ffmpeg cannot re-mux the input
///   (corrupt input, invalid stream index, ...).
pub fn edit_tags(
    input: &[u8],
    options: &TagOptions,
    limits: &MediaLimits,
    rt: &Runtime,
) -> MediaResult<MediaOutput> {
    if let Some(max) = limits.max_input_bytes
        && (input.len() as u64) > max
    {
        return Err(MediaError::LimitExceeded(format!(
            "input is {} bytes, exceeding max_input_bytes {max}",
            input.len()
        )));
    }

    // Validate all keys up front so a bad key cannot leave a
    // half-built command line.
    for edit in options
        .format_tags
        .iter()
        .chain(options.stream_tags.iter().map(|(_, e)| e))
    {
        check_key(&edit.key)?;
    }

    let ext = sniff_extension(input);
    let dir = TempDir::new("tags")?;
    let in_name = format!("input.{ext}");
    dir.write(&in_name, input)?;

    let out_name = format!("output.{ext}");
    let out_path = dir.file(&out_name);

    let mut argv: Vec<OsString> = vec![
        "-hide_banner".into(),
        "-loglevel".into(),
        "error".into(),
        "-nostdin".into(),
        "-y".into(),
        "-i".into(),
        dir.file(&in_name).into_os_string(),
        // Copy every stream so no stream is silently dropped by
        // ffmpeg's default stream selection.
        "-map".into(),
        "0".into(),
        "-c".into(),
        "copy".into(),
    ];

    for edit in &options.format_tags {
        argv.push("-metadata".into());
        argv.push(tag_arg(edit).into());
    }
    for (idx, edit) in &options.stream_tags {
        argv.push(format!("-metadata:s:{idx}").into());
        argv.push(tag_arg(edit).into());
    }

    argv.push(out_path.into_os_string());

    let out = process::run_checked("ffmpeg", &rt.ffmpeg, &argv, None, limits)?;

    let mut warnings = Vec::new();
    if !out.stderr.is_empty() {
        warnings.push(out.stderr);
    }

    let bytes = dir.read_limited(&out_name, limits.max_output_bytes)?;
    if bytes.is_empty() {
        return Err(MediaError::ToolFailed {
            tool: "ffmpeg",
            status: "empty output".to_string(),
            detail: "tag edit produced a zero-byte file".to_string(),
        });
    }

    let mut result = MediaOutput::single(out_name, bytes);
    result.warnings = warnings;
    Ok(result)
}

/// Read all container and per-stream tags from `input` via `ffprobe`.
///
/// Runs `ffprobe -v error -print_format json -show_format
/// -show_streams` and merges [`ProbeReport`]'s format tags and stream
/// tags into a [`TagReport`].
///
/// # Errors
///
/// - [`MediaError::MissingTool`] when `ffprobe` is not on `PATH`.
/// - [`MediaError::LimitExceeded`] when `input` exceeds
///   `limits.max_input_bytes`.
/// - [`MediaError::InvalidInput`] when ffprobe fails or its JSON
///   cannot be parsed.
pub fn read_tags(input: &[u8], limits: &MediaLimits, rt: &Runtime) -> MediaResult<TagReport> {
    if let Some(max) = limits.max_input_bytes
        && (input.len() as u64) > max
    {
        return Err(MediaError::LimitExceeded(format!(
            "input is {} bytes, exceeding max_input_bytes {max}",
            input.len()
        )));
    }

    let ext = sniff_extension(input);
    let dir = TempDir::new("tags-read")?;
    let in_name = format!("input.{ext}");
    let input_path = dir.write(&in_name, input)?;

    let args: Vec<OsString> = [
        "-v",
        "error",
        "-print_format",
        "json",
        "-show_format",
        "-show_streams",
    ]
    .iter()
    .map(OsString::from)
    .chain(std::iter::once(input_path.into_os_string()))
    .collect();

    let out = process::run("ffprobe", &rt.ffprobe, &args, None, limits)?;
    if !out.success || out.stdout.is_empty() {
        let detail = if out.stderr.is_empty() {
            "ffprobe produced no output".to_string()
        } else {
            out.stderr
        };
        return Err(MediaError::InvalidInput(detail));
    }

    let report: ProbeReport = serde_json::from_slice(&out.stdout)
        .map_err(|e| MediaError::InvalidInput(format!("ffprobe JSON parse failed: {e}")))?;

    let format = report
        .format
        .as_ref()
        .map(|f| f.tags.clone())
        .unwrap_or_default();

    let streams = report
        .streams
        .iter()
        .enumerate()
        .filter_map(|(pos, s)| {
            // ffprobe always emits `index`; fall back to the position
            // in `streams[]` for older/odd builds.
            let idx = s.index.unwrap_or(pos as u32);
            Some((idx, s.tags.clone()))
        })
        .collect();

    Ok(TagReport { format, streams })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::crates::multimedia_engine::runtime::tempdir::TempDir;

    /// Generate a tiny mp4 with ffmpeg lavfi: 1s of 64x64@10fps
    /// testsrc video + 440Hz sine audio. Skips gracefully when the
    /// needed encoders are unavailable.
    fn tiny_mp4() -> Option<Vec<u8>> {
        let rt = Runtime::discover().ok()?;
        let dir = TempDir::new("tags-test").ok()?;
        let out_path = dir.file("fixture.mp4");
        let args: Vec<OsString> = [
            "-hide_banner",
            "-loglevel",
            "error",
            "-nostdin",
            "-y",
            "-f",
            "lavfi",
            "-i",
            "testsrc=size=64x64:rate=10:duration=1",
            "-f",
            "lavfi",
            "-i",
            "sine=frequency=440:duration=1",
            "-c:v",
            "libx264",
            "-pix_fmt",
            "yuv420p",
            "-c:a",
            "aac",
            "-shortest",
            out_path.to_str()?,
        ]
        .iter()
        .map(OsString::from)
        .collect();
        let limits = MediaLimits::default();
        let out = process::run("ffmpeg", &rt.ffmpeg, &args, None, &limits).ok()?;
        if !out.success {
            return None;
        }
        std::fs::read(&out_path).ok()
    }

    fn set(title: &str, value: Option<&str>) -> TagOptions {
        TagOptions {
            format_tags: vec![TagEdit {
                key: title.to_string(),
                value: value.map(str::to_string),
            }],
            stream_tags: Vec::new(),
        }
    }

    #[test]
    fn edit_tags_sets_and_reads_format_title() {
        let Some(mp4) = tiny_mp4() else {
            eprintln!("skipping: ffmpeg lavfi/x264/aac unavailable");
            return;
        };
        let rt = Runtime::discover().unwrap();
        let limits = MediaLimits::default();

        let out = edit_tags(&mp4, &set("title", Some("Test Title")), &limits, &rt)
            .expect("edit_tags set title");
        let tagged = &out.primary.as_ref().unwrap().bytes;

        let report = read_tags(tagged, &limits, &rt).expect("read_tags");
        assert_eq!(
            report.format.get("title").map(String::as_str),
            Some("Test Title"),
            "format tags: {:?}",
            report.format
        );
    }

    #[test]
    fn edit_tags_deletes_format_tag() {
        let Some(mp4) = tiny_mp4() else {
            eprintln!("skipping: ffmpeg lavfi/x264/aac unavailable");
            return;
        };
        let rt = Runtime::discover().unwrap();
        let limits = MediaLimits::default();

        // First set a title, then delete it.
        let tagged = edit_tags(&mp4, &set("title", Some("Doomed")), &limits, &rt)
            .expect("set title")
            .primary
            .unwrap()
            .bytes;
        let deleted = edit_tags(&tagged, &set("title", None), &limits, &rt)
            .expect("delete title")
            .primary
            .unwrap()
            .bytes;

        let report = read_tags(&deleted, &limits, &rt).expect("read_tags");
        assert!(
            !report.format.contains_key("title"),
            "title should be deleted, got {:?}",
            report.format
        );
    }

    #[test]
    fn edit_tags_sets_stream_tag_and_rejects_bad_key() {
        let Some(mp4) = tiny_mp4() else {
            eprintln!("skipping: ffmpeg lavfi/x264/aac unavailable");
            return;
        };
        let rt = Runtime::discover().unwrap();
        let limits = MediaLimits::default();

        let opts = TagOptions {
            format_tags: Vec::new(),
            stream_tags: vec![(
                0,
                TagEdit {
                    key: "title".to_string(),
                    value: Some("Stream Zero".to_string()),
                },
            )],
        };
        let tagged = edit_tags(&mp4, &opts, &limits, &rt)
            .expect("set stream tag")
            .primary
            .unwrap()
            .bytes;

        let report = read_tags(&tagged, &limits, &rt).expect("read_tags");
        let stream0 = report
            .streams
            .iter()
            .find(|(idx, _)| *idx == 0)
            .expect("stream index 0 present");
        assert_eq!(
            stream0.1.get("title").map(String::as_str),
            Some("Stream Zero"),
            "stream 0 tags: {:?}",
            stream0.1
        );

        // A key containing '=' must be rejected before ffmpeg runs.
        let bad = TagOptions {
            format_tags: vec![TagEdit {
                key: "bad=key".to_string(),
                value: Some("x".to_string()),
            }],
            stream_tags: Vec::new(),
        };
        let err = edit_tags(&mp4, &bad, &limits, &rt).expect_err("bad key must fail");
        assert!(
            matches!(err, MediaError::InvalidInput(_)),
            "unexpected error: {err:?}"
        );
    }
}
