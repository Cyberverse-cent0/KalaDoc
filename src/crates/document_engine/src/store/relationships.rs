//! Document relationship persistence — directed links between documents.

use serde_json::json;
use sqlx::{Row, SqlitePool, sqlite::SqliteRow};

use crate::crates::document_engine::audit::DocumentAuditEventType;
use crate::crates::document_engine::document::Document;
use crate::crates::document_engine::ids::DocumentId;
use crate::crates::document_engine::relationship::{
    DocumentRelationship, DocumentRelationshipType,
};

use super::documents::{document_from_row, fetch_document};
use super::history::record_event;
use super::{StoreError, StoreResult, enum_parse, enum_str, gen_id, now_rfc3339};

const REL_COLS: &str =
    "id, source_document_id, target_document_id, relationship_type, created_by, created_at";

fn relationship_from_row(row: &SqliteRow) -> StoreResult<DocumentRelationship> {
    Ok(DocumentRelationship {
        id: row.get("id"),
        source_document_id: DocumentId::new(row.get::<String, _>("source_document_id")),
        target_document_id: DocumentId::new(row.get::<String, _>("target_document_id")),
        relationship_type: enum_parse(&row.get::<String, _>("relationship_type"))?,
        created_by: row.get("created_by"),
        created_at: row.get("created_at"),
    })
}

/// Creates a directed relationship from `source_document_id` to
/// `target_document_id`. Both documents must exist, self-links are
/// rejected, and duplicates (same source/target/type triple) are
/// rejected with [`StoreError::Validation`]. Records a
/// [`DocumentAuditEventType::RelationshipCreated`] event on the source
/// document.
pub async fn create_relationship(
    pool: &SqlitePool,
    source_document_id: &DocumentId,
    target_document_id: &DocumentId,
    relationship_type: DocumentRelationshipType,
    created_by: Option<&str>,
) -> StoreResult<DocumentRelationship> {
    if source_document_id == target_document_id {
        return Err(StoreError::validation(
            "target_document_id",
            "a document cannot be related to itself",
        ));
    }
    fetch_document(pool, source_document_id)
        .await?
        .ok_or_else(|| StoreError::not_found("document", source_document_id))?;
    fetch_document(pool, target_document_id)
        .await?
        .ok_or_else(|| StoreError::not_found("document", target_document_id))?;

    let type_str = enum_str(&relationship_type)?;
    let existing: Option<String> = sqlx::query_scalar(
        "SELECT id FROM document_relationships_tbl
         WHERE source_document_id = ? AND target_document_id = ? AND relationship_type = ?",
    )
    .bind(source_document_id.as_str())
    .bind(target_document_id.as_str())
    .bind(&type_str)
    .fetch_optional(pool)
    .await?;
    if existing.is_some() {
        return Err(StoreError::validation(
            "relationship",
            "relationship already exists",
        ));
    }

    let relationship = DocumentRelationship {
        id: gen_id("rel"),
        source_document_id: source_document_id.clone(),
        target_document_id: target_document_id.clone(),
        relationship_type,
        created_by: created_by.map(str::to_string),
        created_at: now_rfc3339(),
    };
    sqlx::query(
        "INSERT INTO document_relationships_tbl
         (id, source_document_id, target_document_id, relationship_type, created_by, created_at)
         VALUES (?, ?, ?, ?, ?, ?)",
    )
    .bind(&relationship.id)
    .bind(source_document_id.as_str())
    .bind(target_document_id.as_str())
    .bind(&type_str)
    .bind(created_by)
    .bind(&relationship.created_at)
    .execute(pool)
    .await?;

    record_event(
        pool,
        source_document_id,
        created_by,
        DocumentAuditEventType::RelationshipCreated,
        Some(&json!({
            "relationship_id": relationship.id,
            "target_document_id": target_document_id.as_str(),
            "relationship_type": type_str,
        })),
    )
    .await?;

    Ok(relationship)
}

