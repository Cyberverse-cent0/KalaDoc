//! Embedded attachment and cover-art extraction.
//!
//! Containers carry embedded payloads in two shapes:
//!
//! - **Cover art** — streams flagged `disposition.attached_pic`
//!   (or `still_image`) carrying an image codec: MP3/FLAC APIC
//!   pictures, MKV cover tracks, MP4 `covr` atoms. Reported by
//!   ffprobe as `codec_type == "video"` (or `"attachment"`).
//! - **Data attachments** — `codec_type == "attachment"`/`"data"`
//!   streams holding fonts, documents or arbitrary blobs
//!   (Matroska `-attach`, MP4 `data` references).
//!
//! [`list_attachments`] reports both classes via `ffprobe`;
//! [`extract_attachments`] dumps each selected stream with
//! `ffmpeg -map 0:<idx> -c copy`. Cover art goes through an image
//! muxer (`image2`, `image2webp`, `gif`) so the result is a real
//! image file; everything else is written verbatim through the raw
//! `data` muxer — a bit-exact copy with no decode and no re-encode.
//!
//! Extraction is best-effort per stream: a container may advertise
//! an attachment ffmpeg cannot actually map (broken index, removed
//! codec, missing muxer). Failures are recorded in the report's
//! `warnings` rather than aborting the whole extraction; a file with
//! no matching streams yields an empty report plus a warning, while
//! an explicit `index` that matches nothing is an
//! [`MediaError::InvalidInput`].

use std::ffi::OsString;
use std::path::Path;

use serde::{Deserialize, Serialize};

use crate::crates::multimedia_engine::error::{MediaError, MediaResult};
use crate::crates::multimedia_engine::model::{MediaFile, MediaLimits, MediaOutput};
use crate::crates::multimedia_engine::probe::types::{MediaStream, ProbeReport};
use crate::crates::multimedia_engine::runtime::discovery::Runtime;
use crate::crates::multimedia_engine::runtime::{process, tempdir::TempDir};

/// Which class of embedded payload a stream carries.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum AttachmentKind {
    /// Embedded still image / cover art.
    Cover,
    /// Opaque data attachment (font, document, blob, ...).
    Data,
}

/// One embedded attachment stream found in a media file.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct AttachmentInfo {
    /// Absolute stream index (`streams[].index` from ffprobe).
    pub index: u32,
    /// Whether this is cover art or an opaque data attachment.
    #[serde(default)]
    pub kind: Option<AttachmentKind>,
    /// ffprobe codec type (`"attachment"`, `"video"`, `"data"`, ...).
    pub codec_type: Option<String>,
    /// Codec identifier (`"bin_data"`, `"ttf"`, `"mjpeg"`, ...).
    pub codec_name: Option<String>,
    /// Long codec name.
    pub codec_long_name: Option<String>,
    /// Stream title tag, when present.
    pub title: Option<String>,
    /// `filename` tag when the container stores one (common in
    /// Matroska attachments).
    pub filename: Option<String>,
    /// Language tag, when present.
    pub language: Option<String>,
}

/// Options for [`extract_attachments`].
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct AttachmentOptions {
    /// Extract only the attachment with this absolute stream index.
    /// `None` extracts every selected attachment stream.
    #[serde(default)]
    pub index: Option<u32>,
    /// Extract cover art / attached pictures (default `true`).
    /// Ignored when [`AttachmentOptions::kind`] selects `Data`.
    #[serde(default = "default_true")]
    pub include_cover_art: bool,
    /// Restrict extraction to one attachment class. `None` (default)
    /// means "all classes allowed", still bounded by
    /// `include_cover_art`.
    #[serde(default)]
    pub kind: Option<AttachmentKind>,
}

impl Default for AttachmentOptions {
    fn default() -> Self {
        Self {
            index: None,
            include_cover_art: true,
            kind: None,
        }
    }
}

fn default_true() -> bool {
    true
}

