//! Export container chapter markers to sidecar formats.
//!
//! [`export_chapters`] probes the input with `ffprobe` (via
//! [`probe::chapters::chapters`]) and renders the chapter list as
//! one or more text files — no ffmpeg run needed for the export
//! itself:
//!
//! - **SRT** — SubRip cues, `HH:MM:SS,mmm` timestamps.
//! - **VTT** — WebVTT cues, `HH:MM:SS.mmm` timestamps.
//! - **JSON** — a pretty-printed array of chapter objects.
//!
//! Files are named `chapters.<ext>`; the first requested format's
//! file becomes the primary [`MediaFile`].

use serde::{Deserialize, Serialize};

use crate::crates::multimedia_engine::error::{MediaError, MediaResult};
use crate::crates::multimedia_engine::model::{MediaFile, MediaLimits, MediaOutput};
use crate::crates::multimedia_engine::probe::chapters::{ChapterInfo, chapters};

/// Sidecar format for [`export_chapters`].
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum ChapterExportFormat {
    /// SubRip `.srt`.
    Srt,
    /// WebVTT `.vtt`.
    Vtt,
    /// JSON array `.json`.
    Json,
}

impl ChapterExportFormat {
    /// Output file extension (no dot).
    fn extension(self) -> &'static str {
        match self {
            Self::Srt => "srt",
            Self::Vtt => "vtt",
            Self::Json => "json",
        }
    }

    /// MIME type for the produced file.
    fn content_type(self) -> &'static str {
        match self {
            Self::Srt => "application/x-subrip",
            Self::Vtt => "text/vtt",
            Self::Json => "application/json",
        }
    }

    /// Render `chapters` in this format.
    fn render(self, chapters: &[ChapterInfo]) -> String {
        match self {
            Self::Srt => render_srt(chapters),
            Self::Vtt => render_vtt(chapters),
            Self::Json => render_json(chapters),
        }
    }
}

/// Options for [`export_chapters`].
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ChapterExportOptions {
    /// Formats to produce. Must be non-empty; duplicates are
    /// ignored (first occurrence wins).
    #[serde(default = "default_formats")]
    pub formats: Vec<ChapterExportFormat>,
}

impl Default for ChapterExportOptions {
    fn default() -> Self {
        Self {
            formats: default_formats(),
        }
    }
}

fn default_formats() -> Vec<ChapterExportFormat> {
    vec![ChapterExportFormat::Srt]
}

/// Report returned by [`export_chapters`].
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct ChapterExportReport {
    /// The chapters that were exported (as probed from the input).
    pub chapters: Vec<ChapterInfo>,
    /// Produced sidecar files (`chapters.srt`, `chapters.vtt`,
    /// `chapters.json` — one per requested format).
    pub files: Vec<MediaFile>,
    /// Non-fatal diagnostics.
    pub warnings: Vec<String>,
}

impl ChapterExportReport {
    /// Convert into the shared [`MediaOutput`] shape.
    pub fn into_output(self) -> MediaOutput {
        let mut iter = self.files.into_iter();
        MediaOutput {
            primary: iter.next(),
            extra: iter.collect(),
            warnings: self.warnings,
        }
    }
}

/// Export `input`'s chapter markers to sidecar subtitle/JSON files.
///
/// Probes with `ffprobe -show_chapters -show_format`; no ffmpeg
/// invocation is needed since the sidecars are generated in-process.
///
/// # Errors
///
/// - [`MediaError::InvalidInput`] — empty `formats`, input has no
///   chapters, or ffprobe failed.
/// - [`MediaError::LimitExceeded`] — input exceeds
///   `max_input_bytes`, or generated output exceeds
///   `max_output_bytes` / `max_output_files`.
/// - [`MediaError::MissingTool`] — `ffprobe` not on `PATH`.
pub fn export_chapters(
    input: &[u8],
    options: &ChapterExportOptions,
    limits: &MediaLimits,
) -> MediaResult<ChapterExportReport> {
    if options.formats.is_empty() {
        return Err(MediaError::InvalidInput(
            "at least one export format is required".to_string(),
        ));
    }
    // De-duplicate while preserving request order.
    let mut formats: Vec<ChapterExportFormat> = Vec::new();
    for f in &options.formats {
        if !formats.contains(f) {
            formats.push(*f);
        }
    }
    if let Some(max_files) = limits.max_output_files
        && formats.len() as u64 > max_files as u64
    {
        return Err(MediaError::LimitExceeded(format!(
            "{} export formats exceed max_output_files {max_files}",
            formats.len()
        )));
    }

    let list = chapters(input, limits)?;
    if list.chapters.is_empty() {
        return Err(MediaError::InvalidInput(
            "input has no chapters to export".to_string(),
        ));
    }

    let mut files = Vec::with_capacity(formats.len());
    let mut total = 0u64;
    for format in formats {
        let text = format.render(&list.chapters);
        total += text.len() as u64;
        if let Some(max) = limits.max_output_bytes
            && total > max
        {
            return Err(MediaError::LimitExceeded(format!(
                "chapter exports total {total} bytes, exceeding max_output_bytes {max}"
            )));
        }
        files.push(MediaFile {
            name: format!("chapters.{}", format.extension()),
            bytes: text.into_bytes(),
            content_type: Some(format.content_type().to_string()),
        });
    }

    Ok(ChapterExportReport {
        chapters: list.chapters,
        files,
        warnings: Vec::new(),
    })
}

