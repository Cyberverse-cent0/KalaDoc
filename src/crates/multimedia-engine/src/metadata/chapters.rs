//! Chapter-marker editing for media containers.
//!
//! [`chapters`] writes chapter markers into a media file's metadata
//! without re-encoding any stream. It builds an `ffmetadata` document
//! (`;FFMETADATA1` + `[CHAPTER]` sections on a `TIMEBASE=1/1000`
//! clock) and remuxes with `ffmpeg -map_chapters 1 -c copy`.
//!
//! # Add vs. Replace
//!
//! `-map_chapters 1` *replaces* the output's chapter list with the
//! ffmetadata file's — it does not merge. So [`ChapterMode::Add`]
//! first reads the input's existing chapters via ffprobe
//! (`-show_chapters`), merges them with the requested ones, sorts by
//! start time, drops exact duplicates and chapters fully contained in
//! an earlier same-titled chapter, and writes the merged set.
//! [`ChapterMode::Replace`] skips the read and writes only the
//! requested chapters.
//!
//! # Caveats
//!
//! - Chapter support is container-dependent: mp4/mov/mkv/ogg/flac
//!   accept chapters; raw streams and some formats do not. A muxer
//!   that cannot store chapters fails at ffmpeg level
//!   ([`MediaError::ToolFailed`]) — there is no cheap capability bit
//!   for "muxer supports chapters".
//! - Chapter times are milliseconds here, but ffmpeg/container time
//!   bases may quantize them (mp4 uses the movie timescale); the
//!   values round-trip within a few ms.
//! - Chapters past the media duration are kept as-is; players
//!   typically clamp them to the file end.
//!
//! This module is distinct from `container::chapters_edit` (a
//! replace/clear-only op): [`chapters`] is the add-with-merge path
//! for files that may have no chapters at all.

use std::ffi::OsString;
use std::path::PathBuf;

use serde::{Deserialize, Serialize};

use crate::crates::multimedia_engine::error::{MediaError, MediaResult};
use crate::crates::multimedia_engine::model::{MediaLimits, MediaOutput};
use crate::crates::multimedia_engine::probe::types::ProbeReport;
use crate::crates::multimedia_engine::runtime::discovery::Runtime;
use crate::crates::multimedia_engine::runtime::process;
use crate::crates::multimedia_engine::runtime::tempdir::TempDir;

/// One chapter marker to write.
///
/// Times are in **milliseconds** from the start of the media.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ChapterSpec {
    /// Chapter start, milliseconds. Must be `< end_ms`.
    pub start_ms: u64,
    /// Chapter end, milliseconds. Must be `> start_ms`.
    pub end_ms: u64,
    /// Chapter title. An empty/whitespace-only title defaults to
    /// `"Chapter N"` where N is the 1-based position in
    /// [`ChaptersOptions::chapters`]. Newlines and control
    /// characters are replaced with spaces (ffmetadata is a
    /// line-based format).
    pub title: String,
}

/// Whether to merge with existing chapters or replace them.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum ChapterMode {
    /// Keep existing chapters, append the new ones, sort by start.
    #[default]
    Add,
    /// Drop all existing chapters; write only `options.chapters`.
    Replace,
}

/// Options for [`chapters`].
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct ChaptersOptions {
    /// Chapters to write. Must be non-empty; each entry must satisfy
    /// `start_ms < end_ms`.
    #[serde(default)]
    pub chapters: Vec<ChapterSpec>,
    /// Output container extension/muxer name (e.g. `"mp4"`, `"mkv"`).
    /// `None` keeps the input's sniffed container. Changing the
    /// container re-muxes into it — streams are still copied, but
    /// codec support in the target container is ffmpeg's problem.
    pub output_format: Option<String>,
    /// Add (merge) or Replace existing chapters. Defaults to
    /// [`ChapterMode::Add`].
    #[serde(default)]
    pub mode: ChapterMode,
}

/// Internal normalized chapter in milliseconds.
#[derive(Debug, Clone, PartialEq, Eq)]
struct ChapterMs {
    start_ms: u64,
    end_ms: u64,
    title: String,
}

