//! Router-level tests: `tower::ServiceExt::oneshot` against an
//! in-memory SQLite pool and a tempdir storage root — no network, no
//! real database file.

use std::sync::Arc;
use std::time::Duration;

use axum::Router;
use axum::body::Body;
use axum::http::{Request, StatusCode};
use serde_json::Value;
use sqlx::SqlitePool;
use sqlx::sqlite::SqlitePoolOptions;
use tower::ServiceExt;

use crate::config::ApiKey;
use crate::crates::api::{ApiState, router};
use crate::crates::application::Organization::auth_user::{NewUser, create_user};
use crate::crates::application::Organization::init_db::{
    SessionConfig, create_sessions_table, create_users_table,
};
use crate::crates::application::Organization::session::create_session;
use crate::crates::application::services::{AppServices, StorageService};
use crate::crates::document_engine::store::schema::init_document_store;
use crate::crates::engine_registry::EngineRegistry;
use crate::crates::job_engine::create_jobs_table;
use crate::crates::storage::local::config::StorageConfig;

/// Pool with every table the API touches (users, sessions, jobs,
/// document store). Single connection: `sqlite::memory:` is
/// per-connection.
async fn test_pool() -> SqlitePool {
    let pool = SqlitePoolOptions::new()
        .max_connections(1)
        .connect("sqlite::memory:")
        .await
        .unwrap();
    create_users_table(&pool).await.unwrap();
    create_sessions_table(&pool).await.unwrap();
    create_jobs_table(&pool).await.unwrap();
    init_document_store(&pool).await.unwrap();
    pool
}

fn test_session_config() -> SessionConfig {
    SessionConfig {
        ttl: Duration::from_secs(3600),
        absolute_ttl: Duration::ZERO,
        renew_on_use: true,
        touch_interval: Duration::ZERO,
        cleanup_interval: Duration::from_secs(60),
    }
}

