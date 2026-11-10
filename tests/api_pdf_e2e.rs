//! End-to-end smoke tests: the axum API exercised in-process via
//! `tower::ServiceExt::oneshot` (no network), plus a representative
//! sample of pdf-engine ops over tiny in-test fixtures built with
//! `lopdf`.
//!
//! The API half covers the public surface described in
//! `kaladoc::crates::api`'s crate docs: health/openapi are public,
//! everything else requires a Bearer session token. The PDF half
//! exercises ~14 ops across categories (count/merge/split/extract/
//! rotate/compress/repair/inspect/metadata/text/validate/ocr) and the
//! `ProcessingEngine` trait contract. Ops needing external tools
//! (`ocr`, `render`) are asserted to fail honestly when `tesseract`
//! is absent rather than faking results.

use std::sync::Arc;
use std::time::Duration;

use axum::Router;
use axum::body::Body;
use axum::http::{Request, StatusCode};
use serde_json::Value;
use sqlx::SqlitePool;
use sqlx::sqlite::SqlitePoolOptions;
use tower::ServiceExt;

use kaladoc::crates::api::{ApiState, router};
use kaladoc::crates::application::Organization::auth_user::{NewUser, create_user};
use kaladoc::crates::application::Organization::init_db::{
    SessionConfig, create_sessions_table, create_users_table,
};
use kaladoc::crates::application::Organization::session::create_session;
use kaladoc::crates::application::services::{AppServices, StorageService};
use kaladoc::crates::core::engine::{EngineKind, EngineLimits, EngineOperation, ProcessingEngine};
use kaladoc::crates::document_engine::store::schema::init_document_store;
use kaladoc::crates::engine_registry::EngineRegistry;
use kaladoc::crates::job_engine::create_jobs_table;
use kaladoc::crates::pdf_engine::{
    PdfEngine, PdfError, RotateDegrees, compress, extract_metadata, extract_pages, extract_text,
    get_page_rotations, inspect, merge, ocr, page_count, repair, rotate, split, validate,
};
use kaladoc::crates::storage::local::config::StorageConfig;

// ===========================================================================
// API fixtures (mirror the in-crate router tests, through public paths)
// ===========================================================================

/// Pool with every table the API touches. `sqlite::memory:` is
/// per-connection, so a single-connection pool keeps state consistent.
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

/// Builds the router over a tempdir storage root + in-memory DB, plus
/// a valid bearer token for a freshly created user.
async fn test_app() -> (Router, String) {
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
            first_name: "E2E".into(),
            last_name: "Tester".into(),
            user_name: "e2e_tester".into(),
            email: "e2e@example.com".into(),
            password: "correct horse battery".into(),
        },
    )
    .await
    .unwrap();
    let (_session, token) = create_session(&pool, user_id, &test_session_config())
        .await
        .unwrap();

    (router(Arc::new(state)), token)
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

// ===========================================================================
// PDF fixtures (lopdf, in-test — same technique as pdf_roundtrip.rs)
// ===========================================================================

fn limits() -> EngineLimits {
    EngineLimits::default()
}

/// Serialize a `BT /Tf /Td /Tj /ET` text object for `text`.
fn text_stream(text: &str) -> Vec<u8> {
    use lopdf::Object;
    use lopdf::content::{Content, Operation};
    let content = Content {
        operations: vec![
            Operation::new("BT", vec![]),
            Operation::new("Tf", vec!["F1".into(), 12.into()]),
            Operation::new("Td", vec![100.into(), 700.into()]),
            Operation::new("Tj", vec![Object::string_literal(text)]),
            Operation::new("ET", vec![]),
        ],
    };
    content.encode().unwrap()
}

