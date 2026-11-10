//! HTTP API layer — thin axum adapter over the application services.
//!
//! The API layer sits **above** the job engine and application services
//! (see `.doc/Architecture.md`): it authenticates requests against
//! Organization sessions, validates uploads through
//! `security_engine::validate_upload`, delegates persistence to
//! [`AppServices`] (which owns the document store and the streaming
//! ingestion pipeline used by the `document.ingest` job handler), and
//! enqueues follow-up jobs through `job_engine`. No business rules live
//! here — authorization scoping, quotas, and document invariants stay
//! in the layers below.
//!
//! # Endpoints
//!
//! | Method & path                              | Auth | Purpose                                   |
//! |--------------------------------------------|------|-------------------------------------------|
//! | `GET /health`                              | no   | Aggregated engine health                  |
//! | `GET /openapi.json`                         | no   | OpenAPI 3.x description of this API       |
//! | `GET /engines`                             | yes  | Engine registry + per-engine health       |
//! | `GET /engines/{name}/operations`           | yes  | Per-engine capability detail              |
//! | `GET /storage`                             | yes  | Storage backend status                    |
//! | `POST /documents`                          | yes  | Upload bytes → validate → ingest          |
//! | `GET /documents`                           | yes  | List (`?q=` search, owner/folder filters) |
//! | `GET /documents/{id}`                      | yes  | Document metadata + artifacts             |
//! | `PATCH /documents/{id}`                    | yes  | Rename / retype / move to folder          |
//! | `DELETE /documents/{id}`                   | yes  | Trash (soft delete)                       |
//! | `POST /documents/{id}/restore`             | yes  | Restore a trashed document                |
//! | `GET /documents/{id}/versions`             | yes  | Version list                              |
//! | `POST /documents/{id}/versions`            | yes  | Create a new version (`write`)            |
//! | `POST /documents/{id}/versions/{n}/restore`| yes  | Restore version pointer                   |
//! | `POST /documents/{id}/copy`                | yes  | Duplicate document (`read` on source)     |
//! | `POST /documents/{id}/archive`             | yes  | Archive (`write`)                         |
//! | `POST /documents/{id}/unarchive`           | yes  | Unarchive (`write`)                       |
//! | `POST /documents/{id}/purge`               | yes  | Permanently delete a trashed doc (`manage`)|
//! | `GET/POST/DELETE /documents/{id}/permissions` | yes | Permission grants (`manage` to mutate) |
//! | `GET /documents/{id}/permissions/subject/{s}` | yes | Grants held by a subject             |
//! | `GET/POST/DELETE /documents/{id}/relationships` | yes | Document links (`write` to mutate)   |
//! | `GET /documents/{id}/related`              | yes  | Deduplicated related documents           |
//! | `PATCH/DELETE /folders/{id}`               | yes  | Rename/move/delete folder (owner only)   |
//! | `GET /documents/{id}/tags`                 | yes  | Assigned tags                             |
//! | `PUT /documents/{id}/tags`                 | yes  | Assign a tag (`{"name": ...}`)            |
//! | `DELETE /documents/{id}/tags/{name}`       | yes  | Remove a tag assignment                   |
//! | `GET /documents/{id}/history`              | yes  | Audit event history                       |
//! | `GET /documents/{id}/artifacts/{aid}/download` | yes | Download artifact bytes               |
//! | `POST /documents/{id}/process`             | yes  | Enqueue processing job (`metadata_extract`, `index`, `thumbnail`, or any pdf op via `op` + `params`) |
//! | `POST /documents/{id}/ops/{op}`            | yes  | Generic engine op: `image.*` runs synchronously (output → new artifact), `pdf.*`/bare pdf ops and `pipeline.run` enqueue jobs |
//! | `GET/PATCH/DELETE /documents/{id}/settings`| yes  | Per-document settings blob + `is_locked`  |
//! | `GET/PATCH/DELETE /documents/{id}/metadata`| yes  | Descriptive metadata row                   |
//! | `PUT/DELETE /documents/{id}/metadata/fields/{key}` | yes | Metadata custom-field set/remove   |
//! | `GET /pipelines`                          | yes  | List built-in pipeline definitions        |
//! | `GET /pipelines/{name}`                   | yes  | One built-in pipeline definition (404 unknown) |
//! | `POST /pipelines/run`                       | yes  | Enqueue a `pipeline.run` job (built-in name or full PipelineDefinition) |
//! | `GET /folders`                             | yes  | List folders                              |
//! | `POST /folders`                            | yes  | Create folder                             |
//! | `GET /jobs`                                | yes  | List jobs (`?status=` filter)             |
//! | `GET /jobs/{id}`                           | yes  | Job status from the durable queue         |
//! | `POST /jobs/{id}/cancel`                   | yes  | Cancel a pending/running job              |
//! | `POST /jobs/{id}/retry`                    | yes  | Requeue a failed job                      |
//! | `POST /jobs/batch`                         | yes  | Enqueue a batch of jobs (shared `batch_id`) |
//! | `GET /jobs/batch/{id}`                     | yes  | Batch progress + member jobs              |
//! | `POST /jobs/batch/{id}/cancel`             | yes  | Cancel all non-terminal jobs in a batch   |
//! | `GET/POST/DELETE /schedules`               | yes  | Recurring job schedules (global)          |
//! | `POST /schedules/tick`                     | yes  | Run one scheduler tick now                |
//! | `GET /media/capabilities`                  | yes  | ffmpeg capability snapshot (`media` module) |
//! | `POST /media/probe`                        | yes  | ffprobe inspection of uploaded media bytes |
//! | `POST /media/thumbnail`                    | yes  | PNG frame at `?time=`/`?width=` (`media` module) |
//!
//! Authenticated routes require `Authorization: Bearer <credential>`;
//! the credential is either an Organization session token (validated
//! against `sessions_tbl` on every request) or a static API key from
//! `api.api_keys` / `KALADOC_API_KEYS` (`name:key` pairs — see
//! [`auth`]). API-key callers act as `apikey:<name>`.
//!
//! Route groups can be switched off with `api.disabled_groups` /
//! `KALADOC_API_DISABLE` (comma list of `documents`, `folders`,
//! `jobs`, `pipelines`, `engines`, `storage`, `schedules`); disabled
//! groups answer `404` — see [`flags`].
//!
//! # Authorization
//!
//! Every `/documents/{id}/…` route enforces document-level access
//! (see [`crate::crates::application::services::access`]): the caller
//! must own the document or hold a sufficient `document_permissions_tbl`
//! grant (`read` for GETs, `write` for mutations, `manage` for
//! permission management and purge). Denials are `403`; unknown ids
//! are `404`. `GET /documents` and `GET /folders` are always scoped to
//! the caller — the `owner_id` query parameter is accepted but
//! ignored. There is no admin role: API-key actors
//! (`apikey:<name>`) follow the same owner/permission rules as session
//! users. In-process callers (CLI) bypass this layer by design.
//!
//! # Errors
//!
//! Every error response uses the stable JSON shape from the CLI spec
//! §5.3: `{"error": {"code", "message", "request_id", "retryable"}}`.
//! See [`error::ApiError`].
//!
//! # Running
//!
//! ```no_run
//! # async fn example() -> Result<(), Box<dyn std::error::Error>> {
//! let config = kaladoc::config::AppConfig::load()?;
//! let state = std::sync::Arc::new(kaladoc::crates::api::ApiState::init(&config).await?);
//! let addr: std::net::SocketAddr = config.api.addr.parse()?;
//! kaladoc::crates::api::serve(state, addr).await?;
//! # Ok(())
//! # }
//! ```

