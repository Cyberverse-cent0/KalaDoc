//! Remove invisible ("hidden") text from page content streams.
//!
//! PDF text can be shown with render mode `Tr 3` — the text state
//! machinery runs (position advances, extraction sees the glyphs) but
//! nothing is painted. This is exactly how OCR text layers are built
//! (`make_searchable` emits `3 Tr` + `Tj` blocks), and it is also how
//! some producers hide extractable-but-invisible text. This op walks
//! every selected page's `/Contents` streams, tracks the `Tr` text
//! state, and **deletes** every text-showing operator
//! (`Tj`, `TJ`, `'`, `"`) executed while `Tr == 3`.
//!
//! # What gets removed
//!
//! - Each hidden text-showing op is dropped and counted in
//!   [`HiddenTextReport::text_ops_removed`].
//! - When a `BT`…`ET` text object contains *only* hidden shows plus
//!   text-state operators (`Tf`, `Td`/`TD`/`Tm`/`T*`, `Tc`, `Tw`,
//!   `Tz`, `TL`, `Ts`, `Tr`), the whole object — `BT`, `ET` and the
//!   state ops — is dropped, since none of it can affect visible
//!   output. Those non-show ops are counted in
//!   [`HiddenTextReport::other_ops_removed`]. This is the common OCR
//!   layer shape (`BT /F1 12 Tf 3 Tr … Tj ET`) and removes it cleanly.
//! - A text object that mixes hidden shows with other surviving ops
//!   keeps its `BT`/`ET` and surviving ops; only the hidden shows are
//!   removed, a warning is recorded, and remaining text positions may
//!   shift (a removed `Tj` no longer advances the text matrix, and a
//!   removed `'`/`"` also loses its implicit line move and, for `"`,
//!   its word/char spacing side effect).
//! - Show ops found *outside* any `BT`/`ET` pair (malformed but seen
//!   in the wild) are removed too and reported via a warning.
//!
//! # Intent — read this
//!
//! This op deliberately destroys OCR / invisible text layers: that is
//! its purpose (anti-scraping, layer stripping). It is **not** a
//! redaction tool for visible content — visible text is untouched.
//! Use `redact`/`sanitize` for that.
//!
//! # Honest limitations
//!
//! - Only the page's own `/Contents` streams are rewritten. Hidden
//!   text inside Form XObjects (`Do`), Type3 glyph descriptions,
//!   tiling patterns, annotation appearance streams and optional
//!   content layers is **not** visited.
//! - `Tr` state is tracked per content stream and persists across
//!   `BT`/`ET` boundaries, matching how viewers and `text_markup`
//!   interpret it; a stream that sets `3 Tr` once and never resets it
//!   has *all* its subsequent shows removed — including ones a producer
//!   may have intended as visible. `Tr` values other than `3` are
//!   treated as visible per the PDF spec's render-mode table.
//! - Streams are decoded with `lopdf`'s **strict** content parser. A
//!   stream that fails strict decode is left completely unchanged and
//!   reported in `warnings` — partially-parsed output is never
//!   re-encoded, so no undecodable tail is silently dropped (the
//!   lenient-decoder truncation caveat that `grayscale` documents does
//!   not apply here).
//! - Rewritten streams are stored **uncompressed** (`/Filter` and
//!   `/DecodeParms` dropped), matching `grayscale`/`redact`; output
//!   may be larger than the input.
//! - Encrypted documents are rejected ([`PdfError::Encrypted`](crate::crates::pdf_engine::error::PdfError::Encrypted)).

use lopdf::content::{Content, Operation};
use lopdf::{Document, ObjectId};
use serde::{Deserialize, Serialize};

use crate::crates::core::engine::EngineLimits;

use crate::crates::pdf_engine::error::PdfResult;

use super::load::{load_pdf, reject_encrypted, save_pdf};
use super::watermark::selected_pages;

/// Decompression bound for a single content stream — lopdf's own
/// default, matching the other stream-rewriting ops.
use lopdf::DEFAULT_MAX_DECOMPRESSED_SIZE as MAX_CONTENT_STREAM;

/// Text-showing operators removed while `Tr == 3`.
const SHOW_OPS: &[&str] = &["Tj", "TJ", "'", "\""];

