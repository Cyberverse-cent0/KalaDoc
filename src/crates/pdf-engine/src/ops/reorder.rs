//! Reorder the pages of a PDF document.
//!
//! `order` is a strict comma-separated permutation of **all** page
//! numbers, each exactly once (e.g. `"3,1,2"`); ranges and duplicates
//! are rejected. The reorder is applied by rewriting the `/Kids` array
//! of the shared `/Pages` parent node — each page's own `/Parent`
//! entry is left untouched because it already points at that node.
//!
//! Only flat page trees are supported: if the pages do not share a
//! single parent whose `/Kids` are exactly the page objects, the op
//! fails with [`PdfError::UnsupportedOperation`] rather than producing
//! a subtly broken tree.

use std::collections::BTreeSet;

use lopdf::{Object, ObjectId};

use crate::crates::core::engine::EngineLimits;

use crate::crates::pdf_engine::error::{PdfError, PdfResult};

use super::load::{load_pdf, lopdf_err, reject_encrypted, save_pdf};

/// Reorder the pages of `input` according to `order`.
///
/// `order` must list every page number from `1` to the document's
/// page count exactly once, in the desired sequence. Returns the
/// reordered document as a new PDF.
pub fn reorder(input: &[u8], order: &str, limits: &EngineLimits) -> PdfResult<Vec<u8>> {
    let mut doc = load_pdf(input, limits)?;
    reject_encrypted(&doc)?;

    let pages_map = doc.get_pages();
    let page_count = pages_map.len() as u32;
    let order = parse_permutation(order, page_count)?;

    let new_ids: Vec<ObjectId> = order.iter().map(|n| pages_map[n]).collect();

    // All pages must share exactly one indirect /Pages parent whose
    // /Kids are precisely the page objects — anything else is a nested
    // page tree this op does not rewrite.
    let mut parents = BTreeSet::new();
    for id in &new_ids {
        let dict = doc.get_dictionary(*id).map_err(lopdf_err)?;
        let parent = dict
            .get(b"Parent")
            .and_then(Object::as_reference)
            .map_err(|_| {
                PdfError::UnsupportedOperation(
                    "page dictionary has no indirect /Parent; nested page trees are not supported"
                        .to_string(),
                )
            })?;
        parents.insert(parent);
    }
    if parents.len() != 1 {
        return Err(PdfError::UnsupportedOperation(
            "pages do not share a single /Pages parent; nested page trees are not supported"
                .to_string(),
        ));
    }
    let parent_id = *parents.iter().next().unwrap();

    let parent_dict = doc.get_dictionary(parent_id).map_err(lopdf_err)?;
    let kid_ids: BTreeSet<ObjectId> = parent_dict
        .get(b"Kids")
        .and_then(Object::as_array)
        .map_err(|_| {
            PdfError::UnsupportedOperation(
                "page tree root has no /Kids array; nested page trees are not supported"
                    .to_string(),
            )
        })?
        .iter()
        .filter_map(|obj| obj.as_reference().ok())
        .collect();
    let all_ids: BTreeSet<ObjectId> = pages_map.values().copied().collect();
    if kid_ids != all_ids {
        return Err(PdfError::UnsupportedOperation(
            "page tree is nested; reorder supports only a flat /Kids list".to_string(),
        ));
    }

    let new_kids: Vec<Object> = new_ids.iter().copied().map(Object::Reference).collect();
    doc.get_dictionary_mut(parent_id)
        .map_err(lopdf_err)?
        .set("Kids", Object::Array(new_kids));
    // /Count is unchanged — a permutation never adds or removes pages.

    save_pdf(&mut doc)
}

/// Swap two pages in `input`, leaving every other page in place.
///
/// `a` and `b` are 1-based page numbers; both must be in range. `a == b`
/// is accepted and returns an equivalent document. Implemented as a
/// full permutation through [`reorder`], so it inherits the same
/// flat-page-tree limitation.
pub fn swap_pages(input: &[u8], a: u32, b: u32, limits: &EngineLimits) -> PdfResult<Vec<u8>> {
    let mut doc = load_pdf(input, limits)?;
    let page_count = doc.get_pages().len() as u32;
    if a == 0 || a > page_count {
        return Err(PdfError::InvalidPageRange(format!(
            "page {a} is out of range; document has {page_count} page(s)"
        )));
    }
    if b == 0 || b > page_count {
        return Err(PdfError::InvalidPageRange(format!(
            "page {b} is out of range; document has {page_count} page(s)"
        )));
    }
    if a == b {
        return save_pdf(&mut doc);
    }
    let mut order: Vec<u32> = (1..=page_count).collect();
    order.swap((a - 1) as usize, (b - 1) as usize);
    let spec = order
        .iter()
        .map(u32::to_string)
        .collect::<Vec<_>>()
        .join(",");
    reorder(input, &spec, limits)
}

