//! `kaladoc media op` — local-file multimedia operations via
//! `services::media_ops::run_media_op` on the multimedia engine.
//!
//! The op dispatch is the same one the `media.op` job kind uses
//! (ffmpeg/ffprobe on `PATH`); running locally avoids enqueueing a
//! job for interactive use. Ops are synchronous, so they run inside
//! `spawn_blocking`. File outputs are written under `--output-dir`
//! (or `--output-file` for a single file); report ops emit JSON.

use std::fs;
use std::path::Path;

use serde::Serialize;
use serde_json::{Value, json};

use crate::crates::application::services::media_ops::{self, MediaOpOutcome};
use crate::crates::multimedia_engine::error::MediaError;
use crate::crates::multimedia_engine::model::{MediaLimits, MediaOutput};
use crate::crates::multimedia_engine::runtime::discovery::Runtime;

use super::super::args::MediaArgs;
use super::{CliError, Context};

pub async fn run(ctx: &Context, args: &MediaArgs) -> Result<i32, CliError> {
    match args {
        MediaArgs::Op {
            input,
            op,
            params,
            output_format,
            output_dir,
            output_file,
            list_ops,
        } => {
            op_run(
                ctx,
                input,
                op.as_deref(),
                params.as_deref(),
                output_format.as_deref(),
                output_dir.as_deref(),
                output_file.as_deref(),
                *list_ops,
            )
            .await
        }
    }
}

async fn op_run(
    ctx: &Context,
    input: &Path,
    op: Option<&str>,
    params: Option<&str>,
    output_format: Option<&str>,
    output_dir: Option<&Path>,
    output_file: Option<&Path>,
    list_ops: bool,
) -> Result<i32, CliError> {
    if list_ops {
        let ops: Vec<&str> = media_ops::MEDIA_OPS.to_vec();
        if ctx.emit_structured(&ops)? {
            return Ok(0);
        }
        for op in ops {
            println!("{op}");
        }
        return Ok(0);
    }

    let op =
        op.ok_or_else(|| CliError::Usage("media op requires --op (or --list-ops)".to_string()))?;
    if !media_ops::MEDIA_OPS.contains(&op) {
        return Err(CliError::Usage(format!(
            "unknown media op {op:?}; run `kaladoc media op --list-ops`"
        )));
    }
    // Owned copy: the blocking closure below must be 'static.
    let op = op.to_string();

    // `params` must be a JSON object; `output_format` fills
    // `params.output_format` when the op's params lack one — same
    // rule as the `media.op` job handler.
    let mut params: Value = match params {
        Some(p) => serde_json::from_str(p)?,
        None => json!({}),
    };
    if let Some(fmt) = output_format {
        if !params.is_object() {
            return Err(CliError::Usage(
                "--params must be a JSON object when --output-format is used".to_string(),
            ));
        }
        if params.get("output_format").is_none_or(Value::is_null) {
            params["output_format"] = json!(fmt);
        }
    }
    if !params.is_object() {
        return Err(CliError::Usage(
            "--params must be a JSON object".to_string(),
        ));
    }

    let data = read_file(input)?;
    // The input name's extension steers ffmpeg's format probing.
    let input_name = input
        .file_name()
        .map(|n| n.to_string_lossy().into_owned())
        .unwrap_or_else(|| "input".to_string());

    let output_dir = output_dir.map(Path::to_path_buf);
    let output_file = output_file.map(Path::to_path_buf);
    let op_for_closure = op.clone();
    let outcome = blocking(move || {
        let rt = Runtime::discover()?;
        let limits = MediaLimits::default();
        media_ops::run_media_op(
            &op_for_closure,
            &input_name,
            &data,
            &params,
            None,
            &limits,
            &rt,
        )
    })
    .await?;

    match outcome {
        MediaOpOutcome::Report(report) => {
            if ctx.emit_structured(&report)? {
                return Ok(0);
            }
            println!("{}", serde_json::to_string_pretty(&report)?);
            Ok(0)
        }
        MediaOpOutcome::Files(out) => {
            let written = write_outputs(&out, output_dir.as_deref(), output_file.as_deref())?;
            let report = FilesReport {
                op: op.clone(),
                files_written: written.len(),
                files: written,
                warnings: out.warnings,
            };
            if ctx.emit_structured(&report)? {
                return Ok(0);
            }
            for f in &report.files {
                println!("wrote {} ({} bytes)", f.path, f.bytes);
            }
            for w in &report.warnings {
                eprintln!("warning: {w}");
            }
            Ok(0)
        }
    }
}

