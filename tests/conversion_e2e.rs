//! Integration tests for the conversion engine (`kaladoc::crates::conversion_engine`).
//!
//! These tests exercise the *public* API only. External tools
//! (pandoc, libreoffice, poppler) may or may not be installed; every
//! test that needs one detects it first and skips honestly with an
//! `eprintln!` rather than failing.

use std::sync::{Arc, Mutex};

use kaladoc::crates::conversion_engine::{
    BackendAvailability, ConversionBackend, ConversionEngine, ConvertError, ConvertOutput,
    ConvertRequest, ConvertResult, Format,
};
use kaladoc::crates::core::engine::{EngineKind, EngineOperation, ProcessingEngine};

// ---------------------------------------------------------------------------
// Helpers
// ---------------------------------------------------------------------------

fn req(input_format: Format, output_format: Format, input: &[u8]) -> ConvertRequest {
    ConvertRequest {
        input_format,
        output_format,
        input: input.to_vec(),
        options: Default::default(),
    }
}

/// Minimal `block_on` for the `EngineFuture` returned by `health()`.
fn block_on<F: std::future::Future>(fut: F) -> F::Output {
    use std::sync::Arc as StdArc;
    use std::task::{Context, Poll, Wake, Waker};

    struct Noop;
    impl Wake for Noop {
        fn wake(self: StdArc<Self>) {}
    }

    let waker = Waker::from(StdArc::new(Noop));
    let mut cx = Context::from_waker(&waker);
    let mut fut = std::pin::pin!(fut);
    loop {
        match fut.as_mut().poll(&mut cx) {
            Poll::Ready(v) => return v,
            Poll::Pending => std::thread::yield_now(),
        }
    }
}

/// Names of all detected backends (external tools + rust fallback).
fn detected_backend_names(engine: &ConversionEngine) -> Vec<String> {
    engine
        .backends()
        .iter()
        .map(|b| b.name().to_string())
        .collect()
}

fn backend_named<'a>(
    engine: &'a ConversionEngine,
    name: &str,
) -> Option<&'a Arc<dyn ConversionBackend>> {
    engine.backends().iter().find(|b| b.name() == name)
}

/// A tiny valid PNG (8×8 opaque red) encoded in memory.
fn tiny_png() -> Vec<u8> {
    let img = image::RgbaImage::from_pixel(8, 8, image::Rgba([200, 30, 30, 255]));
    let mut buf = std::io::Cursor::new(Vec::new());
    image::DynamicImage::ImageRgba8(img)
        .write_to(&mut buf, image::ImageFormat::Png)
        .expect("png encode failed");
    buf.into_inner()
}

// ---------------------------------------------------------------------------
// Configurable mock backend (same pattern as the in-crate unit tests)
// ---------------------------------------------------------------------------

#[derive(Clone, Copy)]
enum Outcome {
    Ok,
    BackendFailed,
    InvalidInput,
    TooLarge,
}

struct MockBackend {
    name: &'static str,
    availability: BackendAvailability,
    pairs: Vec<(Format, Format)>,
    outcome: Outcome,
    calls: Mutex<u32>,
}

impl MockBackend {
    fn available(name: &'static str, pairs: Vec<(Format, Format)>) -> Self {
        Self {
            name,
            availability: BackendAvailability::Available { version: None },
            pairs,
            outcome: Outcome::Ok,
            calls: Mutex::new(0),
        }
    }

    fn unavailable(name: &'static str, pairs: Vec<(Format, Format)>, reason: &str) -> Self {
        Self {
            name,
            availability: BackendAvailability::Unavailable {
                reason: reason.to_string(),
            },
            pairs,
            outcome: Outcome::Ok,
            calls: Mutex::new(0),
        }
    }

    fn with_outcome(mut self, outcome: Outcome) -> Self {
        self.outcome = outcome;
        self
    }

    fn call_count(&self) -> u32 {
        *self.calls.lock().unwrap()
    }
}

