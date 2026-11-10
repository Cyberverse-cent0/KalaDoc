// Advanced-operations integration tests for the PDF engine.
//
// Exercises the extended `ops` surface through the **public API**
// (`kaladoc::crates::pdf_engine::*`) only. All fixtures are tiny PDFs
// built in-test with `lopdf` — no fixture files, no external tools.
//
// Operations that require external binaries (`make_searchable` needs
// `pdftoppm` + `tesseract`) are exercised "gracefully": the test
// accepts either a successful report or a `MissingTool` error, which
// is the engine's honest contract.
//
// Coverage gaps (no public op exists for these, so they cannot be
// tested here): named-destination create/read/update (only bulk
// removal via `remove_named_objects` is covered), page transitions,
// optional-content (OCG) inspection, and a dedicated font-inspection
// report.

use lopdf::content::{Content, Operation};
use lopdf::{Document, Object, Stream, dictionary};
use sha2::{Digest, Sha256};

use kaladoc::crates::core::engine::EngineLimits;

// ---------------------------------------------------------------------------
// Fixture builders (lopdf, in-test)
// ---------------------------------------------------------------------------

fn unlimited() -> EngineLimits {
    EngineLimits::default()
}

/// Serialize a `BT /Tf /Td /Tj /ET` text object for `text`.
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

/// Build a PDF with one page per entry; `Some(bytes)` sets `/Contents`,
/// `None` leaves the page without a content stream (a blank page).
fn build_pdf(streams: &[Option<Vec<u8>>], title: &str) -> Vec<u8> {
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

    let mut kids = Vec::with_capacity(streams.len());
    for stream in streams {
        let mut page = dictionary! {
            "Type" => "Page",
            "Parent" => pages_id,
            "Resources" => resources_id,
            "MediaBox" => vec![0.into(), 0.into(), 595.into(), 842.into()],
        };
        if let Some(bytes) = stream {
            let content_id = doc.add_object(Stream::new(dictionary! {}, bytes.clone()));
            page.set("Contents", Object::Reference(content_id));
        }
        let page_id = doc.add_object(page);
        kids.push(Object::Reference(page_id));
    }

    let pages = dictionary! {
        "Type" => "Pages",
        "Kids" => kids,
        "Count" => streams.len() as i64,
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
        "Author" => Object::string_literal("KalaDoc Tests"),
    });
    doc.trailer.set("Root", catalog_id);
    doc.trailer.set("Info", info_id);

    let mut buf = Vec::new();
    doc.save_to(&mut buf).unwrap();
    buf
}

/// One text page per entry of `texts`.
fn pdf_text(texts: &[&str]) -> Vec<u8> {
    build_pdf(
        &texts.iter().map(|t| Some(text_stream(t))).collect::<Vec<_>>(),
        "advanced-ops test",
    )
}

/// A single-page PDF whose content sets an RGB nonstroking color then
/// draws text — exercises the color-rewrite ops.
fn pdf_color() -> Vec<u8> {
    let content = Content {
        operations: vec![
            Operation::new("rg", vec![0.8.into(), 0.2.into(), 0.2.into()]),
            Operation::new("BT", vec![]),
            Operation::new("Tf", vec!["F1".into(), 12.into()]),
            Operation::new("Td", vec![100.into(), 700.into()]),
            Operation::new("Tj", vec![Object::string_literal("color page")]),
            Operation::new("ET", vec![]),
        ],
    };
    build_pdf(&[Some(content.encode().unwrap())], "color test")
}

/// Page count via `lopdf` (the ops' own counter is covered elsewhere).
fn page_count(pdf: &[u8]) -> usize {
    Document::load_mem(pdf).unwrap().get_pages().len()
}

