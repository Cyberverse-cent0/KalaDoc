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
//! | `GET /engines`                             | yes  | Engine registry + per-engine health       |
//! | `GET /storage`                             | yes  | Storage backend status                    |
//! | `POST /documents`                          | yes  | Upload bytes → validate → ingest          |
//! | `GET /documents`                           | yes  | List (`?q=` search, owner/folder filters) |
//! | `GET /documents/{id}`                      | yes  | Document metadata + artifacts             |
//! | `PATCH /documents/{id}`                    | yes  | Rename / retype / move to folder          |
//! | `DELETE /documents/{id}`                   | yes  | Trash (soft delete)                       |
//! | `POST /documents/{id}/restore`             | yes  | Restore a trashed document                |
//! | `GET /documents/{id}/versions`             | yes  | Version list                              |
//! | `POST /documents/{id}/versions/{n}/restore`| yes  | Restore version pointer                   |
//! | `GET /documents/{id}/tags`                 | yes  | Assigned tags                             |
//! | `PUT /documents/{id}/tags`                 | yes  | Assign a tag (`{"name": ...}`)            |
//! | `DELETE /documents/{id}/tags/{name}`       | yes  | Remove a tag assignment                   |
//! | `GET /documents/{id}/history`              | yes  | Audit event history                       |
//! | `GET /documents/{id}/artifacts/{aid}/download` | yes | Download artifact bytes               |
//! | `POST /documents/{id}/process`             | yes  | Enqueue processing job (`metadata_extract`, `index`, `thumbnail`, or any pdf op via `op` + `params`) |
//! | `GET /folders`                             | yes  | List folders                              |
//! | `POST /folders`                            | yes  | Create folder                             |
//! | `GET /jobs`                                | yes  | List jobs (`?status=` filter)             |
//! | `GET /jobs/{id}`                           | yes  | Job status from the durable queue         |
//! | `POST /jobs/{id}/cancel`                   | yes  | Cancel a pending/running job              |
//! | `POST /jobs/{id}/retry`                    | yes  | Requeue a failed job                      |
//!
//! Authenticated routes require `Authorization: Bearer <session-token>`;
//! tokens are validated against `sessions_tbl` on every request.
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
pub mod handlers;
pub mod state;

#[cfg(test)]
mod tests;

use std::net::SocketAddr;
use std::sync::Arc;

use axum::routing::{delete, get, post};
use axum::{Json, Router, middleware};
use serde_json::Value;

pub use error::ApiError;
pub use state::ApiState;

/// Builds the application router.
///
/// `GET /health` is public; everything else sits behind the Bearer
/// session middleware in [`auth`]. Handlers extract
/// [`state::ApiState`] via `State<Arc<ApiState>>`.
pub fn router(state: Arc<ApiState>) -> Router {
    let protected = Router::new()
        .route("/engines", get(handlers::engines))
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
        .route("/documents/{id}/versions", get(handlers::list_versions))
        .route(
            "/documents/{id}/versions/{version}/restore",
            post(handlers::restore_version),
        )
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
        .route(
            "/folders",
            get(handlers::list_folders).post(handlers::create_folder),
        )
        .route("/jobs", get(handlers::list_jobs_route))
        .route("/jobs/{id}", get(handlers::get_job))
        .route("/jobs/{id}/cancel", post(handlers::cancel_job_route))
        .route("/jobs/{id}/retry", post(handlers::retry_job_route))
        .layer(middleware::from_fn_with_state(
            state.clone(),
            auth::require_auth,
        ));

    Router::new()
        .route("/health", get(handlers::health))
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
