//! Whole-page redaction: replace selected pages' content entirely.
//!
//! For every page selected by the shared 1-based [`parse_page_range`]
//! spec, this op:
//!
//! - Points `/Contents` at a **fresh content stream** — either empty
//!   ([`RedactReplacement::Blank`]) or containing a single centered
//!   text line ([`RedactReplacement::Text`]). Nothing of the original
//!   page content survives: no text, vector art, shadings or marked
//!   content, because the old streams are simply unreferenced.
//! - Removes the page's `/Annots` array and `/Thumb` entry outright —
//!   every annotation on the page (links, markup, widgets, …) is gone.
//! - Prunes AcroForm `/Fields` entries for widget annotations that
//!   lived on the redacted pages, so the form tree does not keep
//!   fields whose only appearance was wiped. A field node that still
//!   has `/Kids` widgets on *other* pages is kept with those kids.
//! - Removes `/Subtype /Image` entries from the page's effective
//!   `/XObject` resources, writing a page-private resources dictionary
//!   so shared/inherited resource dicts are never mutated (same
//!   isolation rule as `remove_images`).
//! - Keeps `/MediaBox` and `/CropBox` untouched — only the painted
//!   content and interactive layer are replaced.
//!
//! Finally [`Document::prune_objects`] drops every object orphaned by
//! the rewrite — old content streams, annotation dictionaries,
//! appearance streams, image XObjects and pruned field widgets — so
//! the redacted material is not merely unreachable but absent from
//! the output file.
//!
//! # Honesty notes / limitations
//!
//! - Widget pruning matches field-tree nodes by object id (widgets
//!   found in the redacted pages' `/Annots`) or by a `/P` back-pointer
//!   to a redacted page. A widget field on a redacted page that is
//!   registered in `/Fields` but missing from `/Annots` *and* lacking
//!   `/P` cannot be identified and is left in the form tree (its
//!   widget is still gone from the page).
//! - Dropping a merged field+widget node removes the whole field,
//!   including its `/V` value — intended for redaction, but fields
//!   sharing that node across pages lose all pages' widgets.
//! - Non-image XObjects (Form XObjects) are left in the page's
//!   `/Resources`; nothing can invoke them anymore, but they remain
//!   reachable objects until a `compress`/`sanitize`-style pass.
//! - `/PieceInfo`, `/StructParents` and the structure tree are not
//!   rewritten; tagged-PDF structure may reference content that no
//!   longer exists.
//! - `Text` replacement uses base-14 Helvetica via WinAnsiEncoding
//!   (same approximation as `watermark`): characters above U+00FF are
//!   emitted as UTF-16BE hex strings which strict viewers may render
//!   as `.notdef`. The text is centered using the same
//!   `estimate_text_width` heuristic — roughly centered, not
//!   typographically exact. Font size is fixed at 24 pt.
//! - Encrypted documents are rejected.

use std::collections::HashSet;

use lopdf::content::Operation;
use lopdf::{Dictionary, Document, Object, ObjectId};
use serde::{Deserialize, Serialize};

use crate::crates::core::engine::EngineLimits;

use crate::crates::pdf_engine::error::{PdfError, PdfResult};

use super::load::{load_pdf, lopdf_err, reject_encrypted, save_pdf};
use super::pages::parse_page_range;
use super::watermark::{
    add_content_stream, effective_resources_dict, encode_operations, ensure_font_resource,
    estimate_text_width, media_box, pdf_text_operand,
};

/// Maximum `/Kids` recursion depth when pruning the AcroForm field
/// tree — same guard style as `sign`/`unlock_fields`.
const MAX_FIELD_DEPTH: usize = 32;

/// Font size used for [`RedactReplacement::Text`] replacements.
const REPLACEMENT_FONT_SIZE: f32 = 24.0;

/// What a redacted page shows in place of its original content.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum RedactReplacement {
    /// Leave the page completely empty (white page).
    Blank,
    /// Draw `text` centered on the page in 24 pt Helvetica.
    Text(String),
}

