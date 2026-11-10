// Round-trip / property integration tests for the PDF engine.
//
// Exercises the public API (`kaladoc::crates::pdf_engine::*`) only.
// All fixtures are tiny PDFs built in-test with `lopdf` — no fixture
// files, no external tools. Each test applies one operation (or a
// short chain) and asserts the result survives save/load through the
// engine's own public ops.

use lopdf::content::{Content, Operation};
use lopdf::{Document, Object, Stream, dictionary};

use kaladoc::crates::core::engine::EngineLimits;
use kaladoc::crates::pdf_engine::{
    DupPos, FitMode, InsertPos, MarkPosition, MetadataPatch, NamedDest, OutlineItemSpec, PageSize,
    PackageLayout, PdfaLevel, PdfPermissions, RotateDegrees, TextFieldSpec, WatermarkOptions,
    add_named_destinations, clear_form, clear_page_labels, compress, create_text_field, decrypt,
    delete_form_field, duplicate_pages, encrypt, extract_metadata, extract_text, fill_form,
    garbage_collect, get_page_labels, get_page_rotations, get_xmp, grayscale, insert_blank,
    inspect_javascript, interleave, invert_colors, list_form_fields, list_named_destinations,
    list_outline, merge, nup, package_documents, page_count, remove_embedded_files,
    remove_named_destination, remove_outline, remove_pages, rotate, sanitize, set_metadata,
    set_outline, set_page_labels, split, to_pdfa, validate, watermark,
};

// ---------------------------------------------------------------------------
// Fixture builders (lopdf, in-test)
// ---------------------------------------------------------------------------

fn limits() -> EngineLimits {
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
        "Author" => Object::string_literal("KalaDoc Roundtrip Tests"),
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
        "roundtrip test",
    )
}

/// Page count via the public op (the engine's own counter).
fn pages(pdf: &[u8]) -> u32 {
    page_count(pdf, &limits()).unwrap()
}

/// Concatenated extracted text of every page.
fn all_text(pdf: &[u8]) -> String {
    extract_text(pdf, None, &limits())
        .unwrap()
        .iter()
        .map(|p| p.text.clone())
        .collect::<Vec<_>>()
        .join("\n")
}

/// A one-page PDF whose catalog carries a `/OpenAction` JavaScript
/// action — the active-content vector `sanitize` must strip.
fn pdf_with_open_action() -> Vec<u8> {
    let base = pdf_text(&["sanitize target"]);
    let mut doc = Document::load_mem(&base).unwrap();
    let action = dictionary! {
        "S" => "JavaScript",
        "JS" => Object::string_literal("app.alert('open')"),
    };
    let catalog = doc.catalog_mut().unwrap();
    catalog.set("OpenAction", action);
    let mut buf = Vec::new();
    doc.save_to(&mut buf).unwrap();
    buf
}

// ===========================================================================
// 1. merge → split → merge: page-count conservation
// ===========================================================================

#[test]
fn rt_merge_split_merge_conserves_pages() {
    let a = pdf_text(&["a1", "a2"]);
    let b = pdf_text(&["b1", "b2", "b3"]);
    let merged = merge(&[&a, &b], &limits()).unwrap();
    assert_eq!(pages(&merged), 5);

    let chunks = split(&merged, 2, &limits()).unwrap();
    assert_eq!(chunks.len(), 3, "5 pages / 2 per file → 3 chunks");
    assert_eq!(pages(&chunks[0]), 2);
    assert_eq!(pages(&chunks[1]), 2);
    assert_eq!(pages(&chunks[2]), 1);

    let remerged = merge(&[&chunks[0], &chunks[1], &chunks[2]], &limits()).unwrap();
    assert_eq!(pages(&remerged), 5);
    assert!(validate(&remerged, &limits()).unwrap().ok);
}

// ===========================================================================
// 2. rotate 90 → 180 → (back to 0): text preserved
// ===========================================================================

