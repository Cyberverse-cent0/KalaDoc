//! Route handlers — thin adapters over [`ApiState`] services.
//!
//! Handlers do no validation or business logic of their own: upload
//! checks go through `security_engine::validate_upload`, persistence
//! through `services.ingestion()`/`services.documents()`, queue state
//! through `job_engine`, and health through `EngineRegistry` +
//! `observability` aggregation.

use std::io::Read;
use std::sync::Arc;

use axum::Json;
use axum::body::Body;
use axum::extract::{Extension, Path, Query, State};
use axum::http::{HeaderMap, StatusCode, header};
use axum::response::Response;
use serde::Deserialize;
use serde_json::{Value, json};

use crate::crates::application::job_handlers;
use crate::crates::application::services::ingestion::IngestRequest;
use crate::crates::document_engine::document::DocumentType;
use crate::crates::document_engine::ids::{DocumentId, FolderId};
use crate::crates::document_engine::store::documents::DocumentPatch;
use crate::crates::job_engine::{self, Job, JobStatus, cancel_job, retry_job};
use crate::crates::observability::{ComponentHealth, HealthStatus, OverallHealth, aggregate};
use crate::crates::security_engine::validate_upload;
use crate::crates::storage::local::debug_when_full::FaultInjectingFs;
use crate::crates::storage::stream::StreamingBackend;

use super::auth::AuthUser;
use super::state::ApiState;
use super::{ApiError, json_body};

/// `list_jobs` has no by-id lookup (job_engine is below this layer and
/// out of scope for changes); scan the newest jobs instead. 10 000 is
/// generous for a scaffold — revisit if the queue grows past it.
const JOB_LOOKUP_LIMIT: i64 = 10_000;

/// Default page size for collection endpoints.
const DEFAULT_LIST_LIMIT: i64 = 100;
/// Hard cap on `limit` query parameters.
const MAX_LIST_LIMIT: i64 = 1_000;
/// Largest artifact the download endpoint will buffer into a response.
const MAX_ARTIFACT_DOWNLOAD_BYTES: u64 = 256 * 1024 * 1024;

// ---------------------------------------------------------------------------
// GET /health — public
// ---------------------------------------------------------------------------

/// Aggregated engine health. Always reports per-component detail; the
/// HTTP status is `503` only when the overall status is `unavailable`.
pub async fn health(State(state): State<Arc<ApiState>>) -> (StatusCode, Json<Value>) {
    let entries = state.engines.health_all().await;
    let components: Vec<ComponentHealth> = entries
        .into_iter()
        .map(|entry| {
            ComponentHealth::from_engine_health(
                entry.name,
                crate::crates::core::engine::EngineHealth {
                    status: entry.status,
                    message: entry.message,
                },
            )
        })
        .collect();
    let overall: OverallHealth = aggregate(components);
    let code = if overall.status == HealthStatus::Unavailable {
        StatusCode::SERVICE_UNAVAILABLE
    } else {
        StatusCode::OK
    };
    (code, json_body(&overall))
}

// ---------------------------------------------------------------------------
// POST /documents — authenticated upload
// ---------------------------------------------------------------------------

/// Query parameters for [`create_document`]. The filename may also come
/// from the `x-file-name` header; `upload.bin` is the last resort.
#[derive(Debug, Deserialize)]
pub struct CreateDocumentQuery {
    filename: Option<String>,
}