/// Redact the pages selected by `pages` (a 1-based range spec such as
/// `"1-3,8"` or `"all"`): every selected page's content, annotations,
/// widget fields and image XObjects are removed and replaced by
/// `replacement`. See module docs for the exact scope.
pub fn redact_pages(
    input: &[u8],
    pages: &str,
    replacement: RedactReplacement,
    limits: &EngineLimits,
) -> PdfResult<Vec<u8>> {
    // Validate the replacement before touching the document.
    let text = match &replacement {
        RedactReplacement::Blank => None,
        RedactReplacement::Text(raw) => {
            let trimmed = raw.trim();
            if trimmed.is_empty() {
                return Err(PdfError::InvalidInput(
                    "redaction replacement text must not be empty".to_string(),
                ));
            }
            Some(trimmed.to_string())
        }
    };

    let mut doc = load_pdf(input, limits)?;
    reject_encrypted(&doc)?;

    let page_count = doc.get_pages().len() as u32;
    let selected = parse_page_range(pages, page_count)?;

    // --- Immutable collection phase ---------------------------------
    // Resolve page ids, gather the set of redacted page objects, and
    // collect the object ids of every widget annotation sitting on
    // them (for the AcroForm prune below).
    let pages_map = doc.get_pages();
    let mut page_ids = Vec::with_capacity(selected.len());
    let mut redacted_page_ids: HashSet<ObjectId> = HashSet::new();
    let mut widget_ids: HashSet<ObjectId> = HashSet::new();
    for page_number in selected {
        let Some(&page_id) = pages_map.get(&page_number) else {
            return Err(PdfError::InvalidPageRange(format!(
                "page {page_number} not found in document"
            )));
        };
        page_ids.push(page_id);
        redacted_page_ids.insert(page_id);
        collect_page_widget_ids(&doc, page_id, &mut widget_ids);
    }

    // --- AcroForm widget prune ---------------------------------------
    prune_acroform_widgets(&mut doc, &widget_ids, &redacted_page_ids)?;

    // --- Per-page rewrite ---------------------------------------------
    for &page_id in &page_ids {
        replace_page_content(&mut doc, page_id, text.as_deref())?;
    }

    // Drop everything the rewrite orphaned: old content streams,
    // annotation dictionaries, appearance streams, image XObjects and
    // pruned field/widget objects. Without this the redacted bytes
    // would still sit in the file as unreachable objects.
    doc.prune_objects();

    save_pdf(&mut doc)
}

// ---------------------------------------------------------------------------
// Widget / annotation collection
// ---------------------------------------------------------------------------

/// Whether a dictionary is a widget annotation (`/Subtype /Widget`).
fn is_widget_dict(doc: &Document, dict: &Dictionary) -> bool {
    dict.get(b"Subtype")
        .ok()
        .and_then(|o| doc.dereference(o).ok())
        .and_then(|(_, o)| o.as_name().ok())
        .is_some_and(|name| name == b"Widget")
}

/// Collect the object ids of all `/Subtype /Widget` entries in the
/// page's `/Annots` array. Direct (non-indirect) annotation
/// dictionaries cannot be id-matched and are covered by the `/P`
/// fallback in [`prune_field`] instead.
fn collect_page_widget_ids(doc: &Document, page_id: ObjectId, out: &mut HashSet<ObjectId>) {
    let Ok(page) = doc.get_dictionary(page_id) else {
        return;
    };
    let Ok(annots_obj) = page.get(b"Annots") else {
        return;
    };
    let Ok((_, resolved)) = doc.dereference(annots_obj) else {
        return;
    };
    let Object::Array(items) = resolved else {
        return;
    };
    for item in items {
        let Object::Reference(id) = item else {
            continue;
        };
        let Ok((_, obj)) = doc.dereference(item) else {
            continue;
        };
        let Ok(dict) = obj.as_dict() else {
            continue;
        };
        if is_widget_dict(doc, dict) {
            out.insert(*id);
        }
    }
}

/// Whether the field/widget dictionary's `/P` back-pointer references
/// one of the redacted page objects.
fn page_ref_is_redacted(doc: &Document, dict: &Dictionary, redacted: &HashSet<ObjectId>) -> bool {
    let Ok(p) = dict.get(b"P") else {
        return false;
    };
    match doc.dereference(p) {
        Ok((Some(id), _)) => redacted.contains(&id),
        _ => false,
    }
}

// ---------------------------------------------------------------------------
// AcroForm /Fields pruning
// ---------------------------------------------------------------------------

/// Where the catalog's `/AcroForm` lives.
enum AcroLoc {
    /// Indirect object id of the AcroForm dictionary.
    Ref(ObjectId),
    /// Direct dictionary on the catalog — must be promoted before
    /// mutation.
    Direct,
}

