//! End-to-end integration tests spanning the pipeline engine, the
//! scheduler, and the job engine.
//!
//! Pipeline coverage: built-in definitions (`pdf.intake`,
//! `pdf.extract-text`) against a generated PDF, custom
//! `PipelineDefinition` JSON round-trips, fail-fast step semantics,
//! resource limits, and the `pipeline.run` job kind driven through a
//! real [`JobEngine`] + document store + storage backend.
//!
//! Scheduler coverage: schedule CRUD, `tick` idempotency and
//! missed-interval coalescing, and `scheduler.tick` driving due
//! schedules into the job pool end-to-end.

use std::future::Future;
use std::path::PathBuf;
use std::pin::Pin;
use std::sync::Arc;
use std::sync::atomic::{AtomicU64, AtomicUsize, Ordering};
use std::time::Duration;

use kaladoc::crates::application::job_handlers::{
    DOCUMENT_INGEST_KIND, DocumentIngestHandler, PIPELINE_RUN_KIND, PipelineRunHandler,
    enqueue_ingest, enqueue_pipeline_dag, enqueue_pipeline_run,
};
use kaladoc::crates::document_engine::document::{DocumentOrigin, DocumentSource, DocumentType};
use kaladoc::crates::document_engine::store::artifacts::list_artifacts;
use kaladoc::crates::document_engine::store::documents::{NewDocument, create_document};
use kaladoc::crates::document_engine::store::metadata::get_document_metadata;
use kaladoc::crates::document_engine::store::schema::init_document_store;
use kaladoc::crates::job_engine::{
    Job, JobContext, JobEngine, JobEngineConfig, JobError, JobHandler, JobStatus, NewJob,
    create_jobs_table, list_jobs,
};
use kaladoc::crates::pipeline_engine::{
    DagDefinition, DagEdge, DagNode, PipelineArtifact, PipelineDefinition, PipelineEngine,
    PipelineError, PipelineLimits, PipelineStep, builtin_definitions, engine_name, operation,
    pdf_extract_text,
};
use kaladoc::crates::scheduler::{
    NewSchedule, SCHEDULER_TICK_KIND, Schedule, SchedulerConfig, SchedulerTickHandler,
    create_schedule_table, enqueue_tick, list_schedules, remove_schedule, tick, upsert_schedule,
};
use kaladoc::crates::security_engine::ResourceLimits;
use kaladoc::crates::storage::local::config::StorageConfig;
use serde_json::json;
use sqlx::SqlitePool;
use sqlx::sqlite::SqlitePoolOptions;

// ---------------------------------------------------------------------------
// Helpers
// ---------------------------------------------------------------------------

/// Build a small valid one-page PDF containing `text` via lopdf —
/// mirrors the pipeline-engine unit-test fixture.
fn pdf_with_text(text: &str) -> Vec<u8> {
    use lopdf::content::{Content, Operation};
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
    let content = Content {
        operations: vec![
            Operation::new("BT", vec![]),
            Operation::new("Tf", vec!["F1".into(), 12.into()]),
            Operation::new("Td", vec![100.into(), 700.into()]),
            Operation::new("Tj", vec![Object::string_literal(text)]),
            Operation::new("ET", vec![]),
        ],
    };
    let content_id = doc.add_object(Stream::new(dictionary! {}, content.encode().unwrap()));
    let page_id = doc.add_object(dictionary! {
        "Type" => "Page",
        "Parent" => pages_id,
        "Contents" => content_id,
        "Resources" => resources_id,
        "MediaBox" => vec![0.into(), 0.into(), 595.into(), 842.into()],
    });
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
    doc.trailer.set("Root", catalog_id);
    let mut buf = Vec::new();
    doc.save_to(&mut buf).unwrap();
    buf
}

fn now_unix() -> i64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs() as i64)
        .unwrap_or(0)
}

/// Pool carrying both the job queue and the scheduler tables — the
/// same shape the application wires up at startup.
async fn jobs_and_schedules_pool() -> SqlitePool {
    let pool = SqlitePoolOptions::new()
        .max_connections(1)
        .connect("sqlite::memory:")
        .await
        .unwrap();
    create_jobs_table(&pool).await.unwrap();
    create_schedule_table(&pool).await.unwrap();
    pool
}

fn fast_engine_config() -> JobEngineConfig {
    JobEngineConfig {
        poll_interval: Duration::from_millis(10),
        default_max_attempts: 3,
        backoff_base: Duration::from_secs(0),
        stale_after: Duration::from_secs(60),
    }
}

