//! `kaladoc video probe|thumbnail|trim|transcode|concat|extract-audio`
//!
//! Thin adapter over [`VideoEngine`]: reads the input file(s), calls
//! the matching engine method, and either prints the [`VideoInfo`]
//! report or writes the produced bytes to `--output-file` (or a derived
//! path next to the input). No configuration or database access is
//! needed.

use std::path::{Path, PathBuf};

use serde::Serialize;

use crate::crates::core::engine::EngineLimits;
use crate::crates::video_engine::{VideoEngine, VideoError};

use super::super::args::VideoArgs;
use super::{CliError, Context};

pub fn run(ctx: &Context, args: &VideoArgs) -> Result<i32, CliError> {
    let engine = VideoEngine::new();
    match args {
        VideoArgs::Probe { file } => probe(ctx, &engine, file),
        VideoArgs::Thumbnail {
            file,
            at,
            output_file: output,
        } => thumbnail(ctx, &engine, file, *at, output.as_deref()),
        VideoArgs::Trim {
            file,
            start,
            end,
            reencode,
            output_file: output,
        } => trim(ctx, &engine, file, *start, *end, *reencode, output.as_deref()),
        VideoArgs::Transcode {
            file,
            target_ext,
            output_file: output,
        } => transcode(ctx, &engine, file, target_ext, output.as_deref()),
        VideoArgs::Concat {
            files,
            target_ext,
            output_file: output,
        } => concat(ctx, &engine, files, target_ext, output.as_deref()),
        VideoArgs::ExtractAudio {
            file,
            ext,
            output_file: output,
        } => extract_audio(ctx, &engine, file, ext, output.as_deref()),
    }
}

// ---------------------------------------------------------------------------
// probe
// ---------------------------------------------------------------------------

fn probe(ctx: &Context, engine: &VideoEngine, file: &Path) -> Result<i32, CliError> {
    let input = read_input(file)?;
    let info = engine.probe(&input).map_err(map_video_error)?;

    if ctx.emit_structured(&info)? {
        return Ok(0);
    }
    println!("file: {}", file.display());
    // Table fallback: render the serialized fields as key/value lines.
    // Going through serde_json keeps this robust to Option fields.
    let v = serde_json::to_value(&info)?;
    for key in [
        "duration_secs",
        "width",
        "height",
        "fps",
        "vcodec",
        "acodec",
        "format_name",
        "size_bytes",
        "has_audio",
    ] {
        if let Some(val) = v.get(key) {
            println!("{key}: {}", render_value(val));
        }
    }
    Ok(0)
}

// ---------------------------------------------------------------------------
// thumbnail / trim / transcode / concat / extract-audio
// ---------------------------------------------------------------------------

#[derive(Debug, Serialize)]
struct WriteReport {
    input: String,
    output: String,
    operation: &'static str,
    bytes_written: u64,
}

fn thumbnail(
    ctx: &Context,
    engine: &VideoEngine,
    file: &Path,
    at: Option<f64>,
    output: Option<&Path>,
) -> Result<i32, CliError> {
    let input = read_input(file)?;
    let bytes = engine
        .thumbnail(&input, at, &EngineLimits::default())
        .map_err(map_video_error)?;

    let out = match output {
        Some(p) => p.to_path_buf(),
        None => marked_default(file, "thumb", "png")?,
    };
    check_output(&out, &[file])?;
    write_output(&out, &bytes)?;
    report(ctx, file, &out, "thumbnail", bytes.len())
}

fn trim(
    ctx: &Context,
    engine: &VideoEngine,
    file: &Path,
    start: f64,
    end: Option<f64>,
    reencode: bool,
    output: Option<&Path>,
) -> Result<i32, CliError> {
    if start < 0.0 {
        return Err(CliError::Usage("--start must be >= 0".to_string()));
    }
    if let Some(end) = end {
        if end <= start {
            return Err(CliError::Usage(
                "--end must be greater than --start".to_string(),
            ));
        }
    }
    let input = read_input(file)?;
    let bytes = engine
        .trim(&input, start, end, reencode, &EngineLimits::default())
        .map_err(map_video_error)?;

    // Trimmed output keeps the input container; default to
    // `<stem>.trimmed.<ext>` so the input is never overwritten.
    let out = match output {
        Some(p) => p.to_path_buf(),
        None => trimmed_default(file)?,
    };
    check_output(&out, &[file])?;
    write_output(&out, &bytes)?;
    report(ctx, file, &out, "trim", bytes.len())
}

fn transcode(
    ctx: &Context,
    engine: &VideoEngine,
    file: &Path,
    target_ext: &str,
    output: Option<&Path>,
) -> Result<i32, CliError> {
    let ext = sanitize_extension(target_ext)?;
    let input = read_input(file)?;
    let bytes = engine
        .transcode(&input, &ext, &EngineLimits::default())
        .map_err(map_video_error)?;

    let out = match output {
        Some(p) => p.to_path_buf(),
        None => file.with_extension(&ext),
    };
    check_output(&out, &[file])?;
    write_output(&out, &bytes)?;
    report(ctx, file, &out, "transcode", bytes.len())
}

