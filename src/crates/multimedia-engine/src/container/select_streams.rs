//! Select a subset of streams and remux them into a new container.
//!
//! [`select_streams`] probes the input with `ffprobe`, resolves each
//! [`StreamSelection`] (by stream kind, absolute stream index and/or
//! language tag) to concrete streams, then runs
//! `ffmpeg -i <in> -map 0:<idx> ... -c copy <out>` — a pure remux with
//! no re-encode. Chapters and global metadata are preserved by
//! default (`-map_chapters 0`, `-map_metadata 0`); both can be turned
//! off via [`SelectStreamsOptions`].
//!
//! Because no encoder is involved, the only capability gate is the
//! output container's muxer — checked *before* ffmpeg is spawned.

use std::collections::BTreeSet;
use std::ffi::OsString;
use std::path::Path;

use serde::{Deserialize, Serialize};

use crate::crates::multimedia_engine::error::{MediaError, MediaResult};
use crate::crates::multimedia_engine::model::{MediaFile, MediaLimits, MediaOutput};
use crate::crates::multimedia_engine::probe::types::{MediaStream, ProbeReport};
use crate::crates::multimedia_engine::runtime::discovery::Runtime;
use crate::crates::multimedia_engine::runtime::{process, tempdir::TempDir};

/// Stream kind used by [`StreamSelection`].
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum StreamKind {
    /// Any stream (video, audio, subtitle, data, attachment).
    Any,
    /// `codec_type == "video"` — includes attached pictures, which
    /// ffprobe reports as video streams with the `attached_pic`
    /// disposition. Use [`StreamKind::Attachment`] to target cover
    /// art specifically.
    Video,
    /// `codec_type == "audio"`.
    Audio,
    /// `codec_type == "subtitle"`.
    Subtitle,
    /// `codec_type == "data"`.
    Data,
    /// `codec_type == "attachment"` or any stream flagged
    /// `attached_pic` (cover art).
    Attachment,
}

impl Default for StreamKind {
    fn default() -> Self {
        Self::Any
    }
}

/// One stream selection rule. A probed stream matches when *all*
/// provided filters match: `kind` (when not [`StreamKind::Any`]),
/// `index` (absolute ffprobe stream index) and `language` (compared
/// case-insensitively against the stream's `language` field or its
/// `language` tag).
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct StreamSelection {
    /// Stream kind filter. Defaults to [`StreamKind::Any`].
    #[serde(default)]
    pub kind: StreamKind,
    /// Absolute stream index (`streams[].index` from ffprobe).
    /// `None` matches any index.
    #[serde(default)]
    pub index: Option<u32>,
    /// Language tag filter (`"eng"`, `"jpn"`, ...), matched
    /// case-insensitively. `None` matches any language.
    #[serde(default)]
    pub language: Option<String>,
    /// `false` (default): pick the single best match — the
    /// default-disposition stream when present, else the first in
    /// stream order. `true`: pick *every* match, capped by `limit`.
    #[serde(default)]
    pub all: bool,
    /// Maximum number of matches to keep when `all` is `true`.
    /// `None` keeps all matches. Ignored when `all` is `false`.
    #[serde(default)]
    pub limit: Option<u32>,
}

impl StreamSelection {
    /// Select every stream of every kind — the identity selection.
    fn all_streams() -> Self {
        Self {
            kind: StreamKind::Any,
            index: None,
            language: None,
            all: true,
            limit: None,
        }
    }
}

/// Options for [`select_streams`].
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct SelectStreamsOptions {
    /// Selection rules, applied in order. An empty list defaults to
    /// a single [`StreamKind::Any`] + `all` selection, i.e. every
    /// stream is copied (equivalent to a plain remux).
    #[serde(default)]
    pub selections: Vec<StreamSelection>,
    /// Output container extension without a leading dot
    /// (`"mp4"`, `"mkv"`, `"webm"`, ...). Becomes `output.<format>`.
    /// `None` keeps the input's sniffed container — stream copy
    /// cannot freely change containers, so an explicit different
    /// extension may fail inside ffmpeg when a codec is incompatible
    /// with the new muxer.
    #[serde(default)]
    pub output_format: Option<String>,
    /// Copy chapter markers from the input (default `true`).
    /// `false` adds `-map_chapters -1`.
    #[serde(default = "default_true")]
    pub copy_chapters: bool,
    /// Copy container-level metadata (default `true`). `false` adds
    /// `-map_metadata -1`.
    #[serde(default = "default_true")]
    pub copy_metadata: bool,
}