/// Chapter display text: title when present, else `Chapter N`.
fn chapter_text(ch: &ChapterInfo, pos: usize) -> String {
    ch.title
        .clone()
        .unwrap_or_else(|| format!("Chapter {}", pos + 1))
}

/// `HH:MM:SS,mmm` for SRT.
fn fmt_srt_time(secs: f64) -> String {
    fmt_time(secs, ',')
}

/// `HH:MM:SS.mmm` for WebVTT.
fn fmt_vtt_time(secs: f64) -> String {
    fmt_time(secs, '.')
}

/// Format seconds as `HH:MM:SS<sep>mmm` (hours unbounded).
fn fmt_time(secs: f64, sep: char) -> String {
    let ms = (secs.max(0.0) * 1000.0).round() as u64;
    let h = ms / 3_600_000;
    let m = (ms / 60_000) % 60;
    let s = (ms / 1000) % 60;
    let milli = ms % 1000;
    format!("{h:02}:{m:02}:{s:02}{sep}{milli:03}")
}

fn render_srt(chapters: &[ChapterInfo]) -> String {
    let mut out = String::new();
    for (i, ch) in chapters.iter().enumerate() {
        out.push_str(&format!("{}\n", i + 1));
        out.push_str(&format!(
            "{} --> {}\n",
            fmt_srt_time(ch.start_secs),
            fmt_srt_time(ch.end_secs)
        ));
        out.push_str(&chapter_text(ch, i));
        out.push_str("\n\n");
    }
    out
}

fn render_vtt(chapters: &[ChapterInfo]) -> String {
    let mut out = String::from("WEBVTT\n\n");
    for (i, ch) in chapters.iter().enumerate() {
        out.push_str(&format!(
            "{}\n{} --> {}\n{}\n\n",
            i + 1,
            fmt_vtt_time(ch.start_secs),
            fmt_vtt_time(ch.end_secs),
            chapter_text(ch, i)
        ));
    }
    out
}

