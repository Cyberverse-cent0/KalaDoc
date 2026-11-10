//! Embedded image (attached-picture) extraction.
//!
//! Cover art and other still images are often stored inside media
//! containers as video streams flagged with the `attached_pic`
//! disposition (codec_type `video`, codec `mjpeg`/`png`/`bmp`/...).
//! [`list_embedded_images`] reports them; [`extract_embedded_images`]
//! decodes each selected stream's single frame and re-encodes it as
//! a standalone image file (`png` by default, or `jpg`/`bmp`/`webp`
//! via `output_format`).
//!
//! Unlike [`extract_attachments`](super::attachments) — which dumps
//! `codec_type == "attachment"` payloads bit-exact — attached
//! pictures are real encoded frames, so they go through a decode +
//! image-encoder path. The encoder is capability-gated before
//! ffmpeg is spawned.

use std::path::Path;

use serde::{Deserialize, Serialize};

use crate::crates::multimedia_engine::error::{MediaError, MediaResult};
use crate::crates::multimedia_engine::model::{MediaFile, MediaLimits, MediaOutput};
use crate::crates::multimedia_engine::probe::types::{MediaStream, ProbeReport};
use crate::crates::multimedia_engine::runtime::discovery::Runtime;
use crate::crates::multimedia_engine::runtime::{process, tempdir::TempDir};

/// One embedded image (attached-picture) stream.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct EmbeddedImageInfo {
    /// Absolute stream index (`streams[].index` from ffprobe).
    pub index: u32,
    /// Codec identifier of the stored frame (`mjpeg`, `png`, ...).
    pub codec_name: Option<String>,
    /// Width in pixels, when reported.
    pub width: Option<u32>,
    /// Height in pixels, when reported.
    pub height: Option<u32>,
    /// Stream title tag, when present.
    pub title: Option<String>,
    /// `filename` tag when the container stores one.
    pub filename: Option<String>,
}

/// Options for [`extract_embedded_images`].
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct EmbeddedImageOptions {
    /// Extract only the image at this absolute stream index.
    /// `None` extracts every attached-picture stream.
    #[serde(default)]
    pub index: Option<u32>,
    /// Output image format extension without a dot: `"png"` (default),
    /// `"jpg"`/`"jpeg"`, `"bmp"` or `"webp"`.
    #[serde(default = "default_png")]
    pub output_format: String,
}

impl Default for EmbeddedImageOptions {
    fn default() -> Self {
        Self {
            index: None,
            output_format: default_png(),
        }
    }
}

fn default_png() -> String {
    "png".to_string()
}

/// Report returned by [`extract_embedded_images`].
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct EmbeddedImageReport {
    /// Info for each successfully extracted image.
    pub items: Vec<EmbeddedImageInfo>,
    /// Produced image files (`embedded_image_<index>.<ext>`).
    pub files: Vec<MediaFile>,
    /// Per-stream failures and other non-fatal diagnostics.
    pub warnings: Vec<String>,
}

