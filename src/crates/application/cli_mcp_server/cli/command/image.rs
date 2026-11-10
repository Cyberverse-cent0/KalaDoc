//! `kaladoc image inspect|resize|convert`
//!
//! Thin adapter over [`ImageEngine`]: reads the input file, calls the
//! matching engine method, and either prints the [`ImageInfo`] report
//! or writes the produced bytes to `--output-file` (or a derived path next
//! to the input). No configuration or database access is needed.

use std::path::{Path, PathBuf};

use serde::Serialize;

use crate::crates::image_engine::{ImageEngine, ImageError, ResizeOptions};

use super::super::args::ImageArgs;
use super::{CliError, Context};

pub fn run(ctx: &Context, args: &ImageArgs) -> Result<i32, CliError> {
    let engine = ImageEngine::new();
    match args {
        ImageArgs::Inspect { file } => inspect(ctx, &engine, file),
        ImageArgs::Resize {
            file,
            width,
            height,
            output_file: output,
        } => resize(ctx, &engine, file, *width, *height, output.as_deref()),
        ImageArgs::Convert {
            file,
            target,
            output_file: output,
        } => convert(ctx, &engine, file, target, output.as_deref()),
    }
}

// ---------------------------------------------------------------------------
// inspect
// ---------------------------------------------------------------------------

fn inspect(ctx: &Context, engine: &ImageEngine, file: &Path) -> Result<i32, CliError> {
    let input = read_input(file)?;
    let info = engine.inspect(&input).map_err(map_image_error)?;

    if ctx.emit_structured(&info)? {
        return Ok(0);
    }
    println!("file: {}", file.display());
    println!("format: {}", info.format);
    println!("dimensions: {}x{}", info.width, info.height);
    println!("pixel_count: {}", info.pixel_count);
    println!("file_size_bytes: {}", info.file_size_bytes);
    println!(
        "color_type: {}",
        info.color_type.as_deref().unwrap_or("unknown")
    );
    println!("has_alpha: {}", info.has_alpha);
    Ok(0)
}

// ---------------------------------------------------------------------------
// resize / convert
// ---------------------------------------------------------------------------

#[derive(Debug, Serialize)]
struct WriteReport {
    input: String,
    output: String,
    operation: &'static str,
    bytes_written: u64,
}

fn resize(
    ctx: &Context,
    engine: &ImageEngine,
    file: &Path,
    width: Option<u32>,
    height: Option<u32>,
    output: Option<&Path>,
) -> Result<i32, CliError> {
    if width.is_none() && height.is_none() {
        return Err(CliError::Usage(
            "at least one of --width or --height is required".to_string(),
        ));
    }
    let input = read_input(file)?;
    let bytes = engine
        .resize(
            &input,
            ResizeOptions {
                max_width: width,
                max_height: height,
            },
        )
        .map_err(map_image_error)?;

    // Resized output keeps the input format; default to
    // `<stem>.resized.<ext>` so the input is never overwritten.
    let out = match output {
        Some(p) => p.to_path_buf(),
        None => resized_default(file)?,
    };
    write_output(&out, &bytes)?;
    report(ctx, file, &out, "resize", bytes.len())
}

fn convert(
    ctx: &Context,
    engine: &ImageEngine,
    file: &Path,
    target: &str,
    output: Option<&Path>,
) -> Result<i32, CliError> {
    let input = read_input(file)?;
    let bytes = engine.convert(&input, target).map_err(map_image_error)?;

    let ext = normalize_extension(target);
    let out = match output {
        Some(p) => p.to_path_buf(),
        None => file.with_extension(&ext),
    };
    if out.as_path() == file {
        return Err(CliError::Usage(format!(
            "output would overwrite the input {}; pass --output-file",
            file.display()
        )));
    }
    write_output(&out, &bytes)?;
    report(ctx, file, &out, "convert", bytes.len())
}

fn report(
    ctx: &Context,
    input: &Path,
    output: &Path,
    operation: &'static str,
    bytes: usize,
) -> Result<i32, CliError> {
    let report = WriteReport {
        input: input.display().to_string(),
        output: output.display().to_string(),
        operation,
        bytes_written: bytes as u64,
    };
    if ctx.emit_structured(&report)? {
        return Ok(0);
    }
    println!(
        "{} {} -> {} ({} bytes)",
        report.operation, report.input, report.output, report.bytes_written
    );
    Ok(0)
}

// ---------------------------------------------------------------------------
// Internals
// ---------------------------------------------------------------------------

fn read_input(file: &Path) -> Result<Vec<u8>, CliError> {
    std::fs::read(file).map_err(|e| {
        CliError::Io(std::io::Error::new(
            e.kind(),
            format!("cannot read {}: {e}", file.display()),
        ))
    })
}

fn write_output(path: &Path, bytes: &[u8]) -> Result<(), CliError> {
    std::fs::write(path, bytes).map_err(|e| {
        CliError::Io(std::io::Error::new(
            e.kind(),
            format!("cannot write {}: {e}", path.display()),
        ))
    })
}

/// `<stem>.resized.<ext>` next to the input; falls back to appending
/// `.resized` when the input has no usable name/extension.
fn resized_default(file: &Path) -> Result<PathBuf, CliError> {
    let name = file
        .file_name()
        .and_then(|n| n.to_str())
        .ok_or_else(|| CliError::Usage(format!("bad input path {}", file.display())))?;
    let out_name = if let Some((stem, ext)) = name.rsplit_once('.') {
        format!("{stem}.resized.{ext}")
    } else {
        format!("{name}.resized")
    };
    Ok(file.with_file_name(out_name))
}

/// Lowercased target name usable as a file extension (`" JPG "` →
/// `"jpg"`). The engine validates the format itself; this only shapes
/// the default output path.
fn normalize_extension(target: &str) -> String {
    target.trim().to_ascii_lowercase()
}

/// Maps [`ImageError`] onto the CLI contract: bad input is a usage
/// problem, unsupported formats are `unsupported`, encode failures are
/// internal.
fn map_image_error(e: ImageError) -> CliError {
    match e {
        ImageError::UnsupportedFormat(msg) => CliError::Unsupported(msg),
        ImageError::InvalidInput(msg) | ImageError::LimitExceeded(msg) => CliError::Validation(msg),
        ImageError::Decode(_) => CliError::Validation("invalid or unreadable image".to_string()),
        ImageError::Encode(_) => CliError::Internal("image encoding failed".to_string()),
    }
}
