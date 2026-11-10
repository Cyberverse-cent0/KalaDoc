//! Insert blank pages into a PDF document.
//!
//! `lopdf` 0.47 has no page-insertion API, so this op splices new leaf
//! page objects directly into the `/Kids` array of the `/Pages` node
//! that owns the anchor page, then bumps `/Count` on that node and all
//! of its ancestors. Because insertion happens inside the anchor's own
//! parent, the technique works for nested page trees as well as flat
//! ones: placing a leaf immediately before/after its sibling preserves
//! depth-first page order.
//!
//! Each blank page gets an empty content stream, a `/MediaBox` chosen
//! via [`PageSize`], and a `/Resources` entry copied (by reference) from
//! the neighbouring page when it has one — inheritable attributes left
//! unset still resolve through the page-tree parent chain.

use lopdf::{Dictionary, Document, Object, ObjectId, Stream, dictionary};

use crate::crates::core::engine::EngineLimits;

use crate::crates::pdf_engine::error::{PdfError, PdfResult};

use super::load::{check_page_limit, load_pdf, lopdf_err, reject_encrypted, save_pdf};

/// Where the blank pages are inserted.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum InsertPos {
    /// Insert immediately before the given 1-based page number.
    Before(u32),
    /// Insert immediately after the given 1-based page number.
    After(u32),
    /// Insert at absolute 1-based position `n`: the first blank page
    /// becomes page `n`. `n == page_count + 1` appends at the end
    /// (equivalent to [`InsertPos::End`]).
    At(u32),
    /// Append after the last page.
    End,
}

/// Page size for the inserted blank pages.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PageSize {
    /// ISO A4: 595.28 × 841.89 pt.
    A4,
    /// US Letter: 612 × 792 pt.
    Letter,
    /// Copy the `/MediaBox` of the neighbouring anchor page.
    Match,
}

/// Insert `count` blank pages into `input`.
///
/// Returns the output PDF bytes together with the 1-based page numbers
/// the new pages occupy in the result. `size == None` behaves like
/// [`PageSize::Match`]. Fails with [`PdfError::InvalidInput`] when
/// `count` is 0, the document has no pages, or the anchor page is out
/// of range; [`PdfError::LimitExceeded`] when the result would exceed
/// `max_pages`.
pub fn insert_blank(
    input: &[u8],
    position: InsertPos,
    count: u32,
    size: Option<PageSize>,
    limits: &EngineLimits,
) -> PdfResult<(Vec<u8>, Vec<u32>)> {
    if count == 0 {
        return Err(PdfError::InvalidInput(
            "count must be greater than 0".to_string(),
        ));
    }

    let mut doc = load_pdf(input, limits)?;
    reject_encrypted(&doc)?;

    let pages = doc.get_pages();
    let page_count = pages.len() as u32;
    if page_count == 0 {
        return Err(PdfError::InvalidInput("document has no pages".to_string()));
    }
    check_page_limit(page_count as usize + count as usize, limits)?;

    // Resolve the anchor page and the insertion point inside its
    // parent's /Kids array.
    let (parent_id, index, anchor_id) = resolve_position(&doc, &pages, position)?;

    let anchor_dict = doc.get_dictionary(anchor_id).map_err(lopdf_err)?;
    let media_box = match size.unwrap_or(PageSize::Match) {
        PageSize::A4 => Some(media_box(&[0.0, 0.0, 595.28, 841.89])),
        PageSize::Letter => Some(media_box(&[0.0, 0.0, 612.0, 792.0])),
        PageSize::Match => anchor_dict.get(b"MediaBox").cloned().ok(),
    };
    let resources = anchor_dict.get(b"Resources").cloned().ok();

    let new_ids = add_blank_pages(&mut doc, parent_id, count, media_box, resources)?;
    insert_into_kids(&mut doc, parent_id, index, &new_ids)?;
    bump_counts(&mut doc, Some(parent_id), count as i64)?;

    let first = index as u32 + 1;
    let numbers = (first..first + count).collect();
    let bytes = save_pdf(&mut doc)?;
    Ok((bytes, numbers))
}

