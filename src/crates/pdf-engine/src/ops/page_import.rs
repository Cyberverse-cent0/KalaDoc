//! Import pages from another PDF, and replace pages with imported ones.
//!
//! Pages are copied **across documents**: the selected source pages'
//! reachable object graphs (content streams, resource dictionaries,
//! fonts, images, annotations, ...) are deep-copied into the target
//! document under fresh object ids, with all internal references
//! remapped through an old→new id map — the same object-graph copying
//! strategy [`duplicate_pages`](super::duplicate_pages) uses within
//! one document, extended so objects come from the *source* document
//! but are inserted into the *target*.
//!
//! `/Parent` is the one exception, exactly as in `duplicate_pages`:
//! it is *not* traversed during the copy (it points up the source
//! page tree), the copied reference is overwritten, and each imported
//! page's `/Parent` is set to the target `/Pages` node it is inserted
//! under. Any other reference inside a copied object is guaranteed to
//! be inside the copied subgraph — the traversal collects every
//! reachable reference — so nothing dangles back into the source
//! document.
//!
//! Insertion reuses the page-tree splicing technique of
//! [`duplicate_pages`](super::duplicate_pages): imported pages are
//! placed as leaves inside the anchor page's parent `/Kids` array at
//! the resolved slot, which preserves depth-first page order for
//! nested trees too. `/Count` is bumped up the ancestor chain.
//!
//! [`replace_pages`] composes the same machinery: the imported pages
//! are inserted immediately before the first replaced page, then the
//! original target pages — whose numbers have all shifted by the
//! number of imported pages — are deleted. Non-contiguous target
//! selections are allowed: every selected page is replaced by the
//! whole imported block landing at the first selected page's slot.
//!
//! Both documents are loaded through [`load_pdf`] (input limits
//! enforced) and must not be encrypted.

use std::collections::{BTreeMap, HashMap, HashSet};

use lopdf::{Document, Object, ObjectId};

use crate::crates::core::engine::EngineLimits;

use crate::crates::pdf_engine::error::{PdfError, PdfResult};

use super::load::{check_page_limit, load_pdf, lopdf_err, reject_encrypted, save_pdf};
use super::pages::parse_page_range;

/// Where imported pages are inserted in the target document.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ImportPos {
    /// Insert before the first page.
    Start,
    /// Append after the last page.
    End,
    /// Insert immediately before the given 1-based target page.
    Before(u32),
    /// Insert immediately after the given 1-based target page.
    After(u32),
}

/// Import the pages selected by `source_pages` (a 1-based range spec
/// such as `"1-3,8"` or `"all"`, resolved against the *source*
/// document) from `source` into `input` at `position`.
///
/// Page numbers in `position` are 1-based in the **target** document.
/// Returns the output PDF bytes. Fails with
/// [`PdfError::LimitExceeded`] when the result would exceed
/// `max_pages`.
pub fn import_pages(
    input: &[u8],
    source: &[u8],
    source_pages: &str,
    position: ImportPos,
    limits: &EngineLimits,
) -> PdfResult<Vec<u8>> {
    let mut doc = load_pdf(input, limits)?;
    reject_encrypted(&doc)?;
    let src_doc = load_pdf(source, limits)?;
    reject_encrypted(&src_doc)?;

    let src_pages_map = src_doc.get_pages();
    let src_count = src_pages_map.len() as u32;
    let selected = parse_page_range(source_pages, src_count)?;

    let tgt_pages_map = doc.get_pages();
    let tgt_count = tgt_pages_map.len() as u32;
    let (parent, index) = resolve_slot(&doc, &tgt_pages_map, tgt_count, position)?;

    check_page_limit(tgt_count as usize + selected.len(), limits)?;

    let new_ids = import_selected(&mut doc, &src_doc, &src_pages_map, &selected, parent)?;
    insert_into_kids(&mut doc, parent, index, &new_ids)?;
    bump_counts(&mut doc, Some(parent), new_ids.len() as i64)?;

    save_pdf(&mut doc)
}