fn default_true() -> bool {
    true
}

/// Report returned by [`select_streams`].
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct SelectStreamsReport {
    /// Absolute stream indices that were mapped into the output, in
    /// ascending order.
    pub selected: Vec<u32>,
    /// Absolute stream indices present in the input but not mapped.
    pub skipped: Vec<u32>,
    /// Produced files (always exactly one for this op).
    pub files: Vec<MediaFile>,
    /// Non-fatal diagnostics.
    pub warnings: Vec<String>,
}

impl SelectStreamsReport {
    /// Convert into the shared [`MediaOutput`] shape: first file is
    /// the primary output, the rest are `extra`.
    pub fn into_output(self) -> MediaOutput {
        let mut iter = self.files.into_iter();
        MediaOutput {
            primary: iter.next(),
            extra: iter.collect(),
            warnings: self.warnings,
        }
    }
}

/// Select streams from `input` and remux them into a new container.
///
/// # Errors
///
/// - [`MediaError::InvalidInput`] — empty input, empty selection
///   result, or a selection matching no streams.
/// - [`MediaError::LimitExceeded`] — input exceeds
///   `max_input_bytes`, or the output exceeds `max_output_bytes`.
/// - [`MediaError::Unsupported`] — the output container's muxer is
///   not in this FFmpeg build.
/// - [`MediaError::ToolFailed`] — ffmpeg rejected the remux (e.g. a
///   stream codec the target container cannot hold).
pub fn select_streams(
    input: &[u8],
    options: &SelectStreamsOptions,
    limits: &MediaLimits,
    rt: &Runtime,
) -> MediaResult<SelectStreamsReport> {
    check_input_size(input, limits)?;

    // Capability gate before any probing/spawning: stream copy needs
    // no encoder, only the output muxer.
    let caps = rt.capabilities()?;
    let output_ext = resolve_output_ext(input, options.output_format.as_deref())?;
    let muxer = muxer_name_for(&output_ext);
    if !caps.has_muxer(muxer) {
        return Err(MediaError::Unsupported(format!(
            "muxer '{muxer}' (for .{output_ext}) is not available in this FFmpeg build"
        )));
    }

    let dir = TempDir::new("select-streams")?;
    let input_path = dir.write(&format!("input.{}", sniff_extension(input)), input)?;
    let streams = media_streams(&input_path, limits, rt)?;
    if streams.is_empty() {
        return Err(MediaError::InvalidInput(
            "input has no media streams to select from".to_string(),
        ));
    }

    let selections = if options.selections.is_empty() {
        vec![StreamSelection::all_streams()]
    } else {
        options.selections.clone()
    };

    // Resolve selections to absolute stream indices; a BTreeSet
    // dedupes overlapping selections and keeps map order stable.
    let mut selected: BTreeSet<u32> = BTreeSet::new();
    for sel in &selections {
        let matches = resolve_selection(&streams, sel);
        if matches.is_empty() {
            return Err(MediaError::InvalidInput(format!(
                "stream selection (kind={:?}, index={:?}, language={:?}) matched no streams",
                sel.kind, sel.index, sel.language
            )));
        }
        for s in matches {
            if let Some(idx) = s.index {
                selected.insert(idx);
            }
        }
    }
    if selected.is_empty() {
        return Err(MediaError::InvalidInput(
            "stream selection produced no mappable streams".to_string(),
        ));
    }

    let selected: Vec<u32> = selected.into_iter().collect();
    let selected_set: BTreeSet<u32> = selected.iter().copied().collect();
    let skipped: Vec<u32> = streams
        .iter()
        .filter_map(|s| s.index)
        .filter(|i| !selected_set.contains(i))
        .collect();

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
    for idx in &selected {
        argv.push(OsString::from("-map"));
        argv.push(OsString::from(format!("0:{idx}")));
    }
    argv.push(OsString::from("-c"));
    argv.push(OsString::from("copy"));
    argv.push(OsString::from("-map_chapters"));
    argv.push(OsString::from(if options.copy_chapters {
        "0"
    } else {
        "-1"
    }));
    argv.push(OsString::from("-map_metadata"));
    argv.push(OsString::from(if options.copy_metadata {
        "0"
    } else {
        "-1"
    }));
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

    let mut warnings = Vec::new();
    if !skipped.is_empty() {
        warnings.push(format!(
            "deselected {} stream(s): {}",
            skipped.len(),
            skipped
                .iter()
                .map(u32::to_string)
                .collect::<Vec<_>>()
                .join(",")
        ));
    }

    Ok(SelectStreamsReport {
        selected,
        skipped,
        files: vec![MediaFile {
            name: output_name,
            bytes,
            content_type: content_type_for(&output_ext),
        }],
        warnings,
    })
}