/// Parse `order` into the requested sequence of 1-based page numbers.
///
/// Strict grammar: digits and commas only — no ranges, no `all`, no
/// signs. The result must be a permutation of `1..=page_count`.
fn parse_permutation(order: &str, page_count: u32) -> PdfResult<Vec<u32>> {
    let order = order.trim();
    if order.is_empty() {
        return Err(PdfError::InvalidPageRange(
            "empty page order; expected a permutation like \"3,1,2\"".to_string(),
        ));
    }
    let mut seen = BTreeSet::new();
    let mut parsed = Vec::new();
    for part in order.split(',') {
        let part = part.trim();
        if part.is_empty() || !part.bytes().all(|b| b.is_ascii_digit()) {
            return Err(PdfError::InvalidPageRange(format!(
                "invalid page order \"{order}\": expected comma-separated page numbers"
            )));
        }
        let n = part.parse::<u32>().map_err(|_| {
            PdfError::InvalidPageRange(format!("page number \"{part}\" is too large"))
        })?;
        if n == 0 || n > page_count {
            return Err(PdfError::InvalidPageRange(format!(
                "page {n} is out of range; document has {page_count} page(s)"
            )));
        }
        if !seen.insert(n) {
            return Err(PdfError::InvalidPageRange(format!(
                "page {n} appears more than once in order \"{order}\""
            )));
        }
        parsed.push(n);
    }
    if seen.len() as u32 != page_count {
        return Err(PdfError::InvalidPageRange(format!(
            "order \"{order}\" must list every page 1..={page_count} exactly once"
        )));
    }
    Ok(parsed)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::crates::pdf_engine::ops::test_utils::{
        page_count as test_page_count, pdf_with_pages,
    };
    use lopdf::Document;

    /// Text drawn on page `n` (1-based) of a serialized PDF.
    fn page_text(pdf: &[u8], n: u32) -> String {
        let doc = Document::load_mem(pdf).unwrap();
        let page_id = doc.get_pages()[&n];
        String::from_utf8_lossy(&doc.get_page_content(page_id)).into_owned()
    }

    #[test]
    fn reorders_pages() {
        let pdf = pdf_with_pages(&["alpha", "bravo", "charlie"]);
        let out = reorder(&pdf, "3,1,2", &EngineLimits::default()).unwrap();
        assert_eq!(test_page_count(&out), 3);
        assert!(page_text(&out, 1).contains("charlie"));
        assert!(page_text(&out, 2).contains("alpha"));
        assert!(page_text(&out, 3).contains("bravo"));
    }

    #[test]
    fn identity_order_is_valid() {
        let pdf = pdf_with_pages(&["one", "two"]);
        let out = reorder(&pdf, "1,2", &EngineLimits::default()).unwrap();
        assert_eq!(test_page_count(&out), 2);
    }

    #[test]
    fn rejects_bad_permutations() {
        let pdf = pdf_with_pages(&["one", "two", "three"]);
        for order in [
            "",          // empty
            "1,1,2",     // duplicate
            "1,2",       // missing page
            "1-3",       // range syntax not allowed
            "all",       // keyword not allowed
            "0,1,2",     // 0 is not a page
            "4,1,2",     // out of range
            "1,2,three", // non-numeric
            "1,,2,3",    // empty item
        ] {
            let err = reorder(&pdf, order, &EngineLimits::default()).unwrap_err();
            assert!(
                matches!(err, PdfError::InvalidPageRange(_)),
                "order {order:?} should fail with InvalidPageRange, got {err:?}"
            );
        }
        assert!(reorder(b"junk", "1", &EngineLimits::default()).is_err());
    }
}
