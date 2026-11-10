//! Application services — business operations shared by CLI, HTTP, and MCP.
//!
//! Per the CLI/MCP specification, adapters must not reach into database
//! tables or filesystem implementations directly. Everything an adapter
//! needs goes through the services in this module:
//!
//! - [`error::ServiceError`] — the single error type adapters match on.
//! - [`storage::StorageService`] — object storage facade (put/get/
//!   delete/exists/status) over the storage engine's `StorageBackend`.
//!   Reports health via [`storage::StorageStatus`] without leaking
//!   absolute server paths.
//! - [`documents::DocumentService`] — async facade over the
//!   `document_engine` SQLite store: documents, artifacts, folders,
//!   versions, tags, permissions, and audit history.
//! - [`ingestion::DocumentIngestionService`] — the full ingestion
//!   pipeline: async byte stream → staged streaming upload → atomic
//!   commit (SHA-256) → document/artifact/version/metadata/audit
//!   registration, with rollback on failure and a `recover` sweeper
//!   for crash leftovers.
//! - [`app::AppServices`] — aggregate bundling storage + documents plus
//!   the shared `SqlitePool`, built from [`crate::config::AppConfig`] via
//!   [`app::AppServices::init`]. This is the handle CLI commands hold.
//! - [`engines`] — engine-registry diagnostics facade.
//!
//! Store functions intentionally **not** wrapped (mid-refactor or
//! unclear stability): none currently — every public
//! `document_engine::store` function with a stable signature is exposed
//! through [`documents::DocumentService`]. `store::gen_id`,
//! `store::now_rfc3339`, and the `pub(crate)` row mappers stay internal
//! by design.

pub mod app;
pub mod documents;
pub mod engines;
pub mod error;
pub mod folder_watch;
pub mod ingestion;
pub mod storage;

pub use app::AppServices;
pub use documents::DocumentService;
pub use error::ServiceError;
pub use ingestion::{
    DocumentIngestionService, IngestOutcome, IngestRequest, RecoveryOptions, RecoveryReport,
};
pub use storage::{StorageService, StorageStatus};
pub mod access;
pub mod schedules;

pub mod media_ops;
