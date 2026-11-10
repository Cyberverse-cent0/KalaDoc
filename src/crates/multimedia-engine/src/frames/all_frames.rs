//! Extract EVERY frame of a video as a numbered image sequence.
//!
//! [`all_frames`] writes the input buffer to a temp file and runs
//! `ffmpeg -i <in> -vsync 0 -start_number 1 frame_%06d.<ext>` so the
//! `image2` muxer emits one file per decoded frame in presentation
//! order. An optional `select`/`scale` filter chain trims the range
//! (`start_secs`/`end_secs`) and/or downscales (`scale`).
//!
//! Every produced image is read back under
//! [`MediaLimits::max_output_bytes`], the file count is capped by
//! [`MediaLimits::max_output_files`] and
//! [`AllFramesOptions::max_frames`], and the first image becomes the
//! primary output with the rest in [`MediaOutput::extra`].

use std::ffi::OsString;

use serde::{Deserialize, Serialize};

use crate::crates::multimedia_engine::error::{MediaError, MediaResult};
use crate::crates::multimedia_engine::model::{MediaFile, MediaLimits, MediaOutput};
use crate::crates::multimedia_engine::runtime::discovery::Runtime;
use crate::crates::multimedia_engine::runtime::process;
use crate::crates::multimedia_engine::runtime::tempdir::TempDir;

/// Options for [`all_frames`].
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct AllFramesOptions {
    /// Output image format without a leading dot. Only `"png"` and
    /// `"jpg"`/`"jpeg"` are supported; anything else is
    /// [`MediaError::Unsupported`]. Defaults to `"png"`.
    pub format: String,
    /// Only extract frames at or after this many seconds into the
    /// input (`-ss`, input seeking). Must be non-negative and finite.
    pub start_secs: Option<f64>,
    /// Only extract frames before this many seconds into the input
    /// (`-t` = `end_secs - start_secs`). Must be finite and greater
    /// than `start_secs` when both are set.
    pub end_secs: Option<f64>,
    /// Maximum number of frames to keep. `None` keeps all. Exceeding
    /// it — like exceeding `MediaLimits::max_output_files` — is a
    /// [`MediaError::LimitExceeded`], not a silent truncation.
    pub max_frames: Option<usize>,
    /// Optional exact output size `(width, height)` applied with the
    /// `scale` filter. `None` keeps the source dimensions; both
    /// components must be greater than 0.
    pub scale: Option<(u32, u32)>,
}

impl Default for AllFramesOptions {
    fn default() -> Self {
        Self {
            format: "png".to_string(),
            start_secs: None,
            end_secs: None,
            max_frames: None,
            scale: None,
        }
    }
}