/// Replace the target pages selected by `target_pages` (1-based, in
/// `input`) with the pages selected by `source_pages` (1-based, in
/// `source`), preserving the imported pages' order.
///
/// The imported block lands at the slot of the **first** replaced
/// page; every originally selected page is then removed, so the
/// result is: pages before the selection, then the imported pages,
/// then the pages after the selection. When the selection is
/// non-contiguous (e.g. `"1,3"`), the imported block still lands once
/// at the first selected page's position and *all* selected pages are
/// deleted — the unselected pages in between are kept. Counts need
/// not match. Fails with [`PdfError::LimitExceeded`] when the final
/// page count would exceed `max_pages`.
pub fn replace_pages(
    input: &[u8],
    source: &[u8],
    source_pages: &str,
    target_pages: &str,
    limits: &EngineLimits,
) -> PdfResult<Vec<u8>> {
    let mut doc = load_pdf(input, limits)?;
    reject_encrypted(&doc)?;
    let src_doc = load_pdf(source, limits)?;
    reject_encrypted(&src_doc)?;

    let tgt_pages_map = doc.get_pages();
    let tgt_count = tgt_pages_map.len() as u32;
    let target_sel = parse_page_range(target_pages, tgt_count)?;

    let src_pages_map = src_doc.get_pages();
    let src_count = src_pages_map.len() as u32;
    let src_sel = parse_page_range(source_pages, src_count)?;

    let final_count = tgt_count as usize + src_sel.len() - target_sel.len();
    check_page_limit(final_count, limits)?;

    // Import immediately before the first replaced page.
    let first = target_sel[0];
    let (parent, index) = resolve_slot(&doc, &tgt_pages_map, tgt_count, ImportPos::Before(first))?;
    let new_ids = import_selected(&mut doc, &src_doc, &src_pages_map, &src_sel, parent)?;
    insert_into_kids(&mut doc, parent, index, &new_ids)?;
    bump_counts(&mut doc, Some(parent), new_ids.len() as i64)?;

    // Every selected page sat at or after the insertion point, so all
    // of them shifted by exactly the number of imported pages.
    let shifted: Vec<u32> = target_sel
        .iter()
        .map(|p| p + new_ids.len() as u32)
        .collect();
    doc.delete_pages(&shifted);

    save_pdf(&mut doc)
}

/// Resolve `position` to the `(parent, kids_index)` slot the imported
/// block is spliced into.
fn resolve_slot(
    doc: &Document,
    pages_map: &BTreeMap<u32, ObjectId>,
    page_count: u32,
    position: ImportPos,
) -> PdfResult<(ObjectId, usize)> {
    match position {
        ImportPos::Start => {
            let first_id = pages_map.iter().next().map(|(_, id)| *id).ok_or_else(|| {
                PdfError::InvalidInput("document has no pages to insert before".to_string())
            })?;
            let (parent, index) = page_slot(doc, first_id)?;
            Ok((parent, index))
        }
        ImportPos::End => {
            let (parent, index, _) = end_slot(doc, pages_map)?;
            Ok((parent, index))
        }
        ImportPos::After(n) | ImportPos::Before(n) => {
            validate_anchor(n, page_count)?;
            let anchor_id = pages_map[&n];
            let (parent, index) = page_slot(doc, anchor_id)?;
            let index = if matches!(position, ImportPos::After(_)) {
                index + 1
            } else {
                index
            };
            Ok((parent, index))
        }
    }
}

/// Deep-copy every selected source page into `doc`, returning the new
/// page object ids in selection order with `/Parent` set to `parent`.
fn import_selected(
    doc: &mut Document,
    src_doc: &Document,
    src_pages_map: &BTreeMap<u32, ObjectId>,
    selected: &[u32],
    parent: ObjectId,
) -> PdfResult<Vec<ObjectId>> {
    let mut new_ids = Vec::with_capacity(selected.len());
    for &page_num in selected {
        let src_id = src_pages_map[&page_num];
        let page_id = import_page(doc, src_doc, src_id)?;
        doc.get_dictionary_mut(page_id)
            .map_err(lopdf_err)?
            .set("Parent", parent);
        new_ids.push(page_id);
    }
    Ok(new_ids)
}

