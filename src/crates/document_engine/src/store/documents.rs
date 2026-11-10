//! Document CRUD and organization operations.

use serde_json::json;
use sqlx::{Row, SqlitePool, sqlite::SqliteRow};

use crate::crates::document_engine::audit::DocumentAuditEventType;
use crate::crates::document_engine::document::{
    Document, DocumentName, DocumentSource, DocumentStatus, DocumentType,
};
use crate::crates::document_engine::ids::{DocumentId, FolderId};

use super::history::record_event;
use super::{StoreError, StoreResult, enum_parse, enum_str, gen_id, now_rfc3339};

pub(crate) const DOC_COLS: &str = "id, name, document_type, status, origin, source_uri, \
     source_external_ref, source_imported_at, folder_id, owner_id, created_at, updated_at";

/// Input for [`create_document`]: fields needed to register a brand-new
/// document (typically an upload).
#[derive(Debug, Clone)]
pub struct NewDocument {
    pub name: String,
    pub document_type: DocumentType,
    pub owner_id: String,
    pub source: DocumentSource,
}

/// Patch for [`update_document`]. `None` fields are left unchanged.
#[derive(Debug, Clone, Default, serde::Serialize)]
pub struct DocumentPatch {
    pub name: Option<String>,
    pub document_type: Option<DocumentType>,
}

pub(crate) fn document_from_row(row: &SqliteRow) -> StoreResult<Document> {
    Ok(Document {
        id: DocumentId::new(row.get::<String, _>("id")),
        name: DocumentName::new(row.get::<String, _>("name"))?,
        document_type: enum_parse(&row.get::<String, _>("document_type"))?,
        status: enum_parse(&row.get::<String, _>("status"))?,
        source: DocumentSource {
            origin: enum_parse(&row.get::<String, _>("origin"))?,
            uri: row.get("source_uri"),
            external_reference: row.get("source_external_ref"),
            imported_at: row.get("source_imported_at"),
        },
        folder_id: row.get::<Option<String>, _>("folder_id").map(FolderId::new),
        owner_id: row.get("owner_id"),
        created_at: row.get("created_at"),
        updated_at: row.get("updated_at"),
    })
}

async fn insert_document(pool: &SqlitePool, doc: &Document) -> StoreResult<()> {
    sqlx::query(
        "INSERT INTO documents_tbl
         (id, name, document_type, status, lifecycle_status, origin, source_uri,
          source_external_ref, source_imported_at, folder_id, owner_id,
          created_at, updated_at, deleted_at)
         VALUES (?, ?, ?, ?, 'active', ?, ?, ?, ?, ?, ?, ?, ?, NULL)",
    )
    .bind(doc.id.as_str())
    .bind(doc.name.as_str())
    .bind(enum_str(&doc.document_type)?)
    .bind(enum_str(&doc.status)?)
    .bind(enum_str(&doc.source.origin)?)
    .bind(&doc.source.uri)
    .bind(&doc.source.external_reference)
    .bind(&doc.source.imported_at)
    .bind(doc.folder_id.as_ref().map(|f| f.as_str().to_string()))
    .bind(&doc.owner_id)
    .bind(&doc.created_at)
    .bind(&doc.updated_at)
    .execute(pool)
    .await?;
    Ok(())
}

pub(crate) async fn fetch_document(
    pool: &SqlitePool,
    id: &DocumentId,
) -> StoreResult<Option<Document>> {
    let row = sqlx::query(&format!(
        "SELECT {DOC_COLS} FROM documents_tbl WHERE id = ?"
    ))
    .bind(id.as_str())
    .fetch_optional(pool)
    .await?;
    row.map(|r| document_from_row(&r)).transpose()
}

/// Creates a new document with a generated id and records a `created`
/// audit event. Returns the persisted [`Document`].
pub async fn create_document(pool: &SqlitePool, new: &NewDocument) -> StoreResult<Document> {
    let name = DocumentName::new(new.name.clone())?;
    let now = now_rfc3339();
    let doc = Document::new(
        DocumentId::new(gen_id("doc")),
        name,
        new.document_type,
        new.owner_id.clone(),
        new.source.clone(),
        now,
    );
    insert_document(pool, &doc).await?;
    record_event(
        pool,
        &doc.id,
        Some(&doc.owner_id),
        DocumentAuditEventType::Created,
        Some(&json!({ "name": doc.name.as_str() })),
    )
    .await?;
    Ok(doc)
}

