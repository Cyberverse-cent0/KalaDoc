//! Document permission reads.
//!
//! These functions only *store and return* permission records. Actual
//! authorization decisions are evaluated by the central authorization
//! service — the Document Engine must not make allow/deny decisions
//! independently.

use sqlx::{Row, SqlitePool};

use crate::crates::document_engine::ids::DocumentId;
use crate::crates::document_engine::lifecycle::DocumentPermission;

use super::{StoreResult, enum_parse};

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