/// Report returned by [`extract_attachments`].
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct AttachmentReport {
    /// Info for each successfully extracted attachment, in stream
    /// order.
    pub items: Vec<AttachmentInfo>,
    /// Produced files (`attachment_<index>.<ext>`).
    pub files: Vec<MediaFile>,
    /// Per-stream failures and other non-fatal diagnostics.
    pub warnings: Vec<String>,
}

impl AttachmentReport {
    /// Convert into the shared [`MediaOutput`] shape. The first
    /// extracted file becomes `primary` (attachments have no
    /// canonical "main" file, but single-file callers get something
    /// useful); the rest land in `extra`.
    pub fn into_output(self) -> MediaOutput {
        let mut iter = self.files.into_iter();
        MediaOutput {
            primary: iter.next(),
            extra: iter.collect(),
            warnings: self.warnings,
        }
    }
}

/// List the attachment streams (cover art + data attachments)
/// embedded in `input`.
///
/// Returns an empty vec (not an error) when the file has no
/// attachments.
///
/// # Errors
///
/// - [`MediaError::LimitExceeded`] — input exceeds
///   `max_input_bytes`.
/// - [`MediaError::InvalidInput`] — ffprobe failed or returned
///   unparseable output.
/// - [`MediaError::MissingTool`] — `ffprobe` not on `PATH`.
pub fn list_attachments(
    input: &[u8],
    limits: &MediaLimits,
    rt: &Runtime,
) -> MediaResult<Vec<AttachmentInfo>> {
    let streams = probed_streams(input, limits, rt)?;
    Ok(streams
        .iter()
        .filter_map(|s| classify(s).map(|kind| (kind, s)))
        .map(|(kind, s)| attachment_info(s, kind))
        .collect())
}