/// Accepts raw request-body bytes as a document upload.
///
/// Filename resolution order: `?filename=` query param →
/// `x-file-name` header → `"upload.bin"`. The declared `Content-Type`
/// header is recorded on the artifact as caller-declared metadata (the
/// *detected* type always comes from magic bytes inside the ingestion
/// pipeline).
///
/// Pipeline: read bounded body → `security_engine::validate_upload` →
/// `DocumentIngestionService::ingest` (staged streaming write +
/// document/artifact/version registration) → best-effort
/// `document.metadata_extract` job enqueue.
pub async fn create_document(
    State(state): State<Arc<ApiState>>,
    Extension(user): Extension<AuthUser>,
    headers: HeaderMap,
    Query(query): Query<CreateDocumentQuery>,
    body: Body,
) -> Result<(StatusCode, Json<Value>), ApiError> {
    let filename = query
        .filename
        .filter(|s| !s.trim().is_empty())
        .or_else(|| {
            headers
                .get("x-file-name")
                .and_then(|v| v.to_str().ok())
                .map(str::to_string)
                .filter(|s| !s.trim().is_empty())
        })
        .unwrap_or_else(|| "upload.bin".to_string());

    let mime_type = headers
        .get(header::CONTENT_TYPE)
        .and_then(|v| v.to_str().ok())
        .map(str::trim)
        .filter(|s| !s.is_empty())
        .map(str::to_string);

    // Read at most max_upload_bytes + 1 so `validate_upload` (not the
    // body reader) is the authority on the size limit and reports a
    // proper 413 via SecurityError::FileTooLarge.
    let limit = usize::try_from(state.security_limits.max_upload_bytes)
        .unwrap_or(usize::MAX)
        .saturating_add(1);
    let bytes = axum::body::to_bytes(body, limit)
        .await
        .map_err(|_| ApiError::payload_too_large("request body exceeds the upload size limit"))?;

    let report = validate_upload(
        &filename,
        bytes.len() as u64,
        &bytes,
        &state.security_limits,
        None,
    )?;

    let outcome = state
        .services
        .ingestion()
        .ingest(
            &bytes[..],
            IngestRequest {
                owner_id: user.user_id.to_string(),
                file_name: report.sanitized_filename.clone(),
                mime_type,
                folder_id: None,
                tags: Vec::new(),
                actor_id: Some(user.user_id.to_string()),
                title: None,
            },
        )
        .await?;

    // Best-effort follow-up processing; the document is already
    // registered, so a queue failure must not fail the upload.
    let processing_job = match job_handlers::enqueue_metadata_extract(
        state.services.pool(),
        &outcome.document.id,
        Some(&outcome.artifact.id),
    )
    .await
    {
        Ok(job) => Some(job_to_json(&job)),
        Err(e) => {
            tracing::warn!(error = %e, document_id = %outcome.document.id, "failed to enqueue metadata extraction");
            None
        }
    };

    Ok((
        StatusCode::CREATED,
        Json(json!({
            "document": outcome.document,
            "artifact": outcome.artifact,
            "version": outcome.version,
            "storage_uri": outcome.storage_uri,
            "size_bytes": outcome.size_bytes,
            "sha256": outcome.sha256,
            "detected_mime_type": outcome.detected_mime_type,
            "processing_job": processing_job,
        })),
    ))
}

// ---------------------------------------------------------------------------
// GET /documents/{id} — authenticated
// ---------------------------------------------------------------------------

/// Returns document metadata plus its artifacts (and stored metadata
/// row, when present). `404` when the id is unknown.
pub async fn get_document(
    State(state): State<Arc<ApiState>>,
    Extension(_user): Extension<AuthUser>,
    Path(id): Path<String>,
) -> Result<Json<Value>, ApiError> {
    let document_id = DocumentId::new(id.clone());
    let documents = state.services.documents();

    let document = documents
        .get_document(&document_id)
        .await?
        .ok_or_else(|| ApiError::not_found("document", id.clone()))?;
    let artifacts = documents.list_artifacts(&document_id).await?;
    let metadata = documents.get_document_metadata(&document_id).await?;

    Ok(Json(json!({
        "document": document,
        "artifacts": artifacts,
        "metadata": metadata,
    })))
}

// ---------------------------------------------------------------------------
// GET /jobs/{id} — authenticated
// ---------------------------------------------------------------------------

/// Returns one job's status/payload from the durable queue. `404` when
/// no job has that id.
pub async fn get_job(
    State(state): State<Arc<ApiState>>,
    Extension(_user): Extension<AuthUser>,
    Path(id): Path<String>,
) -> Result<Json<Value>, ApiError> {
    let job_id: i64 = id
        .parse()
        .map_err(|_| ApiError::bad_request("job id must be an integer"))?;

    let jobs = job_engine::list_jobs(state.services.pool(), None, JOB_LOOKUP_LIMIT).await?;
    let job = jobs
        .into_iter()
        .find(|j| j.id == job_id)
        .ok_or_else(|| ApiError::not_found("job", id))?;

    Ok(Json(job_to_json(&job)))
}

// ---------------------------------------------------------------------------
// GET /engines — authenticated engine registry report
// ---------------------------------------------------------------------------

