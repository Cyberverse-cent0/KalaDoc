// Performance smoke tests for the PDF engine.
//
// The project's #1 objective is advanced-PDF-editing performance, so
// this suite exists to (a) give every transforming op a generous
// wall-clock budget on a realistic-ish ~50-page document, and (b)
// produce baseline timings (`cargo test --test pdf_perf -- --nocapture`)
// that future optimizations can be measured against.
//
// Design notes:
//
// - The fixture is built in-test with `lopdf`: 50 A4 pages sharing one
//   font and one `/Resources` dictionary, each page carrying ~15 text
//   lines plus stroked/filled vector marks and color operators —
//   representative of a text-heavy report PDF.
// - Every test builds (or lazily reuses via [`OnceLock`]) the fixture
//   **once**, times only the engine call with [`Instant`], asserts a
//   generous `< 5 s` budget (CI machines vary wildly; this catches
//   pathological blowups, not micro-regressions), sanity-checks the
//   result through the public API, and records the real elapsed time
//   via `eprintln!`.
// - [`scaling_report`] times the same ops at 10 / 50 / 100 pages and
//   prints a table. It has **no hard assert** — it is a report — but
//   it warns via `eprintln!` when an op's 100-page time is more than
//   10x its 10-page time, which is the signature of an O(n²) (or
//   worse) regression.

use std::sync::OnceLock;
use std::time::{Duration, Instant};

use lopdf::content::{Content, Operation};
use lopdf::{Document, Object, Stream, dictionary};

use kaladoc::crates::core::engine::EngineLimits;
use kaladoc::crates::pdf_engine::{
    ColorReport, GcReport, HeaderFooterSpec, MarkPosition, MetadataPatch, PageNumberStyle,
    WatermarkOptions, compress, extract_pages, extract_text, garbage_collect, grayscale,
    headers_footers, inspect, invert_colors, merge, nup, page_count, remove_pages, set_metadata,
    validate, watermark,
};

/// Generous per-op budget. These are smoke tests: the point is to catch
/// orders-of-magnitude regressions, not to gate on machine speed.
const PERF_BUDGET: Duration = Duration::from_secs(5);

fn limits() -> EngineLimits {
    EngineLimits::default()
}

// ---------------------------------------------------------------------------
// Fixture: synthetic N-page PDF, shared font/resources, text + vector marks
// ---------------------------------------------------------------------------

