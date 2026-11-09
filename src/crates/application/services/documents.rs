//! Document service — thin async facade over the `document_engine`
//! SQLite store.
//!
//! [`DocumentService`] is the only place adapters should reach document
//! persistence: it holds a [`SqlitePool`] and forwards each call to the
//! corresponding `store::*` function, mapping [`StoreError`] into
//! [`ServiceError`]. No business rules are added here yet — authorization,
//! principal binding, and quota decisions belong to higher layers — but
//! every mutating store call already records its audit event, so this
//! facade preserves that behavior for free.
//!
//! Signatures mirror `document_engine::store` one-to-one. Where the
//! store API was mid-refactor or ambiguous, functions were skipped —
//! see the module notes in `services/mod.rs`.

use serde_json::Value;
use sqlx::SqlitePool;

use crate::crates::document_engine::artifact::DocumentArtifact;
use crate::crates::document_engine::audit::{DocumentAuditEvent, DocumentAuditEventType};
use crate::crates::document_engine::document::Document;
use crate::crates::document_engine::ids::{DocumentId, FolderId};
use crate::crates::document_engine::lifecycle::{DocumentAccessLevel, DocumentPermission};
use crate::crates::document_engine::organization::{DocumentTag, Folder, TagAssignment};
use crate::crates::document_engine::store::artifacts;
use crate::crates::document_engine::store::documents;
use crate::crates::document_engine::store::folders;
use crate::crates::document_engine::store::history;
use crate::crates::document_engine::store::permissions;
use crate::crates::document_engine::store::schema;
use crate::crates::document_engine::store::tags;
use crate::crates::document_engine::store::versions;
use crate::crates::document_engine::version::DocumentVersion;

use super::error::ServiceError;

// Re-export the store's input types so adapters can name them without
// reaching into `document_engine::store` internals.
pub use crate::crates::document_engine::store::artifacts::NewArtifact;
pub use crate::crates::document_engine::store::documents::{DocumentPatch, NewDocument};

/// Application-facing document service.
///
/// Cheap to clone-share via the inner [`SqlitePool`]; construct with
/// [`DocumentService::new`] (or [`DocumentService::init`] which also runs
/// the schema migrations).
#[derive(Clone)]
pub struct DocumentService {
    pool: SqlitePool,
}

impl DocumentService {
    /// Wraps an existing pool. The document-store schema must already be
    /// initialized (see [`DocumentService::init`] or
    /// `init_db_with`).
    pub fn new(pool: SqlitePool) -> Self {
        Self { pool }
    }

    /// Builds a pool against `sqlite::memory:` — **tests only**.
    ///
    /// `sqlite::memory:` databases are per-connection, so the pool is
    /// capped at one connection to keep a single shared database.
    #[cfg(test)]
    async fn test_pool() -> SqlitePool {
        sqlx::sqlite::SqlitePoolOptions::new()
            .max_connections(1)
            .connect("sqlite::memory:")
            .await
            .expect("in-memory sqlite pool")
    }

    /// Runs the document-store schema migrations on the pool. Idempotent;
    /// safe to call on every startup. Returns the service ready for use.
    ///
    /// # Errors
    /// [`ServiceError::Store`] wraps `sqlx::Error` on failure.
    pub async fn init(pool: SqlitePool) -> Result<Self, ServiceError> {
        // `init_document_store` returns `sqlx::Error` directly; route it
        // through `StoreError` so adapters see one document error type.
        schema::init_document_store(&pool)
            .await
            .map_err(crate::crates::document_engine::store::StoreError::Sqlx)?;
        Ok(Self::new(pool))
    }

    /// The underlying pool, for callers that need to compose with other
    /// services sharing the same database (jobs, sessions).
    pub fn pool(&self) -> &SqlitePool {
        &self.pool
    }

    // ------------------------------------------------------------------
    // Documents
    // ------------------------------------------------------------------

    /// Registers a new document and records a `created` audit event.
    pub async fn create_document(&self, new: &NewDocument) -> Result<Document, ServiceError> {
        Ok(documents::create_document(&self.pool, new).await?)
    }

    /// Persists a fully-constructed [`Document`] (import pipelines).
    /// Does not generate an id or audit event — the caller owns those.
    pub async fn register_document(&self, doc: &Document) -> Result<(), ServiceError> {
        Ok(documents::register_document(&self.pool, doc).await?)
    }

    /// Fetches a document by id; `None` when it does not exist.
    pub async fn get_document(&self, id: &DocumentId) -> Result<Option<Document>, ServiceError> {
        Ok(documents::get_document(&self.pool, id).await?)
    }