/// Engine registry listing plus per-engine health — the same shape as
/// `services::engines::engine_report`, built from the state's registry.
pub async fn engines(State(state): State<Arc<ApiState>>) -> Json<Value> {
    Json(json!({
        "engines": state.engines.list(),
        "health": state.engines.health_all().await,
    }))
}

// ---------------------------------------------------------------------------
// GET /storage — authenticated storage status
// ---------------------------------------------------------------------------

/// Storage backend snapshot (kind, root configured, usage, quota).
/// Contains no absolute paths.
pub async fn storage_status(State(state): State<Arc<ApiState>>) -> Json<Value> {
    let s = state.services.storage().status();
    Json(json!({
        "backend": s.backend,
        "root_configured": s.root_configured,
        "used_bytes": s.used_bytes,
        "quota_bytes": s.quota_bytes,
    }))
}

// ---------------------------------------------------------------------------
// GET /documents — authenticated list / search
// ---------------------------------------------------------------------------

/// Query parameters for [`list_documents`].
#[derive(Debug, Deserialize)]
pub struct ListDocumentsQuery {
    /// Case-insensitive name substring; switches to search mode.
    q: Option<String>,
    /// Restrict to an owner. Defaults to the caller's user id.
    owner_id: Option<String>,
    /// Restrict to a folder.
    folder_id: Option<String>,
    /// Include trashed documents.
    include_deleted: Option<bool>,
    limit: Option<i64>,
}

/// Lists the caller's documents, or runs a name search when `q` is
/// given. Newest first, capped at [`MAX_LIST_LIMIT`].
pub async fn list_documents(
    State(state): State<Arc<ApiState>>,
    Extension(user): Extension<AuthUser>,
    Query(query): Query<ListDocumentsQuery>,
) -> Result<Json<Value>, ApiError> {
    let limit = bounded_limit(query.limit);
    let documents = state.services.documents();

    if let Some(q) = query.q.filter(|s| !s.trim().is_empty()) {
        let found = documents.search_documents(q.trim(), limit).await?;
        return Ok(Json(json!({ "documents": found })));
    }

    let owner_id = query
        .owner_id
        .filter(|s| !s.trim().is_empty())
        .unwrap_or_else(|| user.user_id.to_string());
    let folder_id = query.folder_id.filter(|s| !s.is_empty()).map(FolderId::new);
    let found = documents
        .list_documents(
            Some(&owner_id),
            folder_id.as_ref(),
            query.include_deleted.unwrap_or(false),
            limit,
        )
        .await?;
    Ok(Json(json!({ "documents": found })))
}

// ---------------------------------------------------------------------------
// PATCH /documents/{id} — rename / retype / move
// ---------------------------------------------------------------------------

/// Request body for [`update_document`]. `folder_id` distinguishes
/// "absent" (unchanged) from `null` (move to root) via the
/// `present_null` deserializer.
#[derive(Debug, Deserialize)]
pub struct UpdateDocumentRequest {
    name: Option<String>,
    document_type: Option<DocumentType>,
    #[serde(default, deserialize_with = "present_null")]
    folder_id: Option<Option<String>>,
}

/// Deserializes a field that may be absent, `null`, or a string:
/// absent → `None`, `null` → `Some(None)`, `"x"` → `Some(Some("x"))`.
fn present_null<'de, D>(deserializer: D) -> Result<Option<Option<String>>, D::Error>
where
    D: serde::Deserializer<'de>,
{
    Ok(Some(Option::<String>::deserialize(deserializer)?))
}

/// Applies name/type changes, then the folder move when `folder_id` was
/// present in the body. Returns the updated document.
pub async fn update_document(
    State(state): State<Arc<ApiState>>,
    Extension(_user): Extension<AuthUser>,
    Path(id): Path<String>,
    Json(body): Json<UpdateDocumentRequest>,
) -> Result<Json<Value>, ApiError> {
    let document_id = DocumentId::new(id.clone());
    let documents = state.services.documents();

    // Verify existence up front so a pure folder move on a missing
    // document still 404s instead of silently succeeding.
    documents
        .get_document(&document_id)
        .await?
        .ok_or_else(|| ApiError::not_found("document", id.clone()))?;

    let mut document = if body.name.is_some() || body.document_type.is_some() {
        documents
            .update_document(
                &document_id,
                &DocumentPatch {
                    name: body.name,
                    document_type: body.document_type,
                },
            )
            .await?
    } else {
        documents
            .get_document(&document_id)
            .await?
            .ok_or_else(|| ApiError::not_found("document", id.clone()))?
    };

    if let Some(folder) = body.folder_id {
        let folder_id = folder.filter(|s| !s.is_empty()).map(FolderId::new);
        document = documents
            .move_document(&document_id, folder_id.as_ref())
            .await?;
    }

    Ok(Json(json!({ "document": document })))
}

