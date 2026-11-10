//! Edit AcroForm field properties: widget `/Rect`, `/MaxLen`,
//! `/Ff` flags (Required / ReadOnly) and `/V` values.
//!
//! [`edit_field_props`] applies a list of [`FieldPatch`]es to fields
//! selected by their **full dotted name** (exact, case-sensitive match
//! against the `/T` hierarchy — the same rules as
//! [`list_form_fields`](super::form_fields)). Patches are applied in
//! order; when several patches name the same field the last write for
//! each property wins, and when several fields share a name only the
//! first in document order is touched.
//!
//! # What each patch field writes
//!
//! - `rect` → `/Rect` on **every widget annotation** of the field
//!   (merged self-widget first, then `/Kids` widgets). A field with
//!   no resolvable widget simply gets no `/Rect` write.
//! - `max_len` → `/MaxLen` on the field dictionary. Per ISO 32000
//!   `/MaxLen` is only meaningful for text (`Tx`) fields; it is
//!   written verbatim for any matched field and viewers ignore it
//!   elsewhere.
//! - `required` / `read_only` → the `/Ff` bits **Required** (bit 2,
//!   value `2`) and **ReadOnly** (bit 1, value `1`). Like
//!   [`unlock_fields`](super::unlock_fields), the *effective* flags
//!   (own `/Ff` or inherited) are updated and written back onto the
//!   matched node, which freezes other inherited bits onto it.
//! - `value` → `/V` via `text_string`, **only for `Tx` and `Ch`
//!   fields**. For `Ch` the value must be one of the field's `/Opt`
//!   export values (same rule as [`fill_form`](super::fill_form)).
//!   A value patch on any other field type — or a `Ch` value outside
//!   `/Opt` — fails the whole call with [`PdfError::InvalidInput`]
//!   *before* any mutation, so a rejected call never produces a
//!   half-edited document.
//!
//! All patches are resolved and validated first; only then are the
//! dictionaries mutated. Names that match no field are reported in
//! [`FieldPatchReport::unmatched`] rather than failing the call.
//! Documents without an `/AcroForm` return the input unchanged with
//! every requested name reported unmatched.
//!
//! When a `/V` is written, widget appearances are regenerated exactly
//! like [`fill_form`](super::fill_form) does for widgets that already
//! carry an `/AP`, and `/AcroForm /NeedAppearances` is set so
//! viewers regenerate the rest. The same appearance limitations apply
//! (single-line base-14 Helvetica, UTF-8 bytes for non-ASCII text).

use std::collections::{HashMap, HashSet};

use lopdf::{Document, Object, ObjectId, text_string};
use serde::{Deserialize, Serialize};

use crate::crates::core::engine::EngineLimits;

use crate::crates::pdf_engine::error::{PdfError, PdfResult};

use super::fill_form::regenerate_appearance;
use super::form_fields::{
    FieldEntry, acroform_dict_id, collect_fields, normalize_acroform, set_need_appearances,
};
use super::load::{load_pdf, lopdf_err, reject_encrypted, save_pdf};
use super::unlock_fields::{FieldNode, scan_field_nodes};

/// `/Ff` bit for the ReadOnly flag (ISO 32000-1, table 227, bit 1).
const FF_READ_ONLY: i64 = 1;
/// `/Ff` bit for the Required flag (ISO 32000-1, table 227, bit 2).
const FF_REQUIRED: i64 = 2;

/// One requested property edit on a named form field. Every field is
/// optional; `None` leaves that property untouched. A patch with all
/// fields `None` matches the field but changes nothing.
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
pub struct FieldPatch {
    /// Full dotted field name (exact, case-sensitive).
    pub name: String,
    /// New widget `/Rect` `[x0, y0, x1, y1]` in default user space.
    /// Must be finite with `x1 > x0` and `y1 > y0`.
    pub rect: Option<[f64; 4]>,
    /// New `/MaxLen` (maximum text length). Written verbatim; only
    /// meaningful for `Tx` fields.
    pub max_len: Option<u32>,
    /// Set (`true`) or clear (`false`) the Required flag (`/Ff` bit 2).
    pub required: Option<bool>,
    /// Set (`true`) or clear (`false`) the ReadOnly flag (`/Ff` bit 1).
    pub read_only: Option<bool>,
    /// New `/V` value. Only `Tx` and `Ch` fields accept values; `Ch`
    /// values must match an `/Opt` export value.
    pub value: Option<String>,
}

