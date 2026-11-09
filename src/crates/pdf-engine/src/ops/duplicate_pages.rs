//! Duplicate selected pages within a PDF document.
//!
//! Each selected page is deep-copied: the page dictionary and every
//! object reachable from it (content streams, resource dictionaries,
//! fonts, images, annotations, ...) is cloned into the same document
//! under fresh object ids, with all internal references remapped — the
//! same object-graph copying strategy `merge` uses, applied to a
//! single document. `/Parent` is the one exception: it is *not*
//! traversed during the copy, because it points up the page tree; each
//! copy's `/Parent` is instead set to the `/Pages` node it is inserted
//! under.
//!
//! Insertion reuses the page-tree splicing technique of
//! [`insert_blank`](super::insert_blank): copies are placed as leaves
//! inside the anchor page's parent `/Kids` array, which preserves
//! depth-first page order for nested trees too.

use std::collections::{BTreeMap, HashMap, HashSet};

use lopdf::{Document, Object, ObjectId};

use crate::crates::core::engine::EngineLimits;

use crate::crates::pdf_engine::error::{PdfError, PdfResult};

use super::load::{check_page_limit, load_pdf, lopdf_err, reject_encrypted, save_pdf};
use super::pages::parse_page_range;

/// Where the duplicated pages are inserted.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum DupPos {
    /// Insert the copies immediately after the given 1-based page.
    After(u32),
    /// Insert the copies immediately before the given 1-based page.
    Before(u32),
    /// Insert each page's copies immediately after that page itself,
    /// keeping the copies next to their source.
    InPlace,
    /// Append all copies after the last page.
    End,
}

/// Duplicate the pages selected by `pages` (a 1-based range spec such
/// as `"1-3,8"` or `"all"`), `copies` times each.
///
/// Returns the output PDF bytes together with the sorted 1-based page
/// numbers the copies occupy in the result. `copies` must be at least
/// 1. Fails with [`PdfError::LimitExceeded`] when the result would
/// exceed `max_pages`.
pub fn duplicate_pages(
    input: &[u8],
    pages: &str,
    copies: u32,
    position: DupPos,
    limits: &EngineLimits,
) -> PdfResult<(Vec<u8>, Vec<u32>)> {
    if copies == 0 {
        return Err(PdfError::InvalidInput(
            "copies must be at least 1".to_string(),
        ));
    }

    let mut doc = load_pdf(input, limits)?;
    reject_encrypted(&doc)?;

    let pages_map = doc.get_pages();
    let page_count = pages_map.len() as u32;
    let selected = parse_page_range(pages, page_count)?;

    let added = selected
        .len()
        .checked_mul(copies as usize)
        .ok_or_else(|| PdfError::LimitExceeded("page count overflow".to_string()))?;
    check_page_limit(page_count as usize + added, limits)?;

    // Object ids of every inserted copy, used to report their final
    // 1-based page numbers after the tree mutation.
    let mut inserted: Vec<ObjectId> = Vec::with_capacity(added);

    match position {
        DupPos::InPlace => {
            // Selected pages ascend; each insertion shifts later pages,
            // so resolve every anchor slot fresh from the live tree.
            for &page_num in &selected {
                let source_id = pages_map[&page_num];
                let (parent, index) = page_slot(&doc, source_id)?;
                let copy_ids = duplicate_page(&mut doc, source_id, copies)?;
                set_parents(&mut doc, &copy_ids, parent)?;
                insert_into_kids(&mut doc, parent, index + 1, &copy_ids)?;
                bump_counts(&mut doc, Some(parent), copies as i64)?;
                inserted.extend(copy_ids);
            }
        }
        DupPos::After(n) | DupPos::Before(n) => {
            validate_anchor(n, page_count)?;
            let anchor_id = pages_map[&n];
            let (parent, index) = page_slot(&doc, anchor_id)?;
            let index = if matches!(position, DupPos::After(_)) {
                index + 1
            } else {
                index
            };
            insert_block(
                &mut doc,
                &pages_map,
                &selected,
                copies,
                parent,
                index,
                &mut inserted,
            )?;
        }
        DupPos::End => {
            let (parent, index, _) = end_slot(&doc, &pages_map)?;
            insert_block(
                &mut doc,
                &pages_map,
                &selected,
                copies,
                parent,
                index,
                &mut inserted,
            )?;
        }
    }

    let numbers = page_numbers_of(&doc, &inserted)?;
    let bytes = save_pdf(&mut doc)?;
    Ok((bytes, numbers))
}