// ---------------------------------------------------------------------------
// DELETE /documents/{id} + POST /documents/{id}/restore — lifecycle
// ---------------------------------------------------------------------------

/// Soft-deletes (trashes) a document. Restorable via
/// [`restore_document`]; permanent removal is `purge`, intentionally
/// not exposed over HTTP.
pub async fn delete_document(
    State(state): State<Arc<ApiState>>,
    Extension(_user): Extension<AuthUser>,
    Path(id): Path<String>,
) -> Result<Json<Value>, ApiError> {
    let document = state
        .services
        .documents()
        .delete_document(&DocumentId::new(id.clone()))
        .await?;
    Ok(Json(json!({ "document": document })))
}

/// Restores a trashed document to active.
pub async fn restore_document(
    State(state): State<Arc<ApiState>>,
    Extension(_user): Extension<AuthUser>,
    Path(id): Path<String>,
) -> Result<Json<Value>, ApiError> {
    let document = state
        .services
        .documents()
        .restore_document(&DocumentId::new(id.clone()))
        .await?;
    Ok(Json(json!({ "document": document })))
}

// ---------------------------------------------------------------------------
// Versions, tags, history — document sub-resources
// ---------------------------------------------------------------------------

/// Lists a document's versions, newest first.
pub async fn list_versions(
    State(state): State<Arc<ApiState>>,
    Extension(_user): Extension<AuthUser>,
    Path(id): Path<String>,
) -> Result<Json<Value>, ApiError> {
    let document_id = DocumentId::new(id.clone());
    state
        .services
        .documents()
        .get_document(&document_id)
        .await?
        .ok_or_else(|| ApiError::not_found("document", id.clone()))?;
    let versions = state
        .services
        .documents()
        .list_versions(&document_id)
        .await?;
    Ok(Json(json!({ "versions": versions })))
}

/// Moves the current-version pointer to `version_number`.
pub async fn restore_version(
    State(state): State<Arc<ApiState>>,
    Extension(_user): Extension<AuthUser>,
    Path((id, version)): Path<(String, u32)>,
) -> Result<Json<Value>, ApiError> {
    let document = state
        .services
        .documents()
        .restore_version(&DocumentId::new(id.clone()), version)
        .await?;
    Ok(Json(json!({ "document": document })))
}

/// Lists tags assigned to a document.
pub async fn list_tags(
    State(state): State<Arc<ApiState>>,
    Extension(_user): Extension<AuthUser>,
    Path(id): Path<String>,
) -> Result<Json<Value>, ApiError> {
    let document_id = DocumentId::new(id.clone());
    state
        .services
        .documents()
        .get_document(&document_id)
        .await?
        .ok_or_else(|| ApiError::not_found("document", id.clone()))?;
    let tags = state.services.documents().list_tags(&document_id).await?;
    Ok(Json(json!({ "tags": tags })))
}

/// Body for [`add_tag`].
#[derive(Debug, Deserialize)]
pub struct AddTagRequest {
    name: String,
}

/// Assigns a tag to a document (created on first use).
pub async fn add_tag(
    State(state): State<Arc<ApiState>>,
    Extension(user): Extension<AuthUser>,
    Path(id): Path<String>,
    Json(body): Json<AddTagRequest>,
) -> Result<(StatusCode, Json<Value>), ApiError> {
    if body.name.trim().is_empty() {
        return Err(ApiError::bad_request("tag name must not be empty"));
    }
    let assignment = state
        .services
        .documents()
        .add_tag(
            &DocumentId::new(id.clone()),
            body.name.trim(),
            Some(&user.user_id.to_string()),
        )
        .await?;
    Ok((StatusCode::CREATED, Json(json!({ "tag": assignment }))))
}