/// Deletes a relationship by id. Returns `true` when a row was
/// removed. When the relationship existed, a
/// [`DocumentAuditEventType::RelationshipDeleted`] event is recorded on
/// the source document.
pub async fn delete_relationship(pool: &SqlitePool, id: &str) -> StoreResult<bool> {
    let row = sqlx::query(&format!(
        "SELECT {REL_COLS} FROM document_relationships_tbl WHERE id = ?"
    ))
    .bind(id)
    .fetch_optional(pool)
    .await?;
    let Some(row) = row else {
        return Ok(false);
    };
    let relationship = relationship_from_row(&row)?;

    sqlx::query("DELETE FROM document_relationships_tbl WHERE id = ?")
        .bind(id)
        .execute(pool)
        .await?;

    record_event(
        pool,
        &relationship.source_document_id,
        None,
        DocumentAuditEventType::RelationshipDeleted,
        Some(&json!({
            "relationship_id": relationship.id,
            "target_document_id": relationship.target_document_id.as_str(),
            "relationship_type": enum_str(&relationship.relationship_type)?,
        })),
    )
    .await?;
    Ok(true)
}

/// Lists relationships where `source_document_id` is the source
/// (outgoing links), oldest first.
pub async fn list_outgoing_relationships(
    pool: &SqlitePool,
    source_document_id: &DocumentId,
) -> StoreResult<Vec<DocumentRelationship>> {
    let rows = sqlx::query(&format!(
        "SELECT {REL_COLS} FROM document_relationships_tbl
         WHERE source_document_id = ? ORDER BY created_at, id"
    ))
    .bind(source_document_id.as_str())
    .fetch_all(pool)
    .await?;
    rows.iter().map(relationship_from_row).collect()
}

/// Lists relationships where `target_document_id` is the target
/// (incoming links), oldest first.
pub async fn list_incoming_relationships(
    pool: &SqlitePool,
    target_document_id: &DocumentId,
) -> StoreResult<Vec<DocumentRelationship>> {
    let rows = sqlx::query(&format!(
        "SELECT {REL_COLS} FROM document_relationships_tbl
         WHERE target_document_id = ? ORDER BY created_at, id"
    ))
    .bind(target_document_id.as_str())
    .fetch_all(pool)
    .await?;
    rows.iter().map(relationship_from_row).collect()
}

/// A document related to another via a [`DocumentRelationship`].
#[derive(Debug, Clone)]
pub struct RelatedDocument {
    /// The other document in the relationship.
    pub document: Document,
    pub relationship_type: DocumentRelationshipType,
    /// Direction relative to the queried document: `true` when the
    /// queried document is the relationship source (the related
    /// document is the target), `false` when it is the target.
    pub outgoing: bool,
}

