//! Flatten annotation appearance streams into page content.
//!
//! For every page, each annotation's appearance stream (`/AP` → `/N`)
//! is imported as a Form XObject and painted onto the page at the
//! annotation's `/Rect`, scaled from the appearance's `/BBox`. The
//! flattened annotations are then removed from the page's `/Annots`
//! array; annotations without a usable appearance stream (link
//! annotations, popup markers, damaged entries) are left untouched.
//!
//! # What is NOT flattened (honest scope)
//!
//! - **Optional-content layers** (`/OCProperties`, `/OCGs`, optional
//!   content groups): untouched.
//! - **JavaScript** and document-level actions: untouched.
//! - **AcroForm field structure**: widget annotations that *are*
//!   flattened lose their page-level `/Annots` entry, but the field
//!   dictionaries under `/AcroForm /Fields` are kept — a viewer may
//!   still render the field's own (now unreferenced) widget or show it
//!   in a forms panel. Full AcroForm flattening is out of scope.
//! - **Annotation `/Matrix`**: honored when present on the appearance
//!   stream (copied to the Form XObject); a `/Matrix` on the annotation
//!   dictionary itself is not applied.
//! - **Widget appearance states**: `/AP /N` given as a *dictionary* of
//!   states is resolved through the annotation's `/AS` entry; without
//!   `/AS` the annotation is skipped rather than guessing.
//! - **Rotation**: appearance placement uses axis-aligned `/Rect`
//!   scaling only; rotated annotation rects are approximated.

use std::collections::BTreeSet;

use lopdf::{Document, Object, ObjectId, Stream, dictionary};

use crate::crates::core::engine::EngineLimits;

use crate::crates::pdf_engine::error::PdfResult;

use super::load::{load_pdf, lopdf_err, reject_encrypted, save_pdf};

// NOTE: this module deliberately keeps its own small helpers instead of
// importing from sibling op modules, so it stays self-contained.

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

/// Format a number for a content stream.
pub(crate) fn pdf_num(value: f64) -> String {
    if value.is_finite() && value.fract() == 0.0 && value.abs() < 9.0e15 {
        format!("{}", value as i64)
    } else if value.is_finite() {
        format!("{value}")
    } else {
        "0".to_string()
    }
}

/// The page's effective `/Resources` as a cloned dictionary: resolves
/// one level of indirection and walks `/Parent` nodes when needed.
fn effective_resources_dict(doc: &Document, page_id: ObjectId) -> Option<lopdf::Dictionary> {
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
                return doc
                    .get_object(*rid)
                    .ok()
                    .and_then(|o| o.as_dict().ok())
                    .cloned();
            }
            Ok(Object::Dictionary(res)) => return Some(res.clone()),
            _ => {}
        }
        current = dict.get(b"Parent").ok().and_then(|p| p.as_reference().ok());
    }
    None
}