/// Best-effort container sniff for temp file extensions and the
/// default output container. Same magic-byte table as the other
/// metadata ops; ffmpeg re-probes content itself.
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
        if input.len() >= 8 && &input[4..8] == b"ftyp" {
            return "mp4";
        }
    }
    "bin"
}

/// Extension → ffmpeg muxer name. Most extensions are their own
/// muxer; `m4a` files mux through the `ipod` muxer.
fn muxer_for(ext: &str) -> &str {
    match ext {
        "m4a" => "ipod",
        other => other,
    }
}

/// Sanitize a title for ffmetadata: control characters (including
/// newlines) become spaces, then trim and collapse runs of spaces.
/// Returns `None` when nothing usable remains.
fn sanitize_title(raw: &str) -> Option<String> {
    let mut out = String::with_capacity(raw.len());
    let mut last_space = true; // trim leading whitespace for free
    for c in raw.chars() {
        if c.is_control() {
            if !last_space {
                out.push(' ');
                last_space = true;
            }
        } else {
            out.push(c);
            last_space = c.is_whitespace();
            if c.is_whitespace() {
                // collapse: only keep one whitespace char
                if out.ends_with("  ") {
                    out.pop();
                }
            }
        }
    }
    let trimmed = out.trim();
    if trimmed.is_empty() {
        None
    } else {
        Some(trimmed.to_string())
    }
}

/// Validate and normalize the requested [`ChapterSpec`]s, applying
/// the `"Chapter N"` default titles. `specs` positions are 1-based.
fn normalize_specs(options: &ChaptersOptions) -> MediaResult<Vec<ChapterMs>> {
    if options.chapters.is_empty() {
        return Err(MediaError::InvalidInput(
            "chapters list is empty; nothing to write".to_string(),
        ));
    }
    options
        .chapters
        .iter()
        .enumerate()
        .map(|(i, spec)| {
            if spec.start_ms >= spec.end_ms {
                return Err(MediaError::InvalidInput(format!(
                    "chapter {} has start_ms {} >= end_ms {}",
                    i + 1,
                    spec.start_ms,
                    spec.end_ms
                )));
            }
            let title = sanitize_title(&spec.title).unwrap_or_else(|| format!("Chapter {}", i + 1));
            Ok(ChapterMs {
                start_ms: spec.start_ms,
                end_ms: spec.end_ms,
                title,
            })
        })
        .collect()
}

/// Reject inputs over `max_input_bytes`.
fn check_input_size(input: &[u8], limits: &MediaLimits) -> MediaResult<()> {
    if let Some(max) = limits.max_input_bytes
        && (input.len() as u64) > max
    {
        return Err(MediaError::LimitExceeded(format!(
            "input is {} bytes, exceeding max_input_bytes {max}",
            input.len()
        )));
    }
    Ok(())
}

/// Read existing chapters from `path` via `ffprobe -show_chapters`.
/// Returns an empty vec for chapterless or unparsable containers —
/// a probe failure here should not block writing new chapters, so
/// parse/ffprobe errors propagate only as far as ffprobe itself
/// failing (non-zero exit); empty chapters are fine.
fn probe_existing_chapters(
    path: &PathBuf,
    limits: &MediaLimits,
    rt: &Runtime,
) -> MediaResult<Vec<ChapterMs>> {
    let args: Vec<OsString> = process::args(&[
        "-v",
        "error",
        "-print_format",
        "json",
        "-show_chapters",
        "-show_format",
        path.to_str()
            .ok_or_else(|| MediaError::InvalidInput("non-UTF-8 temp path".to_string()))?,
    ]);
    let out = process::run_checked("ffprobe", &rt.ffprobe, &args, None, limits)?;
    let report: ProbeReport = serde_json::from_slice(&out.stdout)
        .map_err(|e| MediaError::InvalidInput(format!("ffprobe JSON parse failed: {e}")))?;

    let mut chapters = Vec::with_capacity(report.chapters.len());
    for ch in &report.chapters {
        let (Some(start), Some(end)) = (ch.start_secs(), ch.end_secs()) else {
            continue;
        };
        let start_ms = (start * 1000.0).round().max(0.0) as u64;
        let end_ms = (end * 1000.0).round().max(0.0) as u64;
        if end_ms <= start_ms {
            continue;
        }
        let title = ch
            .title()
            .and_then(sanitize_title)
            .unwrap_or_else(|| "Chapter".to_string());
        chapters.push(ChapterMs {
            start_ms,
            end_ms,
            title,
        });
    }
    Ok(chapters)
}

