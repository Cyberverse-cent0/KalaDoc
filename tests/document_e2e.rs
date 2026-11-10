//! End-to-end document lifecycle tests through the application-service
//! layer (`kaladoc::crates::application::services`).
//!
//! Covers the Definition-of-Done flow: ingest → artifact/version/
//! metadata/audit registration → versioning → restore → rename/move/
//! copy/delete/restore/purge → folders/tags/permissions/relationships/
//! metadata/settings roundtrips → migration idempotency → error paths.
//!
//! Services are built without real config: an in-memory SQLite pool
//! (`sqlite::memory:`, capped at one connection so all clones share the
//! same database) plus a [`StorageConfig`] rooted at a unique tempdir.

use std::io;
use std::path::PathBuf;
use std::pin::Pin;
use std::sync::atomic::{AtomicU64, Ordering};
use std::task::{Context, Poll};

use kaladoc::crates::application::services::documents::{
    DocumentPatch, DocumentRelationshipType, NewArtifact, NewDocument,
};
use kaladoc::crates::application::services::{
    DocumentIngestionService, DocumentService, IngestRequest, ServiceError, StorageService,
};
use kaladoc::crates::document_engine::artifact::{ChecksumAlgorithm, DocumentChecksum};
use kaladoc::crates::document_engine::audit::DocumentAuditEventType;
use kaladoc::crates::document_engine::document::{
    DocumentName, DocumentOrigin, DocumentSource, DocumentStatus, DocumentType,
};
use kaladoc::crates::document_engine::ids::{DocumentId, FolderId};
use kaladoc::crates::document_engine::lifecycle::DocumentAccessLevel;
use kaladoc::crates::document_engine::metadata::DocumentMetadata;
use kaladoc::crates::document_engine::settings::DocumentSettings;
use kaladoc::crates::document_engine::store::StoreError;
use kaladoc::crates::storage::local::config::StorageConfig;
use serde_json::{Map, json};
use sha2::{Digest, Sha256};
use sqlx::{SqlitePool, sqlite::SqlitePoolOptions};
use tokio::io::{AsyncRead, ReadBuf};

// ---------------------------------------------------------------------------
// Test helpers
// ---------------------------------------------------------------------------

static NEXT_DIR: AtomicU64 = AtomicU64::new(0);

/// Unique tempdir per test; removed on drop.
struct TempRoot(PathBuf);

