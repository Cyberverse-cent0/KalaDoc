//! Tagged-PDF structure tree inspection.
//!
//! [`inspect_structure`] reports whether a document is tagged
//! (`/MarkInfo` → `/Marked true`), counts the entries of the
//! `/StructTreeRoot` `/RoleMap`, and walks the `/K` tree collecting one
//! [`StructElementInfo`] per structure element: its `/S` role, optional
//! `/Alt` text, and the 1-based page number resolved from `/Pg`.
//! Integer `/K` children are marked-content identifiers (MCIDs) and are
//! counted in [`StructReport::mcid_refs`] rather than recorded as
//! elements.
//!
//! Like [`super::info::inspect`], encrypted input is tolerated: the
//! walk is structural, so roles (names are never encrypted) still
//! resolve, but `/Alt` strings in encrypted documents cannot be
//! decoded without a password and surface as `None`.
//!
//! The walk is read-only and bounded: recursion is capped at
//! [`MAX_DEPTH`] and at most [`MAX_ELEMENTS`] elements are recorded;
//! hitting either cap appends a [`StructReport::warnings`] entry and
//! truncates the report rather than failing.

use std::collections::HashMap;

use lopdf::{Document, Object, ObjectId, decode_text_string};
use serde::{Deserialize, Serialize};

use crate::crates::core::engine::EngineLimits;

use crate::crates::pdf_engine::error::PdfResult;

use super::load::load_pdf;

/// Maximum `/K` recursion depth before the walk is truncated.
const MAX_DEPTH: usize = 64;
/// Maximum structure elements recorded in one report.
const MAX_ELEMENTS: usize = 500;

/// One node of the document's structure tree.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct StructElementInfo {
    /// The element's `/S` role name (e.g. `"P"`, `"H1"`), or `"Unknown"`
    /// when the entry is missing or undecodable.
    pub role: String,
    /// The element's `/Alt` alternate text, when present and decodable.
    pub alt_text: Option<String>,
    /// 1-based page number resolved from the element's `/Pg` entry,
    /// when it references a page in the document's page tree.
    pub page: Option<u32>,
}

/// Structure-tree summary for a PDF document.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct StructReport {
    /// Whether the catalog `/MarkInfo` dictionary declares
    /// `/Marked true`. When `false` every other field is empty.
    pub tagged: bool,
    /// Number of entries in `/StructTreeRoot` → `/RoleMap` (0 when the
    /// entry is absent).
    pub role_map_count: usize,
    /// Structure elements in tree order (truncated at [`MAX_ELEMENTS`]).
    pub elements: Vec<StructElementInfo>,
    /// Count of integer `/K` children encountered — marked-content
    /// references into page content streams.
    pub mcid_refs: usize,
    /// Non-fatal problems: depth/element caps hit, missing
    /// `/StructTreeRoot`, undecodable references, and so on.
    pub warnings: Vec<String>,
}

/// Inspect the tagged structure tree of `input`.
///
/// Untagged documents (no `/MarkInfo` → `/Marked true`) return a
/// `tagged: false` report with empty sections — this is not an error.
pub fn inspect_structure(input: &[u8], limits: &EngineLimits) -> PdfResult<StructReport> {
    let doc = load_pdf(input, limits)?;

    let mut report = StructReport {
        tagged: false,
        role_map_count: 0,
        elements: Vec::new(),
        mcid_refs: 0,
        warnings: Vec::new(),
    };

    let Ok(catalog) = doc.catalog() else {
        return Ok(report);
    };

    let marked = catalog
        .get(b"MarkInfo")
        .ok()
        .and_then(|o| doc.dereference(o).ok())
        .and_then(|(_, o)| o.as_dict().ok())
        .and_then(|d| d.get(b"Marked").ok())
        .and_then(|o| doc.dereference(o).ok())
        .and_then(|(_, o)| o.as_bool().ok())
        .unwrap_or(false);
    if !marked {
        return Ok(report);
    }
    report.tagged = true;

    let Some(root) = catalog
        .get(b"StructTreeRoot")
        .ok()
        .and_then(|o| doc.dereference(o).ok())
        .map(|(_, o)| o.clone())
    else {
        report
            .warnings
            .push("/MarkInfo /Marked is true but /StructTreeRoot is missing".to_string());
        return Ok(report);
    };
    let Ok(root_dict) = root.as_dict() else {
        report
            .warnings
            .push("/StructTreeRoot is not a dictionary".to_string());
        return Ok(report);
    };

    report.role_map_count = root_dict
        .get(b"RoleMap")
        .ok()
        .and_then(|o| doc.dereference(o).ok())
        .and_then(|(_, o)| o.as_dict().ok())
        .map(|d| d.len())
        .unwrap_or(0);

    // Reverse map: page object id -> 1-based page number.
    let page_of: HashMap<ObjectId, u32> = doc
        .get_pages()
        .iter()
        .map(|(number, id)| (*id, *number))
        .collect();

    if let Ok(k) = root_dict.get(b"K") {
        walk_k(&doc, k, 0, &page_of, &mut report);
    }
    Ok(report)
}

