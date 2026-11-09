//! Chapter inspection via `ffprobe -show_chapters -show_format`.
//!
//! Reads chapter markers (and the container's `format_name`) out of a
//! media file. A container with no chapters is not an error — the
//! returned [`ChapterList`] simply has an empty `chapters` vec.

use std::ffi::OsString;
use std::path::PathBuf;

use serde::Serialize;

use crate::crates::multimedia_engine::error::{MediaError, MediaResult};
use crate::crates::multimedia_engine::model::MediaLimits;
use crate::crates::multimedia_engine::probe::types::ProbeReport;
use crate::crates::multimedia_engine::runtime::{discovery, process, tempdir::TempDir};

/// Chapter list extracted from a media container.
#[derive(Debug, Clone, Default, Serialize)]
pub struct ChapterList {
    /// Chapters in file order.
    pub chapters: Vec<ChapterInfo>,
    /// Container `format_name` as reported by ffprobe
    /// (`"mov,mp4,m4a,3gp,3g2,mj2"`), when available.
    pub format_name: Option<String>,
}

/// One chapter marker.
#[derive(Debug, Clone, Serialize)]
pub struct ChapterInfo {
    /// Chapter index (`id` from ffprobe, falling back to position).
    pub index: u32,
    /// Start time in seconds.
    pub start_secs: f64,
    /// End time in seconds.
    pub end_secs: f64,
    /// Duration in seconds (`end_secs - start_secs`).
    pub duration_secs: f64,
    /// Chapter title tag, when present.
    pub title: Option<String>,
}

/// Probe `input` for chapter markers.
///
/// `input` is written to a temp file because ffprobe needs a real
/// path; the probe runs with `-show_chapters -show_format -of json`
/// and is parsed through [`ProbeReport`]. Containers without
/// chapters yield `Ok(ChapterList { chapters: vec![], .. })`.
pub fn chapters(input: &[u8], limits: &MediaLimits) -> MediaResult<ChapterList> {
    if let Some(max) = limits.max_input_bytes
        && input.len() as u64 > max
    {
        return Err(MediaError::LimitExceeded(format!(
            "input is {} bytes, exceeding max_input_bytes {max}",
            input.len()
        )));
    }

    let ffprobe = discovery::find_tool("ffprobe")
        .ok_or_else(|| MediaError::MissingTool("ffprobe".to_string()))?;

    let dir = TempDir::new("chapters")?;
    // ffprobe probes by content; a neutral name avoids extension-based
    // demuxer pinning while still giving a stable suffix.
    let input_path = write_input(&dir, input)?;

    let argv: Vec<OsString> = process::args(&[
        "-v",
        "error",
        "-show_chapters",
        "-show_format",
        "-of",
        "json",
        input_path.to_str().unwrap_or("input.bin"),
    ]);
    let out = process::run_checked("ffprobe", &ffprobe, &argv, None, limits)?;

    let report: ProbeReport = serde_json::from_slice(&out.stdout)
        .map_err(|e| MediaError::InvalidInput(format!("ffprobe JSON parse failed: {e}")))?;

    let format_name = report.format.as_ref().and_then(|f| f.format_name.clone());

    let chapters = report
        .chapters
        .iter()
        .enumerate()
        .map(|(pos, ch)| {
            let start = ch.start_secs().unwrap_or(0.0);
            let end = ch.end_secs().unwrap_or(start);
            ChapterInfo {
                index: ch.id.unwrap_or(pos as u32),
                start_secs: start,
                end_secs: end,
                duration_secs: (end - start).max(0.0),
                title: ch.title().map(str::to_string),
            }
        })
        .collect();

    Ok(ChapterList {
        chapters,
        format_name,
    })
}