    /// Lists documents filtered by owner/folder. Trashed rows are
    /// excluded unless `include_deleted` is set.
    pub async fn list_documents(
        &self,
        owner_id: Option<&str>,
        folder_id: Option<&FolderId>,
        include_deleted: bool,
        limit: i64,
    ) -> Result<Vec<Document>, ServiceError> {
        Ok(
            documents::list_documents(&self.pool, owner_id, folder_id, include_deleted, limit)
                .await?,
        )
    }

    /// Case-insensitive substring search on document names.
    pub async fn search_documents(
        &self,
        query: &str,
        limit: i64,
    ) -> Result<Vec<Document>, ServiceError> {
        Ok(documents::search_documents(&self.pool, query, limit).await?)
    }

    /// Applies a patch (name and/or type) to a document.
    pub async fn update_document(
        &self,
        id: &DocumentId,
        patch: &DocumentPatch,
    ) -> Result<Document, ServiceError> {
        Ok(documents::update_document(&self.pool, id, patch).await?)
    }

    /// Renames a document.
    pub async fn rename_document(
        &self,
        id: &DocumentId,
        new_name: &str,
    ) -> Result<Document, ServiceError> {
        Ok(documents::rename_document(&self.pool, id, new_name).await?)
    }

    /// Moves a document into a folder (`None` = root).
    pub async fn move_document(
        &self,
        id: &DocumentId,
        folder_id: Option<&FolderId>,
    ) -> Result<Document, ServiceError> {
        Ok(documents::move_document(&self.pool, id, folder_id).await?)
    }

    /// Duplicates a document (metadata + artifact rows; bytes shared).
    pub async fn copy_document(
        &self,
        id: &DocumentId,
        new_name: Option<&str>,
    ) -> Result<Document, ServiceError> {
        Ok(documents::copy_document(&self.pool, id, new_name).await?)
    }

    /// Soft-deletes (trashes) a document; restorable via
    /// [`DocumentService::restore_document`].
    pub async fn delete_document(&self, id: &DocumentId) -> Result<Document, ServiceError> {
        Ok(documents::delete_document(&self.pool, id).await?)
    }

    /// Restores a trashed document to active.
    pub async fn restore_document(&self, id: &DocumentId) -> Result<Document, ServiceError> {
        Ok(documents::restore_document(&self.pool, id).await?)
    }

    /// Permanently deletes a trashed document and its dependents.
    /// Returns `false` when the document does not exist.
    pub async fn purge_document(&self, id: &DocumentId) -> Result<bool, ServiceError> {
        Ok(documents::purge_document(&self.pool, id).await?)
    }

    // ------------------------------------------------------------------
    // Artifacts
    // ------------------------------------------------------------------

    /// Registers artifact metadata after its bytes were written to
    /// `storage_uri` by [`super::storage::StorageService`].
    pub async fn upload_artifact(
        &self,
        new: &NewArtifact,
    ) -> Result<DocumentArtifact, ServiceError> {
        Ok(artifacts::upload_artifact(&self.pool, new).await?)
    }

    /// Resolves artifact metadata by id — the "download" lookup that
    /// hands `storage_uri` to the storage layer. Records an `accessed`
    /// audit event on the owning document.
    pub async fn download_artifact(
        &self,
        artifact_id: &str,
    ) -> Result<Option<DocumentArtifact>, ServiceError> {
        Ok(artifacts::download_artifact(&self.pool, artifact_id).await?)
    }

    /// Lists a document's artifacts, oldest first.
    pub async fn list_artifacts(
        &self,
        document_id: &DocumentId,
    ) -> Result<Vec<DocumentArtifact>, ServiceError> {
        Ok(artifacts::list_artifacts(&self.pool, document_id).await?)
    }

    // ------------------------------------------------------------------
    // Folders
    // ------------------------------------------------------------------

    /// Creates a folder, optionally nested under `parent_id`.
    pub async fn create_folder(
        &self,
        name: &str,
        parent_id: Option<&FolderId>,
        owner_id: &str,
    ) -> Result<Folder, ServiceError> {
        Ok(folders::create_folder(&self.pool, name, parent_id, owner_id).await?)
    }

    /// Fetches a folder by id; `None` when it does not exist.
    pub async fn get_folder(&self, id: &FolderId) -> Result<Option<Folder>, ServiceError> {
        Ok(folders::get_folder(&self.pool, id).await?)
    }

    /// Lists folders for an owner. `parent_id` selects direct children;
    /// `None` lists root-level folders.
    pub async fn list_folders(
        &self,
        owner_id: Option<&str>,
        parent_id: Option<&FolderId>,
    ) -> Result<Vec<Folder>, ServiceError> {
        Ok(folders::list_folders(&self.pool, owner_id, parent_id).await?)
    }

