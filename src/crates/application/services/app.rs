//! Application service aggregate — what CLI/MCP/HTTP entry points hold.
//!
//! [`AppServices`] bundles the domain services behind one struct so an
//! adapter (CLI command context, MCP server state, HTTP app state) owns
//! a single handle instead of threading `SqlitePool` + `StorageConfig`
//! through every layer. Business rules live here and below; adapters
//! stay thin per the CLI/MCP specification ("do not let CLI or MCP
//! handlers reach directly into database tables or filesystem
//! implementations").

use sqlx::SqlitePool;

use crate::config::AppConfig;
use crate::crates::application::Organization::init_db;

use super::documents::DocumentService;
use super::error::ServiceError;
use super::storage::StorageService;

/// All application services sharing one configuration and database.
pub struct AppServices {
    storage: StorageService,
    documents: DocumentService,
    /// The shared pool, retained so future services (jobs, sessions,
    /// audit) can be added without changing constructors.
    pool: SqlitePool,
}

impl AppServices {
    /// Composes services from already-built parts. `pool` must have the
    /// document-store schema initialized (see
    /// [`DocumentService::init`] or `init_db_with`).
    pub fn new(storage: StorageService, pool: SqlitePool) -> Self {
        Self {
            storage,
            documents: DocumentService::new(pool.clone()),
            pool,
        }
    }

    /// Builds every service from an [`AppConfig`]: prepares the storage
    /// root, opens/migrates the database (users, sessions, jobs,
    /// document store), and wraps both behind [`AppServices`].
    ///
    /// # Errors
    /// [`ServiceError::Config`]/[`ServiceError::Io`] for config and
    /// storage-root problems, [`ServiceError::Db`] for database open or
    /// migration failures.
    pub async fn init(config: &AppConfig) -> Result<Self, ServiceError> {
        let storage = StorageService::from_config(&config.to_storage_config())?;
        let pool = init_db::init_db_with(&config.to_db_config()).await?;
        Ok(Self::new(storage, pool))
    }

    /// Storage service. Takes `&mut self` because backend operations
    /// need mutable access (quota counters, fault injection).
    pub fn storage(&self) -> &StorageService {
        &self.storage
    }

    pub fn storage_mut(&mut self) -> &mut StorageService {
        &mut self.storage
    }

    /// Document service — stateless facade over the shared pool.
    pub fn documents(&self) -> &DocumentService {
        &self.documents
    }

    /// The shared SQLite pool for composing additional services.
    pub fn pool(&self) -> &SqlitePool {
        &self.pool
    }
}