#[test]
fn rt_rotate_roundtrip_preserves_text() {
    let pdf = pdf_text(&["rotation survives"]);
    let before = all_text(&pdf);
    assert!(before.contains("rotation survives"));

    // Rotations accumulate mod 360: 90 + 180 + 90 = 360 ≡ 0.
    let r90 = rotate(&pdf, "1", RotateDegrees::Degrees90, &limits()).unwrap();
    assert_eq!(get_page_rotations(&r90, &limits()).unwrap()[0].1, 90);
    assert_eq!(all_text(&r90), before);

    let r270 = rotate(&r90, "1", RotateDegrees::Degrees180, &limits()).unwrap();
    assert_eq!(get_page_rotations(&r270, &limits()).unwrap()[0].1, 270);
    assert_eq!(all_text(&r270), before);

    let r0 = rotate(&r270, "1", RotateDegrees::Degrees90, &limits()).unwrap();
    assert_eq!(get_page_rotations(&r0, &limits()).unwrap()[0].1, 0);
    assert_eq!(all_text(&r0), before);
    assert!(validate(&r0, &limits()).unwrap().ok);
}

// ===========================================================================
// 3. encrypt → decrypt roundtrip with password
// ===========================================================================

#[test]
fn rt_encrypt_decrypt_roundtrip() {
    let pdf = pdf_text(&["secret page", "secret page 2"]);
    let encrypted = encrypt(
        &pdf,
        "user-pass",
        Some("owner-pass"),
        PdfPermissions::default(),
        &limits(),
    )
    .unwrap();

    // Decrypting with the wrong password must fail.
    assert!(decrypt(&encrypted, "wrong-pass", &limits()).is_err());

    let decrypted = decrypt(&encrypted, "user-pass", &limits()).unwrap();
    assert_eq!(pages(&decrypted), 2);
    let text = all_text(&decrypted);
    assert!(text.contains("secret page"));
    assert!(text.contains("secret page 2"));
}

// ===========================================================================
// 4. watermark → original text still extractable
// ===========================================================================

#[test]
fn rt_watermark_preserves_original_text() {
    let pdf = pdf_text(&["keep me readable"]);
    let opts = WatermarkOptions {
        text: "DRAFT".to_string(),
        position: MarkPosition::Center,
        ..Default::default()
    };
    let out = watermark(&pdf, &opts, &limits()).unwrap();
    assert_eq!(pages(&out), 1);
    assert!(validate(&out, &limits()).unwrap().ok);

    let text = all_text(&out);
    assert!(text.contains("keep me readable"));
}

// ===========================================================================
// 5. set_metadata → fields survive save/load
// ===========================================================================

#[test]
fn rt_set_metadata_fields_survive_reload() {
    let pdf = pdf_text(&["meta"]);
    let patch = MetadataPatch {
        title: Some("Roundtrip Title".to_string()),
        author: Some("Roundtrip Author".to_string()),
        subject: Some("roundtrip subject".to_string()),
        keywords: Some(vec!["alpha".to_string(), "beta".to_string()]),
        producer: Some("pdf-roundtrip-test".to_string()),
        ..Default::default()
    };
    let out = set_metadata(&pdf, &patch, &limits()).unwrap();

    // extract_metadata loads the bytes fresh — this is the save/load
    // roundtrip through the public API.
    let meta = extract_metadata(&out, &limits()).unwrap();
    assert_eq!(meta.title.as_deref(), Some("Roundtrip Title"));
    assert_eq!(meta.author.as_deref(), Some("Roundtrip Author"));
    assert_eq!(meta.subject.as_deref(), Some("roundtrip subject"));
    assert_eq!(meta.keywords.as_deref(), Some("alpha, beta"));
    assert_eq!(meta.producer.as_deref(), Some("pdf-roundtrip-test"));
}

// ===========================================================================
// 6. page_labels set → get → clear
// ===========================================================================

#[test]
fn rt_page_labels_set_get_clear() {
    let pdf = pdf_text(&["p1", "p2", "p3"]);
    let labeled = set_page_labels(
        &pdf,
        &[
            (1, "cover".to_string()),
            (2, "body".to_string()),
            (3, "body".to_string()),
        ],
        &limits(),
    )
    .unwrap();

    let labels = get_page_labels(&labeled, &limits()).unwrap();
    assert_eq!(labels.len(), 3);
    assert_eq!(labels[0].as_deref(), Some("cover"));
    assert_eq!(labels[1].as_deref(), Some("body"));
    assert_eq!(labels[2].as_deref(), Some("body"));

    // Labels also surface through extract_metadata after reload.
    let meta = extract_metadata(&labeled, &limits()).unwrap();
    assert_eq!(meta.page_labels[0].as_deref(), Some("cover"));

    let cleared = clear_page_labels(&labeled, &limits()).unwrap();
    let labels = get_page_labels(&cleared, &limits()).unwrap();
    assert!(labels.iter().all(|l| l.is_none()));
}