    /// Renames a folder.
    pub async fn rename_folder(&self, id: &FolderId, name: &str) -> Result<Folder, ServiceError> {
        Ok(folders::rename_folder(&self.pool, id, name).await?)
    }

    /// Reparents a folder (`None` = root). Cycle moves are rejected.
    pub async fn move_folder(
        &self,
        id: &FolderId,
        new_parent: Option<&FolderId>,
    ) -> Result<Folder, ServiceError> {
        Ok(folders::move_folder(&self.pool, id, new_parent).await?)
    }

    /// Deletes a folder; descendants cascade, documents move to root.
    /// Returns `false` when the folder does not exist.
    pub async fn delete_folder(&self, id: &FolderId) -> Result<bool, ServiceError> {
        Ok(folders::delete_folder(&self.pool, id).await?)
    }

    // ------------------------------------------------------------------
    // Versions
    // ------------------------------------------------------------------

    /// Creates the next version for a document, pointing at
    /// `artifact_id` when given.
    pub async fn create_version(
        &self,
        document_id: &DocumentId,
        artifact_id: Option<&str>,
        created_by: &str,
        note: Option<&str>,
    ) -> Result<DocumentVersion, ServiceError> {
        Ok(
            versions::create_version(&self.pool, document_id, artifact_id, created_by, note)
                .await?,
        )
    }

    /// Fetches one version; `None` when absent.
    pub async fn get_version(
        &self,
        document_id: &DocumentId,
        version_number: u32,
    ) -> Result<Option<DocumentVersion>, ServiceError> {
        Ok(versions::get_version(&self.pool, document_id, version_number).await?)
    }

    /// Lists all versions of a document, newest first.
    pub async fn list_versions(
        &self,
        document_id: &DocumentId,
    ) -> Result<Vec<DocumentVersion>, ServiceError> {
        Ok(versions::list_versions(&self.pool, document_id).await?)
    }

    /// Moves the current-version pointer to `version_number`.
    pub async fn restore_version(
        &self,
        document_id: &DocumentId,
        version_number: u32,
    ) -> Result<Document, ServiceError> {
        Ok(versions::restore_version(&self.pool, document_id, version_number).await?)
    }

    // ------------------------------------------------------------------
    // Tags
    // ------------------------------------------------------------------

    /// Assigns a tag (by name — created on first use) to a document.
    pub async fn add_tag(
        &self,
        document_id: &DocumentId,
        tag_name: &str,
        assigned_by: Option<&str>,
    ) -> Result<TagAssignment, ServiceError> {
        Ok(tags::add_tag(&self.pool, document_id, tag_name, assigned_by).await?)
    }

    /// Removes a tag assignment by name; `false` when none existed.
    pub async fn remove_tag(
        &self,
        document_id: &DocumentId,
        tag_name: &str,
    ) -> Result<bool, ServiceError> {
        Ok(tags::remove_tag(&self.pool, document_id, tag_name).await?)
    }

    /// Lists tags assigned to a document.
    pub async fn list_tags(
        &self,
        document_id: &DocumentId,
    ) -> Result<Vec<DocumentTag>, ServiceError> {
        Ok(tags::list_tags(&self.pool, document_id).await?)
    }

    // ------------------------------------------------------------------
    // Permissions (records only — authorization lives elsewhere)
    // ------------------------------------------------------------------

    /// Lists permission grants recorded for a document.
    pub async fn get_document_permissions(
        &self,
        document_id: &DocumentId,
    ) -> Result<Vec<DocumentPermission>, ServiceError> {
        Ok(permissions::get_document_permissions(&self.pool, document_id).await?)
    }

    /// Grants or updates a subject's access level on a document.
    pub async fn grant_permission(
        &self,
        document_id: &DocumentId,
        subject_id: &str,
        access_level: DocumentAccessLevel,
        granted_by: Option<&str>,
    ) -> Result<DocumentPermission, ServiceError> {
        Ok(permissions::grant_permission(
            &self.pool,
            document_id,
            subject_id,
            access_level,
            granted_by,
        )
        .await?)
    }

    /// Revokes a subject's permission; `false` when none existed.
    pub async fn revoke_permission(
        &self,
        document_id: &DocumentId,
        subject_id: &str,
        revoked_by: Option<&str>,
    ) -> Result<bool, ServiceError> {
        Ok(permissions::revoke_permission(&self.pool, document_id, subject_id, revoked_by).await?)
    }

