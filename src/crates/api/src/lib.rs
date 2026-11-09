//! HTTP API layer — a thin `axum` adapter over the application
//! services, the job engine, and the engine registry.
//!
//! # Position in the architecture
//!
//! ```text
//!   HTTP client ──▶ api::router(AppState) ──▶ services / job_engine / engines
//! ```
//!
//! This crate owns **no** business logic: validation that matters
//! (payload contracts, file-name safety, storage confinement) stays
//! in the `security_engine`, the job handlers, and the document
//! store. The API's job is to translate HTTP requests into service
//! calls and service results back into status codes and JSON.
//!
//! # Endpoints
//!
//! | Method & path        | Auth | Purpose                                            |
//! |----------------------|------|----------------------------------------------------|
//! | `GET /health`        | no   | Aggregated engine health ([`handlers::health`])    |
//! | `POST /documents`    | yes  | Enqueue a `document.ingest` job                    |
//! | `GET /jobs`          | yes  | List jobs, optional `?status=`/`?limit=` filter    |
//! | `GET /jobs/{id}`     | yes  | Fetch one job by id                                |
//!
//! "Auth" means a valid `Authorization: Bearer <token>` session
//! token (see [`auth::AuthenticatedSession`]); missing, malformed,
//! expired, or revoked tokens all yield `401`.
//!
//! # Wiring
//!
//! The caller owns the database: [`AppState`] takes an already-open
//! [`SqlitePool`] — this module never runs migrations. Build the
//! router with [`router`], then serve it however you like, e.g.:
//!
//! ```no_run
//! # async fn example() -> Result<(), Box<dyn std::error::Error>> {
//! use kaladoc::crates::api::{AppState, router};
//! use kaladoc::crates::application::Organization::init_db;
//! let config = init_db::load_config()?;
//! let pool = init_db::open_db(&config).await?;
//! let state = AppState::from_config(pool, &kaladoc::config::AppConfig::load_lenient()?);
//! let listener = tokio::net::TcpListener::bind("127.0.0.1:8080").await?;
//! axum::serve(listener, router(state)).await?;
//! # Ok(())
//! # }
//! ```
//!
//! # Notes / known scaffold limitations
//!
//! - `POST /documents` enqueues a `document.ingest` job; the job
//!   handler requires the `document_id` to reference an **existing**
//!   document row (document creation is a separate concern — see the
//!   `document_engine` store / `DocumentService`).
//! - `GET /jobs/{id}` currently scans `list_jobs` — fine for a
//!   scaffold, but a dedicated `get_job` store query is the proper
//!   follow-up.
//! - Job endpoints are authenticated because job payloads can carry
//!   user data; only `/health` is public.

mod auth;
mod dto;
mod error;
mod handlers;

pub use auth::AuthenticatedSession;
pub use dto::{CreateDocumentRequest, JobResponse, JobsQuery};
pub use error::ApiError;

use std::sync::Arc;

use axum::Router;
use axum::routing::{get, post};
use sqlx::SqlitePool;

use crate::config::AppConfig;
use crate::crates::application::Organization::init_db::SessionConfig;
use crate::crates::engine_registry::EngineRegistry;
use crate::crates::job_engine::JobEngineConfig;

/// Shared state for every route — cloned cheaply into each request.
///
/// Holds exactly what the handlers need and nothing more:
///
/// - `pool` — the SQLite pool owning `users_tbl`, `sessions_tbl`,
///   `jobs_tbl`, and the document store. Provided by the caller; this
///   crate never opens or migrates it.
/// - `session_config` — session TTL/renewal policy used by the auth
///   extractor when validating bearer tokens.
/// - `job_config` — engine tunables; only `default_max_attempts` is
///   consumed today (job *execution* lives in the separately spawned
///   [`job_engine::JobEngine`], not here).
/// - `engines` — the [`EngineRegistry`] probed by `/health`. Built
///   from [`EngineRegistry::builtin`] by default; tests may inject a
///   custom registry via [`AppState::with_engines`].
#[derive(Clone)]
pub struct AppState {
    /// Shared SQLite pool (users, sessions, jobs, documents).
    pub pool: SqlitePool,
    /// Session validation policy for bearer tokens.
    pub session_config: SessionConfig,
    /// Job engine tunables (retry budget, backoff, staleness).
    pub job_config: JobEngineConfig,
    /// Engine registry behind `GET /health`.
    engines: Arc<EngineRegistry>,
}

impl AppState {
    /// Builds state around an already-open pool. Engine health uses
    /// the built-in registry (`pdf`, `security`, `convert`).
    pub fn new(
        pool: SqlitePool,
        session_config: SessionConfig,
        job_config: JobEngineConfig,
    ) -> Self {
        Self {
            pool,
            session_config,
            job_config,
            engines: Arc::new(EngineRegistry::builtin()),
        }
    }