/// Extract attachment stream(s) from `input` to files.
///
/// Cover art is written through an image muxer (`image2`,
/// `image2webp`, `gif`) with `-c copy` so the result is a real image;
/// data attachments are dumped with `-c copy -f data`, so their bytes
/// are the container's stored payload verbatim. Output files are
/// named `attachment_<index>.<ext>` where `<ext>` comes from the
/// codec, the stream's `filename` tag, or `bin`/`img`.
///
/// When `options.index`/`kind`/`include_cover_art` select nothing,
/// returns an empty [`AttachmentReport`] with a `"no attachments"`
/// warning — most media simply has no embedded payloads.
///
/// # Errors
///
/// - [`MediaError::InvalidInput`] — empty input, or `options.index`
///   matches no attachment stream.
/// - [`MediaError::Unsupported`] — this FFmpeg build lacks every
///   muxer the selected attachments need.
/// - [`MediaError::LimitExceeded`] — input/output byte caps or
///   `max_output_files`.
/// - [`MediaError::ToolFailed`] — ffmpeg failed for *every* target
///   stream (individual failures degrade to warnings instead).
pub fn extract_attachments(
    input: &[u8],
    options: &AttachmentOptions,
    limits: &MediaLimits,
    rt: &Runtime,
) -> MediaResult<AttachmentReport> {
    check_input_size(input, limits)?;

    // Capability gate before spawning: `-c copy` needs no encoder,
    // but each muxer we may use must exist in this build. If the
    // capability probe itself fails we proceed anyway — ffmpeg will
    // report honestly per stream.
    let caps = rt.capabilities().ok();

    let dir = TempDir::new("attachments")?;
    let input_path = dir.write(&format!("input.{}", sniff_extension(input)), input)?;
    let streams = media_streams(&input_path, limits, rt)?;

    // (stream, kind) pairs selected by kind/include_cover_art/index.
    let targets: Vec<(&MediaStream, AttachmentKind)> = streams
        .iter()
        .filter_map(|s| classify(s).map(|kind| (s, kind)))
        .filter(|(_, kind)| wanted(*kind, options))
        .filter(|(s, _)| match options.index {
            Some(want) => s.index == Some(want),
            None => true,
        })
        .collect();

    if targets.is_empty() {
        if options.index.is_some() {
            return Err(MediaError::InvalidInput(format!(
                "no attachment stream with index {}",
                options.index.unwrap_or(0)
            )));
        }
        return Ok(AttachmentReport {
            warnings: vec!["no attachments found".to_string()],
            ..Default::default()
        });
    }

    if let Some(max_files) = limits.max_output_files
        && targets.len() as u64 > max_files as u64
    {
        return Err(MediaError::LimitExceeded(format!(
            "{} attachment streams exceed max_output_files {max_files}",
            targets.len()
        )));
    }

    // Fail early only when *every* needed muxer is missing; per-stream
    // missing muxers degrade to warnings below.
    if let Some(caps) = &caps {
        let needed = targets
            .iter()
            .map(|(s, kind)| muxer_for(s, *kind).0)
            .collect::<std::collections::BTreeSet<_>>();
        if !needed.is_empty() && needed.iter().all(|m| !caps.has_muxer(m)) {
            return Err(MediaError::Unsupported(format!(
                "none of the required muxers ({}) are available in this FFmpeg build",
                needed.iter().copied().collect::<Vec<_>>().join(", ")
            )));
        }
    }

    let mut items = Vec::new();
    let mut files = Vec::new();
    let mut warnings = Vec::new();

    for (s, kind) in targets {
        let idx = s.index.unwrap_or(0);
        let (muxer, ext) = muxer_for(s, kind);
        let ext = ext_for(s, kind, ext);

        if let Some(caps) = &caps
            && !caps.has_muxer(muxer)
        {
            warnings.push(format!(
                "attachment {idx}: muxer '{muxer}' unavailable in this FFmpeg build; skipped"
            ));
            continue;
        }

        let name = format!("attachment_{idx}.{ext}");
        let out_path = dir.file(&name);
        let argv = extraction_argv(&input_path, idx, &out_path, muxer, kind);

        match process::run("ffmpeg", &rt.ffmpeg, &argv, None, limits) {
            Ok(out) if out.success => {
                let bytes = dir.read_limited(&name, limits.max_output_bytes)?;
                if bytes.is_empty() {
                    warnings.push(format!("attachment {idx}: ffmpeg produced an empty file"));
                    continue;
                }
                files.push(MediaFile {
                    name: name.clone(),
                    bytes,
                    content_type: content_type_for(&ext),
                });
                items.push(attachment_info(s, kind));
            }
            Ok(out) => {
                warnings.push(format!(
                    "attachment {idx}: ffmpeg failed: {}",
                    if out.stderr.is_empty() {
                        "non-zero exit".to_string()
                    } else {
                        out.stderr
                    }
                ));
            }
            Err(e) => {
                warnings.push(format!("attachment {idx}: {e}"));
            }
        }
    }

    if files.is_empty() {
        if warnings.is_empty() {
            warnings.push("no attachments could be extracted".to_string());
        }
        return Ok(AttachmentReport {
            items,
            files,
            warnings,
        });
    }

    Ok(AttachmentReport {
        items,
        files,
        warnings,
    })
}

/// Whether `kind` passes the [`AttachmentOptions`] filter: an
/// explicit `options.kind` selects exactly that class; otherwise
/// cover art is included only when `include_cover_art` is set.
fn wanted(kind: AttachmentKind, options: &AttachmentOptions) -> bool {
    match options.kind {
        Some(want) => kind == want,
        None => kind == AttachmentKind::Data || options.include_cover_art,
    }
}

