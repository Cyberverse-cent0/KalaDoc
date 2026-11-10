//! `kaladoc pipeline list|run` — built-in pipeline definitions via
//! `PipelineEngine`.
//!
//! `list` prints the definitions shipped by
//! [`builtin_definitions`] (`pdf.intake`, `pdf.extract-text`). `run`
//! executes one on a file: step reports go to stdout (JSON in
//! structured modes), and the result artifact is written to
//! `--output-file` (single stream) or `--output-dir` (multi-document).

use std::fs;
use std::path::Path;

use serde::Serialize;

use crate::crates::pipeline_engine::{
    DagDefinition, DagNodeStatus, PipelineArtifact, PipelineEngine, PipelineError, StepError,
    StepReport, builtin_definitions, run_dag,
};
use crate::crates::security_engine::{SniffedFormat, sniff_format};

use super::super::args::PipelineArgs;
use super::{CliError, Context};

/// Input-size cap for pipeline runs — matches the pdf command's
/// generous interactive limit.
const MAX_INPUT_BYTES: u64 = 256 * 1024 * 1024;

pub async fn run(ctx: &Context, args: &PipelineArgs) -> Result<i32, CliError> {
    match args {
        PipelineArgs::List => list(ctx),
        PipelineArgs::Run {
            name,
            input,
            output_file,
            output_dir,
        } => {
            run_pipeline(
                ctx,
                name,
                input,
                output_file.as_deref(),
                output_dir.as_deref(),
            )
            .await
        }
        PipelineArgs::RunDag {
            dag,
            input,
            output_dir,
            dry_run,
        } => run_dag_file(ctx, dag, input, output_dir.as_deref(), *dry_run).await,
    }
}

// ---------------------------------------------------------------------------
// list
// ---------------------------------------------------------------------------

fn list(ctx: &Context) -> Result<i32, CliError> {
    let defs = builtin_definitions();

    if ctx.emit_structured(&defs)? {
        return Ok(0);
    }

    for def in &defs {
        println!("{}", def.name);
        if let Some(desc) = &def.description {
            println!("  {desc}");
        }
        for (i, step) in def.steps.iter().enumerate() {
            println!(
                "  {:<2} {:<24} {}.{}",
                i + 1,
                step.name,
                step.engine,
                step.operation
            );
        }
        println!();
    }
    if defs.is_empty() {
        println!("(no built-in pipelines)");
    }
    Ok(0)
}

// ---------------------------------------------------------------------------
// run
// ---------------------------------------------------------------------------

#[derive(Debug, Serialize)]
struct RunReport {
    pipeline: String,
    steps: Vec<StepReport>,
    /// `"bytes"` or `"multiple"`.
    artifact_kind: &'static str,
    /// Byte counts per produced document (one entry for `bytes`).
    artifact_sizes: Vec<u64>,
    /// Paths the artifact was written to, when an output was requested.
    files: Vec<String>,
}

async fn run_pipeline(
    ctx: &Context,
    name: &str,
    input: &Path,
    output_file: Option<&Path>,
    output_dir: Option<&Path>,
) -> Result<i32, CliError> {
    let data = fs::read(input).map_err(|e| {
        CliError::Io(std::io::Error::new(
            e.kind(),
            format!("{}: {e}", input.display()),
        ))
    })?;
    if data.len() as u64 > MAX_INPUT_BYTES {
        return Err(CliError::Validation(format!(
            "input exceeds {} byte limit",
            MAX_INPUT_BYTES
        )));
    }

    // Pipeline execution is synchronous CPU-bound work.
    let engine = PipelineEngine::new();
    let pipeline_name = name.to_string();
    let output = tokio::task::spawn_blocking(move || engine.run_named(&pipeline_name, &data))
        .await
        .map_err(|e| CliError::Internal(format!("pipeline worker task failed: {e}")))?
        .map_err(pipeline_err)?;

    let files = write_artifact(&output.artifact, output_file, output_dir, input)?;

    let report = RunReport {
        pipeline: output.pipeline,
        steps: output.steps,
        artifact_kind: match &output.artifact {
            PipelineArtifact::Bytes(_) => "bytes",
            PipelineArtifact::Multiple(_) => "multiple",
        },
        artifact_sizes: artifact_sizes(&output.artifact),
        files,
    };

    if ctx.emit_structured(&report)? {
        return Ok(0);
    }

    println!("pipeline: {}", report.pipeline);
    for step in &report.steps {
        let detail = step
            .output
            .as_ref()
            .map(|o| format!(" -> {}", compact_json(o)))
            .unwrap_or_default();
        println!(
            "  step {:<2} {:<24} {}.{}{}",
            step.index + 1,
            step.name,
            step.engine,
            step.operation,
            detail
        );
    }
    match report.artifact_kind {
        "bytes" => println!(
            "artifact: {} byte(s){}",
            report.artifact_sizes.first().copied().unwrap_or(0),
            if report.files.is_empty() {
                String::new()
            } else {
                format!(" -> {}", report.files.join(", "))
            }
        ),
        _ => println!(
            "artifact: {} document(s){}",
            report.artifact_sizes.len(),
            if report.files.is_empty() {
                String::new()
            } else {
                format!(" -> {}", report.files.join(", "))
            }
        ),
    }
    Ok(0)
}