// ===========================================================================
// 7. form create → fill → clear → list empty
// ===========================================================================

#[test]
fn rt_form_create_fill_clear_list() {
    let pdf = pdf_text(&["form page"]);
    let spec = TextFieldSpec {
        name: "rt.field".to_string(),
        page: 1,
        rect: [100.0, 600.0, 300.0, 620.0],
        value: None,
        read_only: false,
    };
    let with_field = create_text_field(&pdf, &spec, &limits()).unwrap();

    let fields = list_form_fields(&with_field, &limits()).unwrap();
    assert_eq!(fields.len(), 1);
    assert_eq!(fields[0].name, "rt.field");
    assert_eq!(fields[0].type_name, "Tx");

    let filled = fill_form(
        &with_field,
        &[("rt.field".to_string(), "filled value".to_string())],
        &limits(),
    )
    .unwrap();
    let fields = list_form_fields(&filled, &limits()).unwrap();
    assert_eq!(fields[0].value.as_deref(), Some("filled value"));

    let cleared = clear_form(&filled, &limits()).unwrap();
    let fields = list_form_fields(&cleared, &limits()).unwrap();
    assert!(fields[0].value.is_none() || fields[0].value.as_deref() == Some(""));

    // Removing the field leaves an empty field list.
    let deleted = delete_form_field(&cleared, "rt.field", &limits()).unwrap();
    assert!(list_form_fields(&deleted, &limits()).unwrap().is_empty());
}

// ===========================================================================
// 8. outline set → list → remove
// ===========================================================================

#[test]
fn rt_outline_set_list_remove() {
    let pdf = pdf_text(&["one", "two", "three"]);
    let items = vec![
        OutlineItemSpec {
            title: "Chapter A".to_string(),
            page: 1,
            level: 0,
            bold: false,
            italic: false,
        },
        OutlineItemSpec {
            title: "Section A.1".to_string(),
            page: 2,
            level: 1,
            bold: true,
            italic: false,
        },
    ];
    let out = set_outline(&pdf, &items, &limits()).unwrap();

    let entries = list_outline(&out, &limits()).unwrap();
    assert_eq!(entries.len(), 2);
    assert_eq!(entries[0].title, "Chapter A");
    assert_eq!(entries[0].page, Some(1));
    assert_eq!(entries[1].title, "Section A.1");
    assert_eq!(entries[1].level, 1);

    let removed = remove_outline(&out, &limits()).unwrap();
    assert!(list_outline(&removed, &limits()).unwrap().is_empty());
    assert!(validate(&removed, &limits()).unwrap().ok);
}

// ===========================================================================
// 9. attachments add → extract → remove
// ===========================================================================

#[test]
fn rt_attachments_add_extract_remove() {
    let a = pdf_text(&["package part one"]);
    let b = pdf_text(&["package part two"]);

    // package_documents embeds each input verbatim under
    // /Names → /EmbeddedFiles as doc-N.pdf.
    let packaged = package_documents(&[&a, &b], "RT Package", PackageLayout::List, &limits()).unwrap();
    // cover page + 1 + 1 input pages = 3
    assert_eq!(pages(&packaged), 3);

    let meta = extract_metadata(&packaged, &limits()).unwrap();
    assert!(
        meta.embedded_files.iter().any(|n| n == "doc-1.pdf"),
        "embedded_files: {:?}",
        meta.embedded_files
    );
    assert!(
        meta.embedded_files.iter().any(|n| n == "doc-2.pdf"),
        "embedded_files: {:?}",
        meta.embedded_files
    );

    let stripped = remove_embedded_files(&packaged, &limits()).unwrap();
    let meta = extract_metadata(&stripped, &limits()).unwrap();
    assert!(meta.embedded_files.is_empty());
}

