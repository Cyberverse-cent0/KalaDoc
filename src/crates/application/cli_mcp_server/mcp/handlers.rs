//! Tool handler functions — plain async adapters over
//! [`AppServices`], independent of rmcp so they can be unit-tested
//! directly. Each returns `Result<serde_json::Value, ToolFault>`; the
//! `#[tool]` wrappers in [`super::server`] convert that into MCP
//! responses.

use base64::Engine as _;
use base64::engine::general_purpose::STANDARD as B64;
use serde_json::{Value, json};

use crate::crates::application::services::app::AppServices;
use crate::crates::application::services::documents::{DocumentService, NewDocument};
use crate::crates::application::services::engines;
use crate::crates::application::services::ingestion::IngestRequest;
use crate::crates::document_engine::document::{
    Document, DocumentOrigin, DocumentSource, DocumentType,
};
use crate::crates::document_engine::ids::{DocumentId, FolderId};
use crate::crates::job_engine::{self, JobStatus, NewJob};
use crate::crates::pipeline_engine::builtin_definitions;

use super::{DEFAULT_OWNER, MAX_INGEST_BYTES, ToolFault, job_view};

// ---------------------------------------------------------------------------
// Shared helpers
// ---------------------------------------------------------------------------

fn doc_id(id: &str) -> DocumentId {
    DocumentId::new(id)
}

fn folder_id(id: &str) -> FolderId {
    FolderId::new(id)
}

fn opt_folder(id: Option<&str>) -> Option<FolderId> {
    id.map(folder_id)
}

/// Ensures `id` names an existing document so doc-scoped tools report
/// `not_found` consistently instead of an empty list.
async fn require_document(docs: &DocumentService, id: &DocumentId) -> Result<Document, ToolFault> {
    docs.get_document(id)
        .await?
        .ok_or_else(|| ToolFault::not_found(format!("document not found: {}", id.as_str())))
}

/// Positive-or-default limit guard shared by list/search tools.
fn check_limit(limit: i64) -> Result<i64, ToolFault> {
    if limit <= 0 {
        return Err(ToolFault::validation("limit must be positive"));
    }
    Ok(limit)
}

/// Parses a document type name, falling back to extension inference —
/// same vocabulary as `kaladoc doc create --type`.
fn resolve_document_type(name: &str, explicit: Option<&str>) -> Result<DocumentType, ToolFault> {
    if let Some(t) = explicit {
        return match t {
            "pdf" => Ok(DocumentType::Pdf),
            "image" => Ok(DocumentType::Image),
            "spreadsheet" => Ok(DocumentType::Spreadsheet),
            "presentation" => Ok(DocumentType::Presentation),
            "text" => Ok(DocumentType::Text),
            "archive" => Ok(DocumentType::Archive),
            "other" => Ok(DocumentType::Other),
            other => Err(ToolFault::validation(format!(
                "invalid document_type {other:?}; expected pdf, image, spreadsheet, \
                 presentation, text, archive, or other"
            ))),
        };
    }
    let ext = name
        .rsplit_once('.')
        .map(|(_, e)| e.to_ascii_lowercase())
        .unwrap_or_default();
    Ok(match ext.as_str() {
        "pdf" => DocumentType::Pdf,
        "png" | "jpg" | "jpeg" | "gif" | "webp" | "bmp" | "tiff" | "svg" => DocumentType::Image,
        "xls" | "xlsx" | "ods" | "csv" | "tsv" => DocumentType::Spreadsheet,
        "ppt" | "pptx" | "odp" | "key" => DocumentType::Presentation,
        "txt" | "md" | "markdown" | "log" | "rtf" => DocumentType::Text,
        "zip" | "tar" | "gz" | "tgz" | "bz2" | "xz" | "7z" | "rar" => DocumentType::Archive,
        _ => DocumentType::Other,
    })
}

