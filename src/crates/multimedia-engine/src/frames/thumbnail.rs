//! Video thumbnail / poster-frame generation and storyboard
//! (contact-sheet) extraction.
//!
//! [`thumbnail`] grabs a single frame at a requested timestamp —
//! defaulting to the middle of the video (probed via ffprobe) — and
//! writes it as `jpg`/`png`/`webp`. [`storyboard`] samples frames
//! evenly across the whole clip and tiles them into one grid image.
//!
//! Both ops write the input buffer to a temp file (ffmpeg needs a
//! seekable path), run ffmpeg with an argv vector under
//! [`MediaLimits`], and read the produced image back under
//! `max_output_bytes`. Codec/filter availability is checked against
//! the [`Runtime`] capability registry — no silent fallbacks.

use std::ffi::OsString;

use crate::crates::multimedia_engine::error::{MediaError, MediaResult};
use crate::crates::multimedia_engine::model::{MediaLimits, MediaOutput};
use crate::crates::multimedia_engine::probe::inspect::inspect;
use crate::crates::multimedia_engine::runtime::discovery::Runtime;
use crate::crates::multimedia_engine::runtime::process;
use crate::crates::multimedia_engine::runtime::tempdir::TempDir;

/// Options for [`thumbnail`] and [`storyboard`].
#[derive(Debug, Clone)]
pub struct ThumbnailOptions {
    /// Timestamp to grab the frame at, in seconds. `None` → the
    /// middle of the video (`probed duration / 2`). Ignored by
    /// [`storyboard`], which samples across the whole clip.
    pub at_secs: Option<f64>,
    /// Maximum output width in pixels. For [`thumbnail`] the frame is
    /// scaled to fit *inside* `width`×`height` preserving aspect
    /// ratio (`force_original_aspect_ratio=decrease`); a missing
    /// dimension keeps the source aspect. For [`storyboard`] these
    /// are the per-tile cell size (defaults 160×90).
    pub width: Option<u32>,
    /// Maximum output height in pixels — see [`Self::width`].
    pub height: Option<u32>,
    /// Output image format: `"jpg"` (default), `"png"` or `"webp"`.
    pub output_format: String,
    /// Quality hint 0–100. Mapped to ffmpeg `-q:v` for jpeg and
    /// `-quality` for webp; ignored for lossless png.
    pub quality: Option<u8>,
}

impl Default for ThumbnailOptions {
    fn default() -> Self {
        Self {
            at_secs: None,
            width: None,
            height: None,
            output_format: "jpg".to_string(),
            quality: None,
        }
    }
}

/// Resolved output format: file extension, MIME type and the ffmpeg
/// encoder name(s) that may produce it.
struct ImageFormat {
    ext: &'static str,
    mime: &'static str,
    /// Acceptable encoder names — any one satisfies the capability
    /// check (webp exists as both native `webp` and `libwebp`).
    encoders: &'static [&'static str],
}

fn resolve_format(name: &str) -> MediaResult<ImageFormat> {
    let lower = name.trim().to_ascii_lowercase();
    match lower.as_str() {
        "jpg" | "jpeg" => Ok(ImageFormat {
            ext: "jpg",
            mime: "image/jpeg",
            encoders: &["mjpeg"],
        }),
        "png" => Ok(ImageFormat {
            ext: "png",
            mime: "image/png",
            encoders: &["png"],
        }),
        "webp" => Ok(ImageFormat {
            ext: "webp",
            mime: "image/webp",
            encoders: &["webp", "libwebp"],
        }),
        other => Err(MediaError::Unsupported(format!(
            "thumbnail output_format '{other}' is not supported (use jpg, png or webp)"
        ))),
    }
}