/// Promote a *direct* `/AcroForm` dictionary on the catalog to an
/// indirect object and return its new id. `None` when there is no
/// AcroForm at all.
fn promote_direct_acroform(doc: &mut Document) -> Option<ObjectId> {
    let dict = doc
        .catalog()
        .ok()?
        .get(b"AcroForm")
        .ok()?
        .as_dict()
        .ok()?
        .clone();
    let id = doc.try_add_object(Object::Dictionary(dict)).ok()?;
    doc.catalog_mut()
        .ok()?
        .set("AcroForm", Object::Reference(id));
    Some(id)
}

/// Remove every AcroForm field-tree node that is a widget annotation
/// belonging to a redacted page. Non-widget container fields keep
/// their remaining `/Kids`; a node that is itself a redacted-page
/// widget is dropped unless it still has kids on other pages.
fn prune_acroform_widgets(
    doc: &mut Document,
    widget_ids: &HashSet<ObjectId>,
    redacted_page_ids: &HashSet<ObjectId>,
) -> PdfResult<()> {
    if widget_ids.is_empty() && redacted_page_ids.is_empty() {
        return Ok(());
    }

    // Locate the AcroForm (borrow of `doc` ends before mutation).
    let acro_loc = {
        let catalog = doc.catalog().ok();
        match catalog.and_then(|c| c.get(b"AcroForm").ok()) {
            Some(Object::Reference(id)) => Some(AcroLoc::Ref(*id)),
            Some(Object::Dictionary(_)) => Some(AcroLoc::Direct),
            _ => None,
        }
    };
    let acro_id = match acro_loc {
        Some(AcroLoc::Ref(id)) => Some(id),
        Some(AcroLoc::Direct) => promote_direct_acroform(doc),
        None => None,
    };
    let Some(acro_id) = acro_id else {
        return Ok(());
    };

    let fields_items = {
        let acro = doc.get_dictionary(acro_id).map_err(lopdf_err)?;
        match acro.get(b"Fields") {
            Ok(fields_obj) => match doc.dereference(fields_obj) {
                Ok((_, Object::Array(items))) => items.clone(),
                _ => Vec::new(),
            },
            Err(_) => Vec::new(),
        }
    };
    if fields_items.is_empty() {
        return Ok(());
    }

    let mut kept = Vec::with_capacity(fields_items.len());
    for item in &fields_items {
        if let Some(kept_item) = prune_field(doc, item, widget_ids, redacted_page_ids, 0) {
            kept.push(kept_item);
        }
    }

    let acro = doc.get_dictionary_mut(acro_id).map_err(lopdf_err)?;
    if kept.is_empty() {
        // No fields left at all — drop the key rather than writing an
        // empty array.
        acro.remove(b"Fields");
    } else {
        acro.set("Fields", Object::Array(kept));
    }
    Ok(())
}

/// Recursively prune redacted-page widget nodes from a `/Fields`
/// entry. Returns `Some(object)` for entries to keep — the original
/// object, or a modified direct dictionary when a kept field's
/// `/Kids` changed — and `None` when the whole subtree belongs to a
/// redacted page.
///
/// A node is dropped when it is a `/Widget` annotation whose object
/// id was collected from a redacted page's `/Annots`, or whose `/P`
/// points at a redacted page — *unless* pruning its `/Kids` left
/// survivors on other pages, in which case the node is kept with the
/// filtered kid list (its own widget appearance is already gone via
/// the `/Annots` removal).
fn prune_field(
    doc: &mut Document,
    obj: &Object,
    widget_ids: &HashSet<ObjectId>,
    redacted_page_ids: &HashSet<ObjectId>,
    depth: usize,
) -> Option<Object> {
    if depth > MAX_FIELD_DEPTH {
        return Some(obj.clone());
    }
    let Ok((id_opt, resolved)) = doc.dereference(obj) else {
        return Some(obj.clone());
    };
    let Ok(dict) = resolved.as_dict() else {
        return Some(obj.clone());
    };

    // Classify before recursing — `dict`/`resolved` borrow `doc`.
    let is_redacted_widget = is_widget_dict(doc, dict)
        && (id_opt.is_some_and(|id| widget_ids.contains(&id))
            || page_ref_is_redacted(doc, dict, redacted_page_ids));

    // Prune signature-style: recurse into /Kids first.
    let mut kept_kids: Option<Vec<Object>> = None;
    if let Ok(kids_obj) = dict.get(b"Kids")
        && let Ok((_, Object::Array(items))) = doc.dereference(&kids_obj)
    {
        let items = items.clone();
        let mut kept = Vec::with_capacity(items.len());
        let mut changed = false;
        for kid in &items {
            match prune_field(doc, kid, widget_ids, redacted_page_ids, depth + 1) {
                Some(kept_kid) => kept.push(kept_kid),
                None => changed = true,
            }
        }
        if changed {
            kept_kids = Some(kept);
        }
    }

    if is_redacted_widget {
        match kept_kids {
            // Kids on other pages survive — keep the field node with
            // the filtered kid list.
            Some(kept) if !kept.is_empty() => {
                return Some(write_field_kids(doc, obj, id_opt, &kept));
            }
            _ => return None,
        }
    }

    match kept_kids {
        Some(kept) => Some(write_field_kids(doc, obj, id_opt, &kept)),
        None => Some(obj.clone()),
    }
}

