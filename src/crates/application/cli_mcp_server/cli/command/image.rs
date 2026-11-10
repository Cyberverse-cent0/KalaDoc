//! `kaladoc image <op>` — local-file raster image operations via
//! `ImageEngine`.
//!
//! Commands read an image file from disk, run the synchronous
//! image-engine operation on a blocking thread, and write byte
//! outputs to `--output-file` or emit structured reports as JSON.
//! Supported codecs: PNG, JPEG, GIF, WebP, BMP.

use std::fs;
use std::path::Path;

use serde::Serialize;

use crate::crates::core::engine::EngineLimits;
use crate::crates::image_engine::{
    CompressOptions, ImageEngine, ImageError, ImageFormat, ImageResult, ResizeOptions,
};

use super::super::args::ImageArgs;
use super::{CliError, Context};

/// Input-size/pixel caps mirroring the pdf command's limits — generous
/// for interactive CLI use while still bounding hostile files.
fn limits() -> EngineLimits {
    EngineLimits {
        max_input_bytes: Some(256 * 1024 * 1024),
        max_pixels: Some(100_000_000),
        ..Default::default()
    }
}

fn engine() -> ImageEngine {
    ImageEngine::with_limits(limits())
}

pub async fn run(ctx: &Context, args: &ImageArgs) -> Result<i32, CliError> {
    match args {
        ImageArgs::Inspect { input } => {
            let data = read_file(input)?;
            let info = blocking(move || engine().inspect(&data)).await?;
            emit_report(ctx, &info)?;
            Ok(0)
        }
        ImageArgs::Resize {
            input,
            width,
            height,
            output_file,
        } => {
            if width.is_none() && height.is_none() {
                return Err(CliError::Usage(
                    "resize requires --width and/or --height".to_string(),
                ));
            }
            let data = read_file(input)?;
            let options = ResizeOptions {
                max_width: *width,
                max_height: *height,
            };
            let out = blocking(move || engine().resize(&data, options)).await?;
            write_file(output_file, &out)?;
            Ok(0)
        }
        ImageArgs::Convert {
            input,
            to,
            output_file,
        } => {
            let data = read_file(input)?;
            let target = to.clone();
            // `convert` takes the format name directly and validates it.
            let out = blocking(move || engine().convert(&data, &target)).await?;
            write_file(output_file, &out)?;
            Ok(0)
        }
        ImageArgs::Rotate {
            input,
            degrees,
            output_file,
        } => {
            let data = read_file(input)?;
            let degrees = *degrees;
            let out = blocking(move || engine().rotate(&data, degrees)).await?;
            write_file(output_file, &out)?;
            Ok(0)
        }
        ImageArgs::Compress {
            input,
            format,
            quality,
            output_file,
        } => {
            let data = read_file(input)?;
            let options = CompressOptions {
                format: format.as_deref().map(parse_format).transpose()?,
                quality: *quality,
            };
            let out = blocking(move || engine().compress(&data, options)).await?;
            write_file(output_file, &out)?;
            Ok(0)
        }
        ImageArgs::Thumbnail {
            input,
            width,
            height,
            output_file,
        } => {
            if *width == 0 || *height == 0 {
                return Err(CliError::Usage(
                    "thumbnail --width and --height must be positive".to_string(),
                ));
            }
            let data = read_file(input)?;
            let (w, h) = (*width, *height);
            let out = blocking(move || engine().thumbnail(&data, w, h)).await?;
            write_file(output_file, &out)?;
            Ok(0)
        }
        ImageArgs::Metadata { input } => {
            let data = read_file(input)?;
            let meta = blocking(move || engine().metadata(&data)).await?;
            emit_report(ctx, &meta)?;
            Ok(0)
        }
        ImageArgs::Validate { input } => {
            let data = read_file(input)?;
            let report = blocking(move || engine().validate(&data)).await?;
            emit_report(ctx, &report)?;
            Ok(0)
        }
    }
}

// ---------------------------------------------------------------------------
// Helpers
// ---------------------------------------------------------------------------

/// Run a synchronous engine call on the blocking pool.
async fn blocking<F, R>(f: F) -> Result<R, CliError>
where
    F: FnOnce() -> ImageResult<R> + Send + 'static,
    R: Send + 'static,
{
    tokio::task::spawn_blocking(f)
        .await
        .map_err(|e| CliError::Internal(format!("image worker task failed: {e}")))?
        .map_err(image_err)
}

fn image_err(e: ImageError) -> CliError {
    match e {
        ImageError::UnsupportedFormat(m) => CliError::Unsupported(m),
        ImageError::InvalidInput(m) => CliError::Usage(m),
        ImageError::LimitExceeded(m) => CliError::Validation(m),
        ImageError::Decode(_) => CliError::Validation("invalid or unreadable image".to_string()),
        ImageError::Encode(_) => CliError::Internal("image encoding failed".to_string()),
        ImageError::Io(io) => CliError::Io(io),
        ImageError::ExternalTool { tool, detail } => {
            CliError::Internal(format!("external tool {tool} failed: {detail}"))
        }
    }
}

fn read_file(path: &Path) -> Result<Vec<u8>, CliError> {
    fs::read(path).map_err(|e| {
        CliError::Io(std::io::Error::new(
            e.kind(),
            format!("{}: {e}", path.display()),
        ))
    })
}

fn write_file(path: &Path, bytes: &[u8]) -> Result<(), CliError> {
    if let Some(parent) = path.parent()
        && !parent.as_os_str().is_empty()
    {
        fs::create_dir_all(parent)?;
    }
    // Write-then-rename keeps a failed write from leaving a truncated
    // output at the target path.
    let tmp = path.with_extension("kaladoc-tmp");
    fs::write(&tmp, bytes)?;
    fs::rename(&tmp, path)?;
    Ok(())
}

/// Parse a `--format` identifier for `compress`. `ImageFormat`'s own
/// parser is crate-private, so the accepted names are mirrored here.
fn parse_format(s: &str) -> Result<ImageFormat, CliError> {
    match s.trim().to_ascii_lowercase().as_str() {
        "png" => Ok(ImageFormat::Png),
        "jpg" | "jpeg" => Ok(ImageFormat::Jpeg),
        "gif" => Ok(ImageFormat::Gif),
        "webp" => Ok(ImageFormat::WebP),
        "bmp" => Ok(ImageFormat::Bmp),
        other => Err(CliError::Usage(format!(
            "unknown format {other:?}; expected png|jpeg|gif|webp|bmp"
        ))),
    }
}

/// Reports are JSON-first: structured formats get the typed value;
/// table mode falls back to pretty JSON (these are data reports, not
/// human tables).
fn emit_report<T: Serialize>(ctx: &Context, report: &T) -> Result<(), CliError> {
    if ctx.emit_structured(report)? {
        return Ok(());
    }
    println!("{}", serde_json::to_string_pretty(report)?);
    Ok(())
}
