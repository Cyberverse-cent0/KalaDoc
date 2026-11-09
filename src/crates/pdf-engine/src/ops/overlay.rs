//! Overlay pages of one PDF onto the pages of another.
//!
//! Every page of `overlay_pdf` is imported into the target document as a
//! Form XObject (`/Subtype /Form`, `/BBox` taken from the source page's
//! `/MediaBox`, `/Resources` carried over) and painted onto matching
//! target pages via the `Do` operator in an appended content stream.
//! All indirect objects of the overlay document are copied into the
//! target with remapped object ids — the same technique [`merge`] uses —
//! so fonts, images and other resources referenced by the overlay keep
//! working.
//!
//! # Page mapping (documented, deterministic)
//!
//! - Overlay with **1 page**: that page is painted, centered, onto
//!   *every* selected target page.
//! - Overlay with **N pages** and **N selected target pages**: page *i*
//!   of the overlay is painted onto selected target page *i* (both in
//!   ascending page order).
//! - Any other combination is rejected with
//!   [`PdfError::InvalidInput`] before the document is modified.
//!
//! # Honest limitations
//!
//! - No scaling: the overlay is painted at 100% size, centered on the
//!   target page's `/MediaBox`. An overlay larger than the target page
//!   overflows the visible area (viewers clip to the crop box).
//! - The overlay's own optional-content (OCG) state is not preserved;
//!   its `/Resources` are imported as-is.
//! - Annotations, outlines and document-level structures of the overlay
//!   are **not** imported — only page content and resources.
//! - Overlays that rely on transparency groups may render differently
//!   than in their source document; no `/Group` is synthesized.

use lopdf::content::{Content, Operation};
use lopdf::{Dictionary, Document, Object, ObjectId, Stream, StringFormat, dictionary};

use crate::crates::core::engine::EngineLimits;

use crate::crates::pdf_engine::error::{PdfError, PdfResult};

use super::flatten::pdf_num;
use super::load::{load_pdf, lopdf_err, reject_encrypted, save_pdf};
use super::pages::parse_page_range;

