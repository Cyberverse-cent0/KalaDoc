//! End-to-end integration tests for the durable job engine
//! (`kaladoc::crates::job_engine`) driving the real built-in document
//! handlers from `application::job_handlers`.
//!
//! Covered:
//! - `system.ping` enqueue → `run_once` → `succeeded`
//! - `document.ingest` end-to-end: bytes on disk + artifact row +
//!   `Imported` audit event
//! - retry/backoff fields and terminal `failed` after `max_attempts`
//! - cancellation before a job ever runs
//! - crash recovery via `JobEngine::recover` and `reap_stale`
//! - `document.metadata_extract` / `document.index` /
//!   `thumbnail.generate` on a real (lopdf-built) PDF
//! - concurrent `run_once` claimants never double-process a job

use std::future::Future;
use std::path::PathBuf;
use std::pin::Pin;
use std::sync::Arc;
use std::sync::atomic::{AtomicU64, AtomicUsize, Ordering};
use std::time::Duration;

use kaladoc::crates::application::job_handlers::{
    self, DocumentIndexHandler, DocumentIngestHandler, DocumentMetadataExtractHandler,
    ThumbnailGenerateHandler, enqueue_index, enqueue_ingest, enqueue_metadata_extract,
    enqueue_thumbnail,
};
use kaladoc::crates::document_engine::audit::DocumentAuditEventType;
use kaladoc::crates::document_engine::document::{DocumentOrigin, DocumentSource, DocumentType};
use kaladoc::crates::document_engine::ids::DocumentId;
use kaladoc::crates::document_engine::store::artifacts::list_artifacts;
use kaladoc::crates::document_engine::store::documents::{NewDocument, create_document};
use kaladoc::crates::document_engine::store::history::get_document_history;
use kaladoc::crates::document_engine::store::metadata::get_document_metadata;
use kaladoc::crates::document_engine::store::schema::init_document_store;
use kaladoc::crates::job_engine::{
    Job, JobContext, JobEngine, JobEngineConfig, JobError, JobHandler, JobStatus, NewJob,
    cancel_job, create_jobs_table, list_jobs,
};
use kaladoc::crates::storage::local::config::StorageConfig;
use lopdf::content::{Content, Operation};
use lopdf::{Document as LopdfDocument, Object, Stream, dictionary};
use serde_json::json;
use sqlx::sqlite::{SqliteConnectOptions, SqlitePoolOptions};
use sqlx::{Row, SqlitePool};

// ---------------------------------------------------------------------------
// Shared fixtures
// ---------------------------------------------------------------------------

static NEXT_DIR: AtomicU64 = AtomicU64::new(0);

/// In-memory pool with both schemas. `max_connections(1)` keeps every
/// connection on the same `:memory:` database.
async fn mem_pool() -> SqlitePool {
    let pool = SqlitePoolOptions::new()
        .max_connections(1)
        .connect("sqlite::memory:")
        .await
        .unwrap();
    create_jobs_table(&pool).await.unwrap();
    init_document_store(&pool).await.unwrap();
    pool
}

/// File-backed pool — required for the concurrency test, since each
/// `:memory:` connection would otherwise get its own private database.
async fn file_pool() -> (SqlitePool, PathBuf) {
    let dir = temp_root();
    let db = dir.join("jobs.db");
    let opts = SqliteConnectOptions::new()
        .filename(&db)
        .create_if_missing(true);
    let pool = SqlitePoolOptions::new()
        .max_connections(4)
        .connect_with(opts)
        .await
        .unwrap();
    create_jobs_table(&pool).await.unwrap();
    init_document_store(&pool).await.unwrap();
    (pool, dir)
}

fn engine(pool: SqlitePool, backoff_base: Duration) -> JobEngine {
    JobEngine::new(
        pool,
        JobEngineConfig {
            poll_interval: Duration::from_millis(10),
            default_max_attempts: 3,
            backoff_base,
            stale_after: Duration::from_secs(60),
        },
    )
}