/// Locate the `/Pages` parent, the `/Kids` index and the anchor page
/// for `position`. Page numbers are 1-based.
fn resolve_position(
    doc: &Document,
    pages: &std::collections::BTreeMap<u32, ObjectId>,
    position: InsertPos,
) -> PdfResult<(ObjectId, usize, ObjectId)> {
    let page_count = pages.len() as u32;
    let validate = |n: u32| -> PdfResult<()> {
        if n == 0 || n > page_count {
            return Err(PdfError::InvalidPageRange(format!(
                "page {n} is out of range; document has {page_count} page(s)"
            )));
        }
        Ok(())
    };

    match position {
        InsertPos::Before(n) => {
            validate(n)?;
            let page_id = pages[&n];
            let (parent, index) = page_slot(doc, page_id)?;
            Ok((parent, index, page_id))
        }
        InsertPos::After(n) => {
            validate(n)?;
            let page_id = pages[&n];
            let (parent, index) = page_slot(doc, page_id)?;
            Ok((parent, index + 1, page_id))
        }
        InsertPos::At(n) => {
            if n == 0 || n > page_count + 1 {
                return Err(PdfError::InvalidPageRange(format!(
                    "position {n} is out of range; document has {page_count} page(s)"
                )));
            }
            if n == page_count + 1 {
                return end_slot(doc, pages);
            }
            let page_id = pages[&n];
            let (parent, index) = page_slot(doc, page_id)?;
            Ok((parent, index, page_id))
        }
        InsertPos::End => end_slot(doc, pages),
    }
}

