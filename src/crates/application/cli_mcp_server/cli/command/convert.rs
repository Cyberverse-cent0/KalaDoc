//! `kaladoc convert --from <fmt> --to <fmt> <input>` — local-file
//! document conversion via `conversion_engine::ConversionEngine`.
//!
//! The engine dispatches to detected external backends (pandoc,
//! libreoffice, poppler) with a pure-Rust fallback; conversion is
//! synchronous and may shell out, so it runs inside `spawn_blocking`.
//! Output bytes go to `--output-file` or raw to stdout; a small
//! summary report prints when `--output-file` is used.

use std::fs;
use std::path::{Path, PathBuf};

use serde::Serialize;

use crate::crates::conversion_engine::{
    ConversionEngine, ConvertError, ConvertOptions, ConvertOutput, ConvertRequest, Format,
};

use super::{CliError, Context};

pub async fn run(
    ctx: &Context,
    input: &Path,
    from: &str,
    to: &str,
    options: Option<&str>,
    output_file: Option<&PathBuf>,
) -> Result<i32, CliError> {
    let from = parse_format(from)?;
    let to = parse_format(to)?;
    let options: ConvertOptions = match options {
        Some(raw) => serde_json::from_str(raw)
            .map_err(|e| CliError::Usage(format!("--options must be a JSON object: {e}")))?,
        None => ConvertOptions::default(),
    };
    let data = read_file(input)?;

    let req = ConvertRequest {
        input_format: from,
        output_format: to,
        input: data,
        options,
    };
    let out = blocking(move || ConversionEngine::new().convert(&req)).await?;

    match output_file {
        Some(path) => {
            write_file(path, &out.bytes)?;
            let report = ConvertReport::from_output(&out, path);
            if ctx.emit_structured(&report)? {
                return Ok(0);
            }
            println!(
                "converted {} -> {} via {} backend: {} bytes -> {}",
                from.as_str(),
                to.as_str(),
                out.backend,
                out.bytes.len(),
                path.display()
            );
            for w in &out.warnings {
                println!("warning: {w}");
            }
            Ok(0)
        }
        None => {
            // Raw bytes to stdout — structured output formats are
            // meaningless for binary payloads.
            use std::io::Write;
            let mut stdout = std::io::stdout().lock();
            stdout.write_all(&out.bytes)?;
            for w in &out.warnings {
                eprintln!("warning: {w}");
            }
            Ok(0)
        }
    }
}

/// Parses a `--from`/`--to` format name (aliases like `md`, `jpg`,
/// `text` accepted by [`Format::parse`]).
fn parse_format(s: &str) -> Result<Format, CliError> {
    Format::parse(s).ok_or_else(|| {
        CliError::Usage(format!(
            "unknown format {s:?}; expected one of: {}",
            Format::ALL
                .iter()
                .map(|f| f.as_str())
                .collect::<Vec<_>>()
                .join(", ")
        ))
    })
}

#[derive(Debug, Serialize)]
struct ConvertReport {
    output_format: String,
    backend: String,
    bytes: usize,
    output_file: String,
    warnings: Vec<String>,
}

impl ConvertReport {
    fn from_output(out: &ConvertOutput, path: &Path) -> Self {
        Self {
            output_format: out.output_format.as_str().to_string(),
            backend: out.backend.clone(),
            bytes: out.bytes.len(),
            output_file: path.display().to_string(),
            warnings: out.warnings.clone(),
        }
    }
}

/// Run the synchronous conversion on the blocking pool.
async fn blocking<F, R>(f: F) -> Result<R, CliError>
where
    F: FnOnce() -> Result<R, ConvertError> + Send + 'static,
    R: Send + 'static,
{
    tokio::task::spawn_blocking(f)
        .await
        .map_err(|e| CliError::Internal(format!("convert worker task failed: {e}")))?
        .map_err(convert_err)
}

fn convert_err(e: ConvertError) -> CliError {
    match e {
        ConvertError::UnsupportedConversion { .. } | ConvertError::BackendUnavailable { .. } => {
            CliError::Unsupported(e.to_string())
        }
        ConvertError::InvalidInput { reason } => CliError::Usage(reason),
        ConvertError::TooLarge { .. } | ConvertError::Timeout { .. } => {
            CliError::Validation(e.to_string())
        }
        ConvertError::BackendFailed { .. } => CliError::Validation(e.to_string()),
        ConvertError::Io(io) => CliError::Io(io),
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