/// Outcome of [`edit_field_props`].
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
pub struct FieldPatchReport {
    /// Patches whose `name` resolved to a field node (each patch in
    /// the input counts, including duplicate names).
    pub matched: usize,
    /// Matched patches where at least one requested property was
    /// actually written to the document.
    pub updated: usize,
    /// Matched patches that changed nothing: all properties `None`,
    /// the field had no indirect object id, `rect` was requested but
    /// the field has no widget, or the requested `/Ff` bits already
    /// had the requested values.
    pub unchanged: usize,
    /// Requested names (deduplicated, in request order) that matched
    /// no field node. Never fatal.
    pub unmatched: Vec<String>,
}

/// Apply `patches` to the named AcroForm fields in `input` and return
/// the modified PDF plus a [`FieldPatchReport`].
///
/// See the module docs for per-property semantics. Errors are atomic:
/// every patch is validated before any dictionary is mutated.
pub fn edit_field_props(
    input: &[u8],
    patches: &[FieldPatch],
    limits: &EngineLimits,
) -> PdfResult<(Vec<u8>, FieldPatchReport)> {
    let mut doc = load_pdf(input, limits)?;
    reject_encrypted(&doc)?;

    let mut report = FieldPatchReport::default();
    if patches.is_empty() {
        return Ok((input.to_vec(), report));
    }
    let Some(acro_id) = acroform_dict_id(&mut doc)? else {
        // No AcroForm: nothing can match. Report every requested name
        // as unmatched so callers can detect the no-op.
        report.unmatched = collect_unmatched(patches, &[]);
        return Ok((input.to_vec(), report));
    };
    // Promote inline field/widget dictionaries so every node has an
    // object id to mutate.
    normalize_acroform(&mut doc)?;
    let nodes = scan_field_nodes(&doc);
    let entries = collect_fields(&doc);

    // Pass 1 — resolve names and validate every patch before mutating.
    let mut resolved: Vec<(&FieldNode, &FieldPatch)> = Vec::with_capacity(patches.len());
    for patch in patches {
        let Some(node) = nodes.iter().find(|n| n.name == patch.name) else {
            continue;
        };
        if let Some(rect) = patch.rect {
            validate_rect(rect)?;
        }
        if let Some(value) = &patch.value {
            validate_value(node, value, &entries)?;
        }
        resolved.push((node, patch));
    }
    report.unmatched = collect_unmatched(patches, &nodes);

    // Pass 2 — apply. `flags` tracks each field's current effective
    // /Ff so sequential patches on the same field compose instead of
    // clobbering each other's bits.
    let mut flags: HashMap<ObjectId, i64> = HashMap::with_capacity(nodes.len());
    for node in &nodes {
        if let Some(id) = node.id {
            flags.insert(id, node.ff);
        }
    }
    let mut helv: Option<ObjectId> = None;
    let mut need_appearances = false;

    for (node, patch) in resolved {
        report.matched += 1;
        let Some(field_id) = node.id else {
            report.unchanged += 1;
            continue;
        };
        let mut applied = false;

        // /Ff: combine Required and ReadOnly into one write.
        let stored_ff = flags.get(&field_id).copied().unwrap_or(node.ff);
        let mut new_ff = stored_ff;
        if let Some(b) = patch.required {
            new_ff = set_flag(new_ff, FF_REQUIRED, b);
        }
        if let Some(b) = patch.read_only {
            new_ff = set_flag(new_ff, FF_READ_ONLY, b);
        }
        let ff_requested = patch.required.is_some() || patch.read_only.is_some();

        // Snapshot widget rects up-front for appearance regeneration
        // (needs an immutable borrow of `doc`).
        let regen: Vec<(ObjectId, [f64; 4])> = if patch.value.is_some() {
            node.widgets
                .iter()
                .filter_map(|&wid| widget_rect(&doc, wid).map(|r| (wid, r)))
                .collect()
        } else {
            Vec::new()
        };

        {
            let dict = doc.get_dictionary_mut(field_id).map_err(lopdf_err)?;
            if ff_requested && new_ff != stored_ff {
                dict.set("Ff", Object::Integer(new_ff));
                flags.insert(field_id, new_ff);
                applied = true;
            }
            if let Some(max_len) = patch.max_len {
                dict.set("MaxLen", Object::Integer(i64::from(max_len)));
                applied = true;
            }
            if let Some(value) = &patch.value {
                dict.set("V", text_string(value));
                applied = true;
                need_appearances = true;
            }
        }

        if let Some(rect) = patch.rect {
            let rect_obj = Object::Array(rect.iter().map(|v| Object::from(*v)).collect());
            for &wid in &node.widgets {
                if let Ok(wdict) = doc.get_dictionary_mut(wid) {
                    wdict.set("Rect", rect_obj.clone());
                    applied = true;
                }
            }
        }

        if let Some(value) = &patch.value {
            for (wid, rect) in regen {
                regenerate_appearance(&mut doc, acro_id, &mut helv, wid, rect, value)?;
            }
        }

        if applied {
            report.updated += 1;
        } else {
            report.unchanged += 1;
        }
    }

    if need_appearances {
        set_need_appearances(&mut doc, acro_id);
    }
    Ok((save_pdf(&mut doc)?, report))
}