/// Drain every due job, bounded so a self-perpetuating chain (the
/// scheduler tick) cannot loop forever — the next tick is always
/// scheduled in the future, so the queue empties quickly.
async fn drain(engine: &JobEngine, max: usize) {
    for _ in 0..max {
        if !engine.run_once().await.unwrap() {
            break;
        }
    }
}

// ---------------------------------------------------------------------------
// Pipeline engine
// ---------------------------------------------------------------------------

#[test]
fn builtin_pipelines_run_against_generated_pdf() {
    let engine = PipelineEngine::new();
    let pdf = pdf_with_text("e2e builtin intake");

    // pdf.intake: security gate → validate → inspect → extract_text.
    let intake = engine.run_named("pdf.intake", &pdf).unwrap();
    assert_eq!(intake.pipeline, "pdf.intake");
    assert_eq!(intake.steps.len(), 4);
    assert_eq!(intake.steps[0].name, "security-gate");
    assert_eq!(intake.steps[1].operation, "validate");
    assert_eq!(intake.steps[2].operation, "inspect");
    // inspect report carries the page count.
    assert_eq!(intake.steps[2].output.as_ref().unwrap()["page_count"], 1);
    // extract_text report carries the drawn text.
    let text = &intake.steps[3].output.as_ref().unwrap();
    assert!(
        text[0]["text"]
            .as_str()
            .unwrap()
            .contains("e2e builtin intake")
    );
    // All steps are report steps — the artifact is the input unchanged.
    match &intake.artifact {
        PipelineArtifact::Bytes(b) => assert_eq!(b, &pdf),
        other => panic!("expected bytes artifact, got {other:?}"),
    }

    // pdf.extract-text: validate → extract_text.
    let extract = engine.run_named("pdf.extract-text", &pdf).unwrap();
    assert_eq!(extract.pipeline, "pdf.extract-text");
    assert_eq!(extract.steps.len(), 2);
    assert!(extract.steps.iter().all(|s| s.output.is_some()));

    // Unknown built-in name is a dedicated error.
    assert!(matches!(
        engine.run_named("pdf.nonexistent", &pdf),
        Err(PipelineError::PipelineNotFound(_))
    ));
}

#[test]
fn builtin_definitions_are_listed_and_serializable() {
    let defs = builtin_definitions();
    let names: Vec<&str> = defs.iter().map(|d| d.name.as_str()).collect();
    assert!(names.contains(&"pdf.intake"));
    assert!(names.contains(&"pdf.extract-text"));
    for def in &defs {
        // Every built-in must itself survive a serde round-trip.
        let json = serde_json::to_string(def).unwrap();
        let back: PipelineDefinition = serde_json::from_str(&json).unwrap();
        assert_eq!(&back, def);
    }
}

#[test]
fn custom_definition_json_roundtrip_then_run() {
    // Hand-written JSON definition — the config-file path.
    let json = serde_json::json!({
        "name": "e2e.custom",
        "description": "integration test pipeline",
        "steps": [
            { "name": "gate", "engine": "security", "operation": "validate" },
            { "name": "pages", "engine": "pdf", "operation": "page_count" },
            {
                "name": "text",
                "engine": "pdf",
                "operation": "extract_text",
                "params": { "pages": "all" }
            },
            {
                "name": "subset",
                "engine": "pdf",
                "operation": "extract_pages",
                "params": { "pages": "1" }
            }
        ]
    });
    let def: PipelineDefinition = serde_json::from_value(json.clone()).unwrap();

    // Round-trip through a string form too (config on disk).
    let text = serde_json::to_string_pretty(&def).unwrap();
    let def: PipelineDefinition = serde_json::from_str(&text).unwrap();
    assert_eq!(def.name, "e2e.custom");
    assert_eq!(def.steps.len(), 4);

    let engine = PipelineEngine::new();
    let pdf = pdf_with_text("roundtrip text");
    let out = engine.run_pipeline(&def, &pdf).unwrap();

    assert_eq!(out.pipeline, "e2e.custom");
    assert_eq!(out.steps.len(), 4);
    // Per-step results: page_count report and extract_text report.
    assert_eq!(out.steps[1].output.as_ref().unwrap()["page_count"], 1);
    assert!(
        out.steps[2].output.as_ref().unwrap()[0]["text"]
            .as_str()
            .unwrap()
            .contains("roundtrip text")
    );
    // The terminal extract_pages transform still yields a valid PDF.
    match &out.artifact {
        PipelineArtifact::Bytes(b) => {
            kaladoc::crates::pdf_engine::PdfEngine::new()
                .inspect(b)
                .unwrap();
        }
        other => panic!("expected bytes artifact, got {other:?}"),
    }

    // PipelineOutput itself round-trips through JSON (base64 artifact).
    let out_json = serde_json::to_string(&out).unwrap();
    let back: kaladoc::crates::pipeline_engine::PipelineOutput =
        serde_json::from_str(&out_json).unwrap();
    assert_eq!(back.pipeline, out.pipeline);
    assert_eq!(back.steps.len(), out.steps.len());
}

