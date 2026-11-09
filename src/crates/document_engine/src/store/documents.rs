//! Document CRUD and organization operations.

use serde_json::json;
use sqlx::{Row, SqlitePool, sqlite::SqliteRow};

use crate::crates::document_engine::audit::DocumentAuditEventType;
use crate::crates::document_engine::document::{
    Document, DocumentName, DocumentOrigin, DocumentSource, DocumentStatus, DocumentType,
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
    let row = sqlx::query(&format!("SELECT {DOC_COLS} FROM documents_tbl WHERE id = ?"))
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
        let exists: bool = sqlx::query_scalar(
            "SELECT EXISTS(SELECT 1 FROM folders_tbl WHERE id = ?)",
        )
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

    record_event(
        pool,
        id,
        None,
        DocumentAuditEventType::Trashed,
        None,
    )
    .await?;

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

