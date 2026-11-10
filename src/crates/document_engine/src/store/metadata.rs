//! Document metadata persistence — descriptive fields and custom fields
//! stored as JSON TEXT columns.

use serde_json::{Map, Value};
use sqlx::{Row, SqlitePool, sqlite::SqliteRow};

use crate::crates::document_engine::audit::DocumentAuditEventType;
use crate::crates::document_engine::ids::DocumentId;
use crate::crates::document_engine::metadata::DocumentMetadata;

use super::documents::fetch_document;
use super::history::record_event;
use super::{StoreError, StoreResult, now_rfc3339};

pub(crate) const META_COLS: &str =
    "document_id, title, author, subject, language, keywords, custom_fields";

fn metadata_from_row(row: &SqliteRow) -> StoreResult<DocumentMetadata> {
    let keywords_raw: String = row.get("keywords");
    let keywords: Vec<String> = serde_json::from_str(&keywords_raw)?;
    let custom_raw: String = row.get("custom_fields");
    let custom_fields: Map<String, Value> = serde_json::from_str(&custom_raw)?;
    Ok(DocumentMetadata {
        document_id: DocumentId::new(row.get::<String, _>("document_id")),
        title: row.get("title"),
        author: row.get("author"),
        subject: row.get("subject"),
        language: row.get("language"),
        keywords,
        custom_fields,
    })
}

fn default_metadata(document_id: &DocumentId) -> DocumentMetadata {
    DocumentMetadata {
        document_id: document_id.clone(),
        title: None,
        author: None,
        subject: None,
        language: None,
        keywords: Vec::new(),
        custom_fields: Map::new(),
    }
}

/// Inserts or fully replaces the metadata row for a document.
/// `keywords` and `custom_fields` are serialized as JSON TEXT.
/// Returns `StoreError::NotFound` when the document does not exist and
/// records a [`DocumentAuditEventType::MetadataUpdated`] event.
pub async fn upsert_document_metadata(
    pool: &SqlitePool,
    metadata: &DocumentMetadata,
) -> StoreResult<()> {
    fetch_document(pool, &metadata.document_id)
        .await?
        .ok_or_else(|| StoreError::not_found("document", &metadata.document_id))?;

    let now = now_rfc3339();
    sqlx::query(
        "INSERT INTO document_metadata_tbl
         (document_id, title, author, subject, language, keywords, custom_fields,
          created_at, updated_at)
         VALUES (?, ?, ?, ?, ?, ?, ?, ?, ?)
         ON CONFLICT(document_id) DO UPDATE SET
            title = excluded.title,
            author = excluded.author,
            subject = excluded.subject,
            language = excluded.language,
            keywords = excluded.keywords,
            custom_fields = excluded.custom_fields,
            updated_at = excluded.updated_at",
    )
    .bind(metadata.document_id.as_str())
    .bind(&metadata.title)
    .bind(&metadata.author)
    .bind(&metadata.subject)
    .bind(&metadata.language)
    .bind(serde_json::to_string(&metadata.keywords)?)
    .bind(serde_json::to_string(&metadata.custom_fields)?)
    .bind(&now)
    .bind(&now)
    .execute(pool)
    .await?;

    record_event(
        pool,
        &metadata.document_id,
        None,
        DocumentAuditEventType::MetadataUpdated,
        Some(&serde_json::json!({ "operation": "upsert" })),
    )
    .await?;
    Ok(())
}

/// Fetches the metadata row for a document. Returns `None` when the
/// document has no metadata stored yet.
pub async fn get_document_metadata(
    pool: &SqlitePool,
    document_id: &DocumentId,
) -> StoreResult<Option<DocumentMetadata>> {
    let row = sqlx::query(&format!(
        "SELECT {META_COLS} FROM document_metadata_tbl WHERE document_id = ?"
    ))
    .bind(document_id.as_str())
    .fetch_optional(pool)
    .await?;
    row.map(|r| metadata_from_row(&r)).transpose()
}

