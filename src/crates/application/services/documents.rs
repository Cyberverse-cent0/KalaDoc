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
use crate::crates::document_engine::metadata::DocumentMetadata;
use crate::crates::document_engine::organization::{DocumentTag, Folder, TagAssignment};
use crate::crates::document_engine::store::artifacts;
use crate::crates::document_engine::store::documents;
use crate::crates::document_engine::store::folders;
use crate::crates::document_engine::store::history;
use crate::crates::document_engine::store::metadata;
use crate::crates::document_engine::store::permissions;
use crate::crates::document_engine::store::relationships;
use crate::crates::document_engine::store::schema;
use crate::crates::document_engine::store::settings;
use crate::crates::document_engine::store::tags;
use crate::crates::document_engine::store::versions;
use crate::crates::document_engine::version::DocumentVersion;

use super::error::ServiceError;

// Re-export the store's input types so adapters can name them without
// reaching into `document_engine::store` internals.
pub use crate::crates::document_engine::relationship::{
    DocumentRelationship, DocumentRelationshipType,
};
pub use crate::crates::document_engine::settings::DocumentSettings;
pub use crate::crates::document_engine::store::artifacts::NewArtifact;
pub use crate::crates::document_engine::store::documents::{DocumentPatch, NewDocument};
pub use crate::crates::document_engine::store::relationships::RelatedDocument;

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

    /// Archives a document (`status → archived`). Archived documents
    /// remain listed; [`DocumentService::unarchive_document`] reverses
    /// it. Trashed/deleted documents cannot be archived.
    pub async fn archive_document(&self, id: &DocumentId) -> Result<Document, ServiceError> {
        Ok(documents::archive_document(&self.pool, id).await?)
    }

    /// Unarchives a document (`status → active`). Returns
    /// [`ServiceError::Store`]`(StoreError::Validation)` when the
    /// document is not currently archived.
    pub async fn unarchive_document(&self, id: &DocumentId) -> Result<Document, ServiceError> {
        Ok(documents::unarchive_document(&self.pool, id).await?)
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
    // Metadata
    // ------------------------------------------------------------------

    /// Inserts or fully replaces the metadata row for a document.
    /// `keywords` and `custom_fields` persist as JSON; records a
    /// `metadata_updated` audit event. Returns
    /// [`ServiceError::Store`]`(StoreError::NotFound)` when the document
    /// does not exist.
    pub async fn upsert_document_metadata(
        &self,
        metadata: &DocumentMetadata,
    ) -> Result<(), ServiceError> {
        Ok(metadata::upsert_document_metadata(&self.pool, metadata).await?)
    }

    /// Fetches the stored metadata row for a document; `None` when the
    /// document has no metadata yet.
    pub async fn get_document_metadata(
        &self,
        document_id: &DocumentId,
    ) -> Result<Option<DocumentMetadata>, ServiceError> {
        Ok(metadata::get_document_metadata(&self.pool, document_id).await?)
    }

    /// Sets (or overwrites) a single custom field, creating the
    /// metadata row with defaults when none exists. `key` must be
    /// non-empty. Returns the updated [`DocumentMetadata`].
    pub async fn set_custom_field(
        &self,
        document_id: &DocumentId,
        key: &str,
        value: &Value,
    ) -> Result<DocumentMetadata, ServiceError> {
        Ok(metadata::set_custom_field(&self.pool, document_id, key, value).await?)
    }

    /// Removes a single custom field. Returns `true` when a field was
    /// actually removed, `false` when the key was absent (or no
    /// metadata row exists).
    pub async fn remove_custom_field(
        &self,
        document_id: &DocumentId,
        key: &str,
    ) -> Result<bool, ServiceError> {
        Ok(metadata::remove_custom_field(&self.pool, document_id, key).await?)
    }

    /// Deletes the metadata row for a document; the document itself is
    /// not touched. Returns `false` when no row existed.
    pub async fn delete_document_metadata(
        &self,
        document_id: &DocumentId,
    ) -> Result<bool, ServiceError> {
        Ok(metadata::delete_document_metadata(&self.pool, document_id).await?)
    }

    // ------------------------------------------------------------------
    // Settings
    // ------------------------------------------------------------------

    /// Inserts or fully replaces the settings row for a document —
    /// the JSON `settings` blob and the `is_locked` flag. The blob is
    /// validated (keys and serialized size); records a
    /// `settings_updated` audit event.
    pub async fn upsert_document_settings(
        &self,
        settings: &DocumentSettings,
    ) -> Result<(), ServiceError> {
        Ok(settings::upsert_document_settings(&self.pool, settings).await?)
    }

    /// Fetches the settings row for a document; `None` when the
    /// document has no settings stored yet.
    pub async fn get_document_settings(
        &self,
        document_id: &DocumentId,
    ) -> Result<Option<DocumentSettings>, ServiceError> {
        Ok(settings::get_document_settings(&self.pool, document_id).await?)
    }

    /// Merges `patch` into the document's settings object — keys in
    /// `patch` overwrite, all other keys (and `is_locked`) are
    /// preserved. Creates the row with defaults when none exists.
    /// Returns the updated [`DocumentSettings`].
    pub async fn update_document_settings(
        &self,
        document_id: &DocumentId,
        patch: &serde_json::Map<String, Value>,
    ) -> Result<DocumentSettings, ServiceError> {
        Ok(settings::update_document_settings(&self.pool, document_id, patch).await?)
    }

    /// Sets (or clears) the document's `is_locked` flag, creating the
    /// settings row with defaults when none exists. Returns the
    /// updated [`DocumentSettings`].
    pub async fn set_document_locked(
        &self,
        document_id: &DocumentId,
        locked: bool,
    ) -> Result<DocumentSettings, ServiceError> {
        Ok(settings::set_document_locked(&self.pool, document_id, locked).await?)
    }

    /// Deletes the settings row for a document; the document itself
    /// is not touched. Returns `false` when no row existed.
    pub async fn delete_document_settings(
        &self,
        document_id: &DocumentId,
    ) -> Result<bool, ServiceError> {
        Ok(settings::delete_document_settings(&self.pool, document_id).await?)
    }

    // ------------------------------------------------------------------
    // Relationships
    // ------------------------------------------------------------------

    /// Creates a directed relationship from `source_document_id` to
    /// `target_document_id`. Both documents must exist, self-links and
    /// exact duplicates are rejected. Records a
    /// `relationship_created` audit event on the source document.
    pub async fn create_relationship(
        &self,
        source_document_id: &DocumentId,
        target_document_id: &DocumentId,
        relationship_type: DocumentRelationshipType,
        created_by: Option<&str>,
    ) -> Result<DocumentRelationship, ServiceError> {
        Ok(relationships::create_relationship(
            &self.pool,
            source_document_id,
            target_document_id,
            relationship_type,
            created_by,
        )
        .await?)
    }

    /// Deletes a relationship by id. Returns `false` when it did not
    /// exist.
    pub async fn delete_relationship(&self, id: &str) -> Result<bool, ServiceError> {
        Ok(relationships::delete_relationship(&self.pool, id).await?)
    }

    /// Lists relationships where the document is the source (outgoing
    /// links), oldest first.
    pub async fn list_outgoing_relationships(
        &self,
        source_document_id: &DocumentId,
    ) -> Result<Vec<DocumentRelationship>, ServiceError> {
        Ok(relationships::list_outgoing_relationships(&self.pool, source_document_id).await?)
    }

    /// Lists relationships where the document is the target (incoming
    /// links), oldest first.
    pub async fn list_incoming_relationships(
        &self,
        target_document_id: &DocumentId,
    ) -> Result<Vec<DocumentRelationship>, ServiceError> {
        Ok(relationships::list_incoming_relationships(&self.pool, target_document_id).await?)
    }

    /// Returns all documents related to `document_id` in both
    /// directions, deduplicated by document id. Outgoing links are
    /// listed before incoming ones.
    pub async fn related_documents(
        &self,
        document_id: &DocumentId,
    ) -> Result<Vec<RelatedDocument>, ServiceError> {
        Ok(relationships::related_documents(&self.pool, document_id).await?)
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

    #[tokio::test]
    async fn metadata_roundtrip_through_facade() {
        let svc = DocumentService::init(DocumentService::test_pool().await)
            .await
            .unwrap();
        let doc = svc.create_document(&new_doc("meta.pdf")).await.unwrap();

        assert!(svc.get_document_metadata(&doc.id).await.unwrap().is_none());

        let meta = DocumentMetadata {
            document_id: doc.id.clone(),
            title: Some("Annual Report".to_string()),
            author: Some("Ada".to_string()),
            subject: None,
            language: Some("en".to_string()),
            keywords: vec!["finance".to_string()],
            custom_fields: serde_json::Map::new(),
        };
        svc.upsert_document_metadata(&meta).await.unwrap();
        let fetched = svc.get_document_metadata(&doc.id).await.unwrap().unwrap();
        assert_eq!(fetched, meta);

        // Custom fields can be patched without a full upsert.
        let updated = svc
            .set_custom_field(&doc.id, "financial_year", &Value::from(2026))
            .await
            .unwrap();
        assert_eq!(updated.custom_fields["financial_year"], Value::from(2026));

        assert!(
            svc.remove_custom_field(&doc.id, "financial_year")
                .await
                .unwrap()
        );
        assert!(
            !svc.remove_custom_field(&doc.id, "financial_year")
                .await
                .unwrap()
        );
        let fetched = svc.get_document_metadata(&doc.id).await.unwrap().unwrap();
        assert!(!fetched.custom_fields.contains_key("financial_year"));

        assert!(svc.delete_document_metadata(&doc.id).await.unwrap());
        assert!(!svc.delete_document_metadata(&doc.id).await.unwrap());
        assert!(svc.get_document_metadata(&doc.id).await.unwrap().is_none());
    }

    #[tokio::test]
    async fn metadata_requires_existing_document() {
        let svc = DocumentService::init(DocumentService::test_pool().await)
            .await
            .unwrap();
        let meta = DocumentMetadata {
            document_id: DocumentId::new("doc-missing"),
            title: None,
            author: None,
            subject: None,
            language: None,
            keywords: Vec::new(),
            custom_fields: serde_json::Map::new(),
        };
        assert!(matches!(
            svc.upsert_document_metadata(&meta).await,
            Err(ServiceError::Store(
                crate::crates::document_engine::store::StoreError::NotFound { .. }
            ))
        ));
    }

    #[tokio::test]
    async fn relationships_through_facade() {
        let svc = DocumentService::init(DocumentService::test_pool().await)
            .await
            .unwrap();
        let a = svc.create_document(&new_doc("a.pdf")).await.unwrap();
        let b = svc.create_document(&new_doc("b.pdf")).await.unwrap();

        let rel = svc
            .create_relationship(
                &a.id,
                &b.id,
                DocumentRelationshipType::Attachment,
                Some("user-1"),
            )
            .await
            .unwrap();
        assert_eq!(rel.source_document_id, a.id);
        assert_eq!(rel.target_document_id, b.id);
        assert_eq!(rel.created_by.as_deref(), Some("user-1"));

        // Exact duplicates and self-links are rejected by the store.
        assert!(
            svc.create_relationship(&a.id, &b.id, DocumentRelationshipType::Attachment, None)
                .await
                .is_err()
        );
        assert!(
            svc.create_relationship(&a.id, &a.id, DocumentRelationshipType::Related, None)
                .await
                .is_err()
        );

        let outgoing = svc.list_outgoing_relationships(&a.id).await.unwrap();
        assert_eq!(outgoing.len(), 1);
        let incoming = svc.list_incoming_relationships(&b.id).await.unwrap();
        assert_eq!(incoming.len(), 1);
        assert!(
            svc.list_incoming_relationships(&a.id)
                .await
                .unwrap()
                .is_empty()
        );

        // `related_documents` sees the link from the target's side.
        let related = svc.related_documents(&b.id).await.unwrap();
        assert_eq!(related.len(), 1);
        assert_eq!(related[0].document.id, a.id);
        assert_eq!(
            related[0].relationship_type,
            DocumentRelationshipType::Attachment
        );
        assert!(!related[0].outgoing);

        assert!(svc.delete_relationship(&rel.id).await.unwrap());
        assert!(!svc.delete_relationship(&rel.id).await.unwrap());
        assert!(
            svc.list_outgoing_relationships(&a.id)
                .await
                .unwrap()
                .is_empty()
        );
    }

    #[tokio::test]
    async fn settings_roundtrip_through_facade() {
        let svc = DocumentService::init(DocumentService::test_pool().await)
            .await
            .unwrap();
        let doc = svc.create_document(&new_doc("settings.pdf")).await.unwrap();

        assert!(svc.get_document_settings(&doc.id).await.unwrap().is_none());

        // Merge-style update creates the row lazily.
        let mut patch = serde_json::Map::new();
        patch.insert("default_view".to_string(), Value::from("continuous"));
        let updated = svc.update_document_settings(&doc.id, &patch).await.unwrap();
        assert_eq!(updated.settings["default_view"], Value::from("continuous"));
        assert!(!updated.is_locked);

        // Lock flag is independent of the settings blob.
        let locked = svc.set_document_locked(&doc.id, true).await.unwrap();
        assert!(locked.is_locked);
        assert_eq!(locked.settings.len(), 1);

        // Full upsert replaces both blob and flag.
        let mut blob = serde_json::Map::new();
        blob.insert("theme".to_string(), Value::from("dark"));
        svc.upsert_document_settings(&DocumentSettings {
            document_id: doc.id.clone(),
            settings: blob,
            is_locked: false,
            created_at: String::new(),
            updated_at: String::new(),
        })
        .await
        .unwrap();
        let fetched = svc.get_document_settings(&doc.id).await.unwrap().unwrap();
        assert_eq!(fetched.settings.len(), 1);
        assert_eq!(fetched.settings["theme"], Value::from("dark"));
        assert!(!fetched.is_locked);
        // created_at from the pre-existing row is preserved.
        assert!(!fetched.created_at.is_empty());

        assert!(svc.delete_document_settings(&doc.id).await.unwrap());
        assert!(!svc.delete_document_settings(&doc.id).await.unwrap());
        assert!(svc.get_document_settings(&doc.id).await.unwrap().is_none());

        // Audit events recorded for each mutation: update, set_locked,
        // upsert, and the successful delete (the second delete removes
        // nothing and records nothing).
        let history = svc.get_document_history(&doc.id).await.unwrap();
        let settings_events = history
            .iter()
            .filter(|e| {
                e.event_type
                    == crate::crates::document_engine::audit::DocumentAuditEventType::SettingsUpdated
            })
            .count();
        assert_eq!(settings_events, 4);
    }

    #[tokio::test]
    async fn settings_validation_through_facade() {
        let svc = DocumentService::init(DocumentService::test_pool().await)
            .await
            .unwrap();
        let doc = svc.create_document(&new_doc("v.pdf")).await.unwrap();

        let mut bad = serde_json::Map::new();
        bad.insert("".to_string(), Value::from(1));
        assert!(matches!(
            svc.update_document_settings(&doc.id, &bad).await,
            Err(ServiceError::Store(
                crate::crates::document_engine::store::StoreError::Validation { .. }
            ))
        ));

        // Unknown document → NotFound.
        let missing = DocumentId::new("doc-missing");
        assert!(matches!(
            svc.set_document_locked(&missing, true).await,
            Err(ServiceError::Store(
                crate::crates::document_engine::store::StoreError::NotFound { .. }
            ))
        ));
    }

    #[tokio::test]
    async fn archive_unarchive_through_facade() {
        let svc = DocumentService::init(DocumentService::test_pool().await)
            .await
            .unwrap();
        let doc = svc.create_document(&new_doc("archive.pdf")).await.unwrap();

        let archived = svc.archive_document(&doc.id).await.unwrap();
        assert!(
            archived.status == crate::crates::document_engine::document::DocumentStatus::Archived
        );
        // Archived documents are still listed (not trashed).
        assert_eq!(
            svc.list_documents(Some("user-1"), None, false, 10)
                .await
                .unwrap()
                .len(),
            1
        );
        // Double-archive is rejected.
        assert!(svc.archive_document(&doc.id).await.is_err());

        let restored = svc.unarchive_document(&doc.id).await.unwrap();
        assert!(
            restored.status == crate::crates::document_engine::document::DocumentStatus::Active
        );
        // Unarchiving a non-archived document is rejected.
        assert!(svc.unarchive_document(&doc.id).await.is_err());

        // Trashed documents cannot be archived.
        svc.delete_document(&doc.id).await.unwrap();
        assert!(svc.archive_document(&doc.id).await.is_err());
    }
}
