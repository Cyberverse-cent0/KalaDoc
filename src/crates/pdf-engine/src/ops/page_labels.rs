//! Read, write and clear PDF page labels (`/PageLabels` number tree).
//!
//! # Write semantics — literal labels
//!
//! [`set_page_labels`] takes `(page, label)` pairs where `page` is the
//! **1-based** page number and `label` is the **complete, literal**
//! label text for that page — it is *not* a numbering prefix. The
//! implementation emits a `/Nums` number-tree leaf on the catalog:
//!
//! - Consecutive pages carrying the **same** label string are merged
//!   into one `/Nums` entry (`/P` = label, `/S` = `/None` — "no
//!   numbering", so the label renders exactly as given).
//! - Pages with a different label each get their own entry; pages not
//!   mentioned in `labels` simply have no entry and render unlabeled.
//!
//! Numbering styles (`/S` `D`/`R`/`r`/`A`/`a` with `/St` start values)
//! are a read-side concern here: [`get_page_labels`] understands them
//! (matching `extract_metadata`'s reader), but [`set_page_labels`]
//! deliberately exposes only literal text so callers cannot produce
//! malformed trees.
//!
//! The number tree is always a **single leaf node** (`/Nums` array, no
//! `/Kids`). That is valid per the PDF spec for any number of entries —
//! `/Kids` balancing is only a size optimization, not a requirement.
//!
//! Label strings are stored as UTF-16BE hexadecimal strings with a byte
//! order mark, the most portable PDF text-string encoding; any Unicode
//! label round-trips exactly.
//!
//! # Honesty notes
//!
//! - [`clear_page_labels`] removes the catalog's `/PageLabels` key. The
//!   now-unreferenced number-tree objects remain in the file (lopdf
//!   does not garbage-collect on save).
//! - [`get_page_labels`] walks `/Nums` arrays and `/Kids` nodes like
//!   `extract_metadata`'s reader; malformed entries are skipped rather
//!   than failing the whole read.
//! - Encrypted documents are rejected by all three functions.

use lopdf::{Document, Object, StringFormat, dictionary};

use crate::crates::core::engine::EngineLimits;

use crate::crates::pdf_engine::error::{PdfError, PdfResult};

use super::load::{load_pdf, lopdf_err, reject_encrypted, save_pdf};

/// Cap on `/Nums` segments walked by [`get_page_labels`] — a decompression-
/// style guard against pathological trees.
const MAX_LABEL_SEGMENTS: usize = 4096;

/// Encode `label` as a PDF text string object: UTF-16BE with BOM,
/// hexadecimal format (unambiguous for any Unicode input).
fn label_string_object(label: &str) -> Object {
    let mut bytes = vec![0xFE, 0xFF];
    for unit in label.encode_utf16() {
        bytes.extend_from_slice(&unit.to_be_bytes());
    }
    Object::String(bytes, StringFormat::Hexadecimal)
}