#[test]
fn step_failure_aborts_pipeline_with_step_context() {
    let engine = PipelineEngine::new();
    let pdf = pdf_with_text("fail mid-pipeline");

    // Step 0 passes (page_count report); step 1 fails (missing required
    // `pages` param); step 2 must never run — fail-fast semantics.
    let def = PipelineDefinition {
        name: "e2e.fail".to_string(),
        description: None,
        steps: vec![
            PipelineStep::new("count", engine_name::PDF, operation::PDF_PAGE_COUNT),
            PipelineStep::new("extract", engine_name::PDF, "extract_pages"),
            PipelineStep::new("never", engine_name::PDF, "inspect"),
        ],
    };

    let err = engine.run_pipeline(&def, &pdf).unwrap_err();
    match err {
        PipelineError::StepFailed {
            index,
            name,
            engine,
            operation,
            ..
        } => {
            assert_eq!(index, 1);
            assert_eq!(name, "extract");
            assert_eq!(engine, "pdf");
            assert_eq!(operation, "extract_pages");
        }
        other => panic!("expected StepFailed, got {other}"),
    }
    // No partial output escapes: run_pipeline returns Err, not a
    // PipelineOutput with truncated steps.
}

#[test]
fn limits_are_enforced() {
    let pdf = pdf_with_text("limits");
    let def = PipelineDefinition {
        name: "e2e.limits".to_string(),
        description: None,
        steps: vec![
            PipelineStep::new("a", engine_name::PDF, "page_count"),
            PipelineStep::new("b", engine_name::PDF, "inspect"),
            PipelineStep::new("c", engine_name::PDF, "validate"),
        ],
    };

    // max_steps: definition has 3 steps, limit is 2 → rejected before
    // any step executes.
    let mut limits = PipelineLimits::default();
    limits.max_steps = 2;
    let engine = PipelineEngine::with_limits(limits, ResourceLimits::default());
    assert!(matches!(
        engine.run_pipeline(&def, &pdf),
        Err(PipelineError::LimitExceeded(_))
    ));

    // max_input_bytes: input exceeds the per-step budget.
    let mut limits = PipelineLimits::default();
    limits.max_input_bytes = 8;
    let engine = PipelineEngine::with_limits(limits, ResourceLimits::default());
    assert!(matches!(
        engine.run_pipeline(&def, &pdf),
        Err(PipelineError::LimitExceeded(_))
    ));

    // max_total_bytes: cumulative budget smaller than the input.
    let mut limits = PipelineLimits::default();
    limits.max_total_bytes = 4;
    let engine = PipelineEngine::with_limits(limits, ResourceLimits::default());
    assert!(matches!(
        engine.run_pipeline(&def, &pdf),
        Err(PipelineError::LimitExceeded(_))
    ));

    // Empty definitions and unknown engines are definition errors.
    let empty = PipelineDefinition {
        name: "empty".to_string(),
        description: None,
        steps: vec![],
    };
    assert!(matches!(
        PipelineEngine::new().run_pipeline(&empty, &pdf),
        Err(PipelineError::InvalidDefinition(_))
    ));
    let bad_engine = PipelineDefinition {
        name: "bad".to_string(),
        description: None,
        steps: vec![PipelineStep::new("x", "multimedia", "probe")],
    };
    assert!(matches!(
        PipelineEngine::new().run_pipeline(&bad_engine, &pdf),
        Err(PipelineError::UnknownEngine { .. })
    ));
}

// ---------------------------------------------------------------------------
// pipeline.run job kind — JobEngine + document store + storage
// ---------------------------------------------------------------------------

static NEXT_DIR: AtomicU64 = AtomicU64::new(0);

fn temp_root() -> PathBuf {
    let dir = std::env::temp_dir().join(format!(
        "kaladoc-e2e-{}-{}",
        std::process::id(),
        NEXT_DIR.fetch_add(1, Ordering::Relaxed)
    ));
    std::fs::create_dir_all(&dir).unwrap();
    dir
}

/// Pool with the job queue *and* the document store — the shape the
/// `pipeline.run` handler needs.
async fn jobs_and_documents_pool() -> SqlitePool {
    let pool = SqlitePoolOptions::new()
        .max_connections(1)
        .connect("sqlite::memory:")
        .await
        .unwrap();
    create_jobs_table(&pool).await.unwrap();
    init_document_store(&pool).await.unwrap();
    pool
}

