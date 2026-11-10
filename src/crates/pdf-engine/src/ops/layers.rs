//! Optional-content ("layer") usage scanning.
//!
//! [`list_oc_marked_content`] is the read-only counterpart of
//! [`list_layers`](super::optional_content::list_layers): where that op
//! reports the groups *declared* in `/OCProperties`, this op reports the
//! layers *actually referenced* by each page's content streams through
//! marked-content operators:
//!
//! ```text
//! /OC /LayerName BDC          % property operand = OCG name
//! /OC << /Name (L) >> BDC     % property operand = OCG/OCMD dictionary
//! /OC /LayerName BMC          % begin marked content, no properties
//! ... EMC
//! ```
//!
//! Only the page's own `/Contents` streams are scanned — marked content
//! inside Form XObjects reached via `Do` is not followed (same scope as
//! `redact_regions`). Streams are decompressed with a per-stream bound;
//! a page's streams are skipped once their combined decompressed size
//! exceeds [`MAX_PAGE_CONTENT_TOTAL`].

use std::collections::{HashMap, HashSet};

use lopdf::content::Content;
use lopdf::{Document, Object, ObjectId, decode_text_string};
use serde::{Deserialize, Serialize};

use crate::crates::core::engine::EngineLimits;

use crate::crates::pdf_engine::error::PdfResult;

use super::load::load_pdf;

/// Per-stream decompressed-size bound — same guard as `redact_regions`
/// and `remove_images`.
const MAX_CONTENT_STREAM: usize = 64 * 1024 * 1024;

/// Total decompressed bytes accepted from one page's `/Contents`
/// streams combined. Streams beyond the cap are skipped.
const MAX_PAGE_CONTENT_TOTAL: usize = 256 * 1024 * 1024;

/// The optional-content layers a single page references.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct OcUsage {
    /// 1-based page number, matching `doc.get_pages()` ordering.
    pub page: u32,
    /// Layer names found in `/OC ... BDC` / `/OC ... BMC` operators on
    /// this page, deduplicated in first-seen order. Names come from the
    /// marked-content operand: the name object itself, or the `/Name`
    /// entry of an inline/referenced OCG/OCMD dictionary.
    pub layer_names: Vec<String>,
}

/// Declared-layer lookup built from the catalog's
/// `/OCProperties//OCGs`: a map from indirect OCG object id to its
/// `/Name`, used when a marked-content operand is a bare reference to
/// an OCG dictionary that lacks its own `/Name`. Name operands need no
/// lookup — per spec the name object *is* the layer name.
#[derive(Default)]
struct DeclaredLayers {
    by_id: HashMap<ObjectId, String>,
}

/// Collect the declared OCGs from `/OCProperties//OCGs`. Tolerates
/// missing/malformed pieces — an absent catalog simply yields an empty
/// map and operands are reported verbatim.
fn declared_layers(doc: &Document) -> DeclaredLayers {
    let mut out = DeclaredLayers::default();
    let Some(root) = doc
        .trailer
        .get(b"Root")
        .ok()
        .and_then(|o| o.as_reference().ok())
    else {
        return out;
    };
    let Ok(catalog) = doc.get_dictionary(root) else {
        return out;
    };
    let Ok(ocprops_raw) = catalog.get(b"OCProperties") else {
        return out;
    };
    let Ok((_, ocprops_obj)) = doc.dereference(ocprops_raw) else {
        return out;
    };
    let Ok(ocprops) = ocprops_obj.as_dict() else {
        return out;
    };
    let Ok(ocgs_raw) = ocprops.get(b"OCGs") else {
        return out;
    };
    let Ok((_, ocgs_obj)) = doc.dereference(ocgs_raw) else {
        return out;
    };
    let Ok(items) = ocgs_obj.as_array() else {
        return out;
    };
    for item in items {
        let Ok((id_opt, obj)) = doc.dereference(item) else {
            continue;
        };
        let Ok(dict) = obj.as_dict() else {
            continue;
        };
        let Ok(name_obj) = dict.get(b"Name") else {
            continue;
        };
        let Ok(name) = decode_text_string(name_obj) else {
            continue;
        };
        if let Some(id) = id_opt {
            out.by_id.insert(id, name);
        }
    }
    out
}

/// `/Name` text of a dictionary, dereferencing the value.
fn dict_name(doc: &Document, dict: &lopdf::Dictionary) -> Option<String> {
    let raw = dict.get(b"Name").ok()?;
    let (_, resolved) = doc.dereference(raw).ok()?;
    decode_text_string(resolved).ok()
}