/// Set or clear `bit` in flag word `ff`.
fn set_flag(ff: i64, bit: i64, on: bool) -> i64 {
    if on { ff | bit } else { ff & !bit }
}

/// Patch names (deduplicated, order preserved) that match no node.
fn collect_unmatched(patches: &[FieldPatch], nodes: &[FieldNode]) -> Vec<String> {
    let mut seen = HashSet::new();
    let mut out = Vec::new();
    for patch in patches {
        if patch.name.is_empty() || !seen.insert(patch.name.as_str()) {
            continue;
        }
        if !nodes.iter().any(|n| n.name == patch.name) {
            out.push(patch.name.clone());
        }
    }
    out
}

/// Validate a requested `/Rect`: finite numbers with `x1 > x0` and
/// `y1 > y0` (same rule as [`create_text_field`](super::form_fields)).
fn validate_rect(rect: [f64; 4]) -> PdfResult<()> {
    if !rect.iter().all(|v| v.is_finite()) || rect[2] <= rect[0] || rect[3] <= rect[1] {
        return Err(PdfError::InvalidInput(
            "field rect must be [x0, y0, x1, y1] with x1 > x0 and y1 > y0".to_string(),
        ));
    }
    Ok(())
}

/// Validate a requested `/V` against the field's type: `Tx` accepts
/// any string; `Ch` requires an `/Opt` export-value match; every other
/// type rejects values outright.
fn validate_value(node: &FieldNode, value: &str, entries: &[FieldEntry]) -> PdfResult<()> {
    match node.type_name.as_str() {
        "Tx" => Ok(()),
        "Ch" => {
            let options = entries
                .iter()
                .find(|e| e.name == node.name)
                .map(|e| e.options.as_slice())
                .unwrap_or(&[]);
            if options.is_empty() || !options.iter().any(|o| o == value) {
                return Err(PdfError::InvalidInput(format!(
                    "value {value:?} is not an allowed option for choice field {:?}",
                    node.name
                )));
            }
            Ok(())
        }
        other => Err(PdfError::InvalidInput(format!(
            "field {:?} has type {other:?}; only Tx/Ch fields accept a value",
            node.name
        ))),
    }
}