/// Build a PDF with `n` pages. Every page shares the same `/Font F1`
/// (Helvetica) and `/Resources` dictionary; each page's content stream
/// holds a title line, ~12 body text lines, and a handful of stroked /
/// filled rectangles and lines with explicit RGB color operators so the
/// color-rewrite ops (`grayscale`, `invert_colors`) have real work.
fn build_pdf(n: usize) -> Vec<u8> {
    let mut doc = Document::with_version("1.5");

    let pages_id = doc.new_object_id();
    let font_id = doc.add_object(dictionary! {
        "Type" => "Font",
        "Subtype" => "Type1",
        "BaseFont" => "Helvetica",
        "Encoding" => "WinAnsiEncoding",
    });
    let resources_id = doc.add_object(dictionary! {
        "Font" => dictionary! {
            "F1" => font_id,
        },
    });

    let mut kids = Vec::with_capacity(n);
    for i in 0..n {
        let page_no = i + 1;
        let mut ops: Vec<Operation> = Vec::with_capacity(80);

        // Title line.
        ops.push(Operation::new("BT", vec![]));
        ops.push(Operation::new("Tf", vec!["F1".into(), Object::Real(20.0)]));
        ops.push(Operation::new(
            "Td",
            vec![Object::Real(72.0), Object::Real(770.0)],
        ));
        ops.push(Operation::new(
            "Tj",
            vec![Object::string_literal(format!(
                "KalaDoc Performance Report — Page {page_no} of {n}"
            ))],
        ));
        ops.push(Operation::new("ET", vec![]));

        // Body lines: text interleaved with color operators and a few
        // stroked rules so color ops and text extraction both see work.
        for j in 0..12 {
            let y = 720.0 - j as f32 * 18.0;
            let shade = 0.15 + (j as f32) * 0.05;
            ops.push(Operation::new(
                "rg",
                vec![Object::Real(shade), Object::Real(0.2), Object::Real(0.3)],
            ));
            ops.push(Operation::new("BT", vec![]));
            ops.push(Operation::new("Tf", vec!["F1".into(), Object::Real(11.0)]));
            ops.push(Operation::new(
                "Td",
                vec![Object::Real(72.0), Object::Real(y)],
            ));
            ops.push(Operation::new(
                "Tj",
                vec![Object::string_literal(format!(
                    "Line {j} of page {page_no}: the quick brown fox jumps over the lazy dog."
                ))],
            ));
            ops.push(Operation::new("ET", vec![]));
        }

        // Vector marks: a stroked rectangle border and a diagonal rule.
        ops.push(Operation::new(
            "RG",
            vec![Object::Real(0.1), Object::Real(0.1), Object::Real(0.1)],
        ));
        ops.push(Operation::new("w", vec![Object::Real(0.5)]));
        ops.push(Operation::new(
            "re",
            vec![
                Object::Real(72.0),
                Object::Real(80.0),
                Object::Real(451.0),
                Object::Real(660.0),
            ],
        ));
        ops.push(Operation::new("S", vec![]));
        ops.push(Operation::new(
            "m",
            vec![Object::Real(72.0), Object::Real(80.0)],
        ));
        ops.push(Operation::new(
            "l",
            vec![Object::Real(523.0), Object::Real(740.0)],
        ));
        ops.push(Operation::new("S", vec![]));

        let content = Content { operations: ops };
        let content_id = doc.add_object(Stream::new(dictionary! {}, content.encode().unwrap()));
        let page_id = doc.add_object(dictionary! {
            "Type" => "Page",
            "Parent" => pages_id,
            "Contents" => content_id,
            "Resources" => resources_id,
            "MediaBox" => vec![0.into(), 0.into(), 595.into(), 842.into()],
        });
        kids.push(Object::Reference(page_id));
    }

    let pages = dictionary! {
        "Type" => "Pages",
        "Kids" => kids,
        "Count" => n as i64,
        "Resources" => resources_id,
        "MediaBox" => vec![0.into(), 0.into(), 595.into(), 842.into()],
    };
    doc.objects.insert(pages_id, Object::Dictionary(pages));

    let catalog_id = doc.add_object(dictionary! {
        "Type" => "Catalog",
        "Pages" => pages_id,
    });
    let info_id = doc.add_object(dictionary! {
        "Title" => Object::string_literal("KalaDoc performance fixture"),
        "Author" => Object::string_literal("pdf_perf.rs"),
        "Creator" => Object::string_literal("kaladoc-pdf-perf-test"),
        "Producer" => Object::string_literal("lopdf"),
    });
    doc.trailer.set("Root", catalog_id);
    doc.trailer.set("Info", info_id);

    let mut buf = Vec::new();
    doc.save_to(&mut buf).unwrap();
    buf
}

/// Lazily-built 50-page fixture shared by the per-op smoke tests.
fn pdf50() -> &'static Vec<u8> {
    static PDF: OnceLock<Vec<u8>> = OnceLock::new();
    PDF.get_or_init(|| build_pdf(50))
}

// ---------------------------------------------------------------------------
// Timing helpers
// ---------------------------------------------------------------------------

/// Run `f`, print the elapsed milliseconds, and assert it stayed under
/// [`PERF_BUDGET`]. Returns `f`'s result for sanity assertions.
fn time_op<F, T>(name: &str, f: F) -> T
where
    F: FnOnce() -> T,
{
    let start = Instant::now();
    let out = f();
    let elapsed = start.elapsed();
    eprintln!(
        "[pdf_perf] {name:<44} {:>10.1} ms",
        elapsed.as_secs_f64() * 1000.0
    );
    assert!(
        elapsed < PERF_BUDGET,
        "{name} took {elapsed:?}, exceeding the {PERF_BUDGET:?} smoke budget"
    );
    out
}