fn artifact_sizes(artifact: &PipelineArtifact) -> Vec<u64> {
    match artifact {
        PipelineArtifact::Bytes(b) => vec![b.len() as u64],
        PipelineArtifact::Multiple(v) => v.iter().map(|b| b.len() as u64).collect(),
    }
}

/// Write the result artifact when an output destination was given.
/// `Bytes` goes to `--output-file`; `Multiple` goes to `--output-dir`
/// as `<stem>-part-NNN.pdf`. Returns the written paths.
fn write_artifact(
    artifact: &PipelineArtifact,
    output_file: Option<&Path>,
    output_dir: Option<&Path>,
    input: &Path,
) -> Result<Vec<String>, CliError> {
    match artifact {
        PipelineArtifact::Bytes(bytes) => match output_file {
            Some(path) => {
                write_file(path, bytes)?;
                Ok(vec![path.display().to_string()])
            }
            None => Ok(Vec::new()),
        },
        PipelineArtifact::Multiple(docs) => {
            let dir = output_dir.ok_or_else(|| {
                CliError::Usage(format!(
                    "pipeline produced {} documents; pass --output-dir",
                    docs.len()
                ))
            })?;
            fs::create_dir_all(dir)?;
            let mut files = Vec::with_capacity(docs.len());
            for (i, bytes) in docs.iter().enumerate() {
                let path = dir.join(split_name(input, i + 1));
                write_file(&path, bytes)?;
                files.push(path.display().to_string());
            }
            Ok(files)
        }
    }
}

