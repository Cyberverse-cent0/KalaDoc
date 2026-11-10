//! Link annotations (`/Subtype /Link`) — clickable rectangles on pages.
//!
//! Three operations, all operating on lopdf dictionaries:
//!
//! - [`add_links`] — append link annotations. URI links carry
//!   `/A << /S /URI /URI (…) >>`; internal links carry `/Dest [page /Fit …]`
//!   (the `FitMode` family shared with named destinations). Every link
//!   gets `/Border [0 0 0]` so viewers do not draw a default border.
//! - [`list_links`] — walk every page's `/Annots`, keep `/Subtype /Link`
//!   entries, and report page, rect, action kind and a human-readable
//!   target string.
//! - [`remove_links`] — delete only `/Link` annotations (other
//!   annotation types are untouched) on the selected pages; delegates to
//!   [`annotations::remove_annotations`] with a subtype filter.
//!
//! # Security notes
//!
//! URI targets are validated before being written: only `http`, `https`
//! and `mailto` schemes or relative references are accepted. Anything
//! with another scheme — most importantly `javascript:` — is rejected
//! with [`PdfError::InvalidInput`]. Control characters are rejected so a
//! URI cannot smuggle line breaks into the PDF string.
//!
//! # Honesty notes / limitations
//!
//! - `/Annots` arrays that are indirect objects are not mutated in
//!   place: the page dictionary is pointed at a fresh direct array and
//!   the old object is left unreferenced (same trade-off as
//!   `annotations`/`flatten`).
//! - GoTo `/Dest` arrays are written inline on the annotation. Existing
//!   documents may also express links via `/A << /S /GoTo /D … >>`,
//!   `/Dest` names, or `/A << /S /URI >>` — [`list_links`] tolerates all
//!   of these when reading.
//! - `list_links` is read-only and tolerates encrypted input the same
//!   way `list_annotations` does; `add_links`/`remove_links` reject
//!   encrypted documents via [`reject_encrypted`].
//! - `/QuadPoints`, `/A` action dictionaries other than `/URI`/`/GoTo*`
//!   and `/StructParent` are ignored when reading; `/Dest`-only and
//!   action-less link annotations still list with an empty target.

use std::collections::BTreeMap;
use std::collections::HashMap;

use lopdf::{Document, Object, ObjectId, decode_text_string, dictionary};
use serde::{Deserialize, Serialize};

use crate::crates::core::engine::EngineLimits;

use crate::crates::pdf_engine::error::{PdfError, PdfResult};

use super::annotations::{AnnotFilter, remove_annotations};
use super::crop::number_object;
use super::load::{load_pdf, lopdf_err, reject_encrypted, save_pdf};
use super::named_destinations::FitMode;

/// Maximum number of link annotations a single [`add_links`] call may
/// carry.
const MAX_LINKS: usize = 100_000;

/// Maximum URI byte length accepted by [`add_links`].
const MAX_URI_BYTES: usize = 8192;

/// Maximum destination-name indirections followed when listing.
const MAX_DEST_DEPTH: u32 = 8;

/// Specification for one link annotation, as accepted by [`add_links`].
/// `page` is **1-based** and must exist in the document.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct LinkSpec {
    /// Target page the annotation sits on, 1-based.
    pub page: u32,
    /// Clickable rectangle `[x0, y0, x1, y1]` in default user space.
    /// Corners may be given in either order; they are normalized so
    /// `x0 <= x1` and `y0 <= y1`. Must have non-zero width and height.
    pub rect: [f64; 4],
    /// Where the link points.
    pub target: LinkTarget,
}

/// Where a link annotation points.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub enum LinkTarget {
    /// External URI — written as `/A << /S /URI /URI (…) >>`.
    /// See [`validate_uri`] for the accepted schemes.
    Uri(String),
    /// Internal go-to — written as `/Dest [page_ref /Fit …]`.
    /// `page` is 1-based and must exist; `fit` reuses the
    /// [`FitMode`] family shared with named destinations.
    GoTo { page: u32, fit: FitMode },
}

/// One row returned by [`list_links`].
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct LinkInfo {
    /// 1-based page number the annotation sits on.
    pub page: u32,
    /// `/Rect` as `[x0, y0, x1, y1]` (zeros when absent/malformed).
    pub rect: [f64; 4],
    /// Action kind: the `/A` `/S` subtype (`"URI"`, `"GoTo"`,
    /// `"GoToR"`, `"Named"`, …), `"Dest"` for a bare `/Dest` entry, or
    /// `"Link"` when neither is present.
    pub kind: String,
    /// Human-readable target: the decoded URI for URI actions, or a
    /// `"page N"`/`"named <name>"` description for internal targets.
    /// Empty when the target cannot be resolved.
    pub target: String,
}

