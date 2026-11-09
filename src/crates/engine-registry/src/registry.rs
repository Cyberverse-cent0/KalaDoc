//! In-memory registry of processing engines.
//!
//! [`EngineRegistry`] stores `Arc<dyn ProcessingEngine>` instances keyed
//! by engine name and is the single lookup point used by the CLI/MCP
//! layers for discovery (`list`, `capabilities`) and diagnostics
//! (`health`). Registration order does not matter: [`EngineRegistry::list`]
//! always returns engines sorted by name.

use std::collections::BTreeMap;
use std::sync::Arc;

use serde::Serialize;

use crate::crates::conversion_engine::ConversionEngine;
use crate::crates::core::engine::{
    EngineCapabilities, EngineHealth, EngineHealthStatus, EngineKind, EngineOperation,
    ProcessingEngine,
};
use crate::crates::image_engine::ImageEngine;
use crate::crates::multimedia_engine::MediaEngine;
use crate::crates::pdf_engine::PdfEngine;
use crate::crates::pipeline_engine::PipelineEngine;
use crate::crates::security_engine::SecurityEngine;

/// Summary of a registered engine, returned by [`EngineRegistry::list`].
#[derive(Debug, Clone, Serialize)]
pub struct EngineInfo {
    /// Stable engine identifier (same as `ProcessingEngine::name`).
    pub name: String,
    /// Engine version string.
    pub version: String,
    /// Category the engine belongs to.
    pub kind: EngineKind,
    /// Operations the engine advertises.
    pub operations: Vec<EngineOperation>,
    /// Input format identifiers the engine accepts.
    pub input_formats: Vec<String>,
    /// Output format identifiers the engine produces.
    pub output_formats: Vec<String>,
}

/// One engine's health as reported by [`EngineRegistry::health_all`].
#[derive(Debug, Clone, Serialize)]
pub struct EngineHealthEntry {
    /// Stable engine identifier.
    pub name: String,
    /// Point-in-time health status.
    pub status: EngineHealthStatus,
    /// Human-readable detail when not fully healthy.
    pub message: Option<String>,
}

/// In-memory engine registry. Cheap to construct; wrap in `Arc` and
/// share between subsystems if needed.
#[derive(Default)]
pub struct EngineRegistry {
    // BTreeMap keeps iteration (and thus `list`) deterministically
    // sorted by name without a separate sort pass.
    engines: BTreeMap<String, Arc<dyn ProcessingEngine>>,
}

impl EngineRegistry {
    /// Creates an empty registry.
    pub fn new() -> Self {
        Self::default()
    }

    /// A registry pre-populated with the built-in engines: `convert`,
    /// `image`, `pdf`, `pipeline` and `security`. Additional engines can
    /// be added with [`EngineRegistry::register`].
    pub fn builtin() -> Self {
        let mut registry = Self::new();
        registry.register(Arc::new(ConversionEngine::new()));
        registry.register(Arc::new(ImageEngine::new()));
        registry.register(Arc::new(MediaEngine::new()));
        registry.register(Arc::new(PdfEngine::new()));
        registry.register(Arc::new(PipelineEngine::new()));
        registry.register(Arc::new(SecurityEngine::new()));
        registry
    }

    /// Registers `engine` under its [`ProcessingEngine::name`].
    ///
    /// Re-registering an existing name **replaces** the previous engine —
    /// last registration wins. Engines are expected to report honest,
    /// non-empty names; an empty name is stored under the empty key and
    /// is effectively unreachable via [`EngineRegistry::get`].
    pub fn register(&mut self, engine: Arc<dyn ProcessingEngine>) {
        let key = engine.name().to_string();
        self.engines.insert(key, engine);
    }

    /// Removes the engine registered under `name`, returning it.
    /// `None` if no engine has that name.
    pub fn unregister(&mut self, name: &str) -> Option<Arc<dyn ProcessingEngine>> {
        self.engines.remove(name)
    }

    /// Looks up an engine by name.
    pub fn get(&self, name: &str) -> Option<Arc<dyn ProcessingEngine>> {
        self.engines.get(name).cloned()
    }

    /// Whether an engine is registered under `name`.
    pub fn contains(&self, name: &str) -> bool {
        self.engines.contains_key(name)
    }

    /// Number of registered engines.
    pub fn len(&self) -> usize {
        self.engines.len()
    }

    /// Whether the registry is empty.
    pub fn is_empty(&self) -> bool {
        self.engines.is_empty()
    }

