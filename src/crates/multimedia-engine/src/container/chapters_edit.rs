//! Container chapter editing via ffmpeg's `ffmetadata` demuxer.
//!
//! [`chapters_edit`] remuxes `input` with `-c copy` (no re-encode)
//! while replacing or removing its chapter markers:
//!
//! - [`ChaptersEditMode::Replace`] writes a `;FFMETADATA1` file
//!   containing one `[CHAPTER]` block per [`ChapterEdit`]
//!   (`TIMEBASE=1/1000`, millisecond `START`/`END`, `title=`) and runs
//!   `ffmpeg -i input -i chapters.ffmeta -map 0 -map_chapters 1
//!   -map_metadata 0 -c copy out.<ext>`.
//! - [`ChaptersEditMode::Clear`] runs the same remux with
//!   `-map_chapters -1` and no metadata input, dropping every chapter.
//!
//! Chapter times are milliseconds in the `1/1000` ffmetadata
//! timebase. Entries are validated (`start_ms < end_ms`), sorted by
//! start time, and titles are sanitized to single lines before being
//! written.
//!
//! The only capability gates are the output container's muxer (which
//! must exist in this FFmpeg build) and — for `Replace` — the
//! `ffmetadata` demuxer. Muxers that do not store chapter markers
//! (e.g. raw audio/video elementary streams) produce a warning rather
//! than an error: the remux still succeeds, chapters are just lost.

use std::ffi::OsString;

use serde::{Deserialize, Serialize};

use crate::crates::multimedia_engine::error::{MediaError, MediaResult};
use crate::crates::multimedia_engine::model::{MediaLimits, MediaOutput};
use crate::crates::multimedia_engine::runtime::discovery::Runtime;
use crate::crates::multimedia_engine::runtime::{process, tempdir::TempDir};

/// Sanity bound on the number of chapters a single edit may write —
/// well above any real-world use, guards against pathological inputs.
const MAX_CHAPTERS: usize = 10_000;

/// What [`chapters_edit`] should do with the input's chapters.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum ChaptersEditMode {
    /// Replace every existing chapter with the supplied list.
    /// Default.
    #[default]
    Replace,
    /// Remove all chapters. `chapters` must be empty.
    Clear,
}

/// One chapter to write. Times are milliseconds relative to the start
/// of the media, in the ffmetadata `1/1000` timebase.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ChapterEdit {
    /// Chapter start in milliseconds. Must be `< end_ms`.
    pub start_ms: u64,
    /// Chapter end in milliseconds. Must be `> start_ms`.
    pub end_ms: u64,
    /// Chapter title shown by players. Newlines/control characters
    /// are sanitized to spaces; empty means untitled.
    #[serde(default)]
    pub title: String,
}

/// Options for [`chapters_edit`].
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct ChaptersEditOptions {
    /// Replace or clear. Defaults to [`ChaptersEditMode::Replace`].
    #[serde(default)]
    pub mode: ChaptersEditMode,
    /// Chapters to write in [`ChaptersEditMode::Replace`]. Must be
    /// non-empty; entries are sorted by `start_ms`. Must be empty in
    /// [`ChaptersEditMode::Clear`].
    #[serde(default)]
    pub chapters: Vec<ChapterEdit>,
    /// Output container extension without a leading dot (`"mp4"`,
    /// `"mkv"`, ...). `None` keeps the input's sniffed container.
    /// Stream copy cannot freely change containers — a different
    /// extension may fail inside ffmpeg when a codec is incompatible
    /// with the new muxer.
    #[serde(default)]
    pub output_format: Option<String>,
}

