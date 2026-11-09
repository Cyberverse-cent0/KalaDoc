//! Merge multiple PDFs into one, preserving input order.
//!
//! `lopdf` has no built-in merge, so this op copies every indirect
//! object of each input into a fresh document with remapped object IDs,
//! then builds a single flat page tree. Page content, fonts, images and
//! other resources are preserved because they are ordinary indirect
//! objects; document-level structures that cannot be merged safely —
//! outlines/bookmarks, page labels, optional content, signatures — are
//! **not** carried over. The merged document gets a fresh catalog and
//! keeps the first input's `/Info` dictionary (with `/Producer` set to
//! this engine).

use lopdf::{Document, Object, ObjectId, dictionary};

use crate::crates::core::engine::EngineLimits;

use crate::crates::pdf_engine::error::{PdfError, PdfResult};

use super::load::{check_page_limit, load_pdf, lopdf_err, reject_encrypted, save_pdf};

/// Merge `inputs` (raw PDF bytes, in order) into a single PDF document.
///
/// Every input is loaded and limit-checked up front; the combined page
/// count must also satisfy `max_pages`. Returns
/// [`PdfError::Encrypted`] if any input is encrypted.
pub fn merge(inputs: &[&[u8]], limits: &EngineLimits) -> PdfResult<Vec<u8>> {
    if inputs.is_empty() {
        return Err(PdfError::InvalidInput(
            "merge requires at least one input document".to_string(),
        ));
    }

    // Load everything first so a bad input fails before any output work.
    // Enforce the aggregate page limit across all inputs.
    let mut docs = Vec::with_capacity(inputs.len());
    let mut total_pages = 0usize;
    for bytes in inputs {
        let doc = load_pdf(bytes, limits)?;
        reject_encrypted(&doc)?;
        let pages = doc.get_pages().len();
        total_pages += pages;
        docs.push(doc);
    }
    check_page_limit(total_pages, limits)?;

    let mut out = Document::new();
    // Xref streams (PDF 1.5) are the safest common denominator for the
    // rewritten object graph.
    out.version = "1.5".to_string();

    // Cumulative object-number offset. Object numbers in each source are
    // shifted by `offset` so ranges never collide; generation numbers are
    // preserved.
    let mut offset: u32 = 0;
    let mut all_page_ids: Vec<ObjectId> = Vec::new();
    let mut first_info: Option<Object> = None;

    for doc in &docs {
        // Span covers the largest object number actually present, which
        // is safer than trusting `max_id` on repaired documents.
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

        // Collect this document's page ids (already ordered by page
        // number via the BTreeMap) with the same remap applied.
        for (_, page_id) in doc.get_pages() {
            all_page_ids.push(remap_id(page_id, doc_offset)?);
        }

        if first_info.is_none()
            && let Ok(info) = doc.trailer.get(b"Info")
        {
            let mut info = info.clone();
            remap_object(&mut info, doc_offset)?;
            first_info = Some(info);
        }
    }

    let pages_dict = dictionary! {
        "Type" => "Pages",
        "Kids" => all_page_ids.iter().map(|id| Object::Reference(*id)).collect::<Vec<Object>>(),
        "Count" => all_page_ids.len() as i64,
    };
    let pages_id = out.try_add_object(pages_dict).map_err(lopdf_err)?;

    // Reparent every merged page to the new flat page tree so viewers
    // walking /Parent chains stay consistent.
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
    // Best-effort provenance; only touches the Info dict if it resolves.
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
/// References are shifted unconditionally: references to objects missing
/// from the source document stay dangling (now in the shifted range),
/// matching the source's own brokenness without risking collisions with
/// real objects of *other* inputs.
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