/// Persists an already-constructed [`Document`] aggregate — used when a
/// document arrives from an external system or import pipeline with its
/// id and metadata already assigned. Unlike [`create_document`], this
/// does not generate an id or audit event; callers own those decisions.
pub async fn register_document(pool: &SqlitePool, doc: &Document) -> StoreResult<()> {
    insert_document(pool, doc).await
}

/// Fetches a document by id. Returns `None` when no such document exists
/// (including soft-deleted rows only if the caller filters them out —
/// this returns rows regardless of `status`).
pub async fn get_document(pool: &SqlitePool, id: &DocumentId) -> StoreResult<Option<Document>> {
    fetch_document(pool, id).await
}

/// Lists documents, optionally filtered by owner and/or folder.
/// `include_deleted` controls whether trashed/deleted rows appear.
pub async fn list_documents(
    pool: &SqlitePool,
    owner_id: Option<&str>,
    folder_id: Option<&FolderId>,
    include_deleted: bool,
    limit: i64,
) -> StoreResult<Vec<Document>> {
    let mut qb = sqlx::QueryBuilder::<sqlx::Sqlite>::new(format!(
        "SELECT {DOC_COLS} FROM documents_tbl WHERE 1=1"
    ));
    if let Some(owner) = owner_id {
        qb.push(" AND owner_id = ").push_bind(owner);
    }
    if let Some(folder) = folder_id {
        qb.push(" AND folder_id = ").push_bind(folder.as_str());
    }
    if !include_deleted {
        qb.push(" AND deleted_at IS NULL");
    }
    qb.push(" ORDER BY created_at DESC LIMIT ").push_bind(limit);
    let rows = qb.build().fetch_all(pool).await?;
    rows.iter().map(document_from_row).collect()
}

/// Escapes `LIKE` wildcards so user input matches literally.
fn like_escape(s: &str) -> String {
    s.replace('\\', "\\\\")
        .replace('%', "\\%")
        .replace('_', "\\_")
}

/// Searches documents by name substring (case-insensitive). Trashed and
/// deleted documents are excluded.
pub async fn search_documents(
    pool: &SqlitePool,
    query: &str,
    limit: i64,
) -> StoreResult<Vec<Document>> {
    let pattern = format!("%{}%", like_escape(query));
    let rows = sqlx::query(&format!(
        "SELECT {DOC_COLS} FROM documents_tbl
         WHERE name LIKE ? ESCAPE '\\' AND deleted_at IS NULL
         ORDER BY name LIMIT ?"
    ))
    .bind(pattern)
    .bind(limit)
    .fetch_all(pool)
    .await?;
    rows.iter().map(document_from_row).collect()
}

/// Applies a [`DocumentPatch`] to a document and bumps `updated_at`.
/// Returns `StoreError::NotFound` when the document does not exist.
pub async fn update_document(
    pool: &SqlitePool,
    id: &DocumentId,
    patch: &DocumentPatch,
) -> StoreResult<Document> {
    let existing = fetch_document(pool, id)
        .await?
        .ok_or_else(|| StoreError::not_found("document", id))?;

    let name = match &patch.name {
        Some(n) => DocumentName::new(n.clone())?.as_str().to_string(),
        None => existing.name.as_str().to_string(),
    };
    let doc_type = patch.document_type.unwrap_or(existing.document_type);

    sqlx::query(
        "UPDATE documents_tbl SET name = ?, document_type = ?, updated_at = ? WHERE id = ?",
    )
    .bind(&name)
    .bind(enum_str(&doc_type)?)
    .bind(now_rfc3339())
    .bind(id.as_str())
    .execute(pool)
    .await?;

    record_event(
        pool,
        id,
        None,
        DocumentAuditEventType::Updated,
        Some(&json!({ "patch": patch })),
    )
    .await?;

    fetch_document(pool, id)
        .await?
        .ok_or_else(|| StoreError::not_found("document", id))
}

/// Renames a document. Equivalent to [`update_document`] with only the
/// name set; exists as a separate operation for command dispatch.
pub async fn rename_document(
    pool: &SqlitePool,
    id: &DocumentId,
    new_name: &str,
) -> StoreResult<Document> {
    update_document(
        pool,
        id,
        &DocumentPatch {
            name: Some(new_name.to_string()),
            document_type: None,
        },
    )
    .await
}