// ===========================================================================
// 10. named_dest add → list → remove
// ===========================================================================

#[test]
fn rt_named_dest_add_list_remove() {
    let pdf = pdf_text(&["dest one", "dest two"]);

    let added = add_named_destinations(
        &pdf,
        &[NamedDest {
            name: "rt-chapter".to_string(),
            page: 2,
            fit: FitMode::Fit,
        }],
        &limits(),
    )
    .unwrap();

    let dests = list_named_destinations(&added, &limits()).unwrap();
    assert_eq!(dests.len(), 1);
    assert_eq!(dests[0].name, "rt-chapter");
    assert_eq!(dests[0].page, Some(2));

    let removed = remove_named_destination(&added, "rt-chapter", &limits()).unwrap();
    assert!(list_named_destinations(&removed, &limits()).unwrap().is_empty());
}

// ===========================================================================
// 11. grayscale / invert on a text PDF → still a valid reloadable PDF
// ===========================================================================

#[test]
fn rt_grayscale_and_invert_still_valid() {
    let content = Content {
        operations: vec![
            Operation::new("rg", vec![0.8.into(), 0.2.into(), 0.2.into()]),
            Operation::new("BT", vec![]),
            Operation::new("Tf", vec!["F1".into(), 12.into()]),
            Operation::new("Td", vec![100.into(), 700.into()]),
            Operation::new("Tj", vec![Object::string_literal("color text")]),
            Operation::new("ET", vec![]),
        ],
    };
    let pdf = build_pdf(&[Some(content.encode().unwrap())], "color rt");

    let (gray, _) = grayscale(&pdf, None, &limits()).unwrap();
    assert!(validate(&gray, &limits()).unwrap().ok);
    assert_eq!(pages(&gray), 1);
    assert!(all_text(&gray).contains("color text"));

    let (inverted, _) = invert_colors(&pdf, None, &limits()).unwrap();
    assert!(validate(&inverted, &limits()).unwrap().ok);
    assert_eq!(pages(&inverted), 1);
    assert!(all_text(&inverted).contains("color text"));
}

// ===========================================================================
// 12. compress → validate ok
// ===========================================================================

#[test]
fn rt_compress_produces_valid_pdf() {
    let pdf = pdf_text(&["compress me", "me too", "and me"]);
    let out = compress(&pdf, &limits()).unwrap();
    assert_eq!(pages(&out), 3);
    let report = validate(&out, &limits()).unwrap();
    assert!(report.ok, "validation issues: {:?}", report.issues);
    assert!(all_text(&out).contains("compress me"));
}

// ===========================================================================
// 13. remove_pages → page_count matches
// ===========================================================================

#[test]
fn rt_remove_pages_updates_count() {
    let pdf = pdf_text(&["keep1", "drop1", "keep2", "drop2", "keep3"]);
    let out = remove_pages(&pdf, "2,4", &limits()).unwrap();
    assert_eq!(pages(&out), 3);

    let texts = extract_text(&out, None, &limits()).unwrap();
    let joined: Vec<&str> = texts.iter().map(|p| p.text.as_str()).collect();
    assert_eq!(joined.len(), 3);
    assert!(joined[0].contains("keep1"));
    assert!(joined[1].contains("keep2"));
    assert!(joined[2].contains("keep3"));
}

// ===========================================================================
// 14. interleave 2 docs → page order
// ===========================================================================

#[test]
fn rt_interleave_preserves_round_robin_order() {
    let a = pdf_text(&["a-one", "a-two"]);
    let b = pdf_text(&["b-one"]);
    let out = interleave(&[&a, &b], &limits()).unwrap();
    assert_eq!(pages(&out), 3);

    let texts = extract_text(&out, None, &limits()).unwrap();
    assert_eq!(texts.len(), 3);
    assert!(texts[0].text.contains("a-one"));
    assert!(texts[1].text.contains("b-one"));
    assert!(texts[2].text.contains("a-two"));
}

// ===========================================================================
// 15. duplicate_pages → count doubles the selected pages
// ===========================================================================