/// Add a catalog `/Names` → `/EmbeddedFiles` entry carrying
/// `notes.txt` to a fresh one-page PDF.
fn pdf_with_embedded_file() -> Vec<u8> {
    let base = pdf_text(&["has attachment"]);
    let mut doc = Document::load_mem(&base).unwrap();

    let ef_stream_id = doc.add_object(Stream::new(dictionary! {}, b"attachment bytes".to_vec()));
    let filespec_id = doc.add_object(dictionary! {
        "Type" => "Filespec",
        "F" => Object::string_literal("notes.txt"),
        "UF" => Object::string_literal("notes.txt"),
        "EF" => dictionary! { "F" => ef_stream_id },
    });
    let names = dictionary! {
        "Names" => vec![
            Object::string_literal("notes.txt"),
            Object::Reference(filespec_id),
        ],
    };
    let names_dict = dictionary! { "EmbeddedFiles" => names };

    let catalog = doc.catalog_mut().unwrap();
    catalog.set("Names", names_dict);

    let mut buf = Vec::new();
    doc.save_to(&mut buf).unwrap();
    buf
}

/// Add a catalog `/Names` → `/Dests` name tree with one destination.
fn pdf_with_named_dest() -> Vec<u8> {
    let base = pdf_text(&["dest target"]);
    let mut doc = Document::load_mem(&base).unwrap();

    let page_id = *doc.get_pages().values().next().unwrap();
    let dest = Object::Array(vec![
        Object::Reference(page_id),
        Object::Name(b"XYZ".to_vec()),
        Object::Integer(0),
        Object::Integer(0),
        Object::Null,
    ]);
    let dests = dictionary! {
        "Names" => vec![Object::string_literal("chapter1"), dest],
    };
    let names_dict = dictionary! { "Dests" => dests };

    let catalog = doc.catalog_mut().unwrap();
    catalog.set("Names", names_dict);

    let mut buf = Vec::new();
    doc.save_to(&mut buf).unwrap();
    buf
}

// ---------------------------------------------------------------------------
// Imports (kept separate so helpers above stay readable)
// ---------------------------------------------------------------------------

use kaladoc::crates::pdf_engine::{
    AnnotFilter, BookletSpec, ColorReport, DrawSpec, EditOutcome, HeaderFooterSpec, InkSpec,
    BookletMode, BookletSize, DupPos, InsertPos, MarkupKind, MergeMode, MetadataPatch,
    OrientMode, OutlineItemSpec, PageNumberStyle, PageSize, PageSizeSpec, PdfaLevel, PdfuaLevel,
    PdfxLevel, Shape, ShapeStyle, SizeUnit, SplitMode, Stroke, TimestampAlgorithm,
    TextEdit, TextFieldSpec, add_ink, add_page_numbers, booklet,
    clear_form, clear_page_labels, create_portfolio, create_text_field, delete_form_field,
    detect_orientation, document_digest, draw_shapes, duplicate_pages, edit_text, extract_metadata,
    extract_text, fill_form, get_page_labels, get_page_rotations, get_page_sizes, get_xmp,
    grayscale, headers_footers, insert_blank, inspect, inspect_structure, interleave, invert_colors,
    list_form_fields, list_ink, list_outline, list_portfolio, lock_fields, make_searchable,
    merge_mode, nup, normalize_orientation, poster, redact, remove_blank_pages, remove_embedded_files,
    remove_named_objects, remove_ink, remove_outline, scale_content, set_metadata, set_outline,
    set_page_labels, set_page_rotation, set_page_size, set_xmp, single_page, split_mode,
    text_markup, to_pdfa, to_pdfua, to_pdfx, unlock_fields, validate,
};

// ===========================================================================
// Drawing / annotation ops
// ===========================================================================

#[test]
fn adv_draw_shapes_adds_vector_content() {
    let pdf = pdf_text(&["base"]);
    let spec = DrawSpec {
        shapes: vec![
            Shape::Rectangle {
                x: 50.0,
                y: 50.0,
                width: 200.0,
                height: 100.0,
                style: ShapeStyle {
                    stroke: [255, 0, 0],
                    fill: Some([0, 0, 255]),
                    line_width: 2.0,
                    opacity: None,
                },
            },
            Shape::Line {
                x1: 0.0,
                y1: 0.0,
                x2: 595.0,
                y2: 842.0,
                style: ShapeStyle::default(),
            },
        ],
        pages: None,
    };
    let out = draw_shapes(&pdf, &spec, &unlimited()).unwrap();
    assert_eq!(page_count(&out), 1);
    assert!(validate(&out, &unlimited()).unwrap().ok);
}

