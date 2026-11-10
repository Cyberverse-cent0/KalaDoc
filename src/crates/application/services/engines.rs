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

#[cfg(test)]
mod tests {
    use super::*;

    /// The engine names `EngineRegistry::builtin()` is documented to
    /// populate (see `engine_registry::registry::EngineRegistry::builtin`).
    const BUILTIN_NAMES: [&str; 7] = [
        "convert",
        "image",
        "multimedia",
        "pdf",
        "pipeline",
        "security",
        "video",
    ];

    #[test]
    fn builtin_registry_contains_all_engines() {
        let registry = builtin_registry();
        assert_eq!(registry.len(), BUILTIN_NAMES.len());
        for name in BUILTIN_NAMES {
            assert!(registry.get(name).is_some(), "missing engine {name}");
            assert!(registry.contains(name));
        }
        // Unknown names resolve to None, not a panic.
        assert!(registry.get("does-not-exist").is_none());
        assert!(!registry.contains("does-not-exist"));
    }

    #[test]
    fn list_is_sorted_and_capabilities_do_not_panic() {
        let registry = builtin_registry();
        let infos = registry.list();
        assert_eq!(infos.len(), BUILTIN_NAMES.len());

        // BTreeMap ordering → names sorted ascending.
        let names: Vec<&str> = infos.iter().map(|info| info.name.as_str()).collect();
        let mut sorted = names.clone();
        sorted.sort_unstable();
        assert_eq!(names, sorted);
        assert_eq!(names, BUILTIN_NAMES);

        for info in &infos {
            assert!(!info.version.is_empty(), "{} has no version", info.name);
            // capabilities() must not panic for any registered engine.
            let caps = registry
                .capabilities(&info.name)
                .unwrap_or_else(|| panic!("no capabilities for {}", info.name));
            assert_eq!(caps.operations, info.operations);
            assert_eq!(caps.input_formats, info.input_formats);
            assert_eq!(caps.output_formats, info.output_formats);
        }

        assert!(registry.capabilities("nope").is_none());
        assert!(registry.operations("nope").is_none());
    }

    #[tokio::test]
    async fn health_checks_cover_every_engine_without_panicking() {
        let registry = builtin_registry();
        let single = registry.health("pdf").await;
        assert!(single.is_some());
        assert!(registry.health("nope").await.is_none());

        let all = registry.health_all().await;
        assert_eq!(all.len(), BUILTIN_NAMES.len());
        let names: Vec<&str> = all.iter().map(|entry| entry.name.as_str()).collect();
        assert_eq!(names, BUILTIN_NAMES);
    }

    #[tokio::test]
    async fn engine_report_has_expected_json_shape() {
        let report = engine_report().await;

        let engines = report["engines"]
            .as_array()
            .expect("engines must be an array");
        assert_eq!(engines.len(), BUILTIN_NAMES.len());
        for (i, name) in BUILTIN_NAMES.iter().enumerate() {
            assert_eq!(engines[i]["name"], *name);
            assert!(engines[i]["version"].is_string());
            assert!(engines[i]["operations"].is_array());
        }

        let health = report["health"]
            .as_array()
            .expect("health must be an array");
        assert_eq!(health.len(), BUILTIN_NAMES.len());
        for (i, name) in BUILTIN_NAMES.iter().enumerate() {
            assert_eq!(health[i]["name"], *name);
            assert!(health[i]["status"].is_string());
        }

        // Report must be serializable for CLI --json / MCP output.
        let roundtrip: Value = serde_json::from_str(&report.to_string()).unwrap();
        assert_eq!(roundtrip, report);
    }
}