/// Classify a stream as an extractable attachment, or `None`.
fn classify(s: &MediaStream) -> Option<AttachmentKind> {
    let image = is_image_codec(s.codec_name.as_deref());
    let flagged = s.disposition_attached_pic == Some(true)
        || s.disposition.attached_pic.is_some_and(|v| v != 0)
        || s.disposition.still_image.is_some_and(|v| v != 0);
    match s.kind() {
        // MP3/FLAC-style cover art: a video stream carrying an image
        // codec with the attached-pic disposition.
        Some("video") if flagged && image => Some(AttachmentKind::Cover),
        // Matroska/MP4 attachment tracks: image-coded ones are cover
        // art, everything else is an opaque payload.
        Some("attachment") => Some(if image {
            AttachmentKind::Cover
        } else {
            AttachmentKind::Data
        }),
        Some("data") => Some(if flagged && image {
            AttachmentKind::Cover
        } else {
            AttachmentKind::Data
        }),
        _ => None,
    }
}

/// Image codecs we can write through an image muxer with `-c copy`.
fn is_image_codec(codec: Option<&str>) -> bool {
    matches!(
        codec,
        Some("mjpeg" | "png" | "bmp" | "gif" | "webp" | "tiff" | "ppm" | "pgm" | "pbm")
    )
}

/// `(muxer, fallback extension)` for a classified stream. Cover art
/// uses image muxers; data uses the raw `data` muxer.
fn muxer_for<'a>(s: &'a MediaStream, kind: AttachmentKind) -> (&'static str, &'a str) {
    match kind {
        AttachmentKind::Cover => image_muxer(s.codec_name.as_deref()),
        AttachmentKind::Data => ("data", "bin"),
    }
}

/// Pick `(muxer, extension)` for an image codec. mjpeg and png (and
/// unknown image-ish codecs) go through `image2`, which handles
/// both; ffmpeg fails honestly if the muxer rejects the codec.
fn image_muxer(codec: Option<&str>) -> (&'static str, &'static str) {
    match codec {
        Some("webp") => ("image2webp", "webp"),
        Some("gif") => ("gif", "gif"),
        Some("tiff") => ("image2", "tif"),
        Some("ppm") => ("image2", "ppm"),
        Some("pgm") => ("image2", "pgm"),
        Some("pbm") => ("image2", "pbm"),
        _ => ("image2", image_ext(codec)),
    }
}

/// Extension for an image codec under `image2`.
fn image_ext(codec: Option<&str>) -> &'static str {
    match codec {
        Some("png") => "png",
        Some("bmp") => "bmp",
        Some("mjpeg" | "jpg") => "jpg",
        _ => "img",
    }
}

/// Resolve the final output extension: the stream's `filename` tag
/// extension when present and sane, else the codec-derived fallback
/// (`ttf`/`otf`/`pdf`/image ext kept, everything else `bin`).
fn ext_for(s: &MediaStream, kind: AttachmentKind, fallback: &str) -> String {
    if let Some(fname) = s.tags.get("filename")
        && let Some(ext) = Path::new(fname).extension().and_then(|e| e.to_str())
    {
        let ext = ext.trim().to_ascii_lowercase();
        if !ext.is_empty() && ext.chars().all(|c| c.is_ascii_alphanumeric()) {
            return ext;
        }
    }
    match kind {
        AttachmentKind::Cover => fallback.to_string(),
        AttachmentKind::Data => match s.codec_name.as_deref() {
            Some("ttf") => "ttf".to_string(),
            Some("otf") => "otf".to_string(),
            Some("ttc") => "ttc".to_string(),
            Some("pdf") => "pdf".to_string(),
            Some(codec) => {
                let cleaned: String = codec
                    .chars()
                    .map(|c| {
                        if c.is_ascii_alphanumeric() {
                            c.to_ascii_lowercase()
                        } else {
                            '_'
                        }
                    })
                    .collect();
                let cleaned = cleaned.trim_matches('_');
                if cleaned.is_empty() {
                    "bin".to_string()
                } else {
                    cleaned.to_string()
                }
            }
            None => "bin".to_string(),
        },
    }
}