/// Removes a tag assignment by name. `removed: false` when the tag was
/// not assigned (still `200` — the end state is the same).
pub async fn remove_tag(
    State(state): State<Arc<ApiState>>,
    Extension(_user): Extension<AuthUser>,
    Path((id, name)): Path<(String, String)>,
) -> Result<Json<Value>, ApiError> {
    let removed = state
        .services
        .documents()
        .remove_tag(&DocumentId::new(id), &name)
        .await?;
    Ok(Json(json!({ "removed": removed })))
}

/// Returns a document's audit history, oldest first.
pub async fn document_history(
    State(state): State<Arc<ApiState>>,
    Extension(_user): Extension<AuthUser>,
    Path(id): Path<String>,
) -> Result<Json<Value>, ApiError> {
    let document_id = DocumentId::new(id.clone());
    state
        .services
        .documents()
        .get_document(&document_id)
        .await?
        .ok_or_else(|| ApiError::not_found("document", id.clone()))?;
    let events = state
        .services
        .documents()
        .get_document_history(&document_id)
        .await?;
    Ok(Json(json!({ "events": events })))
}

// ---------------------------------------------------------------------------
// GET /documents/{id}/artifacts/{artifact_id}/download — byte download
// ---------------------------------------------------------------------------

/// Downloads an artifact's stored bytes. `storage_uri` must use the
/// `local://` scheme; the object name is confined to the storage root
/// by the backend. Reads are capped at [`MAX_ARTIFACT_DOWNLOAD_BYTES`].
pub async fn download_artifact(
    State(state): State<Arc<ApiState>>,
    Extension(_user): Extension<AuthUser>,
    Path((id, artifact_id)): Path<(String, String)>,
) -> Result<Response, ApiError> {
    let document_id = DocumentId::new(id.clone());
    let documents = state.services.documents();
    let artifact = documents
        .download_artifact(&artifact_id)
        .await?
        .ok_or_else(|| ApiError::not_found("artifact", artifact_id.clone()))?;
    if artifact.document_id != document_id {
        return Err(ApiError::not_found("artifact", artifact_id));
    }

    let name = artifact
        .storage_uri
        .strip_prefix("local://")
        .filter(|n| !n.is_empty())
        .ok_or_else(|| {
            ApiError::internal(format!(
                "artifact {} has unsupported storage_uri scheme",
                artifact.id
            ))
        })?
        .to_string();

    let mut fs = FaultInjectingFs::new(state.storage_config.clone())
        .map_err(|e| ApiError::internal(format!("storage init failed: {e}")))?;
    let mut reader = StreamingBackend::open_reader(&mut fs, &name).map_err(|e| {
        ApiError::from(crate::crates::application::services::ServiceError::Storage(
            e,
        ))
    })?;

    let mut buf = Vec::new();
    (&mut reader)
        .take(MAX_ARTIFACT_DOWNLOAD_BYTES.saturating_add(1))
        .read_to_end(&mut buf)
        .map_err(|e| ApiError::internal(format!("storage read failed: {e}")))?;
    if buf.len() as u64 > MAX_ARTIFACT_DOWNLOAD_BYTES {
        return Err(ApiError::payload_too_large(
            "artifact exceeds the download size limit",
        ));
    }

    let mut builder = Response::builder()
        .status(StatusCode::OK)
        .header(
            header::CONTENT_TYPE,
            artifact
                .mime_type
                .as_deref()
                .unwrap_or("application/octet-stream"),
        )
        .header(header::CONTENT_LENGTH, buf.len().to_string())
        .header(
            header::CONTENT_DISPOSITION,
            format!(
                "attachment; filename=\"{}\"",
                artifact.file_name.replace('"', "")
            ),
        );
    if let Some(sum) = &artifact.checksum {
        let algo = serde_json::to_value(sum.algorithm)
            .ok()
            .and_then(|v| v.as_str().map(str::to_string))
            .unwrap_or_else(|| "unknown".to_string());
        builder = builder.header(format!("x-checksum-{algo}"), &sum.value);
    }
    builder
        .body(Body::from(buf))
        .map_err(|e| ApiError::internal(format!("failed to build response: {e}")))
}

// ---------------------------------------------------------------------------
// Folders
// ---------------------------------------------------------------------------