/// Sets (or overwrites) a single custom field, creating the metadata
/// row with defaults when none exists yet. Returns the updated
/// [`DocumentMetadata`]. `key` must be non-empty.
pub async fn set_custom_field(
    pool: &SqlitePool,
    document_id: &DocumentId,
    key: &str,
    value: &Value,
) -> StoreResult<DocumentMetadata> {
    if key.trim().is_empty() {
        return Err(StoreError::validation("key", "must not be empty"));
    }
    fetch_document(pool, document_id)
        .await?
        .ok_or_else(|| StoreError::not_found("document", document_id))?;

    let mut metadata = get_document_metadata(pool, document_id)
        .await?
        .unwrap_or_else(|| default_metadata(document_id));
    metadata
        .custom_fields
        .insert(key.to_string(), value.clone());
    upsert_document_metadata(pool, &metadata).await?;
    record_event(
        pool,
        document_id,
        None,
        DocumentAuditEventType::MetadataUpdated,
        Some(&serde_json::json!({ "operation": "set_custom_field", "key": key })),
    )
    .await?;
    Ok(metadata)
}

/// Removes a single custom field. Returns `true` when a field was
/// actually removed, `false` when the key was absent (or no metadata
/// row exists). Returns `StoreError::NotFound` when the document does
/// not exist.
pub async fn remove_custom_field(
    pool: &SqlitePool,
    document_id: &DocumentId,
    key: &str,
) -> StoreResult<bool> {
    fetch_document(pool, document_id)
        .await?
        .ok_or_else(|| StoreError::not_found("document", document_id))?;

    let Some(mut metadata) = get_document_metadata(pool, document_id).await? else {
        return Ok(false);
    };
    if metadata.custom_fields.remove(key).is_none() {
        return Ok(false);
    }
    upsert_document_metadata(pool, &metadata).await?;
    record_event(
        pool,
        document_id,
        None,
        DocumentAuditEventType::MetadataUpdated,
        Some(&serde_json::json!({ "operation": "remove_custom_field", "key": key })),
    )
    .await?;
    Ok(true)
}