/// Text-object operators with no visible side effect of their own.
/// When a `BT`…`ET` block contains only these plus hidden shows, the
/// whole block is dead weight and is dropped entirely.
fn is_text_state_op(operator: &str) -> bool {
    matches!(
        operator,
        "Tf" | "Td" | "TD" | "Tm" | "T*" | "Tc" | "Tw" | "Tz" | "TL" | "Ts" | "Tr"
    )
}

/// Outcome of [`remove_hidden_text`].
///
/// Counts let callers report honestly what the pass removed; `warnings`
/// names every page/stream that could not be fully processed.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct HiddenTextReport {
    /// Selected pages whose content streams were scanned (including
    /// pages where nothing was found or a stream had to be skipped).
    pub pages_scanned: usize,
    /// Text-showing operators (`Tj`/`TJ`/`'`/`"`) removed because they
    /// executed under `Tr == 3`.
    pub text_ops_removed: usize,
    /// Non-show operators removed as part of emptied `BT`…`ET` text
    /// objects (`BT`, `ET`, and dead text-state ops).
    pub other_ops_removed: usize,
    /// Human-readable notes: streams that failed to decode, text
    /// objects mixing hidden and visible content, malformed
    /// `BT`/`ET` nesting, and similar non-fatal conditions.
    pub warnings: Vec<String>,
}

/// Numeric content-stream operand as `f64`, or `None`.
fn numeric_operand(op: &Operation, index: usize) -> Option<f64> {
    op.operands
        .get(index)
        .and_then(|o| o.as_float().ok())
        .map(|v| v as f64)
        .filter(|v| v.is_finite())
}

/// Per-stream scan results before counters are merged into the report.
#[derive(Debug, Default)]
struct StreamEdit {
    /// Operations to keep (input order preserved).
    operations: Vec<Operation>,
    /// Whether anything was removed.
    changed: bool,
    text_ops_removed: usize,
    other_ops_removed: usize,
    /// `BT`…`ET` blocks that mixed hidden shows with surviving ops —
    /// removal may have shifted remaining text positions.
    mixed_blocks: usize,
    /// `BT` operators never closed by `ET` (or nested `BT`).
    unclosed_text_objects: usize,
    /// Hidden shows found outside any `BT`…`ET` pair.
    orphan_shows: usize,
}

/// Scan one decoded content stream and return the operations with all
/// `Tr == 3` text-showing ops (and fully-dead text objects) removed.
fn process_stream_ops(ops: &[Operation]) -> StreamEdit {
    let n = ops.len();
    let mut drop_idx = vec![false; n];

    // Pass 1: track `Tr` and mark hidden show ops. `Tr` is a text-state
    // parameter that persists across `BT`/`ET` within the stream, so a
    // single linear scan is correct (same model as `text_markup`).
    let mut tr: i64 = 0;
    let mut hidden_show = vec![false; n];
    for (i, op) in ops.iter().enumerate() {
        match op.operator.as_str() {
            "Tr" => {
                if let Some(v) = numeric_operand(op, 0) {
                    tr = v.round() as i64;
                }
            }
            op_name if SHOW_OPS.contains(&op_name) => {
                if tr == 3 {
                    hidden_show[i] = true;
                }
            }
            _ => {}
        }
    }

    // Pass 2: pair up BT/ET. A `BT` seen while another is open, or one
    // never closed, ends at the next `BT`/end-of-stream respectively —
    // malformed input is handled, not trusted.
    let mut block_of = vec![usize::MAX; n];
    let mut blocks: Vec<(usize, usize)> = Vec::new(); // (BT idx, end-exclusive)
    let mut bt_start: Option<usize> = None;
    let mut unclosed_text_objects = 0usize;
    for (i, op) in ops.iter().enumerate() {
        match op.operator.as_str() {
            "BT" => {
                if let Some(s) = bt_start.take() {
                    // Previous BT was never closed: its block ends here.
                    blocks.push((s, i));
                    unclosed_text_objects += 1;
                }
                bt_start = Some(i);
            }
            "ET" => {
                if let Some(s) = bt_start.take() {
                    blocks.push((s, i + 1));
                }
            }
            _ => {}
        }
    }
    if let Some(s) = bt_start.take() {
        blocks.push((s, n));
        unclosed_text_objects += 1;
    }
    for (bi, &(s, e)) in blocks.iter().enumerate() {
        for slot in block_of.iter_mut().take(e).skip(s) {
            *slot = bi;
        }
    }

    // Pass 3: per block, drop hidden shows; drop the whole block when
    // nothing but dead text-state ops remains.
    let mut text_ops_removed = 0usize;
    let mut other_ops_removed = 0usize;
    let mut mixed_blocks = 0usize;
    for &(s, e) in &blocks {
        // Interior excludes the BT and, when present, the closing ET.
        let has_et = e > s + 1 && ops[e - 1].operator.as_str() == "ET";
        let interior_end = if has_et { e - 1 } else { e };
        let shows: Vec<usize> = (s + 1..interior_end).filter(|&i| hidden_show[i]).collect();
        if shows.is_empty() {
            continue;
        }
        let all_dead = (s + 1..interior_end)
            .all(|i| hidden_show[i] || is_text_state_op(ops[i].operator.as_str()));
        if all_dead {
            for slot in drop_idx.iter_mut().take(e).skip(s) {
                *slot = true;
            }
            other_ops_removed += (e - s) - shows.len();
        } else {
            for &i in &shows {
                drop_idx[i] = true;
            }
            mixed_blocks += 1;
        }
        text_ops_removed += shows.len();
    }

    // Pass 4: hidden shows outside any text object (malformed streams).
    let mut orphan_shows = 0usize;
    for i in 0..n {
        if hidden_show[i] && block_of[i] == usize::MAX {
            drop_idx[i] = true;
            orphan_shows += 1;
            text_ops_removed += 1;
        }
    }

    let changed = drop_idx.iter().any(|&d| d);
    let operations = if changed {
        ops.iter()
            .enumerate()
            .filter(|(i, _)| !drop_idx[*i])
            .map(|(_, op)| op.clone())
            .collect()
    } else {
        Vec::new()
    };

    StreamEdit {
        operations,
        changed,
        text_ops_removed,
        other_ops_removed,
        mixed_blocks,
        unclosed_text_objects,
        orphan_shows,
    }
}