#[tokio::test]
async fn pipeline_run_job_executes_end_to_end() {
    let pool = jobs_and_documents_pool().await;
    let root = temp_root();

    let mut engine = JobEngine::new(pool.clone(), fast_engine_config());
    engine.register(
        DOCUMENT_INGEST_KIND,
        Arc::new(DocumentIngestHandler::with_storage_config(
            StorageConfig::new(&root),
        )),
    );
    engine.register(
        PIPELINE_RUN_KIND,
        Arc::new(PipelineRunHandler::with_storage_config(StorageConfig::new(
            &root,
        ))),
    );

    // Register a document, then ingest a real PDF as its artifact.
    let doc = create_document(
        &pool,
        &NewDocument {
            name: "e2e-report".into(),
            document_type: DocumentType::Pdf,
            owner_id: "user-e2e".into(),
            source: DocumentSource::new(DocumentOrigin::Upload),
        },
    )
    .await
    .unwrap();
    let pdf = pdf_with_text("pipeline run job text");
    enqueue_ingest(
        &pool,
        &doc.id,
        Some("report.pdf"),
        Some("application/pdf"),
        &pdf,
    )
    .await
    .unwrap();
    drain(&engine, 8).await;

    // The ingest job must have succeeded and produced an artifact row.
    let ingested = list_jobs(&pool, Some(JobStatus::Succeeded), 10)
        .await
        .unwrap();
    assert!(ingested.iter().any(|j| j.kind == DOCUMENT_INGEST_KIND));
    let artifacts_before = list_artifacts(&pool, &doc.id).await.unwrap();
    assert_eq!(artifacts_before.len(), 1);

    // Enqueue pipeline.run with the built-in pdf.extract-text
    // definition (self-contained payload — retries replay identically).
    enqueue_pipeline_run(&pool, &doc.id, None, &pdf_extract_text())
        .await
        .unwrap();
    drain(&engine, 8).await;

    // The pipeline.run job itself succeeded (deterministic pipeline
    // failures would still mark the job succeeded with an error status
    // in metadata — assert the recorded outcome, not just the job).
    let jobs = list_jobs(&pool, None, 20).await.unwrap();
    let run_job = jobs
        .iter()
        .find(|j| j.kind == PIPELINE_RUN_KIND)
        .expect("pipeline.run job missing");
    assert_eq!(
        run_job.status,
        JobStatus::Succeeded,
        "{:?}",
        run_job.last_error
    );

    // Document metadata records the per-pipeline result.
    let meta = get_document_metadata(&pool, &doc.id)
        .await
        .unwrap()
        .expect("metadata row missing");
    let status = meta
        .custom_fields
        .get("pipeline_run_status")
        .and_then(|v| v.as_str());
    assert_eq!(status, Some("succeeded"));

    let pipelines = meta
        .custom_fields
        .get("pipelines")
        .and_then(|v| v.get("pdf.extract-text"))
        .expect("pipelines.pdf.extract-text entry missing");
    assert_eq!(pipelines["status"], "succeeded");
    assert_eq!(pipelines["pipeline"], "pdf.extract-text");
    // The inline report carries both step reports.
    let steps = pipelines["report"]["steps"].as_array().unwrap();
    assert_eq!(steps.len(), 2);
    assert_eq!(steps[0]["operation"], "validate");
    assert_eq!(steps[1]["operation"], "extract_text");

    // Byte output (the pass-through PDF) was stored as an artifact.
    let artifacts_after = list_artifacts(&pool, &doc.id).await.unwrap();
    assert!(
        artifacts_after.len() > artifacts_before.len(),
        "pipeline output artifact was not stored"
    );

    std::fs::remove_dir_all(&root).ok();
}