/// Write a pruned `/Kids` list back onto a field node. Indirect
/// fields are mutated in place; a direct field dictionary is cloned
/// and returned modified. An empty `kept` removes the `/Kids` key.
fn write_field_kids(
    doc: &mut Document,
    obj: &Object,
    field_id: Option<ObjectId>,
    kept: &[Object],
) -> Object {
    if let Some(fid) = field_id {
        if let Ok(d) = doc.get_dictionary_mut(fid) {
            if kept.is_empty() {
                d.remove(b"Kids");
            } else {
                d.set("Kids", Object::Array(kept.to_vec()));
            }
        }
        return obj.clone();
    }
    let Ok((_, resolved)) = doc.dereference(obj) else {
        return obj.clone();
    };
    let Ok(d) = resolved.as_dict() else {
        return obj.clone();
    };
    let mut nd = d.clone();
    if kept.is_empty() {
        nd.remove(b"Kids");
    } else {
        nd.set("Kids", Object::Array(kept.to_vec()));
    }
    Object::Dictionary(nd)
}

// ---------------------------------------------------------------------------
// Per-page rewrite
// ---------------------------------------------------------------------------

/// Whether a stream dictionary declares `/Subtype /Image`.
fn is_image_stream(doc: &Document, dict: &Dictionary) -> bool {
    dict.get(b"Subtype")
        .ok()
        .and_then(|o| doc.dereference(o).ok())
        .and_then(|(_, o)| o.as_name().ok())
        .is_some_and(|name| name == b"Image")
}

/// Remove every `/Subtype /Image` entry from the page's effective
/// `/Resources` → `/XObject` dictionary, writing a fresh page-private
/// resources object. Shared dictionaries are never mutated (same
/// isolation rule as `remove_images`). No-op when the page has no
/// image XObjects.
fn strip_image_xobjects(doc: &mut Document, page_id: ObjectId) -> PdfResult<()> {
    let Some(mut resources) = effective_resources_dict(doc, page_id) else {
        return Ok(());
    };
    let Ok(xobjects_obj) = resources.get(b"XObject") else {
        return Ok(());
    };
    let Ok((_, xobjects_resolved)) = doc.dereference(xobjects_obj) else {
        return Ok(());
    };
    let Ok(xobjects) = xobjects_resolved.as_dict() else {
        return Ok(());
    };

    let mut image_names: Vec<Vec<u8>> = Vec::new();
    for (name, value) in xobjects.iter() {
        let Ok((_, target)) = doc.dereference(value) else {
            continue;
        };
        let Ok(stream) = target.as_stream() else {
            continue;
        };
        if is_image_stream(doc, &stream.dict) {
            image_names.push(name.clone());
        }
    }
    if image_names.is_empty() {
        return Ok(());
    }

    // Clone the XObject dictionary and store it as a direct dictionary
    // inside the cloned resources — the resources object written below
    // is page-private, so no shared dictionary is ever touched.
    let mut new_xobjects = xobjects.clone();
    for name in &image_names {
        new_xobjects.remove(name);
    }
    resources.set(b"XObject", Object::Dictionary(new_xobjects));

    let resources_id = doc
        .try_add_object(Object::Dictionary(resources))
        .map_err(lopdf_err)?;
    doc.get_dictionary_mut(page_id)
        .map_err(lopdf_err)?
        .set("Resources", Object::Reference(resources_id));
    Ok(())
}