/// Query parameters for [`list_folders`].
#[derive(Debug, Deserialize)]
pub struct ListFoldersQuery {
    /// Restrict to an owner. Defaults to the caller's user id.
    owner_id: Option<String>,
    /// List direct children of this folder; absent = root level.
    parent_id: Option<String>,
}

/// Lists folders (root level by default, or children of `parent_id`).
pub async fn list_folders(
    State(state): State<Arc<ApiState>>,
    Extension(user): Extension<AuthUser>,
    Query(query): Query<ListFoldersQuery>,
) -> Result<Json<Value>, ApiError> {
    let owner_id = query
        .owner_id
        .filter(|s| !s.trim().is_empty())
        .unwrap_or_else(|| user.user_id.to_string());
    let parent_id = query.parent_id.filter(|s| !s.is_empty()).map(FolderId::new);
    let folders = state
        .services
        .documents()
        .list_folders(Some(&owner_id), parent_id.as_ref())
        .await?;
    Ok(Json(json!({ "folders": folders })))
}

/// Body for [`create_folder`].
#[derive(Debug, Deserialize)]
pub struct CreateFolderRequest {
    name: String,
    parent_id: Option<String>,
}

/// Creates a folder, optionally nested under `parent_id`.
pub async fn create_folder(
    State(state): State<Arc<ApiState>>,
    Extension(user): Extension<AuthUser>,
    Json(body): Json<CreateFolderRequest>,
) -> Result<(StatusCode, Json<Value>), ApiError> {
    if body.name.trim().is_empty() {
        return Err(ApiError::bad_request("folder name must not be empty"));
    }
    let parent_id = body.parent_id.filter(|s| !s.is_empty()).map(FolderId::new);
    let folder = state
        .services
        .documents()
        .create_folder(
            body.name.trim(),
            parent_id.as_ref(),
            &user.user_id.to_string(),
        )
        .await?;
    Ok((StatusCode::CREATED, Json(json!({ "folder": folder }))))
}

// ---------------------------------------------------------------------------
// Jobs — list, cancel, retry, process
// ---------------------------------------------------------------------------

/// Query parameters for [`list_jobs_route`].
#[derive(Debug, Deserialize)]
pub struct ListJobsQuery {
    /// Filter by status (`pending`, `running`, `succeeded`, `failed`,
    /// `cancelled`).
    status: Option<String>,
    limit: Option<i64>,
}

/// Lists jobs newest-first, optionally filtered by status.
pub async fn list_jobs_route(
    State(state): State<Arc<ApiState>>,
    Extension(_user): Extension<AuthUser>,
    Query(query): Query<ListJobsQuery>,
) -> Result<Json<Value>, ApiError> {
    let status = match query
        .status
        .as_deref()
        .map(str::trim)
        .filter(|s| !s.is_empty())
    {
        Some(s) => Some(
            JobStatus::parse(s)
                .ok_or_else(|| ApiError::bad_request(format!("unknown job status: {s}")))?,
        ),
        None => None,
    };
    let jobs =
        job_engine::list_jobs(state.services.pool(), status, bounded_limit(query.limit)).await?;
    Ok(Json(json!({
        "jobs": jobs.iter().map(job_to_json).collect::<Vec<_>>(),
    })))
}

/// Cancels a `pending`/`running` job. `409` when the job already
/// reached a terminal state (or does not exist).
pub async fn cancel_job_route(
    State(state): State<Arc<ApiState>>,
    Extension(_user): Extension<AuthUser>,
    Path(id): Path<String>,
) -> Result<Json<Value>, ApiError> {
    let job_id = parse_job_id(&id)?;
    if !cancel_job(state.services.pool(), job_id).await? {
        return Err(ApiError::new(
            StatusCode::CONFLICT,
            "conflict",
            "job does not exist or already reached a terminal state",
            false,
        ));
    }
    Ok(Json(json!({ "id": job_id, "status": "cancelled" })))
}

/// Requeues a `failed` job as fresh `pending`. `409` when the job is
/// not in `failed` state.
pub async fn retry_job_route(
    State(state): State<Arc<ApiState>>,
    Extension(_user): Extension<AuthUser>,
    Path(id): Path<String>,
) -> Result<Json<Value>, ApiError> {
    let job_id = parse_job_id(&id)?;
    if !retry_job(state.services.pool(), job_id).await? {
        return Err(ApiError::new(
            StatusCode::CONFLICT,
            "conflict",
            "job does not exist or is not in failed state",
            false,
        ));
    }
    Ok(Json(json!({ "id": job_id, "status": "pending" })))
}

