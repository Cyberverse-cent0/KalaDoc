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

const VER_COLS: &str =
    "document_id, version_number, artifact_id, revision_ids, created_by, note, is_current, created_at";

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
    if matches!(doc.status, DocumentStatus::Trashed | DocumentStatus::Deleted) {
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
    if get_version(pool, document_id, version_number).await?.is_none() {
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