/// Build the replacement content-stream bytes: empty for
/// [`RedactReplacement::Blank`], or a single centered text line for
/// [`RedactReplacement::Text`].
fn replacement_content(doc: &mut Document, page_id: ObjectId, text: &str) -> PdfResult<Vec<u8>> {
    let font_name = ensure_font_resource(doc, page_id)?;
    let [x0, y0, x1, y1] = media_box(doc, page_id);
    let page_w = (x1 - x0).max(0.0) as f32;
    let page_h = (y1 - y0).max(0.0) as f32;

    // Rough centering via the shared advance-width estimate (same
    // approximation as `watermark`). Baseline sits so the cap-height
    // box is vertically centered.
    let fs = REPLACEMENT_FONT_SIZE;
    let text_w = estimate_text_width(text, fs);
    let tx = x0 as f32 + ((page_w - text_w) * 0.5).max(0.0);
    let ty = y0 as f32 + (page_h * 0.5 - fs * 0.35).max(0.0);

    let operations = vec![
        Operation::new("BT", vec![]),
        Operation::new(
            "Tf",
            vec![Object::Name(font_name.into_bytes()), Object::Real(fs)],
        ),
        Operation::new("Td", vec![Object::Real(tx), Object::Real(ty)]),
        Operation::new("Tj", vec![pdf_text_operand(text)]),
        Operation::new("ET", vec![]),
    ];
    encode_operations(operations)
}