/// The `(parent, kids_index)` slot of `page_id` — `page_id` must be a
/// direct leaf of its parent's `/Kids` array, which the PDF page-tree
/// model guarantees.
fn page_slot(doc: &Document, page_id: ObjectId) -> PdfResult<(ObjectId, usize)> {
    let parent_id = page_parent(doc, page_id)?;
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

/// The slot at the very end of the page sequence: the last page's
/// parent `/Kids` tail. The last page in depth-first order is always
/// the last leaf of the last subtree, so appending there appends to
/// the document.
fn end_slot(
    doc: &Document,
    pages: &std::collections::BTreeMap<u32, ObjectId>,
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

fn page_parent(doc: &Document, page_id: ObjectId) -> PdfResult<ObjectId> {
    doc.get_dictionary(page_id)
        .map_err(lopdf_err)?
        .get(b"Parent")
        .and_then(Object::as_reference)
        .map_err(|_| {
            PdfError::UnsupportedOperation("page dictionary has no indirect /Parent".to_string())
        })
}

fn media_box(pts: &[f64; 4]) -> Object {
    Object::Array(pts.iter().map(|v| Object::Real(*v as f32)).collect())
}

/// Create `count` blank page objects parented to `parent_id`.
/// Returns their new object ids in order.
fn add_blank_pages(
    doc: &mut Document,
    parent_id: ObjectId,
    count: u32,
    media_box: Option<Object>,
    resources: Option<Object>,
) -> PdfResult<Vec<ObjectId>> {
    // One shared empty content stream: it is never mutated, so every
    // blank page can reference the same object.
    let stream_id = doc
        .try_add_object(Stream::new(Dictionary::new(), Vec::new()))
        .map_err(lopdf_err)?;

    let mut ids = Vec::with_capacity(count as usize);
    for _ in 0..count {
        let mut dict = dictionary! {
            "Type" => "Page",
            "Parent" => parent_id,
            "Contents" => stream_id,
        };
        if let Some(mb) = &media_box {
            dict.set("MediaBox", mb.clone());
        }
        if let Some(res) = &resources {
            dict.set("Resources", res.clone());
        }
        ids.push(doc.try_add_object(dict).map_err(lopdf_err)?);
    }
    Ok(ids)
}

/// Splice `new_ids` into `parent_id`'s `/Kids` at `index`, preserving order.
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

/// Add `delta` to `/Count` on `start` and every ancestor `/Pages` node.
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

    #[test]
    fn inserts_at_end_by_default_positions() {
        let pdf = pdf_with_pages(&["one", "two", "three"]);
        let (out, numbers) =
            insert_blank(&pdf, InsertPos::End, 2, None, &EngineLimits::default()).unwrap();
        assert_eq!(test_page_count(&out), 5);
        assert_eq!(numbers, vec![4, 5]);
    }

    #[test]
    fn inserts_before_and_after() {
        let pdf = pdf_with_pages(&["one", "two", "three"]);
        let (out, numbers) = insert_blank(
            &pdf,
            InsertPos::Before(2),
            1,
            Some(PageSize::Letter),
            &EngineLimits::default(),
        )
        .unwrap();
        assert_eq!(test_page_count(&out), 4);
        assert_eq!(numbers, vec![2]);

        let (out, numbers) = insert_blank(
            &out,
            InsertPos::After(4),
            1,
            Some(PageSize::A4),
            &EngineLimits::default(),
        )
        .unwrap();
        assert_eq!(test_page_count(&out), 5);
        assert_eq!(numbers, vec![5]);
    }

    #[test]
    fn at_position_and_letter_size() {
        let pdf = pdf_with_pages(&["one", "two"]);
        let (out, numbers) = insert_blank(
            &pdf,
            InsertPos::At(2),
            1,
            Some(PageSize::Letter),
            &EngineLimits::default(),
        )
        .unwrap();
        assert_eq!(numbers, vec![2]);
        assert_eq!(test_page_count(&out), 3);

        // The inserted page should carry a Letter MediaBox.
        let doc = Document::load_mem(&out).unwrap();
        let page_id = doc.get_pages()[&2];
        let dict = doc.get_dictionary(page_id).unwrap();
        let mb = dict.get(b"MediaBox").unwrap().as_array().unwrap();
        assert_eq!(mb.len(), 4);
    }

    #[test]
    fn match_size_copies_neighbor_mediabox() {
        let pdf = pdf_with_pages(&["one", "two"]);
        let (out, _) = insert_blank(
            &pdf,
            InsertPos::After(1),
            1,
            Some(PageSize::Match),
            &EngineLimits::default(),
        )
        .unwrap();
        let doc = Document::load_mem(&out).unwrap();
        let pages = doc.get_pages();
        let src = doc.get_dictionary(pages[&1]).unwrap();
        let new = doc.get_dictionary(pages[&2]).unwrap();
        assert_eq!(src.get(b"MediaBox").unwrap(), new.get(b"MediaBox").unwrap());
    }

    #[test]
    fn rejects_bad_input() {
        let pdf = pdf_with_pages(&["one", "two"]);
        assert!(insert_blank(&pdf, InsertPos::End, 0, None, &EngineLimits::default()).is_err());
        assert!(
            insert_blank(
                &pdf,
                InsertPos::Before(0),
                1,
                None,
                &EngineLimits::default()
            )
            .is_err()
        );
        assert!(
            insert_blank(&pdf, InsertPos::After(3), 1, None, &EngineLimits::default()).is_err()
        );
        assert!(insert_blank(&pdf, InsertPos::At(4), 1, None, &EngineLimits::default()).is_err());
        assert!(insert_blank(b"junk", InsertPos::End, 1, None, &EngineLimits::default()).is_err());
    }

    #[test]
    fn enforces_max_pages() {
        let pdf = pdf_with_pages(&["one", "two"]);
        let limits = EngineLimits {
            max_pages: Some(3),
            ..EngineLimits::default()
        };
        assert!(insert_blank(&pdf, InsertPos::End, 2, None, &limits).is_err());
    }
}
