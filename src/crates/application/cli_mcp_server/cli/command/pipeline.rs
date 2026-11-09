//! `kaladoc pipeline list|run`
//!
//! Thin adapter over [`PipelineEngine`] and
//! [`builtin_definitions`]: `list` prints the shipped definitions,
//! `run` feeds a file's bytes through a named built-in pipeline and
//! prints the `PipelineOutput` step reports. No configuration or
//! database access is needed.

use std::path::Path;

use serde::Serialize;

use crate::crates::pipeline_engine::{PipelineEngine, PipelineError, builtin_definitions};

use super::super::args::PipelineArgs;
use super::{CliError, Context};

pub fn run(ctx: &Context, args: &PipelineArgs) -> Result<i32, CliError> {
    match args {
        PipelineArgs::List => list(ctx),
        PipelineArgs::Run { name, file } => run_named(ctx, name, file),
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

    println!("{:<20} {:<8} description / steps", "name", "steps");
    for def in &defs {
        let desc = def.description.as_deref().unwrap_or("");
        println!(
            "{:<20} {:<8} {} — {}",
            def.name,
            def.steps.len(),
            desc,
            def.steps
                .iter()
                .map(|s| format!("{}.{}", s.engine, s.operation))
                .collect::<Vec<_>>()
                .join(" → ")
        );
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
struct StepSummary {
    index: usize,
    name: String,
    engine: String,
    operation: String,
    has_report: bool,
}

fn run_named(ctx: &Context, name: &str, file: &Path) -> Result<i32, CliError> {
    let input = std::fs::read(file).map_err(|e| {
        CliError::Io(std::io::Error::new(
            e.kind(),
            format!("cannot read {}: {e}", file.display()),
        ))
    })?;

    let engine = PipelineEngine::new();
    let output = engine.run_named(name, &input).map_err(map_pipeline_error)?;

    let (artifact_kind, artifact_bytes, artifact_count) = match &output.artifact {
        crate::crates::pipeline_engine::PipelineArtifact::Bytes(b) => ("bytes", b.len() as u64, 1),
        crate::crates::pipeline_engine::PipelineArtifact::Multiple(v) => {
            ("multiple", output.artifact.len_bytes(), v.len())
        }
    };

    let steps: Vec<StepSummary> = output
        .steps
        .iter()
        .map(|s| StepSummary {
            index: s.index,
            name: s.name.clone(),
            engine: s.engine.clone(),
            operation: s.operation.clone(),
            has_report: s.output.is_some(),
        })
        .collect();

    // JSON/YAML consumers get the full PipelineOutput (step reports
    // included); the table view is the summary only.
    if ctx.json() || ctx.output.is_yaml() {
        ctx.emit_structured(&serde_json::json!({
            "pipeline": output.pipeline,
            "artifact": output.artifact,
            "steps": output.steps,
        }))?;
        return Ok(0);
    }

    println!("pipeline: {}", output.pipeline);
    println!(
        "artifact: {} ({} bytes, {} document{})",
        artifact_kind,
        artifact_bytes,
        artifact_count,
        if artifact_count == 1 { "" } else { "s" }
    );
    println!(
        "{:<4} {:<20} {:<22} report",
        "idx", "step", "engine.operation"
    );
    for s in &steps {
        println!(
            "{:<4} {:<20} {:<22} {}",
            s.index,
            s.name,
            format!("{}.{}", s.engine, s.operation),
            if s.has_report { "yes" } else { "-" }
        );
    }
    Ok(0)
}

// ---------------------------------------------------------------------------
// Internals
// ---------------------------------------------------------------------------

/// Maps [`PipelineError`] onto the CLI contract: unknown pipeline names
/// are `not_found`, definition/parameter problems are usage errors,
/// resource limits and step failures are validation failures.
fn map_pipeline_error(e: PipelineError) -> CliError {
    match &e {
        PipelineError::PipelineNotFound(name) => CliError::NotFound(format!("pipeline {name:?}")),
        PipelineError::UnsupportedOperation { .. } => CliError::Unsupported(e.to_string()),
        _ => CliError::Validation(e.to_string()),
    }
}
