//! `kaladoc engine list|health`
//!
//! Thin adapter over `crate::crates::application::services::engines`:
//! the built-in [`EngineRegistry`] is the single discovery point, so
//! these commands never touch engine crates directly.

use serde::Serialize;

use crate::crates::application::services::engines;

use super::super::args::EngineArgs;
use super::{CliError, Context};

pub async fn run(ctx: &Context, args: &EngineArgs) -> Result<i32, CliError> {
    match args {
        EngineArgs::List => list(ctx),
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