#[test]
fn adv_draw_shapes_rejects_empty_spec() {
    let pdf = pdf_text(&["x"]);
    let spec = DrawSpec::default(); // no shapes
    assert!(draw_shapes(&pdf, &spec, &unlimited()).is_err());
}

#[test]
fn adv_ink_roundtrip_list_remove() {
    let pdf = pdf_text(&["ink page", "ink page 2"]);
    let spec = InkSpec {
        strokes: vec![Stroke {
            points: vec![[100.0, 100.0], [200.0, 200.0], [300.0, 150.0]],
        }],
        pages: None,
        color: [10, 20, 30],
        width: 2.5,
    };
    let with_ink = add_ink(&pdf, &spec, &unlimited()).unwrap();
    assert_eq!(page_count(&with_ink), 2);

    let ink = list_ink(&with_ink, &unlimited()).unwrap();
    assert_eq!(ink.len(), 2, "one ink annot per page: {ink:?}");
    assert_eq!(ink[0].stroke_count, 1);

    let removed = remove_ink(
        &with_ink,
        &AnnotFilter {
            pages: None,
            subtypes: None,
            all: true,
        },
        &unlimited(),
    )
    .unwrap();
    assert!(list_ink(&removed, &unlimited()).unwrap().is_empty());
}

// ===========================================================================
// Text ops
// ===========================================================================

#[test]
fn adv_edit_text_replaces_and_reports() {
    let pdf = pdf_text(&["hello world", "nothing here"]);
    let edit = TextEdit {
        find: "hello".to_string(),
        replace: "goodbye".to_string(),
        pages: None,
        case_sensitive: true,
    };
    let outcome: EditOutcome = edit_text(&pdf, &edit, &unlimited()).unwrap();
    assert_eq!(outcome.report.replacements, 1);
    assert_eq!(outcome.report.pages_affected, vec![1]);

    let pages = extract_text(&outcome.bytes, None, &unlimited()).unwrap();
    assert!(pages[0].text.contains("goodbye"));
    assert!(!pages[0].text.contains("hello"));
}

#[test]
fn adv_edit_text_case_insensitive() {
    let pdf = pdf_text(&["HELLO"]);
    let edit = TextEdit {
        find: "hello".to_string(),
        replace: "hi".to_string(),
        pages: None,
        case_sensitive: false,
    };
    let outcome = edit_text(&pdf, &edit, &unlimited()).unwrap();
    assert!(outcome.report.replacements >= 1);
}

#[test]
fn adv_text_markup_highlight() {
    let pdf = pdf_text(&["mark me"]);
    let out = text_markup(&pdf, MarkupKind::Highlight, None, None, &unlimited()).unwrap();
    assert_eq!(page_count(&out), 1);
    assert!(validate(&out, &unlimited()).unwrap().ok);

    let out2 = text_markup(
        &pdf,
        MarkupKind::Strike,
        Some("1"),
        Some([255, 0, 0]),
        &unlimited(),
    )
    .unwrap();
    assert_eq!(page_count(&out2), 1);
}

#[test]
fn adv_redact_removes_matching_text_ops() {
    let pdf = pdf_text(&["top secret page", "clean page"]);
    let out = redact(&pdf, &["secret".to_string()], None, &unlimited()).unwrap();
    assert_eq!(page_count(&out), 2);
    let pages = extract_text(&out, None, &unlimited()).unwrap();
    assert!(!pages[0].text.contains("secret"));
    assert!(pages[1].text.contains("clean"));
}

// ===========================================================================
// Headers / footers / page numbers
// ===========================================================================