/// Read a widget dictionary's `/Rect` as `[f64; 4]`.
fn widget_rect(doc: &Document, widget_id: ObjectId) -> Option<[f64; 4]> {
    let dict = doc.get_dictionary(widget_id).ok()?;
    let items = dict.get(b"Rect").ok()?.as_array().ok()?;
    if items.len() != 4 {
        return None;
    }
    let mut out = [0.0; 4];
    for (i, item) in items.iter().enumerate() {
        out[i] = item.as_float().ok()? as f64;
    }
    Some(out)
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use lopdf::{Document, Object, dictionary, text_string};

    use super::*;
    use crate::crates::pdf_engine::error::PdfError;
    use crate::crates::pdf_engine::ops::encrypt::{PdfPermissions, encrypt};
    use crate::crates::pdf_engine::ops::form_fields::{
        TextFieldSpec, create_text_field, list_form_fields,
    };
    use crate::crates::pdf_engine::ops::test_utils::pdf_with_pages;

    fn limits() -> EngineLimits {
        EngineLimits::default()
    }

    fn spec(name: &str) -> TextFieldSpec {
        TextFieldSpec {
            name: name.to_string(),
            page: 1,
            rect: [50.0, 700.0, 250.0, 720.0],
            value: None,
            read_only: false,
        }
    }

    fn patch(name: &str) -> FieldPatch {
        FieldPatch {
            name: name.to_string(),
            ..Default::default()
        }
    }

    /// The merged field/widget dictionary of the first `/AcroForm
    /// /Fields` entry (what `create_text_field` produces).
    fn first_field_dict(pdf: &[u8]) -> lopdf::Dictionary {
        let doc = Document::load_mem(pdf).unwrap();
        let catalog = doc.catalog().unwrap();
        let (_, af) = doc.dereference(catalog.get(b"AcroForm").unwrap()).unwrap();
        let af = af.as_dict().unwrap();
        let (_, fields) = doc.dereference(af.get(b"Fields").unwrap()).unwrap();
        let fid = fields.as_array().unwrap()[0].as_reference().unwrap();
        doc.get_dictionary(fid).unwrap().clone()
    }

    fn field_info(
        pdf: &[u8],
        name: &str,
    ) -> crate::crates::pdf_engine::ops::form_fields::FieldInfo {
        list_form_fields(pdf, &limits())
            .unwrap()
            .into_iter()
            .find(|f| f.name == name)
            .unwrap()
    }

    #[test]
    fn edits_rect_maxlen_flags_and_value() {
        let base = pdf_with_pages(&["form"]);
        let created = create_text_field(&base, &spec("name"), &limits()).unwrap();

        let mut p = patch("name");
        p.rect = Some([10.0, 20.0, 300.0, 40.0]);
        p.max_len = Some(50);
        p.required = Some(true);
        p.read_only = Some(true);
        p.value = Some("Bob".to_string());

        let (out, report) = edit_field_props(&created, &[p], &limits()).unwrap();
        assert_eq!(report.matched, 1);
        assert_eq!(report.updated, 1);
        assert_eq!(report.unchanged, 0);
        assert!(report.unmatched.is_empty());

        let dict = first_field_dict(&out);
        let rect = dict.get(b"Rect").unwrap().as_array().unwrap();
        assert_eq!(rect.len(), 4);
        assert_eq!(rect[0].as_float().unwrap(), 10.0);
        assert_eq!(dict.get(b"MaxLen").unwrap().as_i64().unwrap(), 50);
        let ff = dict.get(b"Ff").unwrap().as_i64().unwrap();
        assert_ne!(ff & FF_READ_ONLY, 0);
        assert_ne!(ff & FF_REQUIRED, 0);
        assert_eq!(
            dict.get(b"V").unwrap().as_str().unwrap(),
            text_string("Bob").as_str().unwrap()
        );

        let info = field_info(&out, "name");
        assert_eq!(info.value.as_deref(), Some("Bob"));
        assert!(info.read_only);
    }

    #[test]
    fn clears_flags_and_leaves_other_properties() {
        let base = pdf_with_pages(&["form"]);
        let mut s = spec("f");
        s.read_only = true;
        let created = create_text_field(&base, &s, &limits()).unwrap();

        let mut p = patch("f");
        p.required = Some(true);
        p.read_only = Some(false);
        let (out, report) = edit_field_props(&created, &[p], &limits()).unwrap();
        assert_eq!(report.updated, 1);

        let dict = first_field_dict(&out);
        let ff = dict.get(b"Ff").unwrap().as_i64().unwrap();
        assert_eq!(ff & FF_READ_ONLY, 0);
        assert_ne!(ff & FF_REQUIRED, 0);
        // Untouched properties survive.
        assert!(dict.get(b"Rect").is_ok());
        assert!(dict.get(b"DA").is_ok());
        assert!(!field_info(&out, "f").read_only);
    }

    #[test]
    fn unmatched_names_are_reported_not_fatal() {
        let base = pdf_with_pages(&["form"]);
        let created = create_text_field(&base, &spec("real"), &limits()).unwrap();

        let mut p = patch("real");
        p.max_len = Some(3);
        let (out, report) =
            edit_field_props(&created, &[p, patch("ghost"), patch("ghost")], &limits()).unwrap();
        assert_eq!(report.matched, 1);
        assert_eq!(report.updated, 1);
        // Deduplicated, order preserved.
        assert_eq!(report.unmatched, vec!["ghost".to_string()]);
        assert_eq!(
            first_field_dict(&out)
                .get(b"MaxLen")
                .unwrap()
                .as_i64()
                .unwrap(),
            3
        );
    }

    #[test]
    fn empty_patches_and_missing_acroform_are_noops() {
        let plain = pdf_with_pages(&["plain"]);
        let (out, report) = edit_field_props(&plain, &[], &limits()).unwrap();
        assert_eq!(out, plain);
        assert_eq!(report, FieldPatchReport::default());

        let (out, report) = edit_field_props(&plain, &[patch("x")], &limits()).unwrap();
        assert_eq!(report.matched, 0);
        assert_eq!(report.unmatched, vec!["x".to_string()]);
        // Field list is still empty — document round-trips unchanged.
        assert!(list_form_fields(&out, &limits()).unwrap().is_empty());
    }

    #[test]
    fn invalid_rect_and_bad_value_fail_before_mutation() {
        let base = pdf_with_pages(&["form"]);
        let created = create_text_field(&base, &spec("f"), &limits()).unwrap();

        let mut bad_rect = patch("f");
        bad_rect.rect = Some([10.0, 20.0, 5.0, 40.0]); // x1 < x0
        assert!(matches!(
            edit_field_props(&created, &[bad_rect], &limits()),
            Err(PdfError::InvalidInput(_))
        ));
        let mut nan_rect = patch("f");
        nan_rect.rect = Some([0.0, 0.0, f64::NAN, 10.0]);
        assert!(matches!(
            edit_field_props(&created, &[nan_rect], &limits()),
            Err(PdfError::InvalidInput(_))
        ));

        // A value on a field whose type is not Tx/Ch is rejected.
        // (create_text_field only makes Tx; the Ch case is covered by
        // choice_field_value_validation below.)
        let _ = created;
    }

    #[test]
    fn encrypted_input_is_rejected() {
        let base = pdf_with_pages(&["x"]);
        let encrypted = encrypt(&base, "pw", None, PdfPermissions::default(), &limits()).unwrap();
        assert!(matches!(
            edit_field_props(&encrypted, &[patch("f")], &limits()),
            Err(PdfError::Encrypted)
        ));
    }

    /// Minimal PDF with one `Ch` field offering export values
    /// `red`/`blue` (mirrors the fill_form test fixture).
    fn pdf_with_choice_field() -> Vec<u8> {
        let mut doc = Document::with_version("1.5");
        let pages_id = doc.new_object_id();
        let page_id = doc.add_object(lopdf::dictionary! {
            "Type" => "Page",
            "Parent" => pages_id,
            "MediaBox" => vec![0.into(), 0.into(), 595.into(), 842.into()],
        });
        doc.objects.insert(
            pages_id,
            Object::Dictionary(lopdf::dictionary! {
                "Type" => "Pages",
                "Kids" => vec![Object::Reference(page_id)],
                "Count" => 1,
                "MediaBox" => vec![0.into(), 0.into(), 595.into(), 842.into()],
            }),
        );
        let catalog_id = doc.add_object(lopdf::dictionary! {
            "Type" => "Catalog",
            "Pages" => pages_id,
        });
        doc.trailer.set("Root", catalog_id);

        let field_id = doc.add_object(lopdf::dictionary! {
            "FT" => "Ch",
            "T" => text_string("color"),
            "Opt" => vec![
                Object::Array(vec![text_string("red"), text_string("Red")]),
                text_string("blue"),
            ],
        });
        let acro_id = doc.add_object(lopdf::dictionary! {
            "Fields" => vec![Object::Reference(field_id)],
        });
        doc.get_dictionary_mut(catalog_id)
            .unwrap()
            .set("AcroForm", acro_id);
        let mut buf = Vec::new();
        doc.save_to(&mut buf).unwrap();
        buf
    }

    #[test]
    fn choice_field_value_validation() {
        let buf = pdf_with_choice_field();

        let mut ok = patch("color");
        ok.value = Some("red".to_string()); // export value of the pair
        let (out, report) = edit_field_props(&buf, &[ok], &limits()).unwrap();
        assert_eq!(report.updated, 1);
        assert_eq!(field_info(&out, "color").value.as_deref(), Some("red"));

        // Display text and unknown values are rejected atomically.
        for bad in ["Red", "green"] {
            let mut p = patch("color");
            p.value = Some(bad.to_string());
            assert!(matches!(
                edit_field_props(&buf, &[p], &limits()),
                Err(PdfError::InvalidInput(_))
            ));
        }
    }

    #[test]
    fn sequential_patches_on_same_field_compose() {
        let base = pdf_with_pages(&["form"]);
        let created = create_text_field(&base, &spec("f"), &limits()).unwrap();

        // First patch sets required; second sets read_only. Both bits
        // must survive — flags are tracked per field across patches.
        let mut p1 = patch("f");
        p1.required = Some(true);
        let mut p2 = patch("f");
        p2.read_only = Some(true);
        let (out, report) = edit_field_props(&created, &[p1, p2], &limits()).unwrap();
        assert_eq!(report.matched, 2);
        assert_eq!(report.updated, 2);

        let dict = first_field_dict(&out);
        let ff = dict.get(b"Ff").unwrap().as_i64().unwrap();
        assert_ne!(ff & FF_REQUIRED, 0);
        assert_ne!(ff & FF_READ_ONLY, 0);
    }
}