#[tokio::test]
async fn pipeline_run_job_records_failure_in_metadata() {
    let pool = jobs_and_documents_pool().await;
    let root = temp_root();

    let mut engine = JobEngine::new(pool.clone(), fast_engine_config());
    engine.register(
        DOCUMENT_INGEST_KIND,
        Arc::new(DocumentIngestHandler::with_storage_config(
            StorageConfig::new(&root),
        )),
    );
    engine.register(
        PIPELINE_RUN_KIND,
        Arc::new(PipelineRunHandler::with_storage_config(StorageConfig::new(
            &root,
        ))),
    );

    let doc = create_document(
        &pool,
        &NewDocument {
            name: "e2e-bad".into(),
            document_type: DocumentType::Pdf,
            owner_id: "user-e2e".into(),
            source: DocumentSource::new(DocumentOrigin::Upload),
        },
    )
    .await
    .unwrap();
    // Ingest bytes that are not a PDF — the pipeline's first step
    // (pdf.validate) fails deterministically.
    enqueue_ingest(
        &pool,
        &doc.id,
        Some("bad.pdf"),
        Some("application/pdf"),
        b"not a pdf",
    )
    .await
    .unwrap();
    drain(&engine, 8).await;

    // A definition whose first step is a pdf op on garbage input.
    let bad_def = PipelineDefinition {
        name: "e2e.bad".to_string(),
        description: None,
        steps: vec![PipelineStep::new("validate", engine_name::PDF, "validate")],
    };
    enqueue_pipeline_run(&pool, &doc.id, None, &bad_def)
        .await
        .unwrap();
    drain(&engine, 8).await;

    let jobs = list_jobs(&pool, None, 20).await.unwrap();
    let run_job = jobs
        .iter()
        .find(|j| j.kind == PIPELINE_RUN_KIND)
        .expect("pipeline.run job missing");
    // Deterministic pipeline failures are terminal job *successes*;
    // the failure is recorded in document metadata.
    assert_eq!(run_job.status, JobStatus::Succeeded);

    let meta = get_document_metadata(&pool, &doc.id)
        .await
        .unwrap()
        .unwrap();
    assert_eq!(
        meta.custom_fields
            .get("pipeline_run_status")
            .and_then(|v| v.as_str()),
        Some("error")
    );
    let entry = meta
        .custom_fields
        .get("pipelines")
        .and_then(|v| v.get("e2e.bad"))
        .expect("pipelines.e2e.bad entry missing");
    assert_eq!(entry["status"], "error");
    assert!(entry["error"].as_str().unwrap().contains("validate"));

    std::fs::remove_dir_all(&root).ok();
}

#[tokio::test]
async fn pipeline_run_dag_job_executes_end_to_end() {
    let pool = jobs_and_documents_pool().await;
    let root = temp_root();

    let mut engine = JobEngine::new(pool.clone(), fast_engine_config());
    engine.register(
        DOCUMENT_INGEST_KIND,
        Arc::new(DocumentIngestHandler::with_storage_config(
            StorageConfig::new(&root),
        )),
    );
    engine.register(
        PIPELINE_RUN_KIND,
        Arc::new(PipelineRunHandler::with_storage_config(StorageConfig::new(
            &root,
        ))),
    );

    let doc = create_document(
        &pool,
        &NewDocument {
            name: "e2e-dag".into(),
            document_type: DocumentType::Pdf,
            owner_id: "user-e2e".into(),
            source: DocumentSource::new(DocumentOrigin::Upload),
        },
    )
    .await
    .unwrap();
    let pdf = pdf_with_text("dag job text");
    enqueue_ingest(
        &pool,
        &doc.id,
        Some("dag.pdf"),
        Some("application/pdf"),
        &pdf,
    )
    .await
    .unwrap();
    drain(&engine, 8).await;

    // Diamond DAG: gate → {count, inspect} → join. All report steps —
    // every node passes the input through, so no node artifacts are
    // stored, only the per-node report.
    let mut dag = DagDefinition::new("e2e.dag");
    dag.nodes = vec![
        DagNode::new(
            "gate",
            PipelineStep::new("gate", engine_name::SECURITY, "validate"),
        ),
        DagNode::new(
            "count",
            PipelineStep::new("count", engine_name::PDF, "page_count"),
        ),
        DagNode::new(
            "inspect",
            PipelineStep::new("inspect", engine_name::PDF, "inspect"),
        ),
        DagNode::new(
            "join",
            PipelineStep::new("join", engine_name::PDF, "validate"),
        ),
    ];
    dag.edges = vec![
        DagEdge::new("gate", "count"),
        DagEdge::new("gate", "inspect"),
        DagEdge::new("count", "join"),
        DagEdge::new("inspect", "join"),
    ];
    enqueue_pipeline_dag(&pool, &doc.id, None, &dag)
        .await
        .unwrap();
    drain(&engine, 8).await;

    let jobs = list_jobs(&pool, None, 20).await.unwrap();
    let run_job = jobs
        .iter()
        .find(|j| j.kind == PIPELINE_RUN_KIND)
        .expect("pipeline.run job missing");
    assert_eq!(
        run_job.status,
        JobStatus::Succeeded,
        "{:?}",
        run_job.last_error
    );
    // The payload carried `dag`, not `pipeline`.
    assert!(run_job.payload.get("dag").is_some());
    assert!(run_job.payload.get("pipeline").is_none());

    let meta = get_document_metadata(&pool, &doc.id)
        .await
        .unwrap()
        .expect("metadata row missing");
    assert_eq!(
        meta.custom_fields
            .get("pipeline_run_status")
            .and_then(|v| v.as_str()),
        Some("succeeded")
    );
    let entry = meta
        .custom_fields
        .get("pipelines")
        .and_then(|v| v.get("e2e.dag"))
        .expect("pipelines.e2e.dag entry missing");
    assert_eq!(entry["status"], "succeeded");
    assert_eq!(entry["pipeline"], "e2e.dag");

    // The inline report is the DagReport: per-node statuses in wave
    // order (gate, then sorted count/inspect, then join).
    let report = &entry["report"];
    assert_eq!(report["dag"], "e2e.dag");
    assert_eq!(report["succeeded"], 4);
    assert_eq!(report["failed"], 0);
    assert_eq!(report["skipped"], 0);
    let nodes = report["nodes"].as_array().unwrap();
    assert_eq!(nodes.len(), 4);
    let order: Vec<&str> = nodes.iter().map(|n| n["id"].as_str().unwrap()).collect();
    assert_eq!(order, vec!["gate", "count", "inspect", "join"]);
    for node in nodes {
        assert_eq!(node["status"], "succeeded");
    }
    // count is a report step — its structured output is inline.
    let count = nodes.iter().find(|n| n["id"] == "count").unwrap();
    assert_eq!(count["output"]["page_count"], 1);

    std::fs::remove_dir_all(&root).ok();
}