/// Moves a document into a folder (`None` moves it to the root).
/// Returns `StoreError::NotFound` if the document — or the target
/// folder, when given — does not exist.
pub async fn move_document(
    pool: &SqlitePool,
    id: &DocumentId,
    folder_id: Option<&FolderId>,
) -> StoreResult<Document> {
    fetch_document(pool, id)
        .await?
        .ok_or_else(|| StoreError::not_found("document", id))?;

    if let Some(folder) = folder_id {
        let exists: bool =
            sqlx::query_scalar("SELECT EXISTS(SELECT 1 FROM folders_tbl WHERE id = ?)")
                .bind(folder.as_str())
                .fetch_one(pool)
                .await?;
        if !exists {
            return Err(StoreError::not_found("folder", folder));
        }
    }

    sqlx::query("UPDATE documents_tbl SET folder_id = ?, updated_at = ? WHERE id = ?")
        .bind(folder_id.map(|f| f.as_str()))
        .bind(now_rfc3339())
        .bind(id.as_str())
        .execute(pool)
        .await?;

    record_event(
        pool,
        id,
        None,
        DocumentAuditEventType::Updated,
        Some(&json!({ "move_to_folder": folder_id.map(|f| f.as_str()) })),
    )
    .await?;

    fetch_document(pool, id)
        .await?
        .ok_or_else(|| StoreError::not_found("document", id))
}

/// Duplicates a document under a new id. Copies the document row, all
/// artifact rows (pointing at the same `storage_uri` — bytes are shared,
/// not re-uploaded), and tag assignments. Versions and permissions are
/// not copied. `new_name` defaults to `"<name> (copy)"`.
pub async fn copy_document(
    pool: &SqlitePool,
    id: &DocumentId,
    new_name: Option<&str>,
) -> StoreResult<Document> {
    let source = fetch_document(pool, id)
        .await?
        .ok_or_else(|| StoreError::not_found("document", id))?;

    let now = now_rfc3339();
    let name = DocumentName::new(
        new_name
            .map(str::to_string)
            .unwrap_or_else(|| format!("{} (copy)", source.name.as_str())),
    )?;
    let copy = Document {
        id: DocumentId::new(gen_id("doc")),
        name,
        document_type: source.document_type,
        status: DocumentStatus::Active,
        source: source.source.clone(),
        folder_id: source.folder_id.clone(),
        owner_id: source.owner_id.clone(),
        created_at: now.clone(),
        updated_at: now,
    };
    insert_document(pool, &copy).await?;

    // Copy artifact rows one by one so each gets its own generated id.
    // `storage_uri` is shared — the bytes are not duplicated.
    let artifact_rows = sqlx::query(
        "SELECT storage_uri, file_name, mime_type, size_bytes,
                checksum_algorithm, checksum_value
         FROM artifacts_tbl WHERE document_id = ?",
    )
    .bind(id.as_str())
    .fetch_all(pool)
    .await?;
    for row in &artifact_rows {
        sqlx::query(
            "INSERT INTO artifacts_tbl
             (id, document_id, storage_uri, file_name, mime_type, size_bytes,
              checksum_algorithm, checksum_value, created_at)
             VALUES (?, ?, ?, ?, ?, ?, ?, ?, ?)",
        )
        .bind(gen_id("art"))
        .bind(copy.id.as_str())
        .bind(row.get::<String, _>("storage_uri"))
        .bind(row.get::<String, _>("file_name"))
        .bind(row.get::<Option<String>, _>("mime_type"))
        .bind(row.get::<i64, _>("size_bytes"))
        .bind(row.get::<Option<String>, _>("checksum_algorithm"))
        .bind(row.get::<Option<String>, _>("checksum_value"))
        .bind(&copy.created_at)
        .execute(pool)
        .await?;
    }

    sqlx::query(
        "INSERT OR IGNORE INTO tag_assignments_tbl
         (document_id, tag_id, assigned_by, assigned_at)
         SELECT ?, tag_id, assigned_by, ? FROM tag_assignments_tbl
         WHERE document_id = ?",
    )
    .bind(copy.id.as_str())
    .bind(&copy.created_at)
    .bind(id.as_str())
    .execute(pool)
    .await?;

    record_event(
        pool,
        &copy.id,
        Some(&copy.owner_id),
        DocumentAuditEventType::Created,
        Some(&json!({ "copied_from": id.as_str() })),
    )
    .await?;

    Ok(copy)
}