fn owner_or_default(owner: Option<&str>) -> String {
    owner
        .map(str::trim)
        .filter(|s| !s.is_empty())
        .unwrap_or(DEFAULT_OWNER)
        .to_string()
}

// ---------------------------------------------------------------------------
// System / status tools
// ---------------------------------------------------------------------------

/// `kaladoc.system_info` — product version and advertised capabilities.
pub async fn system_info() -> Result<Value, ToolFault> {
    Ok(json!({
        "name": "kaladoc",
        "version": env!("CARGO_PKG_VERSION"),
        "transport": "stdio",
        "capabilities": {
            "documents": true,
            "folders": true,
            "ingest": true,
            "jobs": ["enqueue", "list", "cancel"],
            "engines": true,
            "storage": true,
        },
    }))
}

/// `kaladoc.system_health` — storage status plus a database ping.
pub async fn system_health(services: &AppServices) -> Result<Value, ToolFault> {
    // `SELECT 1` proves the pool is live without touching any table.
    sqlx::query("SELECT 1")
        .fetch_one(services.pool())
        .await
        .map_err(|e| ToolFault::internal(format!("database ping failed: {e}")))?;
    Ok(json!({
        "status": "ok",
        "storage": services.storage().status(),
        "database": "ok",
    }))
}

/// `kaladoc.engine_list` — registered engines with operations and
/// advertised formats.
pub async fn engine_list() -> Result<Value, ToolFault> {
    let registry = engines::builtin_registry();
    Ok(json!({ "engines": registry.list() }))
}

/// `kaladoc.engine_health` — per-engine health report.
pub async fn engine_health() -> Result<Value, ToolFault> {
    let registry = engines::builtin_registry();
    Ok(json!({ "health": registry.health_all().await }))
}

/// `kaladoc.storage_status` — storage backend health snapshot (no
/// filesystem paths are exposed by design).
pub async fn storage_status(services: &AppServices) -> Result<Value, ToolFault> {
    Ok(json!(services.storage().status()))
}

// ---------------------------------------------------------------------------
// Document discovery
// ---------------------------------------------------------------------------

/// `kaladoc.document_list` — documents, newest first.
#[allow(clippy::too_many_arguments)]
pub async fn document_list(
    services: &AppServices,
    owner_id: Option<&str>,
    folder_id: Option<&str>,
    include_deleted: bool,
    limit: i64,
) -> Result<Value, ToolFault> {
    let limit = check_limit(limit)?;
    let docs = services
        .documents()
        .list_documents(
            owner_id,
            opt_folder(folder_id).as_ref(),
            include_deleted,
            limit,
        )
        .await?;
    Ok(json!({ "documents": docs, "count": docs.len() }))
}

/// `kaladoc.document_search` — case-insensitive name substring search.
pub async fn document_search(
    services: &AppServices,
    query: &str,
    limit: i64,
) -> Result<Value, ToolFault> {
    let query = query.trim();
    if query.is_empty() {
        return Err(ToolFault::validation("query must not be empty"));
    }
    let limit = check_limit(limit)?;
    let docs = services.documents().search_documents(query, limit).await?;
    Ok(json!({ "documents": docs, "count": docs.len() }))
}

/// `kaladoc.document_get` — document plus its artifacts, tags, and
/// metadata row.
pub async fn document_get(services: &AppServices, id: &str) -> Result<Value, ToolFault> {
    let id = doc_id(id);
    let docs = services.documents();
    let document = require_document(docs, &id).await?;
    let artifacts = docs.list_artifacts(&id).await?;
    let tags = docs.list_tags(&id).await?;
    let metadata = docs.get_document_metadata(&id).await?;
    Ok(json!({
        "document": document,
        "artifacts": artifacts,
        "tags": tags,
        "metadata": metadata,
    }))
}