/// Extract every frame of `input` as a numbered image sequence.
///
/// Runs `ffmpeg -hide_banner -loglevel error -nostdin -y [-ss S]
/// -i <in> [-vf scale=W:H] -vsync 0 [-frames:v N] [-t D]
/// -start_number 1 frame_%06d.<ext>` inside a temp dir, then reads the
/// images back in numeric order. The first frame is the primary
/// output; additional frames go to [`MediaOutput::extra`].
///
/// # Errors
///
/// - [`MediaError::InvalidInput`] — empty/oversized input, non-finite
///   or negative `start_secs`/`end_secs`, `end_secs <= start_secs`,
///   or a zero `scale` dimension.
/// - [`MediaError::Unsupported`] — `format` is not `png`/`jpg`, the
///   `image2` muxer is missing, or the image encoder is not in this
///   FFmpeg build's capability set.
/// - [`MediaError::ToolFailed`] — ffmpeg failed or produced no frames.
/// - [`MediaError::LimitExceeded`] — produced frames exceed
///   `options.max_frames` or `limits.max_output_files`, or total
///   output bytes exceed `limits.max_output_bytes`.
pub fn all_frames(
    input: &[u8],
    options: &AllFramesOptions,
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

    // --- validate time range ------------------------------------------------
    let start = options.start_secs.unwrap_or(0.0);
    if let Some(s) = options.start_secs
        && (!s.is_finite() || s < 0.0)
    {
        return Err(MediaError::InvalidInput(format!(
            "start_secs must be a non-negative finite number, got {s}"
        )));
    }
    if let Some(e) = options.end_secs {
        if !e.is_finite() || e < 0.0 {
            return Err(MediaError::InvalidInput(format!(
                "end_secs must be a non-negative finite number, got {e}"
            )));
        }
        if e <= start {
            return Err(MediaError::InvalidInput(format!(
                "end_secs ({e}) must be greater than start_secs ({start})"
            )));
        }
    }

    // --- validate scale -------------------------------------------------------
    if let Some((w, h)) = options.scale
        && (w == 0 || h == 0)
    {
        return Err(MediaError::InvalidInput(
            "scale dimensions must be greater than 0".to_string(),
        ));
    }

    // --- map format to (encoder, extension); only png/jpg supported -----------
    let format = options
        .format
        .trim()
        .trim_start_matches('.')
        .to_ascii_lowercase();
    let (codec, ext) = match format.as_str() {
        "png" => ("png", "png"),
        "jpg" | "jpeg" => ("mjpeg", "jpg"),
        other => {
            return Err(MediaError::Unsupported(format!(
                "all_frames supports only 'png' and 'jpg' output, got '{other}'"
            )));
        }
    };

    // Honest capabilities: image2 muxer + the image encoder.
    let caps = rt.capabilities()?;
    if !caps.has_muxer("image2") {
        return Err(MediaError::Unsupported(
            "muxer 'image2' is not available in this FFmpeg build".to_string(),
        ));
    }
    if !caps.has_encoder(codec) {
        return Err(MediaError::Unsupported(format!(
            "image encoder '{codec}' (from format '{format}') is not available in this FFmpeg build"
        )));
    }

    let dir = TempDir::new("all-frames")?;
    let input_name = "input.bin";
    dir.write(input_name, input)?;

    // `%06d` zero-pads frame numbers; `-start_number 1` makes the
    // first frame `frame_000001.<ext>`.
    let pattern_name = format!("frame_%06d.{ext}");
    let pattern_path = dir.file(&pattern_name);

    // Frame-count bound: request one more than allowed so exceeding
    // the cap is detectable after collection.
    let allowed_frames = match (
        options.max_frames.map(|v| v as u64),
        limits.max_output_files.map(u64::from),
    ) {
        (None, None) => u64::MAX,
        (a, b) => [a, b]
            .into_iter()
            .flatten()
            .min()
            .expect("at least one cap is set"),
    };
    let frame_cap = (allowed_frames != u64::MAX).then_some(allowed_frames.saturating_add(1));

    // --- build the filter chain ----------------------------------------------
    // Only `scale` is needed — `select` is unnecessary because every
    // decoded frame is kept; the time range is handled by `-ss`/`-t`.
    let mut filters: Vec<String> = Vec::new();
    if let Some((w, h)) = options.scale {
        filters.push(format!("scale={w}:{h}"));
    }
    let filter_chain = filters.join(",");

    // --- argv ------------------------------------------------------------------
    let mut argv: Vec<OsString> = vec![
        OsString::from("-hide_banner"),
        OsString::from("-loglevel"),
        OsString::from("error"),
        OsString::from("-nostdin"),
        OsString::from("-y"),
    ];
    if start > 0.0 {
        argv.push(OsString::from("-ss"));
        argv.push(OsString::from(format_secs(start)));
    }
    argv.push(OsString::from("-i"));
    argv.push(dir.file(input_name).into_os_string());
    argv.push(OsString::from("-map"));
    argv.push(OsString::from("0:v:0"));
    if !filter_chain.is_empty() {
        argv.push(OsString::from("-vf"));
        argv.push(OsString::from(filter_chain));
    }
    // `-vsync 0` passes every decoded frame through to the muxer
    // without duplicating or dropping to hit a constant rate.
    argv.push(OsString::from("-vsync"));
    argv.push(OsString::from("0"));
    if let Some(e) = options.end_secs {
        argv.push(OsString::from("-t"));
        argv.push(OsString::from(format_secs(e - start)));
    }
    if let Some(cap) = frame_cap {
        argv.push(OsString::from("-frames:v"));
        argv.push(OsString::from(cap.to_string()));
    }
    argv.push(OsString::from("-an"));
    argv.push(OsString::from("-c:v"));
    argv.push(OsString::from(codec));
    argv.push(OsString::from("-f"));
    argv.push(OsString::from("image2"));
    argv.push(OsString::from("-start_number"));
    argv.push(OsString::from("1"));
    argv.push(pattern_path.into_os_string());

    let out = process::run_checked("ffmpeg", &rt.ffmpeg, &argv, None, limits)?;

    // --- collect produced frames -----------------------------------------------
    let files = collect_frames(dir.path(), "frame", &ext)?;
    if files.is_empty() {
        return Err(MediaError::ToolFailed {
            tool: "ffmpeg",
            status: "no frames produced".to_string(),
            detail: format!(
                "ffmpeg produced no {pattern_name} files{}",
                if out.stderr.is_empty() {
                    String::new()
                } else {
                    format!(": {}", out.stderr)
                }
            ),
        });
    }

    // Enforce the frame-count caps explicitly (the -frames:v bound is
    // allowed+1, so count > allowed means the cap was exceeded).
    if (files.len() as u64) > allowed_frames {
        return Err(MediaError::LimitExceeded(format!(
            "frame extraction produced {} frames, exceeding the cap of {allowed_frames}",
            files.len()
        )));
    }

    // Read frames back under the byte cap; primary = first frame.
    let mut media_files: Vec<MediaFile> = Vec::with_capacity(files.len());
    let mut total_bytes: u64 = 0;
    for name in &files {
        let remaining = limits
            .max_output_bytes
            .map(|max| max.saturating_sub(total_bytes));
        let bytes = match remaining {
            Some(0) => {
                return Err(MediaError::LimitExceeded(format!(
                    "frame outputs exceed max_output_bytes {}",
                    limits.max_output_bytes.unwrap_or_default()
                )));
            }
            budget => dir.read_limited(name, budget)?,
        };
        total_bytes = total_bytes.saturating_add(bytes.len() as u64);
        media_files.push(MediaFile {
            name: name.clone(),
            bytes,
            content_type: content_type_for(&ext),
        });
    }

    let count = media_files.len();
    let mut iter = media_files.into_iter();
    let primary = iter.next();
    let mut result = MediaOutput {
        primary,
        extra: iter.collect(),
        warnings: Vec::new(),
    };
    result.warnings.push(format!("extracted {count} frames"));
    if !out.stderr.is_empty() {
        result.warnings.push(out.stderr);
    }
    Ok(result)
}