/// Soft-deletes a document: `status → trashed`, `deleted_at` stamped,
/// lifecycle moved to `trashed`. The row and its artifacts are retained
/// so [`restore_document`] can bring it back. Returns the updated
/// document, or `NotFound`.
pub async fn delete_document(pool: &SqlitePool, id: &DocumentId) -> StoreResult<Document> {
    let existing = fetch_document(pool, id)
        .await?
        .ok_or_else(|| StoreError::not_found("document", id))?;
    if existing.status == DocumentStatus::Deleted {
        return Err(StoreError::validation(
            "status",
            "document is already permanently deleted",
        ));
    }

    let now = now_rfc3339();
    sqlx::query(
        "UPDATE documents_tbl
         SET status = 'trashed', lifecycle_status = 'trashed', deleted_at = ?, updated_at = ?
         WHERE id = ?",
    )
    .bind(&now)
    .bind(&now)
    .bind(id.as_str())
    .execute(pool)
    .await?;

    record_event(pool, id, None, DocumentAuditEventType::Trashed, None).await?;

    fetch_document(pool, id)
        .await?
        .ok_or_else(|| StoreError::not_found("document", id))
}

/// Restores a soft-deleted document: clears `deleted_at`, returns status
/// to `active` and lifecycle to `restored`. Returns `NotFound` when the
/// document does not exist, `Validation` when it was never deleted.
pub async fn restore_document(pool: &SqlitePool, id: &DocumentId) -> StoreResult<Document> {
    let existing = fetch_document(pool, id)
        .await?
        .ok_or_else(|| StoreError::not_found("document", id))?;
    if existing.status != DocumentStatus::Trashed {
        return Err(StoreError::validation(
            "status",
            "only trashed documents can be restored",
        ));
    }

    let now = now_rfc3339();
    sqlx::query(
        "UPDATE documents_tbl
         SET status = 'active', lifecycle_status = 'restored', deleted_at = NULL, updated_at = ?
         WHERE id = ?",
    )
    .bind(&now)
    .bind(id.as_str())
    .execute(pool)
    .await?;

    record_event(pool, id, None, DocumentAuditEventType::Restored, None).await?;

    fetch_document(pool, id)
        .await?
        .ok_or_else(|| StoreError::not_found("document", id))
}

/// Archives a document: `status → archived`, lifecycle moved to
/// `archived`. The document stays listed (unlike trash) but is flagged
/// as archived; [`unarchive_document`] reverses it. Returns
/// `StoreError::Validation` when the document is trashed or deleted —
/// restore/purge it first.
pub async fn archive_document(pool: &SqlitePool, id: &DocumentId) -> StoreResult<Document> {
    let existing = fetch_document(pool, id)
        .await?
        .ok_or_else(|| StoreError::not_found("document", id))?;
    match existing.status {
        DocumentStatus::Active => {}
        DocumentStatus::Archived => {
            return Err(StoreError::validation(
                "status",
                "document is already archived",
            ));
        }
        _ => {
            return Err(StoreError::validation(
                "status",
                "only active documents can be archived",
            ));
        }
    }

    let now = now_rfc3339();
    sqlx::query(
        "UPDATE documents_tbl
         SET status = 'archived', lifecycle_status = 'archived', updated_at = ?
         WHERE id = ?",
    )
    .bind(&now)
    .bind(id.as_str())
    .execute(pool)
    .await?;

    record_event(pool, id, None, DocumentAuditEventType::Archived, None).await?;

    fetch_document(pool, id)
        .await?
        .ok_or_else(|| StoreError::not_found("document", id))
}

/// Unarchives a document: `status → active`, lifecycle moved to
/// `restored`. Returns `StoreError::Validation` when the document is
/// not currently archived.
pub async fn unarchive_document(pool: &SqlitePool, id: &DocumentId) -> StoreResult<Document> {
    let existing = fetch_document(pool, id)
        .await?
        .ok_or_else(|| StoreError::not_found("document", id))?;
    if existing.status != DocumentStatus::Archived {
        return Err(StoreError::validation(
            "status",
            "only archived documents can be unarchived",
        ));
    }

    let now = now_rfc3339();
    sqlx::query(
        "UPDATE documents_tbl
         SET status = 'active', lifecycle_status = 'restored', updated_at = ?
         WHERE id = ?",
    )
    .bind(&now)
    .bind(id.as_str())
    .execute(pool)
    .await?;

    record_event(pool, id, None, DocumentAuditEventType::Restored, None).await?;

    fetch_document(pool, id)
        .await?
        .ok_or_else(|| StoreError::not_found("document", id))
}

