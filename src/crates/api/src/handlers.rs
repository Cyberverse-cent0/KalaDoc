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
use crate::crates::application::services::access::{self, AccessCheck};
use crate::crates::application::services::documents::DocumentRelationshipType;
use crate::crates::application::services::ingestion::IngestRequest;
use crate::crates::application::services::schedules::ScheduleService;
use crate::crates::application::services::{DocumentService, ServiceError};
use crate::crates::document_engine::artifact::{
    ChecksumAlgorithm, DocumentArtifact, DocumentChecksum,
};
use crate::crates::document_engine::document::{Document, DocumentStatus, DocumentType};
use crate::crates::document_engine::ids::{DocumentId, FolderId};
use crate::crates::document_engine::lifecycle::DocumentAccessLevel;
use crate::crates::document_engine::metadata::DocumentMetadata;
use crate::crates::document_engine::store::artifacts::NewArtifact;
use crate::crates::document_engine::store::documents::DocumentPatch;
use crate::crates::document_engine::store::gen_id;
use crate::crates::image_engine::{
    CompressOptions, FlipDirection, ImageEngine, ImageError, ResizeOptions, WatermarkOptions,
};
use crate::crates::job_engine::{self, Job, JobStatus, NewJob, cancel_job, retry_job};
use crate::crates::observability::{ComponentHealth, HealthStatus, OverallHealth, aggregate};
use crate::crates::pipeline_engine::PipelineDefinition;
use crate::crates::scheduler::NewSchedule;
use crate::crates::security_engine::validate_upload;
use crate::crates::storage::local::debug_when_full::FaultInjectingFs;
use crate::crates::storage::stream::StreamingBackend;

use sha2::{Digest, Sha256};

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
/// Largest batch `POST /jobs/batch` will accept.
const MAX_BATCH_JOBS: usize = 200;
/// `max_attempts` applied to batch items that don't override it —
/// matches `job_handlers::DEFAULT_MAX_ATTEMPTS`.
const DEFAULT_JOB_MAX_ATTEMPTS: u32 = 3;
/// Largest artifact the synchronous `image.*` ops will buffer for
/// processing — matches the job handlers' `MAX_PROCESSING_BYTES`.
const MAX_OP_INPUT_BYTES: u64 = 64 * 1024 * 1024;

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
// GET /openapi.json — public API description
// ---------------------------------------------------------------------------