/// Report the 1-based page numbers of `ids` in the current tree,
/// sorted ascending.
fn page_numbers_of(doc: &Document, ids: &[ObjectId]) -> PdfResult<Vec<u32>> {
    let wanted: HashSet<ObjectId> = ids.iter().copied().collect();
    let mut numbers: Vec<u32> = doc
        .get_pages()
        .into_iter()
        .filter(|(_, id)| wanted.contains(id))
        .map(|(n, _)| n)
        .collect();
    numbers.sort_unstable();
    Ok(numbers)
}

fn validate_anchor(n: u32, page_count: u32) -> PdfResult<()> {
    if n == 0 || n > page_count {
        return Err(PdfError::InvalidPageRange(format!(
            "page {n} is out of range; document has {page_count} page(s)"
        )));
    }
    Ok(())
}

/// Deep-copy every selected page `copies` times and splice the whole
/// block of copies into `parent`'s `/Kids` at `index`.
fn insert_block(
    doc: &mut Document,
    pages_map: &BTreeMap<u32, ObjectId>,
    selected: &[u32],
    copies: u32,
    parent: ObjectId,
    index: usize,
    inserted: &mut Vec<ObjectId>,
) -> PdfResult<()> {
    let mut copy_ids = Vec::with_capacity(selected.len() * copies as usize);
    for &page_num in selected {
        let ids = duplicate_page(doc, pages_map[&page_num], copies)?;
        set_parents(doc, &ids, parent)?;
        copy_ids.extend(ids);
    }
    insert_into_kids(doc, parent, index, &copy_ids)?;
    bump_counts(doc, Some(parent), copy_ids.len() as i64)?;
    inserted.extend(copy_ids);
    Ok(())
}

/// Point every copied page's `/Parent` at `parent`.
fn set_parents(doc: &mut Document, copy_ids: &[ObjectId], parent: ObjectId) -> PdfResult<()> {
    for id in copy_ids {
        doc.get_dictionary_mut(*id)
            .map_err(lopdf_err)?
            .set("Parent", parent);
    }
    Ok(())
}

/// Deep-copy `source_id` `copies` times. Returns the new page object
/// ids in copy order; `/Parent` entries are left pointing at the
/// source's parent until [`set_parents`] fixes them.
fn duplicate_page(
    doc: &mut Document,
    source_id: ObjectId,
    copies: u32,
) -> PdfResult<Vec<ObjectId>> {
    // Collect the object subgraph reachable from the page, excluding
    // the upward /Parent edge so the page tree itself is never copied.
    let reachable = reachable_objects(doc, source_id)?;
    let reachable: Vec<ObjectId> = reachable.into_iter().collect();

    let mut page_copies = Vec::with_capacity(copies as usize);
    for _ in 0..copies {
        // Fresh ids for every reachable object in this copy.
        let mut id_map: HashMap<ObjectId, ObjectId> = HashMap::with_capacity(reachable.len());
        for &old in &reachable {
            let new = doc.try_new_object_id().map_err(lopdf_err)?;
            id_map.insert(old, new);
        }
        for &old in &reachable {
            let mut obj = doc
                .objects
                .get(&old)
                .cloned()
                .ok_or_else(|| PdfError::Processing(lopdf::Error::ObjectNotFound(old)))?;
            remap_refs(&mut obj, &id_map);
            doc.objects.insert(id_map[&old], obj);
        }
        page_copies.push(id_map[&source_id]);
    }
    Ok(page_copies)
}

/// All object ids reachable from `root` through indirect references,
/// not crossing the root's `/Parent` entry.
fn reachable_objects(doc: &Document, root: ObjectId) -> PdfResult<HashSet<ObjectId>> {
    let mut seen = HashSet::new();
    let mut stack = vec![root];
    while let Some(id) = stack.pop() {
        if !seen.insert(id) {
            continue;
        }
        let Some(obj) = doc.objects.get(&id) else {
            continue;
        };
        if id == root
            && let Object::Dictionary(dict) = obj
        {
            for (key, value) in dict.iter() {
                if key.as_slice() == b"Parent" {
                    continue;
                }
                collect_refs(value, &mut stack);
            }
            continue;
        }
        collect_refs(obj, &mut stack);
    }
    Ok(seen)
}

