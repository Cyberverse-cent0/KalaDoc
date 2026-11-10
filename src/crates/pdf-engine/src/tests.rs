//! Integration tests for [`PdfEngine`] and the `ops` functions.
//!
//! All fixtures are generated in-test with `lopdf` itself — see
//! [`crate::crates::pdf_engine::ops::test_utils`].

use lopdf::Document;

use crate::crates::core::engine::{
    EngineHealthStatus, EngineKind, EngineLimits, EngineOperation, ProcessingEngine,
};
use crate::crates::pdf_engine::error::PdfError;
use crate::crates::pdf_engine::ops::test_utils::{
    page_count, pdf_with_pages, pdf_with_pages_and_title,
};
use crate::crates::pdf_engine::{PdfEngine, RotateDegrees};

fn engine() -> PdfEngine {
    PdfEngine::new()
}

fn unlimited() -> EngineLimits {
    EngineLimits::default()
}

#[test]
fn trait_metadata() {
    let engine = engine();
    assert_eq!(engine.name(), "pdf");
    assert_eq!(engine.kind(), EngineKind::Pdf);
    assert!(!engine.version().is_empty());

    let caps = engine.capabilities();
    for op in [
        EngineOperation::Merge,
        EngineOperation::Split,
        EngineOperation::Rotate,
        EngineOperation::Resize,
        EngineOperation::Crop,
        EngineOperation::Reorder,
        EngineOperation::RemovePages,
        EngineOperation::ExtractPages,
        EngineOperation::Compress,
        EngineOperation::Repair,
        EngineOperation::Flatten,
        EngineOperation::Overlay,
        EngineOperation::Watermark,
        EngineOperation::Stamp,
        EngineOperation::Redact,
        EngineOperation::Sanitize,
        EngineOperation::VerifySignature,
        EngineOperation::Sign,
        EngineOperation::RemoveSignatures,
        EngineOperation::Validate,
        EngineOperation::Inspect,
        EngineOperation::PageCount,
        EngineOperation::ExtractMetadata,
        EngineOperation::ExtractImages,
        EngineOperation::ExtractText,
        EngineOperation::Render,
        EngineOperation::Compare,
        EngineOperation::Convert,
        EngineOperation::Ocr,
        EngineOperation::Encrypt,
        EngineOperation::Decrypt,
    ] {
        assert!(caps.operations.contains(&op), "missing {op:?}");
    }
    assert!(caps.input_formats.contains(&"pdf".to_string()));
    assert!(caps.output_formats.contains(&"pdf".to_string()));
}

/// `true` when `tool` resolves to an executable on `PATH` — mirrors
/// the probe `PdfEngine::health` performs for external render/OCR
/// tools, so the health test is environment-aware.
fn tool_on_path(tool: &str) -> bool {
    std::env::var_os("PATH")
        .map(|paths| {
            std::env::split_paths(&paths).any(|dir| {
                let candidate = dir.join(tool);
                candidate.is_file()
                    && std::fs::metadata(&candidate)
                        .map(|m| {
                            #[cfg(unix)]
                            {
                                use std::os::unix::fs::PermissionsExt;
                                m.permissions().mode() & 0o111 != 0
                            }
                            #[cfg(not(unix))]
                            {
                                true
                            }
                        })
                        .unwrap_or(false)
            })
        })
        .unwrap_or(false)
}

#[tokio::test]
async fn health_reflects_external_tool_availability() {
    let health = engine().health().await;
    // `render` needs pdftoppm, `ocr` needs tesseract, `sign` needs
    // openssl; any missing degrades health honestly rather than
    // claiming full health.
    let expected =
        if tool_on_path("pdftoppm") && tool_on_path("tesseract") && tool_on_path("openssl") {
            EngineHealthStatus::Healthy
        } else {
            EngineHealthStatus::Degraded
        };
    assert_eq!(health.status, expected);
    if expected == EngineHealthStatus::Degraded {
        assert!(health.message.is_some());
    }
}

#[test]
fn merge_two_documents() {
    let a = pdf_with_pages(&["alpha one", "alpha two"]);
    let b = pdf_with_pages(&["beta one"]);

    let merged = engine().merge(&[&a, &b]).unwrap();
    assert_eq!(page_count(&merged), 3);

    // Page order follows input order.
    let texts = engine().extract_text(&merged, None).unwrap();
    let joined: Vec<&str> = texts.iter().map(|t| t.text.trim()).collect();
    assert_eq!(joined.len(), 3);
    assert!(joined[0].contains("alpha one"));
    assert!(joined[2].contains("beta one"));
}

#[test]
fn merge_rejects_empty_input_list() {
    assert!(matches!(
        engine().merge(&[]),
        Err(PdfError::InvalidInput(_))
    ));
}

#[test]
fn split_fixed_pages() {
    let pdf = pdf_with_pages(&["p1", "p2", "p3", "p4", "p5"]);
    let parts = engine().split(&pdf, 2).unwrap();
    assert_eq!(parts.len(), 3);
    assert_eq!(page_count(&parts[0]), 2);
    assert_eq!(page_count(&parts[1]), 2);
    assert_eq!(page_count(&parts[2]), 1);

    // Each part is a standalone valid PDF with the right text.
    let first = engine().extract_text(&parts[0], None).unwrap();
    assert!(first[0].text.contains("p1"));
}