/// Decode a page content stream's bytes. Returns `None` when the
/// stream is missing, is not a stream, or its filter cannot be decoded
/// (raw compressed bytes must never be rewritten).
fn decode_stream_bytes(doc: &Document, stream_id: ObjectId) -> Option<Vec<u8>> {
    let object = doc.get_object(stream_id).ok()?;
    let stream = object.as_stream().ok()?;
    match stream.decompressed_content_with_limit(MAX_CONTENT_STREAM) {
        Ok(bytes) => Some(bytes),
        Err(_) => {
            if stream.dict.has(b"Filter") {
                // Still compressed — cannot safely rewrite.
                None
            } else {
                Some(stream.content.clone())
            }
        }
    }
}

/// Scan and rewrite one content stream in place. Failures are reported
/// as warnings; the stream is left untouched whenever anything about
/// it cannot be handled cleanly.
fn rewrite_stream(
    doc: &mut Document,
    stream_id: ObjectId,
    page_number: u32,
    report: &mut HiddenTextReport,
) {
    let Some(bytes) = decode_stream_bytes(doc, stream_id) else {
        report.warnings.push(format!(
            "page {page_number}: a content stream could not be decompressed; left unchanged"
        ));
        return;
    };

    // Strict decode: on failure the stream is skipped whole so a
    // partially-parsed tail is never silently dropped by re-encoding.
    // Whitespace/comment-only streams decode to zero ops and are a
    // no-op, not a warning.
    let content = match Content::decode_strict(&bytes) {
        Ok(content) => content,
        Err(_) => match Content::decode(&bytes) {
            Ok(empty) if empty.operations.is_empty() => return,
            _ => {
                report.warnings.push(format!(
                    "page {page_number}: a content stream failed to parse completely; left unchanged"
                ));
                return;
            }
        },
    };

    let edit = process_stream_ops(&content.operations);
    if !edit.changed {
        return;
    }

    let encoded = match (Content {
        operations: edit.operations,
    })
    .encode()
    {
        Ok(bytes) => bytes,
        Err(_) => {
            report.warnings.push(format!(
                "page {page_number}: a rewritten content stream failed to encode; left unchanged"
            ));
            return;
        }
    };

    let Ok(object) = doc.get_object_mut(stream_id) else {
        return;
    };
    let Ok(stream) = object.as_stream_mut() else {
        return;
    };
    stream.content = encoded;
    // Stored uncompressed — drop filter entries so readers do not try
    // to decode the raw operator bytes.
    stream.dict.remove(b"Filter");
    stream.dict.remove(b"DecodeParms");

    report.text_ops_removed += edit.text_ops_removed;
    report.other_ops_removed += edit.other_ops_removed;
    if edit.mixed_blocks > 0 {
        report.warnings.push(format!(
            "page {page_number}: {} text object(s) mixed hidden and visible text; \
             remaining text positions may have shifted",
            edit.mixed_blocks
        ));
    }
    if edit.unclosed_text_objects > 0 {
        report.warnings.push(format!(
            "page {page_number}: {} unclosed BT text object(s) encountered",
            edit.unclosed_text_objects
        ));
    }
    if edit.orphan_shows > 0 {
        report.warnings.push(format!(
            "page {page_number}: {} hidden text-showing op(s) found outside BT/ET",
            edit.orphan_shows
        ));
    }
}