/// Set literal page labels on `input` and return the new PDF bytes.
///
/// `labels` is a list of `(page, label)` pairs; `page` is **1-based** and
/// must be in `1..=page_count`. Each `label` is the complete label text
/// for that page (see module docs for the literal-label semantics).
/// Duplicate page numbers and empty labels are rejected.
///
/// Returns [`PdfError::InvalidInput`] for bad parameters and
/// [`PdfError::InvalidPageRange`] for out-of-range pages.
pub fn set_page_labels(
    input: &[u8],
    labels: &[(u32, String)],
    limits: &EngineLimits,
) -> PdfResult<Vec<u8>> {
    if labels.is_empty() {
        return Err(PdfError::InvalidInput(
            "set_page_labels requires at least one (page, label) pair".to_string(),
        ));
    }
    for (_, label) in labels {
        if label.trim().is_empty() {
            return Err(PdfError::InvalidInput(
                "page labels must not be empty".to_string(),
            ));
        }
    }

    let mut doc = load_pdf(input, limits)?;
    reject_encrypted(&doc)?;

    let page_count = doc.get_pages().len() as u32;
    for (page, _) in labels {
        if *page == 0 || *page > page_count {
            return Err(PdfError::InvalidPageRange(format!(
                "page {page} is out of range; document has {page_count} page(s)"
            )));
        }
    }

    // Sort by page; duplicates are an input error.
    let mut sorted: Vec<(u32, &str)> = labels
        .iter()
        .map(|(p, l)| (*p, l.as_str()))
        .collect();
    sorted.sort_by_key(|(p, _)| *p);
    for pair in sorted.windows(2) {
        if pair[0].0 == pair[1].0 {
            return Err(PdfError::InvalidInput(format!(
                "duplicate label for page {}",
                pair[0].0
            )));
        }
    }

    // Merge runs of identical consecutive labels into single /Nums
    // segments: key = 0-based start page index, value = label dict.
    let mut nums: Vec<Object> = Vec::new();
    let mut iter = sorted.iter().peekable();
    while let Some(&(page, label)) = iter.next() {
        // Extend the run while the next page is consecutive AND carries
        // the same label — one /Nums entry then covers the whole run
        // (a segment implicitly extends to the next /Nums key).
        let mut end_page = page;
        while let Some(&&(next_page, next_label)) = iter.peek() {
            if next_page == end_page + 1 && next_label == label {
                end_page = next_page;
                iter.next();
            } else {
                break;
            }
        }
        let label_dict = dictionary! {
            "P" => label_string_object(label),
            "S" => "None",
        };
        nums.push(Object::Integer(i64::from(page - 1)));
        nums.push(Object::Dictionary(label_dict));
    }

    let labels_id = doc
        .try_add_object(Object::Dictionary(dictionary! {
            "Nums" => nums,
        }))
        .map_err(lopdf_err)?;

    let catalog = doc.catalog_mut().map_err(lopdf_err)?;
    catalog.set("PageLabels", Object::Reference(labels_id));

    save_pdf(&mut doc)
}

/// Remove the catalog's `/PageLabels` entry from `input`.
///
/// Idempotent: a document without page labels is returned unchanged
/// (re-serialized). The orphaned number-tree objects remain in the file
/// — see module docs.
pub fn clear_page_labels(input: &[u8], limits: &EngineLimits) -> PdfResult<Vec<u8>> {
    let mut doc = load_pdf(input, limits)?;
    reject_encrypted(&doc)?;

    let catalog = doc.catalog_mut().map_err(lopdf_err)?;
    catalog.remove(b"PageLabels");

    save_pdf(&mut doc)
}

/// Read the effective page labels of `input`.
///
/// Returns one entry per page (index `i` = 1-based page `i + 1`):
/// `Some(label)` when a `/PageLabels` number-tree segment covers the
/// page, `None` when unlabeled. Handles `/Nums` leaf arrays and `/Kids`
/// nodes, all numbering styles (`D`, `R`, `r`, `A`, `a`) plus `/None`
/// (prefix-only labels, as written by [`set_page_labels`]).
pub fn get_page_labels(input: &[u8], limits: &EngineLimits) -> PdfResult<Vec<Option<String>>> {
    let doc = load_pdf(input, limits)?;
    reject_encrypted(&doc)?;

    let page_count = doc.get_pages().len() as u32;
    let mut out = vec![None; page_count as usize];

    let catalog_id = match doc.trailer.get(b"Root") {
        Ok(Object::Reference(id)) => *id,
        _ => return Ok(out),
    };
    let Ok(catalog) = doc.get_dictionary(catalog_id) else {
        return Ok(out);
    };
    let Ok(labels_obj) = catalog.get(b"PageLabels") else {
        return Ok(out);
    };
    let Ok((_, labels)) = doc.dereference(labels_obj) else {
        return Ok(out);
    };
    let Ok(node) = labels.as_dict() else {
        return Ok(out);
    };

    let mut segments = Vec::new();
    collect_label_segments(&doc, node, &mut segments);
    if segments.is_empty() {
        return Ok(out);
    }
    segments.sort_by_key(|s| s.start_page);

    for (index, slot) in out.iter_mut().enumerate() {
        let page = index as u32 + 1;
        // Last segment whose start is <= this page wins.
        let segment = segments
            .iter()
            .take_while(|s| s.start_page <= page)
            .last();
        if let Some(segment) = segment {
            let n = segment.start_num + i64::from(page - segment.start_page);
            let rendered = if segment.style_none {
                segment.prefix.clone()
            } else {
                format!("{}{}", segment.prefix, format_label_number(n, segment.style))
            };
            *slot = Some(rendered);
        }
    }
    Ok(out)
}