/// Merge `existing` and `new` chapters: sort by start, remove exact
/// duplicates, and drop any chapter fully contained inside an
/// earlier chapter with the same title (a minimal overlap dedupe —
/// genuinely nested chapters with different titles are kept).
fn merge_chapters(existing: Vec<ChapterMs>, new: Vec<ChapterMs>) -> Vec<ChapterMs> {
    let mut all = existing;
    all.extend(new);
    all.sort_by(|a, b| a.start_ms.cmp(&b.start_ms).then(a.end_ms.cmp(&b.end_ms)));

    let mut kept: Vec<ChapterMs> = Vec::with_capacity(all.len());
    for ch in all {
        if kept
            .iter()
            .any(|k| k.start_ms == ch.start_ms && k.end_ms == ch.end_ms && k.title == ch.title)
        {
            continue; // exact duplicate
        }
        // Fully contained in a previous same-titled chapter → redundant.
        if kept
            .iter()
            .any(|k| k.title == ch.title && k.start_ms <= ch.start_ms && ch.end_ms <= k.end_ms)
        {
            continue;
        }
        kept.push(ch);
    }
    kept
}

/// Render chapters as an `ffmetadata` document on a 1/1000 timebase.
fn render_ffmetadata(chapters: &[ChapterMs]) -> String {
    let mut out = String::from(";FFMETADATA1\n");
    for ch in chapters {
        out.push_str("[CHAPTER]\n");
        out.push_str("TIMEBASE=1/1000\n");
        out.push_str(&format!("START={}\n", ch.start_ms));
        out.push_str(&format!("END={}\n", ch.end_ms));
        out.push_str(&format!("title={}\n", ch.title));
    }
    out
}