impl ConversionBackend for MockBackend {
    fn name(&self) -> &'static str {
        self.name
    }

    fn availability(&self) -> BackendAvailability {
        self.availability.clone()
    }

    fn supports(&self, from: Format, to: Format) -> bool {
        self.pairs.contains(&(from, to))
    }

    fn convert(&self, req: &ConvertRequest) -> ConvertResult<ConvertOutput> {
        *self.calls.lock().unwrap() += 1;
        match self.outcome {
            Outcome::Ok => Ok(ConvertOutput {
                bytes: vec![1, 2, 3],
                output_format: req.output_format,
                backend: self.name.to_string(),
                warnings: Vec::new(),
            }),
            Outcome::BackendFailed => Err(ConvertError::BackendFailed {
                backend: self.name.to_string(),
                message: "boom".to_string(),
            }),
            Outcome::InvalidInput => Err(ConvertError::InvalidInput {
                reason: "unparseable".to_string(),
            }),
            Outcome::TooLarge => Err(ConvertError::TooLarge {
                limit: "1 MiB".to_string(),
                actual: 2 * 1024 * 1024,
            }),
        }
    }
}

// ---------------------------------------------------------------------------
// 1. Format matrix sanity
// ---------------------------------------------------------------------------

#[test]
fn format_as_str_parse_roundtrip_all_variants() {
    const ALL: [Format; 13] = [
        Format::Pdf,
        Format::Txt,
        Format::Markdown,
        Format::Html,
        Format::Png,
        Format::Jpeg,
        Format::Docx,
        Format::Odt,
        Format::Xlsx,
        Format::Xls,
        Format::Pptx,
        Format::Ppt,
        Format::Epub,
    ];
    let mut seen = std::collections::HashSet::new();
    for f in ALL {
        let s = f.as_str();
        assert!(!s.is_empty());
        assert!(seen.insert(s), "duplicate as_str for {f:?}");
        assert_eq!(Format::parse(s), Some(f), "as_str/parse mismatch for {f:?}");
        // serde uses snake_case names identical to as_str().
        let json = serde_json::to_string(&f).unwrap();
        assert_eq!(json, format!("\"{s}\""));
        let back: Format = serde_json::from_str(&json).unwrap();
        assert_eq!(back, f);
    }
}

#[test]
fn format_parse_accepts_aliases_and_rejects_junk() {
    assert_eq!(Format::parse("md"), Some(Format::Markdown));
    assert_eq!(Format::parse("markdown"), Some(Format::Markdown));
    assert_eq!(Format::parse("text"), Some(Format::Txt));
    assert_eq!(Format::parse("txt"), Some(Format::Txt));
    assert_eq!(Format::parse("jpg"), Some(Format::Jpeg));
    assert_eq!(Format::parse("jpeg"), Some(Format::Jpeg));
    assert_eq!(Format::parse("htm"), Some(Format::Html));
    assert_eq!(Format::parse("odf"), Some(Format::Odt));
    assert_eq!(Format::parse("  PDF  "), Some(Format::Pdf)); // trims + case-insensitive
    assert_eq!(Format::parse("PDF"), Some(Format::Pdf));
    assert_eq!(Format::parse("doc"), None);
    assert_eq!(Format::parse(""), None);
    assert_eq!(Format::parse("not-a-format"), None);
}

#[test]
fn format_is_image_only_for_raster() {
    assert!(Format::Png.is_image());
    assert!(Format::Jpeg.is_image());
    assert!(!Format::Pdf.is_image());
    assert!(!Format::Docx.is_image());
}

#[test]
fn mock_backend_supports_matrix_is_honest() {
    // supports() on a fake backend gates dispatch; verify the engine
    // only routes advertised pairs.
    let b = MockBackend::available(
        "fake",
        vec![(Format::Markdown, Format::Pdf), (Format::Txt, Format::Pdf)],
    );
    assert!(b.supports(Format::Markdown, Format::Pdf));
    assert!(b.supports(Format::Txt, Format::Pdf));
    assert!(!b.supports(Format::Pdf, Format::Markdown));
    assert!(!b.supports(Format::Docx, Format::Pdf));
    assert!(!b.supports(Format::Markdown, Format::Txt));
}

// ---------------------------------------------------------------------------
// 2. Backend detection
// ---------------------------------------------------------------------------