#[tokio::test]
async fn pipeline_run_dag_job_records_node_failure() {
    let pool = jobs_and_documents_pool().await;
    let root = temp_root();

    let mut engine = JobEngine::new(pool.clone(), fast_engine_config());
    engine.register(
        DOCUMENT_INGEST_KIND,
        Arc::new(DocumentIngestHandler::with_storage_config(
            StorageConfig::new(&root),
        )),
    );
    engine.register(
        PIPELINE_RUN_KIND,
        Arc::new(PipelineRunHandler::with_storage_config(StorageConfig::new(
            &root,
        ))),
    );

    let doc = create_document(
        &pool,
        &NewDocument {
            name: "e2e-dag-fail".into(),
            document_type: DocumentType::Pdf,
            owner_id: "user-e2e".into(),
            source: DocumentSource::new(DocumentOrigin::Upload),
        },
    )
    .await
    .unwrap();
    enqueue_ingest(
        &pool,
        &doc.id,
        Some("dag-fail.pdf"),
        Some("application/pdf"),
        &pdf_with_text("dag failure text"),
    )
    .await
    .unwrap();
    drain(&engine, 8).await;

    // bad (missing `pages` param) fails; leaf depends on it and must
    // be skipped; sibling still runs — the DAG failure policy.
    let mut dag = DagDefinition::new("e2e.dag-fail");
    dag.nodes = vec![
        DagNode::new(
            "bad",
            PipelineStep::new("bad", engine_name::PDF, "extract_pages"),
        ),
        DagNode::new(
            "leaf",
            PipelineStep::new("leaf", engine_name::PDF, "inspect"),
        ),
        DagNode::new(
            "sib",
            PipelineStep::new("sib", engine_name::PDF, "page_count"),
        ),
    ];
    dag.edges = vec![DagEdge::new("bad", "leaf")];
    enqueue_pipeline_dag(&pool, &doc.id, None, &dag)
        .await
        .unwrap();
    drain(&engine, 8).await;

    let jobs = list_jobs(&pool, None, 20).await.unwrap();
    let run_job = jobs
        .iter()
        .find(|j| j.kind == PIPELINE_RUN_KIND)
        .expect("pipeline.run job missing");
    assert_eq!(run_job.status, JobStatus::Succeeded);

    let meta = get_document_metadata(&pool, &doc.id)
        .await
        .unwrap()
        .unwrap();
    // Any node failure makes the top-level status "error"; the full
    // per-node report is still recorded.
    assert_eq!(
        meta.custom_fields
            .get("pipeline_run_status")
            .and_then(|v| v.as_str()),
        Some("error")
    );
    let entry = meta
        .custom_fields
        .get("pipelines")
        .and_then(|v| v.get("e2e.dag-fail"))
        .expect("pipelines.e2e.dag-fail entry missing");
    assert_eq!(entry["status"], "error");
    let report = &entry["report"];
    assert_eq!(report["failed"], 1);
    assert_eq!(report["skipped"], 1);
    assert_eq!(report["succeeded"], 1);
    let status_of = |id: &str| {
        report["nodes"]
            .as_array()
            .unwrap()
            .iter()
            .find(|n| n["id"] == id)
            .unwrap()["status"]
            .as_str()
            .unwrap()
            .to_string()
    };
    assert_eq!(status_of("bad"), "failed");
    assert_eq!(status_of("leaf"), "skipped");
    assert_eq!(status_of("sib"), "succeeded");

    std::fs::remove_dir_all(&root).ok();
}