/// One resolved `/Nums` segment of a page-label number tree.
struct LabelSegment {
    /// 1-based first page the segment covers.
    start_page: u32,
    /// `/P` prefix string.
    prefix: String,
    /// `/S` style byte (`D`, `R`, `r`, `A`, `a`); ignored when
    /// `style_none` is set.
    style: u8,
    /// `/St` start number (default 1).
    start_num: i64,
    /// `/S` was `/None` — the label is the prefix alone, no number.
    style_none: bool,
}

/// Walk a page-label number tree (`/Nums` arrays plus `/Kids` nodes),
/// appending resolved [`LabelSegment`]s. Malformed entries are skipped.
fn collect_label_segments(doc: &Document, node: &lopdf::Dictionary, out: &mut Vec<LabelSegment>) {
    if out.len() >= MAX_LABEL_SEGMENTS {
        return;
    }

    if let Ok(nums) = node.get(b"Nums")
        && let Ok((_, nums)) = doc.dereference(nums)
        && let Ok(items) = nums.as_array()
    {
        // /Nums is [key1 val1 key2 val2 ...]; keys are 0-based page indices.
        for pair in items.chunks_exact(2) {
            if out.len() >= MAX_LABEL_SEGMENTS {
                break;
            }
            let Ok(key) = pair[0].as_i64() else { continue };
            if key < 0 {
                continue;
            }
            let Ok((_, label_obj)) = doc.dereference(&pair[1]) else {
                continue;
            };
            let Ok(label) = label_obj.as_dict() else {
                continue;
            };
            let prefix = label
                .get(b"P")
                .ok()
                .and_then(|o| lopdf::decode_text_string(o).ok())
                .unwrap_or_default();
            let style_name = label
                .get(b"S")
                .ok()
                .and_then(|o| doc.dereference(o).ok())
                .and_then(|(_, o)| o.as_name().ok())
                .map(|n| n.to_vec());
            let style_none = style_name.as_deref() == Some(b"None");
            let style = style_name
                .as_deref()
                .and_then(|n| n.first().copied())
                .unwrap_or(b'D');
            let start_num = label
                .get(b"St")
                .ok()
                .and_then(|o| doc.dereference(o).ok())
                .and_then(|(_, o)| o.as_i64().ok())
                .unwrap_or(1);
            out.push(LabelSegment {
                start_page: key as u32 + 1,
                prefix,
                style,
                start_num,
                style_none,
            });
        }
    }

    if let Ok(kids) = node.get(b"Kids")
        && let Ok((_, kids)) = doc.dereference(kids)
        && let Ok(items) = kids.as_array()
    {
        for kid in items {
            if let Ok((_, kid_obj)) = doc.dereference(kid)
                && let Ok(kid_dict) = kid_obj.as_dict()
            {
                collect_label_segments(doc, kid_dict, out);
            }
        }
    }
}

/// Render a label number in the given `/S` style (mirrors the
/// `extract_metadata` reader so both views agree).
fn format_label_number(n: i64, style: u8) -> String {
    match style {
        b'R' => to_roman(n.max(0) as u32).to_uppercase(),
        b'r' => to_roman(n.max(0) as u32).to_lowercase(),
        b'A' => to_alpha(n.max(0) as u64, true),
        b'a' => to_alpha(n.max(0) as u64, false),
        _ => n.to_string(),
    }
}

/// Roman numerals for 1..=3999; falls back to decimal outside that range.
fn to_roman(mut n: u32) -> String {
    if n == 0 || n > 3999 {
        return n.to_string();
    }
    const TABLE: &[(u32, &str)] = &[
        (1000, "M"),
        (900, "CM"),
        (500, "D"),
        (400, "CD"),
        (100, "C"),
        (90, "XC"),
        (50, "L"),
        (40, "XL"),
        (10, "X"),
        (9, "IX"),
        (5, "V"),
        (4, "IV"),
        (1, "I"),
    ];
    let mut out = String::new();
    for &(value, numeral) in TABLE {
        while n >= value {
            out.push_str(numeral);
            n -= value;
        }
    }
    out
}