// ---------------------------------------------------------------------------
// add_links
// ---------------------------------------------------------------------------

/// Append `/Subtype /Link` annotations to `input` and return the
/// rewritten PDF.
///
/// Every link is validated before the document is modified: page
/// numbers must exist ([`PdfError::InvalidPageRange`]), rects must be
/// finite with non-zero extent ([`PdfError::InvalidInput`]), URI targets
/// must use an allowed scheme and contain no control characters
/// ([`PdfError::InvalidInput`]), and GoTo fit coordinates must be
/// finite. `links.len()` is capped at [`MAX_LINKS`].
pub fn add_links(input: &[u8], links: &[LinkSpec], limits: &EngineLimits) -> PdfResult<Vec<u8>> {
    if links.len() > MAX_LINKS {
        return Err(PdfError::LimitExceeded(format!(
            "{} links supplied, exceeding the {MAX_LINKS} cap",
            links.len()
        )));
    }

    let mut doc = load_pdf(input, limits)?;
    reject_encrypted(&doc)?;

    let pages_map = doc.get_pages();
    let page_count = pages_map.len() as u32;

    // Validate everything up front; group by page so each page's
    // /Annots array is read and rewritten only once.
    let mut by_page: BTreeMap<u32, Vec<(usize, [f64; 4])>> = BTreeMap::new();
    let mut targets: Vec<LinkTarget> = Vec::with_capacity(links.len());
    for (i, link) in links.iter().enumerate() {
        if link.page == 0 || link.page > page_count {
            return Err(PdfError::InvalidPageRange(format!(
                "link {} targets page {}, but the document has {} page(s)",
                i + 1,
                link.page,
                page_count
            )));
        }
        let rect = checked_rect(link.rect)?;
        let target = validated_target(&link.target)?;
        targets.push(target);
        by_page.entry(link.page).or_default().push((i, rect));
    }

    // Page objects must be looked up after validation; `pages_map` borrows
    // `doc` immutably so re-fetch per group.
    for (page_no, group) in by_page {
        let Some(&page_id) = pages_map.get(&page_no) else {
            return Err(PdfError::InvalidPageRange(format!(
                "link targets page {}, but the document has {} page(s)",
                page_no, page_count
            )));
        };
        for (spec_idx, rect) in group {
            let annot_id = build_link_annot(&mut doc, page_id, rect, &targets[spec_idx])?;
            append_annot(&mut doc, page_id, Object::Reference(annot_id))?;
        }
    }

    save_pdf(&mut doc)
}

/// Validate a [`LinkTarget`] and return it unchanged. Split out so all
/// input checks happen before the document is touched.
fn validated_target(target: &LinkTarget) -> PdfResult<LinkTarget> {
    match target {
        LinkTarget::Uri(uri) => {
            validate_uri(uri)?;
            Ok(LinkTarget::Uri(uri.clone()))
        }
        LinkTarget::GoTo { page, fit } => {
            if !fit_is_finite(fit) {
                return Err(PdfError::InvalidInput(
                    "link GoTo fit has a non-finite coordinate".to_string(),
                ));
            }
            if *page == 0 {
                return Err(PdfError::InvalidInput(
                    "link GoTo target page is 1-based; 0 is not valid".to_string(),
                ));
            }
            Ok(LinkTarget::GoTo {
                page: *page,
                fit: *fit,
            })
        }
    }
}