/// Push `msg` into `report.warnings` unless already present — the caps
/// can be hit from many sibling nodes.
fn warn_once(report: &mut StructReport, msg: &str) {
    if !report.warnings.iter().any(|w| w == msg) {
        report.warnings.push(msg.to_string());
    }
}

/// Resolve one `/K` value: an MCID integer, an array of kids, or a
/// structure-element dictionary (whose own `/K` is walked next).
fn walk_k(
    doc: &Document,
    object: &Object,
    depth: usize,
    page_of: &HashMap<ObjectId, u32>,
    report: &mut StructReport,
) {
    if depth >= MAX_DEPTH {
        warn_once(
            report,
            &format!("structure tree deeper than {MAX_DEPTH} levels; truncated"),
        );
        return;
    }
    let Ok((_, resolved)) = doc.dereference(object) else {
        return;
    };
    match resolved {
        Object::Integer(_) => report.mcid_refs += 1,
        Object::Array(items) => {
            for item in items {
                walk_k(doc, item, depth + 1, page_of, report);
            }
        }
        Object::Dictionary(dict) => visit_element(doc, dict, depth, page_of, report),
        _ => {}
    }
}

/// Record one structure-element dictionary and recurse into its `/K`.
fn visit_element(
    doc: &Document,
    dict: &lopdf::Dictionary,
    depth: usize,
    page_of: &HashMap<ObjectId, u32>,
    report: &mut StructReport,
) {
    if report.elements.len() >= MAX_ELEMENTS {
        warn_once(
            report,
            &format!("structure tree has more than {MAX_ELEMENTS} elements; truncated"),
        );
        return;
    }

    let role = dict
        .get(b"S")
        .ok()
        .and_then(|o| doc.dereference(o).ok())
        .and_then(|(_, o)| o.as_name().ok())
        .map(|name| String::from_utf8_lossy(name).into_owned())
        .filter(|s| !s.is_empty())
        .unwrap_or_else(|| "Unknown".to_string());

    let alt_text = dict
        .get(b"Alt")
        .ok()
        .and_then(|o| doc.dereference(o).ok())
        .and_then(|(_, o)| decode_text_string(o).ok())
        .filter(|s| !s.is_empty());

    // `/Pg` is normally a direct reference to the page object; only a
    // reference can be mapped back to a page number.
    let page = dict.get(b"Pg").ok().and_then(|o| match o {
        Object::Reference(id) => page_of.get(id).copied(),
        _ => None,
    });

    report.elements.push(StructElementInfo {
        role,
        alt_text,
        page,
    });

    if let Ok(k) = dict.get(b"K") {
        walk_k(doc, k, depth + 1, page_of, report);
    }
}

#[cfg(test)]
mod tests {
    use lopdf::{Document, Object, dictionary};

    use crate::crates::core::engine::EngineLimits;
    use crate::crates::pdf_engine::ops::test_utils::pdf_with_pages;

    use super::{MAX_DEPTH, inspect_structure};

