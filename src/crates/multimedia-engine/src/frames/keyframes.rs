//! Keyframe (I-frame) extraction.
//!
//! [`keyframes`] writes the input buffer to a temp file and runs
//! `ffmpeg -i <in> -vf "select='eq(pict_type,I)'" -vsync vfr
//! <prefix>_%04d.<format>`. The `select` filter passes only
//! intra-coded frames through; `-vsync vfr` keeps each surviving
//! frame's original timestamp so no duplicates or drops are
//! synthesized. FFmpeg decodes every frame (it cannot know which are
//! keyframes without decoding) but only *writes* the I-frames.
//!
//! Every produced image is read back under
//! [`MediaLimits::max_output_bytes`], the file count is capped by
//! [`MediaLimits::max_output_files`] and
//! [`KeyframeOptions::max_frames`], and the first image becomes the
//! primary output with the rest in [`MediaOutput::extra`].

use std::ffi::OsString;

use serde::{Deserialize, Serialize};

use crate::crates::multimedia_engine::error::{MediaError, MediaResult};
use crate::crates::multimedia_engine::model::{MediaFile, MediaLimits, MediaOutput};
use crate::crates::multimedia_engine::runtime::discovery::Runtime;
use crate::crates::multimedia_engine::runtime::process;
use crate::crates::multimedia_engine::runtime::tempdir::TempDir;

/// Options for [`keyframes`].
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct KeyframeOptions {
    /// Output image format extension without a leading dot
    /// (`"png"`, `"jpg"`, `"webp"`, ...). Passed to the ffmpeg
    /// `image2` muxer via the output filename.
    pub output_format: String,
    /// File-name prefix for produced images; frame numbers are
    /// appended as `_<NNNN>` (e.g. `prefix="kf"`, `output_format=
    /// "png"` → `kf_0001.png`). Path separators and other unsafe
    /// characters are replaced with `_`.
    pub prefix: String,
    /// Maximum number of keyframes to keep. `None` keeps all.
    /// Implemented with ffmpeg `-frames:v`, so extraction stops
    /// early rather than decoding the whole file when the cap is
    /// reached.
    pub max_frames: Option<u32>,
}

/// Extract every keyframe (I-frame) of `input` as an image file.
///
/// Runs `ffmpeg -hide_banner -loglevel error -nostdin -y -i <in>
/// -vf select='eq(pict_type,I)' -vsync vfr [-frames:v N]
/// <prefix>_%04d.<format>` inside a temp dir, then reads the images
/// back in name order.
///
/// The primary output is the first keyframe; additional keyframes go
/// to [`MediaOutput::extra`]. [`MediaOutput::warnings`] reports the
/// extracted count.
///
/// # Errors
///
/// - [`MediaError::InvalidInput`] — empty/oversized input, empty
///   `output_format`, or a stream with no decodable video frames.
/// - [`MediaError::Unsupported`] — the `select` filter is not in
///   this FFmpeg build's capability set.
/// - [`MediaError::ToolFailed`] / [`MediaError::Timeout`] — ffmpeg
///   rejected the input.
/// - [`MediaError::LimitExceeded`] — input size, output file count or
///   output bytes exceeded.
pub fn keyframes(
    input: &[u8],
    options: &KeyframeOptions,
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
    let output_ext = options.output_format.trim().trim_start_matches('.');
    if output_ext.is_empty() {
        return Err(MediaError::InvalidInput(
            "output_format must not be empty".to_string(),
        ));
    }

    // Honest capabilities: the select filter is the only non-default
    // filtergraph feature this op requires.
    let caps = rt.capabilities()?;
    if !caps.has_filter("select") {
        return Err(MediaError::Unsupported(
            "filter 'select' is not available in this FFmpeg build".to_string(),
        ));
    }

    let dir = TempDir::new("keyframes")?;
    dir.write("input.bin", input)?;

    let prefix = sanitize_prefix(&options.prefix);
    let pattern_name = format!("{prefix}_%04d.{output_ext}");
    let pattern_path = dir.file(&pattern_name);

    let mut argv: Vec<OsString> = vec![
        OsString::from("-hide_banner"),
        OsString::from("-loglevel"),
        OsString::from("error"),
        OsString::from("-nostdin"),
        OsString::from("-y"),
        OsString::from("-i"),
        dir.file("input.bin").into_os_string(),
        // Single argv element — the quotes are part of the filter
        // expression syntax, not shell quoting.
        OsString::from("-vf"),
        OsString::from("select='eq(pict_type,I)'"),
        OsString::from("-vsync"),
        OsString::from("vfr"),
    ];
    if let Some(max) = options.max_frames {
        argv.push(OsString::from("-frames:v"));
        argv.push(OsString::from(max.to_string()));
    }
    argv.push(pattern_path.into_os_string());

    process::run_checked("ffmpeg", &rt.ffmpeg, &argv, None, limits)?;

    // Collect produced images: <prefix>_NNNN.<ext>, name-sorted so
    // frame order is preserved regardless of filesystem ordering.
    let mut produced: Vec<String> = Vec::new();
    let entries = std::fs::read_dir(dir.path())?;
    let name_prefix = format!("{prefix}_");
    let name_suffix = format!(".{output_ext}");
    for entry in entries {
        let entry = entry?;
        let name = entry.file_name();
        let Some(name) = name.to_str() else { continue };
        if name.starts_with(&name_prefix) && name.ends_with(&name_suffix) {
            produced.push(name.to_string());
        }
    }
    produced.sort();

    if produced.is_empty() {
        return Err(MediaError::InvalidInput(
            "input has no video stream or no keyframes were produced".to_string(),
        ));
    }

    if let Some(max_files) = limits.max_output_files
        && produced.len() as u64 > max_files as u64
    {
        return Err(MediaError::LimitExceeded(format!(
            "ffmpeg produced {} keyframe files, exceeding max_output_files {max_files}",
            produced.len()
        )));
    }

    let content_type = image_content_type(output_ext);
    let mut files = Vec::with_capacity(produced.len());
    for name in &produced {
        let bytes = dir.read_limited(name, limits.max_output_bytes)?;
        files.push(MediaFile {
            name: name.clone(),
            bytes,
            content_type: content_type.clone(),
        });
    }

    let count = files.len();
    let mut iter = files.into_iter();
    let primary = iter.next();
    let mut out = MediaOutput {
        primary,
        extra: iter.collect(),
        warnings: Vec::new(),
    };
    out.warnings.push(format!("extracted {count} keyframes"));
    Ok(out)
}

