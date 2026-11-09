//! Tests for the pipeline engine.
//!
//! Uses the minimal-PDF byte pattern from the security-engine tests
//! plus `lopdf`-built documents (same approach as pdf-engine's test
//! utils) for steps that need real page content.

use super::*;
use crate::crates::core::engine::{
    EngineHealthStatus, EngineKind, EngineOperation, ProcessingEngine,
};
use crate::crates::pdf_engine::PdfEngine;
use crate::crates::security_engine::ResourceLimits;

/// Minimal but structurally valid one-page PDF (same fixture as
/// security-engine tests).
const MINIMAL_PDF: &[u8] = b"%PDF-1.4\n\
1 0 obj<</Type/Catalog/Pages 2 0 R>>endobj\n\
2 0 obj<</Type/Pages/Kids[3 0 R]/Count 1>>endobj\n\
3 0 obj<</Type/Page/Parent 2 0 R/MediaBox[0 0 612 792]>>endobj\n\
trailer<</Root 1 0 R/ID[<0123456789abcdef0123456789abcdef><0123456789abcdef0123456789abcdef>]>>\n\
%%EOF";

/// Build a small valid PDF with one text page via lopdf — mirrors
/// pdf-engine's `test_utils::pdf_with_pages`.
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

fn intake_def() -> PipelineDefinition {
    PipelineDefinition {
        name: "test.intake".to_string(),
        description: None,
        steps: vec![
            PipelineStep::new("gate", engine_name::SECURITY, operation::VALIDATE),
            PipelineStep::new("check", engine_name::PDF, operation::PDF_VALIDATE),
            PipelineStep::new("info", engine_name::PDF, operation::PDF_INSPECT),
            PipelineStep::new("text", engine_name::PDF, operation::PDF_EXTRACT_TEXT),
        ],
    }
}

#[test]
fn intake_pipeline_runs_all_steps_and_reports() {
    let engine = PipelineEngine::new();
    let pdf = pdf_with_text("hello pipeline");
    let out = engine.run_pipeline(&intake_def(), &pdf).unwrap();

    assert_eq!(out.pipeline, "test.intake");
    assert_eq!(out.steps.len(), 4);
    // Byte stream passes through report steps unchanged.
    match &out.artifact {
        PipelineArtifact::Bytes(b) => assert_eq!(b, &pdf),
        other => panic!("expected bytes artifact, got {other:?}"),
    }
    // inspect report carries page count.
    let inspect = &out.steps[2].output.as_ref().unwrap();
    assert_eq!(inspect["page_count"], 1);
    // extract_text report carries the page text.
    let text = &out.steps[3].output.as_ref().unwrap();
    assert!(text[0]["text"].as_str().unwrap().contains("hello pipeline"));
}

#[test]
fn run_named_executes_builtin_pdf_intake() {
    let engine = PipelineEngine::new();
    let out = engine.run_named("pdf.intake", MINIMAL_PDF).unwrap();
    assert_eq!(out.pipeline, "pdf.intake");
    assert_eq!(out.steps.len(), 4);
}

#[test]
fn run_named_unknown_pipeline_fails() {
    let engine = PipelineEngine::new();
    assert!(matches!(
        engine.run_named("nope", MINIMAL_PDF),
        Err(PipelineError::PipelineNotFound(name)) if name == "nope"
    ));
}

#[test]
fn fails_fast_with_step_context() {
    let engine = PipelineEngine::new();
    // Second step gets non-PDF bytes after the first step's transform —
    // simpler: feed garbage that passes step 1? security.validate
    // rejects garbage immediately, so use a def whose first step is a
    // transform and second a pdf op on bad output.
    let def = PipelineDefinition {
        name: "fail.fast".to_string(),
        description: None,
        steps: vec![
            PipelineStep::new("extract", engine_name::PDF, operation::PDF_EXTRACT_PAGES)
                .with_params(serde_json::json!({"pages": "all"})),
            PipelineStep::new("inspect", engine_name::PDF, operation::PDF_INSPECT),
            PipelineStep::new("bogus", engine_name::PDF, operation::PDF_INSPECT),
        ],
    };
    // Garbage input fails at step 0 with index/name/engine context.
    let err = engine.run_pipeline(&def, b"not a pdf").unwrap_err();
    match err {
        PipelineError::StepFailed {
            index,
            name,
            engine,
            operation,
            ..
        } => {
            assert_eq!(index, 0);
            assert_eq!(name, "extract");
            assert_eq!(engine, "pdf");
            assert_eq!(operation, "extract_pages");
        }
        other => panic!("expected StepFailed, got {other}"),
    }
}