#[test]
fn engine_detection_returns_backends_without_panic() {
    let engine = ConversionEngine::new();
    let backends = engine.backends();
    assert!(!backends.is_empty(), "engine must always have ≥1 backend");

    // The pure-Rust fallback is registered last and is always available.
    let rust =
        backend_named(&engine, "rust").expect("rust fallback backend must always be registered");
    assert!(matches!(
        rust.availability(),
        BackendAvailability::Available { .. }
    ));

    // Detected external tools are a subset of the known set; anything
    // present must report Available (detect() omits unusable tools).
    for b in backends {
        assert!(
            matches!(b.name(), "rust" | "pandoc" | "libreoffice" | "poppler"),
            "unexpected backend name: {}",
            b.name()
        );
        match b.availability() {
            BackendAvailability::Available { version } => {
                if let Some(v) = version {
                    assert!(v.len() <= 200, "version string too long");
                }
            }
            BackendAvailability::Unavailable { reason } => {
                panic!(
                    "detected backend {} reported Unavailable: {reason}",
                    b.name()
                )
            }
        }
    }

    let names = detected_backend_names(&engine);
    eprintln!("detected backends: {names:?}");
    for expected in ["pandoc", "libreoffice", "poppler"] {
        if names.iter().any(|n| n == expected) {
            eprintln!("  {expected}: present");
        } else {
            eprintln!("  {expected}: absent (skipping dependent tests)");
        }
    }
}

#[test]
fn pandoc_real_conversion_md_to_html_when_present() {
    let engine = ConversionEngine::new();
    if backend_named(&engine, "pandoc").is_none() {
        eprintln!("skipping: pandoc not detected");
        return;
    }
    let out = engine
        .convert(&req(
            Format::Markdown,
            Format::Html,
            b"# Hello\n\nsome *markdown* text\n",
        ))
        .expect("pandoc md→html failed");
    assert_eq!(out.output_format, Format::Html);
    assert!(
        out.backend.contains("pandoc"),
        "backend was {}",
        out.backend
    );
    assert!(!out.bytes.is_empty());
    let text = String::from_utf8_lossy(&out.bytes);
    assert!(
        text.contains("Hello"),
        "html output missing content: {text:?}"
    );
}

#[test]
fn libreoffice_real_conversion_png_to_pdf_when_present() {
    let engine = ConversionEngine::new();
    if backend_named(&engine, "libreoffice").is_none() {
        eprintln!("skipping: libreoffice/soffice not detected");
        return;
    }
    let png = tiny_png();
    let out = engine
        .convert(&req(Format::Png, Format::Pdf, &png))
        .expect("libreoffice png→pdf failed");
    assert_eq!(out.output_format, Format::Pdf);
    assert!(!out.bytes.is_empty());
    assert!(
        out.bytes.starts_with(b"%PDF"),
        "not a PDF: {:?}",
        &out.bytes[..8]
    );
    eprintln!("png→pdf handled by backend: {}", out.backend);
}

#[test]
fn poppler_real_conversion_pdf_to_html_when_present() {
    let engine = ConversionEngine::new();
    if backend_named(&engine, "poppler").is_none() {
        eprintln!("skipping: poppler tools not detected");
        return;
    }
    // Need a real PDF: produce one via txt→pdf first (rust backend or pandoc).
    let pdf = engine
        .convert(&req(Format::Txt, Format::Pdf, b"poppler probe text\n"))
        .expect("could not produce a PDF for poppler test");
    let out = engine
        .convert(&req(Format::Pdf, Format::Html, &pdf.bytes))
        .expect("poppler pdf→html failed");
    assert_eq!(out.output_format, Format::Html);
    assert!(
        out.backend.contains("poppler"),
        "backend was {}",
        out.backend
    );
    assert!(!out.bytes.is_empty());
}

// ---------------------------------------------------------------------------
// 3. Rust fallback backend (exercised through the engine; it is the only
//    backend guaranteeing txt/md→pdf and pdf→markdown, and is always last
//    in dispatch order)
// ---------------------------------------------------------------------------

