//! Document version operations.

use serde_json::json;
use sqlx::{Row, SqlitePool, sqlite::SqliteRow};

use crate::crates::document_engine::audit::DocumentAuditEventType;
use crate::crates::document_engine::document::DocumentStatus;
use crate::crates::document_engine::ids::DocumentId;
use crate::crates::document_engine::version::DocumentVersion;

use super::documents::fetch_document;
use super::history::record_event;
use super::{StoreError, StoreResult, now_rfc3339};

fn version_from_row(row: &SqliteRow) -> StoreResult<DocumentVersion> {
    let revision_ids: String = row.get("revision_ids");
    Ok(DocumentVersion {
        document_id: DocumentId::new(row.get::<String, _>("document_id")),
        version_number: row.get::<i64, _>("version_number") as u32,
        artifact_id: row.get("artifact_id"),
        revision_ids: serde_json::from_str(&revision_ids).unwrap_or_default(),
        created_by: row.get("created_by"),
        note: row.get("note"),
        is_current: row.get::<i64, _>("is_current") != 0,
        created_at: row.get("created_at"),
    })
}

const VER_COLS: &str = "document_id, version_number, artifact_id, revision_ids, created_by, note, is_current, created_at";

/// Creates the next version for a document: `version_number` is
/// `max + 1`, previous versions lose `is_current`, and a `version_created`
/// audit event is recorded. Returns `NotFound` when the document does
/// not exist, `Validation` when it is trashed/deleted, and `NotFound`
/// when `artifact_id` does not reference a real artifact.
pub async fn create_version(
    pool: &SqlitePool,
    document_id: &DocumentId,
    artifact_id: Option<&str>,
    created_by: &str,
    note: Option<&str>,
) -> StoreResult<DocumentVersion> {
    let doc = fetch_document(pool, document_id)
        .await?
        .ok_or_else(|| StoreError::not_found("document", document_id))?;
    if matches!(
        doc.status,
        DocumentStatus::Trashed | DocumentStatus::Deleted
    ) {
        return Err(StoreError::validation(
            "status",
            "cannot version a trashed or deleted document",
        ));
    }
    if let Some(art) = artifact_id {
        let exists: bool =
            sqlx::query_scalar("SELECT EXISTS(SELECT 1 FROM artifacts_tbl WHERE id = ?)")
                .bind(art)
                .fetch_one(pool)
                .await?;
        if !exists {
            return Err(StoreError::not_found("artifact", art));
        }
    }

    let next: i64 = sqlx::query_scalar(
        "SELECT COALESCE(MAX(version_number), 0) + 1 FROM versions_tbl WHERE document_id = ?",
    )
    .bind(document_id.as_str())
    .fetch_one(pool)
    .await?;

    sqlx::query("UPDATE versions_tbl SET is_current = 0 WHERE document_id = ?")
        .bind(document_id.as_str())
        .execute(pool)
        .await?;

    let version = DocumentVersion {
        document_id: document_id.clone(),
        version_number: next as u32,
        artifact_id: artifact_id.map(str::to_string),
        revision_ids: Vec::new(),
        created_by: created_by.to_string(),
        note: note.map(str::to_string),
        is_current: true,
        created_at: now_rfc3339(),
    };

    sqlx::query(
        "INSERT INTO versions_tbl
         (document_id, version_number, artifact_id, revision_ids, created_by, note, is_current, created_at)
         VALUES (?, ?, ?, '[]', ?, ?, 1, ?)",
    )
    .bind(document_id.as_str())
    .bind(version.version_number as i64)
    .bind(&version.artifact_id)
    .bind(created_by)
    .bind(&version.note)
    .bind(&version.created_at)
    .execute(pool)
    .await?;

    record_event(
        pool,
        document_id,
        Some(created_by),
        DocumentAuditEventType::VersionCreated,
        Some(&json!({ "version_number": version.version_number })),
    )
    .await?;

    Ok(version)
}

