//! `kaladoc engine list|capabilities|health`
//!
//! Thin adapter over [`EngineRegistry::builtin`]: discovery and health
//! only — running engine operations lives in the `image`/`pipeline`
//! command groups. No configuration or database access is needed, so
//! these commands work before the app is configured.

use serde::Serialize;

use crate::crates::core::engine::{EngineKind, EngineOperation};
use crate::crates::engine_registry::{EngineHealthEntry, EngineRegistry};

use super::super::args::EngineArgs;
use super::{CliError, Context};

pub async fn run(ctx: &Context, args: &EngineArgs) -> Result<i32, CliError> {
    let registry = EngineRegistry::builtin();
    match args {
        EngineArgs::List => list(ctx, &registry),
        EngineArgs::Capabilities { name } => capabilities(ctx, &registry, name),
        EngineArgs::Health => health(ctx, &registry).await,
    }
}

// ---------------------------------------------------------------------------
// list
// ---------------------------------------------------------------------------

fn list(ctx: &Context, registry: &EngineRegistry) -> Result<i32, CliError> {
    let engines = registry.list();
    if ctx.emit_structured(&engines)? {
        return Ok(0);
    }

    println!(
        "{:<10} {:<8} {:<12} {:<28} operations / formats",
        "name", "version", "kind", "operations"
    );
    for info in &engines {
        let ops: Vec<&str> = info.operations.iter().map(|op| op.as_str()).collect();
        println!(
            "{:<10} {:<8} {:<12} {:<28} in={} out={}",
            info.name,
            info.version,
            kind_label(info.kind),
            ops.join(","),
            info.input_formats.join(","),
            info.output_formats.join(",")
        );
    }
    if engines.is_empty() {
        println!("(no engines registered)");
    }
    Ok(0)
}

// ---------------------------------------------------------------------------
// capabilities
// ---------------------------------------------------------------------------

/// Serializable view of [`EngineCapabilities`] — the engine type itself
/// is not `Serialize` (its `EngineLimits` are not either).
#[derive(Debug, Serialize)]
struct CapabilitiesReport {
    name: String,
    version: String,
    kind: EngineKind,
    operations: Vec<EngineOperation>,
    input_formats: Vec<String>,
    output_formats: Vec<String>,
    limits: LimitsView,
}

#[derive(Debug, Serialize)]
struct LimitsView {
    max_input_bytes: Option<u64>,
    max_pages: Option<u32>,
    max_pixels: Option<u64>,
    max_outputs: Option<u32>,
}

fn capabilities(ctx: &Context, registry: &EngineRegistry, name: &str) -> Result<i32, CliError> {
    let engine = registry
        .get(name)
        .ok_or_else(|| CliError::NotFound(format!("engine {name:?}")))?;
    let caps = engine.capabilities();
    let report = CapabilitiesReport {
        name: engine.name().to_string(),
        version: engine.version().to_string(),
        kind: engine.kind(),
        operations: caps.operations,
        input_formats: caps.input_formats,
        output_formats: caps.output_formats,
        limits: LimitsView {
            max_input_bytes: caps.limits.max_input_bytes,
            max_pages: caps.limits.max_pages,
            max_pixels: caps.limits.max_pixels,
            max_outputs: caps.limits.max_outputs,
        },
    };

    if ctx.emit_structured(&report)? {
        return Ok(0);
    }

    println!("engine: {}", report.name);
    println!("version: {}", report.version);
    println!("kind: {}", kind_label(report.kind));
    println!(
        "operations: {}",
        report
            .operations
            .iter()
            .map(|op| op.as_str())
            .collect::<Vec<_>>()
            .join(", ")
    );
    println!("input_formats: {}", report.input_formats.join(", "));
    println!("output_formats: {}", report.output_formats.join(", "));
    println!("limits:");
    print_limit("  max_input_bytes", report.limits.max_input_bytes.map(u64::from));
    print_limit("  max_pages", report.limits.max_pages.map(u64::from));
    print_limit("  max_pixels", report.limits.max_pixels);
    print_limit("  max_outputs", report.limits.max_outputs.map(u64::from));
    Ok(0)
}

// ---------------------------------------------------------------------------
// health
// ---------------------------------------------------------------------------

async fn health(ctx: &Context, registry: &EngineRegistry) -> Result<i32, CliError> {
    let results: Vec<EngineHealthEntry> = registry.health_all().await;
    if ctx.emit_structured(&results)? {
        return Ok(0);
    }

    println!("{:<10} {:<12} message", "name", "status");
    for entry in &results {
        println!(
            "{:<10} {:<12} {}",
            entry.name,
            health_label(&entry.status),
            entry.message.as_deref().unwrap_or("")
        );
    }
    if results.is_empty() {
        println!("(no engines registered)");
    }
    Ok(0)
}

fn print_limit(label: &str, value: Option<u64>) {
    match value {
        Some(v) => println!("{label}: {v}"),
        None => println!("{label}: (unlimited)"),
    }
}

// ---------------------------------------------------------------------------
// Internals
// ---------------------------------------------------------------------------

fn kind_label(kind: EngineKind) -> &'static str {
    match kind {
        EngineKind::Pdf => "pdf",
        EngineKind::Image => "image",
        EngineKind::Conversion => "conversion",
        EngineKind::Security => "security",
        EngineKind::Pipeline => "pipeline",
    }
}

fn health_label(status: &crate::crates::core::engine::EngineHealthStatus) -> &'static str {
    use crate::crates::core::engine::EngineHealthStatus::*;
    match status {
        Healthy => "healthy",
        Degraded => "degraded",
        Unavailable => "unavailable",
    }
}