/// Resolve the property operand of an `/OC` marked-content operator to
/// a layer name:
///
/// - `Object::Name` — the OCG name itself (per spec the name object
///   *is* the layer name).
/// - `Object::Dictionary` — its `/Name` entry (OCG or OCMD property
///   list). When `/Name` is absent but the operand was a reference to
///   a declared OCG, that OCG's catalog name is used.
/// - `Object::Reference` — dereferenced and handled as above.
fn oc_operand_name(doc: &Document, declared: &DeclaredLayers, operand: &Object) -> Option<String> {
    match operand {
        Object::Name(raw) => Some(String::from_utf8_lossy(raw).into_owned()),
        Object::Dictionary(d) => dict_name(doc, d),
        Object::Reference(id) => {
            let (_, resolved) = doc.dereference(operand).ok()?;
            match resolved {
                Object::Dictionary(d) => {
                    dict_name(doc, d).or_else(|| declared.by_id.get(id).cloned())
                }
                Object::Name(raw) => Some(String::from_utf8_lossy(raw).into_owned()),
                _ => None,
            }
        }
        _ => None,
    }
}

/// Decode a content stream bounded by [`MAX_CONTENT_STREAM`]. Streams
/// without `/Filter` fall back to raw bytes; undecodable or over-limit
/// streams yield `None`.
fn stream_content(stream: &lopdf::Stream) -> Option<Vec<u8>> {
    match stream.decompressed_content_with_limit(MAX_CONTENT_STREAM) {
        Ok(data) => Some(data),
        Err(_) if !stream.dict.has(b"Filter") => Some(stream.content.clone()),
        Err(_) => None,
    }
}

/// Scan one content stream's operations for `/OC ... BDC`/`BMC` and
/// append resolved layer names to `found` (deduplicated, ordered).
fn scan_operations(
    doc: &Document,
    declared: &DeclaredLayers,
    raw: &[u8],
    found: &mut Vec<String>,
    seen: &mut HashSet<String>,
) {
    let Ok(content) = Content::decode(raw) else {
        return;
    };
    for op in &content.operations {
        if op.operator != "BDC" && op.operator != "BMC" {
            continue;
        }
        // Operands: /OC <property>. The property operand is the second
        // one; a lone `/OC` (no property) is ignored.
        let Some(oc_tag) = op.operands.first() else {
            continue;
        };
        if oc_tag.as_name().ok() != Some(&b"OC"[..]) {
            continue;
        }
        let Some(prop) = op.operands.get(1) else {
            continue;
        };
        let Some(name) = oc_operand_name(doc, declared, prop) else {
            continue;
        };
        if seen.insert(name.clone()) {
            found.push(name);
        }
    }
}