/// Accept `http`, `https` and `mailto` URIs plus relative references;
/// reject every other scheme — above all `javascript:` — and any URI
/// containing ASCII control characters.
fn validate_uri(uri: &str) -> PdfResult<()> {
    let trimmed = uri.trim();
    if trimmed.is_empty() {
        return Err(PdfError::InvalidInput(
            "link URI must not be empty".to_string(),
        ));
    }
    if trimmed.len() > MAX_URI_BYTES {
        return Err(PdfError::InvalidInput(format!(
            "link URI exceeds {MAX_URI_BYTES} bytes"
        )));
    }
    if trimmed.chars().any(|c| c.is_control()) {
        return Err(PdfError::InvalidInput(
            "link URI must not contain control characters".to_string(),
        ));
    }

    // A scheme is `ALPHA *( ALPHA / DIGIT / "+" / "-" / "." ) ":"` and can
    // only appear before any `/`, `?` or `#`. Anything else is a
    // relative reference.
    let scheme_end = trimmed.find(':');
    if let Some(end) = scheme_end {
        let before = &trimmed[..end];
        let looks_like_scheme = before
            .bytes()
            .next()
            .is_some_and(|b| b.is_ascii_alphabetic())
            && before
                .bytes()
                .all(|b| b.is_ascii_alphanumeric() || matches!(b, b'+' | b'-' | b'.'))
            && !before.contains('/')
            && !before.contains('?')
            && !before.contains('#');
        if looks_like_scheme {
            let scheme = before.to_ascii_lowercase();
            if !matches!(scheme.as_str(), "http" | "https" | "mailto") {
                return Err(PdfError::InvalidInput(format!(
                    "link URI scheme \"{scheme}:\" is not allowed; \
                     only http, https and mailto URIs or relative \
                     references are supported"
                )));
            }
        }
    }
    Ok(())
}

/// Whether every coordinate carried by `fit` is finite.
fn fit_is_finite(fit: &FitMode) -> bool {
    match *fit {
        FitMode::Fit | FitMode::FitB => true,
        FitMode::FitH(y) | FitMode::FitV(y) | FitMode::FitBH(y) | FitMode::FitBV(y) => {
            y.is_finite()
        }
        FitMode::FitR(rect) => rect.iter().all(|v| v.is_finite()),
    }
}

/// The `/Fit`-family operands after the page reference, as in
/// `named_destinations::fit_tail` (re-implemented to stay private).
fn fit_tail(fit: FitMode) -> Vec<Object> {
    let fit_name = |s: &[u8]| Object::Name(s.to_vec());
    match fit {
        FitMode::Fit => vec![fit_name(b"Fit")],
        FitMode::FitH(y) => vec![fit_name(b"FitH"), number_object(y)],
        FitMode::FitV(x) => vec![fit_name(b"FitV"), number_object(x)],
        FitMode::FitR(rect) => {
            let mut v = vec![fit_name(b"FitR")];
            v.extend(rect.iter().map(|&n| number_object(n)));
            v
        }
        FitMode::FitB => vec![fit_name(b"FitB")],
        FitMode::FitBH(y) => vec![fit_name(b"FitBH"), number_object(y)],
        FitMode::FitBV(x) => vec![fit_name(b"FitBV"), number_object(x)],
    }
}

/// Build the `/Dest [page_ref /Fit …]` array for an internal link.
fn build_dest_array(page_id: ObjectId, fit: FitMode) -> Object {
    let mut items = vec![Object::Reference(page_id)];
    items.extend(fit_tail(fit));
    Object::Array(items)
}

/// Create the link annotation dictionary as an indirect object.
fn build_link_annot(
    doc: &mut Document,
    _page_id: ObjectId,
    rect: [f64; 4],
    target: &LinkTarget,
) -> PdfResult<ObjectId> {
    let mut dict = dictionary! {
        "Type" => "Annot",
        "Subtype" => Object::Name(b"Link".to_vec()),
        "Rect" => rect.iter().map(|v| number_object(*v)).collect::<Vec<_>>(),
        "Border" => vec![
            number_object(0.0),
            number_object(0.0),
            number_object(0.0),
        ],
    };
    match target {
        LinkTarget::Uri(uri) => {
            let mut action = lopdf::Dictionary::new();
            action.set("S", Object::Name(b"URI".to_vec()));
            action.set("URI", Object::string_literal(uri.as_str()));
            dict.set("A", Object::Dictionary(action));
        }
        LinkTarget::GoTo { page, fit } => {
            // Resolve the 1-based target page to its object id. The
            // caller validated the page exists; re-check defensively.
            let Some(&dest_page_id) = doc.get_pages().get(page) else {
                return Err(PdfError::InvalidPageRange(format!(
                    "link targets page {page}, but the document has {} page(s)",
                    doc.get_pages().len()
                )));
            };
            dict.set("Dest", build_dest_array(dest_page_id, *fit));
        }
    }
    doc.try_add_object(Object::Dictionary(dict))
        .map_err(lopdf_err)
}

// ---------------------------------------------------------------------------
// Shared /Annots helpers (private copies of the annotations.rs ones —
// those are module-private)
// ---------------------------------------------------------------------------