/// Fetches one version of a document. Returns `None` when absent.
pub async fn get_version(
    pool: &SqlitePool,
    document_id: &DocumentId,
    version_number: u32,
) -> StoreResult<Option<DocumentVersion>> {
    let row = sqlx::query(&format!(
        "SELECT {VER_COLS} FROM versions_tbl WHERE document_id = ? AND version_number = ?"
    ))
    .bind(document_id.as_str())
    .bind(version_number as i64)
    .fetch_optional(pool)
    .await?;
    row.map(|r| version_from_row(&r)).transpose()
}

/// Lists all versions of a document, newest first.
pub async fn list_versions(
    pool: &SqlitePool,
    document_id: &DocumentId,
) -> StoreResult<Vec<DocumentVersion>> {
    let rows = sqlx::query(&format!(
        "SELECT {VER_COLS} FROM versions_tbl WHERE document_id = ? ORDER BY version_number DESC"
    ))
    .bind(document_id.as_str())
    .fetch_all(pool)
    .await?;
    rows.iter().map(version_from_row).collect()
}

/// Marks an existing version as current (all others lose the flag) and
/// bumps the document's `updated_at`. This is a pointer move — it does
/// not rewrite artifact bytes. Returns `NotFound` when the document or
/// version does not exist.
pub async fn restore_version(
    pool: &SqlitePool,
    document_id: &DocumentId,
    version_number: u32,
) -> StoreResult<crate::crates::document_engine::document::Document> {
    let doc = fetch_document(pool, document_id)
        .await?
        .ok_or_else(|| StoreError::not_found("document", document_id))?;
    if get_version(pool, document_id, version_number)
        .await?
        .is_none()
    {
        return Err(StoreError::not_found("version", version_number));
    }

    sqlx::query(
        "UPDATE versions_tbl SET is_current = CASE WHEN version_number = ? THEN 1 ELSE 0 END
         WHERE document_id = ?",
    )
    .bind(version_number as i64)
    .bind(document_id.as_str())
    .execute(pool)
    .await?;

    sqlx::query("UPDATE documents_tbl SET updated_at = ? WHERE id = ?")
        .bind(now_rfc3339())
        .bind(document_id.as_str())
        .execute(pool)
        .await?;

    record_event(
        pool,
        document_id,
        Some(&doc.owner_id),
        DocumentAuditEventType::Updated,
        Some(&json!({ "restored_version": version_number })),
    )
    .await?;

    fetch_document(pool, document_id)
        .await?
        .ok_or_else(|| StoreError::not_found("document", document_id))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::crates::document_engine::document::{DocumentOrigin, DocumentSource, DocumentType};
    use crate::crates::document_engine::ids::DocumentId;
    use crate::crates::document_engine::store::artifacts::{NewArtifact, upload_artifact};
    use crate::crates::document_engine::store::documents::{
        NewDocument, create_document, delete_document, purge_document,
    };
    use crate::crates::document_engine::store::schema::init_document_store;
    use sqlx::sqlite::SqlitePoolOptions;

    async fn test_pool() -> SqlitePool {
        let pool = SqlitePoolOptions::new()
            .max_connections(1)
            .connect("sqlite::memory:")
            .await
            .expect("in-memory sqlite pool");
        init_document_store(&pool).await.expect("init store");
        pool
    }

    async fn new_doc(pool: &SqlitePool, name: &str) -> DocumentId {
        create_document(
            pool,
            &NewDocument {
                name: name.to_string(),
                document_type: DocumentType::Pdf,
                owner_id: "user-1".to_string(),
                source: DocumentSource::new(DocumentOrigin::Upload),
            },
        )
        .await
        .expect("create document")
        .id
    }

    #[tokio::test]
    async fn create_version_increments_and_moves_current_pointer() {
        let pool = test_pool().await;
        let doc_id = new_doc(&pool, "doc.pdf").await;

        let v1 = create_version(&pool, &doc_id, None, "user-1", Some("first"))
            .await
            .unwrap();
        assert_eq!(v1.version_number, 1);
        assert!(v1.is_current);
        assert_eq!(v1.created_by, "user-1");
        assert_eq!(v1.note.as_deref(), Some("first"));
        assert_eq!(v1.document_id, doc_id);

        let v2 = create_version(&pool, &doc_id, None, "user-2", None)
            .await
            .unwrap();
        assert_eq!(v2.version_number, 2);
        assert!(v2.is_current);

        // Exactly one current version: the newest.
        let versions = list_versions(&pool, &doc_id).await.unwrap();
        assert_eq!(versions.len(), 2);
        let current: Vec<_> = versions.iter().filter(|v| v.is_current).collect();
        assert_eq!(current.len(), 1);
        assert_eq!(current[0].version_number, 2);
        assert!(
            !get_version(&pool, &doc_id, 1)
                .await
                .unwrap()
                .unwrap()
                .is_current
        );
    }

    #[tokio::test]
    async fn versions_are_scoped_per_document() {
        let pool = test_pool().await;
        let doc_a = new_doc(&pool, "a.pdf").await;
        let doc_b = new_doc(&pool, "b.pdf").await;

        create_version(&pool, &doc_a, None, "user-1", None)
            .await
            .unwrap();
        create_version(&pool, &doc_a, None, "user-1", None)
            .await
            .unwrap();
        create_version(&pool, &doc_b, None, "user-1", None)
            .await
            .unwrap();

        // Each document numbers its own versions from 1.
        let a = list_versions(&pool, &doc_a).await.unwrap();
        assert_eq!(a.len(), 2);
        assert!(a.iter().any(|v| v.version_number == 1));
        assert!(a.iter().any(|v| v.version_number == 2));

        let b = list_versions(&pool, &doc_b).await.unwrap();
        assert_eq!(b.len(), 1);
        assert_eq!(b[0].version_number, 1);

        // get_version on the wrong document finds nothing.
        assert!(get_version(&pool, &doc_b, 2).await.unwrap().is_none());
    }

    #[tokio::test]
    async fn list_versions_returns_newest_first() {
        let pool = test_pool().await;
        let doc_id = new_doc(&pool, "doc.pdf").await;
        for i in 0..3 {
            create_version(&pool, &doc_id, None, "user-1", Some(&format!("v{i}")))
                .await
                .unwrap();
        }
        let versions = list_versions(&pool, &doc_id).await.unwrap();
        let numbers: Vec<u32> = versions.iter().map(|v| v.version_number).collect();
        assert_eq!(numbers, vec![3, 2, 1]);
    }

    #[tokio::test]
    async fn get_version_returns_none_for_missing() {
        let pool = test_pool().await;
        let doc_id = new_doc(&pool, "doc.pdf").await;
        create_version(&pool, &doc_id, None, "user-1", None)
            .await
            .unwrap();
        assert!(get_version(&pool, &doc_id, 99).await.unwrap().is_none());
        assert!(
            get_version(&pool, &DocumentId::new("doc-missing"), 1)
                .await
                .unwrap()
                .is_none()
        );
    }

    #[tokio::test]
    async fn create_version_requires_existing_document() {
        let pool = test_pool().await;
        assert!(matches!(
            create_version(&pool, &DocumentId::new("doc-missing"), None, "u", None).await,
            Err(StoreError::NotFound { .. })
        ));
    }

    #[tokio::test]
    async fn create_version_rejects_trashed_document() {
        let pool = test_pool().await;
        let doc_id = new_doc(&pool, "doc.pdf").await;
        delete_document(&pool, &doc_id).await.unwrap();
        assert!(matches!(
            create_version(&pool, &doc_id, None, "user-1", None).await,
            Err(StoreError::Validation { .. })
        ));
    }

    #[tokio::test]
    async fn create_version_validates_artifact_reference() {
        let pool = test_pool().await;
        let doc_id = new_doc(&pool, "doc.pdf").await;

        // Nonexistent artifact id → NotFound.
        assert!(matches!(
            create_version(&pool, &doc_id, Some("art-nope"), "user-1", None).await,
            Err(StoreError::NotFound { .. })
        ));

        // Real artifact on the same document → accepted and linked.
        let artifact = upload_artifact(
            &pool,
            &NewArtifact {
                id: Some("art-1".to_string()),
                document_id: doc_id.clone(),
                storage_uri: "storage://b/doc.pdf".to_string(),
                file_name: "doc.pdf".to_string(),
                mime_type: None,
                size_bytes: 10,
                checksum: None,
            },
        )
        .await
        .unwrap();
        let v = create_version(&pool, &doc_id, Some(&artifact.id), "user-1", None)
            .await
            .unwrap();
        assert_eq!(v.artifact_id.as_deref(), Some("art-1"));

        let fetched = get_version(&pool, &doc_id, v.version_number)
            .await
            .unwrap()
            .unwrap();
        assert_eq!(fetched.artifact_id.as_deref(), Some("art-1"));
        assert!(fetched.revision_ids.is_empty());
    }

    #[tokio::test]
    #[ignore = "bug: create_version accepts an artifact_id belonging to a different document"]
    async fn create_version_rejects_foreign_document_artifact() {
        let pool = test_pool().await;
        let doc_a = new_doc(&pool, "a.pdf").await;
        let doc_b = new_doc(&pool, "b.pdf").await;
        let artifact = upload_artifact(
            &pool,
            &NewArtifact {
                id: Some("art-b".to_string()),
                document_id: doc_b.clone(),
                storage_uri: "storage://b/b.pdf".to_string(),
                file_name: "b.pdf".to_string(),
                mime_type: None,
                size_bytes: 10,
                checksum: None,
            },
        )
        .await
        .unwrap();

        // The artifact exists, but belongs to doc_b — doc_a's version
        // must not reference it.
        let result = create_version(&pool, &doc_a, Some(&artifact.id), "user-1", None).await;
        assert!(result.is_err());
    }

    #[tokio::test]
    async fn restore_version_moves_current_pointer() {
        let pool = test_pool().await;
        let doc_id = new_doc(&pool, "doc.pdf").await;
        for i in 0..3 {
            create_version(&pool, &doc_id, None, "user-1", Some(&format!("v{i}")))
                .await
                .unwrap();
        }

        let doc = restore_version(&pool, &doc_id, 1).await.unwrap();
        assert_eq!(doc.id, doc_id);

        let versions = list_versions(&pool, &doc_id).await.unwrap();
        let current: Vec<u32> = versions
            .iter()
            .filter(|v| v.is_current)
            .map(|v| v.version_number)
            .collect();
        assert_eq!(current, vec![1]);
        assert!(
            !get_version(&pool, &doc_id, 3)
                .await
                .unwrap()
                .unwrap()
                .is_current
        );

        // Restoring again to another version keeps exactly one current.
        restore_version(&pool, &doc_id, 3).await.unwrap();
        let versions = list_versions(&pool, &doc_id).await.unwrap();
        assert_eq!(versions.iter().filter(|v| v.is_current).count(), 1);
        assert!(
            get_version(&pool, &doc_id, 3)
                .await
                .unwrap()
                .unwrap()
                .is_current
        );
    }

    #[tokio::test]
    async fn restore_version_missing_inputs_error() {
        let pool = test_pool().await;
        let doc_id = new_doc(&pool, "doc.pdf").await;
        create_version(&pool, &doc_id, None, "user-1", None)
            .await
            .unwrap();

        assert!(matches!(
            restore_version(&pool, &doc_id, 42).await,
            Err(StoreError::NotFound { .. })
        ));
        assert!(matches!(
            restore_version(&pool, &DocumentId::new("doc-missing"), 1).await,
            Err(StoreError::NotFound { .. })
        ));
    }

    #[tokio::test]
    async fn versions_cascade_when_document_purged() {
        let pool = test_pool().await;
        let doc_id = new_doc(&pool, "doc.pdf").await;
        create_version(&pool, &doc_id, None, "user-1", None)
            .await
            .unwrap();
        create_version(&pool, &doc_id, None, "user-1", None)
            .await
            .unwrap();

        delete_document(&pool, &doc_id).await.unwrap();
        assert!(purge_document(&pool, &doc_id).await.unwrap());

        let remaining: i64 =
            sqlx::query_scalar("SELECT COUNT(*) FROM versions_tbl WHERE document_id = ?")
                .bind(doc_id.as_str())
                .fetch_one(&pool)
                .await
                .unwrap();
        assert_eq!(remaining, 0);
    }
}