    /// Convenience constructor deriving session and job settings from
    /// an [`AppConfig`]. The pool still comes from the caller.
    pub fn from_config(pool: SqlitePool, config: &AppConfig) -> Self {
        let db = config.to_db_config();
        Self::new(pool, db.session, config.to_job_config())
    }

    /// Replaces the engine registry used by `/health` — primarily for
    /// tests that want deterministic component results.
    pub fn with_engines(mut self, engines: EngineRegistry) -> Self {
        self.engines = Arc::new(engines);
        self
    }

    /// The registry behind `/health`.
    pub fn engines(&self) -> &EngineRegistry {
        &self.engines
    }
}

/// Builds the application router with all routes wired to `state`.
///
/// Returns a [`Router<()>`] — ready for `axum::serve`, `tower`
/// testing via `ServiceExt::oneshot`, or nesting under a larger app.
pub fn router(state: AppState) -> Router {
    Router::new()
        .route("/health", get(handlers::health))
        .route("/documents", post(handlers::create_document))
        .route("/jobs", get(handlers::list_jobs))
        .route("/jobs/{id}", get(handlers::get_job))
        .with_state(state)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::crates::application::Organization::auth_user::{NewUser, create_user};
    use crate::crates::application::Organization::init_db::{
        create_sessions_table, create_users_table,
    };
    use crate::crates::application::Organization::session::create_session;
    use crate::crates::job_engine::{create_jobs_table, list_jobs as store_list_jobs};
    use axum::body::Body;
    use axum::http::{Request, StatusCode, header};
    use serde_json::{Value, json};
    use sqlx::sqlite::SqlitePoolOptions;
    use tower::ServiceExt;

    fn test_session_config() -> SessionConfig {
        SessionConfig {
            ttl: std::time::Duration::from_secs(3600),
            absolute_ttl: std::time::Duration::ZERO,
            renew_on_use: true,
            touch_interval: std::time::Duration::ZERO,
            cleanup_interval: std::time::Duration::from_secs(60),
        }
    }

    /// In-memory pool (single connection) with users, sessions, and
    /// jobs tables — everything the scaffolded routes touch.
    async fn test_state() -> AppState {
        let pool = SqlitePoolOptions::new()
            .max_connections(1)
            .connect("sqlite::memory:")
            .await
            .unwrap();
        create_users_table(&pool).await.unwrap();
        create_sessions_table(&pool).await.unwrap();
        create_jobs_table(&pool).await.unwrap();
        AppState::new(pool, test_session_config(), JobEngineConfig::default())
    }

    /// Creates a user + session; returns the raw bearer token.
    async fn seed_session(state: &AppState) -> String {
        let user_id = create_user(
            &state.pool,
            &NewUser {
                first_name: "Api".into(),
                last_name: "Tester".into(),
                user_name: "api_tester".into(),
                email: "api@example.com".into(),
                password: "correct horse battery".into(),
            },
        )
        .await
        .unwrap();
        let (_session, token) = create_session(&state.pool, user_id, &state.session_config)
            .await
            .unwrap();
        token
    }

    /// Sends one request through the router; returns (status, parsed
    /// JSON body or `Value::Null` for empty/non-JSON bodies).
    async fn send(
        app: &Router,
        method: &str,
        uri: &str,
        token: Option<&str>,
        body: Option<Value>,
    ) -> (StatusCode, Value) {
        let mut builder = Request::builder().method(method).uri(uri);
        if let Some(t) = token {
            builder = builder.header(header::AUTHORIZATION, format!("Bearer {t}"));
        }
        let req = match body {
            Some(b) => builder
                .header(header::CONTENT_TYPE, "application/json")
                .body(Body::from(b.to_string()))
                .unwrap(),
            None => builder.body(Body::empty()).unwrap(),
        };
        let resp = app.clone().oneshot(req).await.unwrap();
        let status = resp.status();
        let bytes = axum::body::to_bytes(resp.into_body(), usize::MAX)
            .await
            .unwrap();
        let value = if bytes.is_empty() {
            Value::Null
        } else {
            serde_json::from_slice(&bytes).unwrap_or(Value::Null)
        };
        (status, value)
    }

    #[tokio::test]
    async fn health_returns_200_with_aggregated_components() {
        let app = router(test_state().await);
        let (status, body) = send(&app, "GET", "/health", None, None).await;
        assert_eq!(status, StatusCode::OK);
        assert!(body["status"].is_string());
        let components = body["components"].as_array().expect("components array");
        assert!(!components.is_empty());
        for c in components {
            assert!(c["name"].is_string());
            assert!(c["status"].is_string());
        }
    }

    #[tokio::test]
    async fn create_document_requires_auth() {
        let state = test_state().await;
        let app = router(state.clone());
        let body = json!({"document_id": "doc-1", "content": "hello"});

        // No Authorization header → 401.
        let (status, _) = send(&app, "POST", "/documents", None, Some(body.clone())).await;
        assert_eq!(status, StatusCode::UNAUTHORIZED);

        // Bogus token → 401.
        let (status, _) = send(&app, "POST", "/documents", Some("bogus"), Some(body)).await;
        assert_eq!(status, StatusCode::UNAUTHORIZED);

        // Nothing was enqueued.
        let jobs = store_list_jobs(&state.pool, None, 10).await.unwrap();
        assert!(jobs.is_empty());
    }

    #[tokio::test]
    async fn create_document_enqueues_ingest_job() {
        let state = test_state().await;
        let token = seed_session(&state).await;
        let app = router(state.clone());

        let body = json!({
            "document_id": "doc-42",
            "content": "hello world",
            "file_name": "note.txt",
            "mime_type": "text/plain",
        });
        let (status, resp) = send(&app, "POST", "/documents", Some(&token), Some(body)).await;
        assert_eq!(status, StatusCode::ACCEPTED);
        assert_eq!(resp["kind"], "document.ingest");
        assert_eq!(resp["status"], "pending");
        let job_id = resp["id"].as_i64().unwrap();

        // The row is really in jobs_tbl with the documented payload.
        let jobs = store_list_jobs(&state.pool, None, 10).await.unwrap();
        assert_eq!(jobs.len(), 1);
        assert_eq!(jobs[0].id, job_id);
        assert_eq!(jobs[0].payload["document_id"], "doc-42");
        assert_eq!(jobs[0].payload["content"], "hello world");
        assert_eq!(jobs[0].payload["file_name"], "note.txt");

        // And it shows up via the jobs endpoints.
        let (status, list) = send(&app, "GET", "/jobs?status=pending", Some(&token), None).await;
        assert_eq!(status, StatusCode::OK);
        assert_eq!(list.as_array().unwrap().len(), 1);

        let (status, one) = send(
            &app,
            "GET",
            &format!("/jobs/{job_id}"),
            Some(&token),
            None,
        )
        .await;
        assert_eq!(status, StatusCode::OK);
        assert_eq!(one["id"], job_id);

        let (status, _) = send(&app, "GET", "/jobs/999999", Some(&token), None).await;
        assert_eq!(status, StatusCode::NOT_FOUND);
    }

    #[tokio::test]
    async fn create_document_rejects_bad_payloads() {
        let state = test_state().await;
        let token = seed_session(&state).await;
        let app = router(state.clone());

        // Missing document_id.
        let (status, _) = send(
            &app,
            "POST",
            "/documents",
            Some(&token),
            Some(json!({"content": "x"})),
        )
        .await;
        assert_eq!(status, StatusCode::BAD_REQUEST);

        // Neither content_b64 nor content.
        let (status, _) = send(
            &app,
            "POST",
            "/documents",
            Some(&token),
            Some(json!({"document_id": "d", "file_name": "f"})),
        )
        .await;
        assert_eq!(status, StatusCode::BAD_REQUEST);

        // Both content fields set.
        let (status, _) = send(
            &app,
            "POST",
            "/documents",
            Some(&token),
            Some(json!({"document_id": "d", "content": "x", "content_b64": "eA=="})),
        )
        .await;
        assert_eq!(status, StatusCode::BAD_REQUEST);

        // Invalid base64.
        let (status, _) = send(
            &app,
            "POST",
            "/documents",
            Some(&token),
            Some(json!({"document_id": "d", "content_b64": "!!!"})),
        )
        .await;
        assert_eq!(status, StatusCode::BAD_REQUEST);

        let jobs = store_list_jobs(&state.pool, None, 10).await.unwrap();
        assert!(jobs.is_empty());
    }

    #[tokio::test]
    async fn jobs_endpoints_require_auth_and_reject_bad_status() {
        let state = test_state().await;
        let token = seed_session(&state).await;
        let app = router(state);

        // Unauthenticated list → 401.
        let (status, _) = send(&app, "GET", "/jobs", None, None).await;
        assert_eq!(status, StatusCode::UNAUTHORIZED);

        // Unknown status filter → 400.
        let (status, _) = send(&app, "GET", "/jobs?status=bogus", Some(&token), None).await;
        assert_eq!(status, StatusCode::BAD_REQUEST);

        // Empty list → 200 [].
        let (status, body) = send(&app, "GET", "/jobs", Some(&token), None).await;
        assert_eq!(status, StatusCode::OK);
        assert_eq!(body.as_array().unwrap().len(), 0);
    }
}