/// Entries of a named resource sub-dictionary, resolved one level of
/// indirection.
fn subdict_entries(
    doc: &Document,
    resources: &lopdf::Dictionary,
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

/// Merge `value` into `resources[category]` under a fresh, non-
/// colliding `Fm{n}` name; returns the chosen name.
fn ensure_xobject_name(
    doc: &Document,
    resources: &mut lopdf::Dictionary,
    value: Object,
) -> lopdf::Result<Vec<u8>> {
    let mut entries = subdict_entries(doc, resources, b"XObject");
    let mut counter = 0u32;
    let name = loop {
        let candidate = format!("Fm{counter}").into_bytes();
        counter += 1;
        if !entries.iter().any(|(k, _)| *k == candidate) {
            break candidate;
        }
        if counter > 100_000 {
            return Err(lopdf::Error::DictKey("XObject".to_string()));
        }
    };
    entries.push((name.clone(), value));
    let mut dict = lopdf::Dictionary::new();
    for (k, v) in entries {
        dict.set(k, v);
    }
    resources.set(b"XObject", Object::Dictionary(dict));
    Ok(name)
}

/// Replace a page's `/Resources` with `resources` stored as a fresh
/// indirect object.
fn set_page_resources(
    doc: &mut Document,
    page_id: ObjectId,
    resources: lopdf::Dictionary,
) -> PdfResult<()> {
    let resources_id = doc
        .try_add_object(Object::Dictionary(resources))
        .map_err(lopdf_err)?;
    doc.get_dictionary_mut(page_id)
        .map_err(lopdf_err)?
        .set("Resources", Object::Reference(resources_id));
    Ok(())
}

/// Decompression bound for appearance streams — lopdf's own default,
/// matching what `get_page_content`-style readers use.
use lopdf::DEFAULT_MAX_DECOMPRESSED_SIZE as MAX_APPEARANCE_BYTES;

/// Flatten annotation appearances on every page of `input`.
pub fn flatten(input: &[u8], limits: &EngineLimits) -> PdfResult<Vec<u8>> {
    let mut doc = load_pdf(input, limits)?;
    reject_encrypted(&doc)?;

    let page_ids: Vec<ObjectId> = doc.get_pages().into_values().collect();
    for page_id in page_ids {
        flatten_page(&mut doc, page_id)?;
    }

    save_pdf(&mut doc)
}

/// Appearance objects borrowed from the doc make borrow-checking
/// awkward, so resolution happens in two phases: first collect the data
/// needed to build each Form XObject (cloned), then mutate the doc.
struct PendingAppearance {
    /// Index into the page's `/Annots` array this came from.
    annot_index: usize,
    /// Cloned appearance-stream content (already decompressed).
    content: Vec<u8>,
    /// Appearance `/BBox` as `[x0, y0, x1, y1]`.
    bbox: [f64; 4],
    /// Cloned `/Resources` object of the appearance stream, if any.
    resources: Option<Object>,
    /// Cloned `/Matrix` of the appearance stream, if any.
    matrix: Option<Object>,
    /// Annotation `/Rect` as `[x0, y0, x1, y1]`.
    rect: [f64; 4],
}

fn flatten_page(doc: &mut Document, page_id: ObjectId) -> PdfResult<()> {
    let entries = annots_entries(doc, page_id);
    if entries.is_empty() {
        return Ok(());
    }

    // Phase 1: resolve appearances (immutable borrows only).
    let mut pending: Vec<PendingAppearance> = Vec::new();
    for (index, entry) in entries.iter().enumerate() {
        let Some(annot) = resolve_dict(doc, entry) else {
            continue;
        };
        let Some(ap_stream) = appearance_stream(doc, annot) else {
            continue;
        };
        let Some(bbox) = appearance_bbox(doc, ap_stream, annot) else {
            continue;
        };
        let Some(rect) = annot.get(b"Rect").ok().and_then(|r| rect_from_object(r)) else {
            continue;
        };
        let Ok(content) = ap_stream.decompressed_content_with_limit(MAX_APPEARANCE_BYTES) else {
            // Undecodable appearance: leave the annotation alone.
            continue;
        };
        let resources = ap_stream.dict.get(b"Resources").ok().cloned();
        let matrix = ap_stream.dict.get(b"Matrix").ok().cloned();
        pending.push(PendingAppearance {
            annot_index: index,
            content,
            bbox,
            resources,
            matrix,
            rect,
        });
    }

    if pending.is_empty() {
        return Ok(());
    }

    // Phase 2: build Form XObjects and paint them.
    let mut flattened_indices = BTreeSet::new();
    let mut resources = effective_resources_dict(doc, page_id).unwrap_or_default();
    let mut resources_dirty = false;

    for app in pending {
        flattened_indices.insert(app.annot_index);

        let mut form_dict = dictionary! {
            "Type" => "XObject",
            "Subtype" => "Form",
            "FormType" => 1,
            "BBox" => rect_objects(app.bbox),
        };
        if let Some(res) = app.resources {
            form_dict.set("Resources", res);
        } else {
            form_dict.set("Resources", lopdf::Dictionary::new());
        }
        if let Some(matrix) = app.matrix {
            form_dict.set("Matrix", matrix);
        }

        let mut stream = Stream::new(form_dict, app.content);
        let _ = stream.compress();
        let form_id = doc
            .try_add_object(Object::Stream(stream))
            .map_err(lopdf_err)?;

        let name = ensure_xobject_name(doc, &mut resources, Object::Reference(form_id))
            .map_err(lopdf_err)?;
        resources_dirty = true;

        // Map appearance BBox -> annotation Rect.
        let sx = (app.rect[2] - app.rect[0]) / (app.bbox[2] - app.bbox[0]);
        let sy = (app.rect[3] - app.rect[1]) / (app.bbox[3] - app.bbox[1]);
        if !sx.is_finite() || !sy.is_finite() || sx == 0.0 || sy == 0.0 {
            // Degenerate rect/bbox: skip painting but the annotation is
            // still considered handled only if we painted — so un-mark.
            flattened_indices.remove(&app.annot_index);
            continue;
        }
        let e = app.rect[0] - sx * app.bbox[0];
        let f = app.rect[1] - sy * app.bbox[1];

        let content = format!(
            "q\n{} {} {} {} {} {} cm\n/{} Do\nQ\n",
            pdf_num(sx),
            pdf_num(0.0),
            pdf_num(0.0),
            pdf_num(sy),
            pdf_num(e),
            pdf_num(f),
            String::from_utf8_lossy(&name)
        )
        .into_bytes();
        doc.add_page_contents(page_id, content).map_err(lopdf_err)?;
    }

    if resources_dirty {
        set_page_resources(doc, page_id, resources)?;
    }

    // Phase 3: drop flattened annotations, keep the rest.
    if !flattened_indices.is_empty() {
        let remaining: Vec<Object> = entries
            .iter()
            .enumerate()
            .filter(|(i, _)| !flattened_indices.contains(i))
            .map(|(_, o)| o.clone())
            .collect();
        let page = doc.get_dictionary_mut(page_id).map_err(lopdf_err)?;
        if remaining.is_empty() {
            page.remove(b"Annots");
        } else {
            page.set("Annots", Object::Array(remaining));
        }
    }

    Ok(())
}

/// The page's `/Annots` entries as cloned [`Object`]s — the array may
/// be direct or a single indirection.
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

/// Find the annotation's normal appearance stream.
///
/// `/AP /N` is either a stream (or reference to one) or a dictionary of
/// appearance states — in the latter case the annotation's `/AS` name
/// selects the state.
fn appearance_stream<'a>(doc: &'a Document, annot: &'a lopdf::Dictionary) -> Option<&'a Stream> {
    let ap = annot.get(b"AP").ok()?;
    let ap_dict = match ap {
        Object::Dictionary(d) => Some(d),
        Object::Reference(id) => doc.get_object(*id).ok().and_then(|o| o.as_dict().ok()),
        _ => None,
    }?;
    fn stream_of<'a>(doc: &'a Document, obj: &'a Object) -> Option<&'a Stream> {
        match obj {
            Object::Stream(s) => Some(s),
            Object::Reference(id) => doc.get_object(*id).ok().and_then(|o| o.as_stream().ok()),
            _ => None,
        }
    }

    let normal = ap_dict.get(b"N").ok()?;
    match normal {
        Object::Stream(_) | Object::Reference(_) => stream_of(doc, normal),
        Object::Dictionary(states) => {
            let as_name = annot.get(b"AS").ok()?.as_name().ok()?;
            let state = states.get(as_name).ok()?;
            stream_of(doc, state)
        }
        _ => None,
    }
}

/// The appearance's `/BBox`, falling back to the annotation `/Rect`
/// when the stream does not declare one.
fn appearance_bbox(
    _doc: &Document,
    stream: &Stream,
    annot: &lopdf::Dictionary,
) -> Option<[f64; 4]> {
    stream
        .dict
        .get(b"BBox")
        .ok()
        .and_then(rect_from_object)
        .or_else(|| annot.get(b"Rect").ok().and_then(rect_from_object))
}

/// `[x0, y0, x1, y1]` as PDF number objects for a `/BBox` entry.
fn rect_objects(rect: [f64; 4]) -> Vec<Object> {
    rect.iter().map(|v| Object::from(*v as f32)).collect()
}

#[cfg(test)]
mod tests {
    use lopdf::{Document, Stream, dictionary};

    use super::*;
    use crate::crates::pdf_engine::ops::test_utils::pdf_with_pages;

    fn limits() -> EngineLimits {
        EngineLimits::default()
    }

    /// Build a PDF whose single page carries one square red-ish
    /// appearance-stream annotation.
    fn pdf_with_annotation() -> Vec<u8> {
        let mut doc = Document::with_version("1.5");

        let pages_id = doc.new_object_id();
        let font_id = doc.add_object(dictionary! {
            "Type" => "Font",
            "Subtype" => "Type1",
            "BaseFont" => "Courier",
        });
        let resources_id = doc.add_object(dictionary! {
            "Font" => dictionary! { "F1" => font_id },
        });

        // Appearance stream: fills a 100x100 unit square.
        let ap_id = doc.add_object(Stream::new(
            dictionary! {
                "Type" => "XObject",
                "Subtype" => "Form",
                "BBox" => vec![0.into(), 0.into(), 100.into(), 100.into()],
            },
            b"0 0 100 100 re f\n".to_vec(),
        ));
        let annot_id = doc.add_object(dictionary! {
            "Type" => "Annot",
            "Subtype" => "Stamp",
            "Rect" => vec![100.into(), 700.into(), 200.into(), 800.into()],
            "AP" => dictionary! { "N" => ap_id },
        });

        let content_id = doc.add_object(Stream::new(
            dictionary! {},
            b"BT /F1 12 Tf 10 700 Td (base) Tj ET\n".to_vec(),
        ));
        let page_id = doc.add_object(dictionary! {
            "Type" => "Page",
            "Parent" => pages_id,
            "Contents" => content_id,
            "Resources" => resources_id,
            "MediaBox" => vec![0.into(), 0.into(), 595.into(), 842.into()],
            "Annots" => vec![Object::Reference(annot_id)],
        });
        doc.objects.insert(
            pages_id,
            Object::Dictionary(dictionary! {
                "Type" => "Pages",
                "Kids" => vec![Object::Reference(page_id)],
                "Count" => 1,
                "Resources" => resources_id,
                "MediaBox" => vec![0.into(), 0.into(), 595.into(), 842.into()],
            }),
        );
        let catalog_id = doc.add_object(dictionary! {
            "Type" => "Catalog",
            "Pages" => pages_id,
        });
        doc.trailer.set("Root", catalog_id);

        let mut buf = Vec::new();
        doc.save_to(&mut buf).unwrap();
        buf
    }

    #[test]
    fn flattens_appearance_and_removes_annot() {
        let input = pdf_with_annotation();
        let out = flatten(&input, &limits()).unwrap();

        let doc = Document::load_mem(&out).unwrap();
        let page_id = doc.get_pages()[&1];
        let page = doc.get_dictionary(page_id).unwrap();
        // /Annots must be gone (it only held the flattened annot).
        assert!(page.get(b"Annots").is_err(), "Annots should be removed");

        let raw = doc.get_page_content(page_id);
        let content = String::from_utf8_lossy(&raw);
        assert!(content.contains(" Do"), "missing Do: {content}");
        assert!(content.contains("/Fm"), "missing XObject: {content}");
        // Original page content still present.
        assert!(content.contains("(base) Tj"));
        // The form content itself should be reachable in the doc.
        let has_re = doc
            .objects
            .values()
            .filter_map(|o| o.as_stream().ok())
            .any(|s| s.content.windows(2).any(|w| w == b"re"));
        assert!(has_re, "appearance content not found in document");
    }

    #[test]
    fn flatten_is_noop_on_plain_pages() {
        let input = pdf_with_pages(&["plain"]);
        let out = flatten(&input, &limits()).unwrap();
        let doc = Document::load_mem(&out).unwrap();
        let page_id = doc.get_pages()[&1];
        let raw = doc.get_page_content(page_id);
        let content = String::from_utf8_lossy(&raw);
        assert!(content.contains("plain"));
        assert!(!content.contains(" Do"));
    }
}