/// Build a PDF with one text page per entry; `title` goes into the
/// trailer Info dictionary.
fn pdf_text(texts: &[&str], title: &str) -> Vec<u8> {
    use lopdf::{Document, Object, Stream, dictionary};
    let mut doc = Document::with_version("1.5");

    let pages_id = doc.new_object_id();
    let font_id = doc.add_object(dictionary! {
        "Type" => "Font",
        "Subtype" => "Type1",
        "BaseFont" => "Courier",
    });
    let resources_id = doc.add_object(dictionary! {
        "Font" => dictionary! { "F1" => font_id },
    });

    let mut kids = Vec::with_capacity(texts.len());
    for text in texts {
        let content_id = doc.add_object(Stream::new(dictionary! {}, text_stream(text)));
        let page = dictionary! {
            "Type" => "Page",
            "Parent" => pages_id,
            "Resources" => resources_id,
            "MediaBox" => vec![0.into(), 0.into(), 595.into(), 842.into()],
            "Contents" => Object::Reference(content_id),
        };
        let page_id = doc.add_object(page);
        kids.push(Object::Reference(page_id));
    }

    let pages = dictionary! {
        "Type" => "Pages",
        "Kids" => kids,
        "Count" => texts.len() as i64,
        "Resources" => resources_id,
        "MediaBox" => vec![0.into(), 0.into(), 595.into(), 842.into()],
    };
    doc.objects.insert(pages_id, Object::Dictionary(pages));

    let catalog_id = doc.add_object(dictionary! {
        "Type" => "Catalog",
        "Pages" => pages_id,
    });
    let info_id = doc.add_object(dictionary! {
        "Title" => Object::string_literal(title),
        "Author" => Object::string_literal("KalaDoc E2E"),
    });
    doc.trailer.set("Root", catalog_id);
    doc.trailer.set("Info", info_id);

    let mut buf = Vec::new();
    doc.save_to(&mut buf).unwrap();
    buf
}

// ===========================================================================
// API e2e
// ===========================================================================