/// Parse a `/Rect`-style 4-number array into `[x0, y0, x1, y1]`.
fn rect_from_object(obj: &Object) -> Option<[f64; 4]> {
    let items = obj.as_array().ok()?;
    if items.len() != 4 {
        return None;
    }
    let mut out = [0.0; 4];
    for (i, item) in items.iter().enumerate() {
        out[i] = item.as_float().ok()? as f64;
    }
    Some(out)
}

/// Reject non-finite or degenerate rects; normalize so x0<=x1, y0<=y1.
fn checked_rect(rect: [f64; 4]) -> PdfResult<[f64; 4]> {
    if !rect.iter().all(|v| v.is_finite()) {
        return Err(PdfError::InvalidInput(
            "link rect must contain only finite numbers".to_string(),
        ));
    }
    let mut out = rect;
    if out[0] > out[2] {
        out.swap(0, 2);
    }
    if out[1] > out[3] {
        out.swap(1, 3);
    }
    if out[0] == out[2] || out[1] == out[3] {
        return Err(PdfError::InvalidInput(
            "link rect must have non-zero width and height".to_string(),
        ));
    }
    Ok(out)
}

/// Read a page's `/Annots` array as cloned [`Object`]s — the array may
/// be direct or a single indirection, and entries may be direct
/// dictionaries or references.
fn annots_entries(doc: &Document, page_id: ObjectId) -> Vec<Object> {
    let Ok(page) = doc.get_dictionary(page_id) else {
        return Vec::new();
    };
    let Ok(annots) = page.get(b"Annots") else {
        return Vec::new();
    };
    let array = match annots {
        Object::Array(items) => Some(items.clone()),
        Object::Reference(id) => doc
            .get_object(*id)
            .ok()
            .and_then(|o| o.as_array().ok())
            .cloned(),
        _ => None,
    };
    array.unwrap_or_default()
}

/// Resolve an `/Annots` entry to a dictionary (direct or referenced).
fn resolve_dict<'a>(doc: &'a Document, entry: &'a Object) -> Option<&'a lopdf::Dictionary> {
    match entry {
        Object::Dictionary(d) => Some(d),
        Object::Reference(id) => doc.get_dictionary(*id).ok(),
        _ => None,
    }
}

/// Decode a `/Contents`- or `/URI`-style text string.
fn decode_text_object(obj: &Object) -> Option<String> {
    match obj {
        Object::String(..) => decode_text_string(obj).ok(),
        Object::Array(items) => {
            let mut out = String::new();
            for item in items {
                match item {
                    Object::String(..) => out.push_str(&decode_text_string(item).ok()?),
                    _ => return None,
                }
            }
            Some(out)
        }
        _ => None,
    }
}

/// Append `entry` to the page's `/Annots` array, creating it when
/// absent. Any pre-existing array (direct or indirect) is preserved by
/// cloning its entries into a fresh direct array.
fn append_annot(doc: &mut Document, page_id: ObjectId, entry: Object) -> PdfResult<()> {
    let mut entries = annots_entries(doc, page_id);
    entries.push(entry);
    let page = doc.get_dictionary_mut(page_id).map_err(lopdf_err)?;
    page.set("Annots", Object::Array(entries));
    Ok(())
}

// ---------------------------------------------------------------------------
// list_links
// ---------------------------------------------------------------------------

/// Walk every page's `/Annots` and return one [`LinkInfo`] per
/// `/Subtype /Link` annotation, in page order then array order.
///
/// Read-only: encrypted input is tolerated (strings may fail to decode,
/// in which case `target` is empty). Non-dictionary `/Annots` entries
/// and non-Link subtypes are skipped.
pub fn list_links(input: &[u8], limits: &EngineLimits) -> PdfResult<Vec<LinkInfo>> {
    let doc = load_pdf(input, limits)?;

    let pages_map = doc.get_pages();
    let page_ids: HashMap<ObjectId, u32> = pages_map.iter().map(|(&n, &id)| (id, n)).collect();

    let mut out = Vec::new();
    for (page_no, page_id) in &pages_map {
        for entry in annots_entries(&doc, *page_id) {
            let Some(annot) = resolve_dict(&doc, &entry) else {
                continue;
            };
            let is_link = annot
                .get(b"Subtype")
                .ok()
                .and_then(|o| o.as_name().ok())
                .is_some_and(|n| n.eq_ignore_ascii_case(b"Link"));
            if !is_link {
                continue;
            }
            let rect = annot
                .get(b"Rect")
                .ok()
                .and_then(rect_from_object)
                .unwrap_or([0.0, 0.0, 0.0, 0.0]);
            let (kind, target) = describe_target(&doc, annot, &page_ids);
            out.push(LinkInfo {
                page: *page_no,
                rect,
                kind,
                target,
            });
        }
    }
    Ok(out)
}