#[test]
fn split_rejects_zero_and_garbage() {
    let pdf = pdf_with_pages(&["x"]);
    assert!(matches!(
        engine().split(&pdf, 0),
        Err(PdfError::InvalidInput(_))
    ));
    assert!(matches!(
        engine().split(b"not a pdf", 1),
        Err(PdfError::InvalidPdf(_))
    ));
}

#[test]
fn rotate_sets_rotate_entry() {
    let pdf = pdf_with_pages(&["spin me"]);
    let rotated = engine()
        .rotate(&pdf, "1", RotateDegrees::Degrees90)
        .unwrap();

    let doc = Document::load_mem(&rotated).unwrap();
    let (_, page_id) = doc
        .get_pages()
        .iter()
        .next()
        .map(|(n, id)| (*n, *id))
        .unwrap();
    let dict = doc.get_dictionary(page_id).unwrap();
    let rotate = dict.get(b"Rotate").unwrap();
    assert_eq!(rotate.as_i64().unwrap(), 90);

    // 90 + 270 accumulates to 0 (mod 360).
    let again = engine()
        .rotate(&rotated, "1", RotateDegrees::Degrees270)
        .unwrap();
    let doc = Document::load_mem(&again).unwrap();
    let (_, page_id) = doc
        .get_pages()
        .iter()
        .next()
        .map(|(n, id)| (*n, *id))
        .unwrap();
    assert_eq!(
        doc.get_dictionary(page_id)
            .unwrap()
            .get(b"Rotate")
            .unwrap()
            .as_i64()
            .unwrap(),
        0
    );
}

#[test]
fn rotate_rejects_bad_degrees_and_range() {
    let pdf = pdf_with_pages(&["x"]);
    assert!(matches!(
        RotateDegrees::try_from(45u16),
        Err(PdfError::InvalidInput(_))
    ));
    assert!(matches!(
        engine().rotate(&pdf, "0", RotateDegrees::Degrees90),
        Err(PdfError::InvalidPageRange(_))
    ));
    assert!(matches!(
        engine().rotate_degrees(&pdf, "1", 45),
        Err(PdfError::InvalidInput(_))
    ));
}

#[test]
fn extract_pages_by_range() {
    let pdf = pdf_with_pages(&["p1", "p2", "p3", "p4", "p5"]);

    let out = engine().extract_pages(&pdf, "2,4").unwrap();
    assert_eq!(page_count(&out), 2);
    let texts = engine().extract_text(&out, None).unwrap();
    assert!(texts[0].text.contains("p2"));
    assert!(texts[1].text.contains("p4"));

    let out = engine().extract_pages(&pdf, "1-3").unwrap();
    assert_eq!(page_count(&out), 3);

    assert!(matches!(
        engine().extract_pages(&pdf, "3-1"),
        Err(PdfError::InvalidPageRange(_))
    ));
}

#[test]
fn inspect_reports_metadata() {
    let pdf = pdf_with_pages_and_title(&["hello"], "My Report");
    let info = engine().inspect(&pdf).unwrap();
    assert_eq!(info.page_count, 1);
    assert_eq!(info.title.as_deref(), Some("My Report"));
    assert_eq!(info.author.as_deref(), Some("KalaDoc Tests"));
    assert!(!info.encrypted);
    assert_eq!(info.file_size_bytes, pdf.len() as u64);
}

#[test]
fn validate_clean_document() {
    let pdf = pdf_with_pages(&["fine"]);
    let report = engine().validate(&pdf).unwrap();
    assert!(report.ok, "issues: {:?}", report.issues);
    assert!(report.issues.is_empty());
}

#[test]
fn validate_garbage_is_error_not_report() {
    assert!(matches!(
        engine().validate(b"\x00\x01garbage"),
        Err(PdfError::InvalidPdf(_))
    ));
}

#[test]
fn extract_text_per_page() {
    let pdf = pdf_with_pages(&["first page", "second page"]);
    let pages = engine().extract_text(&pdf, Some("2")).unwrap();
    assert_eq!(pages.len(), 1);
    assert_eq!(pages[0].page, 2);
    assert!(pages[0].text.contains("second page"));
}

#[test]
fn limits_are_enforced() {
    let pdf = pdf_with_pages(&["a", "b", "c"]);

    let small_input = PdfEngine::with_limits(EngineLimits {
        max_input_bytes: Some(10),
        ..unlimited()
    });
    assert!(matches!(
        small_input.inspect(&pdf),
        Err(PdfError::LimitExceeded(_))
    ));

    let few_pages = PdfEngine::with_limits(EngineLimits {
        max_pages: Some(2),
        ..unlimited()
    });
    assert!(matches!(
        few_pages.inspect(&pdf),
        Err(PdfError::LimitExceeded(_))
    ));

    let few_outputs = PdfEngine::with_limits(EngineLimits {
        max_outputs: Some(2),
        ..unlimited()
    });
    assert!(matches!(
        few_outputs.split(&pdf, 1),
        Err(PdfError::LimitExceeded(_))
    ));
}

#[test]
fn merge_enforces_combined_page_limit() {
    let a = pdf_with_pages(&["a1", "a2"]);
    let b = pdf_with_pages(&["b1", "b2"]);
    let engine = PdfEngine::with_limits(EngineLimits {
        max_pages: Some(3),
        ..unlimited()
    });
    // Each input alone is under the limit; combined they exceed it.
    assert!(matches!(
        engine.merge(&[&a, &b]),
        Err(PdfError::LimitExceeded(_))
    ));
}