/// Builds router state over a tempdir storage root + in-memory DB,
/// plus a valid bearer token for a freshly created user. Callers can
/// further configure the state (API keys, disabled groups) before
/// wrapping it in [`router`].
async fn test_state() -> (ApiState, String, tempfile::TempDir) {
    let pool = test_pool().await;
    let storage_dir = tempfile::tempdir().unwrap();
    let storage_config = StorageConfig::new(storage_dir.path().to_path_buf());
    let storage = StorageService::from_config(&storage_config).unwrap();
    let services = AppServices::new(storage, pool.clone(), &storage_config);
    let state = ApiState::new(
        services,
        EngineRegistry::new(),
        test_session_config(),
        storage_config,
    );

    let user_id = create_user(
        &pool,
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
    let (_session, token) = create_session(&pool, user_id, &test_session_config())
        .await
        .unwrap();

    (state, token, storage_dir)
}

/// Default test app: unmodified state + session token.
async fn test_app() -> (Router, String, tempfile::TempDir) {
    let (state, token, dir) = test_state().await;
    (router(Arc::new(state)), token, dir)
}

async fn body_json(res: axum::response::Response) -> Value {
    let bytes = axum::body::to_bytes(res.into_body(), usize::MAX)
        .await
        .unwrap();
    serde_json::from_slice(&bytes).unwrap()
}

fn get(uri: &str) -> Request<Body> {
    Request::builder().uri(uri).body(Body::empty()).unwrap()
}

fn authed_get(uri: &str, token: &str) -> Request<Body> {
    Request::builder()
        .uri(uri)
        .header("authorization", format!("Bearer {token}"))
        .body(Body::empty())
        .unwrap()
}

fn authed_json(method: &str, uri: &str, token: &str, body: Value) -> Request<Body> {
    Request::builder()
        .method(method)
        .uri(uri)
        .header("authorization", format!("Bearer {token}"))
        .header("content-type", "application/json")
        .body(Body::from(body.to_string()))
        .unwrap()
}

/// Uploads `dot.png` through the API and returns the created document id.
async fn upload_doc(app: &Router, token: &str) -> String {
    let res = app
        .clone()
        .oneshot(
            Request::builder()
                .method("POST")
                .uri("/documents?filename=dot.png")
                .header("authorization", format!("Bearer {token}"))
                .header("content-type", "image/png")
                .body(Body::from(b"\x89PNG\r\n\x1a\n".to_vec()))
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(res.status(), StatusCode::CREATED);
    let body = body_json(res).await;
    body["document"]["id"].as_str().unwrap().to_string()
}

#[tokio::test]
async fn health_is_public_and_reports_status() {
    let (app, _token, _dir) = test_app().await;
    let res = app.oneshot(get("/health")).await.unwrap();
    assert_eq!(res.status(), StatusCode::OK);
    let body = body_json(res).await;
    assert_eq!(body["status"], "healthy");
    assert!(body["components"].is_array());
}

#[tokio::test]
async fn protected_routes_reject_missing_and_bad_tokens() {
    let (app, _token, _dir) = test_app().await;

    // No Authorization header at all.
    let res = app.clone().oneshot(get("/documents/doc-1")).await.unwrap();
    assert_eq!(res.status(), StatusCode::UNAUTHORIZED);
    let body = body_json(res).await;
    assert_eq!(body["error"]["code"], "unauthorized");
    assert!(body["error"]["request_id"].is_string());
    assert_eq!(body["error"]["retryable"], false);

    // Wrong scheme / garbage token.
    let res = app
        .clone()
        .oneshot(
            Request::builder()
                .uri("/jobs/1")
                .header("authorization", "Bearer not-a-real-token")
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(res.status(), StatusCode::UNAUTHORIZED);
}

#[tokio::test]
async fn authenticated_routes_work_end_to_end() {
    let (app, token, _dir) = test_app().await;

    // Unknown document → 404 in the error envelope.
    let res = app
        .clone()
        .oneshot(authed_get("/documents/doc-missing", &token))
        .await
        .unwrap();
    assert_eq!(res.status(), StatusCode::NOT_FOUND);
    let body = body_json(res).await;
    assert_eq!(body["error"]["code"], "not_found");

    // Upload a tiny PNG (magic bytes are all the sniffer needs).
    let png = b"\x89PNG\r\n\x1a\n".to_vec();
    let res = app
        .clone()
        .oneshot(
            Request::builder()
                .method("POST")
                .uri("/documents?filename=dot.png")
                .header("authorization", format!("Bearer {token}"))
                .header("content-type", "image/png")
                .body(Body::from(png.clone()))
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(res.status(), StatusCode::CREATED);
    let body = body_json(res).await;
    let doc_id = body["document"]["id"].as_str().unwrap().to_string();
    assert_eq!(body["artifact"]["file_name"], "dot.png");
    assert_eq!(body["size_bytes"], png.len() as u64);
    assert!(body["sha256"].as_str().unwrap().len() == 64);
    assert_eq!(body["detected_mime_type"], "image/png");
    let job_id = body["processing_job"]["id"]
        .as_i64()
        .expect("upload should enqueue a processing job");

    // GET /documents/{id} returns the same document + its artifact.
    let res = app
        .clone()
        .oneshot(authed_get(&format!("/documents/{doc_id}"), &token))
        .await
        .unwrap();
    assert_eq!(res.status(), StatusCode::OK);
    let body = body_json(res).await;
    assert_eq!(body["document"]["id"], doc_id);
    assert_eq!(body["artifacts"].as_array().unwrap().len(), 1);

    // The metadata-extract job enqueued by the upload is queryable.
    let res = app
        .clone()
        .oneshot(authed_get(&format!("/jobs/{job_id}"), &token))
        .await
        .unwrap();
    assert_eq!(res.status(), StatusCode::OK);
    let body = body_json(res).await;
    assert_eq!(body["kind"], "document.metadata_extract");
    assert_eq!(body["payload"]["document_id"], doc_id);
    assert!(body["status"].is_string());

    // Unknown job → 404; non-integer id → 400.
    let res = app
        .clone()
        .oneshot(authed_get("/jobs/999999", &token))
        .await
        .unwrap();
    assert_eq!(res.status(), StatusCode::NOT_FOUND);
    let res = app
        .clone()
        .oneshot(authed_get("/jobs/abc", &token))
        .await
        .unwrap();
    assert_eq!(res.status(), StatusCode::BAD_REQUEST);
}

#[tokio::test]
async fn engines_and_storage_endpoints() {
    let (app, token, _dir) = test_app().await;

    let res = app
        .clone()
        .oneshot(authed_get("/engines", &token))
        .await
        .unwrap();
    assert_eq!(res.status(), StatusCode::OK);
    let body = body_json(res).await;
    assert!(body["engines"].is_array());
    assert!(body["health"].is_array());

    let res = app
        .clone()
        .oneshot(authed_get("/storage", &token))
        .await
        .unwrap();
    assert_eq!(res.status(), StatusCode::OK);
    let body = body_json(res).await;
    assert_eq!(body["backend"], "local");
    assert_eq!(body["root_configured"], true);
}

#[tokio::test]
async fn list_search_patch_and_lifecycle() {
    let (app, token, _dir) = test_app().await;
    let doc_id = upload_doc(&app, &token).await;

    // List scoped to the caller finds the upload.
    let res = app
        .clone()
        .oneshot(authed_get("/documents", &token))
        .await
        .unwrap();
    assert_eq!(res.status(), StatusCode::OK);
    let body = body_json(res).await;
    let docs = body["documents"].as_array().unwrap();
    assert_eq!(docs.len(), 1);
    assert_eq!(docs[0]["id"], doc_id);

    // Name search hits; a nonsense query misses.
    let res = app
        .clone()
        .oneshot(authed_get("/documents?q=dot", &token))
        .await
        .unwrap();
    assert_eq!(
        body_json(res).await["documents"].as_array().unwrap().len(),
        1
    );
    let res = app
        .clone()
        .oneshot(authed_get("/documents?q=zzz-no-match", &token))
        .await
        .unwrap();
    assert!(
        body_json(res).await["documents"]
            .as_array()
            .unwrap()
            .is_empty()
    );

    // PATCH rename.
    let res = app
        .clone()
        .oneshot(authed_json(
            "PATCH",
            &format!("/documents/{doc_id}"),
            &token,
            serde_json::json!({ "name": "renamed.png" }),
        ))
        .await
        .unwrap();
    assert_eq!(res.status(), StatusCode::OK);
    assert_eq!(body_json(res).await["document"]["name"], "renamed.png");

    // PATCH with folder_id: null moves to root (no error even with no folders).
    let res = app
        .clone()
        .oneshot(authed_json(
            "PATCH",
            &format!("/documents/{doc_id}"),
            &token,
            serde_json::json!({ "folder_id": null }),
        ))
        .await
        .unwrap();
    assert_eq!(res.status(), StatusCode::OK);

    // Trash then restore.
    let res = app
        .clone()
        .oneshot(
            Request::builder()
                .method("DELETE")
                .uri(format!("/documents/{doc_id}"))
                .header("authorization", format!("Bearer {token}"))
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(res.status(), StatusCode::OK);

    // Trashed docs are hidden from the default list.
    let res = app
        .clone()
        .oneshot(authed_get("/documents", &token))
        .await
        .unwrap();
    assert!(
        body_json(res).await["documents"]
            .as_array()
            .unwrap()
            .is_empty()
    );

    let res = app
        .clone()
        .oneshot(
            Request::builder()
                .method("POST")
                .uri(format!("/documents/{doc_id}/restore"))
                .header("authorization", format!("Bearer {token}"))
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(res.status(), StatusCode::OK);
    let res = app
        .clone()
        .oneshot(authed_get("/documents", &token))
        .await
        .unwrap();
    assert_eq!(
        body_json(res).await["documents"].as_array().unwrap().len(),
        1
    );
}

#[tokio::test]
async fn folders_tags_versions_history_and_download() {
    let (app, token, _dir) = test_app().await;
    let doc_id = upload_doc(&app, &token).await;

    // Create a folder, then move the document into it.
    let res = app
        .clone()
        .oneshot(authed_json(
            "POST",
            "/folders",
            &token,
            serde_json::json!({ "name": "inbox" }),
        ))
        .await
        .unwrap();
    assert_eq!(res.status(), StatusCode::CREATED);
    let folder_id = body_json(res).await["folder"]["id"]
        .as_str()
        .unwrap()
        .to_string();

    let res = app
        .clone()
        .oneshot(authed_get("/folders", &token))
        .await
        .unwrap();
    assert_eq!(body_json(res).await["folders"].as_array().unwrap().len(), 1);

    let res = app
        .clone()
        .oneshot(authed_json(
            "PATCH",
            &format!("/documents/{doc_id}"),
            &token,
            serde_json::json!({ "folder_id": folder_id }),
        ))
        .await
        .unwrap();
    assert_eq!(res.status(), StatusCode::OK);
    assert_eq!(body_json(res).await["document"]["folder_id"], folder_id);

    // Folder-scoped listing.
    let res = app
        .clone()
        .oneshot(authed_get(
            &format!("/documents?folder_id={folder_id}"),
            &token,
        ))
        .await
        .unwrap();
    assert_eq!(
        body_json(res).await["documents"].as_array().unwrap().len(),
        1
    );

    // Tags: assign, list, remove.
    let res = app
        .clone()
        .oneshot(authed_json(
            "PUT",
            &format!("/documents/{doc_id}/tags"),
            &token,
            serde_json::json!({ "name": "scanned" }),
        ))
        .await
        .unwrap();
    assert_eq!(res.status(), StatusCode::CREATED);
    let res = app
        .clone()
        .oneshot(authed_get(&format!("/documents/{doc_id}/tags"), &token))
        .await
        .unwrap();
    assert_eq!(body_json(res).await["tags"].as_array().unwrap().len(), 1);
    let res = app
        .clone()
        .oneshot(
            Request::builder()
                .method("DELETE")
                .uri(format!("/documents/{doc_id}/tags/scanned"))
                .header("authorization", format!("Bearer {token}"))
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(body_json(res).await["removed"], true);

    // Versions + history exist for the uploaded document.
    let res = app
        .clone()
        .oneshot(authed_get(&format!("/documents/{doc_id}/versions"), &token))
        .await
        .unwrap();
    assert!(body_json(res).await["versions"].is_array());
    let res = app
        .clone()
        .oneshot(authed_get(&format!("/documents/{doc_id}/history"), &token))
        .await
        .unwrap();
    let events = body_json(res).await["events"].as_array().unwrap().clone();
    assert!(!events.is_empty());

    // Artifact download round-trips the uploaded bytes.
    let res = app
        .clone()
        .oneshot(authed_get(&format!("/documents/{doc_id}"), &token))
        .await
        .unwrap();
    let artifact_id = body_json(res).await["artifacts"][0]["id"]
        .as_str()
        .unwrap()
        .to_string();
    let res = app
        .clone()
        .oneshot(authed_get(
            &format!("/documents/{doc_id}/artifacts/{artifact_id}/download"),
            &token,
        ))
        .await
        .unwrap();
    assert_eq!(res.status(), StatusCode::OK);
    let bytes = axum::body::to_bytes(res.into_body(), usize::MAX)
        .await
        .unwrap();
    assert_eq!(&bytes[..], b"\x89PNG\r\n\x1a\n");
}

#[tokio::test]
async fn jobs_list_process_cancel_retry() {
    let (app, token, _dir) = test_app().await;
    let doc_id = upload_doc(&app, &token).await;

    // The upload enqueued a metadata_extract job — find it via /jobs.
    let res = app
        .clone()
        .oneshot(authed_get("/jobs", &token))
        .await
        .unwrap();
    assert_eq!(res.status(), StatusCode::OK);
    let jobs = body_json(res).await["jobs"].as_array().unwrap().clone();
    assert_eq!(jobs.len(), 1);
    let job_id = jobs[0]["id"].as_i64().unwrap();

    // Status filter works; a bogus status is a 400.
    let res = app
        .clone()
        .oneshot(authed_get("/jobs?status=pending", &token))
        .await
        .unwrap();
    assert_eq!(body_json(res).await["jobs"].as_array().unwrap().len(), 1);
    let res = app
        .clone()
        .oneshot(authed_get("/jobs?status=bogus", &token))
        .await
        .unwrap();
    assert_eq!(res.status(), StatusCode::BAD_REQUEST);

    // Process endpoint enqueues an index job for the document.
    let res = app
        .clone()
        .oneshot(authed_json(
            "POST",
            &format!("/documents/{doc_id}/process"),
            &token,
            serde_json::json!({ "op": "index" }),
        ))
        .await
        .unwrap();
    assert_eq!(res.status(), StatusCode::ACCEPTED);
    let index_job_id = body_json(res).await["job"]["id"].as_i64().unwrap();
    assert_ne!(index_job_id, job_id);

    // A pdf-engine op enqueues a document.pdf_op job carrying the
    // op name and params through to the worker.
    let res = app
        .clone()
        .oneshot(authed_json(
            "POST",
            &format!("/documents/{doc_id}/process"),
            &token,
            serde_json::json!({
                "op": "page_count",
                "params": { "pages": "1" }
            }),
        ))
        .await
        .unwrap();
    assert_eq!(res.status(), StatusCode::ACCEPTED);
    let pdf_job = body_json(res).await["job"].clone();
    assert_eq!(pdf_job["kind"], "document.pdf_op");
    assert_eq!(pdf_job["payload"]["document_id"], doc_id);
    assert_eq!(pdf_job["payload"]["op"], "page_count");
    assert_eq!(pdf_job["payload"]["params"]["pages"], "1");

    // Unknown op → 400; unknown document → 404.
    let res = app
        .clone()
        .oneshot(authed_json(
            "POST",
            &format!("/documents/{doc_id}/process"),
            &token,
            serde_json::json!({ "op": "explode" }),
        ))
        .await
        .unwrap();
    assert_eq!(res.status(), StatusCode::BAD_REQUEST);
    let res = app
        .clone()
        .oneshot(authed_json(
            "POST",
            "/documents/doc-missing/process",
            &token,
            serde_json::json!({ "op": "index" }),
        ))
        .await
        .unwrap();
    assert_eq!(res.status(), StatusCode::NOT_FOUND);

    // Cancel the pending job → 200; cancelling again → 409.
    let res = app
        .clone()
        .oneshot(
            Request::builder()
                .method("POST")
                .uri(format!("/jobs/{job_id}/cancel"))
                .header("authorization", format!("Bearer {token}"))
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(res.status(), StatusCode::OK);
    assert_eq!(body_json(res).await["status"], "cancelled");
    let res = app
        .clone()
        .oneshot(
            Request::builder()
                .method("POST")
                .uri(format!("/jobs/{job_id}/cancel"))
                .header("authorization", format!("Bearer {token}"))
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(res.status(), StatusCode::CONFLICT);

    // Retry only works on failed jobs → 409 for a cancelled one; a
    // non-integer id → 400.
    let res = app
        .clone()
        .oneshot(
            Request::builder()
                .method("POST")
                .uri(format!("/jobs/{job_id}/retry"))
                .header("authorization", format!("Bearer {token}"))
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(res.status(), StatusCode::CONFLICT);
    let res = app
        .clone()
        .oneshot(
            Request::builder()
                .method("POST")
                .uri("/jobs/abc/cancel")
                .header("authorization", format!("Bearer {token}"))
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(res.status(), StatusCode::BAD_REQUEST);
}

#[tokio::test]
async fn openapi_json_is_public_and_describes_routes() {
    let (app, token, _dir) = test_app().await;

    // Public like /health — no Authorization header needed.
    let res = app.clone().oneshot(get("/openapi.json")).await.unwrap();
    assert_eq!(res.status(), StatusCode::OK);
    let body = body_json(res).await;
    assert_eq!(body["openapi"], "3.0.3");
    assert_eq!(body["info"]["title"], "KalaDoc API");

    // The spec covers every route family, including the newest ones.
    let paths = body["paths"].as_object().unwrap();
    for path in [
        "/health",
        "/documents",
        "/documents/{id}/process",
        "/pipelines/run",
        "/jobs",
        "/jobs/batch",
        "/jobs/batch/{id}",
        "/jobs/batch/{id}/cancel",
    ] {
        assert!(paths.contains_key(path), "missing path {path}");
    }
    // Authenticated routes declare the bearer scheme.
    assert!(
        paths["/pipelines/run"]["post"]["security"]
            .as_array()
            .is_some()
    );

    // Still reachable with a token (middleware doesn't break it).
    let res = app
        .oneshot(authed_get("/openapi.json", &token))
        .await
        .unwrap();
    assert_eq!(res.status(), StatusCode::OK);
}

#[tokio::test]
async fn pipelines_run_enqueues_pipeline_run_job() {
    let (app, token, _dir) = test_app().await;
    let doc_id = upload_doc(&app, &token).await;

    // Unknown document → 404.
    let res = app
        .clone()
        .oneshot(authed_json(
            "POST",
            "/pipelines/run",
            &token,
            serde_json::json!({
                "document_id": "doc-missing",
                "pipeline": { "name": "pdf.intake" }
            }),
        ))
        .await
        .unwrap();
    assert_eq!(res.status(), StatusCode::NOT_FOUND);

    // Unknown built-in name → 400 (resolved up front, not queued).
    let res = app
        .clone()
        .oneshot(authed_json(
            "POST",
            "/pipelines/run",
            &token,
            serde_json::json!({
                "document_id": doc_id,
                "pipeline": { "name": "no.such.pipeline" }
            }),
        ))
        .await
        .unwrap();
    assert_eq!(res.status(), StatusCode::BAD_REQUEST);

    // Neither a name nor a steps-bearing definition → 400.
    let res = app
        .clone()
        .oneshot(authed_json(
            "POST",
            "/pipelines/run",
            &token,
            serde_json::json!({ "document_id": doc_id, "pipeline": {} }),
        ))
        .await
        .unwrap();
    assert_eq!(res.status(), StatusCode::BAD_REQUEST);

    // A `steps` key selects definition mode; malformed → 400.
    let res = app
        .clone()
        .oneshot(authed_json(
            "POST",
            "/pipelines/run",
            &token,
            serde_json::json!({
                "document_id": doc_id,
                "pipeline": { "name": "x", "steps": "not-a-list" }
            }),
        ))
        .await
        .unwrap();
    assert_eq!(res.status(), StatusCode::BAD_REQUEST);

    // Built-in name resolves to its full definition in the payload.
    let res = app
        .clone()
        .oneshot(authed_json(
            "POST",
            "/pipelines/run",
            &token,
            serde_json::json!({
                "document_id": doc_id,
                "pipeline": { "name": "pdf.intake" }
            }),
        ))
        .await
        .unwrap();
    assert_eq!(res.status(), StatusCode::ACCEPTED);
    let job = body_json(res).await["job"].clone();
    assert_eq!(job["kind"], "pipeline.run");
    assert_eq!(job["payload"]["document_id"], doc_id);
    assert_eq!(job["payload"]["pipeline"]["name"], "pdf.intake");
    assert!(job["payload"]["pipeline"]["steps"].is_array());

    // A full PipelineDefinition JSON is stored verbatim.
    let res = app
        .clone()
        .oneshot(authed_json(
            "POST",
            "/pipelines/run",
            &token,
            serde_json::json!({
                "document_id": doc_id,
                "pipeline": {
                    "name": "custom.count",
                    "steps": [{ "name": "count", "engine": "pdf", "operation": "page_count" }]
                }
            }),
        ))
        .await
        .unwrap();
    assert_eq!(res.status(), StatusCode::ACCEPTED);
    let job = body_json(res).await["job"].clone();
    assert_eq!(job["kind"], "pipeline.run");
    assert_eq!(job["payload"]["pipeline"]["name"], "custom.count");
    assert_eq!(
        job["payload"]["pipeline"]["steps"][0]["operation"],
        "page_count"
    );
}

#[tokio::test]
async fn job_batches_create_progress_and_cancel() {
    let (app, token, _dir) = test_app().await;
    let doc_id = upload_doc(&app, &token).await;

    // Empty batch → 400.
    let res = app
        .clone()
        .oneshot(authed_json(
            "POST",
            "/jobs/batch",
            &token,
            serde_json::json!([]),
        ))
        .await
        .unwrap();
    assert_eq!(res.status(), StatusCode::BAD_REQUEST);

    // Non-object payload → 400.
    let res = app
        .clone()
        .oneshot(authed_json(
            "POST",
            "/jobs/batch",
            &token,
            serde_json::json!([{ "kind": "system.ping", "payload": "nope" }]),
        ))
        .await
        .unwrap();
    assert_eq!(res.status(), StatusCode::BAD_REQUEST);

    // Two jobs share one batch_id (the first job's id).
    let res = app
        .clone()
        .oneshot(authed_json(
            "POST",
            "/jobs/batch",
            &token,
            serde_json::json!([
                { "kind": "system.ping" },
                { "kind": "document.index", "payload": { "document_id": doc_id }, "priority": 5 }
            ]),
        ))
        .await
        .unwrap();
    assert_eq!(res.status(), StatusCode::CREATED);
    let body = body_json(res).await;
    let batch_id = body["batch_id"].as_i64().unwrap();
    let jobs = body["jobs"].as_array().unwrap();
    assert_eq!(jobs.len(), 2);
    for job in jobs {
        assert_eq!(job["batch_id"], batch_id);
        assert!(job["progress"].is_null() || job["progress"].is_object());
    }
    assert_eq!(jobs[1]["priority"], 5);
    assert_eq!(jobs[1]["payload"]["document_id"], doc_id);

    // Batch progress endpoint reports counts + member jobs.
    let res = app
        .clone()
        .oneshot(authed_get(&format!("/jobs/batch/{batch_id}"), &token))
        .await
        .unwrap();
    assert_eq!(res.status(), StatusCode::OK);
    let body = body_json(res).await;
    assert_eq!(body["batch_id"], batch_id);
    assert_eq!(body["progress"]["items_total"], 2);
    assert_eq!(body["jobs"].as_array().unwrap().len(), 2);

    // Cancel the batch → both pending jobs cancelled.
    let res = app
        .clone()
        .oneshot(
            Request::builder()
                .method("POST")
                .uri(format!("/jobs/batch/{batch_id}/cancel"))
                .header("authorization", format!("Bearer {token}"))
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(res.status(), StatusCode::OK);
    assert_eq!(body_json(res).await["cancelled"], 2);

    // Progress reflects the cancellations; unknown/non-integer → 404/400.
    let res = app
        .clone()
        .oneshot(authed_get(&format!("/jobs/batch/{batch_id}"), &token))
        .await
        .unwrap();
    let progress = body_json(res).await["progress"].clone();
    assert_eq!(progress["items_cancelled"], 2);
    assert_eq!(progress["complete"], true);

    let res = app
        .clone()
        .oneshot(authed_get("/jobs/batch/999999", &token))
        .await
        .unwrap();
    assert_eq!(res.status(), StatusCode::NOT_FOUND);
    let res = app
        .clone()
        .oneshot(authed_get("/jobs/batch/abc", &token))
        .await
        .unwrap();
    assert_eq!(res.status(), StatusCode::BAD_REQUEST);
}

#[tokio::test]
async fn api_keys_authenticate_alongside_sessions() {
    let (state, session_token, _dir) = test_state().await;
    let app = router(Arc::new(state.with_api_keys(vec![ApiKey {
        name: "ci".into(),
        key: "key-123".into(),
    }])));

    // Session tokens keep working.
    let res = app
        .clone()
        .oneshot(authed_get("/engines", &session_token))
        .await
        .unwrap();
    assert_eq!(res.status(), StatusCode::OK);

    // A configured key authenticates as `apikey:<name>`.
    let res = app
        .clone()
        .oneshot(authed_get("/engines", "key-123"))
        .await
        .unwrap();
    assert_eq!(res.status(), StatusCode::OK);

    // Upload via the key: the document is owned/scoped to the key actor.
    let res = app
        .clone()
        .oneshot(
            Request::builder()
                .method("POST")
                .uri("/documents?filename=keyed.png")
                .header("authorization", "Bearer key-123")
                .header("content-type", "image/png")
                .body(Body::from(b"\x89PNG\r\n\x1a\n".to_vec()))
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(res.status(), StatusCode::CREATED);
    let body = body_json(res).await;
    assert_eq!(body["document"]["owner_id"], "apikey:ci");

    // The key's owner scope lists its own uploads.
    let res = app
        .clone()
        .oneshot(authed_get("/documents", "key-123"))
        .await
        .unwrap();
    assert_eq!(
        body_json(res).await["documents"].as_array().unwrap().len(),
        1
    );

    // Wrong key and unknown session token both → 401.
    let res = app
        .clone()
        .oneshot(authed_get("/engines", "key-124"))
        .await
        .unwrap();
    assert_eq!(res.status(), StatusCode::UNAUTHORIZED);
    let res = app
        .clone()
        .oneshot(authed_get("/engines", "not-a-real-token"))
        .await
        .unwrap();
    assert_eq!(res.status(), StatusCode::UNAUTHORIZED);
}

#[tokio::test]
async fn disabled_groups_return_404_before_auth() {
    let (state, token, _dir) = test_state().await;
    let app =
        router(Arc::new(state.with_disabled_groups([
            "jobs".to_string(),
            "pipelines".to_string(),
        ])));

    // Disabled groups 404 even with a valid token.
    for uri in [
        "/jobs",
        "/jobs/1",
        "/jobs/batch",
        "/pipelines",
        "/pipelines/pdf.intake",
        "/pipelines/run",
    ] {
        let res = app.clone().oneshot(authed_get(uri, &token)).await.unwrap();
        assert_eq!(res.status(), StatusCode::NOT_FOUND, "{uri} should 404");
        let body = body_json(res).await;
        assert_eq!(body["error"]["code"], "not_found");
        assert!(
            body["error"]["message"]
                .as_str()
                .unwrap()
                .contains("disabled"),
            "{uri} error should mention disabled: {}",
            body["error"]["message"]
        );
    }

    // The gate runs before auth: no token still 404s (not 401).
    let res = app.clone().oneshot(get("/jobs")).await.unwrap();
    assert_eq!(res.status(), StatusCode::NOT_FOUND);

    // Other groups and public routes are unaffected.
    let res = app
        .clone()
        .oneshot(authed_get("/documents", &token))
        .await
        .unwrap();
    assert_eq!(res.status(), StatusCode::OK);
    let res = app.clone().oneshot(get("/health")).await.unwrap();
    assert_eq!(res.status(), StatusCode::OK);
}

#[tokio::test]
async fn pipelines_list_and_get_builtin_definitions() {
    let (app, token, _dir) = test_app().await;

    // GET /pipelines lists the built-ins as full definitions.
    let res = app
        .clone()
        .oneshot(authed_get("/pipelines", &token))
        .await
        .unwrap();
    assert_eq!(res.status(), StatusCode::OK);
    let pipelines = body_json(res).await["pipelines"]
        .as_array()
        .unwrap()
        .clone();
    let names: Vec<&str> = pipelines
        .iter()
        .map(|p| p["name"].as_str().unwrap())
        .collect();
    assert!(names.contains(&"pdf.intake"));
    assert!(names.contains(&"pdf.extract-text"));
    for def in &pipelines {
        assert!(def["steps"].as_array().unwrap().len() > 0);
    }

    // GET /pipelines/{name} returns one definition.
    let res = app
        .clone()
        .oneshot(authed_get("/pipelines/pdf.intake", &token))
        .await
        .unwrap();
    assert_eq!(res.status(), StatusCode::OK);
    let body = body_json(res).await;
    assert_eq!(body["pipeline"]["name"], "pdf.intake");
    assert_eq!(body["pipeline"]["steps"].as_array().unwrap().len(), 4);

    // Unknown name → 404; unauthenticated → 401.
    let res = app
        .clone()
        .oneshot(authed_get("/pipelines/nope", &token))
        .await
        .unwrap();
    assert_eq!(res.status(), StatusCode::NOT_FOUND);
    let res = app
        .clone()
        .oneshot(get("/pipelines/pdf.intake"))
        .await
        .unwrap();
    assert_eq!(res.status(), StatusCode::UNAUTHORIZED);
}

// ---------------------------------------------------------------------------
// Authorization — owner / permission / stranger matrix
// ---------------------------------------------------------------------------

/// Creates a second user + session token on the same pool. Returns
/// `(user_id, bearer_token)`.
async fn second_user(pool: &SqlitePool, name: &str) -> (i64, String) {
    let user_id = create_user(
        pool,
        &NewUser {
            first_name: "Second".into(),
            last_name: name.into(),
            user_name: format!("user_{name}"),
            email: format!("{name}@example.com"),
            password: "correct horse battery".into(),
        },
    )
    .await
    .unwrap();
    let (_session, token) = create_session(pool, user_id, &test_session_config())
        .await
        .unwrap();
    (user_id, token)
}

#[tokio::test]
async fn document_access_matrix_owner_permitted_stranger() {
    let (state, token_a, _dir) = test_state().await;
    let pool = state.services.pool().clone();
    let app = router(Arc::new(state));
    let doc_id = upload_doc(&app, &token_a).await;

    let (user_b, token_b) = second_user(&pool, "bob").await;
    let bob = user_b.to_string();

    // Stranger: every document route is 403 (or absent from lists).
    for uri in [
        format!("/documents/{doc_id}"),
        format!("/documents/{doc_id}/versions"),
        format!("/documents/{doc_id}/tags"),
        format!("/documents/{doc_id}/history"),
        format!("/documents/{doc_id}/metadata"),
        format!("/documents/{doc_id}/settings"),
        format!("/documents/{doc_id}/permissions"),
        format!("/documents/{doc_id}/relationships"),
        format!("/documents/{doc_id}/related"),
    ] {
        let res = app
            .clone()
            .oneshot(authed_get(&uri, &token_b))
            .await
            .unwrap();
        assert_eq!(res.status(), StatusCode::FORBIDDEN, "GET {uri}");
        let body = body_json(res).await;
        assert_eq!(body["error"]["code"], "forbidden");
    }
    // Mutations are 403 too.
    let res = app
        .clone()
        .oneshot(authed_json(
            "PATCH",
            &format!("/documents/{doc_id}"),
            &token_b,
            serde_json::json!({ "name": "hijack.png" }),
        ))
        .await
        .unwrap();
    assert_eq!(res.status(), StatusCode::FORBIDDEN);
    let res = app
        .clone()
        .oneshot(
            Request::builder()
                .method("DELETE")
                .uri(format!("/documents/{doc_id}"))
                .header("authorization", format!("Bearer {token_b}"))
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(res.status(), StatusCode::FORBIDDEN);
    let res = app
        .clone()
        .oneshot(authed_json(
            "POST",
            &format!("/documents/{doc_id}/process"),
            &token_b,
            serde_json::json!({ "op": "index" }),
        ))
        .await
        .unwrap();
    assert_eq!(res.status(), StatusCode::FORBIDDEN);

    // owner_id override is ignored: Bob sees only his own (zero) docs.
    let res = app
        .clone()
        .oneshot(authed_get(&format!("/documents?owner_id={bob}"), &token_a))
        .await
        .unwrap();
    // Alice asking for Bob's scope still gets Alice's docs only.
    assert_eq!(
        body_json(res).await["documents"].as_array().unwrap().len(),
        1
    );
    let res = app
        .clone()
        .oneshot(authed_get(
            &format!("/documents?owner_id={doc_id}"),
            &token_b,
        ))
        .await
        .unwrap();
    assert!(
        body_json(res).await["documents"]
            .as_array()
            .unwrap()
            .is_empty()
    );
    // Search never leaks unreadable documents.
    let res = app
        .clone()
        .oneshot(authed_get("/documents?q=dot", &token_b))
        .await
        .unwrap();
    assert!(
        body_json(res).await["documents"]
            .as_array()
            .unwrap()
            .is_empty()
    );

    // Alice grants Bob `read` — but only `manage` holders (i.e. Alice)
    // may grant, so exercise the happy path as Alice.
    let res = app
        .clone()
        .oneshot(authed_json(
            "POST",
            &format!("/documents/{doc_id}/permissions"),
            &token_a,
            serde_json::json!({ "subject_id": bob, "access_level": "read" }),
        ))
        .await
        .unwrap();
    assert_eq!(res.status(), StatusCode::CREATED);

    // Bob can now read but not write.
    let res = app
        .clone()
        .oneshot(authed_get(&format!("/documents/{doc_id}"), &token_b))
        .await
        .unwrap();
    assert_eq!(res.status(), StatusCode::OK);
    let res = app
        .clone()
        .oneshot(authed_json(
            "PATCH",
            &format!("/documents/{doc_id}"),
            &token_b,
            serde_json::json!({ "name": "hijack.png" }),
        ))
        .await
        .unwrap();
    assert_eq!(res.status(), StatusCode::FORBIDDEN);
    // Shared doc shows up in Bob's listing and search.
    let res = app
        .clone()
        .oneshot(authed_get("/documents", &token_b))
        .await
        .unwrap();
    assert_eq!(
        body_json(res).await["documents"].as_array().unwrap().len(),
        1
    );
    let res = app
        .clone()
        .oneshot(authed_get("/documents?q=dot", &token_b))
        .await
        .unwrap();
    assert_eq!(
        body_json(res).await["documents"].as_array().unwrap().len(),
        1
    );

    // Upgrade to write: PATCH succeeds, grant management still 403.
    let res = app
        .clone()
        .oneshot(authed_json(
            "POST",
            &format!("/documents/{doc_id}/permissions"),
            &token_a,
            serde_json::json!({ "subject_id": bob, "access_level": "write" }),
        ))
        .await
        .unwrap();
    assert_eq!(res.status(), StatusCode::CREATED);
    let res = app
        .clone()
        .oneshot(authed_json(
            "PATCH",
            &format!("/documents/{doc_id}"),
            &token_b,
            serde_json::json!({ "name": "bob-edit.png" }),
        ))
        .await
        .unwrap();
    assert_eq!(res.status(), StatusCode::OK);
    let res = app
        .clone()
        .oneshot(authed_json(
            "POST",
            &format!("/documents/{doc_id}/permissions"),
            &token_b,
            serde_json::json!({ "subject_id": "999", "access_level": "read" }),
        ))
        .await
        .unwrap();
    assert_eq!(res.status(), StatusCode::FORBIDDEN);
    // Purge needs manage — write is not enough.
    let res = app
        .clone()
        .oneshot(
            Request::builder()
                .method("POST")
                .uri(format!("/documents/{doc_id}/purge"))
                .header("authorization", format!("Bearer {token_b}"))
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(res.status(), StatusCode::FORBIDDEN);

    // Revoke → back to 403.
    let res = app
        .clone()
        .oneshot(
            Request::builder()
                .method("DELETE")
                .uri(format!("/documents/{doc_id}/permissions/{bob}"))
                .header("authorization", format!("Bearer {token_a}"))
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(res.status(), StatusCode::OK);
    assert_eq!(body_json(res).await["revoked"], true);
    let res = app
        .clone()
        .oneshot(authed_get(&format!("/documents/{doc_id}"), &token_b))
        .await
        .unwrap();
    assert_eq!(res.status(), StatusCode::FORBIDDEN);
}

#[tokio::test]
async fn permissions_routes_validation_and_subject_listing() {
    let (app, token, _dir) = test_app().await;
    let doc_id = upload_doc(&app, &token).await;

    // Unknown document → 404.
    let res = app
        .clone()
        .oneshot(authed_get("/documents/doc-missing/permissions", &token))
        .await
        .unwrap();
    assert_eq!(res.status(), StatusCode::NOT_FOUND);

    // Bad access_level → 400 (serde rejection); empty subject → 400.
    let res = app
        .clone()
        .oneshot(authed_json(
            "POST",
            &format!("/documents/{doc_id}/permissions"),
            &token,
            serde_json::json!({ "subject_id": "x", "access_level": "bogus" }),
        ))
        .await
        .unwrap();
    // Serde-level rejections surface as axum's 422.
    assert_eq!(res.status(), StatusCode::UNPROCESSABLE_ENTITY);
    let res = app
        .clone()
        .oneshot(authed_json(
            "POST",
            &format!("/documents/{doc_id}/permissions"),
            &token,
            serde_json::json!({ "subject_id": "  ", "access_level": "read" }),
        ))
        .await
        .unwrap();
    assert_eq!(res.status(), StatusCode::BAD_REQUEST);

    // Grant + list + subject-scoped list.
    app.clone()
        .oneshot(authed_json(
            "POST",
            &format!("/documents/{doc_id}/permissions"),
            &token,
            serde_json::json!({ "subject_id": "user-9", "access_level": "read" }),
        ))
        .await
        .unwrap();
    let res = app
        .clone()
        .oneshot(authed_get(
            &format!("/documents/{doc_id}/permissions"),
            &token,
        ))
        .await
        .unwrap();
    let perms = body_json(res).await["permissions"]
        .as_array()
        .unwrap()
        .clone();
    assert_eq!(perms.len(), 1);
    assert_eq!(perms[0]["subject_id"], "user-9");
    assert_eq!(perms[0]["access_level"], "read");

    // Self-service subject listing works for the caller's own actor id
    // (the session user's numeric id) — grant the caller first.
    let res = app
        .clone()
        .oneshot(authed_get(&format!("/documents/{doc_id}"), &token))
        .await
        .unwrap();
    let _ = res;
    // Listing another subject without manage → the owner has manage
    // implicitly, so this succeeds for the owner.
    let res = app
        .clone()
        .oneshot(authed_get(
            &format!("/documents/{doc_id}/permissions/subject/user-9"),
            &token,
        ))
        .await
        .unwrap();
    assert_eq!(res.status(), StatusCode::OK);
    assert_eq!(
        body_json(res).await["permissions"]
            .as_array()
            .unwrap()
            .len(),
        1
    );
}

#[tokio::test]
async fn document_lifecycle_extensions_copy_archive_purge_version() {
    let (app, token, _dir) = test_app().await;
    let doc_id = upload_doc(&app, &token).await;

    // Create a second version explicitly.
    let res = app
        .clone()
        .oneshot(authed_json(
            "POST",
            &format!("/documents/{doc_id}/versions"),
            &token,
            serde_json::json!({ "note": "checkpoint" }),
        ))
        .await
        .unwrap();
    assert_eq!(res.status(), StatusCode::CREATED);
    let version = body_json(res).await["version"].clone();
    assert!(version["version_number"].as_u64().unwrap() >= 2);
    assert_eq!(version["note"], "checkpoint");

    // Copy → new document owned by the caller.
    let res = app
        .clone()
        .oneshot(authed_json(
            "POST",
            &format!("/documents/{doc_id}/copy"),
            &token,
            serde_json::json!({ "name": "dot-copy.png" }),
        ))
        .await
        .unwrap();
    assert_eq!(res.status(), StatusCode::CREATED);
    let copy_id = body_json(res).await["document"]["id"]
        .as_str()
        .unwrap()
        .to_string();
    assert_ne!(copy_id, doc_id);

    // Archive → unarchive roundtrip.
    let res = app
        .clone()
        .oneshot(
            Request::builder()
                .method("POST")
                .uri(format!("/documents/{doc_id}/archive"))
                .header("authorization", format!("Bearer {token}"))
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(res.status(), StatusCode::OK);
    // Double archive → 400 (store validation).
    let res = app
        .clone()
        .oneshot(
            Request::builder()
                .method("POST")
                .uri(format!("/documents/{doc_id}/archive"))
                .header("authorization", format!("Bearer {token}"))
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(res.status(), StatusCode::BAD_REQUEST);
    let res = app
        .clone()
        .oneshot(
            Request::builder()
                .method("POST")
                .uri(format!("/documents/{doc_id}/unarchive"))
                .header("authorization", format!("Bearer {token}"))
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(res.status(), StatusCode::OK);

    // Purge requires a trashed document → 400 while active.
    let res = app
        .clone()
        .oneshot(
            Request::builder()
                .method("POST")
                .uri(format!("/documents/{doc_id}/purge"))
                .header("authorization", format!("Bearer {token}"))
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(res.status(), StatusCode::BAD_REQUEST);

    // Trash then purge → gone for good.
    app.clone()
        .oneshot(
            Request::builder()
                .method("DELETE")
                .uri(format!("/documents/{doc_id}"))
                .header("authorization", format!("Bearer {token}"))
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    let res = app
        .clone()
        .oneshot(
            Request::builder()
                .method("POST")
                .uri(format!("/documents/{doc_id}/purge"))
                .header("authorization", format!("Bearer {token}"))
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(res.status(), StatusCode::OK);
    assert_eq!(body_json(res).await["purged"], true);
    let res = app
        .clone()
        .oneshot(authed_get(&format!("/documents/{doc_id}"), &token))
        .await
        .unwrap();
    assert_eq!(res.status(), StatusCode::NOT_FOUND);
}

#[tokio::test]
async fn relationships_endpoints() {
    let (app, token, _dir) = test_app().await;
    let a = upload_doc(&app, &token).await;
    let b = upload_doc(&app, &token).await;

    // Link a → b.
    let res = app
        .clone()
        .oneshot(authed_json(
            "POST",
            &format!("/documents/{a}/relationships"),
            &token,
            serde_json::json!({
                "target_document_id": b,
                "relationship_type": "attachment"
            }),
        ))
        .await
        .unwrap();
    assert_eq!(res.status(), StatusCode::CREATED);
    let rel_id = body_json(res).await["relationship"]["id"]
        .as_str()
        .unwrap()
        .to_string();

    // Both directions are visible; related dedups.
    for id in [&a, &b] {
        let res = app
            .clone()
            .oneshot(authed_get(
                &format!("/documents/{id}/relationships"),
                &token,
            ))
            .await
            .unwrap();
        let body = body_json(res).await;
        assert_eq!(
            body["outgoing"].as_array().unwrap().len() + body["incoming"].as_array().unwrap().len(),
            1
        );
        let res = app
            .clone()
            .oneshot(authed_get(&format!("/documents/{id}/related"), &token))
            .await
            .unwrap();
        assert_eq!(body_json(res).await["related"].as_array().unwrap().len(), 1);
    }

    // Unknown target → 404; self-link → 400; bad rel id → 404.
    let res = app
        .clone()
        .oneshot(authed_json(
            "POST",
            &format!("/documents/{a}/relationships"),
            &token,
            serde_json::json!({ "target_document_id": "doc-missing" }),
        ))
        .await
        .unwrap();
    assert_eq!(res.status(), StatusCode::NOT_FOUND);
    let res = app
        .clone()
        .oneshot(authed_json(
            "POST",
            &format!("/documents/{a}/relationships"),
            &token,
            serde_json::json!({ "target_document_id": a }),
        ))
        .await
        .unwrap();
    assert_eq!(res.status(), StatusCode::BAD_REQUEST);
    let res = app
        .clone()
        .oneshot(
            Request::builder()
                .method("DELETE")
                .uri(format!("/documents/{a}/relationships/rel-nope"))
                .header("authorization", format!("Bearer {token}"))
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(res.status(), StatusCode::NOT_FOUND);

    // Delete via the target side too (incoming link counts).
    let res = app
        .clone()
        .oneshot(
            Request::builder()
                .method("DELETE")
                .uri(format!("/documents/{b}/relationships/{rel_id}"))
                .header("authorization", format!("Bearer {token}"))
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(res.status(), StatusCode::OK);
    assert_eq!(body_json(res).await["deleted"], true);
}

#[tokio::test]
async fn folder_patch_and_delete() {
    let (app, token, _dir) = test_app().await;
    let doc_id = upload_doc(&app, &token).await;

    let res = app
        .clone()
        .oneshot(authed_json(
            "POST",
            "/folders",
            &token,
            serde_json::json!({ "name": "inbox" }),
        ))
        .await
        .unwrap();
    let folder_id = body_json(res).await["folder"]["id"]
        .as_str()
        .unwrap()
        .to_string();

    // Rename.
    let res = app
        .clone()
        .oneshot(authed_json(
            "PATCH",
            &format!("/folders/{folder_id}"),
            &token,
            serde_json::json!({ "name": "archive" }),
        ))
        .await
        .unwrap();
    assert_eq!(res.status(), StatusCode::OK);
    assert_eq!(body_json(res).await["folder"]["name"], "archive");

    // Move doc into folder via document patch, then delete folder →
    // doc returns to root.
    app.clone()
        .oneshot(authed_json(
            "PATCH",
            &format!("/documents/{doc_id}"),
            &token,
            serde_json::json!({ "folder_id": folder_id }),
        ))
        .await
        .unwrap();
    let res = app
        .clone()
        .oneshot(
            Request::builder()
                .method("DELETE")
                .uri(format!("/folders/{folder_id}"))
                .header("authorization", format!("Bearer {token}"))
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(res.status(), StatusCode::OK);
    let res = app
        .clone()
        .oneshot(authed_get(&format!("/documents/{doc_id}"), &token))
        .await
        .unwrap();
    assert!(body_json(res).await["document"]["folder_id"].is_null());

    // Missing folder → 404 on both verbs.
    let res = app
        .clone()
        .oneshot(authed_json(
            "PATCH",
            "/folders/fld-missing",
            &token,
            serde_json::json!({ "name": "x" }),
        ))
        .await
        .unwrap();
    assert_eq!(res.status(), StatusCode::NOT_FOUND);
    let res = app
        .clone()
        .oneshot(
            Request::builder()
                .method("DELETE")
                .uri("/folders/fld-missing")
                .header("authorization", format!("Bearer {token}"))
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(res.status(), StatusCode::NOT_FOUND);
}

#[tokio::test]
async fn schedules_crud_and_tick() {
    let (app, token, _dir) = test_app().await;

    // Empty list.
    let res = app
        .clone()
        .oneshot(authed_get("/schedules", &token))
        .await
        .unwrap();
    assert_eq!(res.status(), StatusCode::OK);
    assert!(
        body_json(res).await["schedules"]
            .as_array()
            .unwrap()
            .is_empty()
    );

    // Validation: zero interval → 400; missing fields → 400.
    let res = app
        .clone()
        .oneshot(authed_json(
            "POST",
            "/schedules",
            &token,
            serde_json::json!({
                "name": "bad", "kind": "system.ping", "interval_secs": 0
            }),
        ))
        .await
        .unwrap();
    assert_eq!(res.status(), StatusCode::BAD_REQUEST);
    let res = app
        .clone()
        .oneshot(authed_json(
            "POST",
            "/schedules",
            &token,
            serde_json::json!({ "name": "only-name" }),
        ))
        .await
        .unwrap();
    // Missing required fields → axum's 422 (serde rejection).
    assert_eq!(res.status(), StatusCode::UNPROCESSABLE_ENTITY);

    // Create a schedule due in the past so tick fires it.
    let past = 1_000_000i64;
    let res = app
        .clone()
        .oneshot(authed_json(
            "POST",
            "/schedules",
            &token,
            serde_json::json!({
                "name": "gc",
                "kind": "system.ping",
                "payload": {"n": 1},
                "interval_secs": 60,
                "next_run_at": past,
                "enabled": true
            }),
        ))
        .await
        .unwrap();
    assert_eq!(res.status(), StatusCode::OK);
    let schedule = body_json(res).await["schedule"].clone();
    assert_eq!(schedule["name"], "gc");
    assert_eq!(schedule["next_run_at"], past);

    // Upsert same name → still one row.
    let res = app
        .clone()
        .oneshot(authed_json(
            "POST",
            "/schedules",
            &token,
            serde_json::json!({
                "name": "gc", "kind": "system.ping", "interval_secs": 120
            }),
        ))
        .await
        .unwrap();
    assert_eq!(res.status(), StatusCode::OK);
    let res = app
        .clone()
        .oneshot(authed_get("/schedules", &token))
        .await
        .unwrap();
    assert_eq!(
        body_json(res).await["schedules"].as_array().unwrap().len(),
        1
    );

    // Tick at a time after next_run_at enqueues one job; second tick
    // does not double-fire.
    let res = app
        .clone()
        .oneshot(authed_json(
            "POST",
            "/schedules/tick",
            &token,
            serde_json::json!({ "now": past + 200 }),
        ))
        .await
        .unwrap();
    assert_eq!(res.status(), StatusCode::OK);
    assert_eq!(body_json(res).await["enqueued"], 1);
    let res = app
        .clone()
        .oneshot(authed_json(
            "POST",
            "/schedules/tick",
            &token,
            serde_json::json!({ "now": past + 200 }),
        ))
        .await
        .unwrap();
    assert_eq!(body_json(res).await["enqueued"], 0);

    // Delete → 200; again → 404.
    let res = app
        .clone()
        .oneshot(
            Request::builder()
                .method("DELETE")
                .uri("/schedules/gc")
                .header("authorization", format!("Bearer {token}"))
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(res.status(), StatusCode::OK);
    let res = app
        .clone()
        .oneshot(
            Request::builder()
                .method("DELETE")
                .uri("/schedules/gc")
                .header("authorization", format!("Bearer {token}"))
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(res.status(), StatusCode::NOT_FOUND);
}