fn concat(
    ctx: &Context,
    engine: &VideoEngine,
    files: &[PathBuf],
    target_ext: &str,
    output: Option<&Path>,
) -> Result<i32, CliError> {
    if files.len() < 2 {
        return Err(CliError::Usage(
            "concat requires at least two input files".to_string(),
        ));
    }
    let ext = sanitize_extension(target_ext)?;
    let mut inputs = Vec::with_capacity(files.len());
    for f in files {
        inputs.push(read_input(f)?);
    }
    let bytes = engine
        .concat(&inputs, &ext, &EngineLimits::default())
        .map_err(map_video_error)?;

    let out = match output {
        Some(p) => p.to_path_buf(),
        None => files[0].with_file_name(format!("concat.{ext}")),
    };
    check_output_multi(&out, files)?;
    write_output(&out, &bytes)?;
    report(ctx, &files[0], &out, "concat", bytes.len())
}

fn extract_audio(
    ctx: &Context,
    engine: &VideoEngine,
    file: &Path,
    ext: &str,
    output: Option<&Path>,
) -> Result<i32, CliError> {
    let normalized = sanitize_extension(ext)?;
    let input = read_input(file)?;
    let bytes = engine
        .extract_audio(&input, &normalized, &EngineLimits::default())
        .map_err(map_video_error)?;

    let out = match output {
        Some(p) => p.to_path_buf(),
        None => file.with_extension(&normalized),
    };
    check_output(&out, &[file])?;
    write_output(&out, &bytes)?;
    report(ctx, file, &out, "extract-audio", bytes.len())
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
    if !file.exists() {
        return Err(CliError::NotFound(file.display().to_string()));
    }
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

/// Refuse to overwrite any input file.
fn check_output(out: &Path, inputs: &[&Path]) -> Result<(), CliError> {
    for input in inputs {
        if out == *input {
            return Err(CliError::Usage(format!(
                "output would overwrite the input {}; pass --output-file",
                input.display()
            )));
        }
    }
    Ok(())
}

fn check_output_multi(out: &Path, inputs: &[PathBuf]) -> Result<(), CliError> {
    for input in inputs {
        if out == input.as_path() {
            return Err(CliError::Usage(format!(
                "output would overwrite the input {}; pass --output-file",
                input.display()
            )));
        }
    }
    Ok(())
}

/// `<stem>.<marker>.<ext>` next to the input; falls back to appending
/// `.<marker>.<ext>` when the input has no usable name/extension.
fn marked_default(file: &Path, marker: &str, ext: &str) -> Result<PathBuf, CliError> {
    let name = file
        .file_name()
        .and_then(|n| n.to_str())
        .ok_or_else(|| CliError::Usage(format!("bad input path {}", file.display())))?;
    let out_name = if let Some((stem, _)) = name.rsplit_once('.') {
        format!("{stem}.{marker}.{ext}")
    } else {
        format!("{name}.{marker}.{ext}")
    };
    Ok(file.with_file_name(out_name))
}

/// `<stem>.trimmed.<same-ext>` — trimmed output keeps the input
/// container; inputs without an extension default to `mp4`.
fn trimmed_default(file: &Path) -> Result<PathBuf, CliError> {
    let ext = file
        .extension()
        .and_then(|e| e.to_str())
        .unwrap_or("mp4");
    marked_default(file, "trimmed", ext)
}

/// Lowercased extension usable as a file extension (`" MKV "` →
/// `"mkv"`). Rejects empty, over-long, or non-alphanumeric values so a
/// hostile extension cannot escape the output directory via the
/// default output path. The engine validates the actual format.
fn sanitize_extension(ext: &str) -> Result<String, CliError> {
    let cleaned = ext.trim().to_ascii_lowercase();
    let ok = !cleaned.is_empty()
        && cleaned.len() <= 16
        && cleaned.chars().all(|c| c.is_ascii_alphanumeric());
    if !ok {
        return Err(CliError::Usage(format!("invalid extension: {ext:?}")));
    }
    Ok(cleaned)
}

/// Render a JSON value for table output: strings unquoted, everything
/// else via `Display` (`null`, numbers, booleans).
fn render_value(v: &serde_json::Value) -> String {
    match v {
        serde_json::Value::String(s) => s.clone(),
        other => other.to_string(),
    }
}

/// Maps [`VideoError`] onto the CLI contract: a missing ffmpeg/ffprobe
/// binary is `unsupported`, bad input/limit violations are
/// `validation`, external-tool failures are `internal`.
fn map_video_error(e: VideoError) -> CliError {
    match e {
        VideoError::MissingTool { tool } => {
            CliError::Unsupported(format!("required video tool not found: {tool}"))
        }
        VideoError::InvalidInput(msg) | VideoError::LimitExceeded(msg) => {
            CliError::Validation(msg)
        }
        VideoError::ExternalTool { tool, detail } => {
            CliError::Internal(format!("{tool} failed: {detail}"))
        }
        VideoError::Io(err) => CliError::Io(err),
        VideoError::Json(err) => CliError::Json(err),
    }
}
