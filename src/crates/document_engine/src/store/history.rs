//! Document audit history — append-only event log per document.

use serde_json::Value;
use sqlx::{Row, SqlitePool};

use crate::crates::document_engine::audit::{DocumentAuditEvent, DocumentAuditEventType};
use crate::crates::document_engine::ids::DocumentId;

use super::{StoreResult, enum_parse, enum_str, gen_id, now_rfc3339};

/// Appends an audit event for a document. Called internally by the
/// mutating operations in the sibling store modules.
pub(crate) async fn record_event(
    pool: &SqlitePool,
    document_id: &DocumentId,
    actor_id: Option<&str>,
    event_type: DocumentAuditEventType,
    details: Option<&Value>,
) -> StoreResult<()> {
    sqlx::query(
        "INSERT INTO document_audit_tbl (id, document_id, actor_id, event_type, occurred_at, details)
         VALUES (?, ?, ?, ?, ?, ?)",
    )
    .bind(gen_id("evt"))
    .bind(document_id.as_str())
    .bind(actor_id)
    .bind(enum_str(&event_type)?)
    .bind(now_rfc3339())
    .bind(details.map(|d| d.to_string()))
    .execute(pool)
    .await?;
    Ok(())
}

/// Returns the full audit history for a document, oldest first.
pub async fn get_document_history(
    pool: &SqlitePool,
    document_id: &DocumentId,
) -> StoreResult<Vec<DocumentAuditEvent>> {
    let rows = sqlx::query(
        "SELECT id, document_id, actor_id, event_type, occurred_at, details
         FROM document_audit_tbl WHERE document_id = ? ORDER BY occurred_at, id",
    )
    .bind(document_id.as_str())
    .fetch_all(pool)
    .await?;

    rows.iter()
        .map(|row| {
            let details: Option<String> = row.get("details");
            Ok(DocumentAuditEvent {
                id: row.get("id"),
                document_id: DocumentId::new(row.get::<String, _>("document_id")),
                actor_id: row.get("actor_id"),
                event_type: enum_parse(&row.get::<String, _>("event_type"))?,
                occurred_at: row.get("occurred_at"),
                details: details.map(|d| serde_json::from_str(&d)).transpose()?,
            })
        })
        .collect()
}