#[test]
fn adv_headers_footers_and_page_numbers() {
    let pdf = pdf_text(&["p1", "p2", "p3"]);
    let spec = HeaderFooterSpec {
        header: Some("Confidential {page}/{total}".to_string()),
        footer: Some("footer text".to_string()),
        page_numbers: PageNumberStyle::BottomCenter,
        font_size: Some(9.0),
        margin: Some(36.0),
    };
    let out = headers_footers(&pdf, &spec, None, &unlimited()).unwrap();
    assert_eq!(page_count(&out), 3);

    let numbered = add_page_numbers(&pdf, PageNumberStyle::BottomRight, &unlimited()).unwrap();
    assert_eq!(page_count(&numbered), 3);

    // Invalid font size is rejected before any work.
    let bad = HeaderFooterSpec {
        font_size: Some(-1.0),
        ..spec.clone()
    };
    assert!(headers_footers(&pdf, &bad, None, &unlimited()).is_err());
}

// ===========================================================================
// Metadata / XMP
// ===========================================================================

#[test]
fn adv_set_metadata_roundtrip() {
    let pdf = pdf_text(&["meta"]);
    let patch = MetadataPatch {
        title: Some("Advanced Ops Title".to_string()),
        author: Some("Test Author".to_string()),
        subject: Some("subject".to_string()),
        keywords: Some(vec!["k1".to_string(), "k2".to_string()]),
        producer: Some("advanced-ops-test".to_string()),
        ..Default::default()
    };
    let out = set_metadata(&pdf, &patch, &unlimited()).unwrap();

    let info = inspect(&out, &unlimited()).unwrap();
    assert_eq!(info.title.as_deref(), Some("Advanced Ops Title"));
    assert_eq!(info.author.as_deref(), Some("Test Author"));

    let meta = extract_metadata(&out, &unlimited()).unwrap();
    assert_eq!(meta.title.as_deref(), Some("Advanced Ops Title"));
    assert_eq!(meta.keywords.as_deref(), Some("k1, k2"));
}

#[test]
fn adv_xmp_set_get_roundtrip() {
    let pdf = pdf_text(&["xmp"]);
    let packet = br#"<?xpacket begin=""?><x:xmpmeta xmlns:x="adobe:ns:meta/"><rdf:RDF xmlns:rdf="http://www.w3.org/1999/02/22-rdf-syntax-ns#"/></x:xmpmeta><?xpacket end="w"?>"#;
    let out = set_xmp(&pdf, packet, &unlimited()).unwrap();
    let got = get_xmp(&out, &unlimited()).unwrap();
    assert!(String::from_utf8_lossy(&got).contains("x:xmpmeta"));
}

// ===========================================================================
// Outline
// ===========================================================================

#[test]
fn adv_outline_set_list_remove() {
    let pdf = pdf_text(&["one", "two", "three"]);
    let items = vec![
        OutlineItemSpec {
            title: "Chapter 1".to_string(),
            page: 1,
            level: 0,
            bold: true,
            italic: false,
        },
        OutlineItemSpec {
            title: "Section 1.1".to_string(),
            page: 2,
            level: 1,
            bold: false,
            italic: true,
        },
    ];
    let out = set_outline(&pdf, &items, &unlimited()).unwrap();

    let entries = list_outline(&out, &unlimited()).unwrap();
    assert_eq!(entries.len(), 2);
    assert_eq!(entries[0].title, "Chapter 1");
    assert_eq!(entries[0].page, Some(1));
    assert_eq!(entries[1].level, 1);

    let cleared = remove_outline(&out, &unlimited()).unwrap();
    assert!(list_outline(&cleared, &unlimited()).unwrap().is_empty());
}

// ===========================================================================
// Forms
// ===========================================================================

