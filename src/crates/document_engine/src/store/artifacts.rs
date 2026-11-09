//! Document artifact operations — metadata for physical files.
//!
//! These functions manage the `artifacts_tbl` rows only. Reading and
//! writing the actual bytes is the storage engine's job; `storage_uri`
//! is the hand-off point.

use sqlx::{Row, SqlitePool, sqlite::SqliteRow};

use crate::crates::document_engine::artifact::{DocumentArtifact, DocumentChecksum};
use crate::crates::document_engine::ids::DocumentId;

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
pub async fn upload_artifact(pool: &SqlitePool, new: &NewArtifact) -> StoreResult<DocumentArtifact> {
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
/// `None` when no such artifact exists.
pub async fn download_artifact(
    pool: &SqlitePool,
    artifact_id: &str,
) -> StoreResult<Option<DocumentArtifact>> {
    let row = sqlx::query(&format!("SELECT {ART_COLS} FROM artifacts_tbl WHERE id = ?"))
        .bind(artifact_id)
        .fetch_optional(pool)
        .await?;
    row.map(|r| artifact_from_row(&r)).transpose()
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