/// Overlay `overlay_pdf` onto `input`; returns a new PDF.
///
/// `pages` selects target pages with the shared 1-based range spec;
/// `None` selects all of them. See the module docs for the page-mapping
/// rules and limitations.
pub fn overlay(
    input: &[u8],
    overlay_pdf: &[u8],
    pages: Option<&str>,
    limits: &EngineLimits,
) -> PdfResult<Vec<u8>> {
    let mut doc = load_pdf(input, limits)?;
    reject_encrypted(&doc)?;
    let overlay = load_pdf(overlay_pdf, limits)?;
    reject_encrypted(&overlay)?;

    let selected = selected_pages(&doc, pages)?;
    let pages_map = doc.get_pages();

    // Overlay page ids in ascending page order.
    let overlay_page_ids: Vec<ObjectId> = overlay.get_pages().into_values().collect();
    if overlay_page_ids.is_empty() {
        return Err(PdfError::InvalidInput(
            "overlay document has no pages".to_string(),
        ));
    }

    // Decide the mapping before touching the target document.
    // (target page number, index into overlay_page_ids)
    let mapping: Vec<(u32, usize)> = if overlay_page_ids.len() == 1 {
        selected.iter().map(|&p| (p, 0)).collect()
    } else if overlay_page_ids.len() == selected.len() {
        selected.iter().enumerate().map(|(i, &p)| (p, i)).collect()
    } else {
        return Err(PdfError::InvalidInput(format!(
            "overlay has {} page(s) but {} target page(s) were selected; \
             use a single-page overlay or match the counts",
            overlay_page_ids.len(),
            selected.len()
        )));
    };

    // ---- Copy every overlay object into the target, id-remapped. ----
    let target_max = doc.objects.keys().map(|(n, _)| *n).max().unwrap_or(0);
    let overlay_span = overlay.objects.keys().map(|(n, _)| *n).max().unwrap_or(0);
    let offset = target_max
        .checked_add(1)
        .ok_or_else(|| PdfError::LimitExceeded("object id space exhausted".to_string()))?;
    // New ids are (old + offset); keep room for the largest remapped id.
    let new_max = offset
        .checked_add(overlay_span)
        .ok_or_else(|| PdfError::LimitExceeded("object id space exhausted".to_string()))?;

    for (id, object) in &overlay.objects {
        let new_id = remap_id(*id, offset)?;
        let mut object = object.clone();
        remap_object(&mut object, offset)?;
        doc.objects.insert(new_id, object);
    }
    doc.max_id = doc.max_id.max(new_max);

    // ---- Build one Form XObject per needed overlay page. ----
    let mut needed: Vec<usize> = mapping.iter().map(|(_, i)| *i).collect();
    needed.sort_unstable();
    needed.dedup();
    let mut form_ids: Vec<Option<ObjectId>> = vec![None; overlay_page_ids.len()];
    // BBox per overlay page (needed for centering math at paint time).
    let mut bboxes: Vec<[f64; 4]> = Vec::with_capacity(overlay_page_ids.len());
    for &oid in &overlay_page_ids {
        bboxes.push(media_box(&overlay, oid));
    }

    for &idx in &needed {
        let oid = overlay_page_ids[idx];
        let content = overlay.get_page_content(oid);
        let bbox = bboxes[idx];

        let mut form_dict = dictionary! {
            "Type" => "XObject",
            "Subtype" => "Form",
            "FormType" => 1,
            "BBox" => vec![
                Object::from(bbox[0] as f32),
                Object::from(bbox[1] as f32),
                Object::from(bbox[2] as f32),
                Object::from(bbox[3] as f32),
            ],
        };
        if let Some(mut res) = resources_object(&overlay, oid) {
            remap_object(&mut res, offset)?;
            form_dict.set("Resources", res);
        }

        let mut stream = Stream::new(form_dict, content);
        // Best-effort: compress the form stream when it pays off.
        let _ = stream.compress();
        let form_id = doc
            .try_add_object(Object::Stream(stream))
            .map_err(lopdf_err)?;
        form_ids[idx] = Some(form_id);
    }

    // ---- Paint each mapped target page. ----
    for (page_number, overlay_idx) in mapping {
        let Some(&page_id) = pages_map.get(&page_number) else {
            return Err(PdfError::InvalidPageRange(format!(
                "page {page_number} not found in document"
            )));
        };
        let Some(form_id) = form_ids[overlay_idx] else {
            // Unreachable: form_ids is populated for every mapped index.
            return Err(PdfError::InvalidInput(
                "internal error: missing form XObject".to_string(),
            ));
        };

        // Center the overlay's bbox on the target page's mediabox.
        let target_box = media_box(&doc, page_id);
        let obox = bboxes[overlay_idx];
        let dx = (target_box[0] + target_box[2]) / 2.0 - (obox[0] + obox[2]) / 2.0;
        let dy = (target_box[1] + target_box[3]) / 2.0 - (obox[1] + obox[3]) / 2.0;

        let mut resources = effective_resources_dict(&doc, page_id);
        let name = ensure_named_resource(
            &mut doc,
            &mut resources,
            b"XObject",
            "Fm",
            Object::Reference(form_id),
        )?;
        set_page_resources(&mut doc, page_id, resources)?;

        let content = format!(
            "q\n1 0 0 1 {} {} cm\n/{} Do\nQ\n",
            pdf_num(dx),
            pdf_num(dy),
            String::from_utf8_lossy(&name)
        )
        .into_bytes();
        doc.add_page_contents(page_id, content).map_err(lopdf_err)?;
    }

    save_pdf(&mut doc)
}

fn remap_id(id: ObjectId, offset: u32) -> PdfResult<ObjectId> {
    let num =
        id.0.checked_add(offset)
            .ok_or_else(|| PdfError::LimitExceeded("object id space exhausted".to_string()))?;
    Ok((num, id.1))
}

/// Rewrite every [`Object::Reference`] inside `object` by `offset`
/// (same scheme as `merge`).
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

// ---------------------------------------------------------------------------
// Shared page geometry / resource helpers used by `overlay`.
//
// Resource dictionaries are always merged onto a per-page clone that is
// stored as a fresh indirect object, so dictionaries shared with sibling
// pages are never mutated.
// ---------------------------------------------------------------------------