fn temp_root() -> PathBuf {
    let dir = std::env::temp_dir().join(format!(
        "kaladoc-jobs-e2e-{}-{}",
        std::process::id(),
        NEXT_DIR.fetch_add(1, Ordering::Relaxed)
    ));
    std::fs::create_dir_all(&dir).unwrap();
    dir
}

async fn make_document(pool: &SqlitePool, name: &str) -> DocumentId {
    create_document(
        pool,
        &NewDocument {
            name: name.into(),
            document_type: DocumentType::Pdf,
            owner_id: "user-e2e".into(),
            source: DocumentSource::new(DocumentOrigin::Upload),
        },
    )
    .await
    .unwrap()
    .id
}

/// Registers the document handlers that need a storage root.
fn register_doc_handlers(engine: &mut JobEngine, root: &std::path::Path) {
    let storage = || StorageConfig::new(root);
    engine.register(
        job_handlers::DOCUMENT_INGEST_KIND,
        Arc::new(DocumentIngestHandler::with_storage_config(storage())),
    );
    engine.register(
        job_handlers::DOCUMENT_METADATA_EXTRACT_KIND,
        Arc::new(DocumentMetadataExtractHandler::with_storage_config(
            storage(),
        )),
    );
    engine.register(
        job_handlers::DOCUMENT_INDEX_KIND,
        Arc::new(DocumentIndexHandler::with_storage_config(storage())),
    );
    engine.register(
        job_handlers::THUMBNAIL_GENERATE_KIND,
        Arc::new(ThumbnailGenerateHandler::with_storage_config(storage())),
    );
}

fn new_job(kind: &str, payload: serde_json::Value) -> NewJob {
    NewJob {
        kind: kind.into(),
        payload,
        priority: 0,
        run_at: None,
        max_attempts: None,
    }
}

async fn job_by_id(pool: &SqlitePool, id: i64) -> kaladoc::crates::job_engine::Job {
    list_jobs(pool, None, 1000)
        .await
        .unwrap()
        .into_iter()
        .find(|j| j.id == id)
        .unwrap()
}

/// Forces a job due again regardless of its scheduled `run_at`.
async fn force_due(pool: &SqlitePool, id: i64) {
    sqlx::query("UPDATE jobs_tbl SET run_at = 0 WHERE id = ?")
        .bind(id)
        .execute(pool)
        .await
        .unwrap();
}

fn now_unix() -> i64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap()
        .as_secs() as i64
}

// ---------------------------------------------------------------------------
// Test handlers
// ---------------------------------------------------------------------------

/// Fails the first `fail_times` invocations, then succeeds.
struct FlakyHandler {
    calls: AtomicUsize,
    fail_times: usize,
}

impl JobHandler for FlakyHandler {
    fn handle<'a>(
        &'a self,
        _ctx: &'a JobContext,
        _job: &'a Job,
    ) -> Pin<Box<dyn Future<Output = Result<(), JobError>> + Send + 'a>> {
        Box::pin(async move {
            let n = self.calls.fetch_add(1, Ordering::SeqCst);
            if n < self.fail_times {
                Err(JobError::Failed("boom".into()))
            } else {
                Ok(())
            }
        })
    }
}

/// Always fails.
struct AlwaysFailHandler;

impl JobHandler for AlwaysFailHandler {
    fn handle<'a>(
        &'a self,
        _ctx: &'a JobContext,
        _job: &'a Job,
    ) -> Pin<Box<dyn Future<Output = Result<(), JobError>> + Send + 'a>> {
        Box::pin(async move { Err(JobError::Failed("permanent".into())) })
    }
}

/// Counts invocations and sleeps briefly — used to prove concurrent
/// claimants don't double-process.
struct SlowCountingHandler {
    calls: AtomicUsize,
}

impl JobHandler for SlowCountingHandler {
    fn handle<'a>(
        &'a self,
        _ctx: &'a JobContext,
        _job: &'a Job,
    ) -> Pin<Box<dyn Future<Output = Result<(), JobError>> + Send + 'a>> {
        Box::pin(async move {
            self.calls.fetch_add(1, Ordering::SeqCst);
            tokio::time::sleep(Duration::from_millis(15)).await;
            Ok(())
        })
    }
}