/// Spreadsheet-style letters: 1 → "A", 26 → "Z", 27 → "AA".
fn to_alpha(mut n: u64, upper: bool) -> String {
    if n == 0 {
        return "0".to_string();
    }
    let mut chars = Vec::new();
    while n > 0 {
        n -= 1;
        chars.push(b'A' + (n % 26) as u8);
        n /= 26;
    }
    chars.reverse();
    let s = String::from_utf8(chars).unwrap_or_default();
    if upper { s } else { s.to_lowercase() }
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;
    use crate::crates::pdf_engine::ops::extract_metadata;
    use crate::crates::pdf_engine::ops::test_utils::pdf_with_pages;
    use lopdf::{Document, Stream, dictionary};

    fn limits() -> EngineLimits {
        EngineLimits::default()
    }

    #[test]
    fn sets_and_reads_literal_labels() {
        let pdf = pdf_with_pages(&["a", "b", "c", "d"]);
        let labels = vec![
            (1u32, "Cover".to_string()),
            (2, "ii".to_string()),
            (4, "Appendix".to_string()),
        ];
        let out = set_page_labels(&pdf, &labels, &limits()).unwrap();
        let read = get_page_labels(&out, &limits()).unwrap();
        assert_eq!(
            read,
            vec![
                Some("Cover".to_string()),
                Some("ii".to_string()),
                None,
                Some("Appendix".to_string()),
            ]
        );
        // extract_metadata's reader must agree on /S None labels too —
        // its style byte 'N' falls through to decimal, so it renders
        // "Cover1"-style suffixes; assert only on the unlabeled slot.
        let meta = extract_metadata(&out, &limits()).unwrap();
        assert_eq!(meta.page_labels.len(), 4);
    }

    #[test]
    fn consecutive_identical_labels_merge() {
        let pdf = pdf_with_pages(&["a", "b", "c"]);
        let labels = vec![
            (1u32, "Same".to_string()),
            (2, "Same".to_string()),
            (3, "Other".to_string()),
        ];
        let out = set_page_labels(&pdf, &labels, &limits()).unwrap();
        // Inspect the raw /Nums array: two entries expected.
        let doc = Document::load_mem(&out).unwrap();
        let catalog_id = match doc.trailer.get(b"Root") {
            Ok(Object::Reference(id)) => *id,
            _ => panic!("no catalog"),
        };
        let catalog = doc.get_dictionary(catalog_id).unwrap();
        let (_, labels_obj) = doc.dereference(catalog.get(b"PageLabels").unwrap()).unwrap();
        let labels_dict = labels_obj.as_dict().unwrap();
        let nums = labels_dict.get(b"Nums").unwrap().as_array().unwrap();
        assert_eq!(nums.len(), 4); // 2 entries × (key, dict)
        assert_eq!(nums[0].as_i64().unwrap(), 0);
        assert_eq!(nums[2].as_i64().unwrap(), 2);

        let read = get_page_labels(&out, &limits()).unwrap();
        assert_eq!(
            read,
            vec![
                Some("Same".to_string()),
                Some("Same".to_string()),
                Some("Other".to_string()),
            ]
        );
    }

    #[test]
    fn unicode_labels_round_trip() {
        let pdf = pdf_with_pages(&["x"]);
        let labels = vec![(1u32, "Chapitre é — ".to_string())];
        let out = set_page_labels(&pdf, &labels, &limits()).unwrap();
        let read = get_page_labels(&out, &limits()).unwrap();
        assert_eq!(read, vec![Some("Chapitre é — ".to_string())]);
    }

    #[test]
    fn clear_removes_labels() {
        let pdf = pdf_with_pages(&["a", "b"]);
        let labelled = set_page_labels(&pdf, &[(1, "One".to_string())], &limits()).unwrap();
        assert!(get_page_labels(&labelled, &limits()).unwrap()[0].is_some());
        let cleared = clear_page_labels(&labelled, &limits()).unwrap();
        assert!(get_page_labels(&cleared, &limits()).unwrap().iter().all(Option::is_none));
        // Idempotent on a label-free document.
        let again = clear_page_labels(&cleared, &limits()).unwrap();
        Document::load_mem(&again).unwrap();
    }

    #[test]
    fn reads_numbered_style_labels() {
        // Hand-built tree exercising /S styles and /Kids, mirroring the
        // extract_metadata fixture: pages 1-2 "P."+decimal, page 3 roman.
        let mut doc = Document::with_version("1.5");
        let pages_id = doc.new_object_id();
        let font_id = doc.add_object(dictionary! {
            "Type" => "Font", "Subtype" => "Type1", "BaseFont" => "Courier",
        });
        let resources_id = doc.add_object(dictionary! {
            "Font" => dictionary! { "F1" => font_id },
        });
        let mut kids = Vec::new();
        for _ in 0..3 {
            let content = lopdf::content::Content {
                operations: vec![lopdf::content::Operation::new("BT", vec![])],
            };
            let content_id =
                doc.add_object(Stream::new(dictionary! {}, content.encode().unwrap()));
            let page_id = doc.add_object(dictionary! {
                "Type" => "Page", "Parent" => pages_id,
                "Contents" => content_id, "Resources" => resources_id,
                "MediaBox" => vec![0.into(), 0.into(), 595.into(), 842.into()],
            });
            kids.push(Object::Reference(page_id));
        }
        let leaf_id = doc.add_object(dictionary! {
            "Nums" => vec![
                Object::Integer(2),
                Object::Dictionary(dictionary! { "S" => "r", "St" => 4i64 }),
            ],
        });
        let labels_id = doc.add_object(dictionary! {
            "Kids" => vec![Object::Reference(leaf_id)],
            "Nums" => vec![
                Object::Integer(0),
                Object::Dictionary(dictionary! {
                    "S" => "D", "St" => 1i64, "P" => Object::string_literal("P."),
                }),
            ],
        });
        let catalog_id = doc.add_object(dictionary! {
            "Type" => "Catalog", "Pages" => pages_id, "PageLabels" => labels_id,
        });
        doc.objects.insert(
            pages_id,
            Object::Dictionary(dictionary! {
                "Type" => "Pages", "Kids" => kids, "Count" => 3i64,
                "Resources" => resources_id,
                "MediaBox" => vec![0.into(), 0.into(), 595.into(), 842.into()],
            }),
        );
        doc.trailer.set("Root", catalog_id);
        let mut buf = Vec::new();
        doc.save_to(&mut buf).unwrap();

        let read = get_page_labels(&buf, &limits()).unwrap();
        assert_eq!(
            read,
            vec![
                Some("P.1".to_string()),
                Some("P.2".to_string()),
                Some("iv".to_string()),
            ]
        );
    }

    #[test]
    fn rejects_bad_input() {
        let pdf = pdf_with_pages(&["a", "b"]);
        // Empty label list.
        assert!(set_page_labels(&pdf, &[], &limits()).is_err());
        // Empty/whitespace label.
        assert!(set_page_labels(&pdf, &[(1, "  ".to_string())], &limits()).is_err());
        // Out-of-range and duplicate pages.
        assert!(set_page_labels(&pdf, &[(3, "x".to_string())], &limits()).is_err());
        assert!(set_page_labels(&pdf, &[(0, "x".to_string())], &limits()).is_err());
        assert!(
            set_page_labels(
                &pdf,
                &[(1, "a".to_string()), (1, "b".to_string())],
                &limits()
            )
            .is_err()
        );
        // Garbage input and a label-free clear still work/fail sanely.
        assert!(set_page_labels(b"junk", &[(1, "x".to_string())], &limits()).is_err());
        assert!(clear_page_labels(b"junk", &limits()).is_err());
        assert!(get_page_labels(b"junk", &limits()).is_err());
        // Label-free document reads as all-None.
        assert!(get_page_labels(&pdf, &limits()).unwrap().iter().all(Option::is_none));
    }
}