/// Grab one frame from `input` and return it as an image file.
///
/// Runs `ffmpeg -ss <t> -i input -frames:v 1 -vf scale=… out.ext`.
/// When `options.at_secs` is `None` the timestamp defaults to the
/// middle of the video, probed via ffprobe; if the duration cannot
/// be determined the frame is taken at `0`.
///
/// Errors:
/// - [`MediaError::LimitExceeded`] when `input` exceeds
///   `max_input_bytes` or the produced image exceeds
///   `max_output_bytes`.
/// - [`MediaError::InvalidInput`] when the media cannot be decoded.
/// - [`MediaError::Unsupported`] for unknown formats or missing
///   ffmpeg filters/encoders in this build.
pub fn thumbnail(
    input: &[u8],
    options: &ThumbnailOptions,
    limits: &MediaLimits,
    rt: &Runtime,
) -> MediaResult<MediaOutput> {
    check_input_size(input, limits)?;
    let fmt = resolve_format(&options.output_format)?;
    validate_dimensions(options.width, options.height)?;

    let timestamp = match options.at_secs {
        Some(t) => Some(t),
        None => probed_duration_secs(input, limits)?.map(|d| (d / 2.0).max(0.0)),
    };

    // `-ss` before `-i` does input-side seeking — fast, and for a
    // single frame the nearest keyframe is fine for a poster.
    let mut vf_parts: Vec<String> = Vec::new();
    if let Some(filter) = thumbnail_scale_filter(options.width, options.height) {
        vf_parts.push(filter);
    }

    let (dir, input_name) = stage_input(input)?;
    let out_name = format!("thumbnail.{}", fmt.ext);
    let out_path = dir.file(&out_name);

    let mut args: Vec<OsString> = vec![
        OsString::from("-y"),
        OsString::from("-v"),
        OsString::from("error"),
    ];
    if let Some(t) = timestamp {
        args.push(OsString::from("-ss"));
        args.push(OsString::from(format!("{t:.3}")));
    }
    args.extend([OsString::from("-i"), dir.file(&input_name).into_os_string()]);
    args.push(OsString::from("-frames:v"));
    args.push(OsString::from("1"));
    if !vf_parts.is_empty() {
        args.push(OsString::from("-vf"));
        args.push(OsString::from(vf_parts.join(",")));
    }
    push_quality_args(&mut args, &fmt, options.quality);
    args.push(out_path.into_os_string());

    ensure_caps(rt, &["scale"], fmt.encoders)?;
    let out = process::run_checked("ffmpeg", &rt.ffmpeg, &args, None, limits)?;

    Ok(finish_output(
        &dir, &out_name, fmt.mime, out.stderr, limits,
    )?)
}