/// Modify `input`'s container chapters and remux.
///
/// All streams are preserved (`-map 0 -c copy`); only the chapter
/// table changes. Container-level metadata is kept from the input
/// (`-map_metadata 0`), and the ffmetadata sidecar contributes only
/// chapters — its global tags are never imported.
///
/// Returns a single-file [`MediaOutput`] named `output.<ext>`.
///
/// # Errors
///
/// - [`MediaError::InvalidInput`] — empty input, `Replace` with an
///   empty/invalid chapter list (`start_ms >= end_ms`, too many
///   chapters), `Clear` with a non-empty list, or a bad
///   `output_format`.
/// - [`MediaError::LimitExceeded`] — input/output byte caps.
/// - [`MediaError::Unsupported`] — output muxer (or the `ffmetadata`
///   demuxer, for `Replace`) missing from this FFmpeg build.
/// - [`MediaError::ToolFailed`] — ffmpeg rejected the remux.
pub fn chapters_edit(
    input: &[u8],
    options: &ChaptersEditOptions,
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

    // Validate and normalize the chapter list up front so a bad
    // request fails before ffmpeg is spawned.
    let chapters = match options.mode {
        ChaptersEditMode::Replace => {
            if options.chapters.is_empty() {
                return Err(MediaError::InvalidInput(
                    "replace mode requires at least one chapter (use mode=clear to remove all)"
                        .to_string(),
                ));
            }
            if options.chapters.len() > MAX_CHAPTERS {
                return Err(MediaError::InvalidInput(format!(
                    "{} chapters exceeds the maximum of {MAX_CHAPTERS}",
                    options.chapters.len()
                )));
            }
            let mut v: Vec<ChapterEdit> = options
                .chapters
                .iter()
                .map(|c| {
                    if c.start_ms >= c.end_ms {
                        return Err(MediaError::InvalidInput(format!(
                            "chapter start_ms {} must be < end_ms {}",
                            c.start_ms, c.end_ms
                        )));
                    }
                    Ok(ChapterEdit {
                        start_ms: c.start_ms,
                        end_ms: c.end_ms,
                        title: sanitize_title(&c.title),
                    })
                })
                .collect::<MediaResult<Vec<_>>>()?;
            // Chapters must be in ascending order for sane output.
            v.sort_by_key(|c| (c.start_ms, c.end_ms));
            v
        }
        ChaptersEditMode::Clear => {
            if !options.chapters.is_empty() {
                return Err(MediaError::InvalidInput(
                    "clear mode does not accept a chapter list".to_string(),
                ));
            }
            Vec::new()
        }
    };

    let caps = rt.capabilities()?;
    let output_ext = resolve_output_ext(input, options.output_format.as_deref())?;
    let muxer = muxer_name_for(&output_ext);
    if !caps.has_muxer(muxer) {
        return Err(MediaError::Unsupported(format!(
            "muxer '{muxer}' (for .{output_ext}) is not available in this FFmpeg build"
        )));
    }
    if options.mode == ChaptersEditMode::Replace && !caps.has_demuxer("ffmetadata") {
        return Err(MediaError::Unsupported(
            "ffmetadata demuxer is not available in this FFmpeg build".to_string(),
        ));
    }

    let dir = TempDir::new("chapters-edit")?;
    let input_path = dir.write(&format!("input.{}", sniff_extension(input)), input)?;

    let output_name = format!("output.{output_ext}");
    let output_path = dir.file(&output_name);

    let mut argv: Vec<OsString> = vec![
        OsString::from("-hide_banner"),
        OsString::from("-loglevel"),
        OsString::from("error"),
        OsString::from("-nostdin"),
        OsString::from("-y"),
        OsString::from("-i"),
        input_path.into_os_string(),
    ];

    let mut warnings = Vec::new();
    match options.mode {
        ChaptersEditMode::Replace => {
            // ffmetadata chapters are exposed on input index 1.
            let meta_name = "chapters.ffmeta";
            dir.write(meta_name, render_ffmetadata(&chapters).as_bytes())?;
            let meta_path = dir.file(meta_name);
            argv.push(OsString::from("-i"));
            argv.push(meta_path.into_os_string());
            argv.push(OsString::from("-map"));
            argv.push(OsString::from("0"));
            argv.push(OsString::from("-map_chapters"));
            argv.push(OsString::from("1"));
            argv.push(OsString::from("-map_metadata"));
            argv.push(OsString::from("0"));
        }
        ChaptersEditMode::Clear => {
            argv.push(OsString::from("-map"));
            argv.push(OsString::from("0"));
            argv.push(OsString::from("-map_chapters"));
            argv.push(OsString::from("-1"));
            argv.push(OsString::from("-map_metadata"));
            argv.push(OsString::from("0"));
        }
    }
    argv.push(OsString::from("-c"));
    argv.push(OsString::from("copy"));
    argv.push(output_path.into_os_string());

    process::run_checked("ffmpeg", &rt.ffmpeg, &argv, None, limits)?;

    let bytes = dir.read_limited(&output_name, limits.max_output_bytes)?;
    if bytes.is_empty() {
        return Err(MediaError::ToolFailed {
            tool: "ffmpeg",
            status: "empty output".to_string(),
            detail: format!("ffmpeg produced an empty {output_name}"),
        });
    }

    if options.mode == ChaptersEditMode::Replace {
        // Overlapping ranges confuse some players; ffmpeg accepts
        // them but we surface it.
        for pair in chapters.windows(2) {
            if pair[0].end_ms > pair[1].start_ms {
                warnings.push(format!(
                    "chapter '{}' ({}-{}ms) overlaps the next chapter",
                    pair[0].title, pair[0].start_ms, pair[0].end_ms
                ));
                break;
            }
        }
    }
    if !muxer_supports_chapters(&output_ext) {
        warnings.push(format!(
            "muxer '{muxer}' (.{output_ext}) may not store chapter markers; \
             chapters could be silently dropped by players"
        ));
    }

    let mut out = MediaOutput::single(output_name, bytes);
    if let Some(f) = out.primary.as_mut() {
        f.content_type = content_type_for(&output_ext);
    }
    out.warnings = warnings;
    Ok(out)
}