/// Serves the hand-written OpenAPI 3.x document covering every route.
/// Public like `/health` — API discovery shouldn't require a session.
pub async fn openapi() -> Json<Value> {
    Json(super::openapi::spec())
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
                owner_id: user.owner_id.clone(),
                file_name: report.sanitized_filename.clone(),
                mime_type,
                folder_id: None,
                tags: Vec::new(),
                actor_id: Some(user.actor_id.clone()),
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
/// row, when present). Requires `read` access — `404` for unknown ids,
/// `403` for documents the caller cannot see (see `services::access`).
pub async fn get_document(
    State(state): State<Arc<ApiState>>,
    Extension(user): Extension<AuthUser>,
    Path(id): Path<String>,
) -> Result<Json<Value>, ApiError> {
    let document_id = DocumentId::new(id.clone());
    let documents = state.services.documents();

    let document =
        require_document_access(&state, &document_id, &id, &user, DocumentAccessLevel::Read)
            .await?;
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

/// Lists the caller's documents — owned **plus** documents shared via
/// permission grants — or runs a name search when `q` is given.
/// Newest first, capped at [`MAX_LIST_LIMIT`].
///
/// `owner_id` is accepted for backward compatibility but **ignored**:
/// listings are always scoped to the caller (see
/// [`services::access`](crate::crates::application::services::access)).
/// Search results are post-filtered through the same access check, so
/// `?q=` never leaks documents the caller cannot read.
pub async fn list_documents(
    State(state): State<Arc<ApiState>>,
    Extension(user): Extension<AuthUser>,
    Query(query): Query<ListDocumentsQuery>,
) -> Result<Json<Value>, ApiError> {
    let limit = bounded_limit(query.limit);
    let documents = state.services.documents();
    let pool = state.services.pool();

    // `owner_id`/`folder_id` query params: owner is forced to the
    // caller; folder still filters.
    let _requested_owner = query.owner_id.as_deref();
    let folder_id = query.folder_id.filter(|s| !s.is_empty()).map(FolderId::new);
    let include_deleted = query.include_deleted.unwrap_or(false);

    if let Some(q) = query.q.filter(|s| !s.trim().is_empty()) {
        // Search spans every document in the store; keep only rows the
        // caller can actually read.
        let found = documents.search_documents(q.trim(), MAX_LIST_LIMIT).await?;
        let mut visible = Vec::new();
        for doc in found {
            if visible.len() as i64 >= limit {
                break;
            }
            if matches!(
                access::check_document_access(
                    pool,
                    &doc.id,
                    &user.actor_id,
                    DocumentAccessLevel::Read,
                )
                .await?,
                AccessCheck::Allow(_)
            ) {
                visible.push(doc);
            }
        }
        return Ok(Json(json!({ "documents": visible })));
    }

    let mut found = documents
        .list_documents(
            Some(&user.owner_id),
            folder_id.as_ref(),
            include_deleted,
            limit,
        )
        .await?;

    // Union in documents shared with the caller via permission grants.
    if (found.len() as i64) < limit {
        for doc_id in access::granted_document_ids(pool, &user.actor_id).await? {
            if found.len() as i64 >= limit {
                break;
            }
            if found.iter().any(|d| d.id == doc_id) {
                continue;
            }
            let AccessCheck::Allow(doc) = access::check_document_access(
                pool,
                &doc_id,
                &user.actor_id,
                DocumentAccessLevel::Read,
            )
            .await?
            else {
                continue;
            };
            if let Some(fid) = &folder_id
                && doc.folder_id.as_ref() != Some(fid)
            {
                continue;
            }
            if !include_deleted
                && matches!(
                    doc.status,
                    DocumentStatus::Trashed | DocumentStatus::Deleted
                )
            {
                continue;
            }
            found.push(doc);
        }
    }
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
    Extension(user): Extension<AuthUser>,
    Path(id): Path<String>,
    Json(body): Json<UpdateDocumentRequest>,
) -> Result<Json<Value>, ApiError> {
    let document_id = DocumentId::new(id.clone());
    let documents = state.services.documents();

    // `write` access required; also verifies existence up front so a
    // pure folder move on a missing document still 404s instead of
    // silently succeeding.
    require_document_access(&state, &document_id, &id, &user, DocumentAccessLevel::Write).await?;

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
/// [`restore_document`]; permanent removal is [`purge_document`].
/// Requires `write` access.
pub async fn delete_document(
    State(state): State<Arc<ApiState>>,
    Extension(user): Extension<AuthUser>,
    Path(id): Path<String>,
) -> Result<Json<Value>, ApiError> {
    let document_id = DocumentId::new(id.clone());
    require_document_access(&state, &document_id, &id, &user, DocumentAccessLevel::Write).await?;
    let document = state
        .services
        .documents()
        .delete_document(&document_id)
        .await?;
    Ok(Json(json!({ "document": document })))
}

/// Restores a trashed document to active. Requires `write` access.
pub async fn restore_document(
    State(state): State<Arc<ApiState>>,
    Extension(user): Extension<AuthUser>,
    Path(id): Path<String>,
) -> Result<Json<Value>, ApiError> {
    let document_id = DocumentId::new(id.clone());
    require_document_access(&state, &document_id, &id, &user, DocumentAccessLevel::Write).await?;
    let document = state
        .services
        .documents()
        .restore_document(&document_id)
        .await?;
    Ok(Json(json!({ "document": document })))
}

// ---------------------------------------------------------------------------
// Lifecycle extensions — copy / archive / purge / new version
// ---------------------------------------------------------------------------

/// Body for [`copy_document`].
#[derive(Debug, Deserialize)]
pub struct CopyDocumentRequest {
    /// Name for the copy; absent = `"<name> (copy)"`.
    name: Option<String>,
}

/// Duplicates a document (metadata + artifact rows; storage bytes are
/// shared, not copied). Requires `read` access on the source.
///
/// Note: the store keeps the **source's `owner_id`** on the copy — a
/// caller copying a document shared with them produces a document they
/// may not be able to see afterwards. Treat copy as owner-facing.
pub async fn copy_document(
    State(state): State<Arc<ApiState>>,
    Extension(user): Extension<AuthUser>,
    Path(id): Path<String>,
    Json(body): Json<CopyDocumentRequest>,
) -> Result<(StatusCode, Json<Value>), ApiError> {
    let document_id = DocumentId::new(id.clone());
    require_document_access(&state, &document_id, &id, &user, DocumentAccessLevel::Read).await?;
    let document = state
        .services
        .documents()
        .copy_document(&document_id, body.name.as_deref())
        .await?;
    Ok((StatusCode::CREATED, Json(json!({ "document": document }))))
}

/// Archives a document (`status → archived`). Requires `write` access.
pub async fn archive_document(
    State(state): State<Arc<ApiState>>,
    Extension(user): Extension<AuthUser>,
    Path(id): Path<String>,
) -> Result<Json<Value>, ApiError> {
    let document_id = DocumentId::new(id.clone());
    require_document_access(&state, &document_id, &id, &user, DocumentAccessLevel::Write).await?;
    let document = state
        .services
        .documents()
        .archive_document(&document_id)
        .await?;
    Ok(Json(json!({ "document": document })))
}

/// Unarchives a document (`status → active`). Requires `write` access.
pub async fn unarchive_document(
    State(state): State<Arc<ApiState>>,
    Extension(user): Extension<AuthUser>,
    Path(id): Path<String>,
) -> Result<Json<Value>, ApiError> {
    let document_id = DocumentId::new(id.clone());
    require_document_access(&state, &document_id, &id, &user, DocumentAccessLevel::Write).await?;
    let document = state
        .services
        .documents()
        .unarchive_document(&document_id)
        .await?;
    Ok(Json(json!({ "document": document })))
}

/// Permanently deletes a **trashed** document and its dependents.
/// Irreversible — requires `manage` access (stricter than trash).
pub async fn purge_document(
    State(state): State<Arc<ApiState>>,
    Extension(user): Extension<AuthUser>,
    Path(id): Path<String>,
) -> Result<Json<Value>, ApiError> {
    let document_id = DocumentId::new(id.clone());
    require_document_access(
        &state,
        &document_id,
        &id,
        &user,
        DocumentAccessLevel::Manage,
    )
    .await?;
    let purged = state
        .services
        .documents()
        .purge_document(&document_id)
        .await?;
    Ok(Json(json!({ "purged": purged })))
}

/// Body for [`create_document_version`].
#[derive(Debug, Deserialize)]
pub struct CreateVersionRequest {
    /// Artifact the new version points at; absent = most recent
    /// artifact (same resolution the processing endpoints use is *not*
    /// applied here — the store records `NULL` when absent).
    artifact_id: Option<String>,
    /// Optional version note / commit message.
    note: Option<String>,
}

/// Creates the next version for a document. Requires `write` access.
pub async fn create_document_version(
    State(state): State<Arc<ApiState>>,
    Extension(user): Extension<AuthUser>,
    Path(id): Path<String>,
    Json(body): Json<CreateVersionRequest>,
) -> Result<(StatusCode, Json<Value>), ApiError> {
    let document_id = DocumentId::new(id.clone());
    require_document_access(&state, &document_id, &id, &user, DocumentAccessLevel::Write).await?;
    let version = state
        .services
        .documents()
        .create_version(
            &document_id,
            body.artifact_id.as_deref(),
            &user.actor_id,
            body.note.as_deref(),
        )
        .await?;
    Ok((StatusCode::CREATED, Json(json!({ "version": version }))))
}

// ---------------------------------------------------------------------------
// Versions, tags, history — document sub-resources
// ---------------------------------------------------------------------------

/// Lists a document's versions, newest first. Requires `read` access.
pub async fn list_versions(
    State(state): State<Arc<ApiState>>,
    Extension(user): Extension<AuthUser>,
    Path(id): Path<String>,
) -> Result<Json<Value>, ApiError> {
    let document_id = DocumentId::new(id.clone());
    require_document_access(&state, &document_id, &id, &user, DocumentAccessLevel::Read).await?;
    let versions = state
        .services
        .documents()
        .list_versions(&document_id)
        .await?;
    Ok(Json(json!({ "versions": versions })))
}

/// Moves the current-version pointer to `version_number`.
/// Requires `write` access.
pub async fn restore_version(
    State(state): State<Arc<ApiState>>,
    Extension(user): Extension<AuthUser>,
    Path((id, version)): Path<(String, u32)>,
) -> Result<Json<Value>, ApiError> {
    let document_id = DocumentId::new(id.clone());
    require_document_access(&state, &document_id, &id, &user, DocumentAccessLevel::Write).await?;
    let document = state
        .services
        .documents()
        .restore_version(&document_id, version)
        .await?;
    Ok(Json(json!({ "document": document })))
}

/// Lists tags assigned to a document. Requires `read` access.
pub async fn list_tags(
    State(state): State<Arc<ApiState>>,
    Extension(user): Extension<AuthUser>,
    Path(id): Path<String>,
) -> Result<Json<Value>, ApiError> {
    let document_id = DocumentId::new(id.clone());
    require_document_access(&state, &document_id, &id, &user, DocumentAccessLevel::Read).await?;
    let tags = state.services.documents().list_tags(&document_id).await?;
    Ok(Json(json!({ "tags": tags })))
}

/// Body for [`add_tag`].
#[derive(Debug, Deserialize)]
pub struct AddTagRequest {
    name: String,
}

/// Assigns a tag to a document (created on first use).
/// Requires `write` access.
pub async fn add_tag(
    State(state): State<Arc<ApiState>>,
    Extension(user): Extension<AuthUser>,
    Path(id): Path<String>,
    Json(body): Json<AddTagRequest>,
) -> Result<(StatusCode, Json<Value>), ApiError> {
    if body.name.trim().is_empty() {
        return Err(ApiError::bad_request("tag name must not be empty"));
    }
    let document_id = DocumentId::new(id.clone());
    require_document_access(&state, &document_id, &id, &user, DocumentAccessLevel::Write).await?;
    let assignment = state
        .services
        .documents()
        .add_tag(&document_id, body.name.trim(), Some(user.actor_id.as_str()))
        .await?;
    Ok((StatusCode::CREATED, Json(json!({ "tag": assignment }))))
}

/// Removes a tag assignment by name. `removed: false` when the tag was
/// not assigned (still `200` — the end state is the same).
/// Requires `write` access.
pub async fn remove_tag(
    State(state): State<Arc<ApiState>>,
    Extension(user): Extension<AuthUser>,
    Path((id, name)): Path<(String, String)>,
) -> Result<Json<Value>, ApiError> {
    let document_id = DocumentId::new(id.clone());
    require_document_access(&state, &document_id, &id, &user, DocumentAccessLevel::Write).await?;
    let removed = state
        .services
        .documents()
        .remove_tag(&document_id, &name)
        .await?;
    Ok(Json(json!({ "removed": removed })))
}

/// Returns a document's audit history, oldest first.
/// Requires `read` access.
pub async fn document_history(
    State(state): State<Arc<ApiState>>,
    Extension(user): Extension<AuthUser>,
    Path(id): Path<String>,
) -> Result<Json<Value>, ApiError> {
    let document_id = DocumentId::new(id.clone());
    require_document_access(&state, &document_id, &id, &user, DocumentAccessLevel::Read).await?;
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
/// Requires `read` access on the owning document.
pub async fn download_artifact(
    State(state): State<Arc<ApiState>>,
    Extension(user): Extension<AuthUser>,
    Path((id, artifact_id)): Path<(String, String)>,
) -> Result<Response, ApiError> {
    let document_id = DocumentId::new(id.clone());
    require_document_access(&state, &document_id, &id, &user, DocumentAccessLevel::Read).await?;
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

/// Lists the caller's folders (root level by default, or children of
/// `parent_id`). `owner_id` is accepted but **ignored** — folder
/// listings are always scoped to the caller (folders have no share
/// mechanism; see `services::access`).
pub async fn list_folders(
    State(state): State<Arc<ApiState>>,
    Extension(user): Extension<AuthUser>,
    Query(query): Query<ListFoldersQuery>,
) -> Result<Json<Value>, ApiError> {
    let _requested_owner = query.owner_id.as_deref();
    let parent_id = query.parent_id.filter(|s| !s.is_empty()).map(FolderId::new);
    let folders = state
        .services
        .documents()
        .list_folders(Some(&user.owner_id), parent_id.as_ref())
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
        .create_folder(body.name.trim(), parent_id.as_ref(), &user.owner_id)
        .await?;
    Ok((StatusCode::CREATED, Json(json!({ "folder": folder }))))
}

/// Body for [`update_folder`]. `parent_id` uses the same tri-state
/// encoding as document moves: absent = unchanged, `null` = move to
/// root, string = reparent.
#[derive(Debug, Deserialize)]
pub struct UpdateFolderRequest {
    name: Option<String>,
    #[serde(default, deserialize_with = "present_null")]
    parent_id: Option<Option<String>>,
}

/// Renames and/or reparents a folder. Owner-only — folders have no
/// permission grants, so a foreign or missing id is `404`.
pub async fn update_folder(
    State(state): State<Arc<ApiState>>,
    Extension(user): Extension<AuthUser>,
    Path(id): Path<String>,
    Json(body): Json<UpdateFolderRequest>,
) -> Result<Json<Value>, ApiError> {
    let folder_id = FolderId::new(id.clone());
    if let Some(name) = &body.name
        && name.trim().is_empty()
    {
        return Err(ApiError::bad_request("folder name must not be empty"));
    }

    let documents = state.services.documents();
    // Owner check up front: 404 for missing *and* foreign folders.
    access::check_folder_access(state.services.pool(), &folder_id, &user.owner_id)
        .await?
        .ok_or_else(|| ApiError::not_found("folder", id.clone()))?;

    let mut folder = if let Some(name) = &body.name {
        documents.rename_folder(&folder_id, name.trim()).await?
    } else {
        documents
            .get_folder(&folder_id)
            .await?
            .ok_or_else(|| ApiError::not_found("folder", id.clone()))?
    };

    if let Some(parent) = body.parent_id {
        let parent_id = parent.filter(|s| !s.is_empty()).map(FolderId::new);
        folder = documents
            .move_folder(&folder_id, parent_id.as_ref())
            .await?;
    }

    Ok(Json(json!({ "folder": folder })))
}

/// Deletes a folder; descendants cascade and contained documents move
/// to root. Owner-only — `404` for missing or foreign folders.
pub async fn delete_folder(
    State(state): State<Arc<ApiState>>,
    Extension(user): Extension<AuthUser>,
    Path(id): Path<String>,
) -> Result<Json<Value>, ApiError> {
    let folder_id = FolderId::new(id.clone());
    access::check_folder_access(state.services.pool(), &folder_id, &user.owner_id)
        .await?
        .ok_or_else(|| ApiError::not_found("folder", id.clone()))?;
    let deleted = state.services.documents().delete_folder(&folder_id).await?;
    Ok(Json(json!({ "deleted": deleted })))
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

/// Enqueues a processing job for a document. Requires `write` access;
/// the artifact is resolved by the worker when `artifact_id` is absent.
/// Pdf-engine ops run asynchronously as `document.pdf_op` jobs — the
/// response is always `202 Accepted` with the queued job.
pub async fn process_document(
    State(state): State<Arc<ApiState>>,
    Extension(user): Extension<AuthUser>,
    Path(id): Path<String>,
    Json(body): Json<ProcessDocumentRequest>,
) -> Result<(StatusCode, Json<Value>), ApiError> {
    let document_id = DocumentId::new(id.clone());
    require_document_access(&state, &document_id, &id, &user, DocumentAccessLevel::Write).await?;

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
// POST /pipelines/run — authenticated pipeline execution
// ---------------------------------------------------------------------------

/// Body for [`run_pipeline`].
#[derive(Debug, Deserialize)]
pub struct RunPipelineRequest {
    /// Document whose artifact feeds the pipeline.
    document_id: String,
    /// Artifact to process; absent = most recent artifact.
    artifact_id: Option<String>,
    /// Either `{"name": "<builtin>"}` — resolved against
    /// `pipeline_engine::builtin_definitions` (`pdf.intake`,
    /// `pdf.extract-text`) — or a full `PipelineDefinition` JSON
    /// object (`name` + `steps`). A `steps` key present means the
    /// value is treated as a definition.
    pipeline: Value,
}

/// Enqueues a `pipeline.run` job for a document. The document must
/// exist; the artifact is resolved by the worker when `artifact_id`
/// is absent. Built-in pipeline names are resolved to their
/// definition up front so unknown names fail with `400` instead of a
/// queued job that can only fail. The response is `202 Accepted` with
/// the queued job.
pub async fn run_pipeline(
    State(state): State<Arc<ApiState>>,
    Extension(user): Extension<AuthUser>,
    Json(body): Json<RunPipelineRequest>,
) -> Result<(StatusCode, Json<Value>), ApiError> {
    let document_id = DocumentId::new(body.document_id.clone());
    require_document_access(
        &state,
        &document_id,
        &body.document_id,
        &user,
        DocumentAccessLevel::Write,
    )
    .await?;

    let definition = resolve_pipeline(&body.pipeline)?;

    let job = job_handlers::enqueue_pipeline_run(
        state.services.pool(),
        &document_id,
        body.artifact_id.as_deref(),
        &definition,
    )
    .await?;
    Ok((
        StatusCode::ACCEPTED,
        Json(json!({ "job": job_to_json(&job) })),
    ))
}

/// Resolves the `pipeline` request field into a concrete
/// [`PipelineDefinition`]: a `steps` member means a full definition
/// (deserialized verbatim — the engine validates engines/operations
/// at run time); otherwise `name` selects a built-in definition.
fn resolve_pipeline(pipeline: &Value) -> Result<PipelineDefinition, ApiError> {
    if pipeline.get("steps").is_some() {
        return serde_json::from_value::<PipelineDefinition>(pipeline.clone())
            .map_err(|e| ApiError::bad_request(format!("invalid pipeline definition: {e}")));
    }
    let name = pipeline
        .get("name")
        .and_then(Value::as_str)
        .map(str::trim)
        .filter(|s| !s.is_empty())
        .ok_or_else(|| {
            ApiError::bad_request(
                "pipeline must be {\"name\": \"<builtin>\"} or a full PipelineDefinition with steps",
            )
        })?;
    crate::crates::pipeline_engine::builtin_definitions()
        .into_iter()
        .find(|def| def.name == name)
        .ok_or_else(|| {
            let builtins = crate::crates::pipeline_engine::builtin_definitions()
                .iter()
                .map(|def| def.name.as_str())
                .collect::<Vec<_>>()
                .join(", ");
            ApiError::bad_request(format!("unknown pipeline {name:?}; built-ins: {builtins}"))
        })
}

// ---------------------------------------------------------------------------
// GET /pipelines + GET /pipelines/{name} — built-in definitions
// ---------------------------------------------------------------------------

/// Lists the built-in pipeline definitions shipped with
/// `pipeline_engine` (`pdf.intake`, `pdf.extract-text`, …) as full
/// [`PipelineDefinition`] JSON — the same shape `POST /pipelines/run`
/// accepts back in its `pipeline` field.
pub async fn list_pipelines(
    State(_state): State<Arc<ApiState>>,
    Extension(_user): Extension<AuthUser>,
) -> Json<Value> {
    Json(json!({
        "pipelines": crate::crates::pipeline_engine::builtin_definitions(),
    }))
}

/// Returns one built-in pipeline definition by name. `404` for names
/// that aren't built-ins (custom definitions aren't stored server-side
/// — clients submit them inline to `POST /pipelines/run`).
pub async fn get_pipeline(
    State(_state): State<Arc<ApiState>>,
    Extension(_user): Extension<AuthUser>,
    Path(name): Path<String>,
) -> Result<Json<Value>, ApiError> {
    let name = name.trim();
    let definition = crate::crates::pipeline_engine::builtin_definitions()
        .into_iter()
        .find(|def| def.name == name)
        .ok_or_else(|| ApiError::not_found("pipeline", name))?;
    Ok(Json(json!({ "pipeline": definition })))
}

// ---------------------------------------------------------------------------
// Job batches — POST /jobs/batch, GET /jobs/batch/{id}, cancel
// ---------------------------------------------------------------------------

/// One item in a `POST /jobs/batch` request — mirrors [`NewJob`]
/// with optional scheduling fields.
#[derive(Debug, Deserialize)]
pub struct BatchJobSpec {
    /// Handler registry key, e.g. `"document.metadata_extract"`.
    kind: String,
    /// JSON payload handed to the handler; must be an object.
    #[serde(default = "empty_object")]
    payload: Value,
    /// Higher runs first among jobs due at the same time.
    priority: Option<i64>,
    /// Unix seconds — earliest claimable time; absent = now.
    run_at: Option<i64>,
    /// `None` uses the default (3 attempts).
    max_attempts: Option<u32>,
}

fn empty_object() -> Value {
    json!({})
}

/// Enqueues up to [`MAX_BATCH_JOBS`] jobs as one batch (shared
/// `batch_id` = first job's id). Returns `201` with the batch id and
/// every enqueued job.
pub async fn create_job_batch(
    State(state): State<Arc<ApiState>>,
    Extension(_user): Extension<AuthUser>,
    Json(specs): Json<Vec<BatchJobSpec>>,
) -> Result<(StatusCode, Json<Value>), ApiError> {
    if specs.is_empty() {
        return Err(ApiError::bad_request("batch must contain at least one job"));
    }
    if specs.len() > MAX_BATCH_JOBS {
        return Err(ApiError::bad_request(format!(
            "batch exceeds the limit of {MAX_BATCH_JOBS} jobs"
        )));
    }

    let mut jobs = Vec::with_capacity(specs.len());
    for (i, spec) in specs.iter().enumerate() {
        let kind = spec.kind.trim();
        if kind.is_empty() {
            return Err(ApiError::bad_request(format!(
                "jobs[{i}].kind must be a non-empty string"
            )));
        }
        if !spec.payload.is_object() {
            return Err(ApiError::bad_request(format!(
                "jobs[{i}].payload must be a JSON object"
            )));
        }
        jobs.push(NewJob {
            kind: kind.to_string(),
            payload: spec.payload.clone(),
            priority: spec.priority.unwrap_or(0),
            run_at: spec.run_at,
            max_attempts: spec.max_attempts,
        });
    }

    let enqueued =
        job_engine::enqueue_batch(state.services.pool(), &jobs, DEFAULT_JOB_MAX_ATTEMPTS).await?;
    // batch_id is the first job's id (see job_engine::enqueue_batch).
    let batch_id = enqueued
        .first()
        .map(|job| job.batch_id.unwrap_or(job.id))
        .unwrap_or(0);
    Ok((
        StatusCode::CREATED,
        Json(json!({
            "batch_id": batch_id,
            "jobs": enqueued.iter().map(job_to_json).collect::<Vec<_>>(),
        })),
    ))
}

/// Returns one batch's aggregate progress plus every member job.
/// `404` when no jobs carry the batch id.
pub async fn get_job_batch(
    State(state): State<Arc<ApiState>>,
    Extension(_user): Extension<AuthUser>,
    Path(id): Path<String>,
) -> Result<Json<Value>, ApiError> {
    let batch_id = parse_job_id(&id)?;
    let pool = state.services.pool();
    let progress = job_engine::batch_progress(pool, batch_id)
        .await?
        .ok_or_else(|| ApiError::not_found("job batch", id))?;
    let jobs = job_engine::batch_jobs(pool, batch_id).await?;
    Ok(Json(json!({
        "batch_id": batch_id,
        "progress": {
            "items_total": progress.items_total,
            "items_done": progress.items_done,
            "items_pending": progress.items_pending,
            "items_succeeded": progress.items_succeeded,
            "items_failed": progress.items_failed,
            "items_skipped": progress.items_skipped,
            "items_cancelled": progress.items_cancelled,
            "pct": progress.pct(),
            "complete": progress.is_complete(),
        },
        "jobs": jobs.iter().map(job_to_json).collect::<Vec<_>>(),
    })))
}

/// Cancels every non-terminal job in the batch. `404` for an unknown
/// batch; already-terminal items keep their outcome.
pub async fn cancel_job_batch(
    State(state): State<Arc<ApiState>>,
    Extension(_user): Extension<AuthUser>,
    Path(id): Path<String>,
) -> Result<Json<Value>, ApiError> {
    let batch_id = parse_job_id(&id)?;
    let pool = state.services.pool();
    if job_engine::batch_progress(pool, batch_id).await?.is_none() {
        return Err(ApiError::not_found("job batch", id));
    }
    let cancelled = job_engine::cancel_batch(pool, batch_id).await?;
    Ok(Json(json!({
        "batch_id": batch_id,
        "cancelled": cancelled,
    })))
}

// ---------------------------------------------------------------------------
// GET /engines/{name}/operations — per-engine capability detail
// ---------------------------------------------------------------------------

/// Returns the named engine's full capability set: advertised
/// operations, input/output formats, and resource limits. `404` when
/// no engine is registered under `name`.
pub async fn engine_operations(
    State(state): State<Arc<ApiState>>,
    Path(name): Path<String>,
) -> Result<Json<Value>, ApiError> {
    let engine = state
        .engines
        .get(&name)
        .ok_or_else(|| ApiError::not_found("engine", name))?;
    let caps = engine.capabilities();
    let limits = &caps.limits;
    Ok(Json(json!({
        "name": engine.name(),
        "version": engine.version(),
        "kind": engine.kind(),
        "operations": caps.operations,
        "input_formats": caps.input_formats,
        "output_formats": caps.output_formats,
        "limits": {
            "max_input_bytes": limits.max_input_bytes,
            "max_pages": limits.max_pages,
            "max_pixels": limits.max_pixels,
            "max_outputs": limits.max_outputs,
        },
    })))
}

// ---------------------------------------------------------------------------
// Document settings + metadata — sub-resources
// ---------------------------------------------------------------------------

/// Verifies the caller holds `level` on the document (`404` unknown,
/// `403` insufficient — see [`services::access`]); shared preamble for
/// the document sub-resource handlers. Returns the loaded document.
async fn require_document_access(
    state: &ApiState,
    document_id: &DocumentId,
    raw_id: &str,
    user: &AuthUser,
    level: DocumentAccessLevel,
) -> Result<Document, ApiError> {
    match access::check_document_access(state.services.pool(), document_id, &user.actor_id, level)
        .await?
    {
        AccessCheck::Allow(document) => Ok(document),
        AccessCheck::NotFound => Err(ApiError::not_found("document", raw_id)),
        AccessCheck::Forbidden => Err(ApiError::forbidden(format!(
            "insufficient permissions on document {raw_id}"
        ))),
    }
}

/// Returns the document's settings row; `settings: null` when none has
/// been stored yet. `404` for an unknown document.
pub async fn get_settings(
    State(state): State<Arc<ApiState>>,
    Extension(user): Extension<AuthUser>,
    Path(id): Path<String>,
) -> Result<Json<Value>, ApiError> {
    let document_id = DocumentId::new(id.clone());
    require_document_access(&state, &document_id, &id, &user, DocumentAccessLevel::Read).await?;
    let settings = state
        .services
        .documents()
        .get_document_settings(&document_id)
        .await?;
    Ok(Json(json!({ "settings": settings })))
}

/// Body for [`patch_settings`]. `settings` is merged key-by-key into
/// the stored object (absent keys are preserved); `is_locked` is
/// independent of the blob.
#[derive(Debug, Deserialize)]
pub struct PatchSettingsRequest {
    settings: Option<serde_json::Map<String, Value>>,
    is_locked: Option<bool>,
}

/// Merges settings keys and/or updates the lock flag. Returns the
/// resulting settings row. Validation (key shape, blob size) happens
/// in the store — oversized/invalid input surfaces as `400`.
pub async fn patch_settings(
    State(state): State<Arc<ApiState>>,
    Extension(user): Extension<AuthUser>,
    Path(id): Path<String>,
    Json(body): Json<PatchSettingsRequest>,
) -> Result<Json<Value>, ApiError> {
    let document_id = DocumentId::new(id.clone());
    require_document_access(&state, &document_id, &id, &user, DocumentAccessLevel::Write).await?;
    let documents = state.services.documents();
    if let Some(patch) = &body.settings {
        documents
            .update_document_settings(&document_id, patch)
            .await?;
    }
    if let Some(locked) = body.is_locked {
        documents.set_document_locked(&document_id, locked).await?;
    }
    let settings = documents.get_document_settings(&document_id).await?;
    Ok(Json(json!({ "settings": settings })))
}

/// Deletes the document's settings row (`deleted: false` when none
/// existed). The document itself is untouched.
pub async fn delete_settings(
    State(state): State<Arc<ApiState>>,
    Extension(user): Extension<AuthUser>,
    Path(id): Path<String>,
) -> Result<Json<Value>, ApiError> {
    let document_id = DocumentId::new(id.clone());
    require_document_access(&state, &document_id, &id, &user, DocumentAccessLevel::Write).await?;
    let deleted = state
        .services
        .documents()
        .delete_document_settings(&document_id)
        .await?;
    Ok(Json(json!({ "deleted": deleted })))
}

/// Returns the document's metadata row; `metadata: null` when none has
/// been stored yet. `404` for an unknown document.
pub async fn get_metadata(
    State(state): State<Arc<ApiState>>,
    Extension(user): Extension<AuthUser>,
    Path(id): Path<String>,
) -> Result<Json<Value>, ApiError> {
    let document_id = DocumentId::new(id.clone());
    require_document_access(&state, &document_id, &id, &user, DocumentAccessLevel::Read).await?;
    let metadata = state
        .services
        .documents()
        .get_document_metadata(&document_id)
        .await?;
    Ok(Json(json!({ "metadata": metadata })))
}

/// Body for [`patch_metadata`]. Every field is tri-state: absent =
/// unchanged, present = overwrite (a JSON `null` clears an optional
/// field). `custom_fields`, when present, replaces the whole map —
/// merge individual keys via [`set_metadata_field`] instead.
#[derive(Debug, Deserialize)]
pub struct PatchMetadataRequest {
    #[serde(default, deserialize_with = "present_null")]
    title: Option<Option<String>>,
    #[serde(default, deserialize_with = "present_null")]
    author: Option<Option<String>>,
    #[serde(default, deserialize_with = "present_null")]
    subject: Option<Option<String>>,
    #[serde(default, deserialize_with = "present_null")]
    language: Option<Option<String>>,
    keywords: Option<Vec<String>>,
    custom_fields: Option<serde_json::Map<String, Value>>,
}

/// Applies a partial update to the document's metadata row, creating
/// it with defaults when none exists. Returns the resulting row.
pub async fn patch_metadata(
    State(state): State<Arc<ApiState>>,
    Extension(user): Extension<AuthUser>,
    Path(id): Path<String>,
    Json(body): Json<PatchMetadataRequest>,
) -> Result<Json<Value>, ApiError> {
    let document_id = DocumentId::new(id.clone());
    require_document_access(&state, &document_id, &id, &user, DocumentAccessLevel::Write).await?;
    let documents = state.services.documents();

    let mut metadata = documents
        .get_document_metadata(&document_id)
        .await?
        .unwrap_or_else(|| DocumentMetadata {
            document_id: document_id.clone(),
            title: None,
            author: None,
            subject: None,
            language: None,
            keywords: Vec::new(),
            custom_fields: serde_json::Map::new(),
        });

    if let Some(title) = body.title {
        metadata.title = title;
    }
    if let Some(author) = body.author {
        metadata.author = author;
    }
    if let Some(subject) = body.subject {
        metadata.subject = subject;
    }
    if let Some(language) = body.language {
        metadata.language = language;
    }
    if let Some(keywords) = body.keywords {
        metadata.keywords = keywords;
    }
    if let Some(custom_fields) = body.custom_fields {
        metadata.custom_fields = custom_fields;
    }

    documents.upsert_document_metadata(&metadata).await?;
    Ok(Json(json!({ "metadata": metadata })))
}

/// Deletes the document's metadata row (`deleted: false` when none
/// existed). The document itself is untouched.
pub async fn delete_metadata(
    State(state): State<Arc<ApiState>>,
    Extension(user): Extension<AuthUser>,
    Path(id): Path<String>,
) -> Result<Json<Value>, ApiError> {
    let document_id = DocumentId::new(id.clone());
    require_document_access(&state, &document_id, &id, &user, DocumentAccessLevel::Write).await?;
    let deleted = state
        .services
        .documents()
        .delete_document_metadata(&document_id)
        .await?;
    Ok(Json(json!({ "deleted": deleted })))
}

/// Body for [`set_metadata_field`]. `value` is any JSON value.
#[derive(Debug, Deserialize)]
pub struct SetMetadataFieldRequest {
    value: Value,
}

/// Sets (or overwrites) one metadata custom field, creating the
/// metadata row with defaults when none exists. Returns the updated
/// metadata row.
pub async fn set_metadata_field(
    State(state): State<Arc<ApiState>>,
    Extension(user): Extension<AuthUser>,
    Path((id, key)): Path<(String, String)>,
    Json(body): Json<SetMetadataFieldRequest>,
) -> Result<Json<Value>, ApiError> {
    let document_id = DocumentId::new(id.clone());
    require_document_access(&state, &document_id, &id, &user, DocumentAccessLevel::Write).await?;
    let metadata = state
        .services
        .documents()
        .set_custom_field(&document_id, &key, &body.value)
        .await?;
    Ok(Json(json!({ "metadata": metadata })))
}

/// Removes one metadata custom field. `removed: false` when the key
/// was absent (still `200` — the end state is the same).
pub async fn remove_metadata_field(
    State(state): State<Arc<ApiState>>,
    Extension(user): Extension<AuthUser>,
    Path((id, key)): Path<(String, String)>,
) -> Result<Json<Value>, ApiError> {
    let document_id = DocumentId::new(id.clone());
    require_document_access(&state, &document_id, &id, &user, DocumentAccessLevel::Write).await?;
    let removed = state
        .services
        .documents()
        .remove_custom_field(&document_id, &key)
        .await?;
    Ok(Json(json!({ "removed": removed })))
}

// ---------------------------------------------------------------------------
// POST /documents/{id}/ops/{op} — generic engine-op endpoint
// ---------------------------------------------------------------------------

/// Body for [`run_document_op`].
#[derive(Debug, Deserialize)]
pub struct RunOpRequest {
    /// Artifact to operate on; absent = most recent artifact.
    artifact_id: Option<String>,
    /// Op-specific parameters as a JSON object, forwarded to the op:
    /// `params.pipeline` for `pipeline.run`, engine options for
    /// `image.*`, op arguments for `pdf.*`.
    params: Option<Value>,
}

/// Runs one engine operation against a document's artifact.
///
/// Op naming is engine-namespaced:
///
/// - `image.<op>` — **synchronous** `image_engine` ops: `inspect`,
///   `metadata`, `validate` (JSON reports) and `convert`, `resize`,
///   `rotate`, `flip`, `crop`, `thumbnail`, `compress`, `watermark`
///   (byte outputs stored as a new artifact under
///   `documents/{id}/outputs/`).
/// - `pdf.<op>` (or a bare `PDF_OPS` name) — enqueues a
///   `document.pdf_op` job (`202`), same as `POST .../process`.
/// - `pipeline.run` — enqueues a `pipeline.run` job; `params.pipeline`
///   is `{"name": "<builtin>"}` or a full `PipelineDefinition`.
///
/// Anything else is a `400`.
pub async fn run_document_op(
    State(state): State<Arc<ApiState>>,
    Extension(user): Extension<AuthUser>,
    Path((id, op)): Path<(String, String)>,
    Json(body): Json<RunOpRequest>,
) -> Result<(StatusCode, Json<Value>), ApiError> {
    let document_id = DocumentId::new(id.clone());
    require_document_access(&state, &document_id, &id, &user, DocumentAccessLevel::Write).await?;

    if let Some(params) = &body.params
        && !params.is_object()
    {
        return Err(ApiError::bad_request("params must be a JSON object"));
    }

    let op = op.trim();
    let artifact_id = body.artifact_id.as_deref();
    let params = body.params.unwrap_or_else(|| json!({}));
    let pool = state.services.pool();

    if let Some(image_op) = op.strip_prefix("image.") {
        return run_image_op(&state, &document_id, artifact_id, image_op, params).await;
    }

    if op == "pipeline.run" {
        let pipeline = params.get("pipeline").cloned().ok_or_else(|| {
            ApiError::bad_request(
                "pipeline.run requires params.pipeline ({\"name\": \"<builtin>\"} or a PipelineDefinition)",
            )
        })?;
        let definition = resolve_pipeline(&pipeline)?;
        let job = job_handlers::enqueue_pipeline_run(pool, &document_id, artifact_id, &definition)
            .await?;
        return Ok((
            StatusCode::ACCEPTED,
            Json(json!({ "job": job_to_json(&job) })),
        ));
    }

    let pdf_op = op.strip_prefix("pdf.").unwrap_or(op);
    if job_handlers::PDF_OPS.contains(&pdf_op) {
        let job =
            job_handlers::enqueue_pdf_op(pool, &document_id, artifact_id, pdf_op, params).await?;
        return Ok((
            StatusCode::ACCEPTED,
            Json(json!({ "job": job_to_json(&job) })),
        ));
    }

    Err(ApiError::bad_request(format!(
        "unknown op {op:?}; expected image.<op>, pdf.<op>, pipeline.run, \
         or a pdf op such as {}",
        job_handlers::PDF_OPS.join(", ")
    )))
}

/// Output of a synchronous image op: either a JSON report returned
/// inline, or encoded bytes stored as a new artifact.
enum ImageOpOutput {
    Report(Value),
    Bytes {
        data: Vec<u8>,
        extension: &'static str,
        mime_type: &'static str,
    },
}

/// Runs an `image.*` op synchronously: resolves the input artifact,
/// buffers its bytes (bounded by [`MAX_OP_INPUT_BYTES`]), executes the
/// op on a blocking thread, and either returns the report or stores
/// the output bytes as a new artifact.
async fn run_image_op(
    state: &Arc<ApiState>,
    document_id: &DocumentId,
    artifact_id: Option<&str>,
    op: &str,
    params: Value,
) -> Result<(StatusCode, Json<Value>), ApiError> {
    let documents = state.services.documents();
    let artifact = resolve_op_artifact(documents, document_id, artifact_id).await?;
    let input = read_artifact_bytes(state, &artifact).await?;

    // `image.watermark` needs a second image; resolve its bytes up
    // front so storage errors surface before the blocking op runs.
    let mark = if op == "watermark" {
        let mark_id = params
            .get("mark_artifact_id")
            .and_then(Value::as_str)
            .map(str::trim)
            .filter(|s| !s.is_empty())
            .ok_or_else(|| {
                ApiError::bad_request("image.watermark requires params.mark_artifact_id")
            })?;
        let mark_artifact = documents
            .download_artifact(mark_id)
            .await?
            .filter(|a| a.document_id == document_id.clone())
            .ok_or_else(|| ApiError::not_found("artifact", mark_id))?;
        Some(read_artifact_bytes(state, &mark_artifact).await?)
    } else {
        None
    };

    let engine = ImageEngine::new();
    let op_name = op.to_string();
    let outcome = tokio::task::spawn_blocking(move || {
        run_image_op_sync(&engine, &input, mark.as_deref(), &op_name, &params)
    })
    .await
    .map_err(|e| ApiError::internal(format!("image op task failed: {e}")))?
    .map_err(ApiError::from)?;

    let qualified_op = format!("image.{op}");
    match outcome {
        ImageOpOutput::Report(result) => Ok((
            StatusCode::OK,
            Json(json!({
                "op": qualified_op,
                "document_id": document_id.as_str(),
                "artifact_id": artifact.id,
                "result": result,
            })),
        )),
        ImageOpOutput::Bytes {
            data,
            extension,
            mime_type,
        } => {
            let output =
                store_op_output(state, document_id, op, extension, mime_type, &data).await?;
            Ok((
                StatusCode::OK,
                Json(json!({
                    "op": qualified_op,
                    "document_id": document_id.as_str(),
                    "source_artifact_id": artifact.id,
                    "artifact": output,
                })),
            ))
        }
    }
}

/// Synchronous dispatch over [`ImageEngine`] — runs on a blocking
/// thread (see [`run_image_op`]). Parameter problems surface as
/// [`ImageError::InvalidInput`] → `400`.
fn run_image_op_sync(
    engine: &ImageEngine,
    input: &[u8],
    mark: Option<&[u8]>,
    op: &str,
    params: &Value,
) -> Result<ImageOpOutput, ImageError> {
    match op {
        "inspect" => Ok(ImageOpOutput::Report(json!(engine.inspect(input)?))),
        "metadata" => Ok(ImageOpOutput::Report(json!(engine.metadata(input)?))),
        "validate" => Ok(ImageOpOutput::Report(json!(engine.validate(input)?))),
        "convert" => {
            let target = params
                .get("format")
                .and_then(Value::as_str)
                .map(str::trim)
                .filter(|s| !s.is_empty())
                .ok_or_else(|| {
                    ImageError::InvalidInput("convert requires params.format".to_string())
                })?;
            let data = engine.convert(input, target)?;
            let (extension, mime_type) = image_format_ext_mime(&normalize_format_name(target));
            Ok(ImageOpOutput::Bytes {
                data,
                extension,
                mime_type,
            })
        }
        "resize" => {
            let options = ResizeOptions {
                max_width: opt_u32(params, "max_width")?,
                max_height: opt_u32(params, "max_height")?,
            };
            preserve_format(engine, input, engine.resize(input, options)?)
        }
        "rotate" => {
            let degrees = req_u16(params, "degrees")?;
            preserve_format(engine, input, engine.rotate(input, degrees)?)
        }
        "flip" => {
            let direction = match params.get("direction") {
                None | Some(Value::Null) => FlipDirection::Horizontal,
                Some(v) => serde_json::from_value::<FlipDirection>(v.clone()).map_err(|e| {
                    ImageError::InvalidInput(format!(
                        "flip direction must be horizontal, vertical, or both: {e}"
                    ))
                })?,
            };
            preserve_format(engine, input, engine.flip(input, direction)?)
        }
        "crop" => {
            let data = engine.crop(
                input,
                req_u32(params, "x")?,
                req_u32(params, "y")?,
                req_u32(params, "width")?,
                req_u32(params, "height")?,
            )?;
            preserve_format(engine, input, data)
        }
        "thumbnail" => {
            let data = engine.thumbnail(
                input,
                req_u32(params, "max_width")?,
                req_u32(params, "max_height")?,
            )?;
            preserve_format(engine, input, data)
        }
        "compress" => {
            let options: CompressOptions = serde_json::from_value(params.clone())
                .map_err(|e| ImageError::InvalidInput(format!("invalid compress params: {e}")))?;
            let data = engine.compress(input, options)?;
            let format = options
                .format
                .map(|f| f.as_str().to_string())
                .unwrap_or_else(|| engine.validate(input).map(|v| v.format).unwrap_or_default());
            let (extension, mime_type) = image_format_ext_mime(&format);
            Ok(ImageOpOutput::Bytes {
                data,
                extension,
                mime_type,
            })
        }
        "watermark" => {
            let mark = mark.ok_or_else(|| {
                ImageError::InvalidInput("watermark requires params.mark_artifact_id".to_string())
            })?;
            let mut options = WatermarkOptions::default();
            if let Some(v) = params.get("position") {
                options.position = serde_json::from_value(v.clone()).map_err(|e| {
                    ImageError::InvalidInput(format!("invalid watermark position: {e}"))
                })?;
            }
            if let Some(v) = params.get("opacity") {
                options.opacity = v.as_f64().map(|f| f as f32).ok_or_else(|| {
                    ImageError::InvalidInput("opacity must be a number".to_string())
                })?;
            }
            if let Some(v) = params.get("scale") {
                options.scale = match v {
                    Value::Null => None,
                    _ => Some(v.as_f64().map(|f| f as f32).ok_or_else(|| {
                        ImageError::InvalidInput("scale must be a number".to_string())
                    })?),
                };
            }
            if let Some(v) = params.get("margin") {
                options.margin = opt_u32_at(v, "margin")?.unwrap_or(options.margin);
            }
            if let Some(v) = params.get("tile_spacing") {
                options.tile_spacing =
                    opt_u32_at(v, "tile_spacing")?.unwrap_or(options.tile_spacing);
            }
            preserve_format(engine, input, engine.watermark(input, mark, &options)?)
        }
        other => Err(ImageError::InvalidInput(format!(
            "unknown image op {other:?}; supported: inspect, metadata, validate, convert, \
             resize, rotate, flip, crop, thumbnail, compress, watermark"
        ))),
    }
}

/// Wraps byte output in the input's own format (resize/rotate/flip/
/// crop/thumbnail/watermark preserve the encoding).
fn preserve_format(
    engine: &ImageEngine,
    input: &[u8],
    data: Vec<u8>,
) -> Result<ImageOpOutput, ImageError> {
    let format = engine.validate(input)?.format;
    let (extension, mime_type) = image_format_ext_mime(&format);
    Ok(ImageOpOutput::Bytes {
        data,
        extension,
        mime_type,
    })
}

/// Resolves the artifact an op runs against: the named artifact (must
/// belong to the document) or the most recently created one.
async fn resolve_op_artifact(
    documents: &DocumentService,
    document_id: &DocumentId,
    artifact_id: Option<&str>,
) -> Result<DocumentArtifact, ApiError> {
    match artifact_id {
        Some(aid) => documents
            .download_artifact(aid)
            .await?
            .filter(|a| a.document_id == document_id.clone())
            .ok_or_else(|| ApiError::not_found("artifact", aid)),
        None => documents
            .list_artifacts(document_id)
            .await?
            .into_iter()
            .next_back()
            .ok_or_else(|| ApiError::not_found("artifact", "most recent for document")),
    }
}

/// Reads an artifact's stored bytes, bounded by [`MAX_OP_INPUT_BYTES`].
/// Same `local://` confinement as [`download_artifact`].
async fn read_artifact_bytes(
    state: &ApiState,
    artifact: &DocumentArtifact,
) -> Result<Vec<u8>, ApiError> {
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
    let mut reader = StreamingBackend::open_reader(&mut fs, &name)
        .map_err(|e| ApiError::from(ServiceError::Storage(e)))?;

    let mut buf = Vec::new();
    (&mut reader)
        .take(MAX_OP_INPUT_BYTES.saturating_add(1))
        .read_to_end(&mut buf)
        .map_err(|e| ApiError::internal(format!("storage read failed: {e}")))?;
    if buf.len() as u64 > MAX_OP_INPUT_BYTES {
        return Err(ApiError::payload_too_large(
            "artifact exceeds the op input size limit",
        ));
    }
    Ok(buf)
}

/// Writes op output bytes under `documents/{id}/outputs/` and registers
/// them as a new artifact (with a SHA-256 checksum, like ingestion).
async fn store_op_output(
    state: &ApiState,
    document_id: &DocumentId,
    op: &str,
    extension: &str,
    mime_type: &str,
    data: &[u8],
) -> Result<DocumentArtifact, ApiError> {
    let object_name = format!(
        "documents/{document_id}/outputs/{op}-{}.{extension}",
        gen_id("img")
    );
    let mut fs = FaultInjectingFs::new(state.storage_config.clone())
        .map_err(|e| ApiError::internal(format!("storage init failed: {e}")))?;
    fs.write(&object_name, data)
        .map_err(|e| ApiError::from(ServiceError::Storage(e)))?;

    let file_name = object_name
        .rsplit('/')
        .next()
        .unwrap_or("output")
        .to_string();
    state
        .services
        .documents()
        .upload_artifact(&NewArtifact {
            id: None,
            document_id: document_id.clone(),
            storage_uri: format!("local://{object_name}"),
            file_name,
            mime_type: Some(mime_type.to_string()),
            size_bytes: data.len() as u64,
            checksum: Some(DocumentChecksum {
                algorithm: ChecksumAlgorithm::Sha256,
                value: hex::encode(Sha256::digest(data)),
            }),
        })
        .await
        .map_err(ApiError::from)
}

/// Lowercases a format name and maps the `jpg` alias onto `jpeg`.
fn normalize_format_name(name: &str) -> String {
    match name.trim().to_ascii_lowercase().as_str() {
        "jpg" => "jpeg".to_string(),
        other => other.to_string(),
    }
}

/// File extension + MIME type for an image format identifier
/// (`"png"`, `"jpeg"`, ...). Unknown identifiers degrade to a generic
/// binary type — the engine has already validated the format by the
/// time this runs.
fn image_format_ext_mime(format: &str) -> (&'static str, &'static str) {
    match format {
        "png" => ("png", "image/png"),
        "jpeg" => ("jpg", "image/jpeg"),
        "gif" => ("gif", "image/gif"),
        "webp" => ("webp", "image/webp"),
        "bmp" => ("bmp", "image/bmp"),
        _ => ("bin", "application/octet-stream"),
    }
}

/// Optional `u32` param: absent/null → `None`; non-integer →
/// [`ImageError::InvalidInput`].
fn opt_u32(params: &Value, key: &str) -> Result<Option<u32>, ImageError> {
    match params.get(key) {
        None | Some(Value::Null) => Ok(None),
        Some(v) => v
            .as_u64()
            .and_then(|n| u32::try_from(n).ok())
            .map(Some)
            .ok_or_else(|| ImageError::InvalidInput(format!("{key} must be a positive integer"))),
    }
}

/// [`opt_u32`] over an already-extracted value (for watermark fields).
fn opt_u32_at(value: &Value, key: &str) -> Result<Option<u32>, ImageError> {
    value
        .as_u64()
        .and_then(|n| u32::try_from(n).ok())
        .map(Some)
        .ok_or_else(|| ImageError::InvalidInput(format!("{key} must be a positive integer")))
}

/// Required `u32` param — missing or non-integer is an input error.
fn req_u32(params: &Value, key: &str) -> Result<u32, ImageError> {
    opt_u32(params, key)?.ok_or_else(|| {
        ImageError::InvalidInput(format!("{key} is required and must be a positive integer"))
    })
}

/// Required `u16` param (image `rotate` degrees).
fn req_u16(params: &Value, key: &str) -> Result<u16, ImageError> {
    params
        .get(key)
        .and_then(Value::as_u64)
        .and_then(|n| u16::try_from(n).ok())
        .ok_or_else(|| {
            ImageError::InvalidInput(format!("{key} is required and must be an integer"))
        })
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
        "batch_id": job.batch_id,
        "progress": job.progress,
    })
}

// ---------------------------------------------------------------------------
// Document permissions — grant / revoke / list
// ---------------------------------------------------------------------------

/// Lists the permission grants recorded on a document. Requires `read`
/// access on the document.
pub async fn list_document_permissions(
    State(state): State<Arc<ApiState>>,
    Extension(user): Extension<AuthUser>,
    Path(id): Path<String>,
) -> Result<Json<Value>, ApiError> {
    let document_id = DocumentId::new(id.clone());
    require_document_access(&state, &document_id, &id, &user, DocumentAccessLevel::Read).await?;
    let permissions = state
        .services
        .documents()
        .get_document_permissions(&document_id)
        .await?;
    Ok(Json(json!({ "permissions": permissions })))
}

/// Body for [`grant_document_permission`]. `access_level` is one of
/// `"read"`, `"write"`, `"manage"` (serde snake_case on
/// [`DocumentAccessLevel`]).
#[derive(Debug, Deserialize)]
pub struct GrantPermissionRequest {
    /// Subject the grant applies to — a user id, `apikey:<name>`,
    /// group id, or role id (meaning is defined by the auth layer).
    subject_id: String,
    access_level: DocumentAccessLevel,
}

/// Grants or updates a subject's access level on a document.
/// Requires `manage` access — permission management is owner/admin
/// territory, not something a `write` collaborator should do.
pub async fn grant_document_permission(
    State(state): State<Arc<ApiState>>,
    Extension(user): Extension<AuthUser>,
    Path(id): Path<String>,
    Json(body): Json<GrantPermissionRequest>,
) -> Result<(StatusCode, Json<Value>), ApiError> {
    if body.subject_id.trim().is_empty() {
        return Err(ApiError::bad_request("subject_id must not be empty"));
    }
    let document_id = DocumentId::new(id.clone());
    require_document_access(
        &state,
        &document_id,
        &id,
        &user,
        DocumentAccessLevel::Manage,
    )
    .await?;
    let permission = state
        .services
        .documents()
        .grant_permission(
            &document_id,
            body.subject_id.trim(),
            body.access_level,
            Some(user.actor_id.as_str()),
        )
        .await?;
    Ok((
        StatusCode::CREATED,
        Json(json!({ "permission": permission })),
    ))
}

/// Revokes a subject's permission on a document. `revoked: false`
/// when no grant existed (still `200`). Requires `manage` access.
pub async fn revoke_document_permission(
    State(state): State<Arc<ApiState>>,
    Extension(user): Extension<AuthUser>,
    Path((id, subject)): Path<(String, String)>,
) -> Result<Json<Value>, ApiError> {
    let document_id = DocumentId::new(id.clone());
    require_document_access(
        &state,
        &document_id,
        &id,
        &user,
        DocumentAccessLevel::Manage,
    )
    .await?;
    let revoked = state
        .services
        .documents()
        .revoke_permission(&document_id, &subject, Some(user.actor_id.as_str()))
        .await?;
    Ok(Json(json!({ "revoked": revoked })))
}

/// Lists every grant `subject` holds across **all** documents —
/// broader than the `{id}` path suggests, so it is restricted: the
/// caller must either be querying their own actor id, or hold `manage`
/// on the path document. Requires `read` on the path document
/// regardless.
pub async fn list_subject_permissions(
    State(state): State<Arc<ApiState>>,
    Extension(user): Extension<AuthUser>,
    Path((id, subject)): Path<(String, String)>,
) -> Result<Json<Value>, ApiError> {
    let document_id = DocumentId::new(id.clone());
    require_document_access(&state, &document_id, &id, &user, DocumentAccessLevel::Read).await?;
    if subject != user.actor_id {
        require_document_access(
            &state,
            &document_id,
            &id,
            &user,
            DocumentAccessLevel::Manage,
        )
        .await?;
    }
    let permissions = state
        .services
        .documents()
        .list_permissions_for_subject(&subject)
        .await?;
    Ok(Json(
        json!({ "subject_id": subject, "permissions": permissions }),
    ))
}

// ---------------------------------------------------------------------------
// Document relationships — create / list / related / delete
// ---------------------------------------------------------------------------

/// Lists a document's relationships in both directions:
/// `{outgoing: [...], incoming: [...]}`. Requires `read` access.
pub async fn list_document_relationships(
    State(state): State<Arc<ApiState>>,
    Extension(user): Extension<AuthUser>,
    Path(id): Path<String>,
) -> Result<Json<Value>, ApiError> {
    let document_id = DocumentId::new(id.clone());
    require_document_access(&state, &document_id, &id, &user, DocumentAccessLevel::Read).await?;
    let documents = state.services.documents();
    let outgoing = documents.list_outgoing_relationships(&document_id).await?;
    let incoming = documents.list_incoming_relationships(&document_id).await?;
    Ok(Json(json!({
        "outgoing": outgoing,
        "incoming": incoming,
    })))
}

/// Returns all documents related to this one (both directions,
/// deduplicated) as `{related: [{document, relationship_type,
/// outgoing}]}`. Requires `read` access.
pub async fn related_documents(
    State(state): State<Arc<ApiState>>,
    Extension(user): Extension<AuthUser>,
    Path(id): Path<String>,
) -> Result<Json<Value>, ApiError> {
    let document_id = DocumentId::new(id.clone());
    require_document_access(&state, &document_id, &id, &user, DocumentAccessLevel::Read).await?;
    let related = state
        .services
        .documents()
        .related_documents(&document_id)
        .await?;
    Ok(Json(json!({ "related": related })))
}

/// Body for [`create_document_relationship`].
#[derive(Debug, Deserialize)]
pub struct CreateRelationshipRequest {
    /// The document the path document links **to**.
    target_document_id: String,
    /// Relationship kind; absent = `"related"`.
    #[serde(default = "default_relationship_type")]
    relationship_type: DocumentRelationshipType,
}

fn default_relationship_type() -> DocumentRelationshipType {
    DocumentRelationshipType::Related
}

/// Creates a directed relationship from the path document to
/// `target_document_id`. Requires `write` on the source **and** `read`
/// on the target — linking to a document you cannot see would leak its
/// existence.
pub async fn create_document_relationship(
    State(state): State<Arc<ApiState>>,
    Extension(user): Extension<AuthUser>,
    Path(id): Path<String>,
    Json(body): Json<CreateRelationshipRequest>,
) -> Result<(StatusCode, Json<Value>), ApiError> {
    if body.target_document_id.trim().is_empty() {
        return Err(ApiError::bad_request(
            "target_document_id must not be empty",
        ));
    }
    let document_id = DocumentId::new(id.clone());
    require_document_access(&state, &document_id, &id, &user, DocumentAccessLevel::Write).await?;
    let target_id = DocumentId::new(body.target_document_id.trim().to_string());
    require_document_access(
        &state,
        &target_id,
        body.target_document_id.trim(),
        &user,
        DocumentAccessLevel::Read,
    )
    .await?;

    let relationship = state
        .services
        .documents()
        .create_relationship(
            &document_id,
            &target_id,
            body.relationship_type,
            Some(user.actor_id.as_str()),
        )
        .await?;
    Ok((
        StatusCode::CREATED,
        Json(json!({ "relationship": relationship })),
    ))
}

/// Deletes a relationship that touches the path document. The
/// relationship id is looked up in the document's outgoing **and**
/// incoming lists first, so ids belonging to unrelated documents 404.
/// Requires `write` access on the path document.
pub async fn delete_document_relationship(
    State(state): State<Arc<ApiState>>,
    Extension(user): Extension<AuthUser>,
    Path((id, rel_id)): Path<(String, String)>,
) -> Result<Json<Value>, ApiError> {
    let document_id = DocumentId::new(id.clone());
    require_document_access(&state, &document_id, &id, &user, DocumentAccessLevel::Write).await?;

    let documents = state.services.documents();
    let outgoing = documents.list_outgoing_relationships(&document_id).await?;
    let incoming = documents.list_incoming_relationships(&document_id).await?;
    let touches =
        outgoing.iter().any(|r| r.id == rel_id) || incoming.iter().any(|r| r.id == rel_id);
    if !touches {
        return Err(ApiError::not_found("relationship", rel_id));
    }

    let deleted = documents.delete_relationship(&rel_id).await?;
    Ok(Json(json!({ "deleted": deleted })))
}

// ---------------------------------------------------------------------------
// Schedules — recurring job definitions (see services::schedules)
// ---------------------------------------------------------------------------

/// Stable JSON view of a [`Schedule`](crate::crates::scheduler::Schedule)
/// (the type itself is not `Serialize`). Timestamps are Unix seconds.
fn schedule_to_json(schedule: &crate::crates::scheduler::Schedule) -> Value {
    json!({
        "id": schedule.id,
        "name": schedule.name,
        "kind": schedule.kind,
        "payload": schedule.payload,
        "interval_secs": schedule.interval_secs,
        "priority": schedule.priority,
        "next_run_at": schedule.next_run_at,
        "created_at": schedule.created_at,
        "last_run_at": schedule.last_run_at,
        "enabled": schedule.enabled,
    })
}

/// Lists all recurring schedules. Schedules are global (not
/// owner-scoped) — any authenticated caller may read them.
pub async fn list_schedules(
    State(state): State<Arc<ApiState>>,
    Extension(_user): Extension<AuthUser>,
) -> Result<Json<Value>, ApiError> {
    let service = ScheduleService::connect(state.services.pool()).await?;
    let schedules = service.list().await?;
    Ok(Json(json!({
        "schedules": schedules.iter().map(schedule_to_json).collect::<Vec<_>>(),
    })))
}

/// Body for [`create_schedule`].
#[derive(Debug, Deserialize)]
pub struct CreateScheduleRequest {
    /// Unique name; upserting an existing name replaces the schedule.
    name: String,
    /// Job kind enqueued on each occurrence (must have a registered
    /// handler when it fires — see `job_engine`).
    kind: String,
    /// JSON payload copied into every enqueued job.
    #[serde(default = "empty_object")]
    payload: Value,
    /// Seconds between occurrences (>= 1).
    interval_secs: u64,
    /// Job priority; absent = 0.
    priority: Option<i64>,
    /// Unix seconds for the first occurrence; absent =
    /// `now + interval_secs`.
    next_run_at: Option<i64>,
    /// `false` pauses the schedule; absent = `true`.
    enabled: Option<bool>,
}

/// Creates or replaces a recurring schedule (upsert by `name`).
/// Returns `200` with the stored schedule — the same endpoint covers
/// create and update since `name` is the identity.
pub async fn create_schedule(
    State(state): State<Arc<ApiState>>,
    Extension(_user): Extension<AuthUser>,
    Json(body): Json<CreateScheduleRequest>,
) -> Result<Json<Value>, ApiError> {
    if !body.payload.is_object() {
        return Err(ApiError::bad_request("payload must be a JSON object"));
    }
    let service = ScheduleService::connect(state.services.pool()).await?;
    let schedule = service
        .upsert(&NewSchedule {
            name: body.name,
            kind: body.kind,
            payload: body.payload,
            interval_secs: body.interval_secs,
            priority: body.priority,
            next_run_at: body.next_run_at,
            enabled: body.enabled,
        })
        .await?;
    Ok(Json(json!({ "schedule": schedule_to_json(&schedule) })))
}

/// Deletes the schedule named `name`. `404` when none exists.
pub async fn delete_schedule(
    State(state): State<Arc<ApiState>>,
    Extension(_user): Extension<AuthUser>,
    Path(name): Path<String>,
) -> Result<Json<Value>, ApiError> {
    let service = ScheduleService::connect(state.services.pool()).await?;
    if !service.remove(&name).await? {
        return Err(ApiError::not_found("schedule", name));
    }
    Ok(Json(json!({ "deleted": true })))
}

/// Body for [`tick_schedules`].
#[derive(Debug, Deserialize)]
pub struct TickSchedulesRequest {
    /// Unix seconds to tick at; absent = now. Mostly useful for tests
    /// and catch-up runs.
    now: Option<i64>,
}

/// Runs one scheduler tick immediately, enqueueing a job for every due
/// schedule. This is the same routine the engine-driven
/// `scheduler.tick` job performs; exposed for administration/testing.
pub async fn tick_schedules(
    State(state): State<Arc<ApiState>>,
    Extension(_user): Extension<AuthUser>,
    Json(body): Json<TickSchedulesRequest>,
) -> Result<Json<Value>, ApiError> {
    let service = ScheduleService::connect(state.services.pool()).await?;
    let outcome = service.tick(body.now).await?;
    Ok(Json(json!({ "enqueued": outcome.enqueued })))
}