/// List `frame_*.ext` files in `dir`, sorted by the numeric sequence
/// component (so `frame_1000000.png` sorts after `frame_999999.png`).
fn collect_frames(dir: &std::path::Path, prefix: &str, ext: &str) -> MediaResult<Vec<String>> {
    let suffix = format!(".{ext}");
    let mut found: Vec<(u64, String)> = Vec::new();
    for entry in std::fs::read_dir(dir)? {
        let entry = entry?;
        let name = entry.file_name().to_string_lossy().into_owned();
        let Some(rest) = name
            .strip_prefix(&format!("{prefix}_"))
            .and_then(|r| r.strip_suffix(&suffix))
        else {
            continue;
        };
        if entry.file_type()?.is_file()
            && let Ok(num) = rest.parse::<u64>()
        {
            found.push((num, name));
        }
    }
    found.sort_by_key(|(num, _)| *num);
    Ok(found.into_iter().map(|(_, name)| name).collect())
}

/// Format seconds compactly for `-ss` / `-t`.
fn format_secs(secs: f64) -> String {
    let rounded = (secs * 1000.0).round() / 1000.0;
    if rounded.fract() == 0.0 {
        format!("{}", rounded as i64)
    } else {
        format!("{rounded}")
    }
}

/// Best-effort MIME type for common image extensions.
fn content_type_for(ext: &str) -> Option<String> {
    let mime = match ext.to_ascii_lowercase().as_str() {
        "png" => "image/png",
        "jpg" | "jpeg" => "image/jpeg",
        "webp" => "image/webp",
        "bmp" => "image/bmp",
        "gif" => "image/gif",
        "tif" | "tiff" => "image/tiff",
        _ => return None,
    };
    Some(mime.to_string())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::crates::multimedia_engine::runtime::process::run;
    use std::path::PathBuf;

    fn runtime() -> Runtime {
        Runtime::discover().expect("ffmpeg/ffprobe must be on PATH")
    }

    fn limits() -> MediaLimits {
        MediaLimits::default()
    }

    /// Generate a 1s x 10fps testsrc mp4 via lavfi. Returns `None`
    /// when ffmpeg is unavailable so tests skip gracefully.
    fn sample_input() -> Option<Vec<u8>> {
        let rt = runtime();
        let dir = TempDir::new("all-frames-test-in").ok()?;
        let out_path: PathBuf = dir.file("sample.mp4");
        let argv = process::args(&[
            "-hide_banner",
            "-loglevel",
            "error",
            "-nostdin",
            "-y",
            "-f",
            "lavfi",
            "-i",
            "testsrc=duration=1:size=160x120:rate=10",
            "-c:v",
            "mpeg4",
            "-pix_fmt",
            "yuv420p",
            out_path.to_str()?,
        ]);
        let out = run("ffmpeg", &rt.ffmpeg, &argv, None, &limits()).ok()?;
        if !out.success {
            return None;
        }
        std::fs::read(&out_path).ok()
    }

    #[test]
    fn all_frames_extracts_every_frame_as_png() {
        let Some(input) = sample_input() else {
            eprintln!("skipping: ffmpeg unavailable");
            return;
        };
        let rt = runtime();
        let options = AllFramesOptions::default();
        let out = all_frames(&input, &options, &limits(), &rt).expect("all_frames");

        let primary = out.primary.as_ref().expect("primary frame");
        assert!(
            primary.name.starts_with("frame_") && primary.name.ends_with(".png"),
            "unexpected name {}",
            primary.name
        );
        // PNG magic bytes.
        assert!(
            primary.bytes.starts_with(&[0x89, b'P', b'N', b'G']),
            "primary is not a PNG"
        );
        assert_eq!(primary.content_type.as_deref(), Some("image/png"));

        // 1 s @ 10 fps ⇒ ~10 frames total.
        let total = 1 + out.extra.len();
        assert!(
            (8..=12).contains(&total),
            "expected ~10 frames, got {total}"
        );
        for f in out.files() {
            assert!(
                f.bytes.starts_with(&[0x89, b'P', b'N', b'G']),
                "{} is not a PNG",
                f.name
            );
        }
        // Numbered names: frame_000001.png ... frame_NNNNNN.png.
        assert_eq!(primary.name, format!("frame_{:06}.png", 1));
    }

    #[test]
    fn all_frames_respects_max_frames_and_rejects_bad_input() {
        let Some(input) = sample_input() else {
            eprintln!("skipping: ffmpeg unavailable");
            return;
        };
        let rt = runtime();

        // max_frames caps the produced images.
        let options = AllFramesOptions {
            max_frames: Some(3),
            ..Default::default()
        };
        let out = all_frames(&input, &options, &limits(), &rt).expect("all_frames");
        assert!(out.primary.is_some());
        assert_eq!(out.extra.len(), 2, "max_frames=3 must yield 3 files");

        // Empty input is rejected before touching ffmpeg.
        let err = all_frames(&[], &options, &limits(), &rt).expect_err("must fail");
        assert!(matches!(err, MediaError::InvalidInput(_)), "{err:?}");

        // Unsupported format is rejected before running ffmpeg.
        let options = AllFramesOptions {
            format: "tiff".to_string(),
            ..Default::default()
        };
        let err = all_frames(&input, &options, &limits(), &rt).expect_err("must fail");
        assert!(matches!(err, MediaError::Unsupported(_)), "{err:?}");

        // Garbage input fails inside ffmpeg.
        let garbage = vec![0xDEu8, 0xAD, 0xBE, 0xEF, 1, 2, 3, 4];
        let options = AllFramesOptions::default();
        let err = all_frames(&garbage, &options, &limits(), &rt).expect_err("must fail");
        assert!(
            matches!(
                err,
                MediaError::ToolFailed { .. } | MediaError::InvalidInput(_)
            ),
            "unexpected error: {err:?}"
        );
    }
}