#[test]
fn rt_duplicate_pages_doubles_selection() {
    let pdf = pdf_text(&["d1", "d2", "d3"]);
    // One extra copy of every page → each selected page appears twice.
    let (out, positions) = duplicate_pages(&pdf, "1-3", 1, DupPos::End, &limits()).unwrap();
    assert_eq!(pages(&out), 6);
    assert_eq!(positions.len(), 3);

    let texts = extract_text(&out, None, &limits()).unwrap();
    let joined: String = texts.iter().map(|p| p.text.as_str()).collect::<Vec<_>>().join("|");
    for needle in ["d1", "d2", "d3"] {
        assert_eq!(joined.matches(needle).count(), 2, "{needle} in {joined}");
    }
}

// ===========================================================================
// 16. insert_blank positions
// ===========================================================================

#[test]
fn rt_insert_blank_positions() {
    let pdf = pdf_text(&["one", "two"]);

    let (out, positions) = insert_blank(&pdf, InsertPos::End, 2, Some(PageSize::Letter), &limits())
        .unwrap();
    assert_eq!(pages(&out), 4);
    assert_eq!(positions, vec![3, 4]);

    let (out2, pos2) = insert_blank(&pdf, InsertPos::Before(1), 1, None, &limits()).unwrap();
    assert_eq!(pages(&out2), 3);
    assert_eq!(pos2, vec![1]);

    // Original text pages are still there and in order.
    let texts = extract_text(&out2, None, &limits()).unwrap();
    assert!(texts[0].text.is_empty() || !texts[0].text.contains("one"));
    assert!(texts[1].text.contains("one"));
    assert!(texts[2].text.contains("two"));
}

// ===========================================================================
// 17. nup 4-up → page count = ceil(n / 4)
// ===========================================================================

#[test]
fn rt_nup_4up_page_count() {
    let pdf = pdf_text(&["n1", "n2", "n3", "n4", "n5", "n6", "n7"]);
    let out = nup(&pdf, 2, 2, None, &limits()).unwrap();
    // ceil(7 / 4) = 2
    assert_eq!(pages(&out), 2);
    assert!(validate(&out, &limits()).unwrap().ok);
}

// ===========================================================================
// 18. garbage_collect → reload ok
// ===========================================================================

#[test]
fn rt_garbage_collect_reload_ok() {
    let pdf = pdf_text(&["gc me", "gc me too"]);
    let report = garbage_collect(&pdf, &limits()).unwrap();
    assert!(!report.bytes.is_empty());

    assert_eq!(pages(&report.bytes), 2);
    assert!(validate(&report.bytes, &limits()).unwrap().ok);
    assert!(all_text(&report.bytes).contains("gc me"));
}

// ===========================================================================
// 19. to_pdfa → XMP contains pdfaid
// ===========================================================================

#[test]
fn rt_to_pdfa_xmp_declares_pdfaid() {
    let pdf = pdf_text(&["pdfa candidate"]);
    let report = to_pdfa(&pdf, PdfaLevel::PdfA2b, &limits()).unwrap();

    let xmp = get_xmp(&report.bytes, &limits()).unwrap();
    let xmp_text = String::from_utf8_lossy(&xmp);
    assert!(
        xmp_text.contains("pdfaid"),
        "XMP packet missing pdfaid declaration: {xmp_text}"
    );
    assert_eq!(pages(&report.bytes), 1);
}

// ===========================================================================
// 20. sanitize → no OpenAction
// ===========================================================================

#[test]
fn rt_sanitize_removes_open_action() {
    let pdf = pdf_with_open_action();

    // Precondition: the fixture really does carry an OpenAction JS hook.
    let before = inspect_javascript(&pdf, &limits()).unwrap();
    assert!(
        before.iter().any(|j| j.location.contains("OpenAction")),
        "fixture missing OpenAction: {before:?}"
    );

    let out = sanitize(&pdf, &limits()).unwrap();
    assert!(validate(&out, &limits()).unwrap().ok);

    let after = inspect_javascript(&out, &limits()).unwrap();
    assert!(
        !after.iter().any(|j| j.location.contains("OpenAction")),
        "OpenAction survived sanitize: {after:?}"
    );
    assert!(all_text(&out).contains("sanitize target"));
}