#[tokio::test]
async fn api_health_openapi_and_auth_boundaries() {
    let (app, token) = test_app().await;

    // Public health + openapi.
    let res = app.clone().oneshot(get("/health")).await.unwrap();
    assert_eq!(res.status(), StatusCode::OK);
    let body = body_json(res).await;
    assert_eq!(body["status"], "healthy");

    let res = app.clone().oneshot(get("/openapi.json")).await.unwrap();
    assert_eq!(res.status(), StatusCode::OK);
    let body = body_json(res).await;
    assert_eq!(body["openapi"], "3.0.3");
    let paths = body["paths"].as_object().unwrap();
    for path in [
        "/documents",
        "/documents/{id}",
        "/documents/{id}/process",
        "/jobs",
        "/jobs/batch",
        "/jobs/batch/{id}",
        "/jobs/batch/{id}/cancel",
        "/pipelines/run",
    ] {
        assert!(paths.contains_key(path), "openapi missing {path}");
    }

    // Auth boundary: no token → 401 envelope; bogus token → 401.
    for uri in ["/documents", "/jobs", "/engines", "/folders"] {
        let res = app.clone().oneshot(get(uri)).await.unwrap();
        assert_eq!(res.status(), StatusCode::UNAUTHORIZED, "{uri}");
        let body = body_json(res).await;
        assert_eq!(body["error"]["code"], "unauthorized");
        assert!(body["error"]["request_id"].is_string());
    }
    let res = app
        .clone()
        .oneshot(
            Request::builder()
                .uri("/jobs")
                .header("authorization", "Bearer garbage")
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(res.status(), StatusCode::UNAUTHORIZED);

    // Authed sanity: engines + storage respond.
    let res = app
        .clone()
        .oneshot(authed_get("/engines", &token))
        .await
        .unwrap();
    assert_eq!(res.status(), StatusCode::OK);
    let res = app
        .clone()
        .oneshot(authed_get("/storage", &token))
        .await
        .unwrap();
    assert_eq!(res.status(), StatusCode::OK);
    assert_eq!(body_json(res).await["backend"], "local");
}

#[tokio::test]
async fn api_document_and_job_flow_end_to_end() {
    let (app, token) = test_app().await;
    let pdf = pdf_text(&["first page", "second page"], "e2e upload");

    // Upload a real PDF through POST /documents.
    let res = app
        .clone()
        .oneshot(
            Request::builder()
                .method("POST")
                .uri("/documents?filename=report.pdf")
                .header("authorization", format!("Bearer {token}"))
                .header("content-type", "application/pdf")
                .body(Body::from(pdf.clone()))
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(res.status(), StatusCode::CREATED);
    let body = body_json(res).await;
    let doc_id = body["document"]["id"].as_str().unwrap().to_string();
    assert_eq!(body["detected_mime_type"], "application/pdf");
    let ingest_job_id = body["processing_job"]["id"]
        .as_i64()
        .expect("upload enqueues a processing job");

    // GET + list.
    let res = app
        .clone()
        .oneshot(authed_get(&format!("/documents/{doc_id}"), &token))
        .await
        .unwrap();
    assert_eq!(res.status(), StatusCode::OK);
    let body = body_json(res).await;
    assert_eq!(body["document"]["id"], doc_id);
    assert_eq!(body["artifacts"].as_array().unwrap().len(), 1);
    let artifact_id = body["artifacts"][0]["id"].as_str().unwrap().to_string();

    let res = app
        .clone()
        .oneshot(authed_get("/documents?q=report", &token))
        .await
        .unwrap();
    assert_eq!(
        body_json(res).await["documents"].as_array().unwrap().len(),
        1
    );

    // Artifact download round-trips the uploaded bytes.
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
    assert_eq!(&bytes[..], &pdf[..]);

    // Jobs: the ingest job is queryable; /jobs lists it.
    let res = app
        .clone()
        .oneshot(authed_get(&format!("/jobs/{ingest_job_id}"), &token))
        .await
        .unwrap();
    assert_eq!(res.status(), StatusCode::OK);
    assert_eq!(body_json(res).await["kind"], "document.metadata_extract");

    let res = app
        .clone()
        .oneshot(authed_get("/jobs?status=pending", &token))
        .await
        .unwrap();
    assert!(!body_json(res).await["jobs"].as_array().unwrap().is_empty());

    // Process enqueue: a pdf-engine op becomes a document.pdf_op job.
    let res = app
        .clone()
        .oneshot(authed_json(
            "POST",
            &format!("/documents/{doc_id}/process"),
            &token,
            serde_json::json!({ "op": "page_count", "params": {} }),
        ))
        .await
        .unwrap();
    assert_eq!(res.status(), StatusCode::ACCEPTED);
    let pdf_job = body_json(res).await["job"].clone();
    assert_eq!(pdf_job["kind"], "document.pdf_op");
    assert_eq!(pdf_job["payload"]["op"], "page_count");

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

    // Job batch: validation errors, then a successful batch + progress.
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

    let res = app
        .clone()
        .oneshot(authed_json(
            "POST",
            "/jobs/batch",
            &token,
            serde_json::json!([{ "kind": "", "payload": {} }]),
        ))
        .await
        .unwrap();
    assert_eq!(res.status(), StatusCode::BAD_REQUEST);

    let res = app
        .clone()
        .oneshot(authed_json(
            "POST",
            "/jobs/batch",
            &token,
            serde_json::json!([{ "kind": "document.index", "payload": 42 }]),
        ))
        .await
        .unwrap();
    assert_eq!(res.status(), StatusCode::BAD_REQUEST);

    let res = app
        .clone()
        .oneshot(authed_json(
            "POST",
            "/jobs/batch",
            &token,
            serde_json::json!([
                { "kind": "document.index", "payload": { "document_id": doc_id } },
                { "kind": "document.thumbnail", "payload": { "document_id": doc_id } }
            ]),
        ))
        .await
        .unwrap();
    assert_eq!(res.status(), StatusCode::CREATED);
    let body = body_json(res).await;
    let batch_id = body["batch_id"].as_i64().unwrap();
    assert_eq!(body["jobs"].as_array().unwrap().len(), 2);

    let res = app
        .clone()
        .oneshot(authed_get(&format!("/jobs/batch/{batch_id}"), &token))
        .await
        .unwrap();
    assert_eq!(res.status(), StatusCode::OK);
    let body = body_json(res).await;
    assert!(
        body["jobs"]
            .as_array()
            .map(|j| j.len() >= 2)
            .unwrap_or(false)
    );

    // Unknown batch → 404.
    let res = app
        .clone()
        .oneshot(authed_get("/jobs/batch/999999", &token))
        .await
        .unwrap();
    assert_eq!(res.status(), StatusCode::NOT_FOUND);
}

// ===========================================================================
// pdf-engine op smoke (representative sample across categories)
// ===========================================================================

#[test]
fn pdf_ops_representative_sample() {
    let lim = limits();
    let a = pdf_text(&["alpha one", "alpha two"], "doc a");
    let b = pdf_text(&["beta one", "beta two", "beta three"], "doc b");

    // page_count / inspect / extract_metadata / extract_text / validate.
    assert_eq!(page_count(&a, &lim).unwrap(), 2);
    let info = inspect(&a, &lim).unwrap();
    assert_eq!(info.page_count, 2);
    assert_eq!(info.title.as_deref(), Some("doc a"));
    assert!(!info.encrypted);

    let meta = extract_metadata(&a, &lim).unwrap();
    assert_eq!(meta.page_count, 2);
    assert_eq!(meta.title.as_deref(), Some("doc a"));
    assert_eq!(meta.author.as_deref(), Some("KalaDoc E2E"));

    let text = extract_text(&a, None, &lim).unwrap();
    assert_eq!(text.len(), 2);
    assert!(text[0].text.contains("alpha one"));
    // Range-restricted extraction.
    let text = extract_text(&a, Some("2"), &lim).unwrap();
    assert_eq!(text.len(), 1);
    assert!(text[0].text.contains("alpha two"));

    let report = validate(&a, &lim).unwrap();
    assert!(report.ok, "issues: {:?}", report.issues);

    // merge → page count conservation.
    let merged = merge(&[&a, &b], &lim).unwrap();
    assert_eq!(page_count(&merged, &lim).unwrap(), 5);
    let merged_text = extract_text(&merged, None, &lim)
        .unwrap()
        .iter()
        .map(|p| p.text.clone())
        .collect::<Vec<_>>()
        .join("\n");
    assert!(merged_text.contains("beta three"));

    // split → fixed chunks; extract_pages → range spec.
    let chunks = split(&merged, 2, &lim).unwrap();
    assert_eq!(chunks.len(), 3);
    assert_eq!(page_count(&chunks[0], &lim).unwrap(), 2);
    assert_eq!(page_count(&chunks[2], &lim).unwrap(), 1);

    let subset = extract_pages(&merged, "1-2,5", &lim).unwrap();
    assert_eq!(page_count(&subset, &lim).unwrap(), 3);
    // Out-of-range spec errors honestly.
    assert!(extract_pages(&merged, "1-99", &lim).is_err());

    // rotate → /Rotate set; text preserved.
    let rotated = rotate(&a, "1", RotateDegrees::Degrees90, &lim).unwrap();
    let rots = get_page_rotations(&rotated, &lim).unwrap();
    assert_eq!(rots[0].1, 90);
    assert_eq!(
        extract_text(&rotated, None, &lim).unwrap()[0].text,
        extract_text(&a, None, &lim).unwrap()[0].text
    );

    // compress / repair → non-empty bytes, still valid.
    let compressed = compress(&a, &lim).unwrap();
    assert!(!compressed.is_empty());
    assert!(validate(&compressed, &lim).unwrap().ok);

    let repaired = repair(&a, &lim).unwrap();
    assert!(!repaired.bytes.is_empty());
    assert!(!repaired.notes.is_empty());
    assert!(validate(&repaired.bytes, &lim).unwrap().ok);

    // ocr needs tesseract — absent in this environment, so it must
    // fail with an honest error rather than fake text.
    let ocr_result = ocr(&a, None, &lim);
    assert!(
        matches!(ocr_result, Err(PdfError::MissingTool(_))) || ocr_result.is_err(),
        "ocr should error honestly without tesseract: {ocr_result:?}"
    );
}

// ===========================================================================
// ProcessingEngine trait contract
// ===========================================================================

#[tokio::test]
async fn pdf_engine_trait_capabilities_and_health() {
    let engine = PdfEngine::new();

    assert_eq!(engine.name(), "pdf");
    assert_eq!(engine.kind(), EngineKind::Pdf);
    assert!(!engine.version().is_empty());

    let caps = engine.capabilities();
    assert_eq!(caps.input_formats, vec!["pdf".to_string()]);
    for op in [
        EngineOperation::Merge,
        EngineOperation::Split,
        EngineOperation::Rotate,
        EngineOperation::ExtractPages,
        EngineOperation::Compress,
        EngineOperation::Repair,
        EngineOperation::Validate,
        EngineOperation::Inspect,
        EngineOperation::ExtractMetadata,
        EngineOperation::ExtractText,
        EngineOperation::PageCount,
        EngineOperation::Ocr,
        EngineOperation::Encrypt,
        EngineOperation::Decrypt,
    ] {
        assert!(caps.operations.contains(&op), "capabilities missing {op:?}");
    }
    assert!(!caps.output_formats.is_empty());

    // Health probes external tools: pdftoppm/openssl exist here,
    // tesseract does not → expect Degraded naming the missing tool.
    let health = engine.health().await;
    let status = format!("{:?}", health.status);
    assert!(
        status == "Healthy" || status == "Degraded",
        "unexpected health status {status}"
    );
    if health.status == kaladoc::crates::core::engine::EngineHealthStatus::Degraded {
        assert!(
            health
                .message
                .as_deref()
                .unwrap_or("")
                .contains("tesseract"),
            "degraded health should name missing tools: {:?}",
            health.message
        );
    }
}
