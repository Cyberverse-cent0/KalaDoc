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
/// plus a valid bearer token for a freshly created user.
async fn test_app() -> (Router, String, tempfile::TempDir) {
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

    (router(Arc::new(state)), token, storage_dir)
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