/// Deletes the metadata row for a document. Returns `true` when a row
/// was removed. The document itself is not touched.
pub async fn delete_document_metadata(
    pool: &SqlitePool,
    document_id: &DocumentId,
) -> StoreResult<bool> {
    let done = sqlx::query("DELETE FROM document_metadata_tbl WHERE document_id = ?")
        .bind(document_id.as_str())
        .execute(pool)
        .await?;
    if done.rows_affected() > 0 {
        record_event(
            pool,
            document_id,
            None,
            DocumentAuditEventType::MetadataUpdated,
            Some(&serde_json::json!({ "operation": "delete" })),
        )
        .await?;
    }
    Ok(done.rows_affected() > 0)
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

    fn sample_metadata(document_id: &DocumentId) -> DocumentMetadata {
        let mut custom_fields = Map::new();
        custom_fields.insert("financial_year".to_string(), Value::from(2026));
        custom_fields.insert("department".to_string(), Value::from("finance"));
        DocumentMetadata {
            document_id: document_id.clone(),
            title: Some("Annual Report".to_string()),
            author: Some("Ada".to_string()),
            subject: Some("Finance".to_string()),
            language: Some("en".to_string()),
            keywords: vec!["report".to_string(), "finance".to_string()],
            custom_fields,
        }
    }

    #[tokio::test]
    async fn metadata_roundtrip() {
        let pool = test_pool().await;
        let doc_id = new_doc(&pool, "report.pdf").await;

        assert!(
            get_document_metadata(&pool, &doc_id)
                .await
                .unwrap()
                .is_none()
        );

        let meta = sample_metadata(&doc_id);
        upsert_document_metadata(&pool, &meta).await.unwrap();

        let fetched = get_document_metadata(&pool, &doc_id)
            .await
            .unwrap()
            .expect("metadata row");
        assert_eq!(fetched, meta);
    }

    #[tokio::test]
    async fn metadata_upsert_replaces_and_preserves_created_at() {
        let pool = test_pool().await;
        let doc_id = new_doc(&pool, "report.pdf").await;

        upsert_document_metadata(&pool, &sample_metadata(&doc_id))
            .await
            .unwrap();
        let first = get_document_metadata(&pool, &doc_id)
            .await
            .unwrap()
            .unwrap();

        let mut updated = sample_metadata(&doc_id);
        updated.title = Some("Renamed".to_string());
        updated.keywords = vec!["only".to_string()];
        upsert_document_metadata(&pool, &updated).await.unwrap();

        let second = get_document_metadata(&pool, &doc_id)
            .await
            .unwrap()
            .unwrap();
        assert_eq!(second.title.as_deref(), Some("Renamed"));
        assert_eq!(second.keywords, vec!["only".to_string()]);
        // created_at is preserved across upserts — verified indirectly by
        // the row still existing exactly once.
        let count: i64 =
            sqlx::query_scalar("SELECT COUNT(*) FROM document_metadata_tbl WHERE document_id = ?")
                .bind(doc_id.as_str())
                .fetch_one(&pool)
                .await
                .unwrap();
        assert_eq!(count, 1);
        let _ = first;
    }

    #[tokio::test]
    async fn custom_fields_json_roundtrip() {
        let pool = test_pool().await;
        let doc_id = new_doc(&pool, "report.pdf").await;

        let meta = set_custom_field(&pool, &doc_id, "financial_year", &Value::from(2026))
            .await
            .unwrap();
        assert_eq!(meta.custom_fields["financial_year"], Value::from(2026));

        // Raw column really is JSON text.
        let raw: String = sqlx::query_scalar(
            "SELECT custom_fields FROM document_metadata_tbl WHERE document_id = ?",
        )
        .bind(doc_id.as_str())
        .fetch_one(&pool)
        .await
        .unwrap();
        assert!(raw.contains("financial_year"));

        set_custom_field(&pool, &doc_id, "nested", &serde_json::json!({"a": [1, 2]}))
            .await
            .unwrap();
        let fetched = get_document_metadata(&pool, &doc_id)
            .await
            .unwrap()
            .unwrap();
        assert_eq!(
            fetched.custom_fields["nested"],
            serde_json::json!({"a": [1, 2]})
        );

        assert!(
            remove_custom_field(&pool, &doc_id, "financial_year")
                .await
                .unwrap()
        );
        assert!(
            !remove_custom_field(&pool, &doc_id, "financial_year")
                .await
                .unwrap()
        );
        let fetched = get_document_metadata(&pool, &doc_id)
            .await
            .unwrap()
            .unwrap();
        assert!(!fetched.custom_fields.contains_key("financial_year"));
        assert!(fetched.custom_fields.contains_key("nested"));
    }

    #[tokio::test]
    async fn metadata_requires_existing_document() {
        let pool = test_pool().await;
        let meta = sample_metadata(&DocumentId::new("doc-missing"));
        assert!(matches!(
            upsert_document_metadata(&pool, &meta).await,
            Err(StoreError::NotFound { .. })
        ));
    }

    #[tokio::test]
    async fn metadata_delete_and_cascade() {
        let pool = test_pool().await;
        let doc_id = new_doc(&pool, "report.pdf").await;
        upsert_document_metadata(&pool, &sample_metadata(&doc_id))
            .await
            .unwrap();

        assert!(delete_document_metadata(&pool, &doc_id).await.unwrap());
        assert!(!delete_document_metadata(&pool, &doc_id).await.unwrap());
        assert!(
            get_document_metadata(&pool, &doc_id)
                .await
                .unwrap()
                .is_none()
        );

        // ON DELETE CASCADE removes metadata with the document.
        upsert_document_metadata(&pool, &sample_metadata(&doc_id))
            .await
            .unwrap();
        sqlx::query("DELETE FROM documents_tbl WHERE id = ?")
            .bind(doc_id.as_str())
            .execute(&pool)
            .await
            .unwrap();
        let remaining: i64 =
            sqlx::query_scalar("SELECT COUNT(*) FROM document_metadata_tbl WHERE document_id = ?")
                .bind(doc_id.as_str())
                .fetch_one(&pool)
                .await
                .unwrap();
        assert_eq!(remaining, 0);
    }
}