/// Returns all documents related to `document_id` in both directions,
/// deduplicated by document id (first relationship wins when a pair
/// has multiple types). Outgoing links are listed before incoming ones.
pub async fn related_documents(
    pool: &SqlitePool,
    document_id: &DocumentId,
) -> StoreResult<Vec<RelatedDocument>> {
    let rows = sqlx::query(&format!(
        "SELECT d.*, r.relationship_type AS rel_type, 1 AS outgoing
         FROM document_relationships_tbl r
         JOIN documents_tbl d ON d.id = r.target_document_id
         WHERE r.source_document_id = ?
         UNION ALL
         SELECT d.*, r.relationship_type AS rel_type, 0 AS outgoing
         FROM document_relationships_tbl r
         JOIN documents_tbl d ON d.id = r.source_document_id
         WHERE r.target_document_id = ?
         ORDER BY outgoing DESC, created_at"
    ))
    .bind(document_id.as_str())
    .bind(document_id.as_str())
    .fetch_all(pool)
    .await?;

    let mut seen = std::collections::HashSet::new();
    let mut result = Vec::new();
    for row in &rows {
        let doc = document_from_row(row)?;
        if seen.insert(doc.id.clone()) {
            result.push(RelatedDocument {
                document: doc,
                relationship_type: enum_parse(&row.get::<String, _>("rel_type"))?,
                outgoing: row.get::<i64, _>("outgoing") != 0,
            });
        }
    }
    Ok(result)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::crates::document_engine::document::{DocumentOrigin, DocumentSource, DocumentType};
    use crate::crates::document_engine::store::documents::{NewDocument, create_document};
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
    async fn relationship_create_list_delete() {
        let pool = test_pool().await;
        let a = new_doc(&pool, "a.pdf").await;
        let b = new_doc(&pool, "b.pdf").await;
        let c = new_doc(&pool, "c.pdf").await;

        let rel = create_relationship(
            &pool,
            &a,
            &b,
            DocumentRelationshipType::Related,
            Some("user-1"),
        )
        .await
        .unwrap();
        assert_eq!(rel.source_document_id, a);
        assert_eq!(rel.target_document_id, b);
        assert_eq!(rel.relationship_type, DocumentRelationshipType::Related);
        assert_eq!(rel.created_by.as_deref(), Some("user-1"));

        create_relationship(&pool, &b, &a, DocumentRelationshipType::Parent, None)
            .await
            .unwrap();
        create_relationship(&pool, &a, &c, DocumentRelationshipType::Attachment, None)
            .await
            .unwrap();

        let outgoing = list_outgoing_relationships(&pool, &a).await.unwrap();
        assert_eq!(outgoing.len(), 2);
        assert!(outgoing.iter().all(|r| r.source_document_id == a));

        let incoming = list_incoming_relationships(&pool, &a).await.unwrap();
        assert_eq!(incoming.len(), 1);
        assert_eq!(incoming[0].source_document_id, b);

        assert!(delete_relationship(&pool, &rel.id).await.unwrap());
        assert!(!delete_relationship(&pool, &rel.id).await.unwrap());
        assert_eq!(
            list_outgoing_relationships(&pool, &a).await.unwrap().len(),
            1
        );
    }

    #[tokio::test]
    async fn relationship_rejects_duplicates_self_links_and_missing_docs() {
        let pool = test_pool().await;
        let a = new_doc(&pool, "a.pdf").await;
        let b = new_doc(&pool, "b.pdf").await;

        create_relationship(&pool, &a, &b, DocumentRelationshipType::Related, None)
            .await
            .unwrap();
        // Exact duplicate rejected.
        assert!(matches!(
            create_relationship(&pool, &a, &b, DocumentRelationshipType::Related, None).await,
            Err(StoreError::Validation { .. })
        ));
        // Same pair with a different type is allowed.
        create_relationship(&pool, &a, &b, DocumentRelationshipType::Duplicate, None)
            .await
            .unwrap();

        // Self-link rejected.
        assert!(matches!(
            create_relationship(&pool, &a, &a, DocumentRelationshipType::Related, None).await,
            Err(StoreError::Validation { .. })
        ));

        // Missing source/target rejected.
        let missing = DocumentId::new("doc-missing");
        assert!(matches!(
            create_relationship(&pool, &missing, &b, DocumentRelationshipType::Related, None).await,
            Err(StoreError::NotFound { .. })
        ));
        assert!(matches!(
            create_relationship(&pool, &a, &missing, DocumentRelationshipType::Related, None).await,
            Err(StoreError::NotFound { .. })
        ));
    }

    #[tokio::test]
    async fn related_documents_both_directions() {
        let pool = test_pool().await;
        let a = new_doc(&pool, "a.pdf").await;
        let b = new_doc(&pool, "b.pdf").await;
        let c = new_doc(&pool, "c.pdf").await;

        create_relationship(&pool, &a, &b, DocumentRelationshipType::Attachment, None)
            .await
            .unwrap();
        create_relationship(&pool, &c, &a, DocumentRelationshipType::Supersedes, None)
            .await
            .unwrap();

        let related = related_documents(&pool, &a).await.unwrap();
        assert_eq!(related.len(), 2);
        let by_id: std::collections::HashMap<_, _> =
            related.iter().map(|r| (r.document.id.clone(), r)).collect();
        assert_eq!(
            by_id[&b].relationship_type,
            DocumentRelationshipType::Attachment
        );
        assert!(by_id[&b].outgoing);
        assert_eq!(
            by_id[&c].relationship_type,
            DocumentRelationshipType::Supersedes
        );
        assert!(!by_id[&c].outgoing);

        assert!(related_documents(&pool, &b).await.unwrap().len() == 1);
    }

    #[tokio::test]
    async fn relationship_cascade_delete() {
        let pool = test_pool().await;
        let a = new_doc(&pool, "a.pdf").await;
        let b = new_doc(&pool, "b.pdf").await;

        create_relationship(&pool, &a, &b, DocumentRelationshipType::Related, None)
            .await
            .unwrap();

        sqlx::query("DELETE FROM documents_tbl WHERE id = ?")
            .bind(a.as_str())
            .execute(&pool)
            .await
            .unwrap();
        let remaining: i64 = sqlx::query_scalar("SELECT COUNT(*) FROM document_relationships_tbl")
            .fetch_one(&pool)
            .await
            .unwrap();
        assert_eq!(remaining, 0);
    }
}
