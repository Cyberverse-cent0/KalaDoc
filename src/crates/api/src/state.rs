//! Shared state for API handlers.
//!
//! [`ApiState`] bundles the [`AppServices`] facade (storage + document
//! services over one pool), the [`EngineRegistry`] used by `/health`,
//! the session settings needed to validate Bearer tokens, and the
//! [`ResourceLimits`] applied to uploads. Construct it once at startup
//! and share it as `Arc<ApiState>` — it is the router state.

use std::collections::BTreeSet;
use std::sync::Arc;

use crate::config::{ApiKey, AppConfig};
use crate::crates::application::Organization::init_db::SessionConfig;
use crate::crates::application::services::{AppServices, ServiceError};
use crate::crates::engine_registry::EngineRegistry;
use crate::crates::security_engine::ResourceLimits;
use crate::crates::storage::local::config::StorageConfig;

/// Everything an API handler needs, behind one `Arc`.
pub struct ApiState {
    /// Storage + document + ingestion services sharing one pool.
    pub services: AppServices,
    /// Engine registry probed by `GET /health`.
    pub engines: EngineRegistry,
    /// Session TTL/renewal settings for Bearer-token validation.
    pub session_config: SessionConfig,
    /// Upload budgets enforced by `security_engine::validate_upload`.
    pub security_limits: ResourceLimits,
    /// Storage root config — artifact downloads open a scoped reader
    /// through it (storage ops need `&mut`, which `Arc<ApiState>`
    /// cannot provide, so each download builds a fresh backend).
    pub storage_config: StorageConfig,
    /// Static API keys accepted as `Authorization: Bearer <key>`
    /// alongside session tokens (`api.api_keys` / `KALADOC_API_KEYS`).
    pub api_keys: Vec<ApiKey>,
    /// Route groups disabled via `KALADOC_API_DISABLE` / TOML
    /// `api.disabled_groups`; see [`crate::crates::api::flags`].
    pub disabled_groups: BTreeSet<String>,
}

impl ApiState {
    /// Production constructor: builds [`AppServices`] from `config`
    /// (storage root + migrated database) and a registry of the
    /// built-in engines.
    ///
    /// # Errors
    /// Propagates [`ServiceError`] from storage setup or database open.
    pub async fn init(config: &AppConfig) -> Result<Self, ServiceError> {
        let services = AppServices::init(config).await?;
        Ok(Self {
            services,
            engines: EngineRegistry::builtin(),
            session_config: config.to_db_config().session,
            security_limits: ResourceLimits::default(),
            storage_config: config.to_storage_config(),
            api_keys: config.api.api_keys.clone(),
            disabled_groups: config.api.disabled_groups.iter().cloned().collect(),
        })
    }

    /// Wraps already-built services — used by tests and embedders that
    /// manage their own bootstrap. `storage_config` must describe the
    /// same root the `AppServices` storage was built from.
    pub fn new(
        services: AppServices,
        engines: EngineRegistry,
        session_config: SessionConfig,
        storage_config: StorageConfig,
    ) -> Self {
        Self {
            services,
            engines,
            session_config,
            security_limits: ResourceLimits::default(),
            storage_config,
            api_keys: Vec::new(),
            disabled_groups: BTreeSet::new(),
        }
    }

    /// Overrides the upload [`ResourceLimits`] (builder-style).
    pub fn with_security_limits(mut self, limits: ResourceLimits) -> Self {
        self.security_limits = limits;
        self
    }

    /// Replaces the static API keys (builder-style; tests/embedders).
    pub fn with_api_keys(mut self, keys: Vec<ApiKey>) -> Self {
        self.api_keys = keys;
        self
    }

    /// Replaces the disabled route groups (builder-style).
    pub fn with_disabled_groups(mut self, groups: impl IntoIterator<Item = String>) -> Self {
        self.disabled_groups = groups.into_iter().collect();
        self
    }

    /// Shared handle for handlers/middleware.
    pub fn arc(self) -> Arc<Self> {
        Arc::new(self)
    }
}