/// Extract frames across the whole video and tile them into one
/// contact-sheet image (`cols × rows` grid).
///
/// Runs `ffmpeg -i input -vf "fps=N/D,scale=CW:CH,…,pad=…,tile=CxR"
/// -frames:v 1 sheet.ext` where `N = cols*rows` and `D` is the
/// probed duration — frames are spread evenly over the clip. Each
/// tile is scaled to fit `options.width`×`options.height` (defaults
/// 160×90) preserving aspect ratio and padded to a uniform cell so
/// the `tile` filter gets consistent frame sizes.
///
/// The probed duration is required: without it there is no way to
/// spread samples across the clip, so [`MediaError::InvalidInput`]
/// is returned.
pub fn storyboard(
    input: &[u8],
    tiles: (u32, u32),
    options: &ThumbnailOptions,
    limits: &MediaLimits,
    rt: &Runtime,
) -> MediaResult<MediaOutput> {
    check_input_size(input, limits)?;
    let fmt = resolve_format(&options.output_format)?;
    let (cols, rows) = tiles;
    if cols == 0 || rows == 0 {
        return Err(MediaError::InvalidInput(format!(
            "storyboard tile grid {cols}x{rows} must be non-zero"
        )));
    }
    let cell_w = options.width.unwrap_or(160);
    let cell_h = options.height.unwrap_or(90);
    if cell_w == 0 || cell_h == 0 {
        return Err(MediaError::InvalidInput(
            "storyboard tile size must be non-zero".to_string(),
        ));
    }

    let duration = probed_duration_secs(input, limits)?.ok_or_else(|| {
        MediaError::InvalidInput(
            "storyboard requires a probeable duration to spread samples".to_string(),
        )
    })?;
    if !(duration > 0.0) {
        return Err(MediaError::InvalidInput(format!(
            "storyboard duration must be positive, got {duration}"
        )));
    }

    let frame_count = cols as u64 * rows as u64;
    // fps = frames / seconds → exactly ~`frame_count` samples spread
    // evenly over the clip.
    let fps = format!("{frame_count}/{}", trim_float(duration));

    let vf = format!(
        "fps={fps},scale={cell_w}:{cell_h}:force_original_aspect_ratio=decrease,pad={cell_w}:{cell_h}:(ow-iw)/2:(oh-ih)/2,tile={cols}x{rows}"
    );

    let (dir, input_name) = stage_input(input)?;
    let out_name = format!("storyboard.{}", fmt.ext);
    let out_path = dir.file(&out_name);

    let mut args: Vec<OsString> = vec![
        OsString::from("-y"),
        OsString::from("-v"),
        OsString::from("error"),
        OsString::from("-i"),
        dir.file(&input_name).into_os_string(),
        OsString::from("-vf"),
        OsString::from(vf),
        OsString::from("-frames:v"),
        OsString::from("1"),
    ];
    push_quality_args(&mut args, &fmt, options.quality);
    args.push(out_path.into_os_string());

    ensure_caps(rt, &["fps", "scale", "pad", "tile"], fmt.encoders)?;
    let out = process::run_checked("ffmpeg", &rt.ffmpeg, &args, None, limits)?;

    Ok(finish_output(
        &dir, &out_name, fmt.mime, out.stderr, limits,
    )?)
}

// ------------------------------------------------------------------
// helpers
// ------------------------------------------------------------------

/// Reject inputs over `max_input_bytes` before doing any work.
fn check_input_size(input: &[u8], limits: &MediaLimits) -> MediaResult<()> {
    if let Some(max) = limits.max_input_bytes
        && input.len() as u64 > max
    {
        return Err(MediaError::LimitExceeded(format!(
            "input is {} bytes, exceeding max_input_bytes {max}",
            input.len()
        )));
    }
    Ok(())
}

/// Reject zero/absurd thumbnail box dimensions early.
fn validate_dimensions(width: Option<u32>, height: Option<u32>) -> MediaResult<()> {
    for (name, dim) in [("width", width), ("height", height)] {
        if let Some(0) = dim {
            return Err(MediaError::InvalidInput(format!(
                "thumbnail {name} must be greater than zero"
            )));
        }
    }
    Ok(())
}

/// Container duration in seconds via the existing ffprobe inspect
/// path. `None` when the container reports no usable duration.
fn probed_duration_secs(input: &[u8], limits: &MediaLimits) -> MediaResult<Option<f64>> {
    let info = inspect(input, limits)?;
    Ok(info.duration_secs.filter(|d| d.is_finite() && *d > 0.0))
}

/// Build the `scale` filter for a thumbnail: fit inside
/// `width`×`height` preserving aspect ratio. `None` → no scaling.
fn thumbnail_scale_filter(width: Option<u32>, height: Option<u32>) -> Option<String> {
    match (width, height) {
        (Some(w), Some(h)) => Some(format!(
            "scale={w}:{h}:force_original_aspect_ratio=decrease"
        )),
        (Some(w), None) => Some(format!("scale={w}:-2")),
        (None, Some(h)) => Some(format!("scale=-2:{h}")),
        (None, None) => None,
    }
}

/// Format `duration` for an ffmpeg `fps=num/den` rate without
/// trailing noise (`2` not `2.000000`).
fn trim_float(v: f64) -> String {
    let s = format!("{v:.6}");
    s.trim_end_matches('0').trim_end_matches('.').to_string()
}