/// Write `input` into the temp dir under a fixed name and return its
/// path. The `.bin` suffix keeps ffprobe format-agnostic — it scores
/// demuxers by probe data, not extension.
fn write_input(dir: &TempDir, input: &[u8]) -> MediaResult<PathBuf> {
    dir.write("input.bin", input)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::process::Command;

    fn ffmpeg(args: &[&str]) -> std::process::Output {
        Command::new("ffmpeg")
            .args(["-hide_banner", "-y"])
            .args(args)
            .output()
            .expect("ffmpeg must run")
    }

    /// Build a small mp4 (2s, mpeg4 video — always compiled in) and
    /// return its bytes.
    fn make_base_mp4() -> Vec<u8> {
        let dir = tempfile::tempdir().expect("tempdir");
        let out = dir.path().join("base.mp4");
        let res = ffmpeg(&[
            "-f",
            "lavfi",
            "-i",
            "testsrc=duration=2:size=64x64:rate=10",
            "-c:v",
            "mpeg4",
            "-pix_fmt",
            "yuv420p",
            out.to_str().unwrap(),
        ]);
        assert!(
            res.status.success(),
            "ffmpeg base failed: {}",
            String::from_utf8_lossy(&res.stderr)
        );
        std::fs::read(&out).expect("read base mp4")
    }

    /// Mux `base` with the given ffmetadata text as chapters and
    /// return the resulting mp4 bytes.
    fn mux_chapters(base: &[u8], meta: &str) -> Vec<u8> {
        let dir = tempfile::tempdir().expect("tempdir");
        let base_path = dir.path().join("in.mp4");
        let meta_path = dir.path().join("meta.txt");
        let out_path = dir.path().join("out.mp4");
        std::fs::write(&base_path, base).unwrap();
        std::fs::write(&meta_path, meta).unwrap();
        let res = ffmpeg(&[
            "-i",
            base_path.to_str().unwrap(),
            "-i",
            meta_path.to_str().unwrap(),
            "-map_metadata",
            "1",
            "-c",
            "copy",
            out_path.to_str().unwrap(),
        ]);
        assert!(
            res.status.success(),
            "ffmpeg mux failed: {}",
            String::from_utf8_lossy(&res.stderr)
        );
        std::fs::read(&out_path).expect("read chaptered mp4")
    }

    const META: &str = ";FFMETADATA1\n\
         title=Test Movie\n\
         [CHAPTER]\n\
         TIMEBASE=1/1000\n\
         START=0\n\
         END=1000\n\
         title=Opening\n\
         [CHAPTER]\n\
         TIMEBASE=1/1000\n\
         START=1000\n\
         END=2000\n\
         title=Closing\n";

    #[test]
    fn chapters_listed_with_titles() {
        let base = make_base_mp4();
        let data = mux_chapters(&base, META);
        let list = chapters(&data, &MediaLimits::default()).expect("probe chapters");

        assert_eq!(list.chapters.len(), 2);
        assert_eq!(list.chapters[0].title.as_deref(), Some("Opening"));
        assert_eq!(list.chapters[0].start_secs, 0.0);
        assert!((list.chapters[0].end_secs - 1.0).abs() < 0.01);
        assert!((list.chapters[0].duration_secs - 1.0).abs() < 0.01);
        assert_eq!(list.chapters[1].title.as_deref(), Some("Closing"));
        assert!((list.chapters[1].start_secs - 1.0).abs() < 0.01);
        assert!((list.chapters[1].end_secs - 2.0).abs() < 0.01);
        assert!(list.format_name.is_some());
    }

    #[test]
    fn no_chapters_is_ok_empty() {
        let base = make_base_mp4();
        let list = chapters(&base, &MediaLimits::default()).expect("probe no chapters");
        assert!(list.chapters.is_empty());
        assert!(list.format_name.is_some());
    }

    #[test]
    fn garbage_input_errors_not_panics() {
        let garbage = vec![0xABu8; 4096];
        // ffprobe may still "succeed" on unrecognized data with an
        // empty report; either way it must not panic.
        let _ = chapters(&garbage, &MediaLimits::default());
    }
}