/// Parse a 4-number box array (`/MediaBox`, `/Rect`, `/BBox`) into
/// `[x0, y0, x1, y1]` as f64.
fn rect_from_object(obj: &Object) -> Option<[f64; 4]> {
    let items = obj.as_array().ok()?;
    if items.len() != 4 {
        return None;
    }
    let mut out = [0.0; 4];
    for (i, item) in items.iter().enumerate() {
        out[i] = f64::from(item.as_float().ok()?);
    }
    Some(out)
}

/// Effective `/MediaBox` of a page as `[x0, y0, x1, y1]` (f64),
/// walking `/Parent` nodes for inherited values. US Letter is the
/// fallback for pages that declare no box anywhere.
fn media_box(doc: &Document, page_id: ObjectId) -> [f64; 4] {
    let mut current = Some(page_id);
    let mut depth = 0usize;
    while let Some(id) = current {
        depth += 1;
        if depth > 64 {
            break;
        }
        let Ok(dict) = doc.get_dictionary(id) else {
            break;
        };
        if let Ok(obj) = dict.get(b"MediaBox")
            && let Some(rect) = rect_from_object(obj)
        {
            return rect;
        }
        current = dict.get(b"Parent").ok().and_then(|p| p.as_reference().ok());
    }
    [0.0, 0.0, 612.0, 792.0]
}

/// The page's effective `/Resources` as a cloned dictionary: resolves
/// one level of indirection and walks `/Parent` nodes when the page has
/// none of its own.
fn effective_resources_dict(doc: &Document, page_id: ObjectId) -> Dictionary {
    let mut current = Some(page_id);
    let mut depth = 0usize;
    while let Some(id) = current {
        depth += 1;
        if depth > 64 {
            break;
        }
        let Ok(dict) = doc.get_dictionary(id) else {
            break;
        };
        match dict.get(b"Resources") {
            Ok(Object::Reference(rid)) => {
                if let Some(d) = doc.get_object(*rid).ok().and_then(|o| o.as_dict().ok()) {
                    return d.clone();
                }
            }
            Ok(Object::Dictionary(res)) => return res.clone(),
            _ => {}
        }
        current = dict.get(b"Parent").ok().and_then(|p| p.as_reference().ok());
    }
    Dictionary::new()
}

/// The page's effective `/Resources` as a raw [`Object`] (reference or
/// direct dictionary), for copying into a Form XObject.
fn resources_object(doc: &Document, page_id: ObjectId) -> Option<Object> {
    let mut current = Some(page_id);
    let mut depth = 0usize;
    while let Some(id) = current {
        depth += 1;
        if depth > 64 {
            break;
        }
        let Ok(dict) = doc.get_dictionary(id) else {
            break;
        };
        match dict.get(b"Resources") {
            Ok(obj @ (Object::Reference(_) | Object::Dictionary(_))) => {
                return Some(obj.clone());
            }
            _ => {}
        }
        current = dict.get(b"Parent").ok().and_then(|p| p.as_reference().ok());
    }
    None
}

/// Replace a page's `/Resources` with `resources` stored as a fresh
/// indirect object, leaving previously shared resource dictionaries
/// untouched.
fn set_page_resources(
    doc: &mut Document,
    page_id: ObjectId,
    resources: Dictionary,
) -> PdfResult<()> {
    let resources_id = doc
        .try_add_object(Object::Dictionary(resources))
        .map_err(lopdf_err)?;
    doc.get_dictionary_mut(page_id)
        .map_err(lopdf_err)?
        .set("Resources", Object::Reference(resources_id));
    Ok(())
}

/// Selected 1-based page numbers: `Some(spec)` via the shared range
/// parser, `None` = every page.
fn selected_pages(doc: &Document, pages: Option<&str>) -> PdfResult<Vec<u32>> {
    let page_count = doc.get_pages().len() as u32;
    match pages {
        Some(spec) => parse_page_range(spec, page_count),
        None => Ok((1..=page_count).collect()),
    }
}

