//! Document artifact operations — metadata for physical files.
//!
//! These functions manage the `artifacts_tbl` rows only. Reading and
//! writing the actual bytes is the storage engine's job; `storage_uri`
//! is the hand-off point.

use serde_json::json;
use sqlx::{Row, SqlitePool, sqlite::SqliteRow};

use crate::crates::document_engine::artifact::{DocumentArtifact, DocumentChecksum};
use crate::crates::document_engine::audit::DocumentAuditEventType;
use crate::crates::document_engine::ids::DocumentId;

use super::history::record_event;
use super::{StoreError, StoreResult, enum_parse, enum_str, gen_id, now_rfc3339};

/// Input for [`upload_artifact`]. `id` is generated when empty.
#[derive(Debug, Clone)]
pub struct NewArtifact {
    /// Optional caller-supplied id; generated via [`gen_id`] when `None`.
    pub id: Option<String>,
    pub document_id: DocumentId,
    pub storage_uri: String,
    pub file_name: String,
    pub mime_type: Option<String>,
    pub size_bytes: u64,
    pub checksum: Option<DocumentChecksum>,
}

fn artifact_from_row(row: &SqliteRow) -> StoreResult<DocumentArtifact> {
    let algo: Option<String> = row.get("checksum_algorithm");
    let value: Option<String> = row.get("checksum_value");
    Ok(DocumentArtifact {
        id: row.get("id"),
        document_id: DocumentId::new(row.get::<String, _>("document_id")),
        storage_uri: row.get("storage_uri"),
        file_name: row.get("file_name"),
        mime_type: row.get("mime_type"),
        size_bytes: row.get::<i64, _>("size_bytes") as u64,
        checksum: match (algo, value) {
            (Some(a), Some(v)) => Some(DocumentChecksum {
                algorithm: enum_parse(&a)?,
                value: v,
            }),
            _ => None,
        },
        created_at: row.get("created_at"),
    })
}

const ART_COLS: &str = "id, document_id, storage_uri, file_name, mime_type, size_bytes,
     checksum_algorithm, checksum_value, created_at";

/// Registers an artifact for a document after its bytes have been
/// written to `storage_uri` by the storage layer. Returns
/// `NotFound` when the document does not exist.
pub async fn upload_artifact(
    pool: &SqlitePool,
    new: &NewArtifact,
) -> StoreResult<DocumentArtifact> {
    let exists: bool =
        sqlx::query_scalar("SELECT EXISTS(SELECT 1 FROM documents_tbl WHERE id = ?)")
            .bind(new.document_id.as_str())
            .fetch_one(pool)
            .await?;
    if !exists {
        return Err(StoreError::not_found("document", &new.document_id));
    }

    let artifact = DocumentArtifact {
        id: new.id.clone().unwrap_or_else(|| gen_id("art")),
        document_id: new.document_id.clone(),
        storage_uri: new.storage_uri.clone(),
        file_name: new.file_name.clone(),
        mime_type: new.mime_type.clone(),
        size_bytes: new.size_bytes,
        checksum: new.checksum.clone(),
        created_at: now_rfc3339(),
    };

    sqlx::query(
        "INSERT INTO artifacts_tbl
         (id, document_id, storage_uri, file_name, mime_type, size_bytes,
          checksum_algorithm, checksum_value, created_at)
         VALUES (?, ?, ?, ?, ?, ?, ?, ?, ?)",
    )
    .bind(&artifact.id)
    .bind(artifact.document_id.as_str())
    .bind(&artifact.storage_uri)
    .bind(&artifact.file_name)
    .bind(&artifact.mime_type)
    .bind(artifact.size_bytes as i64)
    .bind(
        artifact
            .checksum
            .as_ref()
            .map(|c| enum_str(&c.algorithm))
            .transpose()?,
    )
    .bind(artifact.checksum.as_ref().map(|c| c.value.clone()))
    .bind(&artifact.created_at)
    .execute(pool)
    .await?;

    Ok(artifact)
}

/// Fetches artifact metadata by id — this is what a "download" operation
/// resolves before handing `storage_uri` to the storage engine. Returns
/// `None` when no such artifact exists. Each successful resolution
/// records an `accessed` audit event on the owning document.
pub async fn download_artifact(
    pool: &SqlitePool,
    artifact_id: &str,
) -> StoreResult<Option<DocumentArtifact>> {
    let row = sqlx::query(&format!(
        "SELECT {ART_COLS} FROM artifacts_tbl WHERE id = ?"
    ))
    .bind(artifact_id)
    .fetch_optional(pool)
    .await?;
    let artifact = row.map(|r| artifact_from_row(&r)).transpose()?;
    if let Some(art) = &artifact {
        record_event(
            pool,
            &art.document_id,
            None,
            DocumentAuditEventType::Accessed,
            Some(&json!({ "artifact_id": art.id })),
        )
        .await?;
    }
    Ok(artifact)
}