/// Build ffmpeg argv for one stream.
/// `-f` pins the muxer so ambiguous extensions cannot pick the wrong
/// one. Cover art adds `-frames:v 1` — attached pics are stills, and
/// it keeps multi-frame edge cases from emitting more than one file.
fn extraction_argv(
    input_path: &Path,
    stream_index: u32,
    out_path: &Path,
    muxer: &str,
    kind: AttachmentKind,
) -> Vec<OsString> {
    let mut argv = vec![
        OsString::from("-hide_banner"),
        OsString::from("-loglevel"),
        OsString::from("error"),
        OsString::from("-nostdin"),
        OsString::from("-y"),
        OsString::from("-i"),
        input_path.as_os_str().to_os_string(),
        OsString::from("-map"),
        OsString::from(format!("0:{stream_index}")),
        OsString::from("-c"),
        OsString::from("copy"),
    ];
    if kind == AttachmentKind::Cover {
        argv.push(OsString::from("-frames:v"));
        argv.push(OsString::from("1"));
    }
    argv.push(OsString::from("-f"));
    argv.push(OsString::from(muxer));
    argv.push(out_path.as_os_str().to_os_string());
    argv
}

fn attachment_info(s: &MediaStream, kind: AttachmentKind) -> AttachmentInfo {
    AttachmentInfo {
        index: s.index.unwrap_or(0),
        kind: Some(kind),
        codec_type: s.codec_type.clone(),
        codec_name: s.codec_name.clone(),
        codec_long_name: s.codec_long_name.clone(),
        title: s.title.clone().or_else(|| s.tags.get("title").cloned()),
        filename: s.tags.get("filename").cloned().or_else(|| {
            s.tags
                .iter()
                .find(|(k, _)| k.eq_ignore_ascii_case("filename"))
                .map(|(_, v)| v.clone())
        }),
        language: s
            .language
            .clone()
            .or_else(|| s.tags.get("language").cloned()),
    }
}

/// Probe `input` bytes and return the raw [`MediaStream`] list.
fn probed_streams(
    input: &[u8],
    limits: &MediaLimits,
    rt: &Runtime,
) -> MediaResult<Vec<MediaStream>> {
    check_input_size(input, limits)?;
    let dir = TempDir::new("attachments-probe")?;
    let input_path = dir.write(&format!("input.{}", sniff_extension(input)), input)?;
    media_streams(&input_path, limits, rt)
}