/// Remove invisible (`Tr == 3`) text — including OCR text layers —
/// from the selected pages of `input`.
///
/// `pages` is an optional 1-based range spec (`"1-3,8"`, `"all"`);
/// `None` processes every page. Returns the rewritten PDF bytes and a
/// [`HiddenTextReport`]. Encrypted documents are rejected with
/// [`PdfError::Encrypted`](crate::crates::pdf_engine::error::PdfError::Encrypted).
pub fn remove_hidden_text(
    input: &[u8],
    pages: Option<&str>,
    limits: &EngineLimits,
) -> PdfResult<(Vec<u8>, HiddenTextReport)> {
    let mut doc = load_pdf(input, limits)?;
    reject_encrypted(&doc)?;

    let selected = selected_pages(&doc, pages)?;
    let pages_map = doc.get_pages();
    let mut report = HiddenTextReport::default();

    for page_number in selected {
        let Some(&page_id) = pages_map.get(&page_number) else {
            continue;
        };
        report.pages_scanned += 1;
        // Clone the id list: rewriting streams mutates `doc`.
        for stream_id in doc.get_page_contents(page_id) {
            rewrite_stream(&mut doc, stream_id, page_number, &mut report);
        }
    }

    let out = save_pdf(&mut doc)?;
    Ok((out, report))
}

#[cfg(test)]
mod tests {
    use lopdf::content::Content;
    use lopdf::{Document, Object, Stream, dictionary};

    use super::*;
    use crate::crates::pdf_engine::error::PdfError;
    use crate::crates::pdf_engine::ops::test_utils::pdf_with_pages;

    fn limits() -> EngineLimits {
        EngineLimits::default()
    }

    /// Build a one-page PDF whose content stream contains the given
    /// raw operators (same shape as `grayscale`'s test helper).
    fn pdf_with_ops(ops: &str) -> Vec<u8> {
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
        let content_id = doc.add_object(Stream::new(dictionary! {}, ops.as_bytes().to_vec()));
        let page_id = doc.add_object(dictionary! {
            "Type" => "Page",
            "Parent" => pages_id,
            "Contents" => content_id,
            "Resources" => resources_id,
            "MediaBox" => vec![0.into(), 0.into(), 595.into(), 842.into()],
        });
        doc.objects.insert(
            pages_id,
            Object::Dictionary(dictionary! {
                "Type" => "Pages",
                "Kids" => vec![Object::Reference(page_id)],
                "Count" => 1,
                "Resources" => resources_id,
                "MediaBox" => vec![0.into(), 0.into(), 595.into(), 842.into()],
            }),
        );
        let catalog_id = doc.add_object(dictionary! {
            "Type" => "Catalog",
            "Pages" => pages_id,
        });
        doc.trailer.set("Root", catalog_id);
        let mut buf = Vec::new();
        doc.save_to(&mut buf).unwrap();
        buf
    }

    /// Decode a page's content stream back into operations.
    fn page_ops(pdf: &[u8], page: u32) -> Vec<Operation> {
        let doc = Document::load_mem(pdf).unwrap();
        let page_id = doc.get_pages()[&page];
        let raw = doc.get_page_content(page_id);
        Content::decode(&raw).unwrap().operations
    }