fn split_name(input: &Path, n: usize) -> String {
    let stem = input
        .file_stem()
        .map(|s| s.to_string_lossy().into_owned())
        .unwrap_or_else(|| "pipeline".to_string());
    format!("{stem}-part-{n:03}.pdf")
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

fn pipeline_err(e: PipelineError) -> CliError {
    match e {
        PipelineError::PipelineNotFound(name) => CliError::NotFound(format!("pipeline {name}")),
        PipelineError::InvalidDefinition(m) => CliError::Usage(m),
        PipelineError::UnsupportedOperation { operation, .. }
        | PipelineError::UnknownEngine {
            engine: operation, ..
        } => CliError::Unsupported(format!("pipeline operation {operation}")),
        PipelineError::LimitExceeded(m) => CliError::Validation(m),
        PipelineError::StepFailed {
            index,
            name,
            engine,
            operation,
            source,
        } => CliError::Validation(format!(
            "pipeline step {} `{name}` ({engine}.{operation}) failed: {}",
            index + 1,
            step_err_detail(&source)
        )),
    }
}

fn step_err_detail(e: &StepError) -> String {
    match e {
        StepError::Params(m) | StepError::InvalidState(m) => m.clone(),
        StepError::Pdf(p) => p.to_string(),
        StepError::Security(s) => s.to_string(),
        StepError::Image(e) => e.to_string(),
        StepError::Convert(e) => e.to_string(),
        StepError::Media(e) => e.to_string(),
    }
}

/// Compact one-line JSON for table output.
fn compact_json(v: &serde_json::Value) -> String {
    serde_json::to_string(v).unwrap_or_else(|_| v.to_string())
}

// ---------------------------------------------------------------------------
// run-dag
// ---------------------------------------------------------------------------
//
// `pipeline run-dag` executes a `pipeline_engine::DagDefinition` JSON
// file locally through [`run_dag`] — the same executor the `pipeline.run`
// job kind uses for `dag` payloads. DAG semantics (documented in
// `pipeline_engine::dag`): every node runs its step against the *same*
// input bytes (fan-out, no wired data flow), waves execute
// sequentially, and a failed node marks its transitive dependents
// `skipped` while independent siblings continue.

/// Per-node line in the `run-dag` report.
#[derive(Debug, Serialize)]
struct DagNodeLine {
    id: String,
    status: DagNodeStatus,
    detail: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    error: Option<String>,
}

#[derive(Debug, Serialize)]
struct DagRunReport {
    dag: String,
    succeeded: usize,
    failed: usize,
    skipped: usize,
    nodes: Vec<DagNodeLine>,
    /// Paths written under `--output-dir`, when requested.
    files: Vec<String>,
}

async fn run_dag_file(
    ctx: &Context,
    dag_path: &Path,
    input: &Path,
    output_dir: Option<&Path>,
    dry_run: bool,
) -> Result<i32, CliError> {
    let dag_json = fs::read_to_string(dag_path).map_err(|e| {
        CliError::Io(std::io::Error::new(
            e.kind(),
            format!("{}: {e}", dag_path.display()),
        ))
    })?;
    let def: DagDefinition = serde_json::from_str(&dag_json).map_err(|e| {
        CliError::Usage(format!(
            "{} is not a valid dag definition: {e}",
            dag_path.display()
        ))
    })?;

    let data = fs::read(input).map_err(|e| {
        CliError::Io(std::io::Error::new(
            e.kind(),
            format!("{}: {e}", input.display()),
        ))
    })?;
    if data.len() as u64 > MAX_INPUT_BYTES {
        return Err(CliError::Validation(format!(
            "input exceeds {} byte limit",
            MAX_INPUT_BYTES
        )));
    }

    // Validate + compute waves up front — shared by --dry-run and the
    // real run (run_dag re-validates, which is cheap at ≤64 nodes).
    let waves = def
        .execution_waves()
        .map_err(|e| CliError::Validation(e.to_string()))?;

    if dry_run {
        #[derive(Serialize)]
        struct DryRun<'a> {
            dag: &'a str,
            nodes: usize,
            edges: usize,
            waves: Vec<Vec<String>>,
        }
        let report = DryRun {
            dag: &def.name,
            nodes: def.nodes.len(),
            edges: def.edges.len(),
            waves,
        };
        if ctx.emit_structured(&report)? {
            return Ok(0);
        }
        println!("dag: {}", def.name);
        println!("nodes: {}, edges: {}", def.nodes.len(), def.edges.len());
        for (i, wave) in report.waves.iter().enumerate() {
            println!("  wave {}: {}", i, wave.join(", "));
        }
        println!("(dry run — no nodes executed)");
        return Ok(0);
    }

    // DAG execution is synchronous CPU-bound work.
    let engine = PipelineEngine::new();
    let dag_input = data.clone();
    let report = tokio::task::spawn_blocking(move || run_dag(&engine, &def, &dag_input))
        .await
        .map_err(|e| CliError::Internal(format!("dag worker task failed: {e}")))?
        .map_err(|e| CliError::Validation(e.to_string()))?;

    // Write each succeeded node's *changed* artifact under
    // --output-dir. Pass-through report steps (artifact identical to
    // the input) produce no file — their output is in the report.
    let mut files = Vec::new();
    if let Some(dir) = output_dir {
        fs::create_dir_all(dir)?;
        for node in &report.nodes {
            if node.status != DagNodeStatus::Succeeded {
                continue;
            }
            let Some(artifact) = &node.artifact else {
                continue;
            };
            let slug = node_file_slug(&node.id);
            match artifact {
                PipelineArtifact::Bytes(bytes) => {
                    if bytes.as_slice() == data.as_slice() {
                        continue;
                    }
                    let path = dir.join(format!("{slug}.{}", ext_for_bytes(bytes)));
                    write_file(&path, bytes)?;
                    files.push(path.display().to_string());
                }
                PipelineArtifact::Multiple(parts) => {
                    for (i, bytes) in parts.iter().enumerate() {
                        let path = dir.join(format!("{slug}-{}.{}", i + 1, ext_for_bytes(bytes)));
                        write_file(&path, bytes)?;
                        files.push(path.display().to_string());
                    }
                }
            }
        }
    }

    let out = DagRunReport {
        dag: report.dag.clone(),
        succeeded: report.succeeded,
        failed: report.failed,
        skipped: report.skipped,
        nodes: report
            .nodes
            .iter()
            .map(|n| DagNodeLine {
                id: n.id.clone(),
                status: n.status,
                detail: n.detail.clone(),
                error: n.error.clone(),
            })
            .collect(),
        files,
    };

    if ctx.emit_structured(&out)? {
        return Ok(if out.failed > 0 { 1 } else { 0 });
    }

    println!(
        "dag: {} — {} succeeded, {} failed, {} skipped",
        out.dag, out.succeeded, out.failed, out.skipped
    );
    for node in &out.nodes {
        let marker = match node.status {
            DagNodeStatus::Succeeded => "ok",
            DagNodeStatus::Failed => "FAIL",
            DagNodeStatus::Skipped => "skip",
        };
        let err = node
            .error
            .as_ref()
            .map(|e| format!(" — {e}"))
            .unwrap_or_default();
        println!("  [{marker:>4}] {:<24} {}{}", node.id, node.detail, err);
    }
    if !out.files.is_empty() {
        println!("files: {}", out.files.join(", "));
    }
    // Non-zero exit when any node failed — the report is still
    // complete (skipped dependents are recorded, not hidden).
    Ok(if out.failed > 0 { 1 } else { 0 })
}