/// Entries of a named resource sub-dictionary (`/Font`, `/XObject`,
/// `/ExtGState`), resolved one level of indirection.
fn subdict_entries(
    doc: &Document,
    resources: &Dictionary,
    category: &[u8],
) -> Vec<(Vec<u8>, Object)> {
    let Some(obj) = resources.get(category).ok() else {
        return Vec::new();
    };
    let dict = match obj {
        Object::Reference(id) => doc.get_object(*id).ok().and_then(|o| o.as_dict().ok()),
        Object::Dictionary(d) => Some(d),
        _ => None,
    };
    let Some(dict) = dict else {
        return Vec::new();
    };
    dict.iter().map(|(k, v)| (k.clone(), v.clone())).collect()
}

/// Merge `value` into `resources[category]` under a fresh name built
/// from `prefix` (`"KF"`, `"GS"`, `"Fm"` ...). The category dictionary
/// is stored as a new indirect object so previously shared sub-
/// dictionaries are never mutated. Returns the chosen name.
fn ensure_named_resource(
    doc: &mut Document,
    resources: &mut Dictionary,
    category: &[u8],
    prefix: &str,
    value: Object,
) -> PdfResult<Vec<u8>> {
    let mut entries = subdict_entries(doc, resources, category);
    let mut counter = 0u32;
    let name = loop {
        let candidate = if counter == 0 {
            prefix.as_bytes().to_vec()
        } else {
            format!("{prefix}{counter}").into_bytes()
        };
        counter += 1;
        if !entries.iter().any(|(k, _)| *k == candidate) {
            break candidate;
        }
        if counter > 100_000 {
            return Err(PdfError::LimitExceeded(format!(
                "could not find a free {category:?} resource name"
            )));
        }
    };
    entries.push((name.clone(), value));

    let mut dict = Dictionary::new();
    for (k, v) in entries {
        dict.set(k, v);
    }
    let dict_id = doc
        .try_add_object(Object::Dictionary(dict))
        .map_err(lopdf_err)?;
    resources.set(category, Object::Reference(dict_id));
    Ok(name)
}

/// Page-level variant of [`ensure_named_resource`]: clones the page's
/// effective resources, merges the entry, and writes them back.
fn ensure_page_resource(
    doc: &mut Document,
    page_id: ObjectId,
    category: &[u8],
    prefix: &str,
    value: Object,
) -> PdfResult<Vec<u8>> {
    let mut resources = effective_resources_dict(doc, page_id);
    let name = ensure_named_resource(doc, &mut resources, category, prefix, value)?;
    set_page_resources(doc, page_id, resources)?;
    Ok(name)
}

/// Register (or reuse) the base-14 Helvetica font for `page_id` and
/// return its resource name. Each call adds a *fresh* resource name so
/// repeated calls never clobber each other; the underlying font object
/// is shared when an identical one is already reachable.
pub(crate) fn ensure_font_resource(doc: &mut Document, page_id: ObjectId) -> PdfResult<String> {
    // Reuse an existing Helvetica/WinAnsi font object when the page's
    // resources already reference one.
    let font_id = {
        let resources = effective_resources_dict(doc, page_id);
        let mut found = None;
        'outer: for (_, value) in subdict_entries(doc, &resources, b"Font") {
            let dict = match &value {
                Object::Reference(id) => doc.get_dictionary(*id).ok(),
                Object::Dictionary(d) => Some(d),
                _ => None,
            };
            if let Some(d) = dict
                && d.get(b"BaseFont").ok().and_then(|v| v.as_name().ok()) == Some(b"Helvetica")
                && d.get(b"Encoding").ok().and_then(|v| v.as_name().ok())
                    == Some(b"WinAnsiEncoding")
            {
                found = match value {
                    Object::Reference(id) => Some(id),
                    _ => None,
                };
                break 'outer;
            }
        }
        match found {
            Some(id) => id,
            None => doc.add_object(dictionary! {
                "Type" => "Font",
                "Subtype" => "Type1",
                "BaseFont" => "Helvetica",
                "Encoding" => "WinAnsiEncoding",
            }),
        }
    };

    let name = ensure_page_resource(doc, page_id, b"Font", "KF", Object::Reference(font_id))?;
    Ok(String::from_utf8_lossy(&name).into_owned())
}