#[test]
fn rust_txt_to_pdf_produces_valid_pdf() {
    let engine = ConversionEngine::new();
    let out = engine
        .convert(&req(
            Format::Txt,
            Format::Pdf,
            b"Hello from the conversion engine e2e test.\nSecond line.\n",
        ))
        .expect("txt→pdf failed");
    assert_eq!(out.output_format, Format::Pdf);
    assert!(out.bytes.starts_with(b"%PDF"), "not a PDF");
    assert!(
        out.bytes.len() > 500,
        "suspiciously small PDF: {} bytes",
        out.bytes.len()
    );
    eprintln!("txt→pdf handled by backend: {}", out.backend);
    // With no pandoc/pdf-engine on PATH this must be the rust backend;
    // if an external tool claimed it, that's fine too — output is what matters.
    assert!(["rust", "pandoc"].contains(&out.backend.as_str()));
}

#[test]
fn rust_markdown_to_pdf_produces_valid_pdf() {
    let engine = ConversionEngine::new();
    let out = engine
        .convert(&req(
            Format::Markdown,
            Format::Pdf,
            b"# Title\n\n- item one\n- item two\n",
        ))
        .expect("markdown→pdf failed");
    assert_eq!(out.output_format, Format::Pdf);
    assert!(out.bytes.starts_with(b"%PDF"));
    eprintln!("markdown→pdf handled by backend: {}", out.backend);
}

#[test]
fn pdf_roundtrip_txt_via_engine() {
    let engine = ConversionEngine::new();
    let pdf = engine
        .convert(&req(
            Format::Txt,
            Format::Pdf,
            b"roundtrip marker ZXQW-9876\n",
        ))
        .expect("txt→pdf failed");
    assert!(pdf.bytes.starts_with(b"%PDF"));

    // pdf→txt: poppler (pdftotext) may claim it first; rust is fallback.
    let txt = engine
        .convert(&req(Format::Pdf, Format::Txt, &pdf.bytes))
        .expect("pdf→txt failed");
    assert_eq!(txt.output_format, Format::Txt);
    assert!(!txt.bytes.is_empty());
    let text = String::from_utf8_lossy(&txt.bytes);
    assert!(
        text.contains("ZXQW-9876"),
        "extracted text lost the marker: {text:?}"
    );
    eprintln!("pdf→txt handled by backend: {}", txt.backend);
}

#[test]
fn pdf_to_markdown_only_rust_supports_it() {
    let engine = ConversionEngine::new();
    let pdf = engine
        .convert(&req(Format::Txt, Format::Pdf, b"# heading\n\nbody text\n"))
        .expect("txt→pdf failed");
    let out = engine
        .convert(&req(Format::Pdf, Format::Markdown, &pdf.bytes))
        .expect("pdf→markdown failed");
    assert_eq!(out.output_format, Format::Markdown);
    // Only the rust backend advertises pdf→markdown.
    assert_eq!(out.backend, "rust");
    assert!(!out.bytes.is_empty());
}

// ---------------------------------------------------------------------------
// 4. Error paths
// ---------------------------------------------------------------------------

#[test]
fn empty_input_is_invalid_input() {
    let engine = ConversionEngine::new();
    let err = engine
        .convert(&req(Format::Markdown, Format::Pdf, b""))
        .unwrap_err();
    assert!(
        matches!(err, ConvertError::InvalidInput { .. }),
        "expected InvalidInput, got {err:?}"
    );
}

#[test]
fn unsupported_pair_is_unsupported_conversion() {
    let engine = ConversionEngine::new();
    // epub→xlsx: nothing reads epub except pandoc, and nothing writes
    // xlsx at all.
    let err = engine
        .convert(&req(Format::Epub, Format::Xlsx, b"PK\x03\x04 fake epub"))
        .unwrap_err();
    assert!(
        matches!(
            err,
            ConvertError::UnsupportedConversion {
                from: Format::Epub,
                to: Format::Xlsx
            }
        ),
        "expected UnsupportedConversion, got {err:?}"
    );
}