pub mod auth;
pub mod error;
pub mod flags;
pub mod handlers;
mod media;
pub mod openapi;
pub mod state;

#[cfg(test)]
mod tests;

use std::net::SocketAddr;
use std::sync::Arc;

use axum::routing::{delete, get, post, put};
use axum::{Json, Router, middleware};
use serde_json::Value;

pub use error::ApiError;
pub use state::ApiState;

/// Builds the application router.
///
/// `GET /health` and `GET /openapi.json` are public; everything else
/// sits behind the Bearer session middleware in [`auth`]. Handlers
/// extract [`state::ApiState`] via `State<Arc<ApiState>>`.
pub fn router(state: Arc<ApiState>) -> Router {
    let protected = Router::new()
        .route("/engines", get(handlers::engines))
        .route(
            "/engines/{name}/operations",
            get(handlers::engine_operations),
        )
        .route("/storage", get(handlers::storage_status))
        .route(
            "/documents",
            post(handlers::create_document).get(handlers::list_documents),
        )
        .route(
            "/documents/{id}",
            get(handlers::get_document)
                .patch(handlers::update_document)
                .delete(handlers::delete_document),
        )
        .route("/documents/{id}/restore", post(handlers::restore_document))
        .route(
            "/documents/{id}/versions",
            get(handlers::list_versions).post(handlers::create_document_version),
        )
        .route(
            "/documents/{id}/versions/{version}/restore",
            post(handlers::restore_version),
        )
        .route("/documents/{id}/copy", post(handlers::copy_document))
        .route("/documents/{id}/archive", post(handlers::archive_document))
        .route(
            "/documents/{id}/unarchive",
            post(handlers::unarchive_document),
        )
        .route("/documents/{id}/purge", post(handlers::purge_document))
        .route(
            "/documents/{id}/permissions",
            get(handlers::list_document_permissions).post(handlers::grant_document_permission),
        )
        .route(
            "/documents/{id}/permissions/subject/{subject}",
            get(handlers::list_subject_permissions),
        )
        .route(
            "/documents/{id}/permissions/{subject}",
            delete(handlers::revoke_document_permission),
        )
        .route(
            "/documents/{id}/relationships",
            get(handlers::list_document_relationships).post(handlers::create_document_relationship),
        )
        .route(
            "/documents/{id}/relationships/{rel_id}",
            delete(handlers::delete_document_relationship),
        )
        .route("/documents/{id}/related", get(handlers::related_documents))
        .route(
            "/documents/{id}/tags",
            get(handlers::list_tags).put(handlers::add_tag),
        )
        .route("/documents/{id}/tags/{name}", delete(handlers::remove_tag))
        .route("/documents/{id}/history", get(handlers::document_history))
        .route(
            "/documents/{id}/artifacts/{artifact_id}/download",
            get(handlers::download_artifact),
        )
        .route("/documents/{id}/process", post(handlers::process_document))
        .route("/documents/{id}/ops/{op}", post(handlers::run_document_op))
        .route(
            "/documents/{id}/settings",
            get(handlers::get_settings)
                .patch(handlers::patch_settings)
                .delete(handlers::delete_settings),
        )
        .route(
            "/documents/{id}/metadata",
            get(handlers::get_metadata)
                .patch(handlers::patch_metadata)
                .delete(handlers::delete_metadata),
        )
        .route(
            "/documents/{id}/metadata/fields/{key}",
            put(handlers::set_metadata_field).delete(handlers::remove_metadata_field),
        )
        .route("/pipelines", get(handlers::list_pipelines))
        .route("/pipelines/{name}", get(handlers::get_pipeline))
        .route("/pipelines/run", post(handlers::run_pipeline))
        .route(
            "/folders",
            get(handlers::list_folders).post(handlers::create_folder),
        )
        .route(
            "/folders/{id}",
            axum::routing::patch(handlers::update_folder).delete(handlers::delete_folder),
        )
        .route("/jobs", get(handlers::list_jobs_route))
        .route("/jobs/{id}", get(handlers::get_job))
        .route("/jobs/{id}/cancel", post(handlers::cancel_job_route))
        .route("/jobs/{id}/retry", post(handlers::retry_job_route))
        .route("/jobs/batch", post(handlers::create_job_batch))
        .route("/jobs/batch/{id}", get(handlers::get_job_batch))
        .route("/jobs/batch/{id}/cancel", post(handlers::cancel_job_batch))
        .route(
            "/schedules",
            get(handlers::list_schedules).post(handlers::create_schedule),
        )
        .route("/schedules/{name}", delete(handlers::delete_schedule))
        .route("/schedules/tick", post(handlers::tick_schedules))
        .route("/media/capabilities", get(media::capabilities))
        .route("/media/probe", post(media::probe))
        .route("/media/thumbnail", post(media::thumbnail))
        .layer(middleware::from_fn_with_state(
            state.clone(),
            auth::require_auth,
        ))
        // Outermost: disabled route groups 404 before auth runs.
        .layer(middleware::from_fn_with_state(
            state.clone(),
            flags::require_enabled,
        ));

    Router::new()
        .route("/health", get(handlers::health))
        .route("/openapi.json", get(handlers::openapi))
        .merge(protected)
        .fallback(fallback)
        .with_state(state)
}