    /// All string operands shown by text ops, in order.
    fn shown_strings(ops: &[Operation]) -> Vec<String> {
        let mut out = Vec::new();
        for op in ops {
            if !SHOW_OPS.contains(&op.operator.as_str()) {
                continue;
            }
            for obj in &op.operands {
                match obj {
                    Object::String(bytes, _) => {
                        out.push(String::from_utf8_lossy(bytes).into_owned());
                    }
                    Object::Array(items) => {
                        for item in items {
                            if let Object::String(bytes, _) = item {
                                out.push(String::from_utf8_lossy(bytes).into_owned());
                            }
                        }
                    }
                    _ => {}
                }
            }
        }
        out
    }

    #[test]
    fn invisible_text_object_is_removed_visible_kept() {
        // Hand-built stream: one invisible OCR-style block, one visible.
        let input = pdf_with_ops(
            "BT /F1 12 Tf 100 700 Td (visible) Tj ET\n\
             BT /F1 9 Tf 3 Tr 100 680 Td (secret ocr layer) Tj ET\n",
        );
        let (out, report) = remove_hidden_text(&input, None, &limits()).unwrap();
        assert_eq!(report.pages_scanned, 1);
        assert_eq!(report.text_ops_removed, 1);
        // The emptied text object drops BT+Tf+Tr+Td+Tj+ET = 6 ops,
        // of which 1 is the show → 5 other ops.
        assert_eq!(report.other_ops_removed, 5);
        assert!(report.warnings.is_empty(), "{:?}", report.warnings);

        let ops = page_ops(&out, 1);
        assert_eq!(shown_strings(&ops), vec!["visible"]);
        let names: Vec<&str> = ops.iter().map(|o| o.operator.as_str()).collect();
        assert!(!names.contains(&"Tr"), "dead Tr should be gone: {names:?}");
    }

    #[test]
    fn tj_and_tj_array_forms_are_removed() {
        let input = pdf_with_ops(
            "3 Tr\nBT /F1 10 Tf 1 0 0 1 50 50 Tm [(a) -20 (b)] TJ ET\n\
             BT /F1 10 Tf 0 Tr 50 60 Td (shown) Tj ET\n",
        );
        let (out, report) = remove_hidden_text(&input, None, &limits()).unwrap();
        assert_eq!(report.text_ops_removed, 1);
        // The outer `3 Tr` stays (it is outside the text object); the
        // block's BT+Tf+Tm+TJ+ET = 5 ops drop, 1 is the show → 4 other.
        assert_eq!(report.other_ops_removed, 4, "{report:?}");
        let shown = shown_strings(&page_ops(&out, 1));
        assert_eq!(shown, vec!["shown"]);
    }

    #[test]
    fn mixed_block_removes_only_hidden_show_and_warns() {
        // Tr toggles mid-block: hidden show then visible show.
        let input = pdf_with_ops("BT /F1 12 Tf 3 Tr (hid) Tj 0 Tr (vis) Tj ET\n");
        let (out, report) = remove_hidden_text(&input, None, &limits()).unwrap();
        assert_eq!(report.text_ops_removed, 1);
        assert_eq!(report.other_ops_removed, 0);
        assert_eq!(report.warnings.len(), 1);
        assert!(
            report.warnings[0].contains("mixed"),
            "{:?}",
            report.warnings
        );

        let shown = shown_strings(&page_ops(&out, 1));
        assert_eq!(shown, vec!["vis"]);
    }

    #[test]
    fn quote_operators_are_removed() {
        // `'` moves to next line and shows; `"` sets spacing then shows.
        let input = pdf_with_ops("BT /F1 12 Tf 3 Tr 14 TL (hid1) ' 1 2 (hid2) \" ET\n");
        let (out, report) = remove_hidden_text(&input, None, &limits()).unwrap();
        assert_eq!(report.text_ops_removed, 2);
        assert!(shown_strings(&page_ops(&out, 1)).is_empty());
    }

    #[test]
    fn tr_state_persists_across_text_objects() {
        // `3 Tr` set in the first object applies to the second.
        let input = pdf_with_ops("BT 3 Tr ET\nBT /F1 12 Tf 50 50 Td (hid) Tj ET\n");
        let (out, report) = remove_hidden_text(&input, None, &limits()).unwrap();
        assert_eq!(report.text_ops_removed, 1);
        assert!(shown_strings(&page_ops(&out, 1)).is_empty());
    }

