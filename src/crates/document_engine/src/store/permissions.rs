//! Document permission reads.
//!
//! These functions only *store and return* permission records. Actual
//! authorization decisions are evaluated by the central authorization
//! service — the Document Engine must not make allow/deny decisions
//! independently.

use serde_json::json;
use sqlx::{Row, SqlitePool};

use crate::crates::document_engine::audit::DocumentAuditEventType;
use crate::crates::document_engine::ids::DocumentId;
use crate::crates::document_engine::lifecycle::{DocumentAccessLevel, DocumentPermission};

use super::documents::fetch_document;
use super::history::record_event;
use super::{StoreError, StoreResult, enum_parse, enum_str, now_rfc3339};

/// Lists all permission grants recorded for a document.
pub async fn get_document_permissions(
    pool: &SqlitePool,
    document_id: &DocumentId,
) -> StoreResult<Vec<DocumentPermission>> {
    let rows = sqlx::query(
        "SELECT document_id, subject_id, access_level, granted_by, granted_at
         FROM document_permissions_tbl WHERE document_id = ?
         ORDER BY subject_id",
    )
    .bind(document_id.as_str())
    .fetch_all(pool)
    .await?;

    rows.iter()
        .map(|row| {
            Ok(DocumentPermission {
                document_id: DocumentId::new(row.get::<String, _>("document_id")),
                subject_id: row.get("subject_id"),
                access_level: enum_parse(&row.get::<String, _>("access_level"))?,
                granted_by: row.get("granted_by"),
                granted_at: row.get("granted_at"),
            })
        })
        .collect()
}

/// Grants (or updates) a permission for `subject_id` on a document.
/// Re-granting an existing subject upserts the access level. Records a
/// `permission_changed` audit event. Returns `NotFound` when the
/// document does not exist.
pub async fn grant_permission(
    pool: &SqlitePool,
    document_id: &DocumentId,
    subject_id: &str,
    access_level: DocumentAccessLevel,
    granted_by: Option<&str>,
) -> StoreResult<DocumentPermission> {
    if subject_id.trim().is_empty() {
        return Err(StoreError::validation("subject_id", "must not be empty"));
    }
    fetch_document(pool, document_id)
        .await?
        .ok_or_else(|| StoreError::not_found("document", document_id))?;

    let permission = DocumentPermission {
        document_id: document_id.clone(),
        subject_id: subject_id.to_string(),
        access_level,
        granted_by: granted_by.map(str::to_string),
        granted_at: now_rfc3339(),
    };

    sqlx::query(
        "INSERT INTO document_permissions_tbl
         (document_id, subject_id, access_level, granted_by, granted_at)
         VALUES (?, ?, ?, ?, ?)
         ON CONFLICT(document_id, subject_id) DO UPDATE SET
             access_level = excluded.access_level,
             granted_by    = excluded.granted_by,
             granted_at    = excluded.granted_at",
    )
    .bind(document_id.as_str())
    .bind(subject_id)
    .bind(enum_str(&access_level)?)
    .bind(granted_by)
    .bind(&permission.granted_at)
    .execute(pool)
    .await?;

    record_event(
        pool,
        document_id,
        granted_by,
        DocumentAuditEventType::PermissionChanged,
        Some(&json!({
            "subject_id": subject_id,
            "access_level": enum_str(&access_level)?,
            "granted": true,
        })),
    )
    .await?;

    Ok(permission)
}

/// Revokes a subject's permission on a document. Returns `true` when a
/// grant was actually removed; records a `permission_changed` audit
/// event only in that case.
pub async fn revoke_permission(
    pool: &SqlitePool,
    document_id: &DocumentId,
    subject_id: &str,
    revoked_by: Option<&str>,
) -> StoreResult<bool> {
    let done = sqlx::query(
        "DELETE FROM document_permissions_tbl WHERE document_id = ? AND subject_id = ?",
    )
    .bind(document_id.as_str())
    .bind(subject_id)
    .execute(pool)
    .await?;

    if done.rows_affected() > 0 {
        record_event(
            pool,
            document_id,
            revoked_by,
            DocumentAuditEventType::PermissionChanged,
            Some(&json!({
                "subject_id": subject_id,
                "granted": false,
            })),
        )
        .await?;
    }
    Ok(done.rows_affected() > 0)
}

/// Lists every permission grant held by a subject across all documents —
/// the "what can this user/group/role access" query. Backed by the
/// `idx_doc_permissions_subject` index.
pub async fn list_permissions_for_subject(
    pool: &SqlitePool,
    subject_id: &str,
) -> StoreResult<Vec<DocumentPermission>> {
    let rows = sqlx::query(
        "SELECT document_id, subject_id, access_level, granted_by, granted_at
         FROM document_permissions_tbl WHERE subject_id = ?
         ORDER BY document_id",
    )
    .bind(subject_id)
    .fetch_all(pool)
    .await?;

    rows.iter()
        .map(|row| {
            Ok(DocumentPermission {
                document_id: DocumentId::new(row.get::<String, _>("document_id")),
                subject_id: row.get("subject_id"),
                access_level: enum_parse(&row.get::<String, _>("access_level"))?,
                granted_by: row.get("granted_by"),
                granted_at: row.get("granted_at"),
            })
        })
        .collect()
}
