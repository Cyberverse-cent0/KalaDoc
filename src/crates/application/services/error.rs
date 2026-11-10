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

#[cfg(test)]
mod tests {
    use super::*;
    use crate::crates::document_engine::store::StoreError;
    use crate::crates::storage::local::debug_when_full::StorageError;
    use std::path::PathBuf;

    #[test]
    fn storage_variant_display_and_from() {
        let inner = StorageError::NotFound {
            path: PathBuf::from("missing.bin"),
        };
        let expected = inner.to_string();
        let err: ServiceError = inner.into();
        assert!(matches!(err, ServiceError::Storage(_)));
        // `#[error(transparent)]` forwards Display verbatim.
        assert_eq!(err.to_string(), expected);
        assert!(err.to_string().contains("missing.bin"));
    }

    #[test]
    fn store_variant_display_and_from() {
        let err = ServiceError::from(StoreError::NotFound {
            entity: "document",
            id: "doc-1".to_string(),
        });
        assert!(matches!(
            err,
            ServiceError::Store(StoreError::NotFound { .. })
        ));
        assert!(err.to_string().contains("document not found"));
        assert!(err.to_string().contains("doc-1"));

        let err = ServiceError::from(StoreError::Validation {
            field: "owner_id",
            reason: "must not be empty".to_string(),
        });
        assert!(matches!(
            err,
            ServiceError::Store(StoreError::Validation { .. })
        ));
        assert!(err.to_string().contains("owner_id"));
    }

    #[test]
    fn config_variant_display_and_from() {
        let err = ServiceError::from(ConfigError::MissingEnv("APP_SECRET_KEY".to_string()));
        assert!(matches!(err, ServiceError::Config(_)));
        assert!(err.to_string().contains("APP_SECRET_KEY"));
    }

    #[test]
    fn db_variant_display_and_from() {
        let err = ServiceError::from(DbError::Io(io::Error::new(
            io::ErrorKind::PermissionDenied,
            "cannot open db",
        )));
        assert!(matches!(err, ServiceError::Db(_)));
        assert!(err.to_string().contains("cannot open db"));
    }

    #[test]
    fn io_variant_display_and_from() {
        let err = ServiceError::from(io::Error::new(io::ErrorKind::NotFound, "gone"));
        assert!(matches!(err, ServiceError::Io(_)));
        assert_eq!(err.to_string(), "gone");
    }

    #[test]
    fn source_chain_reaches_inner_error() {
        // `ConfigError::ReadFile` carries an `io::Error` as its source;
        // wrapping it in `ServiceError::Config` must keep the chain
        // walkable so adapters can downcast/inspect causes.
        let cfg = ConfigError::ReadFile {
            path: PathBuf::from("kaladoc.toml"),
            source: io::Error::new(io::ErrorKind::PermissionDenied, "denied"),
        };
        let err = ServiceError::Config(cfg);
        assert!(err.to_string().contains("kaladoc.toml"));

        let mut source = std::error::Error::source(&err);
        let mut saw_denied = false;
        while let Some(cause) = source {
            if cause.to_string().contains("denied") {
                saw_denied = true;
            }
            source = cause.source();
        }
        assert!(saw_denied, "io cause lost in error chain: {err:?}");
    }

    #[tokio::test]
    async fn sqlx_errors_route_through_store_and_db_variants() {
        let pool = sqlx::sqlite::SqlitePoolOptions::new()
            .max_connections(1)
            .connect("sqlite::memory:")
            .await
            .expect("in-memory sqlite pool");

        // A real sqlx failure: table does not exist.
        let sqlx_err = sqlx::query("SELECT * FROM definitely_missing_table")
            .execute(&pool)
            .await
            .expect_err("query must fail");

        // StoreError: From<sqlx::Error>, then ServiceError: From<StoreError>.
        let store_err = StoreError::from(sqlx_err);
        let err = ServiceError::from(store_err);
        assert!(matches!(err, ServiceError::Store(StoreError::Sqlx(_))));

        // DbError: From<sqlx::Error>, then ServiceError: From<DbError>.
        let sqlx_err2 = sqlx::query("SELECT * FROM definitely_missing_table")
            .execute(&pool)
            .await
            .expect_err("query must fail");
        let err = ServiceError::from(DbError::from(sqlx_err2));
        assert!(matches!(err, ServiceError::Db(DbError::Sqlx(_))));
    }
}