/// Whether `s` matches every filter set on `sel`.
fn selection_matches(sel: &StreamSelection, s: &MediaStream) -> bool {
    if !kind_matches(sel.kind, s) {
        return false;
    }
    if let Some(want) = sel.index
        && s.index != Some(want)
    {
        return false;
    }
    if let Some(want) = sel.language.as_deref() {
        let have = s
            .language
            .as_deref()
            .or_else(|| s.tags.get("language").map(String::as_str));
        match have {
            Some(have) if have.eq_ignore_ascii_case(want) => {}
            _ => return false,
        }
    }
    true
}

/// Resolve a selection to the concrete matching streams, in stream
/// order. Non-`all` selections return at most one stream: the
/// default-disposition match when present, else the first match.
pub(crate) fn resolve_selection<'a>(
    streams: &'a [MediaStream],
    sel: &StreamSelection,
) -> Vec<&'a MediaStream> {
    let matches: Vec<&MediaStream> = streams
        .iter()
        .filter(|s| selection_matches(sel, s))
        .collect();
    if !sel.all {
        return match matches.iter().position(|s| s.is_default()) {
            Some(pos) => vec![matches[pos]],
            None => matches.into_iter().take(1).collect(),
        };
    }
    match sel.limit {
        Some(limit) => matches.into_iter().take(limit as usize).collect(),
        None => matches,
    }
}

/// Whether `s`'s codec type (or attached-picture flag) satisfies
/// `kind`.
pub(crate) fn kind_matches(kind: StreamKind, s: &MediaStream) -> bool {
    match kind {
        StreamKind::Any => true,
        StreamKind::Video => s.kind() == Some("video"),
        StreamKind::Audio => s.kind() == Some("audio"),
        StreamKind::Subtitle => s.kind() == Some("subtitle"),
        StreamKind::Data => s.kind() == Some("data"),
        StreamKind::Attachment => s.kind() == Some("attachment") || is_attached_pic(s),
    }
}

/// Whether the stream carries the attached-picture (cover art)
/// disposition.
pub(crate) fn is_attached_pic(s: &MediaStream) -> bool {
    s.disposition_attached_pic == Some(true) || s.disposition.attached_pic.is_some_and(|v| v != 0)
}

/// Probe `input_path` with `ffprobe -show_streams` and return the
/// raw [`MediaStream`] list (keeps tags/dispositions that the
/// flattened `StreamDetail` drops).
pub(crate) fn media_streams(
    input_path: &Path,
    limits: &MediaLimits,
    rt: &Runtime,
) -> MediaResult<Vec<MediaStream>> {
    let args = process::args(&[
        "-v",
        "error",
        "-print_format",
        "json",
        "-show_streams",
        input_path.to_str().unwrap_or("input"),
    ]);
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
    Ok(report.streams)
}

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