fn render_json(chapters: &[ChapterInfo]) -> String {
    #[derive(Serialize)]
    struct JsonChapter {
        index: u32,
        start_secs: f64,
        end_secs: f64,
        duration_secs: f64,
        title: Option<String>,
    }
    let items: Vec<JsonChapter> = chapters
        .iter()
        .map(|c| JsonChapter {
            index: c.index,
            start_secs: c.start_secs,
            end_secs: c.end_secs,
            duration_secs: c.duration_secs,
            title: c.title.clone(),
        })
        .collect();
    // Serialization of plain data cannot fail; fall back to an
    // empty array defensively.
    serde_json::to_string_pretty(&items).unwrap_or_else(|_| "[]".to_string()) + "\n"
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::crates::multimedia_engine::runtime::discovery::Runtime;
    use crate::crates::multimedia_engine::runtime::process;
    use crate::crates::multimedia_engine::runtime::tempdir::TempDir;
    use std::path::PathBuf;

    fn limits() -> MediaLimits {
        MediaLimits::default()
    }

    fn sample_chapters() -> Vec<ChapterInfo> {
        vec![
            ChapterInfo {
                index: 0,
                start_secs: 0.0,
                end_secs: 1.5,
                duration_secs: 1.5,
                title: Some("Opening".to_string()),
            },
            ChapterInfo {
                index: 1,
                start_secs: 61.25,
                end_secs: 3661.5,
                duration_secs: 3600.25,
                title: None,
            },
        ]
    }

    #[test]
    fn srt_render_has_cues_and_comma_timestamps() {
        let text = ChapterExportFormat::Srt.render(&sample_chapters());
        assert!(text.starts_with("1\n00:00:00,000 --> 00:00:01,500\nOpening\n"));
        assert!(text.contains("2\n00:01:01,250 --> 01:01:01,500\nChapter 2\n"));
        // Blank line separates cues.
        assert!(text.contains("\n\n2\n"));
    }

    #[test]
    fn vtt_render_has_header_and_dot_timestamps() {
        let text = ChapterExportFormat::Vtt.render(&sample_chapters());
        assert!(text.starts_with("WEBVTT\n\n"));
        assert!(text.contains("00:00:00.000 --> 00:00:01.500"));
        assert!(text.contains("Chapter 2"));
    }

    #[test]
    fn json_render_is_array_of_objects() {
        let text = ChapterExportFormat::Json.render(&sample_chapters());
        let v: serde_json::Value = serde_json::from_str(&text).expect("valid json");
        let arr = v.as_array().expect("array");
        assert_eq!(arr.len(), 2);
        assert_eq!(arr[0]["title"], "Opening");
        assert_eq!(arr[0]["start_secs"], 0.0);
        assert_eq!(arr[1]["title"], serde_json::Value::Null);
        assert!((arr[1]["duration_secs"].as_f64().unwrap() - 3600.25).abs() < 0.001);
    }

    #[test]
    fn fmt_time_handles_rollover_and_hours() {
        assert_eq!(fmt_srt_time(0.0), "00:00:00,000");
        assert_eq!(fmt_srt_time(61.25), "00:01:01,250");
        assert_eq!(fmt_vtt_time(3661.5), "01:01:01.500");
        assert_eq!(fmt_srt_time(-5.0), "00:00:00,000");
    }

    #[test]
    fn export_rejects_empty_formats_and_chapterless_input() {
        let options = ChapterExportOptions { formats: vec![] };
        let err = export_chapters(&[0u8; 32], &options, &limits()).expect_err("must fail");
        assert!(matches!(err, MediaError::InvalidInput(_)), "{err:?}");

        // Garbage input → ffprobe fails → error (InvalidInput or a
        // ToolFailed probe failure depending on where it trips).
        let err = export_chapters(&[0xDEu8; 256], &ChapterExportOptions::default(), &limits())
            .expect_err("must fail");
        assert!(
            matches!(
                err,
                MediaError::InvalidInput(_)
                    | MediaError::MissingTool(_)
                    | MediaError::ToolFailed { .. }
            ),
            "unexpected error: {err:?}"
        );
    }

    /// 2s mpeg4 mp4 muxed with two chapters via ffmetadata.
    fn make_chaptered_mp4() -> Option<Vec<u8>> {
        let rt = Runtime::discover().ok()?;
        let dir = TempDir::new("chexport-in").ok()?;
        let base = dir.file("base.mp4");
        let argv = process::args(&[
            "-hide_banner",
            "-loglevel",
            "error",
            "-nostdin",
            "-y",
            "-f",
            "lavfi",
            "-i",
            "testsrc=duration=2:size=64x64:rate=10",
            "-c:v",
            "mpeg4",
            "-pix_fmt",
            "yuv420p",
            base.to_str()?,
        ]);
        process::run("ffmpeg", &rt.ffmpeg, &argv, None, &limits())
            .ok()?
            .success
            .then_some(())?;

        let meta = ";FFMETADATA1\n\
                    [CHAPTER]\nTIMEBASE=1/1000\nSTART=0\nEND=1000\ntitle=Opening\n\
                    [CHAPTER]\nTIMEBASE=1/1000\nSTART=1000\nEND=2000\ntitle=Closing\n";
        let meta_path = dir.file("meta.txt");
        std::fs::write(&meta_path, meta).ok()?;
        let out: PathBuf = dir.file("chaptered.mp4");
        let argv = process::args(&[
            "-hide_banner",
            "-loglevel",
            "error",
            "-nostdin",
            "-y",
            "-i",
            base.to_str()?,
            "-f",
            "ffmetadata",
            "-i",
            meta_path.to_str()?,
            "-map",
            "0",
            "-map_chapters",
            "1",
            "-c",
            "copy",
            out.to_str()?,
        ]);
        let res = process::run("ffmpeg", &rt.ffmpeg, &argv, None, &limits()).ok()?;
        if !res.success {
            return None;
        }
        std::fs::read(&out).ok()
    }

    #[test]
    fn export_produces_all_formats() {
        let Some(mp4) = make_chaptered_mp4() else {
            eprintln!("skipping: ffmpeg lavfi/mpeg4/ffmetadata unavailable");
            return;
        };
        let options = ChapterExportOptions {
            formats: vec![
                ChapterExportFormat::Srt,
                ChapterExportFormat::Vtt,
                ChapterExportFormat::Json,
                ChapterExportFormat::Srt, // duplicate ignored
            ],
        };
        let report = export_chapters(&mp4, &options, &limits()).expect("export_chapters");
        assert_eq!(report.chapters.len(), 2);
        assert_eq!(report.files.len(), 3);
        assert_eq!(report.files[0].name, "chapters.srt");
        assert_eq!(report.files[1].name, "chapters.vtt");
        assert_eq!(report.files[2].name, "chapters.json");

        let srt = String::from_utf8_lossy(&report.files[0].bytes);
        assert!(srt.contains("Opening"), "srt:\n{srt}");
        let vtt = String::from_utf8_lossy(&report.files[1].bytes);
        assert!(vtt.starts_with("WEBVTT"), "vtt:\n{vtt}");
        let json: serde_json::Value =
            serde_json::from_slice(&report.files[2].bytes).expect("json parses");
        assert_eq!(json.as_array().unwrap().len(), 2);
    }
}
