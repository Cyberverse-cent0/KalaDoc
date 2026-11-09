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