/// Resolve the output container extension: explicit `output_format`
/// when given (validated), otherwise the input's sniffed container.
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

/// Map a file extension to the ffmpeg muxer name used for the
/// capability check. ffmpeg itself resolves the muxer from the output
/// filename; this map only translates well-known aliases so
/// `has_muxer` checks the right name (e.g. `.mkv` → `matroska`).
fn muxer_name_for(ext: &str) -> &str {
    match ext {
        "mkv" => "matroska",
        "m4a" => "ipod",
        "ts" | "mts" | "m2ts" => "mpegts",
        "mpga" | "oga" => "ogg",
        other => other,
    }
}

/// Best-effort container guess from magic bytes, used to pick a
/// default output extension for the remux.
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

/// Best-effort container extension for the temp *input* file —
/// ffprobe/ffmpeg re-probe content anyway; `.bin` fallback keeps the
/// demuxer guess content-driven.
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

/// Best-effort MIME type for the produced container.
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
    use crate::crates::multimedia_engine::runtime::discovery;
    use std::collections::BTreeMap;

    fn runtime() -> Runtime {
        Runtime::discover().expect("ffmpeg/ffprobe must be on PATH")
    }

    fn limits() -> MediaLimits {
        MediaLimits::default()
    }

    fn stream(index: u32, kind: &str, language: Option<&str>, is_default: bool) -> MediaStream {
        let mut s = MediaStream {
            index: Some(index),
            codec_type: Some(kind.to_string()),
            codec_name: Some(format!("{kind}_codec")),
            ..Default::default()
        };
        s.language = language.map(str::to_string);
        s.disposition.default = Some(if is_default { 1 } else { 0 });
        s
    }

    #[test]
    fn kind_matching_covers_all_kinds() {
        let v = stream(0, "video", None, true);
        let a = stream(1, "audio", Some("eng"), false);
        let d = stream(2, "data", None, false);
        assert!(kind_matches(StreamKind::Video, &v));
        assert!(!kind_matches(StreamKind::Audio, &v));
        assert!(kind_matches(StreamKind::Audio, &a));
        assert!(kind_matches(StreamKind::Data, &d));
        assert!(kind_matches(StreamKind::Any, &d));
        assert!(!kind_matches(StreamKind::Attachment, &v));
    }

    #[test]
    fn attachment_kind_matches_attached_pic() {
        let mut pic = stream(3, "video", None, false);
        pic.disposition.attached_pic = Some(1);
        assert!(kind_matches(StreamKind::Attachment, &pic));
        assert!(!kind_matches(
            StreamKind::Attachment,
            &stream(0, "video", None, false)
        ));
    }

    #[test]
    fn resolve_picks_default_then_first_and_all_with_limit() {
        let streams = vec![
            stream(0, "audio", Some("eng"), false),
            stream(1, "audio", Some("jpn"), true),
            stream(2, "audio", Some("fre"), false),
        ];
        // Non-all picks the default-flagged stream (index 1).
        let sel = StreamSelection {
            kind: StreamKind::Audio,
            ..Default::default()
        };
        let m = resolve_selection(&streams, &sel);
        assert_eq!(m.len(), 1);
        assert_eq!(m[0].index, Some(1));

        // Language filter + all + limit.
        let sel = StreamSelection {
            kind: StreamKind::Audio,
            index: None,
            language: Some("JPN".to_string()),
            all: true,
            limit: None,
        };
        let m = resolve_selection(&streams, &sel);
        assert_eq!(m.len(), 1);
        assert_eq!(m[0].index, Some(1));

        let sel = StreamSelection {
            kind: StreamKind::Audio,
            all: true,
            limit: Some(2),
            ..Default::default()
        };
        let m = resolve_selection(&streams, &sel);
        assert_eq!(m.len(), 2);
        assert_eq!(m[0].index, Some(0));
        assert_eq!(m[1].index, Some(1));

        // Index filter.
        let sel = StreamSelection {
            kind: StreamKind::Audio,
            index: Some(2),
            ..Default::default()
        };
        let m = resolve_selection(&streams, &sel);
        assert_eq!(m.len(), 1);
        assert_eq!(m[0].index, Some(2));
    }

    #[test]
    fn language_matches_via_tags_fallback() {
        let mut s = stream(0, "audio", None, false);
        s.tags = BTreeMap::from([("language".to_string(), "deu".to_string())]);
        let sel = StreamSelection {
            kind: StreamKind::Audio,
            language: Some("DEU".to_string()),
            ..Default::default()
        };
        assert_eq!(resolve_selection(&[s], &sel).len(), 1);
    }

    /// 1s 64x64 mpeg4 video + aac audio mp4 — mpeg4/aac are native
    /// encoders, so this works on builds without libx264.
    fn make_av_mp4() -> Option<Vec<u8>> {
        let ffmpeg = discovery::find_tool("ffmpeg")?;
        let dir = TempDir::new("select-test-in").ok()?;
        let out_path = dir.file("av.mp4");
        let argv = process::args(&[
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
            "mpeg4",
            "-pix_fmt",
            "yuv420p",
            "-c:a",
            "aac",
            "-shortest",
            out_path.to_str()?,
        ]);
        let out = process::run("ffmpeg", &ffmpeg, &argv, None, &limits()).ok()?;
        if !out.success {
            return None;
        }
        std::fs::read(&out_path).ok()
    }

    #[test]
    fn select_streams_audio_only_drops_video() {
        let Some(mp4) = make_av_mp4() else {
            eprintln!("skipping: ffmpeg lavfi/mpeg4/aac unavailable");
            return;
        };
        let rt = runtime();
        let options = SelectStreamsOptions {
            selections: vec![StreamSelection {
                kind: StreamKind::Audio,
                all: true,
                ..Default::default()
            }],
            ..Default::default()
        };
        let report = select_streams(&mp4, &options, &limits(), &rt).expect("select audio");
        assert_eq!(report.selected.len(), 1);
        assert_eq!(report.files.len(), 1);
        assert_eq!(report.files[0].name, "output.mp4");

        let info = crate::crates::multimedia_engine::probe::inspect::inspect(
            &report.files[0].bytes,
            &limits(),
        )
        .expect("probe output");
        assert_eq!(info.video_streams, 0);
        assert_eq!(info.audio_streams, 1);
    }

    #[test]
    fn select_streams_default_keeps_all_streams() {
        let Some(mp4) = make_av_mp4() else {
            eprintln!("skipping: ffmpeg lavfi/mpeg4/aac unavailable");
            return;
        };
        let rt = runtime();
        let report = select_streams(&mp4, &SelectStreamsOptions::default(), &limits(), &rt)
            .expect("select all");
        assert_eq!(report.selected.len(), 2);
        assert!(report.skipped.is_empty());

        let info = crate::crates::multimedia_engine::probe::inspect::inspect(
            &report.files[0].bytes,
            &limits(),
        )
        .expect("probe output");
        assert_eq!(info.video_streams, 1);
        assert_eq!(info.audio_streams, 1);
    }

    #[test]
    fn select_streams_rejects_empty_and_unmatched() {
        let rt = runtime();
        let err = select_streams(&[], &SelectStreamsOptions::default(), &limits(), &rt)
            .expect_err("must fail");
        assert!(matches!(err, MediaError::InvalidInput(_)), "{err:?}");

        let Some(mp4) = make_av_mp4() else {
            eprintln!("skipping: ffmpeg lavfi/mpeg4/aac unavailable");
            return;
        };
        let options = SelectStreamsOptions {
            selections: vec![StreamSelection {
                kind: StreamKind::Subtitle,
                ..Default::default()
            }],
            ..Default::default()
        };
        let err = select_streams(&mp4, &options, &limits(), &rt).expect_err("must fail");
        assert!(matches!(err, MediaError::InvalidInput(_)), "{err:?}");
    }
}