impl EmbeddedImageReport {
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

/// List the attached-picture streams in `input`.
///
/// Returns an empty vec (not an error) when there are none.
///
/// # Errors
///
/// - [`MediaError::LimitExceeded`] — input exceeds
///   `max_input_bytes`.
/// - [`MediaError::InvalidInput`] — ffprobe failed or returned
///   unparseable output.
/// - [`MediaError::MissingTool`] — `ffprobe` not on `PATH`.
pub fn list_embedded_images(
    input: &[u8],
    limits: &MediaLimits,
    rt: &Runtime,
) -> MediaResult<Vec<EmbeddedImageInfo>> {
    let streams = probed_streams(input, limits, rt)?;
    Ok(streams
        .iter()
        .filter(|s| is_embedded_image(s))
        .map(image_info)
        .collect())
}

/// Extract attached-picture stream(s) to standalone image files.
///
/// Each target stream is decoded and its first frame re-encoded as
/// `options.output_format`. Extraction is best-effort per stream:
/// individual failures degrade to warnings; only a total failure is
/// an error.
///
/// # Errors
///
/// - [`MediaError::InvalidInput`] — empty input, unsupported
///   `output_format`, or `options.index` matching no attached
///   picture.
/// - [`MediaError::Unsupported`] — this FFmpeg build lacks the
///   requested image encoder.
/// - [`MediaError::LimitExceeded`] — input/output byte caps or
///   `max_output_files`.
/// - [`MediaError::ToolFailed`] — ffmpeg failed for *every* target
///   stream.
pub fn extract_embedded_images(
    input: &[u8],
    options: &EmbeddedImageOptions,
    limits: &MediaLimits,
    rt: &Runtime,
) -> MediaResult<EmbeddedImageReport> {
    check_input_size(input, limits)?;

    let ext = options
        .output_format
        .trim()
        .trim_start_matches('.')
        .to_ascii_lowercase();
    let encoder = encoder_for_ext(&ext).ok_or_else(|| {
        MediaError::InvalidInput(format!(
            "unsupported output_format {ext:?} (expected png, jpg, bmp or webp)"
        ))
    })?;

    // Capability gate before probing/spawning.
    let caps = rt.capabilities()?;
    if !caps.has_encoder(encoder) {
        return Err(MediaError::Unsupported(format!(
            "image encoder '{encoder}' is not available in this FFmpeg build"
        )));
    }

    let dir = TempDir::new("embedded-images")?;
    let input_path = dir.write(&format!("input.{}", sniff_extension(input)), input)?;
    let streams = media_streams(&input_path, limits, rt)?;

    let targets: Vec<&MediaStream> = streams
        .iter()
        .filter(|s| is_embedded_image(s))
        .filter(|s| match options.index {
            Some(want) => s.index == Some(want),
            None => true,
        })
        .collect();

    if targets.is_empty() {
        return Err(MediaError::InvalidInput(if options.index.is_some() {
            format!(
                "no attached-picture stream with index {}",
                options.index.unwrap_or(0)
            )
        } else {
            "input has no embedded images to extract".to_string()
        }));
    }

    if let Some(max_files) = limits.max_output_files
        && targets.len() as u64 > max_files as u64
    {
        return Err(MediaError::LimitExceeded(format!(
            "{} embedded images exceed max_output_files {max_files}",
            targets.len()
        )));
    }

    let mut items = Vec::new();
    let mut files = Vec::new();
    let mut warnings = Vec::new();

    for s in targets.iter().copied() {
        let idx = s.index.unwrap_or(0);
        let name = format!("embedded_image_{idx}.{ext}");
        let out_path = dir.file(&name);

        let argv = process::args(&[
            "-hide_banner",
            "-loglevel",
            "error",
            "-nostdin",
            "-y",
            "-i",
            input_path.to_str().unwrap_or("input"),
            "-map",
            &format!("0:{idx}"),
            "-frames:v",
            "1",
            "-c:v",
            encoder,
            out_path.to_str().unwrap_or("embedded_image"),
        ]);
        match process::run("ffmpeg", &rt.ffmpeg, &argv, None, limits) {
            Ok(out) if out.success => {
                let bytes = dir.read_limited(&name, limits.max_output_bytes)?;
                if bytes.is_empty() {
                    warnings.push(format!(
                        "embedded image {idx}: ffmpeg produced an empty file"
                    ));
                    continue;
                }
                files.push(MediaFile {
                    name: name.clone(),
                    bytes,
                    content_type: image_content_type(&ext),
                });
                items.push(image_info(s));
            }
            Ok(out) => {
                warnings.push(format!(
                    "embedded image {idx}: ffmpeg failed: {}",
                    if out.stderr.is_empty() {
                        "non-zero exit".to_string()
                    } else {
                        out.stderr
                    }
                ));
            }
            Err(e) => {
                warnings.push(format!("embedded image {idx}: {e}"));
            }
        }
    }

    if files.is_empty() {
        return Err(MediaError::ToolFailed {
            tool: "ffmpeg",
            status: "no output".to_string(),
            detail: format!(
                "no embedded images could be extracted ({} target(s))",
                targets.len()
            ),
        });
    }

    Ok(EmbeddedImageReport {
        items,
        files,
        warnings,
    })
}

/// Whether `s` is an attached-picture (embedded still image) stream:
/// either `codec_type == "attached_pic"` or any stream carrying the
/// `attached_pic` disposition flag.
fn is_embedded_image(s: &MediaStream) -> bool {
    s.kind() == Some("attached_pic")
        || s.disposition_attached_pic == Some(true)
        || s.disposition.attached_pic.is_some_and(|v| v != 0)
}

fn image_info(s: &MediaStream) -> EmbeddedImageInfo {
    EmbeddedImageInfo {
        index: s.index.unwrap_or(0),
        codec_name: s.codec_name.clone(),
        width: s.width,
        height: s.height,
        title: s.title.clone().or_else(|| s.tags.get("title").cloned()),
        filename: s.tags.get("filename").cloned().or_else(|| {
            s.tags
                .iter()
                .find(|(k, _)| k.eq_ignore_ascii_case("filename"))
                .map(|(_, v)| v.clone())
        }),
    }
}

/// ffmpeg encoder name for an output image extension.
fn encoder_for_ext(ext: &str) -> Option<&'static str> {
    match ext {
        "png" => Some("png"),
        "jpg" | "jpeg" => Some("mjpeg"),
        "bmp" => Some("bmp"),
        "webp" => Some("libwebp"),
        _ => None,
    }
}

