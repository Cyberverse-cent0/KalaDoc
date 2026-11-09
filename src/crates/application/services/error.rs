//! Unified error type for application services.
//!
//! [`ServiceError`] is the single error surface that CLI, HTTP, and MCP
//! adapters match on. It wraps the lower-level error enums of each
//! backing subsystem (`storage`, `document_engine` store, `config`,
//! database init) so no context is lost when an error crosses a service
//! boundary — adapters never see `StorageError`, `StoreError`, etc.

use std::io;

use crate::config::ConfigError;
use crate::crates::application::Organization::init_db::DbError;
use crate::crates::document_engine::store::StoreError;
use crate::crates::storage::local::debug_when_full::StorageError;

/// Errors returned by application services.
#[derive(Debug, thiserror::Error)]
pub enum ServiceError {
    /// A storage-backend operation failed (I/O, quota, path escape,
    /// injected fault, ...). See [`StorageError`] for the specifics.
    #[error(transparent)]
    Storage(#[from] StorageError),

    /// A document-store operation failed (SQL, validation, not found).
    #[error(transparent)]
    Store(#[from] StoreError),

    /// Configuration could not be loaded or validated.
    #[error(transparent)]
    Config(#[from] ConfigError),

    /// Database open/migration failed while composing services.
    #[error(transparent)]
    Db(#[from] DbError),

    /// A plain filesystem error (e.g. preparing the storage root).
    #[error(transparent)]
    Io(#[from] io::Error),
}