fn collect_refs(obj: &Object, out: &mut Vec<ObjectId>) {
    match obj {
        Object::Reference(id) => out.push(*id),
        Object::Array(items) => {
            for item in items {
                collect_refs(item, out);
            }
        }
        Object::Dictionary(dict) => {
            for (_, value) in dict.iter() {
                collect_refs(value, out);
            }
        }
        Object::Stream(stream) => {
            for (_, value) in stream.dict.iter() {
                collect_refs(value, out);
            }
        }
        _ => {}
    }
}

/// Rewrite references inside `obj`: ids inside the copied subgraph
/// become the per-copy ids in `map`; references outside it (e.g. the
/// source page's `/Parent`) are left untouched.
fn remap_refs(obj: &mut Object, map: &HashMap<ObjectId, ObjectId>) {
    match obj {
        Object::Reference(id) => {
            if let Some(new) = map.get(id) {
                *id = *new;
            }
        }
        Object::Array(items) => {
            for item in items {
                remap_refs(item, map);
            }
        }
        Object::Dictionary(dict) => {
            for (_, value) in dict.iter_mut() {
                remap_refs(value, map);
            }
        }
        Object::Stream(stream) => {
            for (_, value) in stream.dict.iter_mut() {
                remap_refs(value, map);
            }
        }
        _ => {}
    }
}

/// The `(parent, kids_index)` slot of `page_id`.
fn page_slot(doc: &Document, page_id: ObjectId) -> PdfResult<(ObjectId, usize)> {
    let parent_id = doc
        .get_dictionary(page_id)
        .map_err(lopdf_err)?
        .get(b"Parent")
        .and_then(Object::as_reference)
        .map_err(|_| {
            PdfError::UnsupportedOperation("page dictionary has no indirect /Parent".to_string())
        })?;
    let parent = doc.get_dictionary(parent_id).map_err(lopdf_err)?;
    let kids = parent
        .get(b"Kids")
        .and_then(Object::as_array)
        .map_err(|_| {
            PdfError::UnsupportedOperation("page tree node has no /Kids array".to_string())
        })?;
    let index = kids
        .iter()
        .position(|obj| obj.as_reference().ok() == Some(page_id))
        .ok_or_else(|| {
            PdfError::UnsupportedOperation(
                "page is not a direct child of its /Parent node".to_string(),
            )
        })?;
    Ok((parent_id, index))
}

fn end_slot(
    doc: &Document,
    pages: &BTreeMap<u32, ObjectId>,
) -> PdfResult<(ObjectId, usize, ObjectId)> {
    let last_id = pages
        .iter()
        .next_back()
        .map(|(_, id)| *id)
        .ok_or_else(|| PdfError::InvalidInput("document has no pages".to_string()))?;
    let (parent, _) = page_slot(doc, last_id)?;
    let parent_dict = doc.get_dictionary(parent).map_err(lopdf_err)?;
    let len = parent_dict
        .get(b"Kids")
        .and_then(Object::as_array)
        .map_err(|_| {
            PdfError::UnsupportedOperation("page tree node has no /Kids array".to_string())
        })?
        .len();
    Ok((parent, len, last_id))
}

fn insert_into_kids(
    doc: &mut Document,
    parent_id: ObjectId,
    index: usize,
    new_ids: &[ObjectId],
) -> PdfResult<()> {
    let parent = doc.get_dictionary_mut(parent_id).map_err(lopdf_err)?;
    let kids = parent
        .get_mut(b"Kids")
        .and_then(Object::as_array_mut)
        .map_err(|_| {
            PdfError::UnsupportedOperation("page tree node has no /Kids array".to_string())
        })?;
    for (offset, id) in new_ids.iter().enumerate() {
        kids.insert(index + offset, Object::Reference(*id));
    }
    Ok(())
}

