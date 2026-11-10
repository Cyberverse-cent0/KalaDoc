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
use crate::crates::storage::local::config::StorageConfig;

use super::documents::DocumentService;
use super::error::ServiceError;
use super::ingestion::DocumentIngestionService;
use super::storage::StorageService;

/// All application services sharing one configuration and database.
pub struct AppServices {
    storage: StorageService,
    documents: DocumentService,
    ingestion: DocumentIngestionService,
    /// The shared pool, retained so future services (jobs, sessions,
    /// audit) can be added without changing constructors.
    pool: SqlitePool,
}

impl AppServices {
    /// Composes services from already-built parts. `pool` must have the
    /// document-store schema initialized (see
    /// [`DocumentService::init`] or `init_db_with`). `storage_config`
    /// must describe the same storage root the `StorageService` was
    /// built from — the ingestion service stages uploads through it.
    pub fn new(storage: StorageService, pool: SqlitePool, storage_config: &StorageConfig) -> Self {
        Self {
            storage,
            documents: DocumentService::new(pool.clone()),
            ingestion: DocumentIngestionService::new(pool.clone(), storage_config.clone()),
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
        let storage_config = config.to_storage_config();
        let storage = StorageService::from_config(&storage_config)?;
        let pool = init_db::init_db_with(&config.to_db_config()).await?;
        Ok(Self::new(storage, pool, &storage_config))
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

    /// Ingestion service — staged streaming uploads plus document
    /// registration and crash recovery. Cheap to clone; holds the
    /// storage config and stream limits alongside the shared pool.
    pub fn ingestion(&self) -> &DocumentIngestionService {
        &self.ingestion
    }

    /// The shared SQLite pool for composing additional services.
    pub fn pool(&self) -> &SqlitePool {
        &self.pool
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::crates::application::services::ingestion::IngestRequest;
    use std::path::PathBuf;
    use std::sync::atomic::{AtomicU64, Ordering};

    static NEXT_DIR: AtomicU64 = AtomicU64::new(0);

    /// Unique tempdir per test, removed on drop (same pattern as the
    /// `storage`/`ingestion` test modules).
    struct TempRoot(PathBuf);

    impl TempRoot {
        fn new() -> Self {
            let dir = std::env::temp_dir().join(format!(
                "kaladoc-app-test-{}-{}",
                std::process::id(),
                NEXT_DIR.fetch_add(1, Ordering::Relaxed)
            ));
            std::fs::create_dir_all(&dir).unwrap();
            Self(dir)
        }

        fn config(&self) -> StorageConfig {
            StorageConfig::new(&self.0)
        }
    }

    impl Drop for TempRoot {
        fn drop(&mut self) {
            let _ = std::fs::remove_dir_all(&self.0);
        }
    }

    /// In-memory pool with the document-store schema applied — mirrors
    /// `init_db_with`'s document-store step without the file DB.
    async fn test_pool() -> SqlitePool {
        let pool = sqlx::sqlite::SqlitePoolOptions::new()
            .max_connections(1)
            .connect("sqlite::memory:")
            .await
            .expect("in-memory sqlite pool");
        crate::crates::document_engine::store::schema::init_document_store(&pool)
            .await
            .expect("init document store");
        pool
    }

    #[tokio::test]
    async fn new_composes_services_over_one_shared_pool() {
        let root = TempRoot::new();
        let pool = test_pool().await;
        let storage = StorageService::from_config(&root.config()).unwrap();
        let app = AppServices::new(storage, pool.clone(), &root.config());

        // Ingestion writes through the same pool + storage root that
        // `documents()` and `storage_mut()` expose.
        let payload = b"plain text payload for aggregate test".to_vec();
        let outcome = app
            .ingestion()
            .ingest(&payload[..], IngestRequest::new("user-1", "note.txt"))
            .await
            .expect("ingest via AppServices");

        // The document is visible through the documents accessor —
        // proves ingestion and documents share one database.
        let fetched = app
            .documents()
            .get_document(&outcome.document.id)
            .await
            .unwrap()
            .expect("document must be registered");
        assert_eq!(fetched.name.as_str(), "note.txt");
        assert_eq!(fetched.owner_id, "user-1");

        // Bytes landed under the storage root the service was built
        // from, and are readable through the storage accessor.
        let object = format!("documents/{}/note.txt", outcome.document.id);
        assert!(root.0.join(&object).exists());
        let mut app = app;
        assert_eq!(app.storage_mut().get(&object).unwrap(), payload);

        // pool() hands out the same handle the services hold.
        assert_eq!(app.pool().size(), pool.size());
    }

    #[tokio::test]
    async fn init_builds_working_services_from_app_config() {
        let root = TempRoot::new();
        let mut config = AppConfig::default();
        config.database.path = root.0.join("app.db");
        config.storage.root_dir = root.0.join("storage");

        let mut app = AppServices::init(&config).await.expect("AppServices::init");

        // Storage root was prepared and is usable.
        app.storage_mut().put("probe.txt", b"ok").unwrap();
        assert_eq!(app.storage_mut().get("probe.txt").unwrap(), b"ok");
        assert!(root.0.join("storage").join("probe.txt").exists());
        // The SQLite file was created next to the configured path.
        assert!(root.0.join("app.db").exists());

        // Document store schema is live: list works and starts empty.
        let docs = app
            .documents()
            .list_documents(None, None, true, 10)
            .await
            .unwrap();
        assert!(docs.is_empty());

        // Full pipeline through the aggregate: ingest → visible via
        // documents() → object under the configured storage root.
        let outcome = app
            .ingestion()
            .ingest(&b"%PDF-1.4 fake"[..], IngestRequest::new("u", "doc.pdf"))
            .await
            .expect("ingest");
        assert!(
            app.documents()
                .get_document(&outcome.document.id)
                .await
                .unwrap()
                .is_some()
        );
        assert!(
            root.0
                .join("storage")
                .join(format!("documents/{}/doc.pdf", outcome.document.id))
                .exists()
        );
    }

    #[tokio::test]
    async fn init_propagates_storage_root_errors() {
        // A storage root that cannot be created (a *file* where the
        // directory should be) must surface as a ServiceError, not a
        // panic or silent skip.
        let root = TempRoot::new();
        let blocker = root.0.join("blocker");
        std::fs::write(&blocker, b"not a dir").unwrap();

        let mut config = AppConfig::default();
        config.database.path = root.0.join("app.db");
        config.storage.root_dir = blocker.join("storage");

        let err = match AppServices::init(&config).await {
            Ok(_) => panic!("init must fail"),
            Err(e) => e,
        };
        assert!(matches!(
            err,
            ServiceError::Io(_) | ServiceError::Storage(_)
        ));
    }
}