/// Reduce a caller-supplied prefix to safe file-name characters so it
/// cannot escape the temp dir via path separators. Falls back to
/// `"keyframe"` when nothing usable remains.
fn sanitize_prefix(prefix: &str) -> String {
    let cleaned: String = prefix
        .chars()
        .map(|c| {
            if c.is_ascii_alphanumeric() || matches!(c, '-' | '_' | '.') {
                c
            } else {
                '_'
            }
        })
        .collect();
    let cleaned = cleaned.trim_matches('.').trim_matches('_');
    if cleaned.is_empty() {
        "keyframe".to_string()
    } else {
        cleaned.to_string()
    }
}

/// Best-effort MIME type for common image output formats.
fn image_content_type(ext: &str) -> Option<String> {
    let mime = match ext.to_ascii_lowercase().as_str() {
        "png" => "image/png",
        "jpg" | "jpeg" => "image/jpeg",
        "webp" => "image/webp",
        "gif" => "image/gif",
        "bmp" => "image/bmp",
        "tif" | "tiff" => "image/tiff",
        _ => return None,
    };
    Some(mime.to_string())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::crates::multimedia_engine::runtime::discovery;
    use std::path::PathBuf;

    fn runtime() -> Runtime {
        Runtime::discover().expect("ffmpeg/ffprobe must be on PATH")
    }

    fn limits() -> MediaLimits {
        MediaLimits::default()
    }

    /// Generate a 2s x264 mp4 at 25fps with `-g 25` so keyframes land
    /// roughly once per second — well under the default GOP of 250,
    /// which would yield a single keyframe. Returns `None` when
    /// ffmpeg/libx264 are unavailable so tests skip gracefully.
    fn make_mp4() -> Option<Vec<u8>> {
        let ffmpeg = discovery::find_tool("ffmpeg")?;
        let dir = TempDir::new("keyframes-test-in").ok()?;
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
            "testsrc=duration=2:size=160x120:rate=25",
            "-c:v",
            "libx264",
            "-g",
            "25",
            "-pix_fmt",
            "yuv420p",
            out_path.to_str()?,
        ]);
        let out = process::run("ffmpeg", &ffmpeg, &argv, None, &limits()).ok()?;
        if !out.success {
            return None;
        }
        std::fs::read(&out_path).ok()
    }

    #[test]
    fn keyframes_extracts_multiple_iframes() {
        let Some(mp4) = make_mp4() else {
            eprintln!("skipping: ffmpeg/libx264 unavailable");
            return;
        };
        let rt = runtime();
        let options = KeyframeOptions {
            output_format: "png".to_string(),
            prefix: "kf".to_string(),
            max_frames: None,
        };
        let out = keyframes(&mp4, &options, &limits(), &rt).expect("keyframes");

        let primary = out.primary.expect("primary keyframe");
        assert!(
            primary.name.starts_with("kf_") && primary.name.ends_with(".png"),
            "unexpected name {}",
            primary.name
        );
        assert!(!primary.bytes.is_empty());
        assert_eq!(primary.content_type.as_deref(), Some("image/png"));

        // -g 25 at 25fps over 2s → at least 2 I-frames.
        let total = 1 + out.extra.len();
        assert!(total >= 2, "expected >=2 keyframes, got {total}");
        assert!(
            out.warnings.iter().any(|w| w.contains(&total.to_string())),
            "warnings should report count: {:?}",
            out.warnings
        );
        for f in &out.extra {
            assert!(f.name.starts_with("kf_"), "bad name {}", f.name);
            assert!(!f.bytes.is_empty());
        }
    }

    #[test]
    fn keyframes_respects_max_frames_and_rejects_empty() {
        let Some(mp4) = make_mp4() else {
            eprintln!("skipping: ffmpeg/libx264 unavailable");
            return;
        };
        let rt = runtime();

        // max_frames caps the produced images.
        let options = KeyframeOptions {
            output_format: "png".to_string(),
            prefix: "cap".to_string(),
            max_frames: Some(1),
        };
        let out = keyframes(&mp4, &options, &limits(), &rt).expect("keyframes");
        assert!(out.primary.is_some());
        assert!(out.extra.is_empty(), "max_frames=1 must yield one file");

        // Empty input is rejected before touching ffmpeg.
        let err = keyframes(&[], &options, &limits(), &rt).expect_err("must fail");
        assert!(matches!(err, MediaError::InvalidInput(_)), "{err:?}");

        // Garbage input fails inside ffmpeg.
        let garbage = vec![0xDEu8, 0xAD, 0xBE, 0xEF, 1, 2, 3, 4];
        let err = keyframes(&garbage, &options, &limits(), &rt).expect_err("must fail");
        assert!(
            matches!(
                err,
                MediaError::ToolFailed { .. } | MediaError::InvalidInput(_)
            ),
            "unexpected error: {err:?}"
        );
    }
}