// ---------------------------------------------------------------------------
// Per-op smoke tests (~50 pages each)
// ---------------------------------------------------------------------------

#[test]
fn perf_merge_50p() {
    // Five 10-page inputs → one 50-page output.
    let parts: Vec<Vec<u8>> = (0..5).map(|_| build_pdf(10)).collect();
    let inputs: Vec<&[u8]> = parts.iter().map(Vec::as_slice).collect();

    let out = time_op("merge 5x10p -> 50p", || {
        merge(&inputs, &limits()).expect("merge failed")
    });
    assert_eq!(page_count(&out, &limits()).unwrap(), 50);
}

#[test]
fn perf_extract_text_50p() {
    let pdf = pdf50();
    let pages = time_op("extract_text 50p", || {
        extract_text(pdf, None, &limits()).expect("extract_text failed")
    });
    assert_eq!(pages.len(), 50);
    assert!(pages.iter().all(|p| p.text.contains("quick brown fox")));
}

#[test]
fn perf_watermark_50p() {
    let pdf = pdf50();
    let opts = WatermarkOptions {
        text: "DRAFT".to_string(),
        position: MarkPosition::Center,
        ..WatermarkOptions::default()
    };
    let out = time_op("watermark 50p", || {
        watermark(pdf, &opts, &limits()).expect("watermark failed")
    });
    assert_eq!(page_count(&out, &limits()).unwrap(), 50);
    assert!(validate(&out, &limits()).unwrap().ok);
}

#[test]
fn perf_grayscale_invert_50p() {
    let pdf = pdf50();
    let (_gray, gray_report): (Vec<u8>, ColorReport) = time_op("grayscale 50p", || {
        grayscale(pdf, None, &limits()).expect("grayscale failed")
    });
    let (out, inv_report) = time_op("invert_colors 50p", || {
        invert_colors(&_gray, None, &limits()).expect("invert_colors failed")
    });
    eprintln!("[pdf_perf]   grayscale report: {gray_report:?}; invert report: {inv_report:?}");
    assert_eq!(page_count(&out, &limits()).unwrap(), 50);
}

#[test]
fn perf_compress_50p() {
    let pdf = pdf50();
    let out = time_op("compress 50p", || {
        compress(pdf, &limits()).expect("compress failed")
    });
    assert_eq!(page_count(&out, &limits()).unwrap(), 50);
}

#[test]
fn perf_extract_pages_50p() {
    let pdf = pdf50();
    // 10 + 6 + 1 = 17 pages.
    let out = time_op("extract_pages subset (17p of 50p)", || {
        extract_pages(pdf, "1-10,20-25,50", &limits()).expect("extract_pages failed")
    });
    assert_eq!(page_count(&out, &limits()).unwrap(), 17);
}

#[test]
fn perf_remove_pages_50p() {
    let pdf = pdf50();
    // Remove 24 + 11 = 35 pages → 15 remain.
    let out = time_op("remove_pages 50p (drop 35)", || {
        remove_pages(pdf, "2-25,40-50", &limits()).expect("remove_pages failed")
    });
    assert_eq!(page_count(&out, &limits()).unwrap(), 15);
}

#[test]
fn perf_set_metadata_50p() {
    let pdf = pdf50();
    let patch = MetadataPatch {
        title: Some("Perf-timed metadata".to_string()),
        author: Some("pdf_perf.rs".to_string()),
        subject: Some("performance baseline".to_string()),
        keywords: Some(vec!["perf".to_string(), "baseline".to_string()]),
        ..MetadataPatch::default()
    };
    let out = time_op("set_metadata 50p", || {
        set_metadata(pdf, &patch, &limits()).expect("set_metadata failed")
    });
    let info = inspect(&out, &limits()).unwrap();
    assert_eq!(info.title.as_deref(), Some("Perf-timed metadata"));
    assert_eq!(info.page_count, 50);
}

