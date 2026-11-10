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

#[cfg(test)]
mod tests {
    use super::*;
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

    #[tokio::test]
    async fn grant_and_list_document_permissions() {
        let pool = test_pool().await;
        let doc_id = new_doc(&pool, "report.pdf").await;

        let p1 = grant_permission(
            &pool,
            &doc_id,
            "user-2",
            DocumentAccessLevel::Read,
            Some("user-1"),
        )
        .await
        .expect("grant read");
        assert_eq!(p1.document_id, doc_id);
        assert_eq!(p1.subject_id, "user-2");
        assert_eq!(p1.access_level, DocumentAccessLevel::Read);
        assert_eq!(p1.granted_by.as_deref(), Some("user-1"));
        assert!(!p1.granted_at.is_empty());

        grant_permission(
            &pool,
            &doc_id,
            "group-eng",
            DocumentAccessLevel::Manage,
            None,
        )
        .await
        .expect("grant manage");

        // Ordered by subject_id: "group-eng" < "user-2".
        let perms = get_document_permissions(&pool, &doc_id)
            .await
            .expect("list");
        assert_eq!(perms.len(), 2);
        assert_eq!(perms[0].subject_id, "group-eng");
        assert_eq!(perms[0].access_level, DocumentAccessLevel::Manage);
        assert_eq!(perms[0].granted_by, None);
        assert_eq!(perms[1].subject_id, "user-2");
        assert_eq!(perms[1].access_level, DocumentAccessLevel::Read);
    }

    #[tokio::test]
    async fn regrant_upserts_access_level() {
        let pool = test_pool().await;
        let doc_id = new_doc(&pool, "report.pdf").await;

        grant_permission(&pool, &doc_id, "user-2", DocumentAccessLevel::Read, None)
            .await
            .expect("grant read");
        grant_permission(&pool, &doc_id, "user-2", DocumentAccessLevel::Write, None)
            .await
            .expect("upgrade to write");

        let perms = get_document_permissions(&pool, &doc_id)
            .await
            .expect("list");
        assert_eq!(perms.len(), 1);
        assert_eq!(perms[0].access_level, DocumentAccessLevel::Write);
    }

    #[tokio::test]
    async fn revoke_permission_removes_grant_once() {
        let pool = test_pool().await;
        let doc_id = new_doc(&pool, "report.pdf").await;
        grant_permission(&pool, &doc_id, "user-2", DocumentAccessLevel::Read, None)
            .await
            .expect("grant");

        assert!(
            revoke_permission(&pool, &doc_id, "user-2", Some("user-1"))
                .await
                .expect("revoke")
        );
        assert!(
            get_document_permissions(&pool, &doc_id)
                .await
                .expect("list")
                .is_empty()
        );
        // Second revoke finds nothing and records no extra event.
        assert!(
            !revoke_permission(&pool, &doc_id, "user-2", None)
                .await
                .expect("revoke again")
        );
    }

    #[tokio::test]
    async fn list_permissions_for_subject_spans_documents() {
        let pool = test_pool().await;
        let doc_a = new_doc(&pool, "a.pdf").await;
        let doc_b = new_doc(&pool, "b.pdf").await;
        let doc_c = new_doc(&pool, "c.pdf").await;

        grant_permission(&pool, &doc_a, "user-9", DocumentAccessLevel::Read, None)
            .await
            .expect("grant a");
        grant_permission(&pool, &doc_b, "user-9", DocumentAccessLevel::Write, None)
            .await
            .expect("grant b");
        grant_permission(&pool, &doc_b, "user-8", DocumentAccessLevel::Manage, None)
            .await
            .expect("grant b other subject");
        grant_permission(&pool, &doc_c, "user-9", DocumentAccessLevel::Read, None)
            .await
            .expect("grant c");

        let perms = list_permissions_for_subject(&pool, "user-9")
            .await
            .expect("subject perms");
        assert_eq!(perms.len(), 3);
        // Ordered by document_id.
        let doc_ids: Vec<String> = perms
            .iter()
            .map(|p| p.document_id.as_str().to_string())
            .collect();
        let mut sorted = doc_ids.clone();
        sorted.sort();
        assert_eq!(doc_ids, sorted);
        assert!(perms.iter().all(|p| p.subject_id == "user-9"));

        // Unknown subject yields an empty list.
        assert!(
            list_permissions_for_subject(&pool, "nobody")
                .await
                .expect("empty")
                .is_empty()
        );
    }

