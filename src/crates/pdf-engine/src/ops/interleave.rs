//! Interleave the pages of several PDFs round-robin.
//!
//! Given N inputs, the output takes page 1 of doc A, page 1 of doc B,
//! ..., page 2 of doc A, page 2 of doc B, and so on. When shorter
//! documents are exhausted their slots are simply skipped, so the
//! remaining pages of longer documents are appended in order — the
//! natural result of round-robin over unequal lengths.
//!
//! Like `merge`, `lopdf` has no built-in interleave, so every indirect
//! object of each input is copied into a fresh document with remapped
//! object ids and a single flat page tree is built in the interleaved
//! order. Document-level structures that cannot be merged safely
//! (outlines, page labels, signatures, ...) are not carried over; the
//! first input's `/Info` dictionary is kept.

use lopdf::{Document, Object, ObjectId, dictionary};

use crate::crates::core::engine::EngineLimits;

use crate::crates::pdf_engine::error::{PdfError, PdfResult};

use super::load::{check_page_limit, load_pdf, lopdf_err, reject_encrypted, save_pdf};

/// Interleave the pages of `inputs` (raw PDF bytes) round-robin into a
/// single PDF document.
///
/// Requires at least two inputs. The combined page count must satisfy
/// `max_pages`; producing the single output also respects
/// `max_outputs` (a limit of 0 rejects the operation). Returns
/// [`PdfError::Encrypted`] if any input is encrypted.
pub fn interleave(inputs: &[&[u8]], limits: &EngineLimits) -> PdfResult<Vec<u8>> {
    if inputs.len() < 2 {
        return Err(PdfError::InvalidInput(
            "interleave requires at least two input documents".to_string(),
        ));
    }
    if limits.max_outputs == Some(0) {
        return Err(PdfError::LimitExceeded(
            "max_outputs is 0; cannot produce an output document".to_string(),
        ));
    }

    // Load everything first so a bad input fails before any output work.
    let mut docs = Vec::with_capacity(inputs.len());
    let mut total_pages = 0usize;
    for bytes in inputs {
        let doc = load_pdf(bytes, limits)?;
        reject_encrypted(&doc)?;
        total_pages += doc.get_pages().len();
        docs.push(doc);
    }
    check_page_limit(total_pages, limits)?;

    // Per-document page ids in original order; the round-robin below
    // indexes into these. `get_pages` returns them in page order.
    let per_doc_pages: Vec<Vec<ObjectId>> = docs
        .iter()
        .map(|doc| doc.get_pages().into_values().collect())
        .collect();
    let max_len = per_doc_pages.iter().map(Vec::len).max().unwrap_or(0);

    let mut out = Document::new();
    // Xref streams (PDF 1.5) are the safest common denominator for the
    // rewritten object graph — same choice as `merge`.
    out.version = "1.5".to_string();

    // Copy every object's graph into `out` with a per-document id
    // offset, exactly like merge. Page ids are collected afterwards in
    // round-robin order.
    let mut offset: u32 = 0;
    let mut remapped_pages: Vec<Vec<ObjectId>> = Vec::with_capacity(docs.len());
    let mut first_info: Option<Object> = None;

    for (doc_index, doc) in docs.iter().enumerate() {
        let span = doc.objects.keys().map(|(n, _)| *n).max().unwrap_or(0);
        let doc_offset = offset;
        offset = offset
            .checked_add(span)
            .ok_or_else(|| PdfError::LimitExceeded("object id space exhausted".to_string()))?;

        for (id, object) in &doc.objects {
            let new_id = remap_id(*id, doc_offset)?;
            let mut object = object.clone();
            remap_object(&mut object, doc_offset)?;
            out.objects.insert(new_id, object);
        }
        out.max_id = out.max_id.max(offset);

        let mut pages = Vec::with_capacity(per_doc_pages[doc_index].len());
        for page_id in &per_doc_pages[doc_index] {
            pages.push(remap_id(*page_id, doc_offset)?);
        }
        remapped_pages.push(pages);

        if first_info.is_none()
            && let Ok(info) = doc.trailer.get(b"Info")
        {
            let mut info = info.clone();
            remap_object(&mut info, doc_offset)?;
            first_info = Some(info);
        }
    }

    // Round-robin: page r of every doc that still has one, r ascending.
    // Exhausted docs contribute nothing, so longer docs' tails land in
    // order once the shorter ones run out.
    let mut all_page_ids: Vec<ObjectId> = Vec::with_capacity(total_pages);
    for round in 0..max_len {
        for pages in &remapped_pages {
            if let Some(id) = pages.get(round) {
                all_page_ids.push(*id);
            }
        }
    }

    let pages_dict = dictionary! {
        "Type" => "Pages",
        "Kids" => all_page_ids.iter().map(|id| Object::Reference(*id)).collect::<Vec<Object>>(),
        "Count" => all_page_ids.len() as i64,
    };
    let pages_id = out.try_add_object(pages_dict).map_err(lopdf_err)?;

    // Reparent every page to the new flat page tree.
    for page_id in &all_page_ids {
        if let Some(obj) = out.objects.get_mut(page_id)
            && let Ok(dict) = obj.as_dict_mut()
        {
            dict.set("Parent", pages_id);
        }
    }

    let catalog_id = out
        .try_add_object(dictionary! {
            "Type" => "Catalog",
            "Pages" => pages_id,
        })
        .map_err(lopdf_err)?;

    out.trailer.set("Root", catalog_id);
    if let Some(info) = first_info {
        out.trailer.set("Info", info);
    }
    out.change_producer("KalaDoc pdf-engine");
    out.compress();

    save_pdf(&mut out)
}