/// List the optional-content layers referenced by each page's content.
///
/// Returns one [`OcUsage`] per page that contains at least one `/OC
/// ... BDC`/`BMC` marked-content operator; pages without optional
/// content are omitted. Layer names are deduplicated per page in
/// first-seen order and resolved against the catalog's declared OCGs
/// where the operand requires it. Read-only: input is never modified,
/// and encrypted documents are tolerated (their content streams are
/// ciphertext and simply yield no decodable operations).
pub fn list_oc_marked_content(input: &[u8], limits: &EngineLimits) -> PdfResult<Vec<OcUsage>> {
    let doc = load_pdf(input, limits)?;
    let declared = declared_layers(&doc);
    let pages = doc.get_pages();

    let mut out = Vec::new();
    for (page_number, page_id) in pages {
        let mut layer_names: Vec<String> = Vec::new();
        let mut seen: HashSet<String> = HashSet::new();
        let mut total = 0usize;

        for stream_id in doc.get_page_contents(page_id) {
            let Ok(object) = doc.get_object(stream_id) else {
                continue;
            };
            let Ok(stream) = object.as_stream() else {
                continue;
            };
            let Some(raw) = stream_content(stream) else {
                continue;
            };
            total = total.saturating_add(raw.len());
            if total > MAX_PAGE_CONTENT_TOTAL {
                break;
            }
            scan_operations(&doc, &declared, &raw, &mut layer_names, &mut seen);
        }

        if !layer_names.is_empty() {
            out.push(OcUsage {
                page: page_number,
                layer_names,
            });
        }
    }
    out.sort_by_key(|u| u.page);
    Ok(out)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::crates::pdf_engine::ops::test_utils::pdf_with_pages;
    use lopdf::{Stream, dictionary};

    /// Replace a page's `/Contents` streams with a single stream holding
    /// `ops` (raw content-stream syntax).
    fn set_page_content(doc: &mut Document, page_id: ObjectId, ops: &str) {
        let stream_id = doc.add_object(Stream::new(dictionary! {}, ops.as_bytes().to_vec()));
        if let Ok(page) = doc.get_dictionary_mut(page_id) {
            page.set("Contents", Object::Reference(stream_id));
        }
    }

    /// Build a two-page PDF: page 1 uses `/OC /L1 BDC` and
    /// `/OC /L2 BMC`, page 2 uses an inline property dict
    /// `/OC << /Name (Layer B) >> BDC`. The catalog declares OCGs
    /// "L1", "L2" and "Layer B".
    fn pdf_with_oc_usage() -> Vec<u8> {
        let mut doc = Document::load_mem(&pdf_with_pages(&["p1", "p2"])).unwrap();
        let pages = doc.get_pages();
        let ids: Vec<ObjectId> = pages.values().copied().collect();

        set_page_content(
            &mut doc,
            ids[0],
            "/OC /L1 BDC\n0 0 1 rg\n10 10 100 100 re f\nEMC\n\
             /OC /L2 BMC\n0 1 0 rg\nEMC\n",
        );
        set_page_content(
            &mut doc,
            ids[1],
            "/OC << /Name (Layer B) >> BDC\n0 0 0 rg\nEMC\n",
        );

        let ocg1 = doc.add_object(dictionary! {
            "Type" => "OCG",
            "Name" => Object::string_literal("L1"),
        });
        let ocg2 = doc.add_object(dictionary! {
            "Type" => "OCG",
            "Name" => Object::string_literal("L2"),
        });
        let ocg3 = doc.add_object(dictionary! {
            "Type" => "OCG",
            "Name" => Object::string_literal("Layer B"),
        });
        let root_id = doc.trailer.get(b"Root").unwrap().as_reference().unwrap();
        let catalog = doc.get_dictionary_mut(root_id).unwrap();
        catalog.set(
            "OCProperties",
            dictionary! {
                "OCGs" => Object::Array(vec![
                    Object::Reference(ocg1),
                    Object::Reference(ocg2),
                    Object::Reference(ocg3),
                ]),
            },
        );

        let mut buf = Vec::new();
        doc.save_to(&mut buf).unwrap();
        buf
    }

    #[test]
    fn reports_layer_names_per_page() {
        let pdf = pdf_with_oc_usage();
        let usage = list_oc_marked_content(&pdf, &EngineLimits::default()).unwrap();
        assert_eq!(usage.len(), 2);
        assert_eq!(usage[0].page, 1);
        assert_eq!(
            usage[0].layer_names,
            vec!["L1".to_string(), "L2".to_string()]
        );
        assert_eq!(usage[1].page, 2);
        assert_eq!(usage[1].layer_names, vec!["Layer B".to_string()]);
    }

    #[test]
    fn dedupes_repeated_layers_on_a_page() {
        let mut doc = Document::load_mem(&pdf_with_pages(&["dup"])).unwrap();
        let page_id = *doc.get_pages().values().next().unwrap();
        set_page_content(
            &mut doc,
            page_id,
            "/OC /L1 BDC\nEMC\n/OC /L1 BDC\nEMC\n/OC /L2 BDC\nEMC\n",
        );
        let mut buf = Vec::new();
        doc.save_to(&mut buf).unwrap();

        let usage = list_oc_marked_content(&buf, &EngineLimits::default()).unwrap();
        assert_eq!(usage.len(), 1);
        assert_eq!(
            usage[0].layer_names,
            vec!["L1".to_string(), "L2".to_string()]
        );
    }

    #[test]
    fn empty_without_oc_content() {
        let plain = pdf_with_pages(&["no layers"]);
        assert!(
            list_oc_marked_content(&plain, &EngineLimits::default())
                .unwrap()
                .is_empty()
        );
    }

    #[test]
    fn tolerates_malformed_input() {
        assert!(list_oc_marked_content(b"not a pdf", &EngineLimits::default()).is_err());
        // Truncated but loadable-ish garbage must not panic.
        let _ = list_oc_marked_content(&[0u8; 64], &EngineLimits::default());
    }

    #[test]
    fn respects_max_pages_limit() {
        let pdf = pdf_with_pages(&["a", "b", "c"]);
        let limits = EngineLimits {
            max_pages: Some(2),
            ..EngineLimits::default()
        };
        assert!(list_oc_marked_content(&pdf, &limits).is_err());
    }

    #[test]
    fn serde_roundtrip() {
        let usage = OcUsage {
            page: 3,
            layer_names: vec!["A".to_string()],
        };
        let json = serde_json::to_string(&usage).unwrap();
        let back: OcUsage = serde_json::from_str(&json).unwrap();
        assert_eq!(usage, back);
    }
}