    /// Build a tagged PDF: one page whose structure tree holds a
    /// `/Document` root with two children — a `/P` element carrying
    /// `/Alt` text, an `/H1` element, and one referencing a missing
    /// page — plus a `/RoleMap` with two entries.
    fn tagged_pdf() -> Vec<u8> {
        let mut doc = Document::with_version("1.7");

        let pages_id = doc.new_object_id();
        let page_id = doc.add_object(dictionary! {
            "Type" => "Page",
            "Parent" => pages_id,
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

        let paragraph = doc.add_object(dictionary! {
            "Type" => "StructElem",
            "S" => "P",
            "Alt" => Object::string_literal("A paragraph of text"),
            "Pg" => Object::Reference(page_id),
            "K" => vec![Object::Integer(0), Object::Integer(1)],
        });
        let heading = doc.add_object(dictionary! {
            "Type" => "StructElem",
            "S" => "H1",
            "Pg" => Object::Reference(page_id),
            "K" => 2,
        });
        // Element whose /Pg points at a non-page object: page stays None.
        let orphan = doc.add_object(dictionary! {
            "Type" => "StructElem",
            "S" => "Note",
            "Pg" => Object::Reference(pages_id),
            "K" => Object::Array(vec![]),
        });

        let root_id = doc.add_object(dictionary! {
            "Type" => "StructTreeRoot",
            "RoleMap" => dictionary! {
                "H1" => "H2",
                "Note" => "Span",
            },
            "K" => vec![
                Object::Reference(paragraph),
                Object::Reference(heading),
                Object::Reference(orphan),
            ],
        });

        let catalog_id = doc.add_object(dictionary! {
            "Type" => "Catalog",
            "Pages" => pages_id,
            "MarkInfo" => dictionary! { "Marked" => true },
            "StructTreeRoot" => root_id,
        });
        doc.trailer.set("Root", catalog_id);

        let mut buf = Vec::new();
        doc.save_to(&mut buf).unwrap();
        buf
    }

    #[test]
    fn untagged_pdf_reports_not_tagged() {
        let pdf = pdf_with_pages(&["plain"]);
        let report = inspect_structure(&pdf, &EngineLimits::default()).unwrap();
        assert!(!report.tagged);
        assert_eq!(report.role_map_count, 0);
        assert!(report.elements.is_empty());
        assert_eq!(report.mcid_refs, 0);
        assert!(report.warnings.is_empty());
    }

    #[test]
    fn tagged_pdf_walks_structure_tree() {
        let pdf = tagged_pdf();
        let report = inspect_structure(&pdf, &EngineLimits::default()).unwrap();

        assert!(report.tagged);
        assert_eq!(report.role_map_count, 2);
        assert_eq!(report.mcid_refs, 3); // two in /P, one in /H1
        assert_eq!(report.elements.len(), 3);

        let p = &report.elements[0];
        assert_eq!(p.role, "P");
        assert_eq!(p.alt_text.as_deref(), Some("A paragraph of text"));
        assert_eq!(p.page, Some(1));

        let h1 = &report.elements[1];
        assert_eq!(h1.role, "H1");
        assert_eq!(h1.alt_text, None);
        assert_eq!(h1.page, Some(1));

        let note = &report.elements[2];
        assert_eq!(note.role, "Note");
        // /Pg references the /Pages node, not a page — unmapped.
        assert_eq!(note.page, None);

        assert!(report.warnings.is_empty());
    }

    #[test]
    fn garbage_input_is_error_not_panic() {
        assert!(inspect_structure(b"not a pdf", &EngineLimits::default()).is_err());
    }

    #[test]
    fn marked_without_struct_tree_root_warns() {
        let mut doc = Document::with_version("1.7");
        let pages_id = doc.new_object_id();
        doc.objects.insert(
            pages_id,
            Object::Dictionary(dictionary! {
                "Type" => "Pages",
                "Kids" => Vec::<Object>::new(),
                "Count" => 0,
            }),
        );
        let catalog_id = doc.add_object(dictionary! {
            "Type" => "Catalog",
            "Pages" => pages_id,
            "MarkInfo" => dictionary! { "Marked" => true },
        });
        doc.trailer.set("Root", catalog_id);
        let mut buf = Vec::new();
        doc.save_to(&mut buf).unwrap();

        let report = inspect_structure(&buf, &EngineLimits::default()).unwrap();
        assert!(report.tagged);
        assert_eq!(report.elements.len(), 0);
        assert_eq!(report.warnings.len(), 1);
    }

    #[test]
    fn depth_cap_truncates_without_panicking() {
        // Build a chain of nested single-element /K dicts deeper than
        // MAX_DEPTH — every node is a direct dictionary (no object
        // indirection) so lopdf accepts it.
        let mut k = Object::Integer(0);
        for i in 0..(MAX_DEPTH + 10) {
            k = Object::Dictionary(dictionary! {
                "S" => format!("S{i}"),
                "K" => k,
            });
        }
        let mut doc = Document::with_version("1.7");
        let pages_id = doc.new_object_id();
        doc.objects.insert(
            pages_id,
            Object::Dictionary(dictionary! {
                "Type" => "Pages",
                "Kids" => Vec::<Object>::new(),
                "Count" => 0,
            }),
        );
        let root_id = doc.add_object(dictionary! {
            "Type" => "StructTreeRoot",
            "K" => k,
        });
        let catalog_id = doc.add_object(dictionary! {
            "Type" => "Catalog",
            "Pages" => pages_id,
            "MarkInfo" => dictionary! { "Marked" => true },
            "StructTreeRoot" => root_id,
        });
        doc.trailer.set("Root", catalog_id);
        let mut buf = Vec::new();
        doc.save_to(&mut buf).unwrap();

        let report = inspect_structure(&buf, &EngineLimits::default()).unwrap();
        assert!(report.tagged);
        assert_eq!(report.elements.len(), MAX_DEPTH);
        assert_eq!(report.mcid_refs, 0);
        assert!(report.warnings.iter().any(|w| w.contains("deeper than")));
    }
}