/// Extension for a DAG node artifact, sniffed from magic bytes.
fn ext_for_bytes(bytes: &[u8]) -> &'static str {
    match sniff_format(&bytes[..bytes.len().min(4096)]) {
        SniffedFormat::Pdf => "pdf",
        SniffedFormat::Png => "png",
        SniffedFormat::Jpeg => "jpg",
        SniffedFormat::Gif => "gif",
        SniffedFormat::Zip => "zip",
        f @ (SniffedFormat::Video { .. } | SniffedFormat::Audio { .. }) => {
            f.sniffed_extension().trim_start_matches('.')
        }
        SniffedFormat::Unknown => "bin",
    }
}

/// Filename-safe slug for a DAG node id (same character policy as the
/// job handler's `pipeline_slug`).
fn node_file_slug(id: &str) -> String {
    let slug: String = id
        .chars()
        .map(|c| {
            if c.is_ascii_alphanumeric() || matches!(c, '.' | '-' | '_') {
                c
            } else {
                '_'
            }
        })
        .collect();
    if slug.is_empty() {
        "node".to_string()
    } else {
        slug
    }
}

#[cfg(test)]
mod tests {
    use super::super::super::args::{Cli, Commands, PipelineArgs};
    use clap::Parser;

    #[test]
    fn run_dag_args_parse() {
        let cli = Cli::try_parse_from([
            "kaladoc",
            "pipeline",
            "run-dag",
            "dag.json",
            "--input",
            "in.pdf",
            "--output-dir",
            "out",
            "--dry-run",
        ])
        .unwrap();
        match cli.command {
            Some(Commands::Pipeline(PipelineArgs::RunDag {
                dag,
                input,
                output_dir,
                dry_run,
            })) => {
                assert_eq!(dag.as_path(), std::path::Path::new("dag.json"));
                assert_eq!(input.as_path(), std::path::Path::new("in.pdf"));
                assert_eq!(output_dir.as_deref(), Some(std::path::Path::new("out")));
                assert!(dry_run);
            }
            _ => panic!("expected pipeline run-dag"),
        }
    }

    #[test]
    fn run_dag_args_require_dag_and_input() {
        // Missing --input fails clap parsing.
        assert!(Cli::try_parse_from(["kaladoc", "pipeline", "run-dag", "dag.json"]).is_err());
    }
}
