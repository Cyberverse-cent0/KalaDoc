//! Embedded-stream extraction: subtitles, attachments, cover art,
//! chapters, and data streams — pulled out of the container without
//! re-encoding where the format allows.

use std::collections::BTreeMap;

use crate::crates::core::engine::EngineLimits;
use crate::crates::video_engine::error::VideoResult;
use crate::crates::video_engine::model::{MediaOutput, StreamSelection};

/// Extract the stream kinds selected by `selection` into named
/// files. Subtitle streams are converted to `subtitle_format`
/// (`srt`, `ass`, or `vtt` — best effort per source codec). Chapters
/// export as `chapters.json`. Cover art exports as an image file
/// with a sniffed extension. Attachments keep their codec name.
///
/// Enforces `limits.max_outputs`; names are sanitized relative
/// paths safe for callers to write under a directory.
pub(crate) fn extract_streams(
    input: &[u8],
    selection: &StreamSelection,
    subtitle_format: &str,
    limits: &EngineLimits,
) -> VideoResult<MediaOutput> {
    let _ = (input, selection, subtitle_format, limits);
    Err(crate::crates::video_engine::error::VideoError::InvalidInput(
        "extract_streams not implemented".to_string(),
    ))
}

/// Export chapters as a JSON document matching the probe model
/// (`[{id, start_secs, end_secs, title}]`).
pub(crate) fn chapters_json(
    chapters: &[crate::crates::video_engine::model::ChapterInfo],
) -> VideoResult<Vec<u8>> {
    serde_json::to_vec_pretty(chapters).map_err(Into::into)
}

/// Best-effort subtitle codec → text format name table used by
/// callers to pick `subtitle_format` defaults.
pub(crate) fn subtitle_format_for(codec_name: &str) -> &'static str {
    match codec_name {
        "mov_text" | "tx3g" => "srt",
        "subrip" | "srt" => "srt",
        "ass" | "ssa" => "ass",
        "webvtt" => "vtt",
        _ => "srt",
    }
}

/// Sanitize a stream-derived filename component (codec names,
/// languages, indices) into `[a-z0-9_-]`.
pub(crate) fn sanitize_component(raw: &str) -> String {
    raw.chars()
        .map(|c| {
            if c.is_ascii_alphanumeric() {
                c.to_ascii_lowercase()
            } else {
                '_'
            }
        })
        .collect::<String>()
        .trim_matches('_')
        .to_string()
}

/// Merge helper: fold per-stream tag maps when building reports.
pub(crate) fn merge_tags(into: &mut BTreeMap<String, String>, tags: &BTreeMap<String, String>) {
    for (k, v) in tags {
        into.entry(k.clone()).or_insert_with(|| v.clone());
    }
}