fn bump_counts(doc: &mut Document, start: Option<ObjectId>, delta: i64) -> PdfResult<()> {
    let mut current = start;
    while let Some(id) = current {
        let dict = doc.get_dictionary_mut(id).map_err(lopdf_err)?;
        if let Ok(count) = dict.get(b"Count").and_then(Object::as_i64) {
            dict.set("Count", count + delta);
        }
        current = dict.get(b"Parent").and_then(Object::as_reference).ok();
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::crates::pdf_engine::ops::test_utils::{
        page_count as test_page_count, pdf_with_pages,
    };
    use lopdf::Document;

    fn page_text(pdf: &[u8], n: u32) -> String {
        let doc = Document::load_mem(pdf).unwrap();
        let page_id = doc.get_pages()[&n];
        String::from_utf8_lossy(&doc.get_page_content(page_id)).into_owned()
    }

    #[test]
    fn duplicates_after_source() {
        let pdf = pdf_with_pages(&["one", "two", "three"]);
        let (out, numbers) =
            duplicate_pages(&pdf, "2", 1, DupPos::After(2), &EngineLimits::default()).unwrap();
        assert_eq!(test_page_count(&out), 4);
        assert_eq!(numbers, vec![3]);
        assert!(page_text(&out, 3).contains("two"));
        assert!(page_text(&out, 4).contains("three"));
    }

    #[test]
    fn duplicates_before_anchor() {
        let pdf = pdf_with_pages(&["one", "two", "three"]);
        let (out, numbers) =
            duplicate_pages(&pdf, "3", 1, DupPos::Before(2), &EngineLimits::default()).unwrap();
        assert_eq!(test_page_count(&out), 4);
        assert_eq!(numbers, vec![2]);
        assert!(page_text(&out, 2).contains("three"));
        assert!(page_text(&out, 3).contains("two"));
    }

    #[test]
    fn duplicates_in_place_and_multiple_copies() {
        let pdf = pdf_with_pages(&["one", "two", "three"]);
        let (out, numbers) =
            duplicate_pages(&pdf, "1,3", 2, DupPos::InPlace, &EngineLimits::default()).unwrap();
        assert_eq!(test_page_count(&out), 7);
        // Page 1's copies land at positions 2-3; page 3's copies after
        // it in the shifted numbering.
        assert!(page_text(&out, 2).contains("one"));
        assert!(page_text(&out, 3).contains("one"));
        assert!(page_text(&out, 4).contains("two"));
        assert!(page_text(&out, 7).contains("three"));
        assert_eq!(numbers, vec![2, 3, 6, 7]);
    }

    #[test]
    fn duplicates_to_end() {
        let pdf = pdf_with_pages(&["one", "two"]);
        let (out, numbers) =
            duplicate_pages(&pdf, "1", 1, DupPos::End, &EngineLimits::default()).unwrap();
        assert_eq!(test_page_count(&out), 3);
        assert_eq!(numbers, vec![3]);
        assert!(page_text(&out, 3).contains("one"));
    }

    #[test]
    fn copies_have_independent_objects() {
        let pdf = pdf_with_pages(&["one", "two"]);
        let (out, _) =
            duplicate_pages(&pdf, "1", 1, DupPos::After(1), &EngineLimits::default()).unwrap();
        let doc = Document::load_mem(&out).unwrap();
        let pages = doc.get_pages();
        // Original and copy must be distinct objects, as must their
        // content streams.
        assert_ne!(pages[&1], pages[&2]);
        let c1 = doc.get_page_contents(pages[&1]);
        let c2 = doc.get_page_contents(pages[&2]);
        assert_ne!(c1, c2);
        // Both copies' /Parent must point at the same page-tree node.
        let p1 = doc
            .get_dictionary(pages[&1])
            .unwrap()
            .get(b"Parent")
            .unwrap();
        let p2 = doc
            .get_dictionary(pages[&2])
            .unwrap()
            .get(b"Parent")
            .unwrap();
        assert_eq!(p1, p2);
    }

    #[test]
    fn rejects_bad_input() {
        let pdf = pdf_with_pages(&["one", "two"]);
        assert!(duplicate_pages(&pdf, "1", 0, DupPos::End, &EngineLimits::default()).is_err());
        assert!(duplicate_pages(&pdf, "5", 1, DupPos::End, &EngineLimits::default()).is_err());
        assert!(duplicate_pages(&pdf, "1", 1, DupPos::After(3), &EngineLimits::default()).is_err());
        assert!(duplicate_pages(b"junk", "1", 1, DupPos::End, &EngineLimits::default()).is_err());
    }

    #[test]
    fn enforces_max_pages() {
        let pdf = pdf_with_pages(&["one", "two"]);
        let limits = EngineLimits {
            max_pages: Some(3),
            ..EngineLimits::default()
        };
        assert!(duplicate_pages(&pdf, "all", 1, DupPos::End, &limits).is_err());
    }
}