#[test]
fn perf_inspect_validate_50p() {
    let pdf = pdf50();
    let info = time_op("inspect 50p", || {
        inspect(pdf, &limits()).expect("inspect failed")
    });
    assert_eq!(info.page_count, 50);
    assert!(!info.encrypted);

    let report = time_op("validate 50p", || {
        validate(pdf, &limits()).expect("validate failed")
    });
    assert!(report.ok, "validate reported issues: {:?}", report.issues);
}

#[test]
fn perf_nup_50p() {
    let pdf = pdf50();
    // 2x2 → ceil(50/4) = 13 output sheets.
    let out = time_op("nup 2x2 50p -> 13 sheets", || {
        nup(pdf, 2, 2, None, &limits()).expect("nup failed")
    });
    assert_eq!(page_count(&out, &limits()).unwrap(), 13);
}

#[test]
fn perf_headers_footers_50p() {
    let pdf = pdf50();
    let spec = HeaderFooterSpec {
        header: Some("KalaDoc Perf — page {page} of {total}".to_string()),
        footer: Some("Confidential".to_string()),
        page_numbers: PageNumberStyle::BottomCenter,
        font_size: Some(9.0),
        margin: Some(36.0),
    };
    let out = time_op("headers_footers 50p", || {
        headers_footers(pdf, &spec, None, &limits()).expect("headers_footers failed")
    });
    assert_eq!(page_count(&out, &limits()).unwrap(), 50);
}

#[test]
fn perf_garbage_collect_50p() {
    let pdf = pdf50();
    let report: GcReport = time_op("garbage_collect 50p", || {
        garbage_collect(pdf, &limits()).expect("garbage_collect failed")
    });
    assert!(report.objects_before > 0);
    assert!(!report.bytes.is_empty());
    eprintln!(
        "[pdf_perf]   gc: {} -> {} objects, {} -> {} bytes",
        report.objects_before, report.objects_after, report.bytes_before, report.bytes_after
    );
}

// ---------------------------------------------------------------------------
// Scaling report: 10 / 50 / 100 pages, table output, no hard assert.
// ---------------------------------------------------------------------------

/// Like [`time_op`] but never asserts — used by the report-only scaling
/// test where large inputs are expected to be slow. Returns elapsed ms.
fn time_ms<F, T>(name: &str, f: F) -> f64
where
    F: FnOnce() -> T,
{
    let start = Instant::now();
    let _ = f();
    let ms = start.elapsed().as_secs_f64() * 1000.0;
    eprintln!("[pdf_perf] {name:<44} {ms:>10.1} ms");
    ms
}