/// Render the chapter list as an `ffmetadata` document: a
/// `;FFMETADATA1` header followed by one `[CHAPTER]` block each,
/// `TIMEBASE=1/1000` with millisecond `START`/`END` and a sanitized
/// `title=`. Input must already be sorted — sorting happens during
/// validation.
fn render_ffmetadata(chapters: &[ChapterEdit]) -> String {
    let mut out = String::from(";FFMETADATA1\n");
    for ch in chapters {
        out.push_str("[CHAPTER]\nTIMEBASE=1/1000\n");
        out.push_str(&format!("START={}\n", ch.start_ms));
        out.push_str(&format!("END={}\n", ch.end_ms));
        out.push_str(&format!("title={}\n", ch.title));
    }
    out
}

/// Chapter titles go verbatim into a line-oriented key=value file:
/// newlines and other control characters become spaces, surrounding
/// whitespace is trimmed. Backslashes are left as-is — ffmetadata
/// values do not need escaping for the characters we emit.
fn sanitize_title(title: &str) -> String {
    title
        .chars()
        .map(|c| if c.is_control() { ' ' } else { c })
        .collect::<String>()
        .split_whitespace()
        .collect::<Vec<_>>()
        .join(" ")
}

/// Resolve the output container extension (same rules as
/// [`set_language`](super::set_language)).
fn resolve_output_ext(input: &[u8], output_format: Option<&str>) -> MediaResult<String> {
    if let Some(fmt) = output_format {
        let fmt = fmt.trim().trim_start_matches('.').to_ascii_lowercase();
        if fmt.is_empty() || !fmt.chars().all(|c| c.is_ascii_alphanumeric()) {
            return Err(MediaError::InvalidInput(format!(
                "invalid output_format {fmt:?}"
            )));
        }
        return Ok(fmt);
    }
    Ok(sniff_container(input).to_string())
}

fn muxer_name_for(ext: &str) -> &str {
    match ext {
        "mkv" => "matroska",
        "m4a" => "ipod",
        "ts" | "mts" | "m2ts" => "mpegts",
        "mpga" | "oga" => "ogg",
        other => other,
    }
}