/// Lists all artifacts belonging to a document, oldest first.
pub async fn list_artifacts(
    pool: &SqlitePool,
    document_id: &DocumentId,
) -> StoreResult<Vec<DocumentArtifact>> {
    let rows = sqlx::query(&format!(
        "SELECT {ART_COLS} FROM artifacts_tbl WHERE document_id = ? ORDER BY created_at"
    ))
    .bind(document_id.as_str())
    .fetch_all(pool)
    .await?;
    rows.iter().map(artifact_from_row).collect()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::crates::document_engine::artifact::ChecksumAlgorithm;
    use crate::crates::document_engine::audit::DocumentAuditEventType;
    use crate::crates::document_engine::document::{DocumentOrigin, DocumentSource, DocumentType};
    use crate::crates::document_engine::store::documents::{
        NewDocument, create_document, delete_document, purge_document,
    };
    use crate::crates::document_engine::store::history::get_document_history;
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

    fn sample_artifact(document_id: &DocumentId, file_name: &str) -> NewArtifact {
        NewArtifact {
            id: None,
            document_id: document_id.clone(),
            storage_uri: format!("file:///store/{file_name}"),
            file_name: file_name.to_string(),
            mime_type: Some("application/pdf".to_string()),
            size_bytes: 4096,
            checksum: Some(DocumentChecksum {
                algorithm: ChecksumAlgorithm::Sha256,
                value: "ab".repeat(32),
            }),
        }
    }

    #[tokio::test]
    async fn upload_list_and_download_artifact() {
        let pool = test_pool().await;
        let doc_id = new_doc(&pool, "report.pdf").await;

        let a1 = upload_artifact(&pool, &sample_artifact(&doc_id, "orig.pdf"))
            .await
            .expect("upload first artifact");
        let a2 = upload_artifact(
            &pool,
            &NewArtifact {
                id: Some("art-fixed".to_string()),
                mime_type: None,
                size_bytes: 0,
                checksum: None,
                ..sample_artifact(&doc_id, "thumb.png")
            },
        )
        .await
        .expect("upload second artifact");

        // Generated ids carry the `art-` prefix; caller-supplied ids win.
        assert!(a1.id.starts_with("art-"));
        assert_eq!(a1.document_id, doc_id);
        assert_eq!(a1.storage_uri, "file:///store/orig.pdf");
        assert_eq!(a1.file_name, "orig.pdf");
        assert_eq!(a1.mime_type.as_deref(), Some("application/pdf"));
        assert_eq!(a1.size_bytes, 4096);
        let checksum = a1.checksum.clone().expect("checksum present");
        assert_eq!(checksum.algorithm, ChecksumAlgorithm::Sha256);
        assert_eq!(checksum.value, "ab".repeat(32));
        assert!(!a1.created_at.is_empty());

        assert_eq!(a2.id, "art-fixed");
        assert!(a2.checksum.is_none());
        assert!(a2.mime_type.is_none());

        let listed = list_artifacts(&pool, &doc_id)
            .await
            .expect("list artifacts");
        assert_eq!(listed.len(), 2);
        let ids: Vec<&str> = listed.iter().map(|a| a.id.as_str()).collect();
        assert!(ids.contains(&a1.id.as_str()));
        assert!(ids.contains(&"art-fixed"));

        let fetched = download_artifact(&pool, &a1.id)
            .await
            .expect("download")
            .expect("artifact exists");
        assert_eq!(fetched, a1);
    }

    #[tokio::test]
    async fn download_missing_artifact_returns_none() {
        let pool = test_pool().await;
        assert!(
            download_artifact(&pool, "art-does-not-exist")
                .await
                .expect("query")
                .is_none()
        );
    }

    #[tokio::test]
    async fn download_records_accessed_audit_event() {
        let pool = test_pool().await;
        let doc_id = new_doc(&pool, "report.pdf").await;
        let artifact = upload_artifact(&pool, &sample_artifact(&doc_id, "orig.pdf"))
            .await
            .expect("upload");

        download_artifact(&pool, &artifact.id)
            .await
            .expect("download");

        let events = get_document_history(&pool, &doc_id).await.expect("history");
        let accessed: Vec<_> = events
            .iter()
            .filter(|e| e.event_type == DocumentAuditEventType::Accessed)
            .collect();
        assert_eq!(accessed.len(), 1);
        assert_eq!(
            accessed[0]
                .details
                .as_ref()
                .and_then(|d| d.get("artifact_id")),
            Some(&serde_json::json!(artifact.id))
        );
    }

    #[tokio::test]
    async fn upload_artifact_rejects_missing_document() {
        let pool = test_pool().await;
        let missing = DocumentId::new("doc-missing");
        let err = upload_artifact(&pool, &sample_artifact(&missing, "x.pdf"))
            .await
            .expect_err("upload for unknown document must fail");
        assert!(matches!(err, StoreError::NotFound { .. }));
    }

    #[tokio::test]
    async fn orphan_artifact_row_violates_foreign_key() {
        let pool = test_pool().await;
        // The application-level guard in `upload_artifact` rejects unknown
        // documents; the schema-level FK must also reject raw inserts.
        let result = sqlx::query(
            "INSERT INTO artifacts_tbl
             (id, document_id, storage_uri, file_name, mime_type, size_bytes,
              checksum_algorithm, checksum_value, created_at)
             VALUES ('art-orphan', 'doc-missing', 'file:///x', 'x.pdf', NULL, 0, NULL, NULL, '2024-01-01T00:00:00Z')",
        )
        .execute(&pool)
        .await;
        assert!(result.is_err(), "orphan artifact insert should violate FK");
    }

    #[tokio::test]
    async fn artifacts_cascade_when_document_is_purged() {
        let pool = test_pool().await;
        let doc_id = new_doc(&pool, "report.pdf").await;
        upload_artifact(&pool, &sample_artifact(&doc_id, "a.pdf"))
            .await
            .expect("upload a");
        upload_artifact(&pool, &sample_artifact(&doc_id, "b.pdf"))
            .await
            .expect("upload b");

        delete_document(&pool, &doc_id).await.expect("trash");
        assert!(purge_document(&pool, &doc_id).await.expect("purge"));

        let remaining: i64 =
            sqlx::query_scalar("SELECT COUNT(*) FROM artifacts_tbl WHERE document_id = ?")
                .bind(doc_id.as_str())
                .fetch_one(&pool)
                .await
                .expect("count");
        assert_eq!(remaining, 0);
        assert!(
            list_artifacts(&pool, &doc_id)
                .await
                .expect("list")
                .is_empty()
        );
    }
}