// ---------------------------------------------------------------------------
// Scheduler
// ---------------------------------------------------------------------------

fn schedule(name: &str, kind: &str, interval_secs: u64, next_run_at: Option<i64>) -> NewSchedule {
    NewSchedule {
        name: name.into(),
        kind: kind.into(),
        payload: json!({"schedule": name}),
        interval_secs,
        priority: None,
        next_run_at,
        enabled: None,
    }
}

#[tokio::test]
async fn schedule_crud_and_tick_semantics() {
    let pool = jobs_and_schedules_pool().await;
    let now = now_unix();

    // Upsert: one due schedule, one future, one disabled-but-due.
    let due = upsert_schedule(
        &pool,
        &schedule("gc.due", "system.ping", 60, Some(now - 10)),
    )
    .await
    .unwrap();
    assert!(due.enabled);
    assert_eq!(due.next_run_at, now - 10);
    assert_eq!(due.last_run_at, None);

    upsert_schedule(
        &pool,
        &schedule("gc.future", "system.ping", 60, Some(now + 3600)),
    )
    .await
    .unwrap();

    let mut disabled = schedule("gc.off", "system.ping", 60, Some(now - 10));
    disabled.enabled = Some(false);
    upsert_schedule(&pool, &disabled).await.unwrap();

    let listed = list_schedules(&pool).await.unwrap();
    assert_eq!(listed.len(), 3);
    assert!(listed.iter().any(|s| s.name == "gc.due" && s.enabled));
    assert!(listed.iter().any(|s| s.name == "gc.off" && !s.enabled));

    // Upsert on the same name replaces the row (no duplicate).
    let updated = upsert_schedule(
        &pool,
        &schedule("gc.due", "storage.gc_staging", 120, Some(now + 60)),
    )
    .await
    .unwrap();
    assert_eq!(updated.kind, "storage.gc_staging");
    assert_eq!(updated.interval_secs, 120);
    assert_eq!(list_schedules(&pool).await.unwrap().len(), 3);

    // Validation: empty name/kind and zero interval are rejected.
    let mut bad = schedule("  ", "k", 60, None);
    assert!(upsert_schedule(&pool, &bad).await.is_err());
    bad = schedule("ok-name", "", 60, None);
    assert!(upsert_schedule(&pool, &bad).await.is_err());
    bad = schedule("ok-name", "k", 0, None);
    assert!(upsert_schedule(&pool, &bad).await.is_err());

    // tick fires only enabled+due schedules: gc.due was rescheduled
    // to now+60 by the upsert, so nothing is due yet.
    let out = tick(&pool, now).await.unwrap();
    assert_eq!(out.enqueued, 0);

    // Make it due again and tick: exactly one job, once.
    upsert_schedule(
        &pool,
        &schedule("gc.due", "storage.gc_staging", 60, Some(now - 5)),
    )
    .await
    .unwrap();
    let out = tick(&pool, now).await.unwrap();
    assert_eq!(out.enqueued, 1);

    // Second tick at the same instant must not double-fire.
    let out = tick(&pool, now).await.unwrap();
    assert_eq!(out.enqueued, 0);

    let jobs = list_jobs(&pool, None, 10).await.unwrap();
    assert_eq!(jobs.len(), 1);
    assert_eq!(jobs[0].kind, "storage.gc_staging");
    assert_eq!(jobs[0].payload, json!({"schedule": "gc.due"}));
    // The enqueued occurrence is stamped with the schedule's original
    // next_run_at, and the schedule advanced past `now`.
    let sched = list_schedules(&pool)
        .await
        .unwrap()
        .into_iter()
        .find(|s| s.name == "gc.due")
        .unwrap();
    assert_eq!(sched.last_run_at, Some(now));
    assert!(sched.next_run_at > now);

    // Missed intervals coalesce: a schedule three intervals in the
    // past still produces exactly one job.
    let stale = upsert_schedule(
        &pool,
        &schedule("gc.stale", "system.ping", 3600, Some(now - 10_000)),
    )
    .await
    .unwrap();
    assert!(stale.next_run_at <= now);
    let out = tick(&pool, now).await.unwrap();
    assert_eq!(out.enqueued, 1);
    let jobs = list_jobs(&pool, None, 10).await.unwrap();
    assert_eq!(jobs.len(), 2); // gc.due + gc.stale, no burst

    // Disabled schedules never fire even when far past due.
    let out = tick(&pool, now + 100_000).await.unwrap();
    // gc.future becomes due at now+3600; gc.due/gc.stale intervals
    // have elapsed too — count only *new* enqueues beyond what is
    // expected, or simply assert disabled stayed silent by checking
    // no job carries its payload.
    let _ = out;
    let jobs = list_jobs(&pool, None, 50).await.unwrap();
    assert!(
        !jobs
            .iter()
            .any(|j| j.payload == json!({"schedule": "gc.off"}))
    );

    // remove_schedule deletes by name; removing twice is false.
    assert!(remove_schedule(&pool, "gc.off").await.unwrap());
    assert!(!remove_schedule(&pool, "gc.off").await.unwrap());
    assert_eq!(list_schedules(&pool).await.unwrap().len(), 2);
}