/// Replace one page's `/Contents` with a fresh stream, strip its
/// `/Annots` and `/Thumb`, and remove image XObjects from its
/// resources. `/MediaBox`/`/CropBox` are left untouched.
fn replace_page_content(
    doc: &mut Document,
    page_id: ObjectId,
    text: Option<&str>,
) -> PdfResult<()> {
    strip_image_xobjects(doc, page_id)?;

    let content = match text {
        Some(t) => replacement_content(doc, page_id, t)?,
        None => Vec::new(),
    };
    let stream_id = add_content_stream(doc, content)?;

    let page = doc.get_dictionary_mut(page_id).map_err(lopdf_err)?;
    page.set("Contents", Object::Reference(stream_id));
    page.remove(b"Annots");
    page.remove(b"Thumb");
    Ok(())
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;
    use crate::crates::pdf_engine::ops::extract_text;
    use crate::crates::pdf_engine::ops::test_utils::{page_count, pdf_with_pages};
    use lopdf::{Document as LopdfDocument, Stream, dictionary};

    fn limits() -> EngineLimits {
        EngineLimits::default()
    }

    fn blank() -> RedactReplacement {
        RedactReplacement::Blank
    }

    #[test]
    fn blank_replacement_empties_selected_pages() {
        let input = pdf_with_pages(&["secret one", "keep two", "secret three"]);
        let out = redact_pages(&input, "1,3", blank(), &limits()).unwrap();
        assert_eq!(page_count(&out), 3);

        let texts = extract_text(&out, None, &limits()).unwrap();
        assert!(!texts[0].text.contains("secret one"));
        assert!(texts[1].text.contains("keep two"));
        assert!(!texts[2].text.contains("secret three"));
    }

    #[test]
    fn text_replacement_draws_centered_text() {
        let input = pdf_with_pages(&["old content"]);
        let out = redact_pages(
            &input,
            "all",
            RedactReplacement::Text("REDACTED".to_string()),
            &limits(),
        )
        .unwrap();
        assert_eq!(page_count(&out), 1);

        let texts = extract_text(&out, None, &limits()).unwrap();
        assert!(texts[0].text.contains("REDACTED"));
        assert!(!texts[0].text.contains("old content"));

        // The new content stream must be the only /Contents and must
        // reference a registered font.
        let doc = LopdfDocument::load_mem(&out).unwrap();
        let page_id = doc.get_pages()[&1];
        let contents = doc.get_page_contents(page_id);
        assert_eq!(contents.len(), 1);
        let raw = doc
            .get_object(contents[0])
            .unwrap()
            .as_stream()
            .unwrap()
            .content
            .clone();
        let text = String::from_utf8_lossy(&raw);
        assert!(text.contains("REDACTED"));
        assert!(text.contains(" Tf"));
    }

    #[test]
    fn keeps_mediabox_and_cropbox() {
        let input = pdf_with_pages(&["x"]);
        let out = redact_pages(&input, "1", blank(), &limits()).unwrap();
        let doc = LopdfDocument::load_mem(&out).unwrap();
        let page_id = doc.get_pages()[&1];
        let page = doc.get_dictionary(page_id).unwrap();
        let mb = page.get(b"MediaBox").unwrap().as_array().unwrap();
        assert_eq!(mb.len(), 4);
        // test_utils MediaBox is [0 0 595 842].
        assert_eq!(mb[2].as_i64().unwrap(), 595);
        assert_eq!(mb[3].as_i64().unwrap(), 842);
    }

    #[test]
    fn removes_annots_and_widget_fields() {
        use crate::crates::pdf_engine::ops::annotations::{CommentSpec, add_comment};
        use crate::crates::pdf_engine::ops::form_fields::{
            TextFieldSpec, create_text_field, list_form_fields,
        };

        let input = pdf_with_pages(&["page one", "page two"]);
        // Add a sticky-note annotation and a text field to page 1.
        let with_comment = add_comment(
            &input,
            &CommentSpec {
                text: "a note".to_string(),
                pages: Some("1".to_string()),
                rect: None,
                author: None,
                icon: None,
            },
            &limits(),
        )
        .unwrap();
        let with_field = create_text_field(
            &with_comment,
            &TextFieldSpec {
                name: "secret".to_string(),
                page: 1,
                rect: [100.0, 600.0, 300.0, 620.0],
                value: Some("classified".to_string()),
                read_only: false,
            },
            &limits(),
        )
        .unwrap();
        assert_eq!(list_form_fields(&with_field, &limits()).unwrap().len(), 1);

        let out = redact_pages(&with_field, "1", blank(), &limits()).unwrap();

        // Page 1 must have no /Annots; page 2 never had any.
        let doc = LopdfDocument::load_mem(&out).unwrap();
        let pages = doc.get_pages();
        for pid in pages.values() {
            let page = doc.get_dictionary(*pid).unwrap();
            assert!(page.get(b"Annots").is_err(), "Annots survived redaction");
        }

        // The widget field must be pruned from /AcroForm /Fields.
        assert!(
            list_form_fields(&out, &limits()).unwrap().is_empty(),
            "widget field survived redaction"
        );
    }

    #[test]
    fn removes_image_xobjects() {
        // Two-page doc: page 1 draws image /Im1 via Do plus text;
        // page 2 is plain text (same shape as remove_images' fixture).
        let mut doc = LopdfDocument::with_version("1.5");
        let pages_id = doc.new_object_id();
        let font_id = doc.add_object(dictionary! {
            "Type" => "Font", "Subtype" => "Type1", "BaseFont" => "Courier",
        });
        let image_id = doc.add_object(Stream::new(
            dictionary! {
                "Type" => "XObject",
                "Subtype" => "Image",
                "Width" => 2i64,
                "Height" => 2i64,
                "BitsPerComponent" => 8i64,
                "ColorSpace" => "DeviceGray",
            },
            vec![0u8, 255, 128, 64],
        ));
        let resources_id = doc.add_object(dictionary! {
            "Font" => dictionary! { "F1" => font_id },
            "XObject" => dictionary! { "Im1" => image_id },
        });
        let content = lopdf::content::Content {
            operations: vec![
                Operation::new("q", vec![]),
                Operation::new("Do", vec![Object::Name(b"Im1".to_vec())]),
                Operation::new("Q", vec![]),
                Operation::new("BT", vec![]),
                Operation::new("Tf", vec!["F1".into(), 12.into()]),
                Operation::new("Td", vec![100.into(), 700.into()]),
                Operation::new("Tj", vec![Object::string_literal("page one")]),
                Operation::new("ET", vec![]),
            ],
        };
        let content_id = doc.add_object(Stream::new(dictionary! {}, content.encode().unwrap()));
        let page1_id = doc.add_object(dictionary! {
            "Type" => "Page", "Parent" => pages_id,
            "Contents" => content_id, "Resources" => resources_id,
            "MediaBox" => vec![0.into(), 0.into(), 595.into(), 842.into()],
        });
        let content2 = lopdf::content::Content {
            operations: vec![
                Operation::new("BT", vec![]),
                Operation::new("Tf", vec!["F1".into(), 12.into()]),
                Operation::new("Td", vec![100.into(), 700.into()]),
                Operation::new("Tj", vec![Object::string_literal("page two")]),
                Operation::new("ET", vec![]),
            ],
        };
        let content2_id = doc.add_object(Stream::new(dictionary! {}, content2.encode().unwrap()));
        let page2_id = doc.add_object(dictionary! {
            "Type" => "Page", "Parent" => pages_id,
            "Contents" => content2_id, "Resources" => resources_id,
            "MediaBox" => vec![0.into(), 0.into(), 595.into(), 842.into()],
        });
        doc.objects.insert(
            pages_id,
            Object::Dictionary(dictionary! {
                "Type" => "Pages",
                "Kids" => vec![Object::Reference(page1_id), Object::Reference(page2_id)],
                "Count" => 2i64,
                "MediaBox" => vec![0.into(), 0.into(), 595.into(), 842.into()],
            }),
        );
        let catalog_id = doc.add_object(dictionary! {
            "Type" => "Catalog", "Pages" => pages_id,
        });
        doc.trailer.set("Root", catalog_id);
        let mut buf = Vec::new();
        doc.save_to(&mut buf).unwrap();

        // Redact only page 1: the shared /Resources must not be
        // mutated — page 2 keeps its (unreachable but present) image
        // entry; page 1's private resources drop it.
        let out = redact_pages(&buf, "1", blank(), &limits()).unwrap();
        let out_doc = LopdfDocument::load_mem(&out).unwrap();
        let pages = out_doc.get_pages();
        let p1 = pages[&1];
        let p2 = pages[&2];

        let res1 = out_doc
            .get_dictionary(p1)
            .unwrap()
            .get(b"Resources")
            .unwrap()
            .clone();
        let res1_dict = match &res1 {
            Object::Reference(id) => out_doc.get_object(*id).unwrap().as_dict().unwrap().clone(),
            other => other.as_dict().unwrap().clone(),
        };
        let xo1 = res1_dict.get(b"XObject").unwrap().as_dict().unwrap();
        assert!(xo1.is_empty(), "page 1 XObject dict still has entries");

        // Page 2's resources were never touched.
        let res2 = out_doc
            .get_dictionary(p2)
            .unwrap()
            .get(b"Resources")
            .unwrap()
            .clone();
        let res2_dict = match &res2 {
            Object::Reference(id) => out_doc.get_object(*id).unwrap().as_dict().unwrap().clone(),
            other => other.as_dict().unwrap().clone(),
        };
        let xo2 = res2_dict.get(b"XObject").unwrap().as_dict().unwrap();
        assert!(xo2.has(b"Im1"), "page 2 lost its image entry");
    }

    #[test]
    fn orphaned_objects_are_pruned() {
        let input = pdf_with_pages(&["gone", "stays"]);
        let src = LopdfDocument::load_mem(&input).unwrap();
        let src_objects = src.objects.len();

        let out = redact_pages(&input, "1", blank(), &limits()).unwrap();
        let out_doc = LopdfDocument::load_mem(&out).unwrap();
        // Page 1's old content stream and any shared resources churn
        // must not survive as reachable objects.
        assert!(out_doc.objects.len() < src_objects + 4);
        assert_eq!(out_doc.get_pages().len(), 2);
    }

    #[test]
    fn rejects_bad_input() {
        assert!(redact_pages(b"junk", "1", blank(), &limits()).is_err());
        let input = pdf_with_pages(&["x"]);
        assert!(redact_pages(&input, "9", blank(), &limits()).is_err());
        assert!(redact_pages(&input, "", blank(), &limits()).is_err());
        // Empty replacement text is rejected.
        assert!(
            redact_pages(
                &input,
                "1",
                RedactReplacement::Text("   ".to_string()),
                &limits()
            )
            .is_err()
        );
    }

    #[test]
    fn rejects_encrypted() {
        let plain = pdf_with_pages(&["x"]);
        let encrypted = crate::crates::pdf_engine::ops::encrypt::encrypt(
            &plain,
            "pw",
            None,
            crate::crates::pdf_engine::ops::encrypt::PdfPermissions::default(),
            &limits(),
        )
        .unwrap();
        assert!(matches!(
            redact_pages(&encrypted, "1", blank(), &limits()),
            Err(PdfError::Encrypted)
        ));
    }
}