/// Deep-copy `src_id` (a page in `src_doc`) into `doc` under fresh
/// object ids. `/Parent` is copied verbatim (it is excluded from
/// traversal and will dangle until the caller overwrites it with the
/// target parent — see module docs).
fn import_page(doc: &mut Document, src_doc: &Document, src_id: ObjectId) -> PdfResult<ObjectId> {
    // Collect the object subgraph reachable from the source page,
    // excluding the upward /Parent edge so the source page tree
    // itself is never copied.
    let reachable = reachable_objects(src_doc, src_id)?;
    let reachable: Vec<ObjectId> = reachable.into_iter().collect();

    // Fresh ids for every reachable object in the target document.
    // (`try_new_object_id` is used rather than the panicking
    // `new_object_id`, matching `duplicate_pages`.)
    let mut id_map: HashMap<ObjectId, ObjectId> = HashMap::with_capacity(reachable.len());
    for &old in &reachable {
        let new = doc.try_new_object_id().map_err(lopdf_err)?;
        id_map.insert(old, new);
    }
    for &old in &reachable {
        let mut obj = src_doc
            .objects
            .get(&old)
            .cloned()
            .ok_or_else(|| PdfError::Processing(lopdf::Error::ObjectNotFound(old)))?;
        remap_refs(&mut obj, &id_map);
        doc.objects.insert(id_map[&old], obj);
    }
    Ok(id_map[&src_id])
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
/// become the fresh target ids in `map`; references outside it (only
/// the source page's `/Parent`, which the caller overwrites) are left
/// untouched.
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

fn validate_anchor(n: u32, page_count: u32) -> PdfResult<()> {
    if n == 0 || n > page_count {
        return Err(PdfError::InvalidPageRange(format!(
            "page {n} is out of range; document has {page_count} page(s)"
        )));
    }
    Ok(())
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
    use crate::crates::pdf_engine::ops::text::extract_text;
    use lopdf::Document;

    fn page_text(pdf: &[u8], n: u32) -> String {
        let doc = Document::load_mem(pdf).unwrap();
        let page_id = doc.get_pages()[&n];
        String::from_utf8_lossy(&doc.get_page_content(page_id)).into_owned()
    }

    #[test]
    fn imports_pages_at_end() {
        let target = pdf_with_pages(&["t1", "t2"]);
        let source = pdf_with_pages(&["s1", "s2", "s3"]);
        let out = import_pages(
            &target,
            &source,
            "2-3",
            ImportPos::End,
            &EngineLimits::default(),
        )
        .unwrap();
        assert_eq!(test_page_count(&out), 4);
        assert!(page_text(&out, 1).contains("t1"));
        assert!(page_text(&out, 3).contains("s2"));
        assert!(page_text(&out, 4).contains("s3"));
    }

    #[test]
    fn imports_pages_at_start_and_before_after() {
        let target = pdf_with_pages(&["t1", "t2", "t3"]);
        let source = pdf_with_pages(&["s1", "s2"]);

        let out = import_pages(
            &target,
            &source,
            "1",
            ImportPos::Start,
            &EngineLimits::default(),
        )
        .unwrap();
        assert_eq!(test_page_count(&out), 4);
        assert!(page_text(&out, 1).contains("s1"));
        assert!(page_text(&out, 2).contains("t1"));

        let out = import_pages(
            &target,
            &source,
            "all",
            ImportPos::Before(2),
            &EngineLimits::default(),
        )
        .unwrap();
        assert_eq!(test_page_count(&out), 5);
        assert!(page_text(&out, 2).contains("s1"));
        assert!(page_text(&out, 3).contains("s2"));
        assert!(page_text(&out, 4).contains("t2"));

        let out = import_pages(
            &target,
            &source,
            "2",
            ImportPos::After(1),
            &EngineLimits::default(),
        )
        .unwrap();
        assert_eq!(test_page_count(&out), 4);
        assert!(page_text(&out, 2).contains("s2"));
        assert!(page_text(&out, 3).contains("t2"));
    }

    #[test]
    fn imported_pages_have_independent_objects_and_parent() {
        let target = pdf_with_pages(&["t1"]);
        let source = pdf_with_pages(&["s1"]);
        let out = import_pages(
            &target,
            &source,
            "1",
            ImportPos::End,
            &EngineLimits::default(),
        )
        .unwrap();
        let doc = Document::load_mem(&out).unwrap();
        let pages = doc.get_pages();
        assert_ne!(pages[&1], pages[&2]);
        let c1 = doc.get_page_contents(pages[&1]);
        let c2 = doc.get_page_contents(pages[&2]);
        assert_ne!(c1, c2);
        // Both pages' /Parent must point at the same target page-tree
        // node — the copied source /Parent must have been overwritten.
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
        // Page count in the tree root must be correct.
        let root = doc.trailer.get(b"Root").unwrap().as_reference().unwrap();
        let catalog = doc.get_dictionary(root).unwrap();
        let pages_id = catalog.get(b"Pages").unwrap().as_reference().unwrap();
        let count = doc
            .get_dictionary(pages_id)
            .unwrap()
            .get(b"Count")
            .unwrap()
            .as_i64()
            .unwrap();
        assert_eq!(count, 2);
    }

    #[test]
    fn replace_pages_swaps_range() {
        let target = pdf_with_pages(&["t1", "t2", "t3", "t4"]);
        let source = pdf_with_pages(&["s1", "s2"]);
        let out = replace_pages(&target, &source, "all", "2-3", &EngineLimits::default()).unwrap();
        assert_eq!(test_page_count(&out), 4);
        assert!(page_text(&out, 1).contains("t1"));
        assert!(page_text(&out, 2).contains("s1"));
        assert!(page_text(&out, 3).contains("s2"));
        assert!(page_text(&out, 4).contains("t4"));
    }

    #[test]
    fn replace_pages_handles_count_mismatch_and_noncontiguous() {
        let target = pdf_with_pages(&["t1", "t2", "t3", "t4"]);
        let source = pdf_with_pages(&["s1"]);
        // Replace two pages with one: count shrinks.
        let out = replace_pages(&target, &source, "1", "2-3", &EngineLimits::default()).unwrap();
        assert_eq!(test_page_count(&out), 3);
        assert!(page_text(&out, 1).contains("t1"));
        assert!(page_text(&out, 2).contains("s1"));
        assert!(page_text(&out, 3).contains("t4"));

        // Non-contiguous selection "1,3": imported block lands at page
        // 1's slot; both selected pages are removed, page 2 kept.
        let out = replace_pages(&target, &source, "1", "1,3", &EngineLimits::default()).unwrap();
        assert_eq!(test_page_count(&out), 3);
        assert!(page_text(&out, 1).contains("s1"));
        assert!(page_text(&out, 2).contains("t2"));
        assert!(page_text(&out, 3).contains("t4"));
    }

    #[test]
    fn replace_pages_text_is_extractable() {
        let target = pdf_with_pages(&["target one", "target two"]);
        let source = pdf_with_pages(&["imported text"]);
        let out = replace_pages(&target, &source, "1", "1-2", &EngineLimits::default()).unwrap();
        let texts = extract_text(&out, None, &EngineLimits::default()).unwrap();
        assert_eq!(texts.len(), 1);
        assert!(
            texts[0].text.contains("imported text"),
            "{:?}",
            texts[0].text
        );
    }

    #[test]
    fn rejects_bad_input() {
        let target = pdf_with_pages(&["t1", "t2"]);
        let source = pdf_with_pages(&["s1"]);
        // Bad source range.
        assert!(
            import_pages(
                &target,
                &source,
                "5",
                ImportPos::End,
                &EngineLimits::default()
            )
            .is_err()
        );
        // Bad anchor.
        assert!(
            import_pages(
                &target,
                &source,
                "1",
                ImportPos::After(3),
                &EngineLimits::default()
            )
            .is_err()
        );
        // Junk inputs.
        assert!(
            import_pages(
                b"junk",
                &source,
                "1",
                ImportPos::End,
                &EngineLimits::default()
            )
            .is_err()
        );
        assert!(
            import_pages(
                &target,
                b"junk",
                "1",
                ImportPos::End,
                &EngineLimits::default()
            )
            .is_err()
        );
        assert!(replace_pages(&target, &source, "1", "9", &EngineLimits::default()).is_err());
    }

    #[test]
    fn enforces_max_pages_and_rejects_encrypted() {
        let target = pdf_with_pages(&["t1", "t2"]);
        let source = pdf_with_pages(&["s1", "s2"]);
        let limits = EngineLimits {
            max_pages: Some(3),
            ..EngineLimits::default()
        };
        // 2 target + 2 imported = 4 > 3.
        let err = import_pages(&target, &source, "all", ImportPos::End, &limits).unwrap_err();
        assert!(matches!(err, PdfError::LimitExceeded(_)), "got {err:?}");
        // Replace keeps count at 2 — allowed.
        assert!(replace_pages(&target, &source, "all", "all", &limits).is_ok());

        let encrypted = crate::crates::pdf_engine::ops::encrypt::encrypt(
            &source,
            "pw",
            None,
            crate::crates::pdf_engine::ops::encrypt::PdfPermissions::default(),
            &EngineLimits::default(),
        )
        .unwrap();
        let err = import_pages(
            &target,
            &encrypted,
            "1",
            ImportPos::End,
            &EngineLimits::default(),
        )
        .unwrap_err();
        assert!(matches!(err, PdfError::Encrypted), "got {err:?}");
    }
}