/// Register an ExtGState with constant alpha `opacity` for `page_id`
/// and return its resource name (always `GS`-prefixed).
pub(crate) fn ensure_ext_gstate(
    doc: &mut Document,
    page_id: ObjectId,
    opacity: f32,
) -> PdfResult<String> {
    let gs_id = doc.add_object(dictionary! {
        "Type" => "ExtGState",
        "ca" => opacity,
        "CA" => opacity,
    });
    let name = ensure_page_resource(doc, page_id, b"ExtGState", "GS", Object::Reference(gs_id))?;
    Ok(String::from_utf8_lossy(&name).into_owned())
}

/// Create a new stream object holding `content`; returns its id.
pub(crate) fn add_content_stream(doc: &mut Document, content: Vec<u8>) -> PdfResult<ObjectId> {
    doc.try_add_object(Object::Stream(Stream::new(Dictionary::new(), content)))
        .map_err(lopdf_err)
}

/// The page's `/Contents` entries as a list of objects (references or
/// direct streams), normalized from single-reference/array forms.
fn contents_list(doc: &Document, page_id: ObjectId) -> Vec<Object> {
    let Ok(page) = doc.get_dictionary(page_id) else {
        return Vec::new();
    };
    match page.get(b"Contents") {
        Ok(Object::Reference(id)) => match doc.get_object(*id) {
            Ok(Object::Array(arr)) => arr.clone(),
            _ => vec![Object::Reference(*id)],
        },
        Ok(Object::Array(arr)) => arr.clone(),
        _ => Vec::new(),
    }
}

/// Set a page's `/Contents` to `list`.
fn set_contents(doc: &mut Document, page_id: ObjectId, list: Vec<Object>) -> PdfResult<()> {
    doc.get_dictionary_mut(page_id)
        .map_err(lopdf_err)?
        .set("Contents", Object::Array(list));
    Ok(())
}

/// Append `stream_id` after the page's existing content streams —
/// stamps and overlays paint on top.
pub(crate) fn append_page_overlay(
    doc: &mut Document,
    page_id: ObjectId,
    stream_id: ObjectId,
) -> PdfResult<()> {
    let mut list = contents_list(doc, page_id);
    list.push(Object::Reference(stream_id));
    set_contents(doc, page_id, list)
}

/// Encode content-stream operations to bytes.
pub(crate) fn encode_operations(operations: Vec<Operation>) -> PdfResult<Vec<u8>> {
    Content { operations }.encode().map_err(lopdf_err)
}

/// Build a text-showing operand. Text whose characters all fit in a
/// byte (Latin-1 range) becomes a literal string; anything wider becomes
/// a UTF-16BE hex string with BOM so CID-capable viewers can try.
///
/// Note: base-14 fonts only cover WinAnsi glyphs, so characters outside
/// that set may render as `.notdef` — callers document this limitation.
pub(crate) fn pdf_text_operand(text: &str) -> Object {
    /// Map a char to a WinAnsi byte; `None` when unrepresentable.
    fn winansi_byte(c: char) -> Option<u8> {
        let u = c as u32;
        match u {
            0x20..=0x7E => Some(u as u8),
            0xA0..=0xFF => Some(u as u8),
            0x20AC => Some(0x80), // €
            0x201A => Some(0x82),
            0x0192 => Some(0x83),
            0x201E => Some(0x84),
            0x2026 => Some(0x85),
            0x2020 => Some(0x86),
            0x2021 => Some(0x87),
            0x02C6 => Some(0x88),
            0x2030 => Some(0x89),
            0x0160 => Some(0x8A),
            0x2039 => Some(0x8B),
            0x0152 => Some(0x8C),
            0x017D => Some(0x8E),
            0x2018 => Some(0x91),
            0x2019 => Some(0x92),
            0x201C => Some(0x93),
            0x201D => Some(0x94),
            0x2022 => Some(0x95),
            0x2013 => Some(0x96),
            0x2014 => Some(0x97),
            0x02DC => Some(0x98),
            0x2122 => Some(0x99),
            0x0161 => Some(0x9A),
            0x203A => Some(0x9B),
            0x0153 => Some(0x9C),
            0x017E => Some(0x9E),
            0x0178 => Some(0x9F),
            _ => None,
        }
    }

    let all_winansi = text.chars().all(|c| winansi_byte(c).is_some());
    if all_winansi {
        // The writer escapes ()\ and control bytes itself; store raw.
        let bytes: Vec<u8> = text
            .chars()
            .map(|c| winansi_byte(c).unwrap_or(b'?'))
            .collect();
        Object::String(bytes, StringFormat::Literal)
    } else {
        let mut bytes = vec![0xFE, 0xFF];
        for unit in text.encode_utf16() {
            bytes.extend_from_slice(&unit.to_be_bytes());
        }
        Object::String(bytes, StringFormat::Hexadecimal)
    }
}