#[test]
fn adv_form_create_list_fill_clear() {
    let pdf = pdf_text(&["form page"]);
    let spec = TextFieldSpec {
        name: "applicant.name".to_string(),
        page: 1,
        rect: [100.0, 600.0, 300.0, 620.0],
        value: None,
        read_only: false,
    };
    let with_field = create_text_field(&pdf, &spec, &unlimited()).unwrap();

    let fields = list_form_fields(&with_field, &unlimited()).unwrap();
    assert_eq!(fields.len(), 1);
    assert_eq!(fields[0].name, "applicant.name");
    assert_eq!(fields[0].type_name, "Tx");

    let filled = fill_form(
        &with_field,
        &[("applicant.name".to_string(), "Jane Doe".to_string())],
        &unlimited(),
    )
    .unwrap();
    let fields = list_form_fields(&filled, &unlimited()).unwrap();
    assert_eq!(fields[0].value.as_deref(), Some("Jane Doe"));

    let cleared = clear_form(&filled, &unlimited()).unwrap();
    let fields = list_form_fields(&cleared, &unlimited()).unwrap();
    assert!(fields[0].value.is_none() || fields[0].value.as_deref() == Some(""));

    // delete_form_field removes the field entirely.
    let deleted = delete_form_field(&with_field, "applicant.name", &unlimited()).unwrap();
    assert!(list_form_fields(&deleted, &unlimited()).unwrap().is_empty());
}

#[test]
fn adv_lock_unlock_fields() {
    let pdf = pdf_text(&["locked form"]);
    let spec = TextFieldSpec {
        name: "f1".to_string(),
        page: 1,
        rect: [50.0, 500.0, 250.0, 520.0],
        value: None,
        read_only: false,
    };
    let with_field = create_text_field(&pdf, &spec, &unlimited()).unwrap();
    let names = vec!["f1".to_string()];

    let (locked, report) = lock_fields(&with_field, Some(&names), &unlimited()).unwrap();
    assert!(report.changed >= 1, "report: {report:?}");
    assert!(list_form_fields(&locked, &unlimited()).unwrap()[0].read_only);

    let (unlocked, report) = unlock_fields(&locked, Some(&names), &unlimited()).unwrap();
    assert!(report.changed >= 1, "report: {report:?}");
    assert!(!list_form_fields(&unlocked, &unlimited()).unwrap()[0].read_only);

    // Unknown names are reported, not fatal.
    let missing = vec!["nope".to_string()];
    let (_, report) = unlock_fields(&unlocked, Some(&missing), &unlimited()).unwrap();
    assert_eq!(report.unmatched, vec!["nope".to_string()]);
}

// ===========================================================================
// Page labels
// ===========================================================================

#[test]
fn adv_page_labels_set_get_clear() {
    let pdf = pdf_text(&["a", "b", "c"]);
    let labeled = set_page_labels(
        &pdf,
        &[(1, "cover".to_string()), (2, "body".to_string()), (3, "body".to_string())],
        &unlimited(),
    )
    .unwrap();

    let labels = get_page_labels(&labeled, &unlimited()).unwrap();
    assert_eq!(labels.len(), 3);
    assert_eq!(labels[0].as_deref(), Some("cover"));
    assert_eq!(labels[1].as_deref(), Some("body"));
    assert_eq!(labels[2].as_deref(), Some("body"));

    let cleared = clear_page_labels(&labeled, &unlimited()).unwrap();
    let labels = get_page_labels(&cleared, &unlimited()).unwrap();
    assert!(labels.iter().all(|l| l.is_none()));
}

// ===========================================================================
// Page manipulation
// ===========================================================================

#[test]
fn adv_insert_blank_pages() {
    let pdf = pdf_text(&["one", "two"]);
    let (out, positions) = insert_blank(&pdf, InsertPos::End, 2, Some(PageSize::Letter), &unlimited())
        .unwrap();
    assert_eq!(page_count(&out), 4);
    assert_eq!(positions, vec![3, 4]);

    let (out2, pos2) = insert_blank(&pdf, InsertPos::Before(1), 1, None, &unlimited()).unwrap();
    assert_eq!(page_count(&out2), 3);
    assert_eq!(pos2, vec![1]);
}

#[test]
fn adv_duplicate_pages() {
    let pdf = pdf_text(&["one", "two"]);
    let (out, positions) = duplicate_pages(&pdf, "1", 2, DupPos::End, &unlimited()).unwrap();
    assert_eq!(page_count(&out), 4);
    assert_eq!(positions.len(), 2);

    let texts = extract_text(&out, None, &unlimited()).unwrap();
    assert_eq!(texts.len(), 4);
    assert!(texts[2].text.contains("one"));
    assert!(texts[3].text.contains("one"));
}