/// Best-effort MIME type for produced images.
fn image_content_type(ext: &str) -> Option<String> {
    let mime = match ext {
        "png" => "image/png",
        "jpg" | "jpeg" => "image/jpeg",
        "bmp" => "image/bmp",
        "webp" => "image/webp",
        _ => return None,
    };
    Some(mime.to_string())
}

/// Probe `input` bytes and return the raw [`MediaStream`] list.
fn probed_streams(
    input: &[u8],
    limits: &MediaLimits,
    rt: &Runtime,
) -> MediaResult<Vec<MediaStream>> {
    check_input_size(input, limits)?;
    let dir = TempDir::new("embedded-images-probe")?;
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

#[cfg(test)]
mod tests {
    use super::*;

    fn runtime() -> Runtime {
        Runtime::discover().expect("ffmpeg/ffprobe must be on PATH")
    }

    fn limits() -> MediaLimits {
        MediaLimits::default()
    }

    #[test]
    fn is_embedded_image_detects_disposition() {
        let mut pic = MediaStream {
            index: Some(2),
            codec_type: Some("video".to_string()),
            codec_name: Some("mjpeg".to_string()),
            ..Default::default()
        };
        assert!(!is_embedded_image(&pic));
        pic.disposition.attached_pic = Some(1);
        assert!(is_embedded_image(&pic));

        let mut pic2 = MediaStream {
            index: Some(3),
            codec_type: Some("video".to_string()),
            ..Default::default()
        };
        pic2.disposition_attached_pic = Some(true);
        assert!(is_embedded_image(&pic2));

        let plain = MediaStream {
            index: Some(0),
            codec_type: Some("video".to_string()),
            ..Default::default()
        };
        assert!(!is_embedded_image(&plain));
    }

    #[test]
    fn encoder_mapping_and_content_types() {
        assert_eq!(encoder_for_ext("png"), Some("png"));
        assert_eq!(encoder_for_ext("jpg"), Some("mjpeg"));
        assert_eq!(encoder_for_ext("jpeg"), Some("mjpeg"));
        assert_eq!(encoder_for_ext("bmp"), Some("bmp"));
        assert_eq!(encoder_for_ext("webp"), Some("libwebp"));
        assert_eq!(encoder_for_ext("tiff"), None);
        assert_eq!(image_content_type("png").as_deref(), Some("image/png"));
        assert_eq!(image_content_type("jpg").as_deref(), Some("image/jpeg"));
    }

    /// Build a 1s mp4 with a video+audio plus an attached-picture
    /// mjpeg stream (cover art). Returns `None` when ffmpeg or the
    /// needed encoders are unavailable so the test skips.
    fn make_mp4_with_cover() -> Option<Vec<u8>> {
        let rt = Runtime::discover().ok()?;
        let caps = rt.capabilities().ok()?;
        if !(caps.has_encoder("mpeg4") && caps.has_encoder("aac") && caps.has_encoder("mjpeg")) {
            return None;
        }
        let dir = TempDir::new("eimg-test-in").ok()?;

        // 64x64 red png frame to use as cover art.
        let cover = dir.file("cover.png");
        let argv = process::args(&[
            "-hide_banner",
            "-loglevel",
            "error",
            "-nostdin",
            "-y",
            "-f",
            "lavfi",
            "-i",
            "color=red:size=32x32:duration=1:rate=1",
            "-frames:v",
            "1",
            cover.to_str()?,
        ]);
        process::run("ffmpeg", &rt.ffmpeg, &argv, None, &limits())
            .ok()?
            .success
            .then_some(())?;

        let out = dir.file("cover.mp4");
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
            "-i",
            cover.to_str()?,
            "-map",
            "0:v",
            "-map",
            "1:a",
            "-map",
            "2:v",
            "-c:v:0",
            "mpeg4",
            "-pix_fmt:v:0",
            "yuv420p",
            "-c:a",
            "aac",
            "-c:v:1",
            "mjpeg",
            "-disposition:v:1",
            "attached_pic",
            "-id3v2_version",
            "0",
            out.to_str()?,
        ]);
        let res = process::run("ffmpeg", &rt.ffmpeg, &argv, None, &limits()).ok()?;
        if !res.success {
            return None;
        }
        std::fs::read(&out).ok()
    }

    #[test]
    fn list_and_extract_embedded_image() {
        let Some(mp4) = make_mp4_with_cover() else {
            eprintln!("skipping: ffmpeg lavfi/mpeg4/aac/mjpeg/-disposition unavailable");
            return;
        };
        let rt = runtime();
        let caps = match rt.capabilities() {
            Ok(c) => c,
            Err(_) => return,
        };
        if !caps.has_encoder("png") {
            eprintln!("skipping: png encoder unavailable");
            return;
        }

        let infos = list_embedded_images(&mp4, &limits(), &rt).expect("list_embedded_images");
        if infos.is_empty() {
            eprintln!("skipping: fixture has no attached_pic stream");
            return;
        }
        assert_eq!(infos.len(), 1);
        assert_eq!(infos[0].codec_name.as_deref(), Some("mjpeg"));

        let report =
            extract_embedded_images(&mp4, &EmbeddedImageOptions::default(), &limits(), &rt)
                .expect("extract_embedded_images");
        assert_eq!(report.files.len(), 1);
        assert_eq!(report.items.len(), 1);
        assert!(
            report.files[0].name.starts_with("embedded_image_")
                && report.files[0].name.ends_with(".png"),
            "unexpected name {}",
            report.files[0].name
        );
        assert_eq!(report.files[0].content_type.as_deref(), Some("image/png"));
        // PNG magic bytes.
        assert!(
            report.files[0].bytes.starts_with(&[0x89, b'P', b'N', b'G']),
            "expected PNG output"
        );
    }

    #[test]
    fn extract_rejects_bad_format_and_missing_images() {
        let rt = runtime();
        let options = EmbeddedImageOptions {
            output_format: "tiff".to_string(),
            ..Default::default()
        };
        let err =
            extract_embedded_images(&[0u8; 32], &options, &limits(), &rt).expect_err("must fail");
        assert!(matches!(err, MediaError::InvalidInput(_)), "{err:?}");

        // File with streams but no attached pictures → InvalidInput.
        let Some(mp4) = make_mp4_with_cover() else {
            eprintln!("skipping: fixture unavailable");
            return;
        };
        let bad = EmbeddedImageOptions {
            index: Some(9999),
            ..Default::default()
        };
        let err = extract_embedded_images(&mp4, &bad, &limits(), &rt).expect_err("must fail");
        assert!(matches!(err, MediaError::InvalidInput(_)), "{err:?}");
    }
}