/// Write chapter markers into `input`'s container metadata.
///
/// Runs `ffmpeg -i input -f ffmetadata -i meta.ffmeta -map 0
/// -map_chapters 1 -c copy out.<ext>` — a stream-copy remux, so no
/// re-encoding. In [`ChapterMode::Add`] (the default) existing
/// chapters are probed first and merged with the requested ones;
/// [`ChapterMode::Replace`] writes only the requested chapters.
///
/// # Arguments
///
/// - `input` — complete media file bytes.
/// - `options` — see [`ChaptersOptions`].
/// - `limits` — per-op byte/time limits.
/// - `rt` — the discovered FFmpeg [`Runtime`].
///
/// # Errors
///
/// - [`MediaError::InvalidInput`] for an empty chapter list,
///   `start_ms >= end_ms`, or unparseable ffprobe output.
/// - [`MediaError::Unsupported`] when this FFmpeg build lacks the
///   output muxer.
/// - [`MediaError::LimitExceeded`] / [`MediaError::ToolFailed`] /
///   [`MediaError::MissingTool`] / [`MediaError::Timeout`] /
///   [`MediaError::Io`] as usual.
pub fn chapters(
    input: &[u8],
    options: &ChaptersOptions,
    limits: &MediaLimits,
    rt: &Runtime,
) -> MediaResult<MediaOutput> {
    check_input_size(input, limits)?;
    let new_chapters = normalize_specs(options)?;

    // Output container: explicit override wins, else keep input's.
    let ext = options
        .output_format
        .as_deref()
        .map(str::trim)
        .filter(|s| !s.is_empty())
        .map(str::to_string)
        .unwrap_or_else(|| sniff_extension(input).to_string());
    let muxer = muxer_for(&ext);

    let caps = rt.capabilities()?;
    if !caps.has_muxer(muxer) {
        return Err(MediaError::Unsupported(format!(
            "container {ext:?} (muxer {muxer:?}) is not available in this FFmpeg build"
        )));
    }

    let dir = TempDir::new("chapters-write")?;
    let in_ext = sniff_extension(input);
    let in_name = format!("input.{in_ext}");
    let in_path = dir.write(&in_name, input)?;

    // For Add mode, read the input's existing chapters so
    // `-map_chapters 1` doesn't silently drop them.
    let existing = match options.mode {
        ChapterMode::Add => probe_existing_chapters(&in_path, limits, rt)?,
        ChapterMode::Replace => Vec::new(),
    };
    let final_chapters = merge_chapters(existing, new_chapters);
    let meta = render_ffmetadata(&final_chapters);

    let meta_name = "chapters.ffmeta";
    dir.write(meta_name, meta.as_bytes())?;

    let out_name = format!("output.{ext}");
    let out_path = dir.file(&out_name);

    let mut argv: Vec<OsString> = ["-hide_banner", "-loglevel", "error", "-nostdin", "-y"]
        .iter()
        .map(OsString::from)
        .collect();
    argv.push("-i".into());
    argv.push(in_path.into_os_string());
    argv.push("-f".into());
    argv.push("ffmetadata".into());
    argv.push("-i".into());
    argv.push(dir.file(meta_name).into_os_string());
    // Copy every input stream; take chapters from input 1 (the
    // ffmetadata document). `-map_metadata 0` keeps the original
    // container tags — the ffmeta carries only [CHAPTER] sections.
    argv.extend(
        [
            "-map",
            "0",
            "-map_chapters",
            "1",
            "-map_metadata",
            "0",
            "-c",
            "copy",
            "-f",
            muxer,
        ]
        .iter()
        .map(OsString::from),
    );
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
            detail: "chapter write produced a zero-byte file".to_string(),
        });
    }

    let mut result = MediaOutput::single(out_name, bytes);
    result.warnings = warnings;
    Ok(result)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::crates::multimedia_engine::probe::chapters as probe_chapters;

    /// Generate a tiny mp4 (2s, mpeg4 video — always compiled in)
    /// and return its bytes. `None` when ffmpeg/lavfi is missing.
    fn tiny_mp4() -> Option<Vec<u8>> {
        let rt = Runtime::discover().ok()?;
        let dir = TempDir::new("chapters-test").ok()?;
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
            "testsrc=size=64x64:rate=10:duration=2",
            "-c:v",
            "mpeg4",
            "-pix_fmt",
            "yuv420p",
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
        std::fs::read(&out_path).ok().filter(|b| !b.is_empty())
    }

    fn spec(start_ms: u64, end_ms: u64, title: &str) -> ChapterSpec {
        ChapterSpec {
            start_ms,
            end_ms,
            title: title.to_string(),
        }
    }

    #[test]
    fn add_chapters_to_mp4_and_probe_them() {
        let Some(mp4) = tiny_mp4() else {
            eprintln!("skipping: ffmpeg lavfi/mpeg4 unavailable");
            return;
        };
        let rt = Runtime::discover().expect("ffmpeg/ffprobe on PATH");
        let limits = MediaLimits::default();

        let opts = ChaptersOptions {
            chapters: vec![spec(0, 1000, "Intro"), spec(1000, 2000, "Main")],
            output_format: None,
            mode: ChapterMode::Add,
        };
        let out = chapters(&mp4, &opts, &limits, &rt).expect("write chapters");
        let bytes = &out.primary.as_ref().expect("primary").bytes;
        assert_eq!(out.primary.as_ref().unwrap().name, "output.mp4");

        let list = probe_chapters::chapters(bytes, &limits).expect("probe chapters");
        assert!(
            list.chapters.len() >= 2,
            "expected >=2 chapters, got {:?}",
            list.chapters
        );
        let titles: Vec<&str> = list
            .chapters
            .iter()
            .filter_map(|c| c.title.as_deref())
            .collect();
        assert!(titles.contains(&"Intro"), "titles: {titles:?}");
        assert!(titles.contains(&"Main"), "titles: {titles:?}");
    }

    #[test]
    fn replace_mode_drops_existing_chapters() {
        let Some(mp4) = tiny_mp4() else {
            eprintln!("skipping: ffmpeg lavfi/mpeg4 unavailable");
            return;
        };
        let rt = Runtime::discover().expect("ffmpeg/ffprobe on PATH");
        let limits = MediaLimits::default();

        // First write two chapters, then replace with one.
        let opts = ChaptersOptions {
            chapters: vec![spec(0, 1000, "A"), spec(1000, 2000, "B")],
            output_format: None,
            mode: ChapterMode::Add,
        };
        let first = chapters(&mp4, &opts, &limits, &rt)
            .expect("first write")
            .primary
            .unwrap()
            .bytes;

        let replace = ChaptersOptions {
            chapters: vec![spec(500, 1500, "Only")],
            output_format: None,
            mode: ChapterMode::Replace,
        };
        let second = chapters(&first, &replace, &limits, &rt)
            .expect("replace write")
            .primary
            .unwrap()
            .bytes;

        let list = probe_chapters::chapters(&second, &limits).expect("probe");
        assert_eq!(
            list.chapters.len(),
            1,
            "replace should leave exactly one chapter: {:?}",
            list.chapters
        );
        assert_eq!(list.chapters[0].title.as_deref(), Some("Only"));
    }

    #[test]
    fn add_mode_merges_with_existing() {
        let Some(mp4) = tiny_mp4() else {
            eprintln!("skipping: ffmpeg lavfi/mpeg4 unavailable");
            return;
        };
        let rt = Runtime::discover().expect("ffmpeg/ffprobe on PATH");
        let limits = MediaLimits::default();

        let first_opts = ChaptersOptions {
            chapters: vec![spec(0, 1000, "First")],
            output_format: None,
            mode: ChapterMode::Add,
        };
        let first = chapters(&mp4, &first_opts, &limits, &rt)
            .expect("first write")
            .primary
            .unwrap()
            .bytes;

        // Add a disjoint second chapter — Add mode must keep both.
        let second_opts = ChaptersOptions {
            chapters: vec![spec(1000, 2000, "Second")],
            output_format: None,
            mode: ChapterMode::Add,
        };
        let second = chapters(&first, &second_opts, &limits, &rt)
            .expect("second write")
            .primary
            .unwrap()
            .bytes;

        let list = probe_chapters::chapters(&second, &limits).expect("probe");
        assert!(
            list.chapters.len() >= 2,
            "merged chapters missing: {:?}",
            list.chapters
        );
        let titles: Vec<&str> = list
            .chapters
            .iter()
            .filter_map(|c| c.title.as_deref())
            .collect();
        assert!(titles.contains(&"First"), "titles: {titles:?}");
        assert!(titles.contains(&"Second"), "titles: {titles:?}");
    }

    #[test]
    fn rejects_invalid_specs() {
        let rt = Runtime::discover().expect("ffmpeg/ffprobe on PATH");
        let limits = MediaLimits::default();
        let Some(mp4) = tiny_mp4() else {
            eprintln!("skipping: ffmpeg lavfi/mpeg4 unavailable");
            return;
        };

        // start >= end
        let bad = ChaptersOptions {
            chapters: vec![spec(1000, 1000, "X")],
            output_format: None,
            mode: ChapterMode::Add,
        };
        let err = chapters(&mp4, &bad, &limits, &rt).expect_err("bad range");
        assert!(matches!(err, MediaError::InvalidInput(_)));

        // empty list
        let empty = ChaptersOptions {
            chapters: Vec::new(),
            output_format: None,
            mode: ChapterMode::Add,
        };
        let err = chapters(&mp4, &empty, &limits, &rt).expect_err("empty list");
        assert!(matches!(err, MediaError::InvalidInput(_)));
    }

    #[test]
    fn sanitize_title_defaults_and_strips_newlines() {
        let opts = ChaptersOptions {
            chapters: vec![
                spec(0, 500, "  \n\t "),
                spec(500, 1000, "Line1\nLine2\r\nEnd"),
            ],
            output_format: None,
            mode: ChapterMode::Add,
        };
        let normalized = normalize_specs(&opts).expect("normalize");
        assert_eq!(normalized[0].title, "Chapter 1");
        assert_eq!(normalized[1].title, "Line1 Line2 End");
    }
}