/// Whether the muxer for `ext` is known to store chapter markers.
/// Anything outside this list gets a warning rather than an error —
/// ffmpeg's behavior varies by build and some muxers accept but drop
/// chapters silently.
fn muxer_supports_chapters(ext: &str) -> bool {
    matches!(
        ext,
        // QuickTime/MP4 family — chapters become a text/chapters track.
        "mp4" | "m4v" | "m4a" | "mov" | "qt" | "3gp" | "3g2" | "3gpp"
            // Matroska family.
            | "mkv" | "webm"
            // Ogg/Opus containers.
            | "ogg" | "oga" | "ogv" | "opus"
            // ASF/WMV.
            | "asf" | "wmv"
            // NUT stores chapters natively.
            | "nut"
    )
}

fn sniff_container(data: &[u8]) -> &'static str {
    if data.len() >= 12 && &data[4..8] == b"ftyp" {
        return if &data[8..12] == b"qt  " {
            "mov"
        } else {
            "mp4"
        };
    }
    if data.starts_with(&[0x1A, 0x45, 0xDF, 0xA3]) {
        return "mkv";
    }
    if data.starts_with(b"RIFF") && data.len() >= 12 {
        return match &data[8..12] {
            b"WAVE" => "wav",
            b"AVI " => "avi",
            _ => "mp4",
        };
    }
    if data.starts_with(b"OggS") {
        return "ogg";
    }
    if data.starts_with(b"fLaC") {
        return "flac";
    }
    if data.starts_with(b"ID3") || (data.len() >= 2 && data[0] == 0xFF && (data[1] & 0xE0) == 0xE0)
    {
        return "mp3";
    }
    "mp4"
}

fn sniff_extension(data: &[u8]) -> &'static str {
    if data.len() >= 12 && &data[4..8] == b"ftyp" {
        return if &data[8..12] == b"qt  " {
            "mov"
        } else {
            "mp4"
        };
    }
    if data.starts_with(&[0x1A, 0x45, 0xDF, 0xA3]) {
        return "mkv";
    }
    if data.starts_with(b"RIFF") && data.len() >= 12 {
        return match &data[8..12] {
            b"WAVE" => "wav",
            b"AVI " => "avi",
            _ => "bin",
        };
    }
    if data.starts_with(b"OggS") {
        return "ogg";
    }
    if data.starts_with(b"fLaC") {
        return "flac";
    }
    if data.starts_with(b"ID3") || (data.len() >= 2 && data[0] == 0xFF && (data[1] & 0xE0) == 0xE0)
    {
        return "mp3";
    }
    "bin"
}