/// Extract `(kind, target)` for a link annotation: prefer the `/A`
/// action dictionary, then a bare `/Dest` entry, then fall back to a
/// kind-less report.
fn describe_target(
    doc: &Document,
    annot: &lopdf::Dictionary,
    page_ids: &HashMap<ObjectId, u32>,
) -> (String, String) {
    if let Ok(action_obj) = annot.get(b"A") {
        let resolved = doc
            .dereference(action_obj)
            .map(|(_, o)| o)
            .unwrap_or(action_obj);
        if let Ok(action) = resolved.as_dict() {
            let subtype = action
                .get(b"S")
                .ok()
                .and_then(|o| o.as_name().ok())
                .map(|n| String::from_utf8_lossy(n).into_owned())
                .unwrap_or_else(|| "Action".to_string());
            let target = match subtype.as_str() {
                "URI" => action
                    .get(b"URI")
                    .ok()
                    .and_then(decode_text_object)
                    .unwrap_or_default(),
                "GoTo" | "GoToR" => action
                    .get(b"D")
                    .ok()
                    .map(|d| describe_dest(doc, d, page_ids, 0))
                    .unwrap_or_default(),
                _ => String::new(),
            };
            return (subtype, target);
        }
    }
    if let Ok(dest) = annot.get(b"Dest") {
        return ("Dest".to_string(), describe_dest(doc, dest, page_ids, 0));
    }
    ("Link".to_string(), String::new())
}

/// Turn a destination object (`/Dest` value or `/A` `/D`) into a
/// human-readable string: `"page N"` for page references, `"named
/// <name>"` for named destinations, `""` for anything unresolvable.
fn describe_dest(
    doc: &Document,
    dest: &Object,
    page_ids: &HashMap<ObjectId, u32>,
    depth: u32,
) -> String {
    if depth > MAX_DEST_DEPTH {
        return String::new();
    }
    let Ok((_, resolved)) = doc.dereference(dest) else {
        return String::new();
    };
    match resolved {
        Object::Array(items) => {
            let Some(first) = items.first() else {
                return String::new();
            };
            match first {
                Object::Reference(id) => page_ids
                    .get(id)
                    .map(|n| format!("page {n}"))
                    .unwrap_or_default(),
                Object::Integer(n) if *n >= 1 => format!("page {n}"),
                Object::String(..) | Object::Name(_) => named_dest_label(doc, first)
                    .map(|s| format!("named {s}"))
                    .unwrap_or_default(),
                _ => String::new(),
            }
        }
        Object::String(..) | Object::Name(_) => named_dest_label(doc, resolved)
            .map(|s| format!("named {s}"))
            .unwrap_or_default(),
        Object::Dictionary(dict) => dict
            .get(b"D")
            .ok()
            .map(|d| describe_dest(doc, d, page_ids, depth + 1))
            .unwrap_or_default(),
        _ => String::new(),
    }
}

/// Decode a destination name object (string or name) to text.
fn named_dest_label(doc: &Document, obj: &Object) -> Option<String> {
    let (_, resolved) = doc.dereference(obj).ok()?;
    match resolved {
        Object::String(..) => decode_text_string(resolved).ok().or_else(|| {
            resolved
                .as_str()
                .ok()
                .map(|b| String::from_utf8_lossy(b).into_owned())
        }),
        Object::Name(bytes) => Some(String::from_utf8_lossy(bytes).into_owned()),
        _ => None,
    }
}

// ---------------------------------------------------------------------------
// remove_links
// ---------------------------------------------------------------------------

/// Remove only `/Subtype /Link` annotations from `input` and return the
/// rewritten PDF.
///
/// `pages` is an optional 1-based range spec (`"1-3,8"`, `"all"`);
/// `None` selects every page. Annotations of other subtypes — and
/// non-dictionary `/Annots` entries — are never removed. Delegates to
/// [`annotations::remove_annotations`], so an empty `/Annots` array is
/// dropped from the page dictionary entirely.
pub fn remove_links(
    input: &[u8],
    pages: Option<&str>,
    limits: &EngineLimits,
) -> PdfResult<Vec<u8>> {
    let filter = AnnotFilter {
        pages: pages.map(str::to_string),
        subtypes: Some(vec!["Link".to_string()]),
        all: false,
    };
    remove_annotations(input, &filter, limits)
}