impl TempRoot {
    fn new() -> Self {
        let dir = std::env::temp_dir().join(format!(
            "kaladoc-e2e-{}-{}",
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

/// `sqlite::memory:` is per-connection; cap the pool at one connection
/// so every clone shares the same database.
async fn test_pool() -> SqlitePool {
    SqlitePoolOptions::new()
        .max_connections(1)
        .connect("sqlite::memory:")
        .await
        .expect("in-memory sqlite pool")
}

fn new_doc(name: &str) -> NewDocument {
    NewDocument {
        name: name.to_string(),
        document_type: DocumentType::Text,
        owner_id: "user-1".to_string(),
        source: DocumentSource::new(DocumentOrigin::Upload),
    }
}

fn pdf_bytes() -> Vec<u8> {
    b"%PDF-1.4\n1 0 obj\n<<>>\nendobj\ntrailer\n%%EOF\n".to_vec()
}

fn hex_sha256(data: &[u8]) -> String {
    format!("{:x}", Sha256::digest(data))
}

/// Minimal in-memory `AsyncRead` over a byte slice.
struct SliceReader {
    data: Vec<u8>,
    pos: usize,
}

impl SliceReader {
    fn new(data: &[u8]) -> Self {
        Self {
            data: data.to_vec(),
            pos: 0,
        }
    }
}

impl AsyncRead for SliceReader {
    fn poll_read(
        mut self: Pin<&mut Self>,
        _cx: &mut Context<'_>,
        buf: &mut ReadBuf<'_>,
    ) -> Poll<io::Result<()>> {
        let remaining = &self.data[self.pos..];
        let n = remaining.len().min(buf.remaining());
        buf.put_slice(&remaining[..n]);
        self.pos += n;
        Poll::Ready(Ok(()))
    }
}

fn empty_metadata(document_id: &DocumentId) -> DocumentMetadata {
    DocumentMetadata {
        document_id: document_id.clone(),
        title: None,
        author: None,
        subject: None,
        language: None,
        keywords: Vec::new(),
        custom_fields: Map::new(),
    }
}

// ---------------------------------------------------------------------------
// 1. Full DoD ingest → version → restore flow
// ---------------------------------------------------------------------------

#[tokio::test]
async fn ingest_registers_document_and_supports_version_lifecycle() {
    let root = TempRoot::new();
    let config = root.config();
    let pool = test_pool().await;

    // DocumentService::init runs the schema migrations; the ingestion
    // service shares the same pool (single connection → same database).
    let docs = DocumentService::init(pool.clone()).await.unwrap();
    let mut storage = StorageService::from_config(&config).unwrap();
    let ingest = DocumentIngestionService::new(pool.clone(), config);

    let payload = pdf_bytes();
    let mut request = IngestRequest::new("user-1", "report.pdf");
    request.mime_type = Some("application/pdf".to_string());
    request.tags = vec!["finance".to_string(), "q3".to_string()];
    request.actor_id = Some("user-1".to_string());
    request.title = Some("Q3 Report".to_string());

    let outcome = ingest
        .ingest(SliceReader::new(&payload), request)
        .await
        .expect("ingest must succeed");

    // -- Document row -------------------------------------------------
    let doc = &outcome.document;
    assert_eq!(doc.name.as_str(), "report.pdf");
    assert_eq!(doc.document_type, DocumentType::Pdf);
    assert_eq!(doc.status, DocumentStatus::Active);
    assert_eq!(doc.owner_id, "user-1");
    assert_eq!(doc.source.origin, DocumentOrigin::Import);

    // -- Artifact + object bytes --------------------------------------
    let expected_uri = format!("local://documents/{}/report.pdf", doc.id);
    assert_eq!(outcome.storage_uri, expected_uri);
    assert_eq!(outcome.size_bytes, payload.len() as u64);
    assert_eq!(outcome.sha256, hex_sha256(&payload));
    assert_eq!(outcome.detected_mime_type, "application/pdf");

    let artifact = &outcome.artifact;
    assert_eq!(artifact.document_id, doc.id);
    assert_eq!(artifact.storage_uri, expected_uri);
    assert_eq!(artifact.file_name, "report.pdf");
    assert_eq!(artifact.mime_type.as_deref(), Some("application/pdf"));
    assert_eq!(artifact.size_bytes, payload.len() as u64);
    let checksum = artifact.checksum.as_ref().expect("checksum recorded");
    assert_eq!(checksum.algorithm, ChecksumAlgorithm::Sha256);
    assert_eq!(checksum.value, outcome.sha256);

    // The committed bytes are actually readable through StorageService.
    let object_name = expected_uri.strip_prefix("local://").unwrap();
    assert_eq!(storage.get(object_name).unwrap(), payload);

    // -- Version 1 ----------------------------------------------------
    let version = &outcome.version;
    assert_eq!(version.document_id, doc.id);
    assert_eq!(version.version_number, 1);
    assert!(version.is_current);
    assert_eq!(version.artifact_id.as_deref(), Some(artifact.id.as_str()));
    assert_eq!(version.created_by, "user-1");

    // -- Metadata row --------------------------------------------------
    let meta = docs
        .get_document_metadata(&doc.id)
        .await
        .unwrap()
        .expect("metadata row written by ingest");
    assert_eq!(meta.title.as_deref(), Some("Q3 Report"));
    assert_eq!(meta.keywords, vec!["finance".to_string(), "q3".to_string()]);
    assert_eq!(meta.custom_fields["detected_format"], json!("PDF"));
    assert_eq!(
        meta.custom_fields["detected_mime_type"],
        json!("application/pdf")
    );
    assert_eq!(meta.custom_fields["sha256"], json!(outcome.sha256.as_str()));

    // -- Audit history -------------------------------------------------
    let history = docs.get_document_history(&doc.id).await.unwrap();
    let event_types: Vec<_> = history.iter().map(|e| e.event_type).collect();
    for expected in [
        DocumentAuditEventType::Created,
        DocumentAuditEventType::Imported,
        DocumentAuditEventType::VersionCreated,
        DocumentAuditEventType::MetadataUpdated,
    ] {
        assert!(
            event_types.contains(&expected),
            "missing audit event {expected:?} in {event_types:?}"
        );
    }
    let imported = history
        .iter()
        .find(|e| e.event_type == DocumentAuditEventType::Imported)
        .unwrap();
    assert_eq!(imported.actor_id.as_deref(), Some("user-1"));

    // -- Tags -----------------------------------------------------------
    let tags = docs.list_tags(&doc.id).await.unwrap();
    let mut tag_names: Vec<_> = tags.iter().map(|t| t.name.as_str()).collect();
    tag_names.sort_unstable();
    assert_eq!(tag_names, vec!["finance", "q3"]);

    // -- Listing/search -------------------------------------------------
    let mine = docs
        .list_documents(Some("user-1"), None, false, 10)
        .await
        .unwrap();
    assert_eq!(mine.len(), 1);
    assert_eq!(mine[0].id, doc.id);
    assert!(
        docs.list_documents(Some("someone-else"), None, false, 10)
            .await
            .unwrap()
            .is_empty()
    );
    let found = docs.search_documents("report", 10).await.unwrap();
    assert_eq!(found.len(), 1);
    assert_eq!(found[0].id, doc.id);

    let artifacts = docs.list_artifacts(&doc.id).await.unwrap();
    assert_eq!(artifacts.len(), 1);
    assert_eq!(artifacts[0].id, artifact.id);
    let resolved = docs.download_artifact(&artifact.id).await.unwrap().unwrap();
    assert_eq!(resolved.storage_uri, expected_uri);

    // -- Second artifact + version 2 via service methods ---------------
    let payload_v2 = b"%PDF-1.4\n% second revision bytes\n%%EOF\n".to_vec();
    let v2_object = format!("documents/{}/revision-2.pdf", doc.id);
    storage.put(&v2_object, &payload_v2).unwrap();
    let v2_uri = format!("local://{v2_object}");

    let artifact_v2 = docs
        .upload_artifact(&NewArtifact {
            id: None,
            document_id: doc.id.clone(),
            storage_uri: v2_uri.clone(),
            file_name: "revision-2.pdf".to_string(),
            mime_type: Some("application/pdf".to_string()),
            size_bytes: payload_v2.len() as u64,
            checksum: Some(DocumentChecksum {
                algorithm: ChecksumAlgorithm::Sha256,
                value: hex_sha256(&payload_v2),
            }),
        })
        .await
        .unwrap();

    let version_v2 = docs
        .create_version(&doc.id, Some(&artifact_v2.id), "user-1", Some("second cut"))
        .await
        .unwrap();
    assert_eq!(version_v2.version_number, 2);
    assert!(version_v2.is_current);

    // v1 lost the current flag; v2 holds it.
    let v1 = docs.get_version(&doc.id, 1).await.unwrap().unwrap();
    assert!(!v1.is_current);
    let v2 = docs.get_version(&doc.id, 2).await.unwrap().unwrap();
    assert!(v2.is_current);
    assert_eq!(v2.artifact_id.as_deref(), Some(artifact_v2.id.as_str()));

    let versions = docs.list_versions(&doc.id).await.unwrap();
    assert_eq!(versions.len(), 2);
    assert_eq!(versions[0].version_number, 2); // newest first
    assert_eq!(versions[1].version_number, 1);

    // -- Restore to version 1 -------------------------------------------
    let restored = docs.restore_version(&doc.id, 1).await.unwrap();
    assert_eq!(restored.id, doc.id);

    let v1 = docs.get_version(&doc.id, 1).await.unwrap().unwrap();
    assert!(v1.is_current);
    let v2 = docs.get_version(&doc.id, 2).await.unwrap().unwrap();
    assert!(!v2.is_current);

    // All artifacts are retained — restore moves a pointer, not bytes.
    let artifacts = docs.list_artifacts(&doc.id).await.unwrap();
    assert_eq!(artifacts.len(), 2);
    assert!(storage.exists(&v2_object).unwrap());
    assert!(storage.exists(object_name).unwrap());
}

// ---------------------------------------------------------------------------
// 2. Rename / move / copy / delete / restore / purge lifecycle
// ---------------------------------------------------------------------------

#[tokio::test]
async fn document_lifecycle_rename_move_copy_delete_restore_purge() {
    let pool = test_pool().await;
    let svc = DocumentService::init(pool).await.unwrap();

    let doc = svc.create_document(&new_doc("draft.txt")).await.unwrap();
    assert_eq!(doc.status, DocumentStatus::Active);
    assert!(doc.folder_id.is_none());

    // Rename + patch.
    let renamed = svc.rename_document(&doc.id, "final.txt").await.unwrap();
    assert_eq!(renamed.name.as_str(), "final.txt");
    let patched = svc
        .update_document(
            &doc.id,
            &DocumentPatch {
                name: Some("final-v2.txt".to_string()),
                document_type: Some(DocumentType::Pdf),
            },
        )
        .await
        .unwrap();
    assert_eq!(patched.name.as_str(), "final-v2.txt");
    assert_eq!(patched.document_type, DocumentType::Pdf);

    // Folders: create, nest, move the document.
    let work = svc.create_folder("Work", None, "user-1").await.unwrap();
    let year = svc
        .create_folder("2026", Some(&work.id), "user-1")
        .await
        .unwrap();
    assert_eq!(year.parent_id.as_ref(), Some(&work.id));

    let moved = svc.move_document(&doc.id, Some(&year.id)).await.unwrap();
    assert_eq!(moved.folder_id.as_ref(), Some(&year.id));

    // Back to root.
    let moved = svc.move_document(&doc.id, None).await.unwrap();
    assert!(moved.folder_id.is_none());
    svc.move_document(&doc.id, Some(&year.id)).await.unwrap();

    // Copy duplicates the row (and inherits the folder); bytes are shared.
    let copy = svc
        .copy_document(&doc.id, Some("final-v2-copy.txt"))
        .await
        .unwrap();
    assert_ne!(copy.id, doc.id);
    assert_eq!(copy.name.as_str(), "final-v2-copy.txt");
    assert_eq!(copy.folder_id, doc.folder_id);
    assert_eq!(copy.status, DocumentStatus::Active);

    // Delete → trash → hidden from default listing → restore.
    let trashed = svc.delete_document(&doc.id).await.unwrap();
    assert_eq!(trashed.status, DocumentStatus::Trashed);
    assert!(
        svc.list_documents(Some("user-1"), None, false, 10)
            .await
            .unwrap()
            .iter()
            .all(|d| d.id != doc.id)
    );
    let with_deleted = svc
        .list_documents(Some("user-1"), None, true, 10)
        .await
        .unwrap();
    assert!(with_deleted.iter().any(|d| d.id == doc.id));

    let restored = svc.restore_document(&doc.id).await.unwrap();
    assert_eq!(restored.status, DocumentStatus::Active);

    // Archive / unarchive.
    let archived = svc.archive_document(&doc.id).await.unwrap();
    assert_eq!(archived.status, DocumentStatus::Archived);
    let unarchived = svc.unarchive_document(&doc.id).await.unwrap();
    assert_eq!(unarchived.status, DocumentStatus::Active);
    // Unarchiving an active document is rejected.
    assert!(svc.unarchive_document(&doc.id).await.is_err());

    // Purge requires the trashed state.
    svc.delete_document(&doc.id).await.unwrap();
    assert!(svc.purge_document(&doc.id).await.unwrap());
    assert!(svc.get_document(&doc.id).await.unwrap().is_none());
    // Second purge reports false — already gone.
    assert!(!svc.purge_document(&doc.id).await.unwrap());

    // The copy survives untouched.
    let copy = svc.get_document(&copy.id).await.unwrap().unwrap();
    assert_eq!(copy.status, DocumentStatus::Active);
}

// ---------------------------------------------------------------------------
// 3. Folders + tags + permissions + relationships + metadata/settings
// ---------------------------------------------------------------------------

#[tokio::test]
async fn organization_features_roundtrip() {
    let pool = test_pool().await;
    let svc = DocumentService::init(pool).await.unwrap();

    let doc_a = svc.create_document(&new_doc("a.pdf")).await.unwrap();
    let doc_b = svc.create_document(&new_doc("b.pdf")).await.unwrap();

    // -- Folders --------------------------------------------------------
    let root = svc.create_folder("Invoices", None, "user-1").await.unwrap();
    let child = svc
        .create_folder("2026", Some(&root.id), "user-1")
        .await
        .unwrap();
    svc.rename_folder(&child.id, "FY2026").await.unwrap();
    let renamed = svc.get_folder(&child.id).await.unwrap().unwrap();
    assert_eq!(renamed.name, "FY2026");

    // Reparenting a folder under its own descendant is a cycle → error.
    assert!(
        svc.move_folder(&root.id, Some(&child.id)).await.is_err(),
        "moving a folder under its descendant must fail"
    );
    // Moving the child to root works.
    let moved = svc.move_folder(&child.id, None).await.unwrap();
    assert!(moved.parent_id.is_none());
    svc.move_folder(&child.id, Some(&root.id)).await.unwrap();

    let roots = svc.list_folders(Some("user-1"), None).await.unwrap();
    assert_eq!(roots.len(), 1);
    assert_eq!(roots[0].id, root.id);
    let children = svc
        .list_folders(Some("user-1"), Some(&root.id))
        .await
        .unwrap();
    assert_eq!(children.len(), 1);
    assert_eq!(children[0].id, child.id);

    // Put a document in the folder, then delete the folder: the document
    // is moved back to root while the subtree is removed.
    svc.move_document(&doc_a.id, Some(&child.id)).await.unwrap();
    assert!(svc.delete_folder(&root.id).await.unwrap());
    let doc_a = svc.get_document(&doc_a.id).await.unwrap().unwrap();
    assert!(doc_a.folder_id.is_none());
    assert!(svc.get_folder(&child.id).await.unwrap().is_none());

    // -- Tags -------------------------------------------------------------
    svc.add_tag(&doc_a.id, "urgent", Some("user-1"))
        .await
        .unwrap();
    // Re-adding the same tag is idempotent (INSERT OR IGNORE).
    svc.add_tag(&doc_a.id, "urgent", None).await.unwrap();
    svc.add_tag(&doc_a.id, "review", None).await.unwrap();
    svc.add_tag(&doc_b.id, "urgent", None).await.unwrap();

    let mut tags_a = svc.list_tags(&doc_a.id).await.unwrap();
    tags_a.sort_by(|x, y| x.name.cmp(&y.name));
    let names: Vec<_> = tags_a.iter().map(|t| t.name.as_str()).collect();
    assert_eq!(names, vec!["review", "urgent"]);

    assert!(svc.remove_tag(&doc_a.id, "urgent").await.unwrap());
    assert!(!svc.remove_tag(&doc_a.id, "urgent").await.unwrap());
    let tags_a = svc.list_tags(&doc_a.id).await.unwrap();
    assert_eq!(tags_a.len(), 1);
    assert_eq!(tags_a[0].name, "review");

    // -- Permissions ------------------------------------------------------
    let perm = svc
        .grant_permission(
            &doc_a.id,
            "user-2",
            DocumentAccessLevel::Read,
            Some("user-1"),
        )
        .await
        .unwrap();
    assert_eq!(perm.subject_id, "user-2");
    assert_eq!(perm.access_level, DocumentAccessLevel::Read);
    assert_eq!(perm.granted_by.as_deref(), Some("user-1"));

    // Re-granting updates the level in place.
    let perm = svc
        .grant_permission(&doc_a.id, "user-2", DocumentAccessLevel::Manage, None)
        .await
        .unwrap();
    assert_eq!(perm.access_level, DocumentAccessLevel::Manage);

    let perms = svc.get_document_permissions(&doc_a.id).await.unwrap();
    assert_eq!(perms.len(), 1);
    let subject_perms = svc.list_permissions_for_subject("user-2").await.unwrap();
    assert_eq!(subject_perms.len(), 1);
    assert_eq!(subject_perms[0].document_id, doc_a.id);

    assert!(
        svc.revoke_permission(&doc_a.id, "user-2", Some("user-1"))
            .await
            .unwrap()
    );
    assert!(
        !svc.revoke_permission(&doc_a.id, "user-2", None)
            .await
            .unwrap()
    );
    assert!(
        svc.get_document_permissions(&doc_a.id)
            .await
            .unwrap()
            .is_empty()
    );

    // -- Relationships -----------------------------------------------------
    let rel = svc
        .create_relationship(
            &doc_a.id,
            &doc_b.id,
            DocumentRelationshipType::Attachment,
            Some("user-1"),
        )
        .await
        .unwrap();
    assert_eq!(rel.source_document_id, doc_a.id);
    assert_eq!(rel.target_document_id, doc_b.id);

    let outgoing = svc.list_outgoing_relationships(&doc_a.id).await.unwrap();
    assert_eq!(outgoing.len(), 1);
    let incoming = svc.list_incoming_relationships(&doc_b.id).await.unwrap();
    assert_eq!(incoming.len(), 1);

    // `related_documents` sees the link from both sides.
    let related_a = svc.related_documents(&doc_a.id).await.unwrap();
    assert_eq!(related_a.len(), 1);
    assert_eq!(related_a[0].document.id, doc_b.id);
    assert!(related_a[0].outgoing);
    let related_b = svc.related_documents(&doc_b.id).await.unwrap();
    assert_eq!(related_b.len(), 1);
    assert_eq!(related_b[0].document.id, doc_a.id);
    assert!(!related_b[0].outgoing);

    assert!(svc.delete_relationship(&rel.id).await.unwrap());
    assert!(!svc.delete_relationship(&rel.id).await.unwrap());

    // -- Metadata ----------------------------------------------------------
    assert!(
        svc.get_document_metadata(&doc_a.id)
            .await
            .unwrap()
            .is_none()
    );
    let meta = DocumentMetadata {
        document_id: doc_a.id.clone(),
        title: Some("Annual Report".to_string()),
        author: Some("Ada".to_string()),
        subject: Some("Finance".to_string()),
        language: Some("en".to_string()),
        keywords: vec!["annual".to_string()],
        custom_fields: Map::from_iter([("department".to_string(), json!("ops"))]),
    };
    svc.upsert_document_metadata(&meta).await.unwrap();
    let fetched = svc.get_document_metadata(&doc_a.id).await.unwrap().unwrap();
    assert_eq!(fetched, meta);

    let patched_meta = svc
        .set_custom_field(&doc_a.id, "financial_year", &json!(2026))
        .await
        .unwrap();
    assert_eq!(patched_meta.custom_fields["financial_year"], json!(2026));
    assert_eq!(patched_meta.custom_fields["department"], json!("ops"));

    assert!(
        svc.remove_custom_field(&doc_a.id, "financial_year")
            .await
            .unwrap()
    );
    assert!(svc.delete_document_metadata(&doc_a.id).await.unwrap());
    assert!(!svc.delete_document_metadata(&doc_a.id).await.unwrap());
    assert!(
        svc.get_document_metadata(&doc_a.id)
            .await
            .unwrap()
            .is_none()
    );

    // -- Settings ----------------------------------------------------------
    assert!(
        svc.get_document_settings(&doc_a.id)
            .await
            .unwrap()
            .is_none()
    );
    let mut patch = Map::new();
    patch.insert("default_view".to_string(), json!("grid"));
    let settings = svc
        .update_document_settings(&doc_a.id, &patch)
        .await
        .unwrap();
    assert_eq!(settings.settings["default_view"], json!("grid"));
    assert!(!settings.is_locked);

    let locked = svc.set_document_locked(&doc_a.id, true).await.unwrap();
    assert!(locked.is_locked);
    assert_eq!(locked.settings["default_view"], json!("grid"));

    // Full upsert replaces the blob.
    let mut blob = Map::new();
    blob.insert("theme".to_string(), json!("dark"));
    svc.upsert_document_settings(&DocumentSettings {
        document_id: doc_a.id.clone(),
        settings: blob,
        is_locked: false,
        created_at: "2024-01-01T00:00:00Z".to_string(),
        updated_at: "2024-01-01T00:00:00Z".to_string(),
    })
    .await
    .unwrap();
    let fetched = svc.get_document_settings(&doc_a.id).await.unwrap().unwrap();
    assert_eq!(fetched.settings["theme"], json!("dark"));
    assert!(!fetched.settings.contains_key("default_view"));
    assert!(!fetched.is_locked);

    assert!(svc.delete_document_settings(&doc_a.id).await.unwrap());
    assert!(!svc.delete_document_settings(&doc_a.id).await.unwrap());
    assert!(
        svc.get_document_settings(&doc_a.id)
            .await
            .unwrap()
            .is_none()
    );

    // -- Custom audit event --------------------------------------------------
    svc.record_event(
        &doc_a.id,
        Some("user-1"),
        DocumentAuditEventType::Other,
        Some(&json!({ "note": "e2e" })),
    )
    .await
    .unwrap();
    let history = svc.get_document_history(&doc_a.id).await.unwrap();
    assert!(
        history
            .iter()
            .any(|e| e.event_type == DocumentAuditEventType::Other)
    );
}

// ---------------------------------------------------------------------------
// 4. Migration idempotency
// ---------------------------------------------------------------------------

#[tokio::test]
async fn init_migrations_are_idempotent() {
    let pool = test_pool().await;

    // init() runs the schema migrations; calling it repeatedly on the
    // same pool must succeed and leave a usable store.
    let svc1 = DocumentService::init(pool.clone()).await.unwrap();
    let svc2 = DocumentService::init(pool.clone()).await.unwrap();
    let svc3 = DocumentService::init(pool).await.unwrap();

    // All handles see the same database.
    let doc = svc1.create_document(&new_doc("idem.txt")).await.unwrap();
    assert!(svc2.get_document(&doc.id).await.unwrap().is_some());
    assert!(svc3.get_document(&doc.id).await.unwrap().is_some());

    // A second init on an already-migrated store doesn't wipe data.
    let svc4 = DocumentService::init(svc3.pool().clone()).await.unwrap();
    assert!(svc4.get_document(&doc.id).await.unwrap().is_some());
}

// ---------------------------------------------------------------------------
// 5. Missing resources → None / false
// ---------------------------------------------------------------------------

#[tokio::test]
async fn missing_resources_return_none_or_false() {
    let pool = test_pool().await;
    let svc = DocumentService::init(pool).await.unwrap();

    let missing_doc = DocumentId::new("doc-does-not-exist");
    assert!(svc.get_document(&missing_doc).await.unwrap().is_none());
    assert!(
        svc.get_document_metadata(&missing_doc)
            .await
            .unwrap()
            .is_none()
    );
    assert!(
        svc.get_document_settings(&missing_doc)
            .await
            .unwrap()
            .is_none()
    );
    assert!(svc.list_artifacts(&missing_doc).await.unwrap().is_empty());
    assert!(svc.list_versions(&missing_doc).await.unwrap().is_empty());
    assert!(svc.list_tags(&missing_doc).await.unwrap().is_empty());
    assert!(
        svc.get_document_permissions(&missing_doc)
            .await
            .unwrap()
            .is_empty()
    );
    assert!(
        svc.list_outgoing_relationships(&missing_doc)
            .await
            .unwrap()
            .is_empty()
    );
    assert!(
        svc.list_incoming_relationships(&missing_doc)
            .await
            .unwrap()
            .is_empty()
    );
    assert!(
        svc.related_documents(&missing_doc)
            .await
            .unwrap()
            .is_empty()
    );
    assert!(
        svc.get_document_history(&missing_doc)
            .await
            .unwrap()
            .is_empty()
    );
    assert!(!svc.purge_document(&missing_doc).await.unwrap());

    // Lookups by non-row identifiers.
    assert!(svc.download_artifact("art-nope").await.unwrap().is_none());
    assert!(
        svc.get_folder(&FolderId::new("folder-nope"))
            .await
            .unwrap()
            .is_none()
    );
    assert!(
        !svc.delete_folder(&FolderId::new("folder-nope"))
            .await
            .unwrap()
    );
    assert!(!svc.delete_relationship("rel-nope").await.unwrap());

    // Mutations that report absence with `false`.
    let doc = svc.create_document(&new_doc("present.txt")).await.unwrap();
    assert!(!svc.remove_tag(&doc.id, "nope").await.unwrap());
    assert!(!svc.remove_custom_field(&doc.id, "nope").await.unwrap());
    assert!(!svc.delete_document_metadata(&doc.id).await.unwrap());
    assert!(!svc.delete_document_settings(&doc.id).await.unwrap());
    assert!(
        !svc.revoke_permission(&doc.id, "user-9", None)
            .await
            .unwrap()
    );
    assert!(svc.get_version(&doc.id, 42).await.unwrap().is_none());
}

// ---------------------------------------------------------------------------
// 6. Error paths
// ---------------------------------------------------------------------------

#[tokio::test]
async fn invalid_inputs_are_rejected() {
    let root = TempRoot::new();
    let config = root.config();
    let pool = test_pool().await;
    let svc = DocumentService::init(pool.clone()).await.unwrap();
    let ingest = DocumentIngestionService::new(pool, config);

    // -- Document names ----------------------------------------------------
    for bad in ["", "   ", "a/b.pdf", "a\\b.pdf", "x\x00y"] {
        assert!(
            svc.create_document(&NewDocument {
                name: bad.to_string(),
                document_type: DocumentType::Text,
                owner_id: "user-1".to_string(),
                source: DocumentSource::new(DocumentOrigin::Upload),
            })
            .await
            .is_err(),
            "name {bad:?} must be rejected"
        );
    }
    let long_name = "x".repeat(DocumentName::MAX_LEN + 1);
    assert!(svc.create_document(&new_doc(&long_name)).await.is_err());

    let doc = svc.create_document(&new_doc("ok.txt")).await.unwrap();
    assert!(svc.rename_document(&doc.id, "bad/name").await.is_err());
    assert!(svc.rename_document(&doc.id, "").await.is_err());

    // -- Relationships -------------------------------------------------------
    let other = svc.create_document(&new_doc("other.txt")).await.unwrap();
    // Self-link rejected.
    assert!(
        svc.create_relationship(&doc.id, &doc.id, DocumentRelationshipType::Related, None)
            .await
            .is_err()
    );
    // Missing target rejected.
    assert!(
        svc.create_relationship(
            &doc.id,
            &DocumentId::new("doc-missing"),
            DocumentRelationshipType::Related,
            None,
        )
        .await
        .is_err()
    );
    // Missing source rejected too.
    assert!(
        svc.create_relationship(
            &DocumentId::new("doc-missing"),
            &other.id,
            DocumentRelationshipType::Related,
            None,
        )
        .await
        .is_err()
    );

    // -- Metadata on missing document ----------------------------------------
    let mut meta = empty_metadata(&DocumentId::new("doc-missing"));
    meta.title = Some("ghost".to_string());
    assert!(matches!(
        svc.upsert_document_metadata(&meta).await,
        Err(ServiceError::Store(StoreError::NotFound { .. }))
    ));

    // -- Custom fields ---------------------------------------------------------
    assert!(svc.set_custom_field(&doc.id, "", &json!(1)).await.is_err());
    assert!(
        svc.set_custom_field(&DocumentId::new("doc-missing"), "k", &json!(1))
            .await
            .is_err()
    );

    // -- Versions on missing document / version -------------------------------
    assert!(
        svc.create_version(&DocumentId::new("doc-missing"), None, "user-1", None)
            .await
            .is_err()
    );
    assert!(svc.restore_version(&doc.id, 99).await.is_err());

    // -- Ingest validation ------------------------------------------------------
    // Empty stream.
    assert!(
        ingest
            .ingest(SliceReader::new(b""), IngestRequest::new("u", "e.bin"))
            .await
            .is_err()
    );
    // Unsafe file names.
    for name in ["../evil.pdf", "a/b.pdf", "a\\b.pdf", ".", ".."] {
        assert!(
            ingest
                .ingest(
                    SliceReader::new(b"data"),
                    IngestRequest::new("user-1", name),
                )
                .await
                .is_err(),
            "file name {name:?} must be rejected"
        );
    }
    // Empty owner.
    assert!(
        ingest
            .ingest(
                SliceReader::new(b"data"),
                IngestRequest::new("   ", "ok.bin"),
            )
            .await
            .is_err()
    );
    // Unknown folder fails after commit and rolls everything back.
    let mut req = IngestRequest::new("user-1", "orphan.pdf");
    req.folder_id = Some(FolderId::new("folder-does-not-exist"));
    assert!(
        ingest
            .ingest(SliceReader::new(&pdf_bytes()), req)
            .await
            .is_err()
    );

    // Only `doc` (and `other`) remain — failed ingests left no rows.
    let all = svc.list_documents(None, None, true, 100).await.unwrap();
    assert_eq!(all.len(), 2);
    assert!(all.iter().all(|d| d.id == doc.id || d.id == other.id));
}