#[test]
fn oversized_input_is_too_large() {
    let engine = ConversionEngine::new();
    // Engine cap is 256 MiB; allocate just over it. The check happens
    // before any backend runs.
    let big = vec![0u8; 256 * 1024 * 1024 + 1];
    let err = engine
        .convert(&req(Format::Txt, Format::Pdf, &big))
        .unwrap_err();
    assert!(
        matches!(err, ConvertError::TooLarge { .. }),
        "expected TooLarge, got {err:?}"
    );
    if let ConvertError::TooLarge { actual, .. } = err {
        assert_eq!(actual, 256 * 1024 * 1024 + 1);
    }
}

// ---------------------------------------------------------------------------
// 5. Engine-level dispatch, fallback, capabilities, health
// ---------------------------------------------------------------------------

#[test]
fn engine_dispatch_prefers_first_available_backend() {
    let first = Arc::new(MockBackend::available(
        "first",
        vec![(Format::Markdown, Format::Pdf)],
    ));
    let second = Arc::new(MockBackend::available(
        "second",
        vec![(Format::Markdown, Format::Pdf)],
    ));
    let engine = ConversionEngine::with_backends(vec![first.clone(), second.clone()]);

    let out = engine
        .convert(&req(Format::Markdown, Format::Pdf, b"# hi"))
        .unwrap();
    assert_eq!(out.backend, "first");
    assert_eq!(first.call_count(), 1);
    assert_eq!(second.call_count(), 0);
}

#[test]
fn engine_skips_unavailable_backend() {
    let dead = Arc::new(MockBackend::unavailable(
        "dead",
        vec![(Format::Markdown, Format::Pdf)],
        "tool missing",
    ));
    let live = Arc::new(MockBackend::available(
        "live",
        vec![(Format::Markdown, Format::Pdf)],
    ));
    let engine = ConversionEngine::with_backends(vec![dead, live.clone()]);

    let out = engine
        .convert(&req(Format::Markdown, Format::Pdf, b"# hi"))
        .unwrap();
    assert_eq!(out.backend, "live");
    assert_eq!(live.call_count(), 1);
}

#[test]
fn engine_falls_back_on_backend_failed() {
    let failing = Arc::new(
        MockBackend::available("failing", vec![(Format::Markdown, Format::Pdf)])
            .with_outcome(Outcome::BackendFailed),
    );
    let ok = Arc::new(MockBackend::available(
        "ok",
        vec![(Format::Markdown, Format::Pdf)],
    ));
    let engine = ConversionEngine::with_backends(vec![failing.clone(), ok.clone()]);

    let out = engine
        .convert(&req(Format::Markdown, Format::Pdf, b"# hi"))
        .unwrap();
    assert_eq!(out.backend, "ok");
    assert_eq!(failing.call_count(), 1);
    assert_eq!(ok.call_count(), 1);
}

#[test]
fn engine_falls_back_on_invalid_input_from_backend() {
    let bad = Arc::new(
        MockBackend::available("bad", vec![(Format::Markdown, Format::Pdf)])
            .with_outcome(Outcome::InvalidInput),
    );
    let ok = Arc::new(MockBackend::available(
        "ok",
        vec![(Format::Markdown, Format::Pdf)],
    ));
    let engine = ConversionEngine::with_backends(vec![bad, ok.clone()]);

    let out = engine
        .convert(&req(Format::Markdown, Format::Pdf, b"# hi"))
        .unwrap();
    assert_eq!(out.backend, "ok");
    assert_eq!(ok.call_count(), 1);
}

#[test]
fn engine_does_not_fall_back_on_too_large() {
    let failing = Arc::new(
        MockBackend::available("failing", vec![(Format::Markdown, Format::Pdf)])
            .with_outcome(Outcome::TooLarge),
    );
    let ok = Arc::new(MockBackend::available(
        "ok",
        vec![(Format::Markdown, Format::Pdf)],
    ));
    let engine = ConversionEngine::with_backends(vec![failing, ok.clone()]);

    let err = engine
        .convert(&req(Format::Markdown, Format::Pdf, b"# hi"))
        .unwrap_err();
    assert!(matches!(err, ConvertError::TooLarge { .. }));
    assert_eq!(ok.call_count(), 0);
}