fn remap_id(id: ObjectId, offset: u32) -> PdfResult<ObjectId> {
    let num =
        id.0.checked_add(offset)
            .ok_or_else(|| PdfError::LimitExceeded("object id space exhausted".to_string()))?;
    Ok((num, id.1))
}

/// Rewrite every [`Object::Reference`] inside `object` by `offset`.
///
/// Same semantics as `merge`'s remap: references are shifted
/// unconditionally so dangling references stay dangling in the shifted
/// range without colliding with other inputs' objects.
fn remap_object(object: &mut Object, offset: u32) -> PdfResult<()> {
    match object {
        Object::Reference(id) => *id = remap_id(*id, offset)?,
        Object::Array(items) => {
            for item in items {
                remap_object(item, offset)?;
            }
        }
        Object::Dictionary(dict) => {
            for (_, value) in dict.iter_mut() {
                remap_object(value, offset)?;
            }
        }
        Object::Stream(stream) => {
            for (_, value) in stream.dict.iter_mut() {
                remap_object(value, offset)?;
            }
        }
        _ => {}
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
    fn interleaves_two_docs() {
        let a = pdf_with_pages(&["a1", "a2", "a3"]);
        let b = pdf_with_pages(&["b1", "b2"]);
        let out = interleave(&[&a, &b], &EngineLimits::default()).unwrap();
        assert_eq!(test_page_count(&out), 5);
        // a1,b1,a2,b2 then a3 appended once b is exhausted.
        for (n, want) in [(1, "a1"), (2, "b1"), (3, "a2"), (4, "b2"), (5, "a3")] {
            assert!(page_text(&out, n).contains(want), "page {n}");
        }
    }

    #[test]
    fn interleaves_three_docs() {
        let a = pdf_with_pages(&["a1", "a2"]);
        let b = pdf_with_pages(&["b1"]);
        let c = pdf_with_pages(&["c1", "c2", "c3"]);
        let out = interleave(&[&a, &b, &c], &EngineLimits::default()).unwrap();
        assert_eq!(test_page_count(&out), 6);
        let texts: Vec<String> = (1..=6).map(|n| page_text(&out, n)).collect();
        assert!(texts[0].contains("a1"));
        assert!(texts[1].contains("b1"));
        assert!(texts[2].contains("c1"));
        assert!(texts[3].contains("a2"));
        assert!(texts[4].contains("c2"));
        assert!(texts[5].contains("c3"));
    }

    #[test]
    fn requires_two_inputs() {
        let a = pdf_with_pages(&["a1"]);
        assert!(interleave(&[], &EngineLimits::default()).is_err());
        assert!(interleave(&[&a], &EngineLimits::default()).is_err());
        assert!(interleave(&[b"junk".as_slice()], &EngineLimits::default()).is_err());
    }

    #[test]
    fn enforces_max_pages() {
        let a = pdf_with_pages(&["a1", "a2"]);
        let b = pdf_with_pages(&["b1", "b2"]);
        let limits = EngineLimits {
            max_pages: Some(3),
            ..EngineLimits::default()
        };
        assert!(interleave(&[&a, &b], &limits).is_err());
    }
}