/// Serializable per-file record for `Files` outcomes.
#[derive(Debug, Serialize)]
struct WrittenFile {
    /// Path the file was written to (display form).
    path: String,
    /// Suggested name from the op.
    name: String,
    bytes: usize,
    content_type: Option<String>,
}

#[derive(Debug, Serialize)]
struct FilesReport {
    op: String,
    files_written: usize,
    files: Vec<WrittenFile>,
    warnings: Vec<String>,
}

/// Writes every produced file. `--output-dir` receives all files
/// (name collisions get a numeric suffix); `--output-file` is only
/// valid for a single-file result.
fn write_outputs(
    out: &MediaOutput,
    output_dir: Option<&Path>,
    output_file: Option<&Path>,
) -> Result<Vec<WrittenFile>, CliError> {
    let files: Vec<_> = out.files().collect();
    if files.is_empty() {
        return Err(CliError::Internal(format!(
            "media op produced no output files"
        )));
    }

    let mut written = Vec::new();
    if let Some(dir) = output_dir {
        fs::create_dir_all(dir)?;
        let mut used: Vec<String> = Vec::new();
        for f in files {
            let name = unique_name(&f.name, &used);
            used.push(name.clone());
            let path = dir.join(&name);
            write_file(&path, &f.bytes)?;
            written.push(WrittenFile {
                path: path.display().to_string(),
                name,
                bytes: f.bytes.len(),
                content_type: f.content_type.clone(),
            });
        }
        return Ok(written);
    }

    if let Some(path) = output_file {
        if files.len() != 1 {
            return Err(CliError::Usage(format!(
                "op produced {} files; use --output-dir for multi-file results",
                files.len()
            )));
        }
        let f = files[0];
        write_file(path, &f.bytes)?;
        written.push(WrittenFile {
            path: path.display().to_string(),
            name: f.name.clone(),
            bytes: f.bytes.len(),
            content_type: f.content_type.clone(),
        });
        return Ok(written);
    }

    Err(CliError::Usage(
        "media op produced file output; pass --output-dir or --output-file".to_string(),
    ))
}

/// First free variant of `name` (`name`, `name-1.ext`, `name-2.ext`,
/// ...) given the names already used in this batch.
fn unique_name(name: &str, used: &[String]) -> String {
    if !used.iter().any(|u| u == name) {
        return name.to_string();
    }
    let (stem, ext) = match name.rsplit_once('.') {
        Some((s, e)) if !s.is_empty() => (s.to_string(), format!(".{e}")),
        _ => (name.to_string(), String::new()),
    };
    for i in 1.. {
        let candidate = format!("{stem}-{i}{ext}");
        if !used.iter().any(|u| u == &candidate) {
            return candidate;
        }
    }
    unreachable!()
}

/// Run a synchronous op on the blocking pool.
async fn blocking<F, R>(f: F) -> Result<R, CliError>
where
    F: FnOnce() -> Result<R, MediaError> + Send + 'static,
    R: Send + 'static,
{
    tokio::task::spawn_blocking(f)
        .await
        .map_err(|e| CliError::Internal(format!("media worker task failed: {e}")))?
        .map_err(media_err)
}

fn media_err(e: MediaError) -> CliError {
    match e {
        MediaError::MissingTool(m) => CliError::Unsupported(format!("missing tool: {m}")),
        MediaError::Unsupported(m) => CliError::Unsupported(m),
        MediaError::InvalidInput(m) => CliError::Usage(m),
        MediaError::LimitExceeded(m) => CliError::Validation(m),
        MediaError::Io(io) => CliError::Io(io),
        other => CliError::Internal(other.to_string()),
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