/// Append codec-specific quality args. `-q:v` (jpeg) is 2=best…31=
/// worst, so quality 100→2 and 0→31; webp takes `-quality` 0–100;
/// png is lossless and gets nothing.
fn push_quality_args(args: &mut Vec<OsString>, fmt: &ImageFormat, quality: Option<u8>) {
    let Some(q) = quality else { return };
    match fmt.ext {
        "jpg" => {
            let scaled = 31u32.saturating_sub(u32::from(q) * 29 / 100).clamp(2, 31);
            args.push(OsString::from("-q:v"));
            args.push(OsString::from(scaled.to_string()));
        }
        "webp" => {
            args.push(OsString::from("-quality"));
            args.push(OsString::from(q.to_string()));
        }
        _ => {}
    }
}

/// Verify the required filters and at least one acceptable encoder
/// exist in this FFmpeg build.
fn ensure_caps(rt: &Runtime, filters: &[&str], encoders: &[&str]) -> MediaResult<()> {
    let caps = rt.capabilities()?;
    for f in filters {
        if !caps.has_filter(f) {
            return Err(MediaError::Unsupported(format!(
                "ffmpeg build lacks required filter '{f}'"
            )));
        }
    }
    if !encoders.iter().any(|e| caps.has_encoder(e)) {
        return Err(MediaError::Unsupported(format!(
            "ffmpeg build lacks a usable encoder (need one of: {})",
            encoders.join(", ")
        )));
    }
    Ok(())
}

/// Write `input` into a fresh temp dir and return the dir plus the
/// sniffed input file name.
fn stage_input(input: &[u8]) -> MediaResult<(TempDir, String)> {
    let dir = TempDir::new("thumb")?;
    let name = format!("input{}", sniff_extension(input));
    dir.write(&name, input)?;
    Ok((dir, name))
}

/// Read the produced image back under `max_output_bytes` and wrap it
/// in a [`MediaOutput`]. Non-fatal ffmpeg stderr becomes a warning.
fn finish_output(
    dir: &TempDir,
    name: &str,
    mime: &'static str,
    stderr: String,
    limits: &MediaLimits,
) -> MediaResult<MediaOutput> {
    let bytes = dir.read_limited(name, limits.max_output_bytes)?;
    let mut out = MediaOutput::single(name.to_string(), bytes);
    if let Some(file) = out.primary.as_mut() {
        file.content_type = Some(mime.to_string());
    }
    if !stderr.is_empty() {
        out.warnings.push(stderr);
    }
    Ok(out)
}

/// Sniff a container extension from magic bytes so ffmpeg gets a
/// format hint. `.bin` fallback — ffmpeg probes content itself.
fn sniff_extension(data: &[u8]) -> &'static str {
    if data.len() >= 12 && &data[4..8] == b"ftyp" {
        return if &data[8..12] == b"qt  " {
            ".mov"
        } else {
            ".mp4"
        };
    }
    if data.starts_with(&[0x1A, 0x45, 0xDF, 0xA3]) {
        return ".mkv";
    }
    if data.starts_with(b"RIFF") && data.len() >= 12 {
        return match &data[8..12] {
            b"AVI " => ".avi",
            _ => ".bin",
        };
    }
    if data.starts_with(b"OggS") {
        return ".ogg";
    }
    ".bin"
}

