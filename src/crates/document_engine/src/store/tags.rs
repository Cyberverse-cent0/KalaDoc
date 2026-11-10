//! Document tag operations.

use sqlx::{Row, SqlitePool};

use crate::crates::document_engine::ids::DocumentId;
use crate::crates::document_engine::organization::{DocumentTag, TagAssignment};

use super::documents::fetch_document;
use super::{StoreError, StoreResult, gen_id, now_rfc3339};

/// Assigns a tag to a document. The tag is created on first use —
/// `add_tag` accepts a tag *name*, not an id, matching how users think
/// about tags. Re-adding an existing assignment is a no-op.
/// Returns `NotFound` when the document does not exist.
pub async fn add_tag(
    pool: &SqlitePool,
    document_id: &DocumentId,
    tag_name: &str,
    assigned_by: Option<&str>,
) -> StoreResult<TagAssignment> {
    if tag_name.trim().is_empty() {
        return Err(StoreError::validation("tag_name", "must not be empty"));
    }
    fetch_document(pool, document_id)
        .await?
        .ok_or_else(|| StoreError::not_found("document", document_id))?;

    // Upsert the tag by (case-insensitive) name.
    let tag_id: String = sqlx::query_scalar(
        "INSERT INTO tags_tbl (id, name, created_at)
         VALUES (?, ?, ?)
         ON CONFLICT(name) DO UPDATE SET name = excluded.name
         RETURNING id",
    )
    .bind(gen_id("tag"))
    .bind(tag_name.trim())
    .bind(now_rfc3339())
    .fetch_one(pool)
    .await?;

    let assignment = TagAssignment {
        document_id: document_id.clone(),
        tag_id: tag_id.clone(),
        assigned_by: assigned_by.map(str::to_string),
        assigned_at: now_rfc3339(),
    };
    sqlx::query(
        "INSERT OR IGNORE INTO tag_assignments_tbl
         (document_id, tag_id, assigned_by, assigned_at) VALUES (?, ?, ?, ?)",
    )
    .bind(document_id.as_str())
    .bind(&tag_id)
    .bind(assigned_by)
    .bind(&assignment.assigned_at)
    .execute(pool)
    .await?;
    Ok(assignment)
}

/// Removes a tag assignment by tag name. Returns `true` when an
/// assignment was actually removed.
pub async fn remove_tag(
    pool: &SqlitePool,
    document_id: &DocumentId,
    tag_name: &str,
) -> StoreResult<bool> {
    let done = sqlx::query(
        "DELETE FROM tag_assignments_tbl
         WHERE document_id = ? AND tag_id = (
             SELECT id FROM tags_tbl WHERE name = ? COLLATE NOCASE
         )",
    )
    .bind(document_id.as_str())
    .bind(tag_name)
    .execute(pool)
    .await?;
    Ok(done.rows_affected() > 0)
}