#[test]
fn adv_remove_blank_pages() {
    // Middle page has no /Contents → provably blank.
    let pdf = build_pdf(
        &[
            Some(text_stream("kept one")),
            None,
            Some(text_stream("kept two")),
        ],
        "blank test",
    );
    let report = remove_blank_pages(&pdf, &unlimited()).unwrap();
    assert_eq!(report.removed, vec![2]);
    assert_eq!(report.kept, vec![1, 3]);
    assert_eq!(report.total_before, 3);
    assert_eq!(report.total_after, 2);
    assert_eq!(page_count(&report.pdf), 2);
}

#[test]
fn adv_interleave_documents() {
    let a = pdf_text(&["a1", "a2"]);
    let b = pdf_text(&["b1"]);
    let out = interleave(&[&a, &b], &unlimited()).unwrap();
    assert_eq!(page_count(&out), 3);
    let pages = extract_text(&out, None, &unlimited()).unwrap();
    assert!(pages[0].text.contains("a1"));
    assert!(pages[1].text.contains("b1"));
    assert!(pages[2].text.contains("a2"));

    assert!(interleave(&[&a], &unlimited()).is_err());
}

#[test]
fn adv_nup_poster_single_page_booklet_counts() {
    let four = pdf_text(&["p1", "p2", "p3", "p4"]);
    // 2x1 n-up: 4 source pages → 2 sheets.
    let out = nup(&four, 2, 1, None, &unlimited()).unwrap();
    assert_eq!(page_count(&out), 2);

    // Poster: one page split 2x2 → 4 tile pages.
    let one = pdf_text(&["solo"]);
    let out = poster(&one, None, 2, 2, &unlimited()).unwrap();
    assert_eq!(page_count(&out), 4);

    // Single page: 3 pages on one giant sheet → 1 page.
    let three = pdf_text(&["p1", "p2", "p3"]);
    let out = single_page(&three, None, None, &unlimited()).unwrap();
    assert_eq!(page_count(&out), 1);

    // Booklet: 5 pages → ceil(5/4) chunks × 2 sides = 4 pages.
    let five = pdf_text(&["p1", "p2", "p3", "p4", "p5"]);
    let spec = BookletSpec {
        sheet_size: BookletSize::Letter,
        imposition: BookletMode::LeftRight,
    };
    let out = booklet(&five, spec, &unlimited()).unwrap();
    assert_eq!(page_count(&out), 4);
}

#[test]
fn adv_scale_content() {
    let pdf = pdf_text(&["scale me"]);
    let out = scale_content(&pdf, "1", 0.5, None, &unlimited()).unwrap();
    assert_eq!(page_count(&out), 1);
    // Invalid scale rejected.
    assert!(scale_content(&pdf, "1", 0.0, None, &unlimited()).is_err());
    assert!(scale_content(&pdf, "1", f64::NAN, None, &unlimited()).is_err());
}

#[test]
fn adv_page_size_and_rotation_set_get() {
    let pdf = pdf_text(&["box"]);

    // Page box: set 300x400 pt MediaBox on page 1.
    let spec = PageSizeSpec {
        width: 300.0,
        height: 400.0,
        unit: SizeUnit::Pt,
        scale_content: false,
    };
    let resized = set_page_size(&pdf, "1", &spec, &unlimited()).unwrap();
    let sizes = get_page_sizes(&resized, &unlimited()).unwrap();
    assert_eq!(sizes.len(), 1);
    assert!((sizes[0].width_pt - 300.0).abs() < 0.01);
    assert!((sizes[0].height_pt - 400.0).abs() < 0.01);

    // Rotation: /Rotate is a separate, view-level op.
    let rotated = set_page_rotation(&pdf, "1", 90, &unlimited()).unwrap();
    let rots = get_page_rotations(&rotated, &unlimited()).unwrap();
    assert_eq!(rots, vec![(1, 90)]);

    // Non-multiple-of-90 rejected.
    assert!(set_page_rotation(&pdf, "1", 45, &unlimited()).is_err());
}