    #[tokio::test]
    async fn grant_permission_validates_input_and_document() {
        let pool = test_pool().await;
        let doc_id = new_doc(&pool, "report.pdf").await;

        let err = grant_permission(&pool, &doc_id, "   ", DocumentAccessLevel::Read, None)
            .await
            .expect_err("blank subject must fail");
        assert!(matches!(err, StoreError::Validation { .. }));

        let err = grant_permission(
            &pool,
            &DocumentId::new("doc-missing"),
            "user-2",
            DocumentAccessLevel::Read,
            None,
        )
        .await
        .expect_err("missing document must fail");
        assert!(matches!(err, StoreError::NotFound { .. }));
    }

    #[tokio::test]
    async fn permission_changes_record_audit_events() {
        let pool = test_pool().await;
        let doc_id = new_doc(&pool, "report.pdf").await;

        grant_permission(
            &pool,
            &doc_id,
            "user-2",
            DocumentAccessLevel::Read,
            Some("user-1"),
        )
        .await
        .expect("grant");
        // Re-grant also records an event (it is still a change of record).
        grant_permission(&pool, &doc_id, "user-2", DocumentAccessLevel::Write, None)
            .await
            .expect("re-grant");
        revoke_permission(&pool, &doc_id, "user-2", Some("user-1"))
            .await
            .expect("revoke");
        // Revoking a non-existent grant records nothing.
        revoke_permission(&pool, &doc_id, "user-2", None)
            .await
            .expect("noop revoke");

        let events = get_document_history(&pool, &doc_id).await.expect("history");
        let permission_events: Vec<_> = events
            .iter()
            .filter(|e| {
                e.event_type
                    == crate::crates::document_engine::audit::DocumentAuditEventType::PermissionChanged
            })
            .collect();
        assert_eq!(permission_events.len(), 3);

        // Same-second events tie-break on random event id, so assert on
        // the multiset of `granted` flags rather than positions.
        let mut granted_flags: Vec<bool> = permission_events
            .iter()
            .filter_map(|e| {
                e.details
                    .as_ref()
                    .and_then(|d| d.get("granted"))
                    .and_then(serde_json::Value::as_bool)
            })
            .collect();
        granted_flags.sort();
        assert_eq!(granted_flags, [false, true, true]);
        // Grant events carry the subject and access level.
        let grant_event = permission_events
            .iter()
            .find(|e| {
                e.details
                    .as_ref()
                    .and_then(|d| d.get("access_level"))
                    .and_then(serde_json::Value::as_str)
                    == Some("read")
            })
            .expect("a grant event with access_level=read");
        assert_eq!(
            grant_event
                .details
                .as_ref()
                .and_then(|d| d.get("subject_id")),
            Some(&serde_json::json!("user-2"))
        );
    }

    #[tokio::test]
    async fn permissions_cascade_when_document_is_purged() {
        let pool = test_pool().await;
        let doc_id = new_doc(&pool, "report.pdf").await;
        grant_permission(&pool, &doc_id, "user-2", DocumentAccessLevel::Read, None)
            .await
            .expect("grant");

        delete_document(&pool, &doc_id).await.expect("trash");
        purge_document(&pool, &doc_id).await.expect("purge");

        let remaining: i64 = sqlx::query_scalar(
            "SELECT COUNT(*) FROM document_permissions_tbl WHERE document_id = ?",
        )
        .bind(doc_id.as_str())
        .fetch_one(&pool)
        .await
        .expect("count");
        assert_eq!(remaining, 0);
        assert!(
            get_document_permissions(&pool, &doc_id)
                .await
                .expect("list")
                .is_empty()
        );
    }
}
