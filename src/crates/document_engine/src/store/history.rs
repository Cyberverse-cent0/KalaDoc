//! Document audit history — append-only event log per document.

use serde_json::Value;
use sqlx::{Row, SqlitePool};

use crate::crates::document_engine::audit::{DocumentAuditEvent, DocumentAuditEventType};
use crate::crates::document_engine::ids::DocumentId;

use super::{StoreResult, enum_parse, enum_str, gen_id, now_rfc3339};

/// Appends an audit event for a document. Called internally by the
/// mutating operations in the sibling store modules; public so external
/// services (share flows, import pipelines, ...) can record events such
/// as [`DocumentAuditEventType::Shared`] or [`DocumentAuditEventType::Imported`].
/// The `document_audit_tbl` foreign key rejects events for unknown
/// documents.
pub async fn record_event(
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

#[cfg(test)]
mod tests {
    use super::*;
    use crate::crates::document_engine::audit::DocumentAuditEventType;
    use crate::crates::document_engine::document::{
        Document, DocumentName, DocumentOrigin, DocumentSource, DocumentStatus, DocumentType,
    };
    use crate::crates::document_engine::store::StoreError;
    use crate::crates::document_engine::store::documents::{
        NewDocument, create_document, register_document,
    };
    use crate::crates::document_engine::store::schema::init_document_store;
    use serde_json::json;
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

    /// Registers a document without going through `create_document`, so
    /// no `created` audit event is recorded.
    async fn register_silent_doc(pool: &SqlitePool, id: &str) -> DocumentId {
        let doc = Document {
            id: DocumentId::new(id),
            name: DocumentName::new("silent.pdf").expect("name"),
            document_type: DocumentType::Pdf,
            status: DocumentStatus::Active,
            source: DocumentSource::new(DocumentOrigin::Import),
            folder_id: None,
            owner_id: "user-1".to_string(),
            created_at: "2024-01-01T00:00:00Z".to_string(),
            updated_at: "2024-01-01T00:00:00Z".to_string(),
        };
        register_document(pool, &doc).await.expect("register");
        doc.id
    }

    #[tokio::test]
    async fn record_event_appends_and_history_returns_all_events() {
        let pool = test_pool().await;
        let doc_id = register_silent_doc(&pool, "doc-silent").await;

        // No events yet for a silently-registered document.
        assert!(
            get_document_history(&pool, &doc_id)
                .await
                .expect("history")
                .is_empty()
        );

        record_event(
            &pool,
            &doc_id,
            Some("user-1"),
            DocumentAuditEventType::Created,
            Some(&json!({ "name": "silent.pdf" })),
        )
        .await
        .expect("record created");
        record_event(&pool, &doc_id, None, DocumentAuditEventType::Accessed, None)
            .await
            .expect("record accessed");
        record_event(
            &pool,
            &doc_id,
            Some("user-2"),
            DocumentAuditEventType::Shared,
            Some(&json!({ "target": "group-eng" })),
        )
        .await
        .expect("record shared");

        let events = get_document_history(&pool, &doc_id).await.expect("history");
        assert_eq!(events.len(), 3);

        // Event ids are unique and carry the `evt-` prefix.
        let ids: Vec<&str> = events.iter().map(|e| e.id.as_str()).collect();
        assert!(ids.iter().all(|id| id.starts_with("evt-")));
        let unique: std::collections::HashSet<&&str> = ids.iter().collect();
        assert_eq!(unique.len(), ids.len());

        // All events belong to the document; details round-trip as JSON.
        for e in &events {
            assert_eq!(e.document_id, doc_id);
            assert!(!e.occurred_at.is_empty());
        }
        let by_type = |t: DocumentAuditEventType| {
            events
                .iter()
                .find(|e| e.event_type == t)
                .unwrap_or_else(|| panic!("missing event {t:?}"))
        };
        assert_eq!(
            by_type(DocumentAuditEventType::Created).actor_id.as_deref(),
            Some("user-1")
        );
        assert_eq!(
            by_type(DocumentAuditEventType::Created).details,
            Some(json!({ "name": "silent.pdf" }))
        );
        assert_eq!(by_type(DocumentAuditEventType::Accessed).details, None);
        assert_eq!(by_type(DocumentAuditEventType::Accessed).actor_id, None);
        assert_eq!(
            by_type(DocumentAuditEventType::Shared).details,
            Some(json!({ "target": "group-eng" }))
        );
    }

    #[tokio::test]
    async fn history_is_ordered_by_occurred_at() {
        let pool = test_pool().await;
        let doc_id = register_silent_doc(&pool, "doc-ordered").await;

        // Insert rows directly with controlled timestamps — `record_event`
        // only stamps "now", which has one-second granularity and cannot
        // produce a deterministic multi-event ordering.
        for (id, ts, etype) in [
            ("evt-1", "2024-01-01T00:00:03Z", "shared"),
            ("evt-2", "2024-01-01T00:00:01Z", "created"),
            ("evt-3", "2024-01-01T00:00:02Z", "accessed"),
        ] {
            sqlx::query(
                "INSERT INTO document_audit_tbl
                 (id, document_id, actor_id, event_type, occurred_at, details)
                 VALUES (?, ?, NULL, ?, ?, NULL)",
            )
            .bind(id)
            .bind(doc_id.as_str())
            .bind(etype)
            .bind(ts)
            .execute(&pool)
            .await
            .expect("insert audit row");
        }

        let events = get_document_history(&pool, &doc_id).await.expect("history");
        let order: Vec<&str> = events.iter().map(|e| e.id.as_str()).collect();
        assert_eq!(order, ["evt-2", "evt-3", "evt-1"]);
        assert_eq!(events[0].event_type, DocumentAuditEventType::Created);
        assert_eq!(events[1].event_type, DocumentAuditEventType::Accessed);
        assert_eq!(events[2].event_type, DocumentAuditEventType::Shared);
    }

    #[tokio::test]
    async fn record_event_rejects_unknown_document() {
        let pool = test_pool().await;
        let missing = DocumentId::new("doc-missing");
        let err = record_event(&pool, &missing, None, DocumentAuditEventType::Created, None)
            .await
            .expect_err("event for unknown document must fail");
        // The document_audit_tbl FK rejects the insert.
        assert!(matches!(err, StoreError::Sqlx(_)));
    }

    #[tokio::test]
    async fn create_document_records_created_event() {
        let pool = test_pool().await;
        let doc_id = new_doc(&pool, "report.pdf").await;

        let events = get_document_history(&pool, &doc_id).await.expect("history");
        assert_eq!(events.len(), 1);
        assert_eq!(events[0].event_type, DocumentAuditEventType::Created);
        assert_eq!(events[0].actor_id.as_deref(), Some("user-1"));
    }
}