// ===========================================================================
// Color reports
// ===========================================================================

#[test]
fn adv_grayscale_and_invert_reports() {
    let pdf = pdf_color();

    let (out, report): (Vec<u8>, ColorReport) = grayscale(&pdf, None, &unlimited()).unwrap();
    assert_eq!(report.pages_processed, 1);
    assert!(report.ops_rewritten >= 1, "report: {report:?}");
    assert_eq!(page_count(&out), 1);

    let (out, report) = invert_colors(&pdf, None, &unlimited()).unwrap();
    assert_eq!(report.pages_processed, 1);
    assert_eq!(page_count(&out), 1);
}

// ===========================================================================
// Merge / split modes
// ===========================================================================

#[test]
fn adv_merge_modes() {
    let a = pdf_text(&["a1", "a2"]);
    let b = pdf_text(&["b1", "b2"]);

    let seq = merge_mode(&[&a, &b], MergeMode::Sequential, &unlimited()).unwrap();
    let pages = extract_text(&seq, None, &unlimited()).unwrap();
    let order: Vec<String> = pages.iter().map(|p| p.text.trim().to_string()).collect();
    assert_eq!(order, vec!["a1", "a2", "b1", "b2"]);

    let inter = merge_mode(&[&a, &b], MergeMode::Interleave, &unlimited()).unwrap();
    let pages = extract_text(&inter, None, &unlimited()).unwrap();
    let order: Vec<String> = pages.iter().map(|p| p.text.trim().to_string()).collect();
    assert_eq!(order, vec!["a1", "b1", "a2", "b2"]);

    let rev = merge_mode(&[&a, &b], MergeMode::Reverse, &unlimited()).unwrap();
    let pages = extract_text(&rev, None, &unlimited()).unwrap();
    let order: Vec<String> = pages.iter().map(|p| p.text.trim().to_string()).collect();
    assert_eq!(order, vec!["b1", "b2", "a1", "a2"]);
}

#[test]
fn adv_split_modes() {
    let pdf = pdf_text(&["p1", "p2", "p3", "p4", "p5"]);

    let parts = split_mode(
        &pdf,
        SplitMode::ByCount { pages_per_file: 2 },
        &unlimited(),
    )
    .unwrap();
    assert_eq!(parts.len(), 3);
    assert_eq!(page_count(&parts[0]), 2);
    assert_eq!(page_count(&parts[2]), 1);

    let parts = split_mode(&pdf, SplitMode::ByRange { spec: "1-2,4" }, &unlimited()).unwrap();
    assert_eq!(parts.len(), 2);
    assert_eq!(page_count(&parts[0]), 2);
    assert_eq!(page_count(&parts[1]), 1);

    // Stride 2 over 4 pages → 2 outputs of 2 pages each (page 5 unused
    // for stride assignment? every nth page k+1, k+1+n, ...).
    let four = pdf_text(&["p1", "p2", "p3", "p4"]);
    let parts = split_mode(&four, SplitMode::EveryNth { n: 2 }, &unlimited()).unwrap();
    assert_eq!(parts.len(), 2);
    assert!(parts.iter().all(|p| page_count(p) == 2));
}

// ===========================================================================
// Structure / standards / portfolio
// ===========================================================================

#[test]
fn adv_structure_tree_untagged() {
    let pdf = pdf_text(&["untagged"]);
    let report = inspect_structure(&pdf, &unlimited()).unwrap();
    assert!(!report.tagged);
    assert!(report.elements.is_empty());
}

#[test]
fn adv_pdfua_pdfx_pdfa_declarations() {
    let pdf = pdf_text(&["standards"]);

    let r = to_pdfua(&pdf, PdfuaLevel::PdfUa1, &unlimited()).unwrap();
    assert_eq!(r.declared_part, "1");
    assert_eq!(page_count(&r.bytes), 1);

    let r = to_pdfx(&pdf, PdfxLevel::Pdfx1a, &unlimited()).unwrap();
    assert_eq!(r.declared_part, "1");
    assert_eq!(page_count(&r.bytes), 1);

    let r = to_pdfa(&pdf, PdfaLevel::PdfA2b, &unlimited()).unwrap();
    assert_eq!(r.declared_part, "2");
    assert_eq!(page_count(&r.bytes), 1);
}