#[test]
fn engine_reports_backend_unavailable_when_only_unavailable_supports() {
    let engine = ConversionEngine::with_backends(vec![Arc::new(MockBackend::unavailable(
        "dead",
        vec![(Format::Markdown, Format::Pdf)],
        "binary not found",
    ))]);

    let err = engine
        .convert(&req(Format::Markdown, Format::Pdf, b"# hi"))
        .unwrap_err();
    match err {
        ConvertError::BackendUnavailable { backend, reason } => {
            assert_eq!(backend, "dead");
            assert_eq!(reason, "binary not found");
        }
        other => panic!("expected BackendUnavailable, got {other:?}"),
    }
}

#[test]
fn engine_rejects_empty_output_from_backend() {
    struct EmptyBackend;
    impl ConversionBackend for EmptyBackend {
        fn name(&self) -> &'static str {
            "empty"
        }
        fn availability(&self) -> BackendAvailability {
            BackendAvailability::Available { version: None }
        }
        fn supports(&self, _from: Format, _to: Format) -> bool {
            true
        }
        fn convert(&self, _req: &ConvertRequest) -> ConvertResult<ConvertOutput> {
            Ok(ConvertOutput {
                bytes: Vec::new(),
                output_format: Format::Pdf,
                backend: "empty".to_string(),
                warnings: Vec::new(),
            })
        }
    }
    let engine = ConversionEngine::with_backends(vec![Arc::new(EmptyBackend)]);
    let err = engine
        .convert(&req(Format::Txt, Format::Pdf, b"hi"))
        .unwrap_err();
    assert!(
        matches!(err, ConvertError::BackendFailed { .. }),
        "expected BackendFailed for empty output, got {err:?}"
    );
}

#[test]
fn engine_capabilities_reflect_detected_backends() {
    let engine = ConversionEngine::new();
    let caps = engine.capabilities();

    assert_eq!(engine.name(), "convert");
    assert_eq!(engine.kind(), EngineKind::Conversion);
    assert!(caps.operations.contains(&EngineOperation::Convert));

    // The rust fallback alone guarantees these formats, so they must
    // always be advertised.
    for f in ["pdf", "txt", "markdown"] {
        assert!(
            caps.input_formats.contains(&f.to_string()),
            "input_formats missing {f}: {:?}",
            caps.input_formats
        );
        assert!(
            caps.output_formats.contains(&f.to_string()),
            "output_formats missing {f}: {:?}",
            caps.output_formats
        );
    }
    // Sorted/deduped sets.
    let mut sorted = caps.input_formats.clone();
    sorted.sort();
    sorted.dedup();
    assert_eq!(sorted.len(), caps.input_formats.len());

    let names = detected_backend_names(&engine);
    eprintln!(
        "capabilities with backends {names:?}: in={:?} out={:?}",
        caps.input_formats, caps.output_formats
    );
}

#[test]
fn engine_health_never_unavailable_with_rust_fallback() {
    let engine = ConversionEngine::new();
    let health = block_on(engine.health());
    // Rust backend is always Available → engine is Healthy or Degraded,
    // never Unavailable.
    use kaladoc::crates::core::engine::EngineHealthStatus;
    assert_ne!(health.status, EngineHealthStatus::Unavailable);
    if health.status == EngineHealthStatus::Degraded {
        let msg = health.message.unwrap_or_default();
        assert!(!msg.is_empty());
        // Must name unavailable backends, not leak paths.
        assert!(!msg.contains('/'), "health message leaks a path: {msg}");
    }
}

#[test]
fn engine_with_no_backends_reports_unavailable_health() {
    let engine = ConversionEngine::with_backends(vec![]);
    let health = block_on(engine.health());
    use kaladoc::crates::core::engine::EngineHealthStatus;
    assert_eq!(health.status, EngineHealthStatus::Unavailable);
    // And convert reports UnsupportedConversion (nothing supports anything).
    let err = engine
        .convert(&req(Format::Txt, Format::Pdf, b"hi"))
        .unwrap_err();
    assert!(matches!(err, ConvertError::UnsupportedConversion { .. }));
}