    #[test]
    fn visible_only_document_is_unchanged() {
        let input = pdf_with_pages(&["keep me", "also keep"]);
        let (out, report) = remove_hidden_text(&input, None, &limits()).unwrap();
        assert_eq!(report.pages_scanned, 2);
        assert_eq!(report.text_ops_removed, 0);
        assert_eq!(report.other_ops_removed, 0);
        assert!(report.warnings.is_empty());
        let ops = page_ops(&out, 1);
        assert_eq!(shown_strings(&ops), vec!["keep me"]);
    }

    #[test]
    fn page_range_is_respected() {
        // Two pages, each with an invisible block; only page 2 selected.
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
        let mut kids = Vec::new();
        for _ in 0..2 {
            let content = Content {
                operations: vec![
                    Operation::new("BT", vec![]),
                    Operation::new("Tf", vec!["F1".into(), 12.into()]),
                    Operation::new("Tr", vec![Object::Integer(3)]),
                    Operation::new("Tj", vec![Object::string_literal("hidden")]),
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
            kids.push(Object::Reference(page_id));
        }
        doc.objects.insert(
            pages_id,
            Object::Dictionary(dictionary! {
                "Type" => "Pages",
                "Kids" => kids,
                "Count" => 2,
                "Resources" => resources_id,
                "MediaBox" => vec![0.into(), 0.into(), 595.into(), 842.into()],
            }),
        );
        let catalog_id = doc.add_object(dictionary! {
            "Type" => "Catalog",
            "Pages" => pages_id,
        });
        doc.trailer.set("Root", catalog_id);
        let mut buf = Vec::new();
        doc.save_to(&mut buf).unwrap();

        let (out, report) = remove_hidden_text(&buf, Some("2"), &limits()).unwrap();
        assert_eq!(report.pages_scanned, 1);
        assert_eq!(report.text_ops_removed, 1);
        // Page 1 untouched, page 2 emptied.
        assert_eq!(shown_strings(&page_ops(&out, 1)), vec!["hidden"]);
        assert!(shown_strings(&page_ops(&out, 2)).is_empty());
    }

    #[test]
    fn unparseable_stream_is_skipped_with_warning() {
        // Declare a filter that cannot decode; stream must be left
        // unchanged and a warning recorded — but the page still counts
        // as scanned.
        let mut doc = Document::with_version("1.5");
        let pages_id = doc.new_object_id();
        let content_id = doc.add_object(Stream::new(
            dictionary! { "Filter" => "NoSuchFilter" },
            b"BT 3 Tr (x) Tj ET\n".to_vec(),
        ));
        let page_id = doc.add_object(dictionary! {
            "Type" => "Page",
            "Parent" => pages_id,
            "Contents" => content_id,
            "Resources" => dictionary! {},
            "MediaBox" => vec![0.into(), 0.into(), 595.into(), 842.into()],
        });
        doc.objects.insert(
            pages_id,
            Object::Dictionary(dictionary! {
                "Type" => "Pages",
                "Kids" => vec![Object::Reference(page_id)],
                "Count" => 1,
            }),
        );
        let catalog_id = doc.add_object(dictionary! {
            "Type" => "Catalog",
            "Pages" => pages_id,
        });
        doc.trailer.set("Root", catalog_id);
        let mut buf = Vec::new();
        doc.save_to(&mut buf).unwrap();

        let (out, report) = remove_hidden_text(&buf, None, &limits()).unwrap();
        assert_eq!(report.pages_scanned, 1);
        assert_eq!(report.text_ops_removed, 0);
        assert_eq!(report.warnings.len(), 1);
        // Output still contains the (still-compressed, undecodable)
        // stream — nothing was rewritten.
        let _ = out;
    }

    #[test]
    fn encrypted_input_is_rejected() {
        let input = pdf_with_pages(&["secret"]);
        let encrypted = crate::crates::pdf_engine::ops::encrypt(
            &input,
            "user",
            None,
            crate::crates::pdf_engine::ops::PdfPermissions::default(),
            &limits(),
        )
        .unwrap();
        let err = remove_hidden_text(&encrypted, None, &limits()).unwrap_err();
        assert!(matches!(err, PdfError::Encrypted), "got {err:?}");
    }
}