    /// All registered engines as [`EngineInfo`] summaries — including
    /// advertised operations and format support — sorted by name.
    pub fn list(&self) -> Vec<EngineInfo> {
        self.engines
            .values()
            .map(|engine| {
                let caps = engine.capabilities();
                EngineInfo {
                    name: engine.name().to_string(),
                    version: engine.version().to_string(),
                    kind: engine.kind(),
                    operations: caps.operations,
                    input_formats: caps.input_formats,
                    output_formats: caps.output_formats,
                }
            })
            .collect()
    }

    /// Static capabilities of the named engine, or `None` if it is not
    /// registered.
    pub fn capabilities(&self, name: &str) -> Option<EngineCapabilities> {
        self.engines.get(name).map(|e| e.capabilities())
    }

    /// Operations advertised by the named engine, or `None` if it is
    /// not registered.
    pub fn operations(&self, name: &str) -> Option<Vec<EngineOperation>> {
        self.capabilities(name).map(|c| c.operations)
    }

    /// Runs the engine's health check. `None` if it is not registered.
    pub async fn health(&self, name: &str) -> Option<EngineHealth> {
        let engine = self.engines.get(name)?;
        Some(engine.health().await)
    }

    /// Runs health checks for every registered engine, returning one
    /// [`EngineHealthEntry`] per engine sorted by name.
    ///
    /// Engines are checked one at a time so a slow probe cannot starve
    /// the others on a single-threaded executor; parallelize later if
    /// profiling shows it matters.
    pub async fn health_all(&self) -> Vec<EngineHealthEntry> {
        let mut results = Vec::with_capacity(self.engines.len());
        for (name, engine) in &self.engines {
            let health = engine.health().await;
            results.push(EngineHealthEntry {
                name: name.clone(),
                status: health.status,
                message: health.message,
            });
        }
        results
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::crates::core::engine::{EngineFuture, EngineHealthStatus, EngineLimits};
    use std::sync::atomic::{AtomicUsize, Ordering};

    struct MockEngine {
        name: &'static str,
        version: &'static str,
        kind: EngineKind,
        caps: EngineCapabilities,
        health: EngineHealth,
        health_calls: Arc<AtomicUsize>,
    }

    impl MockEngine {
        fn new(name: &'static str, kind: EngineKind, ops: &[EngineOperation]) -> Self {
            Self {
                name,
                version: "0.1.0",
                kind,
                caps: EngineCapabilities {
                    operations: ops.to_vec(),
                    input_formats: vec!["pdf".to_string()],
                    output_formats: vec!["pdf".to_string()],
                    limits: EngineLimits::default(),
                },
                health: EngineHealth {
                    status: EngineHealthStatus::Healthy,
                    message: None,
                },
                health_calls: Arc::new(AtomicUsize::new(0)),
            }
        }

        fn unhealthy(name: &'static str) -> Self {
            let mut engine = Self::new(name, EngineKind::Pdf, &[]);
            engine.health = EngineHealth {
                status: EngineHealthStatus::Unavailable,
                message: Some("probe failed".to_string()),
            };
            engine
        }
    }

    impl ProcessingEngine for MockEngine {
        fn name(&self) -> &'static str {
            self.name
        }

        fn version(&self) -> &'static str {
            self.version
        }

        fn kind(&self) -> EngineKind {
            self.kind
        }

        fn capabilities(&self) -> EngineCapabilities {
            self.caps.clone()
        }