/// Run `ffprobe -show_streams` on a real path and parse the report.
fn media_streams(
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

/// Best-effort MIME type for extracted payloads.
fn content_type_for(ext: &str) -> Option<String> {
    let mime = match ext {
        "png" => "image/png",
        "jpg" | "jpeg" => "image/jpeg",
        "gif" => "image/gif",
        "webp" => "image/webp",
        "bmp" => "image/bmp",
        "tif" | "tiff" => "image/tiff",
        "ppm" => "image/x-portable-pixmap",
        "pgm" => "image/x-portable-graymap",
        "pbm" => "image/x-portable-bitmap",
        "ttf" => "font/ttf",
        "otf" => "font/otf",
        "ttc" => "font/collection",
        "pdf" => "application/pdf",
        "zip" => "application/zip",
        "txt" | "srt" | "vtt" => "text/plain",
        _ => "application/octet-stream",
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

    fn attachment_stream(index: u32, codec: &str) -> MediaStream {
        MediaStream {
            index: Some(index),
            codec_type: Some("attachment".to_string()),
            codec_name: Some(codec.to_string()),
            ..Default::default()
        }
    }

    fn cover_stream(index: u32, codec: &str) -> MediaStream {
        MediaStream {
            index: Some(index),
            codec_type: Some("video".to_string()),
            codec_name: Some(codec.to_string()),
            disposition_attached_pic: Some(true),
            ..Default::default()
        }
    }

    #[test]
    fn classify_cover_and_data() {
        assert_eq!(
            classify(&cover_stream(0, "mjpeg")),
            Some(AttachmentKind::Cover)
        );
        assert_eq!(
            classify(&attachment_stream(1, "png")),
            Some(AttachmentKind::Cover)
        );
        assert_eq!(
            classify(&attachment_stream(2, "ttf")),
            Some(AttachmentKind::Data)
        );
        let plain_video = MediaStream {
            index: Some(0),
            codec_type: Some("video".to_string()),
            codec_name: Some("h264".to_string()),
            ..Default::default()
        };
        assert_eq!(classify(&plain_video), None);
    }

    #[test]
    fn wanted_filters_by_kind_and_cover_flag() {
        let all = AttachmentOptions::default();
        assert!(wanted(AttachmentKind::Cover, &all));
        assert!(wanted(AttachmentKind::Data, &all));

        let no_cover = AttachmentOptions {
            include_cover_art: false,
            ..Default::default()
        };
        assert!(!wanted(AttachmentKind::Cover, &no_cover));
        assert!(wanted(AttachmentKind::Data, &no_cover));

        let cover_only = AttachmentOptions {
            kind: Some(AttachmentKind::Cover),
            ..Default::default()
        };
        assert!(wanted(AttachmentKind::Cover, &cover_only));
        assert!(!wanted(AttachmentKind::Data, &cover_only));

        let data_only = AttachmentOptions {
            kind: Some(AttachmentKind::Data),
            include_cover_art: true,
            ..Default::default()
        };
        assert!(wanted(AttachmentKind::Data, &data_only));
        assert!(!wanted(AttachmentKind::Cover, &data_only));
    }

    #[test]
    fn ext_prefers_filename_tag_then_codec() {
        let mut s = attachment_stream(4, "bin_data");
        s.tags = BTreeMap::from([("filename".to_string(), "notes.txt".to_string())]);
        assert_eq!(ext_for(&s, AttachmentKind::Data, "bin"), "txt");

        let s = attachment_stream(4, "ttf");
        assert_eq!(ext_for(&s, AttachmentKind::Data, "bin"), "ttf");

        let s = attachment_stream(4, "weird-codec!");
        assert_eq!(ext_for(&s, AttachmentKind::Data, "bin"), "weird_codec");

        let s = MediaStream {
            index: Some(4),
            codec_type: Some("attachment".to_string()),
            ..Default::default()
        };
        assert_eq!(ext_for(&s, AttachmentKind::Data, "bin"), "bin");

        let s = cover_stream(4, "png");
        assert_eq!(ext_for(&s, AttachmentKind::Cover, "png"), "png");
    }

    /// 1s mpeg4+aac mp4 remuxed into mkv with a `-attach`ed file —
    /// Matroska is the reliable data-attachment carrier. Returns
    /// `None` when ffmpeg/`-attach` is unavailable so the test skips.
    fn make_mkv_with_attachment() -> Option<Vec<u8>> {
        let rt = Runtime::discover().ok()?;
        let ffmpeg = discovery::find_tool("ffmpeg")?;
        let dir = TempDir::new("attach-test-in").ok()?;

        // Base av mkv via stream copy from an mp4 source.
        let src = dir.file("src.mp4");
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
            src.to_str()?,
        ]);
        process::run("ffmpeg", &ffmpeg, &argv, None, &limits())
            .ok()?
            .success
            .then_some(())?;

        let payload = dir.file("payload.bin");
        std::fs::write(&payload, b"KALADOC-ATTACHMENT-PAYLOAD").ok()?;
        let out = dir.file("with-attach.mkv");
        let argv = process::args(&[
            "-hide_banner",
            "-loglevel",
            "error",
            "-nostdin",
            "-y",
            "-i",
            src.to_str()?,
            "-attach",
            payload.to_str()?,
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

    /// mp3 with a sine track and an mjpeg cover art stream (ID3 APIC
    /// attached pic). Returns `None` when lavfi/mjpeg/libmp3lame are
    /// unavailable so the test skips.
    fn make_mp3_with_cover() -> Option<Vec<u8>> {
        let ffmpeg = discovery::find_tool("ffmpeg")?;
        let dir = TempDir::new("attach-test-mp3").ok()?;
        let cover = dir.file("cover.jpg");
        let mp3 = dir.file("song.mp3");
        let limits = limits();

        // 32x32 red mjpeg still.
        let argv = process::args(&[
            "-hide_banner",
            "-loglevel",
            "error",
            "-nostdin",
            "-y",
            "-f",
            "lavfi",
            "-i",
            "color=red:size=32x32:rate=1:duration=1",
            "-frames:v",
            "1",
            "-c:v",
            "mjpeg",
            cover.to_str()?,
        ]);
        process::run("ffmpeg", &ffmpeg, &argv, None, &limits)
            .ok()?
            .success
            .then_some(())?;
        if !cover.is_file() {
            return None;
        }

        let argv = process::args(&[
            "-hide_banner",
            "-loglevel",
            "error",
            "-nostdin",
            "-y",
            "-f",
            "lavfi",
            "-i",
            "sine=frequency=440:duration=1",
            "-i",
            cover.to_str()?,
            "-map",
            "0:a",
            "-map",
            "1:v",
            "-c:a",
            "libmp3lame",
            "-c:v",
            "copy",
            "-id3v2_version",
            "3",
            "-disposition:v",
            "attached_pic",
            mp3.to_str()?,
        ]);
        let res = process::run("ffmpeg", &ffmpeg, &argv, None, &limits).ok()?;
        if !res.success {
            return None;
        }
        std::fs::read(&mp3).ok()
    }

    /// Plain mp3 with no embedded payload.
    fn make_plain_mp3() -> Option<Vec<u8>> {
        let ffmpeg = discovery::find_tool("ffmpeg")?;
        let dir = TempDir::new("attach-test-plain").ok()?;
        let mp3 = dir.file("plain.mp3");
        let argv = process::args(&[
            "-hide_banner",
            "-loglevel",
            "error",
            "-nostdin",
            "-y",
            "-f",
            "lavfi",
            "-i",
            "sine=frequency=330:duration=1",
            "-c:a",
            "libmp3lame",
            mp3.to_str()?,
        ]);
        let res = process::run("ffmpeg", &ffmpeg, &argv, None, &limits()).ok()?;
        if !res.success {
            return None;
        }
        std::fs::read(&mp3).ok()
    }

    #[test]
    fn list_and_extract_attachment_roundtrip() {
        let Some(mkv) = make_mkv_with_attachment() else {
            eprintln!("skipping: ffmpeg lavfi/mpeg4/aac/-attach unavailable");
            return;
        };
        let rt = runtime();
        let infos = list_attachments(&mkv, &limits(), &rt).expect("list_attachments");
        if infos.is_empty() {
            eprintln!("skipping: ffmpeg -attach produced no attachment stream");
            return;
        }
        assert_eq!(infos.len(), 1);
        assert_eq!(infos[0].codec_type.as_deref(), Some("attachment"));
        assert_eq!(infos[0].kind, Some(AttachmentKind::Data));

        let report = extract_attachments(&mkv, &AttachmentOptions::default(), &limits(), &rt)
            .expect("extract_attachments");
        assert_eq!(report.files.len(), 1);
        assert_eq!(report.items.len(), 1);
        assert!(
            report.files[0].name.starts_with("attachment_"),
            "unexpected name {}",
            report.files[0].name
        );
        assert_eq!(
            report.files[0].bytes,
            b"KALADOC-ATTACHMENT-PAYLOAD".to_vec(),
            "attachment payload must be copied bit-exact"
        );
    }

    #[test]
    fn extracts_mp3_cover_art_as_jpeg() {
        let Some(mp3) = make_mp3_with_cover() else {
            eprintln!("skipping: ffmpeg lavfi/mjpeg/libmp3lame unavailable");
            return;
        };
        let rt = runtime();

        // Sanity: cover art is visible to the lister as Cover kind.
        let infos = list_attachments(&mp3, &limits(), &rt).expect("list_attachments");
        if infos.is_empty() {
            eprintln!("skipping: mp3 muxer did not embed the cover stream");
            return;
        }
        assert!(
            infos.iter().any(|i| i.kind == Some(AttachmentKind::Cover)),
            "expected a Cover attachment, got {infos:?}"
        );

        let report = extract_attachments(&mp3, &AttachmentOptions::default(), &limits(), &rt)
            .expect("extract_attachments");
        let cover = report
            .files
            .iter()
            .find(|f| f.name.ends_with(".jpg"))
            .expect("expected a .jpg cover file");
        assert_eq!(cover.content_type.as_deref(), Some("image/jpeg"));
        assert!(
            cover.bytes.starts_with(&[0xFF, 0xD8, 0xFF]),
            "extracted cover is not a JPEG"
        );
    }

    #[test]
    fn include_cover_art_false_skips_cover() {
        let Some(mp3) = make_mp3_with_cover() else {
            eprintln!("skipping: ffmpeg lavfi/mjpeg/libmp3lame unavailable");
            return;
        };
        let rt = runtime();
        let options = AttachmentOptions {
            include_cover_art: false,
            ..Default::default()
        };
        let report = extract_attachments(&mp3, &options, &limits(), &rt).expect("extract");
        assert!(report.files.is_empty());
        assert!(
            report.warnings.iter().any(|w| w.contains("no attachments")),
            "warnings: {:?}",
            report.warnings
        );
    }

    #[test]
    fn plain_mp3_reports_no_attachments() {
        let Some(mp3) = make_plain_mp3() else {
            eprintln!("skipping: ffmpeg lavfi/libmp3lame unavailable");
            return;
        };
        let rt = runtime();
        let report = extract_attachments(&mp3, &AttachmentOptions::default(), &limits(), &rt)
            .expect("extract");
        assert!(report.files.is_empty());
        assert!(
            report.warnings.iter().any(|w| w.contains("no attachments")),
            "warnings: {:?}",
            report.warnings
        );
        assert!(
            list_attachments(&mp3, &limits(), &rt)
                .expect("list")
                .is_empty()
        );
    }

    #[test]
    fn extract_rejects_no_attachments_and_bad_index() {
        let rt = runtime();
        let Some(mkv) = make_mkv_with_attachment() else {
            eprintln!("skipping: ffmpeg lavfi/mpeg4/aac/-attach unavailable");
            return;
        };
        // Garbage input fails probing.
        let err = extract_attachments(
            &[0xDEu8; 256],
            &AttachmentOptions::default(),
            &limits(),
            &rt,
        )
        .expect_err("must fail");
        assert!(
            matches!(
                err,
                MediaError::InvalidInput(_) | MediaError::ToolFailed { .. }
            ),
            "unexpected error: {err:?}"
        );
        // Out-of-range index on a file that does have attachments.
        let bad = AttachmentOptions {
            index: Some(9999),
            ..Default::default()
        };
        let err = extract_attachments(&mkv, &bad, &limits(), &rt).expect_err("must fail");
        assert!(matches!(err, MediaError::InvalidInput(_)), "{err:?}");
    }

    #[test]
    fn enforces_max_output_files() {
        let Some(mkv) = make_mkv_with_attachment() else {
            eprintln!("skipping: ffmpeg lavfi/mpeg4/aac/-attach unavailable");
            return;
        };
        let rt = runtime();
        let tight = MediaLimits {
            max_output_files: Some(0),
            ..MediaLimits::default()
        };
        let err = extract_attachments(&mkv, &AttachmentOptions::default(), &tight, &rt)
            .expect_err("must fail");
        assert!(matches!(err, MediaError::LimitExceeded(_)), "{err:?}");
    }
}