/// Body for [`process_document`].
#[derive(Debug, Deserialize)]
pub struct ProcessDocumentRequest {
    /// One of `metadata_extract`, `index`, `thumbnail`, or a pdf-engine
    /// op name (see `job_handlers::PDF_OPS`: `extract_text`,
    /// `extract_metadata`, `extract_images`, `page_count`, `inspect`,
    /// `validate`, `render`, `ocr`, `compress`, `rotate`, `split`,
    /// `watermark`, `stamp`, `encrypt`, `decrypt`, `compare`, ...).
    op: String,
    /// Artifact to process; absent = most recent artifact.
    artifact_id: Option<String>,
    /// Op-specific parameters as a JSON object (page ranges, degrees,
    /// passwords, watermark/stamp options, `mode`/`other_artifact_id`
    /// for `compare`, ...). Forwarded verbatim into the job payload.
    params: Option<Value>,
}

/// Enqueues a processing job for a document. The document must exist;
/// the artifact is resolved by the worker when `artifact_id` is absent.
/// Pdf-engine ops run asynchronously as `document.pdf_op` jobs — the
/// response is always `202 Accepted` with the queued job.
pub async fn process_document(
    State(state): State<Arc<ApiState>>,
    Extension(_user): Extension<AuthUser>,
    Path(id): Path<String>,
    Json(body): Json<ProcessDocumentRequest>,
) -> Result<(StatusCode, Json<Value>), ApiError> {
    let document_id = DocumentId::new(id.clone());
    state
        .services
        .documents()
        .get_document(&document_id)
        .await?
        .ok_or_else(|| ApiError::not_found("document", id.clone()))?;

    if let Some(params) = &body.params
        && !params.is_object()
    {
        return Err(ApiError::bad_request("params must be a JSON object"));
    }

    let artifact_id = body.artifact_id.as_deref();
    let pool = state.services.pool();
    let op = body.op.trim();
    let job = match op {
        "metadata_extract" => {
            job_handlers::enqueue_metadata_extract(pool, &document_id, artifact_id).await?
        }
        "index" => job_handlers::enqueue_index(pool, &document_id, artifact_id).await?,
        "thumbnail" => job_handlers::enqueue_thumbnail(pool, &document_id, artifact_id).await?,
        other if job_handlers::PDF_OPS.contains(&other) => {
            job_handlers::enqueue_pdf_op(
                pool,
                &document_id,
                artifact_id,
                other,
                body.params.unwrap_or_else(|| json!({})),
            )
            .await?
        }
        other => {
            return Err(ApiError::bad_request(format!(
                "unknown processing op {other:?}; expected metadata_extract, index, thumbnail, \
                 or a pdf op such as {}",
                job_handlers::PDF_OPS.join(", ")
            )));
        }
    };
    Ok((
        StatusCode::ACCEPTED,
        Json(json!({ "job": job_to_json(&job) })),
    ))
}

// ---------------------------------------------------------------------------
// Internals
// ---------------------------------------------------------------------------

/// Parses a `{id}` path segment into a job id.
fn parse_job_id(id: &str) -> Result<i64, ApiError> {
    id.parse()
        .map_err(|_| ApiError::bad_request("job id must be an integer"))
}

/// Clamps a `limit` query param into `[1, MAX_LIST_LIMIT]`, defaulting
/// to [`DEFAULT_LIST_LIMIT`].
fn bounded_limit(limit: Option<i64>) -> i64 {
    limit.unwrap_or(DEFAULT_LIST_LIMIT).clamp(1, MAX_LIST_LIMIT)
}

/// Stable JSON view of a [`Job`] (the type itself is not `Serialize`).
fn job_to_json(job: &Job) -> Value {
    json!({
        "id": job.id,
        "kind": job.kind,
        "payload": job.payload,
        "status": job.status.as_str(),
        "priority": job.priority,
        "attempts": job.attempts,
        "max_attempts": job.max_attempts,
        "run_at": job.run_at,
        "created_at": job.created_at,
        "started_at": job.started_at,
        "finished_at": job.finished_at,
        "last_error": job.last_error,
        "worker_id": job.worker_id,
    })
}