/// Ops exercised at each size. Each closure's result is discarded;
/// sanity checks are skipped here (the per-op tests cover them) to keep
/// the report focused on timing. Returns `(op name, elapsed ms)` rows.
fn run_all_ops(pdf: &[u8], tag: &str) -> Vec<(String, f64)> {
    let l = limits();
    let mut rows: Vec<(String, f64)> = Vec::new();
    let mut record = |name: &str, ms: f64| rows.push((name.to_string(), ms));

    // merge: two copies of the current-size doc.
    let other = pdf.to_vec();
    record(
        "merge",
        time_ms(&format!("merge 2x{tag}"), || {
            merge(&[pdf, other.as_slice()], &l).expect("merge failed")
        }),
    );

    record(
        "extract_text",
        time_ms(&format!("extract_text {tag}"), || {
            extract_text(pdf, None, &l).expect("extract_text failed")
        }),
    );

    let wm = WatermarkOptions {
        text: "SCALE".to_string(),
        ..WatermarkOptions::default()
    };
    record(
        "watermark",
        time_ms(&format!("watermark {tag}"), || {
            watermark(pdf, &wm, &l).expect("watermark failed")
        }),
    );

    record(
        "grayscale+invert",
        time_ms(&format!("grayscale+invert {tag}"), || {
            let (g, _): (Vec<u8>, ColorReport) =
                grayscale(pdf, None, &l).expect("grayscale failed");
            invert_colors(&g, None, &l).expect("invert_colors failed")
        }),
    );

    record(
        "compress",
        time_ms(&format!("compress {tag}"), || {
            compress(pdf, &l).expect("compress failed")
        }),
    );

    record(
        "extract_pages",
        time_ms(&format!("extract_pages {tag}"), || {
            extract_pages(pdf, "1-5,10", &l).expect("extract_pages failed")
        }),
    );

    record(
        "remove_pages",
        time_ms(&format!("remove_pages {tag}"), || {
            remove_pages(pdf, "2-4", &l).expect("remove_pages failed")
        }),
    );

    let patch = MetadataPatch {
        title: Some(format!("scaling {tag}")),
        ..MetadataPatch::default()
    };
    record(
        "set_metadata",
        time_ms(&format!("set_metadata {tag}"), || {
            set_metadata(pdf, &patch, &l).expect("set_metadata failed")
        }),
    );

    record(
        "inspect+validate",
        time_ms(&format!("inspect+validate {tag}"), || {
            inspect(pdf, &l).expect("inspect failed");
            validate(pdf, &l).expect("validate failed")
        }),
    );

    record(
        "nup",
        time_ms(&format!("nup {tag}"), || {
            nup(pdf, 2, 2, None, &l).expect("nup failed")
        }),
    );

    let spec = HeaderFooterSpec {
        header: Some("Perf {page}/{total}".to_string()),
        footer: None,
        page_numbers: PageNumberStyle::BottomCenter,
        font_size: None,
        margin: None,
    };
    record(
        "headers_footers",
        time_ms(&format!("headers_footers {tag}"), || {
            headers_footers(pdf, &spec, None, &l).expect("headers_footers failed")
        }),
    );

    record(
        "garbage_collect",
        time_ms(&format!("garbage_collect {tag}"), || {
            garbage_collect(pdf, &l).expect("garbage_collect failed")
        }),
    );

    rows
}

#[test]
fn scaling_report() {
    let sizes = [10usize, 50, 100];

    // (op name -> [ms @ 10p, ms @ 50p, ms @ 100p])
    let mut table: Vec<(String, [f64; 3])> = Vec::new();

    for (idx, &n) in sizes.iter().enumerate() {
        eprintln!("[pdf_perf] === building {n}-page fixture ===");
        let build_start = Instant::now();
        let pdf = build_pdf(n);
        eprintln!(
            "[pdf_perf]   build: {:.1} ms, {} bytes",
            build_start.elapsed().as_secs_f64() * 1000.0,
            pdf.len()
        );

        for (name, ms) in run_all_ops(&pdf, &format!("{n}p")) {
            if let Some(row) = table.iter_mut().find(|(k, _)| *k == name) {
                row.1[idx] = ms;
            } else {
                let mut arr = [f64::NAN; 3];
                arr[idx] = ms;
                table.push((name, arr));
            }
        }
    }

    // Print the table.
    eprintln!("[pdf_perf] ===== scaling report (ms) =====");
    eprintln!(
        "[pdf_perf] {:<22} {:>12} {:>12} {:>12} {:>10}",
        "op", "10 pages", "50 pages", "100 pages", "100/10x"
    );
    for (name, [t10, t50, t100]) in &table {
        let ratio = if *t10 > 0.0 { t100 / t10 } else { f64::NAN };
        eprintln!(
            "[pdf_perf] {:<22} {:>12.1} {:>12.1} {:>12.1} {:>9.1}x",
            name, t10, t50, t100, ratio
        );
        // Superlinearity smoke signal: >10x cost for 10x pages hints at
        // quadratic (or worse) behaviour. Warn only — never fail.
        if ratio.is_finite() && ratio > 10.0 {
            eprintln!(
                "[pdf_perf] WARNING: {name} scaled {ratio:.1}x from 10p to 100p \
                 (>10x) — possible O(n²) regression; investigate before merging perf changes"
            );
        }
    }
    eprintln!("[pdf_perf] ===== end scaling report =====");
}