        fn health<'a>(&'a self) -> EngineFuture<'a, EngineHealth> {
            Box::pin(async move {
                self.health_calls.fetch_add(1, Ordering::SeqCst);
                self.health.clone()
            })
        }
    }

    fn engine(name: &'static str) -> Arc<dyn ProcessingEngine> {
        Arc::new(MockEngine::new(
            name,
            EngineKind::Pdf,
            &[EngineOperation::Merge, EngineOperation::Split],
        ))
    }

    #[test]
    fn register_get_and_contains() {
        let mut registry = EngineRegistry::new();
        assert!(registry.is_empty());
        assert!(!registry.contains("pdf"));
        assert!(registry.get("pdf").is_none());

        registry.register(engine("pdf"));
        assert!(registry.contains("pdf"));
        assert_eq!(registry.len(), 1);

        let fetched = registry.get("pdf").unwrap();
        assert_eq!(fetched.name(), "pdf");
        assert_eq!(fetched.version(), "0.1.0");
        assert_eq!(fetched.kind(), EngineKind::Pdf);
    }

    #[test]
    fn duplicate_name_replaces_previous_engine() {
        let mut registry = EngineRegistry::new();
        registry.register(engine("pdf"));

        // Same name, different kind — replacement must be observable.
        registry.register(Arc::new(MockEngine::new(
            "pdf",
            EngineKind::Conversion,
            &[EngineOperation::Convert],
        )));

        assert_eq!(registry.len(), 1);
        let fetched = registry.get("pdf").unwrap();
        assert_eq!(fetched.kind(), EngineKind::Conversion);
        assert_eq!(
            fetched.capabilities().operations,
            [EngineOperation::Convert]
        );
    }

    #[test]
    fn list_is_sorted_and_reports_capabilities() {
        let mut registry = EngineRegistry::new();
        registry.register(engine("zeta"));
        registry.register(engine("alpha"));

        let infos = registry.list();
        let names: Vec<&str> = infos.iter().map(|info| info.name.as_str()).collect();
        assert_eq!(names, ["alpha", "zeta"]);

        let info = &infos[0];
        assert_eq!(info.version, "0.1.0");
        assert_eq!(info.kind, EngineKind::Pdf);
        assert_eq!(
            info.operations,
            [EngineOperation::Merge, EngineOperation::Split]
        );
        assert_eq!(info.input_formats, ["pdf"]);
        assert_eq!(info.output_formats, ["pdf"]);
    }

    #[test]
    fn capabilities_and_operations_delegate() {
        let mut registry = EngineRegistry::new();
        registry.register(engine("pdf"));

        let caps = registry.capabilities("pdf").unwrap();
        assert_eq!(
            caps.operations,
            [EngineOperation::Merge, EngineOperation::Split]
        );
        assert_eq!(caps.input_formats, ["pdf"]);

        let ops = registry.operations("pdf").unwrap();
        assert_eq!(ops, [EngineOperation::Merge, EngineOperation::Split]);

        assert!(registry.capabilities("nope").is_none());
        assert!(registry.operations("nope").is_none());
    }

    #[test]
    fn builtin_registers_all_engines() {
        let registry = EngineRegistry::builtin();
        assert_eq!(registry.len(), 6);
        for name in ["convert", "image", "multimedia", "pdf", "pipeline", "security"] {
            assert!(registry.get(name).is_some(), "missing {name}");
        }

        let infos = registry.list();
        let names: Vec<&str> = infos.iter().map(|info| info.name.as_str()).collect();
        assert_eq!(
            names,
            ["convert", "image", "multimedia", "pdf", "pipeline", "security"]
        );
    }

    #[tokio::test]
    async fn health_delegates_to_engine() {
        let mut registry = EngineRegistry::new();
        let mock = Arc::new(MockEngine::unhealthy("broken"));
        let calls = mock.health_calls.clone();
        registry.register(mock);

        let health = registry.health("broken").await.unwrap();
        assert_eq!(health.status, EngineHealthStatus::Unavailable);
        assert_eq!(health.message.as_deref(), Some("probe failed"));
        assert_eq!(calls.load(Ordering::SeqCst), 1);

        assert!(registry.health("nope").await.is_none());
    }

    #[tokio::test]
    async fn health_all_covers_every_engine_in_order() {
        let mut registry = EngineRegistry::new();
        registry.register(Arc::new(MockEngine::unhealthy("b-bad")));
        registry.register(engine("a-good"));

        let results = registry.health_all().await;
        let names: Vec<&str> = results.iter().map(|entry| entry.name.as_str()).collect();
        assert_eq!(names, ["a-good", "b-bad"]);
        assert_eq!(results[0].status, EngineHealthStatus::Healthy);
        assert_eq!(results[1].status, EngineHealthStatus::Unavailable);
        assert_eq!(results[1].message.as_deref(), Some("probe failed"));
    }

    #[tokio::test]
    async fn builtin_engines_report_healthy() {
        let registry = EngineRegistry::builtin();
        let results = registry.health_all().await;
        assert_eq!(results.len(), 5);
        for entry in &results {
            // Engines may honestly report Degraded when an optional
            // external tool (e.g. tesseract for pdf OCR) is absent —
            // that is still a working engine, not an unavailable one.
            match entry.status {
                EngineHealthStatus::Healthy => assert!(entry.message.is_none()),
                EngineHealthStatus::Degraded => assert!(entry.message.is_some()),
                EngineHealthStatus::Unavailable => {
                    panic!("{} is unavailable: {:?}", entry.name, entry.message)
                }
            }
        }
    }

    #[test]
    fn unregister_removes_engine() {
        let mut registry = EngineRegistry::new();
        registry.register(engine("pdf"));

        let removed = registry.unregister("pdf").unwrap();
        assert_eq!(removed.name(), "pdf");
        assert!(!registry.contains("pdf"));
        assert!(registry.unregister("pdf").is_none());
    }
}