/// Permanently deletes a document: the row is removed and
/// `ON DELETE CASCADE` takes its artifacts, versions, revisions, tags,
/// permissions, and audit history with it. Only trashed documents can
/// be purged — restore them first otherwise. Returns `true` when a row
/// was removed, `false` when the document does not exist.
pub async fn purge_document(pool: &SqlitePool, id: &DocumentId) -> StoreResult<bool> {
    let existing = fetch_document(pool, id).await?;
    match existing {
        None => return Ok(false),
        Some(doc) if doc.status != DocumentStatus::Trashed => {
            return Err(StoreError::validation(
                "status",
                "only trashed documents can be permanently deleted",
            ));
        }
        Some(_) => {}
    }

    let done = sqlx::query("DELETE FROM documents_tbl WHERE id = ?")
        .bind(id.as_str())
        .execute(pool)
        .await?;
    Ok(done.rows_affected() > 0)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::crates::document_engine::document::DocumentOrigin;
    use crate::crates::document_engine::store::artifacts::{
        NewArtifact, list_artifacts, upload_artifact,
    };
    use crate::crates::document_engine::store::folders::create_folder;
    use crate::crates::document_engine::store::schema::init_document_store;
    use crate::crates::document_engine::store::tags::{add_tag, list_tags};
    use crate::crates::document_engine::store::versions::{create_version, list_versions};
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

    fn new_doc_input(name: &str, owner: &str) -> NewDocument {
        NewDocument {
            name: name.to_string(),
            document_type: DocumentType::Pdf,
            owner_id: owner.to_string(),
            source: DocumentSource::new(DocumentOrigin::Upload),
        }
    }

    async fn create_doc(pool: &SqlitePool, name: &str, owner: &str) -> Document {
        create_document(pool, &new_doc_input(name, owner))
            .await
            .expect("create document")
    }

    #[tokio::test]
    async fn create_and_get_roundtrip() {
        let pool = test_pool().await;
        let created = create_doc(&pool, "report.pdf", "user-1").await;

        assert_eq!(created.name.as_str(), "report.pdf");
        assert_eq!(created.document_type, DocumentType::Pdf);
        assert_eq!(created.status, DocumentStatus::Active);
        assert_eq!(created.owner_id, "user-1");
        assert!(created.folder_id.is_none());
        assert_eq!(created.source.origin, DocumentOrigin::Upload);
        assert!(!created.id.as_str().is_empty());

        let fetched = get_document(&pool, &created.id)
            .await
            .unwrap()
            .expect("document row");
        assert_eq!(fetched, created);
    }

    #[tokio::test]
    async fn register_document_persists_caller_supplied_aggregate() {
        let pool = test_pool().await;
        let now = now_rfc3339();
        let doc = Document::new(
            DocumentId::new("doc-fixed-id"),
            DocumentName::new("imported.docx").unwrap(),
            DocumentType::Text,
            "user-2",
            DocumentSource {
                origin: DocumentOrigin::Import,
                uri: Some("s3://bucket/key".to_string()),
                external_reference: Some("ext-42".to_string()),
                imported_at: Some(now.clone()),
            },
            now,
        );
        register_document(&pool, &doc).await.unwrap();

        let fetched = get_document(&pool, &doc.id)
            .await
            .unwrap()
            .expect("registered document");
        assert_eq!(fetched.id.as_str(), "doc-fixed-id");
        assert_eq!(fetched.document_type, DocumentType::Text);
        assert_eq!(fetched.owner_id, "user-2");
        assert_eq!(fetched.source.uri.as_deref(), Some("s3://bucket/key"));
        assert_eq!(fetched.source.external_reference.as_deref(), Some("ext-42"));
    }

    #[tokio::test]
    async fn get_document_missing_returns_none() {
        let pool = test_pool().await;
        assert!(
            get_document(&pool, &DocumentId::new("doc-nope"))
                .await
                .unwrap()
                .is_none()
        );
    }

    #[tokio::test]
    async fn create_document_rejects_invalid_name() {
        let pool = test_pool().await;
        let result = create_document(
            &pool,
            &NewDocument {
                name: "   ".to_string(),
                document_type: DocumentType::Pdf,
                owner_id: "user-1".to_string(),
                source: DocumentSource::new(DocumentOrigin::Upload),
            },
        )
        .await;
        assert!(result.is_err());
    }

    #[tokio::test]
    async fn list_documents_filters_by_owner() {
        let pool = test_pool().await;
        create_doc(&pool, "a.pdf", "user-1").await;
        create_doc(&pool, "b.pdf", "user-1").await;
        create_doc(&pool, "c.pdf", "user-2").await;

        let all = list_documents(&pool, None, None, false, 100).await.unwrap();
        assert_eq!(all.len(), 3);

        let mine = list_documents(&pool, Some("user-1"), None, false, 100)
            .await
            .unwrap();
        assert_eq!(mine.len(), 2);
        assert!(mine.iter().all(|d| d.owner_id == "user-1"));

        let theirs = list_documents(&pool, Some("user-2"), None, false, 100)
            .await
            .unwrap();
        assert_eq!(theirs.len(), 1);
        assert_eq!(theirs[0].name.as_str(), "c.pdf");

        // Owner scoping: another owner's document is not visible under
        // a foreign owner's listing.
        let none = list_documents(&pool, Some("user-3"), None, false, 100)
            .await
            .unwrap();
        assert!(none.is_empty());
    }

    #[tokio::test]
    async fn list_documents_excludes_deleted_unless_requested() {
        let pool = test_pool().await;
        let keep = create_doc(&pool, "keep.pdf", "user-1").await;
        let trash = create_doc(&pool, "trash.pdf", "user-1").await;
        delete_document(&pool, &trash.id).await.unwrap();

        let active_only = list_documents(&pool, None, None, false, 100).await.unwrap();
        assert_eq!(active_only.len(), 1);
        assert_eq!(active_only[0].id, keep.id);

        let with_deleted = list_documents(&pool, None, None, true, 100).await.unwrap();
        assert_eq!(with_deleted.len(), 2);
        assert!(with_deleted.iter().any(|d| d.id == trash.id));
    }

    #[tokio::test]
    async fn list_documents_filters_by_folder_and_respects_limit() {
        let pool = test_pool().await;
        let folder = create_folder(&pool, "Reports", None, "user-1")
            .await
            .unwrap();
        let in_folder = create_doc(&pool, "in.pdf", "user-1").await;
        create_doc(&pool, "out.pdf", "user-1").await;
        move_document(&pool, &in_folder.id, Some(&folder.id))
            .await
            .unwrap();

        let folder_docs = list_documents(&pool, None, Some(&folder.id), false, 100)
            .await
            .unwrap();
        assert_eq!(folder_docs.len(), 1);
        assert_eq!(folder_docs[0].id, in_folder.id);

        // LIMIT is honored.
        let limited = list_documents(&pool, None, None, false, 1).await.unwrap();
        assert_eq!(limited.len(), 1);
    }

    #[tokio::test]
    async fn search_documents_matches_substring_case_insensitively() {
        let pool = test_pool().await;
        create_doc(&pool, "Quarterly Report.pdf", "user-1").await;
        create_doc(&pool, "report-draft.txt", "user-1").await;
        create_doc(&pool, "unrelated.bin", "user-1").await;
        let trashed = create_doc(&pool, "report-trashed.pdf", "user-1").await;
        delete_document(&pool, &trashed.id).await.unwrap();

        let hits = search_documents(&pool, "REPORT", 100).await.unwrap();
        assert_eq!(hits.len(), 2);
        assert!(
            hits.iter()
                .all(|d| d.name.as_str().to_lowercase().contains("report"))
        );

        let none = search_documents(&pool, "zzz-no-match", 100).await.unwrap();
        assert!(none.is_empty());

        // Results are ordered by name and limited.
        let limited = search_documents(&pool, "report", 1).await.unwrap();
        assert_eq!(limited.len(), 1);
    }

    #[tokio::test]
    async fn search_documents_treats_like_wildcards_literally() {
        let pool = test_pool().await;
        create_doc(&pool, "100%_done.pdf", "user-1").await;
        create_doc(&pool, "100x_done.pdf", "user-1").await;

        // '%' and '_' in the query must not act as wildcards.
        let hits = search_documents(&pool, "100%_", 100).await.unwrap();
        assert_eq!(hits.len(), 1);
        assert_eq!(hits[0].name.as_str(), "100%_done.pdf");
    }

    #[tokio::test]
    async fn rename_document_updates_name_only() {
        let pool = test_pool().await;
        let doc = create_doc(&pool, "old.pdf", "user-1").await;

        let renamed = rename_document(&pool, &doc.id, "new-name.pdf")
            .await
            .unwrap();
        assert_eq!(renamed.name.as_str(), "new-name.pdf");
        assert_eq!(renamed.document_type, doc.document_type);
        assert_eq!(renamed.status, doc.status);

        let fetched = get_document(&pool, &doc.id).await.unwrap().unwrap();
        assert_eq!(fetched.name.as_str(), "new-name.pdf");

        assert!(matches!(
            rename_document(&pool, &DocumentId::new("doc-missing"), "x").await,
            Err(StoreError::NotFound { .. })
        ));
        assert!(rename_document(&pool, &doc.id, "").await.is_err());
    }

    #[tokio::test]
    async fn update_document_patch_changes_fields() {
        let pool = test_pool().await;
        let doc = create_doc(&pool, "patch.pdf", "user-1").await;

        let updated = update_document(
            &pool,
            &doc.id,
            &DocumentPatch {
                name: Some("patched.pdf".to_string()),
                document_type: Some(DocumentType::Spreadsheet),
            },
        )
        .await
        .unwrap();
        assert_eq!(updated.name.as_str(), "patched.pdf");
        assert_eq!(updated.document_type, DocumentType::Spreadsheet);

        // None fields are left unchanged.
        let name_only = update_document(
            &pool,
            &doc.id,
            &DocumentPatch {
                name: Some("patched2.pdf".to_string()),
                document_type: None,
            },
        )
        .await
        .unwrap();
        assert_eq!(name_only.document_type, DocumentType::Spreadsheet);

        assert!(matches!(
            update_document(
                &pool,
                &DocumentId::new("doc-missing"),
                &DocumentPatch::default(),
            )
            .await,
            Err(StoreError::NotFound { .. })
        ));
    }

    #[tokio::test]
    async fn move_document_into_folder_and_back_to_root() {
        let pool = test_pool().await;
        let folder = create_folder(&pool, "Inbox", None, "user-1").await.unwrap();
        let doc = create_doc(&pool, "move.pdf", "user-1").await;
        assert!(doc.folder_id.is_none());

        let moved = move_document(&pool, &doc.id, Some(&folder.id))
            .await
            .unwrap();
        assert_eq!(
            moved.folder_id.as_ref().map(|f| f.as_str()),
            Some(folder.id.as_str())
        );

        let back = move_document(&pool, &doc.id, None).await.unwrap();
        assert!(back.folder_id.is_none());

        // Missing folder is rejected.
        assert!(matches!(
            move_document(&pool, &doc.id, Some(&FolderId::new("fld-nope"))).await,
            Err(StoreError::NotFound { .. })
        ));
        // Missing document is rejected.
        assert!(matches!(
            move_document(&pool, &DocumentId::new("doc-missing"), None).await,
            Err(StoreError::NotFound { .. })
        ));
    }

    #[tokio::test]
    async fn copy_document_duplicates_row_artifacts_and_tags_not_versions() {
        let pool = test_pool().await;
        let folder = create_folder(&pool, "Shared", None, "user-1")
            .await
            .unwrap();
        let source = create_doc(&pool, "source.pdf", "user-1").await;
        move_document(&pool, &source.id, Some(&folder.id))
            .await
            .unwrap();

        let artifact = upload_artifact(
            &pool,
            &NewArtifact {
                id: Some("art-src".to_string()),
                document_id: source.id.clone(),
                storage_uri: "storage://bucket/src.pdf".to_string(),
                file_name: "src.pdf".to_string(),
                mime_type: Some("application/pdf".to_string()),
                size_bytes: 1234,
                checksum: None,
            },
        )
        .await
        .unwrap();
        add_tag(&pool, &source.id, "finance", Some("user-1"))
            .await
            .unwrap();
        create_version(&pool, &source.id, Some(&artifact.id), "user-1", Some("v1"))
            .await
            .unwrap();

        let copy = copy_document(&pool, &source.id, None).await.unwrap();

        // New identity, default "(copy)" name, same folder/owner/type.
        assert_ne!(copy.id, source.id);
        assert_eq!(copy.name.as_str(), "source.pdf (copy)");
        assert_eq!(
            copy.folder_id.as_ref().map(|f| f.as_str()),
            Some(folder.id.as_str())
        );
        assert_eq!(copy.owner_id, source.owner_id);
        assert_eq!(copy.document_type, source.document_type);
        assert_eq!(copy.status, DocumentStatus::Active);

        // Artifact rows are duplicated with fresh ids but share storage_uri.
        let copy_arts = list_artifacts(&pool, &copy.id).await.unwrap();
        assert_eq!(copy_arts.len(), 1);
        assert_ne!(copy_arts[0].id, artifact.id);
        assert_eq!(copy_arts[0].storage_uri, artifact.storage_uri);
        assert_eq!(copy_arts[0].file_name, artifact.file_name);
        assert_eq!(copy_arts[0].size_bytes, artifact.size_bytes);
        assert_eq!(list_artifacts(&pool, &source.id).await.unwrap().len(), 1);

        // Tag assignments are copied.
        let copy_tags = list_tags(&pool, &copy.id).await.unwrap();
        assert_eq!(copy_tags.len(), 1);
        assert_eq!(copy_tags[0].name, "finance");

        // Versions are NOT copied (documented semantics).
        assert!(list_versions(&pool, &copy.id).await.unwrap().is_empty());
        assert_eq!(list_versions(&pool, &source.id).await.unwrap().len(), 1);

        // Copy is a real persisted row.
        assert!(get_document(&pool, &copy.id).await.unwrap().is_some());
    }

    #[tokio::test]
    async fn copy_document_accepts_custom_name() {
        let pool = test_pool().await;
        let source = create_doc(&pool, "orig.pdf", "user-1").await;

        let copy = copy_document(&pool, &source.id, Some("renamed-copy.pdf"))
            .await
            .unwrap();
        assert_eq!(copy.name.as_str(), "renamed-copy.pdf");

        assert!(matches!(
            copy_document(&pool, &DocumentId::new("doc-missing"), None).await,
            Err(StoreError::NotFound { .. })
        ));
    }

    #[tokio::test]
    async fn delete_restore_purge_lifecycle() {
        let pool = test_pool().await;
        let doc = create_doc(&pool, "cycle.pdf", "user-1").await;

        // Cannot purge a non-trashed document.
        assert!(matches!(
            purge_document(&pool, &doc.id).await,
            Err(StoreError::Validation { .. })
        ));

        // Soft delete: status → trashed, still fetchable, hidden from
        // default listings.
        let trashed = delete_document(&pool, &doc.id).await.unwrap();
        assert_eq!(trashed.status, DocumentStatus::Trashed);
        assert!(get_document(&pool, &doc.id).await.unwrap().is_some());
        assert!(
            list_documents(&pool, None, None, false, 100)
                .await
                .unwrap()
                .is_empty()
        );
        assert_eq!(
            list_documents(&pool, None, None, true, 100)
                .await
                .unwrap()
                .len(),
            1
        );

        // Restore returns it to active and visible.
        let restored = restore_document(&pool, &doc.id).await.unwrap();
        assert_eq!(restored.status, DocumentStatus::Active);
        assert_eq!(
            list_documents(&pool, None, None, false, 100)
                .await
                .unwrap()
                .len(),
            1
        );

        // Restoring an active document is a validation error.
        assert!(matches!(
            restore_document(&pool, &doc.id).await,
            Err(StoreError::Validation { .. })
        ));

        // Delete → purge removes the row entirely.
        delete_document(&pool, &doc.id).await.unwrap();
        assert!(purge_document(&pool, &doc.id).await.unwrap());
        assert!(get_document(&pool, &doc.id).await.unwrap().is_none());

        // Purging again / missing id reports false; deleting a purged
        // id is NotFound.
        assert!(!purge_document(&pool, &doc.id).await.unwrap());
        assert!(
            !purge_document(&pool, &DocumentId::new("doc-missing"))
                .await
                .unwrap()
        );
        assert!(matches!(
            delete_document(&pool, &doc.id).await,
            Err(StoreError::NotFound { .. })
        ));
        assert!(matches!(
            restore_document(&pool, &doc.id).await,
            Err(StoreError::NotFound { .. })
        ));
    }

    #[tokio::test]
    async fn archive_and_unarchive_transitions() {
        let pool = test_pool().await;
        let doc = create_doc(&pool, "arch.pdf", "user-1").await;

        let archived = archive_document(&pool, &doc.id).await.unwrap();
        assert_eq!(archived.status, DocumentStatus::Archived);
        // Archived documents remain listed.
        assert_eq!(
            list_documents(&pool, None, None, false, 100)
                .await
                .unwrap()
                .len(),
            1
        );

        // Double archive is rejected.
        assert!(matches!(
            archive_document(&pool, &doc.id).await,
            Err(StoreError::Validation { .. })
        ));

        let unarchived = unarchive_document(&pool, &doc.id).await.unwrap();
        assert_eq!(unarchived.status, DocumentStatus::Active);

        // Unarchiving an active document is rejected.
        assert!(matches!(
            unarchive_document(&pool, &doc.id).await,
            Err(StoreError::Validation { .. })
        ));

        // Trashed documents cannot be archived — restore first.
        delete_document(&pool, &doc.id).await.unwrap();
        assert!(matches!(
            archive_document(&pool, &doc.id).await,
            Err(StoreError::Validation { .. })
        ));
    }

    #[tokio::test]
    async fn copy_of_archived_document_status() {
        let pool = test_pool().await;
        let source = create_doc(&pool, "arch-src.pdf", "user-1").await;
        archive_document(&pool, &source.id).await.unwrap();

        let copy = copy_document(&pool, &source.id, None).await.unwrap();
        // copy_document forces status = Active regardless of source
        // status — this documents the current semantics.
        assert_eq!(copy.status, DocumentStatus::Active);
    }
}