#[test]
fn definition_problems_rejected_before_execution() {
    let engine = PipelineEngine::new();

    // Empty definition.
    let empty = PipelineDefinition {
        name: "empty".to_string(),
        description: None,
        steps: vec![],
    };
    assert!(matches!(
        engine.run_pipeline(&empty, MINIMAL_PDF),
        Err(PipelineError::InvalidDefinition(_))
    ));

    // Unknown engine.
    let bad_engine = PipelineDefinition {
        name: "bad".to_string(),
        description: None,
        steps: vec![PipelineStep::new("x", "image", "resize")],
    };
    assert!(matches!(
        engine.run_pipeline(&bad_engine, MINIMAL_PDF),
        Err(PipelineError::UnknownEngine { engine, .. }) if engine == "image"
    ));

    // Unsupported operation on a known engine.
    let bad_op = PipelineDefinition {
        name: "bad".to_string(),
        description: None,
        steps: vec![PipelineStep::new("x", "pdf", "merge")],
    };
    assert!(matches!(
        engine.run_pipeline(&bad_op, MINIMAL_PDF),
        Err(PipelineError::UnsupportedOperation { operation, .. }) if operation == "merge"
    ));

    // Non-object params.
    let bad_params = PipelineDefinition {
        name: "bad".to_string(),
        description: None,
        steps: vec![
            PipelineStep::new("x", "pdf", "inspect").with_params(serde_json::json!([1, 2])),
        ],
    };
    assert!(matches!(
        engine.run_pipeline(&bad_params, MINIMAL_PDF),
        Err(PipelineError::InvalidDefinition(_))
    ));
}

#[test]
fn max_steps_limit_enforced() {
    let mut limits = PipelineLimits::default();
    limits.max_steps = 2;
    let engine = PipelineEngine::with_limits(limits, ResourceLimits::default());
    assert!(matches!(
        engine.run_pipeline(&intake_def(), MINIMAL_PDF),
        Err(PipelineError::LimitExceeded(_))
    ));
}

#[test]
fn per_step_input_limit_enforced() {
    let mut limits = PipelineLimits::default();
    limits.max_input_bytes = 10;
    let engine = PipelineEngine::with_limits(limits, ResourceLimits::default());
    // security.validate would also reject, but the pipeline's own
    // per-step check fires first.
    assert!(matches!(
        engine.run_pipeline(&intake_def(), MINIMAL_PDF),
        Err(PipelineError::LimitExceeded(_))
    ));
}

#[test]
fn cumulative_byte_limit_enforced() {
    let mut limits = PipelineLimits::default();
    // Input (~200B) + extract_pages output + ... exceeds a tiny cap.
    limits.max_total_bytes = 50;
    let engine = PipelineEngine::with_limits(limits, ResourceLimits::default());
    let def = PipelineDefinition {
        name: "tiny".to_string(),
        description: None,
        steps: vec![
            PipelineStep::new("extract", engine_name::PDF, operation::PDF_EXTRACT_PAGES)
                .with_params(serde_json::json!({"pages": "all"})),
        ],
    };
    let pdf = pdf_with_text("x");
    assert!(matches!(
        engine.run_pipeline(&def, &pdf),
        Err(PipelineError::LimitExceeded(_))
    ));
}

#[test]
fn byte_transform_chains_and_split_is_terminal() {
    let engine = PipelineEngine::new();
    let pdf = pdf_with_text("rotate me");

    // extract_pages → rotate: bytes flow between transforms.
    let def = PipelineDefinition {
        name: "transform".to_string(),
        description: None,
        steps: vec![
            PipelineStep::new("extract", engine_name::PDF, operation::PDF_EXTRACT_PAGES)
                .with_params(serde_json::json!({"pages": "all"})),
            PipelineStep::new("rotate", engine_name::PDF, operation::PDF_ROTATE)
                .with_params(serde_json::json!({"pages": "all", "degrees": 90})),
        ],
    };
    let out = engine.run_pipeline(&def, &pdf).unwrap();
    match &out.artifact {
        PipelineArtifact::Bytes(b) => {
            // Output is still a valid PDF.
            PdfEngine::new().inspect(b).unwrap();
        }
        other => panic!("expected bytes, got {other:?}"),
    }

    // split produces Multiple; a following step fails fast.
    let split_then = PipelineDefinition {
        name: "split-then".to_string(),
        description: None,
        steps: vec![
            PipelineStep::new("split", engine_name::PDF, operation::PDF_SPLIT)
                .with_params(serde_json::json!({"pages_per_file": 1})),
            PipelineStep::new("inspect", engine_name::PDF, operation::PDF_INSPECT),
        ],
    };
    let err = engine.run_pipeline(&split_then, &pdf).unwrap_err();
    assert!(matches!(err, PipelineError::StepFailed { index: 1, .. }));

    // split alone is a valid terminal step.
    let split_only = PipelineDefinition {
        name: "split-only".to_string(),
        description: None,
        steps: vec![
            PipelineStep::new("split", engine_name::PDF, operation::PDF_SPLIT)
                .with_params(serde_json::json!({"pages_per_file": 1})),
        ],
    };
    let out = engine.run_pipeline(&split_only, &pdf).unwrap();
    match &out.artifact {
        PipelineArtifact::Multiple(v) => assert_eq!(v.len(), 1),
        other => panic!("expected multiple, got {other:?}"),
    }
}

