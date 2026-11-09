//! Component and aggregate health reporting.
//!
//! [`ComponentHealth`] describes one subsystem (engine, storage
//! backend, external tool); [`aggregate`] folds a set of component
//! results into an [`OverallHealth`] whose status is the worst
//! component status. The types serialize to a stable JSON shape for
//! `kaladoc doctor` / `system_health` style output.

use serde::{Deserialize, Serialize};

use crate::crates::core::engine::{EngineHealth, EngineHealthStatus};

/// Health of a single component.
///
/// Variants are ordered by severity — `Unavailable` is worst —
/// so [`aggregate`] can use `max()`.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum HealthStatus {
    Healthy,
    Degraded,
    Unavailable,
}

impl From<EngineHealthStatus> for HealthStatus {
    fn from(status: EngineHealthStatus) -> Self {
        match status {
            EngineHealthStatus::Healthy => Self::Healthy,
            EngineHealthStatus::Degraded => Self::Degraded,
            EngineHealthStatus::Unavailable => Self::Unavailable,
        }
    }
}

/// Health result for one named component.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ComponentHealth {
    /// Component identifier, e.g. an engine name (`"pdf"`).
    pub name: String,
    pub status: HealthStatus,
    /// Human-readable detail when not fully healthy. Never contains
    /// secrets or absolute paths outside the storage root.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub message: Option<String>,
}

impl ComponentHealth {
    /// A healthy component with no detail message.
    pub fn healthy(name: impl Into<String>) -> Self {
        Self {
            name: name.into(),
            status: HealthStatus::Healthy,
            message: None,
        }
    }

    /// A component in `status`, optionally with a detail message.
    pub fn new(name: impl Into<String>, status: HealthStatus) -> Self {
        Self {
            name: name.into(),
            status,
            message: None,
        }
    }

    /// Attach a detail message (builder-style).
    pub fn with_message(mut self, message: impl Into<String>) -> Self {
        self.message = Some(message.into());
        self
    }

    /// Bridge from an engine's [`EngineHealth`] — the caller supplies
    /// the component name because `EngineHealth` does not carry one.
    pub fn from_engine_health(name: impl Into<String>, health: EngineHealth) -> Self {
        Self {
            name: name.into(),
            status: health.status.into(),
            message: health.message,
        }
    }
}

/// Aggregate health of the whole application.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct OverallHealth {
    /// Worst status across all components; `Healthy` when the
    /// component list is empty.
    pub status: HealthStatus,
    pub components: Vec<ComponentHealth>,
}

/// Fold component results into an [`OverallHealth`]. The overall
/// status is the worst component status
/// (`Unavailable` > `Degraded` > `Healthy`); an empty list reports
/// `Healthy`.
pub fn aggregate(components: Vec<ComponentHealth>) -> OverallHealth {
    let status = components
        .iter()
        .map(|c| c.status)
        .max()
        .unwrap_or(HealthStatus::Healthy);
    OverallHealth { status, components }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn component(name: &str, status: HealthStatus) -> ComponentHealth {
        ComponentHealth::new(name, status)
    }

    #[test]
    fn aggregate_picks_worst_status() {
        let health = aggregate(vec![
            component("pdf", HealthStatus::Healthy),
            component("image", HealthStatus::Degraded),
            component("storage", HealthStatus::Unavailable),
        ]);
        assert_eq!(health.status, HealthStatus::Unavailable);
        assert_eq!(health.components.len(), 3);
    }

    #[test]
    fn aggregate_degraded_beats_healthy() {
        let health = aggregate(vec![
            component("a", HealthStatus::Healthy),
            component("b", HealthStatus::Degraded),
        ]);
        assert_eq!(health.status, HealthStatus::Degraded);
    }

    #[test]
    fn aggregate_empty_is_healthy() {
        assert_eq!(aggregate(vec![]).status, HealthStatus::Healthy);
    }

    #[test]
    fn serde_shape_is_stable() {
        let health = aggregate(vec![
            ComponentHealth::healthy("pdf"),
            component("image", HealthStatus::Degraded).with_message("tesseract missing"),
        ]);
        let value = serde_json::to_value(&health).unwrap();
        assert_eq!(
            value,
            serde_json::json!({
                "status": "degraded",
                "components": [
                    {"name": "pdf", "status": "healthy"},
                    {
                        "name": "image",
                        "status": "degraded",
                        "message": "tesseract missing"
                    }
                ]
            })
        );
    }

    #[test]
    fn from_engine_health_maps_status_and_message() {
        let engine_health = EngineHealth {
            status: EngineHealthStatus::Unavailable,
            message: Some("binary not found".to_string()),
        };
        let c = ComponentHealth::from_engine_health("pdf", engine_health);
        assert_eq!(c.name, "pdf");
        assert_eq!(c.status, HealthStatus::Unavailable);
        assert_eq!(c.message.as_deref(), Some("binary not found"));
    }

    #[test]
    fn health_status_roundtrips_snake_case() {
        for (status, s) in [
            (HealthStatus::Healthy, "healthy"),
            (HealthStatus::Degraded, "degraded"),
            (HealthStatus::Unavailable, "unavailable"),
        ] {
            assert_eq!(serde_json::to_value(status).unwrap(), s);
        }
    }
}