/// `kaladoc.document_history` — the document's audit trail, oldest
/// first.
pub async fn document_history(services: &AppServices, id: &str) -> Result<Value, ToolFault> {
    let id = doc_id(id);
    let docs = services.documents();
    require_document(docs, &id).await?;
    let events = docs.get_document_history(&id).await?;
    Ok(json!({ "events": events, "count": events.len() }))
}

/// `kaladoc.document_versions` — all versions, newest first.
pub async fn document_versions(services: &AppServices, id: &str) -> Result<Value, ToolFault> {
    let id = doc_id(id);
    let docs = services.documents();
    require_document(docs, &id).await?;
    let versions = docs.list_versions(&id).await?;
    Ok(json!({ "versions": versions, "count": versions.len() }))
}

/// `kaladoc.document_artifacts` — artifact rows (storage pointers,
/// checksums) for a document, oldest first.
pub async fn document_artifacts(services: &AppServices, id: &str) -> Result<Value, ToolFault> {
    let id = doc_id(id);
    let docs = services.documents();
    require_document(docs, &id).await?;
    let artifacts = docs.list_artifacts(&id).await?;
    Ok(json!({ "artifacts": artifacts, "count": artifacts.len() }))
}

/// `kaladoc.document_tags` — tags assigned to a document.
pub async fn document_tags(services: &AppServices, id: &str) -> Result<Value, ToolFault> {
    let id = doc_id(id);
    let docs = services.documents();
    require_document(docs, &id).await?;
    let tags = docs.list_tags(&id).await?;
    Ok(json!({ "tags": tags, "count": tags.len() }))
}

// ---------------------------------------------------------------------------
// Document mutation
// ---------------------------------------------------------------------------

/// `kaladoc.document_create` — registers document metadata (no bytes;
/// use `document_ingest` to upload content).
#[allow(clippy::too_many_arguments)]
pub async fn document_create(
    services: &AppServices,
    name: &str,
    owner_id: Option<&str>,
    document_type: Option<&str>,
    folder_id: Option<&str>,
) -> Result<Value, ToolFault> {
    let name = name.trim();
    if name.is_empty() {
        return Err(ToolFault::validation("name must not be empty"));
    }
    let doc_type = resolve_document_type(name, document_type)?;
    let owner = owner_or_default(owner_id);
    let docs = services.documents();

    let document = docs
        .create_document(&NewDocument {
            name: name.to_string(),
            document_type: doc_type,
            owner_id: owner,
            source: DocumentSource::new(DocumentOrigin::Upload),
        })
        .await?;

    // `NewDocument` has no folder field; place it explicitly when asked.
    let document = match opt_folder(folder_id) {
        Some(fid) => docs.move_document(&document.id, Some(&fid)).await?,
        None => document,
    };
    Ok(json!({ "document": document }))
}

/// `kaladoc.document_ingest` — uploads base64-encoded bytes through the
/// real ingestion pipeline (staged upload → SHA-256 commit → document,
/// artifact, version 1, metadata, audit).
#[allow(clippy::too_many_arguments)]
pub async fn document_ingest(
    services: &AppServices,
    file_name: &str,
    content_base64: &str,
    owner_id: Option<&str>,
    mime_type: Option<&str>,
    folder_id: Option<&str>,
    tags: Option<Vec<String>>,
) -> Result<Value, ToolFault> {
    let file_name = file_name.trim();
    if file_name.is_empty() {
        return Err(ToolFault::validation("file_name must not be empty"));
    }
    let bytes = B64
        .decode(content_base64)
        .map_err(|e| ToolFault::validation(format!("content_base64 is not valid base64: {e}")))?;
    if bytes.is_empty() {
        return Err(ToolFault::validation("content must not be empty"));
    }
    if bytes.len() as u64 > MAX_INGEST_BYTES {
        return Err(ToolFault::validation(format!(
            "content exceeds the {} MiB ingest limit",
            MAX_INGEST_BYTES / (1024 * 1024)
        )));
    }

    let owner = owner_or_default(owner_id);
    let mut request = IngestRequest::new(owner.clone(), file_name);
    request.mime_type = mime_type.map(str::to_string);
    request.folder_id = opt_folder(folder_id);
    request.tags = tags.unwrap_or_default();
    request.actor_id = Some(owner);

    let outcome = services.ingestion().ingest(&bytes[..], request).await?;
    Ok(json!({
        "document": outcome.document,
        "artifact": outcome.artifact,
        "version": outcome.version,
        "storage_uri": outcome.storage_uri,
        "size_bytes": outcome.size_bytes,
        "sha256": outcome.sha256,
        "detected_mime_type": outcome.detected_mime_type,
    }))
}