#[cfg(test)]
mod tests {
    use lopdf::Document;

    use super::*;
    use crate::crates::pdf_engine::ops::test_utils::{page_count, pdf_with_pages};

    fn limits() -> EngineLimits {
        EngineLimits::default()
    }

    #[test]
    fn single_page_overlay_applies_to_all_targets() {
        let target = pdf_with_pages(&["t1", "t2"]);
        let overlay_pdf = pdf_with_pages(&["OVERLAY"]);
        let out = overlay(&target, &overlay_pdf, None, &limits()).unwrap();
        assert_eq!(page_count(&out), 2);

        let doc = Document::load_mem(&out).unwrap();
        for (_, page_id) in doc.get_pages() {
            let raw = doc.get_page_content(page_id);
            let content = String::from_utf8_lossy(&raw);
            assert!(content.contains(" Do"), "missing Do: {content}");
            assert!(content.contains("/Fm"), "missing XObject ref: {content}");
            // Original content preserved.
            assert!(content.contains("Tj"));
        }
        // The form XObject must exist in the document.
        let has_form = doc
            .objects
            .values()
            .filter_map(|o| o.as_stream().ok())
            .any(|s| {
                s.dict
                    .get(b"Subtype")
                    .ok()
                    .and_then(|v| v.as_name().ok())
                    .map(|n| n == b"Form")
                    .unwrap_or(false)
            });
        assert!(has_form, "no Form XObject found");
    }

    #[test]
    fn page_count_mismatch_is_rejected() {
        let target = pdf_with_pages(&["a", "b", "c"]);
        let overlay_pdf = pdf_with_pages(&["x", "y"]);
        let err = overlay(&target, &overlay_pdf, None, &limits()).unwrap_err();
        assert!(matches!(err, PdfError::InvalidInput(_)), "got {err:?}");
    }

    #[test]
    fn equal_counts_map_page_by_page() {
        let target = pdf_with_pages(&["t1", "t2"]);
        let overlay_pdf = pdf_with_pages(&["o1", "o2"]);
        let out = overlay(&target, &overlay_pdf, Some("2"), &limits()).unwrap_err();
        // 2 overlay pages vs 1 selected target -> mismatch.
        assert!(matches!(out, PdfError::InvalidInput(_)));

        let out = overlay(&target, &overlay_pdf, None, &limits()).unwrap();
        let doc = Document::load_mem(&out).unwrap();
        assert_eq!(doc.get_pages().len(), 2);
    }

    #[test]
    fn overlay_resources_survive_remap() {
        // The overlay fixture has its own /Resources with font F1;
        // after import the form XObject must still reference a valid
        // (remapped) resources object.
        let target = pdf_with_pages(&["host"]);
        let overlay_pdf = pdf_with_pages(&["mark"]);
        let out = overlay(&target, &overlay_pdf, None, &limits()).unwrap();
        let doc = Document::load_mem(&out).unwrap();
        let form = doc
            .objects
            .values()
            .filter_map(|o| o.as_stream().ok())
            .find(|s| {
                s.dict
                    .get(b"Subtype")
                    .ok()
                    .and_then(|v| v.as_name().ok())
                    .map(|n| n == b"Form")
                    .unwrap_or(false)
            })
            .expect("form xobject");
        assert!(form.dict.get(b"Resources").is_ok(), "form lacks /Resources");
        assert!(form.dict.get(b"BBox").is_ok(), "form lacks /BBox");
    }
}