    /// Lists every grant held by a subject across all documents.
    pub async fn list_permissions_for_subject(
        &self,
        subject_id: &str,
    ) -> Result<Vec<DocumentPermission>, ServiceError> {
        Ok(permissions::list_permissions_for_subject(&self.pool, subject_id).await?)
    }

    // ------------------------------------------------------------------
    // Audit history
    // ------------------------------------------------------------------

    /// Appends an audit event for a document. The FK rejects events for
    /// unknown documents.
    pub async fn record_event(
        &self,
        document_id: &DocumentId,
        actor_id: Option<&str>,
        event_type: DocumentAuditEventType,
        details: Option<&Value>,
    ) -> Result<(), ServiceError> {
        Ok(history::record_event(&self.pool, document_id, actor_id, event_type, details).await?)
    }

    /// Returns a document's full audit history, oldest first.
    pub async fn get_document_history(
        &self,
        document_id: &DocumentId,
    ) -> Result<Vec<DocumentAuditEvent>, ServiceError> {
        Ok(history::get_document_history(&self.pool, document_id).await?)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::crates::document_engine::document::{DocumentOrigin, DocumentSource, DocumentType};

    fn new_doc(name: &str) -> NewDocument {
        NewDocument {
            name: name.to_string(),
            document_type: DocumentType::Pdf,
            owner_id: "user-1".to_string(),
            source: DocumentSource::new(DocumentOrigin::Upload),
        }
    }

    #[tokio::test]
    async fn document_crud_roundtrip() {
        let svc = DocumentService::init(DocumentService::test_pool().await)
            .await
            .unwrap();

        let doc = svc.create_document(&new_doc("report.pdf")).await.unwrap();
        assert_eq!(doc.name.as_str(), "report.pdf");

        let fetched = svc.get_document(&doc.id).await.unwrap().unwrap();
        assert_eq!(fetched.id, doc.id);

        let listed = svc
            .list_documents(Some("user-1"), None, false, 10)
            .await
            .unwrap();
        assert_eq!(listed.len(), 1);

        let renamed = svc.rename_document(&doc.id, "final.pdf").await.unwrap();
        assert_eq!(renamed.name.as_str(), "final.pdf");

        // Audit events recorded by the store come back through the facade.
        let history = svc.get_document_history(&doc.id).await.unwrap();
        assert!(history.len() >= 2);
    }

    #[tokio::test]
    async fn trash_restore_purge_lifecycle() {
        let svc = DocumentService::init(DocumentService::test_pool().await)
            .await
            .unwrap();
        let doc = svc.create_document(&new_doc("tmp.txt")).await.unwrap();

        let trashed = svc.delete_document(&doc.id).await.unwrap();
        assert!(
            trashed.status == crate::crates::document_engine::document::DocumentStatus::Trashed
        );
        // Hidden from default listing.
        assert!(
            svc.list_documents(Some("user-1"), None, false, 10)
                .await
                .unwrap()
                .is_empty()
        );

        let restored = svc.restore_document(&doc.id).await.unwrap();
        assert!(
            restored.status == crate::crates::document_engine::document::DocumentStatus::Active
        );

        svc.delete_document(&doc.id).await.unwrap();
        assert!(svc.purge_document(&doc.id).await.unwrap());
        assert!(svc.get_document(&doc.id).await.unwrap().is_none());
    }

    #[tokio::test]
    async fn folder_and_artifact_flow() {
        let svc = DocumentService::init(DocumentService::test_pool().await)
            .await
            .unwrap();
        let doc = svc.create_document(&new_doc("doc.pdf")).await.unwrap();

        let folder = svc.create_folder("Invoices", None, "user-1").await.unwrap();
        svc.move_document(&doc.id, Some(&folder.id)).await.unwrap();

        let art = svc
            .upload_artifact(&NewArtifact {
                id: None,
                document_id: doc.id.clone(),
                storage_uri: "invoices/doc.pdf".to_string(),
                file_name: "doc.pdf".to_string(),
                mime_type: Some("application/pdf".to_string()),
                size_bytes: 42,
                checksum: None,
            })
            .await
            .unwrap();

        let arts = svc.list_artifacts(&doc.id).await.unwrap();
        assert_eq!(arts.len(), 1);
        let resolved = svc.download_artifact(&art.id).await.unwrap().unwrap();
        assert_eq!(resolved.storage_uri, "invoices/doc.pdf");

        assert!(svc.delete_folder(&folder.id).await.unwrap());
        // Deleting the folder moves the document back to root.
        let doc = svc.get_document(&doc.id).await.unwrap().unwrap();
        assert!(doc.folder_id.is_none());
    }
}