#[test]
fn missing_required_params_are_step_errors() {
    let engine = PipelineEngine::new();
    let def = PipelineDefinition {
        name: "params".to_string(),
        description: None,
        steps: vec![PipelineStep::new(
            "extract",
            engine_name::PDF,
            operation::PDF_EXTRACT_PAGES,
        )],
    };
    let err = engine.run_pipeline(&def, MINIMAL_PDF).unwrap_err();
    assert!(matches!(err, PipelineError::StepFailed { index: 0, .. }));
}

#[test]
fn security_validate_upload_step() {
    let engine = PipelineEngine::new();
    let def = PipelineDefinition {
        name: "upload".to_string(),
        description: None,
        steps: vec![
            PipelineStep::new(
                "upload-gate",
                engine_name::SECURITY,
                operation::VALIDATE_UPLOAD,
            )
            .with_params(serde_json::json!({
                "filename": "doc.pdf",
                "expected_format": "pdf"
            })),
        ],
    };
    let out = engine.run_pipeline(&def, MINIMAL_PDF).unwrap();
    let report = &out.steps[0].output.as_ref().unwrap();
    assert_eq!(report["sanitized_filename"], "doc.pdf");
    assert_eq!(report["format"], "pdf");

    // Wrong expected format fails with step context.
    let bad = PipelineDefinition {
        name: "upload-bad".to_string(),
        description: None,
        steps: vec![
            PipelineStep::new(
                "upload-gate",
                engine_name::SECURITY,
                operation::VALIDATE_UPLOAD,
            )
            .with_params(serde_json::json!({
                "filename": "doc.pdf",
                "expected_format": "png"
            })),
        ],
    };
    assert!(matches!(
        engine.run_pipeline(&bad, MINIMAL_PDF),
        Err(PipelineError::StepFailed { index: 0, .. })
    ));
}

#[test]
fn definitions_and_outputs_are_serde_round_trippable() {
    let def = intake_def();
    let json = serde_json::to_string_pretty(&def).unwrap();
    let back: PipelineDefinition = serde_json::from_str(&json).unwrap();
    assert_eq!(back, def);

    let engine = PipelineEngine::new();
    let out = engine.run_pipeline(&def, MINIMAL_PDF).unwrap();
    let json = serde_json::to_string(&out).unwrap();
    let back: PipelineOutput = serde_json::from_str(&json).unwrap();
    // Artifact bytes survive the base64 round-trip.
    match (&out.artifact, &back.artifact) {
        (PipelineArtifact::Bytes(a), PipelineArtifact::Bytes(b)) => assert_eq!(a, b),
        _ => panic!("artifact mismatch"),
    }
    assert_eq!(back.steps.len(), out.steps.len());
}

#[test]
fn engine_metadata_and_capabilities() {
    let engine = PipelineEngine::new();
    assert_eq!(engine.name(), "pipeline");
    assert_eq!(engine.version(), env!("CARGO_PKG_VERSION"));
    assert_eq!(engine.kind(), EngineKind::Pipeline);

    let caps = engine.capabilities();
    for op in [
        EngineOperation::Validate,
        EngineOperation::Inspect,
        EngineOperation::ExtractText,
        EngineOperation::ExtractPages,
        EngineOperation::Rotate,
        EngineOperation::Split,
    ] {
        assert!(caps.operations.contains(&op), "missing {op:?}");
    }
    // merge needs multiple inputs — must not be advertised.
    assert!(!caps.operations.contains(&EngineOperation::Merge));
    assert!(caps.input_formats.contains(&"pdf".to_string()));
    assert!(caps.limits.max_input_bytes.is_some());
}

#[tokio::test]
async fn engine_health_is_healthy() {
    let health = PipelineEngine::new().health().await;
    assert_eq!(health.status, EngineHealthStatus::Healthy);
    assert!(health.message.is_none());
}