// ---------------------------------------------------------------------------
// system.ping
// ---------------------------------------------------------------------------

#[tokio::test]
async fn system_ping_enqueue_run_once_succeeds() {
    let pool = mem_pool().await;
    // `register_builtin` wires the real `SystemPingHandler` (it is
    // private to `job_handlers`; the builtin registry is the public
    // way to get it). Storage-dependent handlers may be skipped when
    // no app config exists — irrelevant for ping.
    let mut eng = engine(pool.clone(), Duration::from_secs(0));
    job_handlers::register_builtin(&mut eng);

    let job = eng
        .enqueue(&new_job(job_handlers::SYSTEM_PING_KIND, json!({})))
        .await
        .unwrap();
    assert_eq!(job.status, JobStatus::Pending);

    assert!(eng.run_once().await.unwrap());
    assert!(!eng.run_once().await.unwrap()); // queue drained

    let jobs = list_jobs(&pool, None, 10).await.unwrap();
    assert_eq!(jobs.len(), 1);
    assert_eq!(jobs[0].id, job.id);
    assert_eq!(jobs[0].status, JobStatus::Succeeded);
    assert_eq!(jobs[0].attempts, 1);
    assert!(jobs[0].finished_at.is_some());
    assert!(jobs[0].last_error.is_none());
}

// ---------------------------------------------------------------------------
// document.ingest
// ---------------------------------------------------------------------------

#[tokio::test]
async fn document_ingest_end_to_end() {
    let pool = mem_pool().await;
    let root = temp_root();
    let doc_id = make_document(&pool, "ingest-me").await;

    let mut eng = engine(pool.clone(), Duration::from_secs(0));
    register_doc_handlers(&mut eng, &root);

    let bytes = b"%PDF-1.4 e2e ingest payload";
    let job = enqueue_ingest(
        &pool,
        &doc_id,
        Some("payload.pdf"),
        Some("application/pdf"),
        bytes,
    )
    .await
    .unwrap();
    assert_eq!(job.kind, job_handlers::DOCUMENT_INGEST_KIND);

    assert!(eng.run_once().await.unwrap());
    assert!(!eng.run_once().await.unwrap());

    // Bytes landed under the deterministic object name.
    let on_disk = root
        .join("documents")
        .join(doc_id.as_str())
        .join("payload.pdf");
    assert_eq!(std::fs::read(&on_disk).unwrap(), bytes);

    // Artifact row with checksum + local:// URI.
    let arts = list_artifacts(&pool, &doc_id).await.unwrap();
    assert_eq!(arts.len(), 1);
    assert_eq!(arts[0].file_name, "payload.pdf");
    assert_eq!(arts[0].mime_type.as_deref(), Some("application/pdf"));
    assert_eq!(
        arts[0].storage_uri,
        format!("local://documents/{}/payload.pdf", doc_id)
    );
    assert_eq!(arts[0].size_bytes, bytes.len() as u64);
    let checksum = arts[0].checksum.as_ref().unwrap();
    assert_eq!(
        checksum.algorithm,
        kaladoc::crates::document_engine::artifact::ChecksumAlgorithm::Sha256
    );
    assert_eq!(checksum.value.len(), 64);

    // Audit trail got an `imported` event referencing the artifact.
    let history = get_document_history(&pool, &doc_id).await.unwrap();
    let imported = history
        .iter()
        .find(|e| e.event_type == DocumentAuditEventType::Imported)
        .expect("expected an Imported audit event");
    assert_eq!(
        imported.details.as_ref().unwrap()["artifact_id"],
        json!(arts[0].id)
    );

    // Job succeeded.
    let done = job_by_id(&pool, job.id).await;
    assert_eq!(done.status, JobStatus::Succeeded);
    assert_eq!(done.attempts, 1);

    std::fs::remove_dir_all(&root).ok();
}

// ---------------------------------------------------------------------------
// retry / backoff / permanent failure
// ---------------------------------------------------------------------------