#[test]
fn adv_portfolio_create_list() {
    let a = pdf_text(&["doc A"]);
    let b = pdf_text(&["doc B"]);
    let names = vec!["Alpha".to_string(), "Beta".to_string()];
    let portfolio = create_portfolio(&[&a, &b], &names, &unlimited()).unwrap();

    let entries = list_portfolio(&portfolio, &unlimited()).unwrap();
    assert_eq!(entries.len(), 2);
    assert_eq!(entries[0].name, "Alpha");
    assert_eq!(entries[1].name, "Beta");
    assert!(entries.iter().all(|e| e.page >= 1));
}

// ===========================================================================
// Attachments / named destinations (partial public surface)
// ===========================================================================

#[test]
fn adv_embedded_files_list_and_remove() {
    let pdf = pdf_with_embedded_file();
    let meta = extract_metadata(&pdf, &unlimited()).unwrap();
    assert!(meta.embedded_files.contains(&"notes.txt".to_string()));

    let stripped = remove_embedded_files(&pdf, &unlimited()).unwrap();
    let meta = extract_metadata(&stripped, &unlimited()).unwrap();
    assert!(!meta.embedded_files.contains(&"notes.txt".to_string()));
}

#[test]
fn adv_named_dests_bulk_removal() {
    let pdf = pdf_with_named_dest();
    assert_eq!(page_count(&pdf), 1);
    let report = remove_named_objects(&pdf, &["Dests".to_string()], &unlimited()).unwrap();
    assert!(report.removed_categories.iter().any(|c| c == "Dests"));
}

// ===========================================================================
// Digest / timestamps / searchable / orientation
// ===========================================================================

#[test]
fn adv_document_digest_sha256_known_answer() {
    let pdf = pdf_text(&["digest me"]);
    let report = document_digest(&pdf, TimestampAlgorithm::Sha256, &unlimited()).unwrap();
    assert_eq!(report.algorithm, "sha256");
    assert_eq!(report.digest_len, 32);
    assert_eq!(report.digest_hex.len(), 64);
    assert_eq!(report.input_size_bytes, pdf.len() as u64);

    // Independent known-answer: SHA-256 of the raw input bytes.
    let expected = hex::encode(Sha256::digest(&pdf));
    assert_eq!(report.digest_hex, expected);

    let r512 = document_digest(&pdf, TimestampAlgorithm::Sha512, &unlimited()).unwrap();
    assert_eq!(r512.digest_len, 64);
    assert_eq!(r512.digest_hex.len(), 128);
}

#[test]
fn adv_make_searchable_graceful_without_tools() {
    let pdf = pdf_text(&["scan me"]);
    match make_searchable(&pdf, None, &unlimited()) {
        Ok((_bytes, _report)) => {
            // Tools present: pipeline ran and produced PDF bytes.
            assert!(_bytes.starts_with(b"%PDF"));
        }
        Err(e) => {
            // Tools absent: honest MissingTool, never fake output.
            assert!(
                matches!(e, kaladoc::crates::pdf_engine::PdfError::MissingTool(_)),
                "unexpected error: {e}"
            );
        }
    }
}

#[test]
fn adv_auto_orient_detect_and_normalize() {
    let pdf = pdf_text(&["portrait"]);
    let orientations = detect_orientation(&pdf, &unlimited()).unwrap();
    assert_eq!(orientations.len(), 1);
    assert_eq!(orientations[0].page, 1);
    // Our fixture is 595x842 pt → portrait.
    assert!(orientations[0].media_h > orientations[0].media_w);

    // AsIs returns the input untouched.
    let out = normalize_orientation(&pdf, OrientMode::AsIs, &unlimited()).unwrap();
    assert_eq!(out, pdf);

    let out = normalize_orientation(&pdf, OrientMode::Portrait, &unlimited()).unwrap();
    assert_eq!(page_count(&out), 1);
}