fn content_type_for(ext: &str) -> Option<String> {
    let mime = match ext {
        "mp4" | "m4v" | "m4a" => "video/mp4",
        "mkv" => "video/x-matroska",
        "webm" => "video/webm",
        "mov" => "video/quicktime",
        "avi" => "video/x-msvideo",
        "mp3" => "audio/mpeg",
        "aac" | "adts" => "audio/aac",
        "wav" => "audio/wav",
        "ogg" | "oga" | "opus" => "audio/ogg",
        "flac" => "audio/flac",
        "ts" | "mts" | "m2ts" => "video/mp2t",
        _ => return None,
    };
    Some(mime.to_string())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::crates::multimedia_engine::probe::chapters::chapters as probe_chapters;
    use std::process::Command;

    fn ffmpeg(args: &[&str]) -> std::process::Output {
        Command::new("ffmpeg")
            .args(["-hide_banner", "-y"])
            .args(args)
            .output()
            .expect("ffmpeg must run")
    }

    /// 2-second mp4, mpeg4 video — always compiled in.
    fn make_base_mp4() -> Option<Vec<u8>> {
        let dir = tempfile::tempdir().ok()?;
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
            out.to_str()?,
        ]);
        if !res.status.success() {
            return None;
        }
        std::fs::read(&out).ok()
    }

    /// Mux `base` with the given ffmetadata text as chapters.
    fn mux_chapters(base: &[u8], meta: &str) -> Option<Vec<u8>> {
        let dir = tempfile::tempdir().ok()?;
        let base_path = dir.path().join("in.mp4");
        let meta_path = dir.path().join("meta.txt");
        let out_path = dir.path().join("out.mp4");
        std::fs::write(&base_path, base).ok()?;
        std::fs::write(&meta_path, meta).ok()?;
        let res = ffmpeg(&[
            "-i",
            base_path.to_str()?,
            "-i",
            meta_path.to_str()?,
            "-map_metadata",
            "1",
            "-map_chapters",
            "1",
            "-c",
            "copy",
            out_path.to_str()?,
        ]);
        if !res.status.success() {
            return None;
        }
        std::fs::read(&out_path).ok()
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

    fn runtime() -> Option<Runtime> {
        Runtime::discover().ok()
    }

    fn limits() -> MediaLimits {
        MediaLimits::default()
    }

    fn replace_options() -> ChaptersEditOptions {
        ChaptersEditOptions {
            mode: ChaptersEditMode::Replace,
            chapters: vec![
                ChapterEdit {
                    start_ms: 1000,
                    end_ms: 2000,
                    title: "Second".to_string(),
                },
                // Deliberately out of order — the op must sort.
                ChapterEdit {
                    start_ms: 0,
                    end_ms: 1000,
                    title: "First\nline".to_string(),
                },
            ],
            output_format: None,
        }
    }

    #[test]
    fn replace_writes_sorted_sanitized_chapters() {
        let Some(rt) = runtime() else {
            eprintln!("skipping: ffmpeg/ffprobe not on PATH");
            return;
        };
        let Some(base) = make_base_mp4() else {
            eprintln!("skipping: lavfi/mpeg4 unavailable");
            return;
        };
        let out = chapters_edit(&base, &replace_options(), &limits(), &rt)
            .expect("chapters_edit replace");
        let primary = out.primary.expect("primary");
        assert_eq!(primary.name, "output.mp4");
        assert_eq!(primary.content_type.as_deref(), Some("video/mp4"));

        let list = probe_chapters(&primary.bytes, &limits()).expect("probe output");
        assert_eq!(list.chapters.len(), 2, "chapters: {:?}", list.chapters);
        // Sorted by start: the 0-1000 chapter comes first.
        assert!((list.chapters[0].start_secs - 0.0).abs() < 0.01);
        assert!((list.chapters[0].end_secs - 1.0).abs() < 0.01);
        // Newline in title sanitized to a space.
        assert_eq!(list.chapters[0].title.as_deref(), Some("First line"));
        assert_eq!(list.chapters[1].title.as_deref(), Some("Second"));
        assert!((list.chapters[1].start_secs - 1.0).abs() < 0.01);
    }

    #[test]
    fn clear_removes_all_chapters() {
        let Some(rt) = runtime() else {
            eprintln!("skipping: ffmpeg/ffprobe not on PATH");
            return;
        };
        let Some(base) = make_base_mp4() else {
            eprintln!("skipping: lavfi/mpeg4 unavailable");
            return;
        };
        let Some(chaptered) = mux_chapters(&base, META) else {
            eprintln!("skipping: chapter fixture build failed");
            return;
        };
        // Sanity: fixture really has chapters.
        let before = probe_chapters(&chaptered, &limits()).expect("probe fixture");
        assert_eq!(before.chapters.len(), 2);

        let options = ChaptersEditOptions {
            mode: ChaptersEditMode::Clear,
            chapters: Vec::new(),
            output_format: None,
        };
        let out = chapters_edit(&chaptered, &options, &limits(), &rt).expect("chapters_edit clear");
        let primary = out.primary.expect("primary");
        let list = probe_chapters(&primary.bytes, &limits()).expect("probe cleared");
        assert!(list.chapters.is_empty(), "chapters: {:?}", list.chapters);
    }

    #[test]
    fn replace_over_existing_chapters() {
        let Some(rt) = runtime() else {
            eprintln!("skipping: ffmpeg/ffprobe not on PATH");
            return;
        };
        let Some(base) = make_base_mp4() else {
            eprintln!("skipping: lavfi/mpeg4 unavailable");
            return;
        };
        let Some(chaptered) = mux_chapters(&base, META) else {
            eprintln!("skipping: chapter fixture build failed");
            return;
        };
        // Replace the two fixture chapters with a single new one.
        let options = ChaptersEditOptions {
            mode: ChaptersEditMode::Replace,
            chapters: vec![ChapterEdit {
                start_ms: 500,
                end_ms: 1500,
                title: "Only".to_string(),
            }],
            output_format: None,
        };
        let out = chapters_edit(&chaptered, &options, &limits(), &rt)
            .expect("chapters_edit replace-over");
        let list = probe_chapters(&out.primary.expect("primary").bytes, &limits())
            .expect("probe replaced");
        assert_eq!(list.chapters.len(), 1);
        assert_eq!(list.chapters[0].title.as_deref(), Some("Only"));
    }

    #[test]
    fn validation_errors() {
        let Some(rt) = runtime() else {
            eprintln!("skipping: ffmpeg/ffprobe not on PATH");
            return;
        };
        let Some(base) = make_base_mp4() else {
            eprintln!("skipping: lavfi/mpeg4 unavailable");
            return;
        };

        // start >= end.
        let bad = ChaptersEditOptions {
            mode: ChaptersEditMode::Replace,
            chapters: vec![ChapterEdit {
                start_ms: 1000,
                end_ms: 1000,
                title: String::new(),
            }],
            output_format: None,
        };
        let err = chapters_edit(&base, &bad, &limits(), &rt).expect_err("must fail");
        assert!(matches!(err, MediaError::InvalidInput(_)), "{err:?}");

        // Replace with empty list.
        let empty = ChaptersEditOptions {
            mode: ChaptersEditMode::Replace,
            chapters: Vec::new(),
            output_format: None,
        };
        let err = chapters_edit(&base, &empty, &limits(), &rt).expect_err("must fail");
        assert!(matches!(err, MediaError::InvalidInput(_)), "{err:?}");

        // Clear with a non-empty list.
        let bad_clear = ChaptersEditOptions {
            mode: ChaptersEditMode::Clear,
            chapters: vec![ChapterEdit {
                start_ms: 0,
                end_ms: 1,
                title: String::new(),
            }],
            output_format: None,
        };
        let err = chapters_edit(&base, &bad_clear, &limits(), &rt).expect_err("must fail");
        assert!(matches!(err, MediaError::InvalidInput(_)), "{err:?}");
    }

    #[test]
    fn empty_input_is_invalid() {
        let Some(rt) = runtime() else {
            eprintln!("skipping: ffmpeg/ffprobe not on PATH");
            return;
        };
        let err = chapters_edit(b"", &ChaptersEditOptions::default(), &limits(), &rt)
            .expect_err("must fail");
        assert!(matches!(err, MediaError::InvalidInput(_)), "{err:?}");
    }

    #[test]
    fn render_ffmetadata_shape() {
        let meta = render_ffmetadata(&[
            ChapterEdit {
                start_ms: 0,
                end_ms: 1500,
                title: "Intro".to_string(),
            },
            ChapterEdit {
                start_ms: 1500,
                end_ms: 3000,
                title: String::new(),
            },
        ]);
        assert!(meta.starts_with(";FFMETADATA1\n"));
        assert_eq!(meta.matches("[CHAPTER]").count(), 2);
        assert!(meta.contains("TIMEBASE=1/1000"));
        assert!(meta.contains("START=0\nEND=1500\ntitle=Intro"));
        assert!(meta.contains("title=\n")); // empty title line
    }

    #[test]
    fn sanitize_title_strips_controls() {
        assert_eq!(sanitize_title("a\nb\rc\td"), "a b c d");
        assert_eq!(sanitize_title("  spaced  "), "spaced");
        assert_eq!(sanitize_title(""), "");
    }
}
