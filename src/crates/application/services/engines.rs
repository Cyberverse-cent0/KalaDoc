//! Engine-registry service — thin adapter over
//! [`crate::crates::engine_registry`].
//!
//! Services are adapters over crates: this module exposes the built-in
//! engine registry and a JSON report for diagnostics (`kaladoc engine
//! list`, `doctor`) without leaking registry internals to callers.

use serde_json::{Value, json};

use crate::crates::engine_registry::EngineRegistry;

/// Returns the registry populated with every built-in engine.
pub fn builtin_registry() -> EngineRegistry {
    EngineRegistry::builtin()
}

/// Builds a JSON report of all built-in engines and their health.
///
/// Shape: `{ "engines": [EngineInfo...], "health": [EngineHealthEntry...] }`
/// — both arrays sorted by engine name (registry order). Intended for
/// CLI `--json` output and MCP tool responses.
pub async fn engine_report() -> Value {
    let registry = builtin_registry();
    json!({
        "engines": registry.list(),
        "health": registry.health_all().await,
    })
}