/// JSON 404 for unmatched routes — same error envelope as everything else.
async fn fallback() -> ApiError {
    ApiError::not_found("route", "unknown")
}

/// Binds `addr` and serves the API until shutdown (Ctrl-C / SIGTERM).
///
/// # Errors
/// [`std::io::Error`] from binding or the accept loop.
pub async fn serve(state: Arc<ApiState>, addr: SocketAddr) -> std::io::Result<()> {
    let listener = tokio::net::TcpListener::bind(addr).await?;
    tracing::info!(%addr, "api server listening");
    axum::serve(listener, router(state))
        .with_graceful_shutdown(shutdown_signal())
        .await
}

async fn shutdown_signal() {
    let ctrl_c = async {
        let _ = tokio::signal::ctrl_c().await;
    };
    #[cfg(unix)]
    let terminate = async {
        match tokio::signal::unix::signal(tokio::signal::unix::SignalKind::terminate()) {
            Ok(mut sig) => {
                sig.recv().await;
            }
            Err(_) => std::future::pending::<()>().await,
        }
    };
    #[cfg(not(unix))]
    let terminate = std::future::pending::<()>();
    tokio::select! {
        _ = ctrl_c => {},
        _ = terminate => {},
    }
    tracing::info!("api server shutting down");
}

/// Serialize a value into an [`axum::Json`] response body, falling back
/// to `null` rather than failing a response that is already assembled.
pub(crate) fn json_body<T: serde::Serialize>(value: &T) -> Json<Value> {
    Json(serde_json::to_value(value).unwrap_or(Value::Null))
}