/// `kaladoc.document_rename`.
pub async fn document_rename(
    services: &AppServices,
    id: &str,
    name: &str,
) -> Result<Value, ToolFault> {
    let id = doc_id(id);
    let docs = services.documents();
    require_document(docs, &id).await?;
    let document = docs.rename_document(&id, name).await?;
    Ok(json!({ "document": document }))
}

/// `kaladoc.document_move` — `folder_id: null` moves to the root.
pub async fn document_move(
    services: &AppServices,
    id: &str,
    folder_id: Option<&str>,
) -> Result<Value, ToolFault> {
    let id = doc_id(id);
    let docs = services.documents();
    require_document(docs, &id).await?;
    let fid = opt_folder(folder_id);
    let document = docs.move_document(&id, fid.as_ref()).await?;
    Ok(json!({ "document": document }))
}

/// `kaladoc.document_delete` — soft-delete (trash); restorable via
/// `document_restore`.
pub async fn document_delete(services: &AppServices, id: &str) -> Result<Value, ToolFault> {
    let id = doc_id(id);
    let docs = services.documents();
    require_document(docs, &id).await?;
    let document = docs.delete_document(&id).await?;
    Ok(json!({ "document": document }))
}

/// `kaladoc.document_restore` — restore a trashed document.
pub async fn document_restore(services: &AppServices, id: &str) -> Result<Value, ToolFault> {
    let id = doc_id(id);
    let docs = services.documents();
    require_document(docs, &id).await?;
    let document = docs.restore_document(&id).await?;
    Ok(json!({ "document": document }))
}

/// `kaladoc.document_restore_version` — moves the current-version
/// pointer to `version_number`.
pub async fn document_restore_version(
    services: &AppServices,
    id: &str,
    version_number: u32,
) -> Result<Value, ToolFault> {
    let id = doc_id(id);
    let docs = services.documents();
    require_document(docs, &id).await?;
    let document = docs.restore_version(&id, version_number).await?;
    Ok(json!({ "document": document }))
}

// ---------------------------------------------------------------------------
// Folders
// ---------------------------------------------------------------------------

/// `kaladoc.folder_list` — folders for an owner; `parent_id` selects
/// direct children (`null` = roots).
pub async fn folder_list(
    services: &AppServices,
    owner_id: Option<&str>,
    parent_id: Option<&str>,
) -> Result<Value, ToolFault> {
    let folders = services
        .documents()
        .list_folders(owner_id, opt_folder(parent_id).as_ref())
        .await?;
    Ok(json!({ "folders": folders, "count": folders.len() }))
}

/// `kaladoc.folder_create`.
pub async fn folder_create(
    services: &AppServices,
    name: &str,
    owner_id: Option<&str>,
    parent_id: Option<&str>,
) -> Result<Value, ToolFault> {
    let name = name.trim();
    if name.is_empty() {
        return Err(ToolFault::validation("name must not be empty"));
    }
    let owner = owner_or_default(owner_id);
    let pid = opt_folder(parent_id);
    let folder = services
        .documents()
        .create_folder(name, pid.as_ref(), &owner)
        .await?;
    Ok(json!({ "folder": folder }))
}

// ---------------------------------------------------------------------------
// Jobs
// ---------------------------------------------------------------------------

