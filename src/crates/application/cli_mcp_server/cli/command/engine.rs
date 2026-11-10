//! `kaladoc engine list|health`
//!
//! Thin adapter over `crate::crates::application::services::engines`:
//! the built-in [`EngineRegistry`] is the single discovery point, so
//! these commands never touch engine crates directly.

use serde::Serialize;

use crate::crates::core::engine::{EngineKind, EngineOperation};

use crate::crates::application::services::engines;

use super::super::args::EngineArgs;
use super::{CliError, Context};

pub async fn run(ctx: &Context, args: &EngineArgs) -> Result<i32, CliError> {
    match args {
        EngineArgs::List => list(ctx),
        EngineArgs::Capabilities { name } => capabilities(ctx, name),
        EngineArgs::Health => health(ctx).await,
    }
}

// ---------------------------------------------------------------------------
// list
// ---------------------------------------------------------------------------

fn list(ctx: &Context) -> Result<i32, CliError> {
    let registry = engines::builtin_registry();
    let engines = registry.list();

    if ctx.emit_structured(&engines)? {
        return Ok(0);
    }

    println!(
        "{:<10} {:<12} {:<12} {:<34} formats",
        "name", "version", "kind", "operations"
    );
    for e in &engines {
        let ops = e
            .operations
            .iter()
            .map(|o| o.as_str().to_string())
            .collect::<Vec<_>>()
            .join(",");
        let formats = format!(
            "in: {}  out: {}",
            e.input_formats.join(","),
            e.output_formats.join(",")
        );
        println!(
            "{:<10} {:<12} {:<12} {:<34} {}",
            e.name,
            e.version,
            snake(&e.kind),
            ops,
            formats
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

/// Serializable view of [`crate::crates::core::engine::EngineCapabilities`]
/// — the engine type itself is not `Serialize` (its `EngineLimits` are
/// not either).
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
    max_duration_secs: Option<u64>,
}

fn capabilities(ctx: &Context, name: &str) -> Result<i32, CliError> {
    let registry = engines::builtin_registry();
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
            max_duration_secs: caps.limits.max_duration_secs,
        },
    };

    if ctx.emit_structured(&report)? {
        return Ok(0);
    }

    println!("engine: {}", report.name);
    println!("version: {}", report.version);
    println!("kind: {}", snake(&report.kind));
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
    print_limit(
        "  max_input_bytes",
        report.limits.max_input_bytes.map(u64::from),
    );
    print_limit("  max_pages", report.limits.max_pages.map(u64::from));
    print_limit("  max_pixels", report.limits.max_pixels);
    print_limit("  max_outputs", report.limits.max_outputs.map(u64::from));
    print_limit("  max_duration_secs", report.limits.max_duration_secs);
    Ok(0)
}

fn print_limit(label: &str, value: Option<u64>) {
    match value {
        Some(v) => println!("{label}: {v}"),
        None => println!("{label}: (unlimited)"),
    }
}

// ---------------------------------------------------------------------------
// health
// ---------------------------------------------------------------------------

async fn health(ctx: &Context) -> Result<i32, CliError> {
    let registry = engines::builtin_registry();
    let health = registry.health_all().await;

    if ctx.emit_structured(&health)? {
        return Ok(0);
    }

    println!("{:<10} {:<12} detail", "name", "status");
    for h in &health {
        println!(
            "{:<10} {:<12} {}",
            h.name,
            snake(&h.status),
            h.message.as_deref().unwrap_or("")
        );
    }
    if health.is_empty() {
        println!("(no engines registered)");
    }
    Ok(0)
}

// ---------------------------------------------------------------------------
// Internals
// ---------------------------------------------------------------------------

/// Renders a `snake_case` serde enum as its stable string form — same
/// spelling used in JSON output, for table cells.
fn snake<T: Serialize>(value: &T) -> String {
    serde_json::to_value(value)
        .ok()
        .and_then(|v| v.as_str().map(str::to_string))
        .unwrap_or_else(|| "unknown".to_string())
}