#[tokio::test]
async fn retry_backoff_then_success() {
    let pool = mem_pool().await;
    // 5 s base → attempt 1 reschedules at now + 5*2^1 = +10 s,
    // attempt 2 at +20 s (exponential backoff).
    let mut eng = engine(pool.clone(), Duration::from_secs(5));
    let handler = Arc::new(FlakyHandler {
        calls: AtomicUsize::new(0),
        fail_times: 2,
    });
    eng.register("test.flaky", handler.clone());

    let job = eng
        .enqueue(&new_job("test.flaky", json!({})))
        .await
        .unwrap();

    // Attempt 1: fails, returns to pending with backoff.
    assert!(eng.run_once().await.unwrap());
    let after1 = job_by_id(&pool, job.id).await;
    assert_eq!(after1.status, JobStatus::Pending);
    assert_eq!(after1.attempts, 1);
    assert_eq!(after1.last_error.as_deref(), Some("boom"));
    assert!(after1.started_at.is_none() == false);
    // Backoff pushed run_at into the future (~+10 s).
    assert!(after1.run_at > now_unix(), "run_at={}", after1.run_at);
    let run_at1 = after1.run_at;

    // Not claimable while backed off.
    assert!(!eng.run_once().await.unwrap());

    // Force due → attempt 2 fails again with a longer backoff.
    force_due(&pool, job.id).await;
    assert!(eng.run_once().await.unwrap());
    let after2 = job_by_id(&pool, job.id).await;
    assert_eq!(after2.status, JobStatus::Pending);
    assert_eq!(after2.attempts, 2);
    assert!(
        after2.run_at > run_at1,
        "backoff should grow: {} -> {}",
        run_at1,
        after2.run_at
    );

    // Force due → attempt 3 succeeds.
    force_due(&pool, job.id).await;
    assert!(eng.run_once().await.unwrap());
    let done = job_by_id(&pool, job.id).await;
    assert_eq!(done.status, JobStatus::Succeeded);
    assert_eq!(done.attempts, 3);
    assert_eq!(handler.calls.load(Ordering::SeqCst), 3);
}

#[tokio::test]
async fn permanent_failure_marks_failed_after_max_attempts() {
    let pool = mem_pool().await;
    let mut eng = engine(pool.clone(), Duration::from_secs(1));
    eng.register("test.always_fail", Arc::new(AlwaysFailHandler));

    let job = eng
        .enqueue(&NewJob {
            max_attempts: Some(2),
            ..new_job("test.always_fail", json!({}))
        })
        .await
        .unwrap();
    assert_eq!(job.max_attempts, 2);

    // Attempt 1 → pending with backoff.
    assert!(eng.run_once().await.unwrap());
    let after1 = job_by_id(&pool, job.id).await;
    assert_eq!(after1.status, JobStatus::Pending);
    assert_eq!(after1.attempts, 1);

    // Attempt 2 (final) → failed.
    force_due(&pool, job.id).await;
    assert!(eng.run_once().await.unwrap());
    let done = job_by_id(&pool, job.id).await;
    assert_eq!(done.status, JobStatus::Failed);
    assert_eq!(done.attempts, 2);
    assert_eq!(done.last_error.as_deref(), Some("permanent"));
    assert!(done.finished_at.is_some());

    // Terminal: nothing left to claim.
    assert!(!eng.run_once().await.unwrap());
}

// ---------------------------------------------------------------------------
// cancel
// ---------------------------------------------------------------------------

#[tokio::test]
async fn cancelled_job_never_runs() {
    let pool = mem_pool().await;
    let mut eng = engine(pool.clone(), Duration::from_secs(0));
    let handler = Arc::new(FlakyHandler {
        calls: AtomicUsize::new(0),
        fail_times: 0,
    });
    eng.register("test.never", handler.clone());

    let job = eng
        .enqueue(&new_job("test.never", json!({})))
        .await
        .unwrap();

    assert!(cancel_job(&pool, job.id).await.unwrap());
    // Second cancel is a no-op on a terminal job.
    assert!(!cancel_job(&pool, job.id).await.unwrap());

    assert!(!eng.run_once().await.unwrap());

    let done = job_by_id(&pool, job.id).await;
    assert_eq!(done.status, JobStatus::Cancelled);
    assert!(done.finished_at.is_some());
    assert_eq!(handler.calls.load(Ordering::SeqCst), 0);
}