/// Lists the tags assigned to a document.
pub async fn list_tags(
    pool: &SqlitePool,
    document_id: &DocumentId,
) -> StoreResult<Vec<DocumentTag>> {
    let rows = sqlx::query(
        "SELECT t.id, t.name, t.color, t.created_at
         FROM tags_tbl t
         JOIN tag_assignments_tbl a ON a.tag_id = t.id
         WHERE a.document_id = ?
         ORDER BY t.name",
    )
    .bind(document_id.as_str())
    .fetch_all(pool)
    .await?;

    Ok(rows
        .iter()
        .map(|row| DocumentTag {
            id: row.get("id"),
            name: row.get("name"),
            color: row.get("color"),
            created_at: row.get("created_at"),
        })
        .collect())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::crates::document_engine::document::{DocumentOrigin, DocumentSource, DocumentType};
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
    async fn add_tag_creates_tag_and_lists_it() {
        let pool = test_pool().await;
        let doc_id = new_doc(&pool, "report.pdf").await;

        let assignment = add_tag(&pool, &doc_id, "  urgent  ", Some("user-1"))
            .await
            .expect("add tag");
        assert_eq!(assignment.document_id, doc_id);
        assert!(assignment.tag_id.starts_with("tag-"));
        assert_eq!(assignment.assigned_by.as_deref(), Some("user-1"));
        assert!(!assignment.assigned_at.is_empty());

        let tags = list_tags(&pool, &doc_id).await.expect("list tags");
        assert_eq!(tags.len(), 1);
        assert_eq!(tags[0].id, assignment.tag_id);
        // The stored tag name is the trimmed input.
        assert_eq!(tags[0].name, "urgent");
        assert_eq!(tags[0].color, None);
    }

    #[tokio::test]
    async fn add_tag_reuses_tag_case_insensitively() {
        let pool = test_pool().await;
        let doc_a = new_doc(&pool, "a.pdf").await;
        let doc_b = new_doc(&pool, "b.pdf").await;

        let first = add_tag(&pool, &doc_a, "Report", None)
            .await
            .expect("add Report");
        // Same name with different casing resolves to the same tag row.
        let second = add_tag(&pool, &doc_b, "report", None)
            .await
            .expect("add report");
        assert_eq!(first.tag_id, second.tag_id);

        // Re-adding to the same document is a no-op: still one assignment.
        add_tag(&pool, &doc_a, "REPORT", Some("user-2"))
            .await
            .expect("re-add");
        let rows: i64 =
            sqlx::query_scalar("SELECT COUNT(*) FROM tag_assignments_tbl WHERE document_id = ?")
                .bind(doc_a.as_str())
                .fetch_one(&pool)
                .await
                .expect("count");
        assert_eq!(rows, 1);

        let tags_a = list_tags(&pool, &doc_a).await.expect("tags a");
        assert_eq!(tags_a.len(), 1);
        // The tag is shared across documents.
        let tags_b = list_tags(&pool, &doc_b).await.expect("tags b");
        assert_eq!(tags_b.len(), 1);
        assert_eq!(tags_a[0].id, tags_b[0].id);
    }

    #[tokio::test]
    async fn add_tag_validates_input_and_document() {
        let pool = test_pool().await;
        let doc_id = new_doc(&pool, "report.pdf").await;

        for bad in ["", "   "] {
            let err = add_tag(&pool, &doc_id, bad, None)
                .await
                .expect_err("blank tag name must fail");
            assert!(matches!(err, StoreError::Validation { .. }));
        }

        let err = add_tag(&pool, &DocumentId::new("doc-missing"), "x", None)
            .await
            .expect_err("missing document must fail");
        assert!(matches!(err, StoreError::NotFound { .. }));
    }

    #[tokio::test]
    async fn remove_tag_is_case_insensitive_and_idempotent() {
        let pool = test_pool().await;
        let doc_id = new_doc(&pool, "report.pdf").await;
        add_tag(&pool, &doc_id, "Report", None).await.expect("add");

        assert!(remove_tag(&pool, &doc_id, "REPORT").await.expect("remove"));
        assert!(list_tags(&pool, &doc_id).await.expect("list").is_empty());
        // Second removal finds nothing.
        assert!(
            !remove_tag(&pool, &doc_id, "report")
                .await
                .expect("remove again")
        );
        // Removing from a document that never had the tag.
        let other = new_doc(&pool, "other.pdf").await;
        assert!(
            !remove_tag(&pool, &other, "Report")
                .await
                .expect("remove from other")
        );
    }

    #[tokio::test]
    async fn tags_and_assignments_behave_on_document_purge() {
        let pool = test_pool().await;
        let doc_id = new_doc(&pool, "report.pdf").await;
        add_tag(&pool, &doc_id, "keep-me", None).await.expect("add");

        delete_document(&pool, &doc_id).await.expect("trash");
        purge_document(&pool, &doc_id).await.expect("purge");

        // Assignments cascade away; the global tag row itself remains.
        let assignments: i64 =
            sqlx::query_scalar("SELECT COUNT(*) FROM tag_assignments_tbl WHERE document_id = ?")
                .bind(doc_id.as_str())
                .fetch_one(&pool)
                .await
                .expect("count assignments");
        assert_eq!(assignments, 0);
        let tags: i64 = sqlx::query_scalar("SELECT COUNT(*) FROM tags_tbl")
            .fetch_one(&pool)
            .await
            .expect("count tags");
        assert_eq!(tags, 1);
    }
}