// ---------------------------------------------------------------------------
// tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use lopdf::{Document, Object};

    use super::*;
    use crate::crates::pdf_engine::ops::test_utils::pdf_with_pages;

    fn limits() -> EngineLimits {
        EngineLimits::default()
    }

    fn spec(page: u32, rect: [f64; 4], target: LinkTarget) -> LinkSpec {
        LinkSpec { page, rect, target }
    }

    fn annots_of(doc: &Document, page: u32) -> Vec<lopdf::Dictionary> {
        let page_id = doc.get_pages()[&page];
        let page_dict = doc.get_dictionary(page_id).unwrap();
        let Ok(annots) = page_dict.get(b"Annots") else {
            return Vec::new();
        };
        let items = match annots {
            Object::Array(items) => items.clone(),
            Object::Reference(id) => doc.get_object(*id).unwrap().as_array().unwrap().clone(),
            _ => panic!("Annots is not an array"),
        };
        items
            .iter()
            .map(|entry| match entry {
                Object::Reference(id) => doc.get_dictionary(*id).unwrap().clone(),
                Object::Dictionary(d) => d.clone(),
                _ => panic!("annot entry is not a dict"),
            })
            .collect()
    }

    #[test]
    fn add_uri_links_and_list() {
        let input = pdf_with_pages(&["a", "b"]);
        let links = vec![
            spec(
                1,
                [10.0, 10.0, 110.0, 40.0],
                LinkTarget::Uri("https://example.com".to_string()),
            ),
            spec(
                2,
                [50.0, 700.0, 200.0, 730.0],
                LinkTarget::Uri("mailto:a@b.c".to_string()),
            ),
            // Relative reference is allowed.
            spec(
                2,
                [50.0, 650.0, 200.0, 680.0],
                LinkTarget::Uri("../other.pdf#page=2".to_string()),
            ),
        ];
        let out = add_links(&input, &links, &limits()).unwrap();
        let doc = Document::load_mem(&out).unwrap();

        // Structural check: /Subtype /Link, /Border [0 0 0], /A /URI.
        let annots = annots_of(&doc, 1);
        assert_eq!(annots.len(), 1);
        assert_eq!(
            annots[0].get(b"Subtype").unwrap().as_name().unwrap(),
            b"Link".as_slice()
        );
        let border = annots[0].get(b"Border").unwrap().as_array().unwrap();
        assert_eq!(border.len(), 3);
        let action = annots[0].get(b"A").unwrap().as_dict().unwrap();
        assert_eq!(
            action.get(b"S").unwrap().as_name().unwrap(),
            b"URI".as_slice()
        );
        let uri = decode_text_string(action.get(b"URI").unwrap()).unwrap();
        assert_eq!(uri, "https://example.com");

        // Listing round-trip.
        let infos = list_links(&out, &limits()).unwrap();
        assert_eq!(infos.len(), 3);
        assert_eq!(infos[0].page, 1);
        assert_eq!(infos[0].kind, "URI");
        assert_eq!(infos[0].target, "https://example.com");
        assert_eq!(infos[0].rect, [10.0, 10.0, 110.0, 40.0]);
        assert_eq!(infos[1].target, "mailto:a@b.c");
        assert_eq!(infos[2].target, "../other.pdf#page=2");
    }

    #[test]
    fn add_goto_link_writes_dest() {
        let input = pdf_with_pages(&["a", "b", "c"]);
        let links = vec![spec(
            1,
            [0.0, 0.0, 100.0, 20.0],
            LinkTarget::GoTo {
                page: 3,
                fit: FitMode::FitH(120.0),
            },
        )];
        let out = add_links(&input, &links, &limits()).unwrap();
        let doc = Document::load_mem(&out).unwrap();
        let annots = annots_of(&doc, 1);
        assert_eq!(annots.len(), 1);
        let dest = annots[0].get(b"Dest").unwrap().as_array().unwrap();
        assert_eq!(dest.len(), 3); // [page /FitH y]
        assert_eq!(
            dest[0],
            Object::Reference(doc.get_pages()[&3]),
            "Dest must point at page 3's object"
        );
        assert_eq!(dest[1].as_name().unwrap(), b"FitH".as_slice());

        let infos = list_links(&out, &limits()).unwrap();
        assert_eq!(infos.len(), 1);
        assert_eq!(infos[0].kind, "Dest");
        assert_eq!(infos[0].target, "page 3");
    }

    #[test]
    fn add_preserves_existing_annots() {
        use crate::crates::pdf_engine::ops::annotations::{CommentSpec, add_comment};
        let input = pdf_with_pages(&["a"]);
        let comment = CommentSpec {
            text: "note".to_string(),
            pages: None,
            rect: None,
            author: None,
            icon: None,
        };
        let pdf = add_comment(&input, &comment, &limits()).unwrap();
        let links = vec![spec(
            1,
            [0.0, 0.0, 50.0, 20.0],
            LinkTarget::Uri("https://x.y".to_string()),
        )];
        let out = add_links(&pdf, &links, &limits()).unwrap();
        let infos = list_links(&out, &limits()).unwrap();
        assert_eq!(infos.len(), 1);
        // list_annotations should still see the Text annot plus the Link.
        let annots = list_annotations_count(&out);
        assert_eq!(annots, 2);
    }

    fn list_annotations_count(pdf: &[u8]) -> usize {
        crate::crates::pdf_engine::ops::list_annotations(pdf, &limits())
            .unwrap()
            .len()
    }

    #[test]
    fn remove_links_only_removes_links() {
        use crate::crates::pdf_engine::ops::annotations::{CommentSpec, add_comment};
        let input = pdf_with_pages(&["a", "b"]);
        let comment = CommentSpec {
            text: "sticky".to_string(),
            pages: None,
            rect: None,
            author: None,
            icon: None,
        };
        let mut pdf = add_comment(&input, &comment, &limits()).unwrap();
        let links = vec![
            spec(
                1,
                [0.0, 0.0, 50.0, 20.0],
                LinkTarget::Uri("https://a.b".to_string()),
            ),
            spec(
                2,
                [0.0, 0.0, 50.0, 20.0],
                LinkTarget::Uri("https://c.d".to_string()),
            ),
        ];
        pdf = add_links(&pdf, &links, &limits()).unwrap();
        assert_eq!(list_links(&pdf, &limits()).unwrap().len(), 2);

        // Remove links only on page 1.
        let out = remove_links(&pdf, Some("1"), &limits()).unwrap();
        let infos = list_links(&out, &limits()).unwrap();
        assert_eq!(infos.len(), 1);
        assert_eq!(infos[0].page, 2);
        // The Text comments survive everywhere.
        let annots = crate::crates::pdf_engine::ops::list_annotations(&out, &limits()).unwrap();
        assert_eq!(annots.len(), 2);
        assert!(annots.iter().all(|a| a.subtype == "Text"));

        // Remove the rest.
        let out = remove_links(&out, None, &limits()).unwrap();
        assert!(list_links(&out, &limits()).unwrap().is_empty());
        let doc = Document::load_mem(&out).unwrap();
        for (_, page_id) in doc.get_pages() {
            let page = doc.get_dictionary(page_id).unwrap();
            let annots = page.get(b"Annots").unwrap().as_array().unwrap();
            assert!(
                annots.iter().all(|e| {
                    resolve_dict_test(&doc, e)
                        .and_then(|d| d.get(b"Subtype").ok())
                        .and_then(|o| o.as_name().ok())
                        .is_some_and(|n| n != b"Link")
                }),
                "no Link annots should remain"
            );
        }
    }

    fn resolve_dict_test<'a>(
        doc: &'a Document,
        entry: &'a Object,
    ) -> Option<&'a lopdf::Dictionary> {
        match entry {
            Object::Dictionary(d) => Some(d),
            Object::Reference(id) => doc.get_dictionary(*id).ok(),
            _ => None,
        }
    }

    #[test]
    fn rejects_javascript_and_bad_schemes() {
        let input = pdf_with_pages(&["a"]);
        for uri in [
            "javascript:alert(1)",
            "JavaScript:alert(1)",
            "data:text/html,<script>x</script>",
            "file:///etc/passwd",
            "vbscript:msgbox(1)",
        ] {
            let links = vec![spec(
                1,
                [0.0, 0.0, 10.0, 10.0],
                LinkTarget::Uri(uri.to_string()),
            )];
            assert!(
                matches!(
                    add_links(&input, &links, &limits()),
                    Err(PdfError::InvalidInput(_))
                ),
                "URI {uri:?} must be rejected"
            );
        }
        // Empty and control-char URIs rejected too.
        for uri in ["", "   ", "https://ok.com\n.evil"] {
            let links = vec![spec(
                1,
                [0.0, 0.0, 10.0, 10.0],
                LinkTarget::Uri(uri.to_string()),
            )];
            assert!(add_links(&input, &links, &limits()).is_err());
        }
    }

    #[test]
    fn validates_rect_and_pages() {
        let input = pdf_with_pages(&["a", "b"]);
        // Non-finite rect.
        let links = vec![spec(
            1,
            [f64::NAN, 0.0, 1.0, 1.0],
            LinkTarget::Uri("https://x.y".to_string()),
        )];
        assert!(matches!(
            add_links(&input, &links, &limits()),
            Err(PdfError::InvalidInput(_))
        ));
        // Degenerate rect.
        let links = vec![spec(
            1,
            [1.0, 1.0, 1.0, 5.0],
            LinkTarget::Uri("https://x.y".to_string()),
        )];
        assert!(matches!(
            add_links(&input, &links, &limits()),
            Err(PdfError::InvalidInput(_))
        ));
        // Out-of-range pages: annotation page and GoTo target.
        let links = vec![spec(
            9,
            [0.0, 0.0, 1.0, 1.0],
            LinkTarget::Uri("https://x.y".to_string()),
        )];
        assert!(matches!(
            add_links(&input, &links, &limits()),
            Err(PdfError::InvalidPageRange(_))
        ));
        let links = vec![spec(
            1,
            [0.0, 0.0, 1.0, 1.0],
            LinkTarget::GoTo {
                page: 0,
                fit: FitMode::Fit,
            },
        )];
        assert!(add_links(&input, &links, &limits()).is_err());
        // Non-finite fit coordinate.
        let links = vec![spec(
            1,
            [0.0, 0.0, 1.0, 1.0],
            LinkTarget::GoTo {
                page: 2,
                fit: FitMode::FitH(f64::INFINITY),
            },
        )];
        assert!(matches!(
            add_links(&input, &links, &limits()),
            Err(PdfError::InvalidInput(_))
        ));
    }

    #[test]
    fn rejects_encrypted_input() {
        use crate::crates::pdf_engine::ops::encrypt::{self, PdfPermissions};
        let input = pdf_with_pages(&["a"]);
        let encrypted =
            encrypt::encrypt(&input, "user", None, PdfPermissions::default(), &limits()).unwrap();
        let links = vec![spec(
            1,
            [0.0, 0.0, 1.0, 1.0],
            LinkTarget::Uri("https://x.y".to_string()),
        )];
        assert!(matches!(
            add_links(&encrypted, &links, &limits()),
            Err(PdfError::Encrypted)
        ));
        assert!(matches!(
            remove_links(&encrypted, None, &limits()),
            Err(PdfError::Encrypted)
        ));
    }

    #[test]
    fn lists_existing_goto_action_links() {
        // Build a link via /A << /S /GoTo /D [page /Fit] >> manually to
        // exercise the tolerant reader path.
        let mut input = pdf_with_pages(&["a", "b"]);
        {
            let mut doc = Document::load_mem(&input).unwrap();
            let page_id = doc.get_pages()[&1];
            let page2_id = doc.get_pages()[&2];
            let mut dict = lopdf::Dictionary::new();
            dict.set("Type", Object::Name(b"Annot".to_vec()));
            dict.set("Subtype", Object::Name(b"Link".to_vec()));
            dict.set(
                "Rect",
                vec![
                    Object::Real(1.0),
                    Object::Real(2.0),
                    Object::Real(30.0),
                    Object::Real(40.0),
                ],
            );
            let mut action = lopdf::Dictionary::new();
            action.set("S", Object::Name(b"GoTo".to_vec()));
            action.set(
                "D",
                Object::Array(vec![
                    Object::Reference(page2_id),
                    Object::Name(b"Fit".to_vec()),
                ]),
            );
            dict.set("A", Object::Dictionary(action));
            let annot_id = doc.add_object(Object::Dictionary(dict));
            let page = doc.get_dictionary_mut(page_id).unwrap();
            page.set("Annots", vec![Object::Reference(annot_id)]);
            let mut buf = Vec::new();
            doc.save_to(&mut buf).unwrap();
            input = buf;
        }
        let infos = list_links(&input, &limits()).unwrap();
        assert_eq!(infos.len(), 1);
        assert_eq!(infos[0].kind, "GoTo");
        assert_eq!(infos[0].target, "page 2");
        assert_eq!(infos[0].rect, [1.0, 2.0, 30.0, 40.0]);
    }

    #[test]
    fn list_empty_and_malformed_pdf() {
        assert!(list_links(b"not a pdf", &limits()).is_err());
        let input = pdf_with_pages(&["a"]);
        assert!(list_links(&input, &limits()).unwrap().is_empty());
    }
}
