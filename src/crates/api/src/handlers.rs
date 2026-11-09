//! Route handlers — HTTP ⇄ service adaptation only.
//!
//! Each handler is intentionally thin: parse/extract, call one
//! service function, map the result onto a status code + DTO. All
//! domain validation (payload contracts, file-name safety, storage
//! confinement, session expiry) lives below this layer.

use axum::Json;
use axum::extract::{Path, Query, State};
use axum::http::StatusCode;

use crate::crates::application::job_handlers::DOCUMENT_INGEST_KIND;
use crate::crates::job_engine::{self, NewJob};
use crate::crates::observability::{
    ComponentHealth, HealthStatus, OverallHealth, aggregate,
};

use super::AppState;
use super::auth::AuthenticatedSession;
use super::dto::{CreateDocumentRequest, JobResponse, JobsQuery};
use super::error::ApiError;

/// `GET /health` — aggregated engine health.
///
/// Runs every registered engine's health probe via
/// [`EngineRegistry::health_all`](crate::crates::engine_registry::EngineRegistry::health_all),
/// folds the per-component results with
/// [`observability::aggregate`], and returns the stable
/// [`OverallHealth`] JSON shape shared with `kaladoc doctor`.
///
/// Status code mirrors severity: `503` when the worst component is
/// `unavailable`, `200` otherwise (`healthy`/`degraded` still return
/// a body describing the problem).
pub async fn health(State(state): State<AppState>) -> (StatusCode, Json<OverallHealth>) {
    let entries = state.engines().health_all().await;
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
    let overall = aggregate(components);
    let code = match overall.status {
        HealthStatus::Unavailable => StatusCode::SERVICE_UNAVAILABLE,
        _ => StatusCode::OK,
    };
    (code, Json(overall))
}

/// `POST /documents` — enqueue a `document.ingest` job.
///
/// Requires a valid bearer session ([`AuthenticatedSession`]). The
/// request body mirrors the documented `document.ingest` payload
/// contract (`document_id`, exactly one of `content_b64`/`content`,
/// optional `file_name`/`mime_type`); the handler builds that JSON
/// verbatim and hands it to [`job_engine::enqueue`]. The actual
/// storage write + artifact registration happens asynchronously in
/// the job worker — hence `202 Accepted` with the queued job.
///
/// The API does **not** create the document row: `document.ingest`
/// attaches bytes to an existing document (see
/// `application::job_handlers` module docs). Document creation is a
/// separate endpoint/follow-up.
pub async fn create_document(
    State(state): State<AppState>,
    _auth: AuthenticatedSession,
    Json(req): Json<CreateDocumentRequest>,
) -> Result<(StatusCode, Json<JobResponse>), ApiError> {
    let (document_id, payload) = req.into_job_payload()?;
    debug_assert!(payload["document_id"].as_str() == Some(document_id.as_str()));

    let job = job_engine::enqueue(
        &state.pool,
        &NewJob {
            kind: DOCUMENT_INGEST_KIND.to_string(),
            payload,
            priority: 0,
            run_at: None,
            max_attempts: None,
        },
        state.job_config.default_max_attempts,
    )
    .await
    .map_err(|e| ApiError::Internal(format!("failed to enqueue job: {e}")))?;

    Ok((StatusCode::ACCEPTED, Json(JobResponse::from(job))))
}

/// `GET /jobs?status=&limit=` — list jobs newest-first.
///
/// `status` is one of `pending|running|succeeded|failed|cancelled`
/// (anything else → `400`); `limit` defaults to 100 and is clamped to
/// `[1, 1000]`. Backed directly by [`job_engine::list_jobs`].
pub async fn list_jobs(
    State(state): State<AppState>,
    _auth: AuthenticatedSession,
    Query(query): Query<JobsQuery>,
) -> Result<Json<Vec<JobResponse>>, ApiError> {
    let (status, limit) = query.into_parts()?;
    let jobs = job_engine::list_jobs(&state.pool, status, limit)
        .await
        .map_err(|e| ApiError::Internal(format!("failed to list jobs: {e}")))?;
    Ok(Json(jobs.into_iter().map(JobResponse::from).collect()))
}

/// `GET /jobs/{id}` — fetch a single job.
///
/// Scaffold note: `job_engine` exposes only the filtered list query,
/// so this scans `list_jobs` and picks the id. A dedicated
/// `get_job(pool, id)` store function is the proper follow-up; until
/// then the endpoint is correct but O(table size).
pub async fn get_job(
    State(state): State<AppState>,
    _auth: AuthenticatedSession,
    Path(id): Path<i64>,
) -> Result<Json<JobResponse>, ApiError> {
    let jobs = job_engine::list_jobs(&state.pool, None, i64::MAX)
        .await
        .map_err(|e| ApiError::Internal(format!("failed to fetch job: {e}")))?;
    jobs.into_iter()
        .find(|j| j.id == id)
        .map(|j| Json(JobResponse::from(j)))
        .ok_or_else(|| ApiError::NotFound(format!("job {id} not found")))
}