// ---------------------------------------------------------------------------
// crash recovery
// ---------------------------------------------------------------------------

/// Simulates a worker that died mid-flight: row stuck in `running`.
async fn mark_running(pool: &SqlitePool, job_id: i64) {
    sqlx::query(
        "UPDATE jobs_tbl
         SET status = 'running', worker_id = 'dead-worker',
             started_at = ?, attempts = attempts + 1,
             last_error = 'simulated crash'
         WHERE id = ?",
    )
    .bind(now_unix() - 10)
    .bind(job_id)
    .execute(pool)
    .await
    .unwrap();
}

#[tokio::test]
async fn recover_requeues_interrupted_job() {
    let pool = mem_pool().await;
    let mut eng = engine(pool.clone(), Duration::from_secs(0));
    let handler = Arc::new(FlakyHandler {
        calls: AtomicUsize::new(0),
        fail_times: 0,
    });
    eng.register("test.crash", handler.clone());

    let job = eng
        .enqueue(&new_job("test.crash", json!({})))
        .await
        .unwrap();
    mark_running(&pool, job.id).await;

    // Startup recovery returns the orphaned job to pending.
    assert_eq!(eng.recover().await.unwrap(), 1);
    let requeued = job_by_id(&pool, job.id).await;
    assert_eq!(requeued.status, JobStatus::Pending);
    assert_eq!(requeued.worker_id, None);
    assert_eq!(requeued.started_at, None);
    assert_eq!(
        requeued.last_error.as_deref(),
        Some("recovered: interrupted by shutdown")
    );

    // ...and it completes normally on the next run_once.
    assert!(eng.run_once().await.unwrap());
    let done = job_by_id(&pool, job.id).await;
    assert_eq!(done.status, JobStatus::Succeeded);
    assert_eq!(handler.calls.load(Ordering::SeqCst), 1);
}

#[tokio::test]
async fn reap_stale_requeues_old_running_job() {
    let pool = mem_pool().await;
    let mut eng = engine(pool.clone(), Duration::from_secs(0));
    eng.register(
        "test.stale",
        Arc::new(FlakyHandler {
            calls: AtomicUsize::new(0),
            fail_times: 0,
        }),
    );

    let job = eng
        .enqueue(&new_job("test.stale", json!({})))
        .await
        .unwrap();
    // started_at 10 s ago; stale_after is 60 s in `engine()` — make
    // the row older than the threshold explicitly.
    sqlx::query(
        "UPDATE jobs_tbl
         SET status = 'running', worker_id = 'dead-worker',
             started_at = ?, attempts = attempts + 1
         WHERE id = ?",
    )
    .bind(now_unix() - 120)
    .bind(job.id)
    .execute(&pool)
    .await
    .unwrap();

    assert_eq!(eng.reap_stale().await.unwrap(), 1);
    let requeued = job_by_id(&pool, job.id).await;
    assert_eq!(requeued.status, JobStatus::Pending);
    assert_eq!(
        requeued.last_error.as_deref(),
        Some("reaped: stale running job")
    );

    assert!(eng.run_once().await.unwrap());
    assert_eq!(job_by_id(&pool, job.id).await.status, JobStatus::Succeeded);
}

// ---------------------------------------------------------------------------
// document processing pipeline on a real PDF
// ---------------------------------------------------------------------------