/// `kaladoc.job_enqueue` — enqueues any registered job kind. `payload`
/// is an arbitrary JSON object handed to the handler verbatim.
#[allow(clippy::too_many_arguments)]
pub async fn job_enqueue(
    services: &AppServices,
    kind: &str,
    payload: Option<Value>,
    priority: i64,
    run_at: Option<i64>,
    max_attempts: Option<u32>,
    default_max_attempts: u32,
) -> Result<Value, ToolFault> {
    let kind = kind.trim();
    if kind.is_empty() {
        return Err(ToolFault::validation("kind must not be empty"));
    }
    let payload = payload.unwrap_or_else(|| json!({}));
    if !payload.is_object() {
        return Err(ToolFault::validation("payload must be a JSON object"));
    }
    let job = job_engine::enqueue(
        services.pool(),
        &NewJob {
            kind: kind.to_string(),
            payload,
            priority,
            run_at,
            max_attempts,
        },
        default_max_attempts,
    )
    .await?;
    Ok(json!({ "job": job_view(&job) }))
}

/// `kaladoc.job_list` — jobs newest first, optionally filtered by
/// status (`pending|running|succeeded|failed|cancelled|skipped`).
pub async fn job_list(
    services: &AppServices,
    status: Option<&str>,
    limit: i64,
) -> Result<Value, ToolFault> {
    let status = match status {
        Some(s) => Some(JobStatus::parse(s).ok_or_else(|| {
            ToolFault::validation(format!(
                "invalid status {s:?}; expected pending, running, succeeded, \
                 failed, cancelled, or skipped"
            ))
        })?),
        None => None,
    };
    let limit = check_limit(limit)?;
    let jobs = job_engine::list_jobs(services.pool(), status, limit).await?;
    let views: Vec<Value> = jobs.iter().map(job_view).collect();
    Ok(json!({ "jobs": views, "count": views.len() }))
}

/// `kaladoc.job_cancel` — cancels a pending or running job. Returns
/// `cancelled: false` when the job is missing or already terminal.
pub async fn job_cancel(services: &AppServices, id: i64) -> Result<Value, ToolFault> {
    let cancelled = job_engine::cancel_job(services.pool(), id).await?;
    Ok(json!({ "id": id, "cancelled": cancelled }))
}

// ---------------------------------------------------------------------------
// Pipelines (read-only listing; runs go through the job queue)
// ---------------------------------------------------------------------------

/// `kaladoc.pipeline_list` — built-in pipeline definitions.
pub async fn pipeline_list() -> Result<Value, ToolFault> {
    Ok(json!({ "pipelines": builtin_definitions() }))
}

/// `kaladoc.pipeline_run` — enqueues a `pipeline.run` job for a
/// built-in pipeline (`pdf.intake`, `pdf.extract-text`) against a
/// document's latest artifact. Returns the job id; poll `job_list`.
pub async fn pipeline_run(
    services: &AppServices,
    document_id: &str,
    pipeline: &str,
    artifact_id: Option<&str>,
) -> Result<Value, ToolFault> {
    let id = doc_id(document_id);
    let docs = services.documents();
    require_document(docs, &id).await?;

    let definition = builtin_definitions()
        .into_iter()
        .find(|def| def.name == pipeline)
        .ok_or_else(|| {
            let names: Vec<String> = builtin_definitions()
                .iter()
                .map(|d| d.name.clone())
                .collect();
            ToolFault::validation(format!(
                "unknown pipeline {pipeline:?}; available: {}",
                names.join(", ")
            ))
        })?;

    let job = crate::crates::application::job_handlers::enqueue_pipeline_run(
        services.pool(),
        &id,
        artifact_id,
        &definition,
    )
    .await?;
    Ok(json!({ "job": job_view(&job), "pipeline": definition.name }))
}