// ------------------------------------------------------------------
// tests
// ------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;
    use std::path::Path;

    /// Generate a tiny mp4 with ffmpeg lavfi: `secs` seconds of
    /// 64x64@10fps testsrc + 440Hz sine → h264/aac. Returns `None`
    /// when ffmpeg or the encoders are unavailable so tests skip
    /// gracefully on minimal builds.
    fn make_clip(secs: u32) -> Option<Vec<u8>> {
        let ffmpeg = crate::crates::multimedia_engine::runtime::discovery::find_tool("ffmpeg")?;
        let dir = TempDir::new("thumb-test").ok()?;
        let out_path = dir.file("clip.mp4");
        let dur = format!("duration={secs}");
        let args: Vec<OsString> = [
            "-y",
            "-v",
            "error",
            "-f",
            "lavfi",
            "-i",
            &format!("testsrc=size=64x64:rate=10:{dur}"),
            "-f",
            "lavfi",
            "-i",
            &format!("sine=frequency=440:{dur}"),
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
        let out = process::run("ffmpeg", &ffmpeg, &args, None, &limits).ok()?;
        if !out.success {
            return None;
        }
        std::fs::read(&out_path).ok()
    }

    fn runtime() -> Option<Runtime> {
        Runtime::discover().ok()
    }

    fn primary_bytes(out: &MediaOutput) -> &[u8] {
        &out.primary.as_ref().expect("primary output").bytes
    }

    #[test]
    fn thumbnail_middle_frame_is_jpeg() {
        let Some(clip) = make_clip(2) else {
            eprintln!("skipping: ffmpeg lavfi/x264 unavailable");
            return;
        };
        let Some(rt) = runtime() else {
            eprintln!("skipping: ffmpeg/ffprobe not on PATH");
            return;
        };
        let opts = ThumbnailOptions::default(); // jpg, middle of video
        let out = thumbnail(&clip, &opts, &MediaLimits::default(), &rt).expect("thumbnail");
        let file = out.primary.as_ref().expect("primary");
        assert!(file.name.ends_with(".jpg"), "name {}", file.name);
        assert_eq!(file.content_type.as_deref(), Some("image/jpeg"));
        let bytes = primary_bytes(&out);
        assert!(bytes.len() > 100, "suspiciously small jpeg");
        assert_eq!(&bytes[..2], &[0xFF, 0xD8], "missing JPEG SOI magic");
        assert_eq!(&bytes[bytes.len() - 2..], &[0xFF, 0xD9], "missing JPEG EOI");
    }

    #[test]
    fn thumbnail_explicit_time_png_and_scale() {
        let Some(clip) = make_clip(2) else {
            eprintln!("skipping: ffmpeg lavfi/x264 unavailable");
            return;
        };
        let Some(rt) = runtime() else {
            eprintln!("skipping: ffmpeg/ffprobe not on PATH");
            return;
        };
        let opts = ThumbnailOptions {
            at_secs: Some(0.5),
            width: Some(32),
            height: Some(32),
            output_format: "png".to_string(),
            quality: None,
        };
        let out = thumbnail(&clip, &opts, &MediaLimits::default(), &rt).expect("thumbnail png");
        let bytes = primary_bytes(&out);
        assert_eq!(
            &bytes[..8],
            &[0x89, b'P', b'N', b'G', 0x0D, 0x0A, 0x1A, 0x0A],
            "missing PNG magic"
        );
    }

    #[test]
    fn storyboard_tiles_frames_into_png() {
        let Some(clip) = make_clip(2) else {
            eprintln!("skipping: ffmpeg lavfi/x264 unavailable");
            return;
        };
        let Some(rt) = runtime() else {
            eprintln!("skipping: ffmpeg/ffprobe not on PATH");
            return;
        };
        let opts = ThumbnailOptions {
            output_format: "png".to_string(),
            ..ThumbnailOptions::default()
        };
        let out =
            storyboard(&clip, (2, 2), &opts, &MediaLimits::default(), &rt).expect("storyboard");
        let bytes = primary_bytes(&out);
        assert!(bytes.len() > 500, "suspiciously small sheet");
        assert_eq!(
            &bytes[..8],
            &[0x89, b'P', b'N', b'G', 0x0D, 0x0A, 0x1A, 0x0A],
            "missing PNG magic"
        );
    }

    #[test]
    fn thumbnail_rejects_garbage_input() {
        let Some(rt) = runtime() else {
            eprintln!("skipping: ffmpeg/ffprobe not on PATH");
            return;
        };
        let garbage = vec![0xDEu8, 0xAD, 0xBE, 0xEF, 1, 2, 3, 4, 5, 6, 7, 8];
        let err = thumbnail(
            &garbage,
            &ThumbnailOptions::default(),
            &MediaLimits::default(),
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
    }
}