/// Handler that counts invocations — proves scheduled jobs actually
/// execute through the engine, not just land in the table.
struct CountingHandler {
    calls: AtomicUsize,
}

impl JobHandler for CountingHandler {
    fn handle<'a>(
        &'a self,
        _ctx: &'a JobContext,
        _job: &'a Job,
    ) -> Pin<Box<dyn Future<Output = Result<(), JobError>> + Send + 'a>> {
        Box::pin(async move {
            self.calls.fetch_add(1, Ordering::SeqCst);
            Ok(())
        })
    }
}

#[tokio::test]
async fn scheduler_tick_job_drives_jobs_end_to_end() {
    let pool = jobs_and_schedules_pool().await;
    let now = now_unix();

    // A schedule ~3 intervals in the past: the tick must enqueue
    // exactly one occurrence (no burst after downtime).
    upsert_schedule(
        &pool,
        &schedule("hourly.report", "test.hourly", 3600, Some(now - 10_000)),
    )
    .await
    .unwrap();

    let counter = Arc::new(CountingHandler {
        calls: AtomicUsize::new(0),
    });
    let mut engine = JobEngine::new(pool.clone(), fast_engine_config());
    engine.register("test.hourly", counter.clone());
    engine.register(
        SCHEDULER_TICK_KIND,
        Arc::new(SchedulerTickHandler::new(SchedulerConfig {
            tick_interval_secs: 60,
            tick_priority: 0,
            tick_max_attempts: None,
        })),
    );

    // Prime the self-perpetuating tick chain, due now.
    enqueue_tick(&pool, Some(now), None).await.unwrap();

    // Drain: tick job runs tick() (enqueues the hourly occurrence +
    // the next tick ~60s out), then the occurrence job runs the
    // counting handler.
    drain(&engine, 10).await;

    assert_eq!(counter.calls.load(Ordering::SeqCst), 1);

    let jobs = list_jobs(&pool, None, 20).await.unwrap();
    let hourly: Vec<_> = jobs.iter().filter(|j| j.kind == "test.hourly").collect();
    assert_eq!(hourly.len(), 1);
    assert_eq!(hourly[0].status, JobStatus::Succeeded);

    // The tick job that ran is marked succeeded; the next tick is
    // pending far in the future (self-perpetuation, not a burst).
    let ticks_done: Vec<_> = jobs
        .iter()
        .filter(|j| j.kind == SCHEDULER_TICK_KIND && j.status == JobStatus::Succeeded)
        .collect();
    assert_eq!(ticks_done.len(), 1);
    let next_tick = jobs
        .iter()
        .find(|j| j.kind == SCHEDULER_TICK_KIND && j.status == JobStatus::Pending)
        .expect("next scheduler.tick job missing");
    assert!(next_tick.run_at > now + 30);

    // Draining again must not re-fire the schedule: next_run_at was
    // advanced past now and the next tick is not yet due.
    drain(&engine, 10).await;
    assert_eq!(counter.calls.load(Ordering::SeqCst), 1);
    let hourly: Vec<_> = list_jobs(&pool, None, 20)
        .await
        .unwrap()
        .into_iter()
        .filter(|j| j.kind == "test.hourly")
        .collect();
    assert_eq!(hourly.len(), 1);

    // Schedule row reflects the fired occurrence.
    let sched: Schedule = list_schedules(&pool)
        .await
        .unwrap()
        .into_iter()
        .find(|s| s.name == "hourly.report")
        .unwrap();
    assert!(sched.next_run_at > now);
    assert!(sched.last_run_at.is_some());
}