/// `BT /Tf /Td /Tj /ET` text object.
fn text_stream(text: &str) -> Vec<u8> {
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

/// One-page PDF carrying extractable text AND a 2×2 DeviceGray image
/// XObject so `thumbnail.generate` has something to find.
fn pdf_with_text_and_image() -> Vec<u8> {
    let mut doc = LopdfDocument::with_version("1.5");

    let pages_id = doc.new_object_id();
    let font_id = doc.add_object(dictionary! {
        "Type" => "Font",
        "Subtype" => "Type1",
        "BaseFont" => "Courier",
    });
    // 2×2 8-bit grayscale thumbnail candidate, unfiltered raw samples.
    let image_id = doc.add_object(Stream::new(
        dictionary! {
            "Type" => "XObject",
            "Subtype" => "Image",
            "Width" => 2,
            "Height" => 2,
            "ColorSpace" => "DeviceGray",
            "BitsPerComponent" => 8,
        },
        vec![0, 85, 170, 255],
    ));
    let resources_id = doc.add_object(dictionary! {
        "Font" => dictionary! { "F1" => font_id },
        "XObject" => dictionary! { "Im1" => image_id },
    });

    let content_id = doc.add_object(Stream::new(
        dictionary! {},
        text_stream("Hello KalaDoc e2e"),
    ));
    let page = dictionary! {
        "Type" => "Page",
        "Parent" => pages_id,
        "Resources" => resources_id,
        "MediaBox" => vec![0.into(), 0.into(), 595.into(), 842.into()],
        "Contents" => content_id,
    };
    let page_id = doc.add_object(page);

    let pages = dictionary! {
        "Type" => "Pages",
        "Kids" => vec![Object::Reference(page_id)],
        "Count" => 1,
        "Resources" => resources_id,
        "MediaBox" => vec![0.into(), 0.into(), 595.into(), 842.into()],
    };
    doc.objects.insert(pages_id, Object::Dictionary(pages));

    let catalog_id = doc.add_object(dictionary! {
        "Type" => "Catalog",
        "Pages" => pages_id,
    });
    let info_id = doc.add_object(dictionary! {
        "Title" => Object::string_literal("e2e pdf"),
        "Author" => Object::string_literal("jobs_e2e"),
    });
    doc.trailer.set("Root", catalog_id);
    doc.trailer.set("Info", info_id);

    let mut buf = Vec::new();
    doc.save_to(&mut buf).unwrap();
    buf
}

#[tokio::test]
async fn document_processing_pipeline_on_real_pdf() {
    let pool = mem_pool().await;
    let root = temp_root();
    let doc_id = make_document(&pool, "pipeline").await;

    let mut eng = engine(pool.clone(), Duration::from_secs(0));
    register_doc_handlers(&mut eng, &root);

    // 1) Ingest the PDF bytes.
    let pdf = pdf_with_text_and_image();
    let ingest_job = enqueue_ingest(
        &pool,
        &doc_id,
        Some("doc.pdf"),
        Some("application/pdf"),
        &pdf,
    )
    .await
    .unwrap();
    assert!(eng.run_once().await.unwrap());
    assert_eq!(
        job_by_id(&pool, ingest_job.id).await.status,
        JobStatus::Succeeded
    );
    assert_eq!(
        std::fs::read(root.join("documents").join(doc_id.as_str()).join("doc.pdf")).unwrap(),
        pdf
    );

    // 2) metadata_extract.
    let meta_job = enqueue_metadata_extract(&pool, &doc_id, None)
        .await
        .unwrap();
    assert!(eng.run_once().await.unwrap());
    assert_eq!(
        job_by_id(&pool, meta_job.id).await.status,
        JobStatus::Succeeded
    );

    // 3) index.
    let index_job = enqueue_index(&pool, &doc_id, None).await.unwrap();
    assert!(eng.run_once().await.unwrap());
    assert_eq!(
        job_by_id(&pool, index_job.id).await.status,
        JobStatus::Succeeded
    );

    // 4) thumbnail.generate.
    let thumb_job = enqueue_thumbnail(&pool, &doc_id, None).await.unwrap();
    assert!(eng.run_once().await.unwrap());
    assert_eq!(
        job_by_id(&pool, thumb_job.id).await.status,
        JobStatus::Succeeded
    );

    // Metadata assertions.
    let meta = get_document_metadata(&pool, &doc_id)
        .await
        .unwrap()
        .expect("metadata row should exist");
    let cf = &meta.custom_fields;

    assert_eq!(cf["detected_format"], json!("pdf"));
    assert_eq!(cf["mime_type"], json!("application/pdf"));
    assert_eq!(cf["metadata_extract_status"], json!("extracted"));
    assert_eq!(cf["processing_status"], json!("succeeded"));
    assert_eq!(cf["page_count"], json!(1));
    assert_eq!(cf["pdf_encrypted"], json!(false));
    // Title/author columns filled from the PDF Info dict.
    assert_eq!(meta.title.as_deref(), Some("e2e pdf"));
    assert_eq!(meta.author.as_deref(), Some("jobs_e2e"));

    assert_eq!(cf["index_status"], json!("indexed"));
    let index = &cf["index"];
    assert_eq!(index["status"], json!("indexed"));
    assert_eq!(index["format"], json!("pdf"));
    assert!(
        index["excerpt"]
            .as_str()
            .unwrap()
            .contains("Hello KalaDoc e2e"),
        "index excerpt: {index}"
    );

    assert_eq!(cf["thumbnail_status"], json!("generated"));
    assert_eq!(cf["thumbnail_format"], json!("png"));
    assert_eq!(cf["thumbnail_page"], json!(1));
    let thumb_uri = cf["thumbnail_uri"].as_str().unwrap().to_string();
    assert!(thumb_uri.starts_with("local://documents/"));
    // Thumbnail bytes exist on disk under the storage root.
    let thumb_path = root.join(thumb_uri.trim_start_matches("local://"));
    assert!(thumb_path.is_file(), "missing {}", thumb_path.display());
    assert!(std::fs::metadata(&thumb_path).unwrap().len() > 0);

    // Audit trail covers the whole pipeline.
    let history = get_document_history(&pool, &doc_id).await.unwrap();
    let has = |t: DocumentAuditEventType| history.iter().any(|e| e.event_type == t);
    assert!(has(DocumentAuditEventType::Imported));
    assert!(has(DocumentAuditEventType::ProcessingStarted));
    assert!(has(DocumentAuditEventType::ProcessingCompleted));
    assert!(has(DocumentAuditEventType::Indexed));
    assert!(has(DocumentAuditEventType::ThumbnailGenerated));

    // All four jobs succeeded; nothing left pending.
    let succeeded = list_jobs(&pool, Some(JobStatus::Succeeded), 50)
        .await
        .unwrap();
    assert_eq!(succeeded.len(), 4);
    assert!(!eng.run_once().await.unwrap());

    std::fs::remove_dir_all(&root).ok();
}

// ---------------------------------------------------------------------------
// concurrent claiming
// ---------------------------------------------------------------------------

#[tokio::test]
async fn concurrent_run_once_never_double_processes() {
    let (pool, dir) = file_pool().await;
    let handler = Arc::new(SlowCountingHandler {
        calls: AtomicUsize::new(0),
    });
    let mut eng = JobEngine::new(
        pool.clone(),
        JobEngineConfig {
            poll_interval: Duration::from_millis(10),
            default_max_attempts: 3,
            backoff_base: Duration::from_secs(0),
            stale_after: Duration::from_secs(60),
        },
    );
    eng.register("test.concurrent", handler.clone());
    let eng = Arc::new(eng);

    const N: usize = 8;
    for _ in 0..N {
        eng.enqueue(&new_job("test.concurrent", json!({})))
            .await
            .unwrap();
    }

    // Four worker tasks race run_once until the queue drains.
    let mut handles = Vec::new();
    for _ in 0..4 {
        let eng = eng.clone();
        handles.push(tokio::spawn(async move {
            let mut ran = 0usize;
            loop {
                match eng.run_once().await {
                    Ok(true) => ran += 1,
                    Ok(false) => break,
                    Err(e) => panic!("run_once failed: {e}"),
                }
                // Safety valve against an infinite loop.
                if ran > N * 4 {
                    panic!("run_once did not drain");
                }
            }
            ran
        }));
    }
    let mut total = 0usize;
    for h in handles {
        total += h.await.unwrap();
    }

    assert_eq!(total, N, "each job must be claimed exactly once");
    assert_eq!(handler.calls.load(Ordering::SeqCst), N);

    let jobs = list_jobs(&pool, None, 100).await.unwrap();
    assert_eq!(jobs.len(), N);
    assert!(jobs.iter().all(|j| j.status == JobStatus::Succeeded));

    std::fs::remove_dir_all(&dir).ok();
}