// ---------------------------------------------------------------------------
// Tests — handlers run against a real AppServices on temp storage+sqlite,
// same construction as `services::app` tests.
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::AppConfig;
    use std::sync::Arc;

    async fn test_services() -> (Arc<AppServices>, tempfile::TempDir) {
        let dir = tempfile::tempdir().expect("tempdir");
        let mut config = AppConfig::default();
        config.database.path = dir.path().join("app.db");
        config.storage.root_dir = dir.path().join("storage");
        let services = AppServices::init(&config).await.expect("AppServices::init");
        (Arc::new(services), dir)
    }

    #[tokio::test]
    async fn status_tools_return_ok() {
        let (services, _dir) = test_services().await;
        assert_eq!(system_info().await.unwrap()["name"], "kaladoc");
        let health = system_health(&services).await.unwrap();
        assert_eq!(health["status"], "ok");
        assert_eq!(health["database"], "ok");
        let storage = storage_status(&services).await.unwrap();
        assert_eq!(storage["backend"], "local");
        let engines = engine_list().await.unwrap();
        assert!(engines["engines"].as_array().unwrap().len() >= 6);
        let health = engine_health().await.unwrap();
        assert!(health["health"].as_array().unwrap().len() >= 6);
    }

    #[tokio::test]
    async fn document_crud_roundtrip() {
        let (services, _dir) = test_services().await;

        let created = document_create(&services, "report.pdf", None, None, None)
            .await
            .unwrap();
        let doc = &created["document"];
        let id = doc["id"].as_str().unwrap().to_string();
        assert_eq!(doc["name"], "report.pdf");
        assert_eq!(doc["owner_id"], DEFAULT_OWNER);

        // get returns document + artifacts + tags + metadata keys.
        let got = document_get(&services, &id).await.unwrap();
        assert_eq!(got["document"]["id"], id);
        assert!(got["artifacts"].is_array());
        assert!(got["tags"].is_array());

        // list + search find it.
        let list = document_list(&services, None, None, false, 50)
            .await
            .unwrap();
        assert_eq!(list["count"], 1);
        let search = document_search(&services, "repo", 20).await.unwrap();
        assert_eq!(search["count"], 1);

        // rename / move-to-root / trash / restore.
        let renamed = document_rename(&services, &id, "final.pdf").await.unwrap();
        assert_eq!(renamed["document"]["name"], "final.pdf");
        document_move(&services, &id, None).await.unwrap();
        document_delete(&services, &id).await.unwrap();
        // Trashed docs are hidden from default list.
        let list = document_list(&services, None, None, false, 50)
            .await
            .unwrap();
        assert_eq!(list["count"], 0);
        let restored = document_restore(&services, &id).await.unwrap();
        assert_eq!(restored["document"]["status"], "active");

        // versions/artifacts/tags/history are listable.
        assert_eq!(document_versions(&services, &id).await.unwrap()["count"], 0);
        assert_eq!(
            document_artifacts(&services, &id).await.unwrap()["count"],
            0
        );
        assert_eq!(document_tags(&services, &id).await.unwrap()["count"], 0);
        let history = document_history(&services, &id).await.unwrap();
        assert!(history["count"].as_u64().unwrap() >= 1);
    }

    #[tokio::test]
    async fn document_ingest_stores_bytes() {
        let (services, dir) = test_services().await;
        let payload = b"hello kaladoc".to_vec();
        let b64 = B64.encode(&payload);

        let out = document_ingest(
            &services,
            "note.txt",
            &b64,
            Some("user-1"),
            Some("text/plain"),
            None,
            Some(vec!["misc".to_string()]),
        )
        .await
        .unwrap();

        assert_eq!(out["document"]["name"], "note.txt");
        assert_eq!(out["document"]["owner_id"], "user-1");
        assert_eq!(out["size_bytes"], payload.len() as u64);
        assert_eq!(out["version"]["version_number"], 1);

        // Bytes really landed under the configured storage root.
        let object = format!(
            "documents/{}/note.txt",
            out["document"]["id"].as_str().unwrap()
        );
        assert!(dir.path().join("storage").join(&object).exists());

        // Bad base64 and oversized payloads are validation errors.
        let err = document_ingest(&services, "x.bin", "!!!", None, None, None, None)
            .await
            .unwrap_err();
        assert_eq!(err.kind(), "validation_failed");
        let big = B64.encode(vec![0u8; (MAX_INGEST_BYTES + 1) as usize]);
        let err = document_ingest(&services, "big.bin", &big, None, None, None, None)
            .await
            .unwrap_err();
        assert_eq!(err.kind(), "validation_failed");
    }

    #[tokio::test]
    async fn missing_document_is_not_found() {
        let (services, _dir) = test_services().await;
        let err = document_get(&services, "doc-nope").await.unwrap_err();
        assert_eq!(err.kind(), "not_found");
        let err = document_rename(&services, "doc-nope", "x")
            .await
            .unwrap_err();
        assert_eq!(err.kind(), "not_found");
        let err = document_versions(&services, "doc-nope").await.unwrap_err();
        assert_eq!(err.kind(), "not_found");
    }

    #[tokio::test]
    async fn folders_and_jobs_roundtrip() {
        let (services, _dir) = test_services().await;

        let folder = folder_create(&services, "Invoices", None, None)
            .await
            .unwrap();
        let fid = folder["folder"]["id"].as_str().unwrap().to_string();

        let list = folder_list(&services, None, None).await.unwrap();
        assert_eq!(list["count"], 1);
        let children = folder_list(&services, None, Some(&fid)).await.unwrap();
        assert_eq!(children["count"], 0);

        // Enqueue + list + cancel.
        let enq = job_enqueue(
            &services,
            "system.ping",
            Some(json!({"probe": true})),
            5,
            None,
            Some(2),
            3,
        )
        .await
        .unwrap();
        let job_id = enq["job"]["id"].as_i64().unwrap();
        assert_eq!(enq["job"]["kind"], "system.ping");
        assert_eq!(enq["job"]["max_attempts"], 2);

        let list = job_list(&services, Some("pending"), 50).await.unwrap();
        assert_eq!(list["count"], 1);
        let cancelled = job_cancel(&services, job_id).await.unwrap();
        assert_eq!(cancelled["cancelled"], true);
        // Second cancel reports false (already terminal).
        let again = job_cancel(&services, job_id).await.unwrap();
        assert_eq!(again["cancelled"], false);

        // Bad status is a validation error.
        let err = job_list(&services, Some("bogus"), 10).await.unwrap_err();
        assert_eq!(err.kind(), "validation_failed");
    }

    #[tokio::test]
    async fn pipeline_tools_list_and_enqueue() {
        let (services, _dir) = test_services().await;
        let list = pipeline_list().await.unwrap();
        let names: Vec<&str> = list["pipelines"]
            .as_array()
            .unwrap()
            .iter()
            .map(|p| p["name"].as_str().unwrap())
            .collect();
        assert!(names.contains(&"pdf.intake"));

        // Unknown pipeline → validation error; missing doc → not_found.
        let err = pipeline_run(&services, "doc-x", "nope", None)
            .await
            .unwrap_err();
        assert_eq!(err.kind(), "not_found");
        let created = document_create(&services, "a.pdf", None, None, None)
            .await
            .unwrap();
        let id = created["document"]["id"].as_str().unwrap().to_string();
        let err = pipeline_run(&services, &id, "nope", None)
            .await
            .unwrap_err();
        assert_eq!(err.kind(), "validation_failed");

        let run = pipeline_run(&services, &id, "pdf.intake", None)
            .await
            .unwrap();
        assert_eq!(run["job"]["kind"], "pipeline.run");
        assert_eq!(run["pipeline"], "pdf.intake");
    }
}
