//! Optional content groups (OCGs / "layers").
//!
//! A PDF may declare optional-content groups in the document catalog's
//! `/OCProperties` dictionary:
//!
//! ```text
//! /OCProperties <<
//!   /OCGs [ << /Type /OCG /Name (Layer A) >> ... ]   % or indirect refs
//!   /D    << /Order [...] /ON [...] /OFF [...] /BaseState true >>
//!   /Configs [ << /Name (Print) /BaseState false ... >> ]
//! >>
//! ```
//!
//! Page content streams mark regions with `/OC /MCID` BDC/EMC operators;
//! this engine does **not** touch marked content — it only rewrites the
//! default configuration's visibility arrays. Per the PDF spec an OCG is
//! shown when it appears in `/ON` (or, when absent from both arrays, when
//! `/BaseState` is true) and hidden when it appears in `/OFF`. Only the
//! default configuration `/D` is managed; named configurations under
//! `/Configs` are left untouched.
//!
//! [`list_layers`] is read-only and tolerates encrypted input (names may
//! be undecodable ciphertext bytes — such entries are skipped). The two
//! transforming ops reject encrypted input via [`reject_encrypted`].

use std::collections::HashSet;

use lopdf::{Dictionary, Document, Object, ObjectId, decode_text_string};

use crate::crates::core::engine::EngineLimits;

use crate::crates::pdf_engine::error::{PdfError, PdfResult};

use super::load::{load_pdf, lopdf_err, reject_encrypted, save_pdf};

/// Description of one optional-content group ("layer"), as reported by
/// [`list_layers`].
#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub struct LayerInfo {
    /// The OCG's `/Name` text string.
    pub name: String,
    /// Object id of the OCG dictionary when it is an indirect object;
    /// `None` for inline (direct) OCG dictionaries.
    pub id: Option<ObjectId>,
    /// Effective visibility under the default configuration `/D`:
    /// `false` when the group is listed in `/OFF`, `true` when listed in
    /// `/ON`, otherwise the `/BaseState` value (default `true`). When
    /// `/BaseState` is `false` and the group is in neither array the
    /// layer is hidden by default — reported as `false`.
    pub visible_default: bool,
    /// Best-effort summary of the OCG's `/Usage` entry: view usage names
    /// (e.g. `Artwork, Image`) joined with `, ` when present, otherwise
    /// the `/Creator` or `/Language` text string. `None` when the OCG
    /// carries no usable usage information.
    pub usage: Option<String>,
}

/// An OCG discovered in `/OCProperties//OCGs`, with the data needed to
/// rewrite the `/ON`//`/OFF` arrays.
struct LayerEntry {
    info: LayerInfo,
    /// The object written into `/ON` or `/OFF` for this layer:
    /// `Reference(id)` for indirect OCGs, the inline dictionary
    /// otherwise.
    canonical: Object,
    /// Identity used for matching: object id when indirect, decoded
    /// `/Name` for inline dictionaries.
    key: LayerKey,
}

#[derive(Debug, Clone, PartialEq, Eq, Hash)]
enum LayerKey {
    Id(ObjectId),
    Name(String),
}

/// Where the catalog's `/OCProperties` dictionary lives.
#[derive(Debug, Clone, Copy)]
enum OcPropsShape {
    /// Inline dictionary inside the catalog.
    Direct,
    /// Indirect object.
    Indirect(ObjectId),
}

/// Where the default configuration `/D` dictionary lives inside
/// `/OCProperties`.
#[derive(Debug, Clone, Copy)]
enum DShape {
    /// Inline dictionary inside `/OCProperties`.
    Direct,
    /// Indirect object.
    Indirect(ObjectId),
    /// `/D` absent — a fresh dictionary will be created on write.
    Missing,
    /// `/D` present but neither a dictionary nor a reference.
    Malformed,
}

fn catalog_id(doc: &Document) -> Option<ObjectId> {
    doc.trailer.get(b"Root").ok()?.as_reference().ok()
}

/// Immutable view of the `/OCProperties` dictionary (direct or indirect).
fn oc_props_dict<'a>(doc: &'a Document) -> Option<&'a Dictionary> {
    let root = catalog_id(doc)?;
    let catalog = doc.get_dictionary(root).ok()?;
    match catalog.get(b"OCProperties").ok()? {
        Object::Dictionary(d) => Some(d),
        Object::Reference(id) => doc.get_dictionary(*id).ok(),
        _ => None,
    }
}

/// Shape of `/OCProperties` on the catalog, for the mutable write path.
fn oc_props_shape(doc: &Document) -> Option<OcPropsShape> {
    let root = catalog_id(doc)?;
    let catalog = doc.get_dictionary(root).ok()?;
    match catalog.get(b"OCProperties").ok()? {
        Object::Dictionary(_) => Some(OcPropsShape::Direct),
        Object::Reference(id) => Some(OcPropsShape::Indirect(*id)),
        _ => None,
    }
}

/// Shape of `/OCProperties//D`.
fn d_shape(doc: &Document) -> Option<DShape> {
    let ocprops = oc_props_dict(doc)?;
    Some(match ocprops.get(b"D") {
        Ok(Object::Dictionary(_)) => DShape::Direct,
        Ok(Object::Reference(id)) => DShape::Indirect(*id),
        Ok(_) => DShape::Malformed,
        Err(_) => DShape::Missing,
    })
}

/// Dereference `raw` and return it as an array's items (empty on any
/// mismatch — callers treat missing/malformed as "no entries").
fn array_items<'a>(doc: &'a Document, raw: &'a Object) -> Vec<&'a Object> {
    let Ok((_, obj)) = doc.dereference(raw) else {
        return Vec::new();
    };
    obj.as_array()
        .map(|a| a.iter().collect())
        .unwrap_or_default()
}

/// Collect every OCG in `/OCProperties//OCGs`, in document order.
fn collect_layers(doc: &Document) -> Vec<LayerEntry> {
    let Some(ocprops) = oc_props_dict(doc) else {
        return Vec::new();
    };
    let Ok(ocgs_raw) = ocprops.get(b"OCGs") else {
        return Vec::new();
    };
    let items = array_items(doc, ocgs_raw);
    if items.is_empty() {
        return Vec::new();
    }

    // Default-config visibility inputs.
    let (base_state, on_ids, off_ids) = default_config_state(doc, ocprops);

    let mut layers = Vec::with_capacity(items.len());
    let mut seen = HashSet::new();
    for item in items {
        let Ok((id_opt, obj)) = doc.dereference(item) else {
            continue;
        };
        let Ok(dict) = obj.as_dict() else {
            continue;
        };
        // /Type should be /OCG; tolerate missing /Type but skip entries
        // explicitly typed as something else.
        if let Ok(t) = dict.get(b"Type")
            && let Ok(name) = t.as_name()
            && name != b"OCG"
        {
            continue;
        }
        let Ok(name_obj) = dict.get(b"Name") else {
            continue;
        };
        let Ok(name) = decode_text_string(name_obj) else {
            continue;
        };
        let key = match id_opt {
            Some(id) => LayerKey::Id(id),
            None => LayerKey::Name(name.clone()),
        };
        if !seen.insert(key.clone()) {
            continue; // duplicate entry — report once
        }

        let visible_default = match &key {
            LayerKey::Id(id) if off_ids.contains(id) => false,
            LayerKey::Id(id) if on_ids.contains(id) => true,
            _ => base_state,
        };
        let canonical = match id_opt {
            Some(id) => Object::Reference(id),
            None => item.clone(),
        };
        layers.push(LayerEntry {
            info: LayerInfo {
                name,
                id: id_opt,
                visible_default,
                usage: usage_text(doc, dict),
            },
            canonical,
            key,
        });
    }
    layers
}

/// `(BaseState, ON ids, OFF ids)` of the default configuration `/D`.
/// Missing pieces default to `true` / empty sets.
fn default_config_state(
    doc: &Document,
    ocprops: &Dictionary,
) -> (bool, HashSet<ObjectId>, HashSet<ObjectId>) {
    let mut base_state = true;
    let mut on_ids = HashSet::new();
    let mut off_ids = HashSet::new();

    let Ok(d_raw) = ocprops.get(b"D") else {
        return (base_state, on_ids, off_ids);
    };
    let Ok((_, d_obj)) = doc.dereference(d_raw) else {
        return (base_state, on_ids, off_ids);
    };
    let Ok(d) = d_obj.as_dict() else {
        return (base_state, on_ids, off_ids);
    };

    if let Ok(v) = d.get(b"BaseState").and_then(|o| o.as_bool()) {
        base_state = v;
    }
    for (key, set) in [(&b"ON"[..], &mut on_ids), (&b"OFF"[..], &mut off_ids)] {
        if let Ok(raw) = d.get(key) {
            for entry in array_items(doc, raw) {
                if let Ok((Some(id), _)) = doc.dereference(entry) {
                    set.insert(id);
                }
            }
        }
    }
    (base_state, on_ids, off_ids)
}

/// Best-effort `/Usage` summary for one OCG dictionary.
fn usage_text(doc: &Document, dict: &Dictionary) -> Option<String> {
    let raw = dict.get(b"Usage").ok()?;
    let (_, usage) = doc.dereference(raw).ok()?;
    match usage {
        Object::Dictionary(d) => usage_dict_text(doc, d),
        Object::String(..) => decode_text_string(usage).ok(),
        Object::Name(n) => Some(String::from_utf8_lossy(n).into_owned()),
        _ => None,
    }
}

/// Extract usage text from an OCG `/Usage` dictionary: prefer the
/// `/View` dictionary's `/Usage` name array (e.g. `/Artwork /Image`),
/// fall back to `/Creator` then `/Language` text strings.
fn usage_dict_text(doc: &Document, d: &Dictionary) -> Option<String> {
    if let Some(names) = view_usage_names(doc, d) {
        return Some(names);
    }
    for key in [&b"Creator"[..], &b"Language"[..]] {
        if let Ok(raw) = d.get(key)
            && let Ok(text) = decode_text_string(raw)
            && !text.is_empty()
        {
            return Some(text);
        }
    }
    None
}

/// `/Usage//View//Usage` name array joined with `, `.
fn view_usage_names(doc: &Document, d: &Dictionary) -> Option<String> {
    let view_raw = d.get(b"View").ok()?;
    let (_, view_obj) = doc.dereference(view_raw).ok()?;
    let view = view_obj.as_dict().ok()?;
    let usage_raw = view.get(b"Usage").ok()?;
    let names: Vec<String> = array_items(doc, usage_raw)
        .iter()
        .filter_map(|o| o.as_name().ok())
        .map(|n| String::from_utf8_lossy(n).into_owned())
        .collect();
    if names.is_empty() {
        None
    } else {
        Some(names.join(", "))
    }
}

/// List the document's optional-content groups (layers).
///
/// Returns one [`LayerInfo`] per OCG in `/OCProperties//OCGs`, in
/// document order. Documents without optional content yield an empty
/// list. Read-only: encrypted input is allowed (structure is still
/// reported; undecodable names are skipped).
pub fn list_layers(input: &[u8], limits: &EngineLimits) -> PdfResult<Vec<LayerInfo>> {
    let doc = load_pdf(input, limits)?;
    Ok(collect_layers(&doc)
        .into_iter()
        .map(|entry| entry.info)
        .collect())
}

/// Set the default-configuration visibility of the named layers.
///
/// Rewrites `/OCProperties//D//ON` and `/OFF`: layers named in `names`
/// are moved into `/ON` (when `visible` is `true`) or `/OFF` (when
/// `false`); their previous entries in the opposite array are removed.
/// Layers not named keep their existing `/ON`//`/OFF` membership, and
/// `/BaseState`, `/Order` and `/Configs` are untouched. `/OC` marked
/// content in page streams is not modified.
///
/// Every name in `names` must match an OCG `/Name` exactly; if any name
/// is unmatched the whole call fails with [`PdfError::InvalidInput`]
/// listing the unknown names and the document is left unmodified.
/// Encrypted input is rejected.
pub fn set_layer_visibility(
    input: &[u8],
    names: &[String],
    visible: bool,
    limits: &EngineLimits,
) -> PdfResult<Vec<u8>> {
    let mut doc = load_pdf(input, limits)?;
    reject_encrypted(&doc)?;

    let layers = collect_layers(&doc);

    // Match requested names against layer names. A name shared by
    // several OCGs toggles all of them; each requested name must match
    // at least one layer.
    let matched: Vec<usize> = layers
        .iter()
        .enumerate()
        .filter(|(_, l)| names.iter().any(|n| n == &l.info.name))
        .map(|(i, _)| i)
        .collect();
    let unmatched: Vec<&str> = names
        .iter()
        .filter(|n| !layers.iter().any(|l| &l.info.name == *n))
        .map(String::as_str)
        .collect();
    if !unmatched.is_empty() {
        return Err(PdfError::InvalidInput(format!(
            "unknown optional-content group name(s): {}",
            unmatched.join(", ")
        )));
    }
    if matched.is_empty() {
        // Nothing to change (empty `names`, or no OCGs at all).
        return save_pdf(&mut doc);
    }

    let matched_keys: HashSet<&LayerKey> = matched.iter().map(|&i| &layers[i].key).collect();

    // Current /ON and /OFF arrays (cloned objects; missing → empty).
    let ocprops = oc_props_dict(&doc);
    let (mut on, mut off) = match ocprops.and_then(|p| default_config_dict(&doc, p)) {
        Some(d) => (config_array(&doc, d, b"ON"), config_array(&doc, d, b"OFF")),
        None => (Vec::new(), Vec::new()),
    };

    // Drop every entry that belongs to a matched layer, then append the
    // canonical object to the target array. Unmatched entries are kept
    // verbatim, preserving order.
    on.retain(|o| !entry_matches(&doc, o, &matched_keys));
    off.retain(|o| !entry_matches(&doc, o, &matched_keys));
    let target = if visible { &mut on } else { &mut off };
    for &i in &matched {
        target.push(layers[i].canonical.clone());
    }

    write_visibility_arrays(&mut doc, on, off)?;
    save_pdf(&mut doc)
}

/// Remove `/OCProperties` from the document catalog.
///
/// Optional-content marked content (`/OC` BDC/EMC, `/Properties`
/// resources) is left in place; without a configuration dictionary
/// viewers render all optional content, i.e. every former layer becomes
/// always-visible. Encrypted input is rejected.
pub fn remove_oc_properties(input: &[u8], limits: &EngineLimits) -> PdfResult<Vec<u8>> {
    let mut doc = load_pdf(input, limits)?;
    reject_encrypted(&doc)?;

    let root = catalog_id(&doc)
        .ok_or_else(|| PdfError::InvalidInput("PDF has no catalog root".to_string()))?;
    let catalog = doc.get_dictionary_mut(root).map_err(lopdf_err)?;
    catalog.remove(b"OCProperties");
    save_pdf(&mut doc)
}

/// The default configuration `/D` dictionary of an `/OCProperties` dict.
fn default_config_dict<'a>(doc: &'a Document, ocprops: &'a Dictionary) -> Option<&'a Dictionary> {
    let raw = ocprops.get(b"D").ok()?;
    let (_, obj) = doc.dereference(raw).ok()?;
    obj.as_dict().ok()
}

/// Clone of a `/D` configuration array (`ON`/`OFF`), empty when absent
/// or malformed.
fn config_array(doc: &Document, d: &Dictionary, key: &[u8]) -> Vec<Object> {
    let Ok(raw) = d.get(key) else {
        return Vec::new();
    };
    let Ok((_, obj)) = doc.dereference(raw) else {
        return Vec::new();
    };
    obj.as_array().cloned().unwrap_or_default()
}

/// Whether an `/ON` or `/OFF` array entry refers to one of the matched
/// layers — by object id for indirect OCGs, by decoded `/Name` for
/// inline ones.
fn entry_matches(doc: &Document, entry: &Object, matched_keys: &HashSet<&LayerKey>) -> bool {
    let Ok((id_opt, obj)) = doc.dereference(entry) else {
        return false;
    };
    if let Some(id) = id_opt {
        return matched_keys.contains(&LayerKey::Id(id));
    }
    let Ok(dict) = obj.as_dict() else {
        return false;
    };
    let Ok(name_obj) = dict.get(b"Name") else {
        return false;
    };
    let Ok(name) = decode_text_string(name_obj) else {
        return false;
    };
    matched_keys.contains(&LayerKey::Name(name))
}

/// Mutable access to the `/OCProperties` dictionary given its shape.
fn ocprops_dict_mut<'a>(
    doc: &'a mut Document,
    shape: OcPropsShape,
) -> PdfResult<&'a mut Dictionary> {
    match shape {
        OcPropsShape::Indirect(id) => doc.get_dictionary_mut(id).map_err(lopdf_err),
        OcPropsShape::Direct => {
            let root = catalog_id(doc)
                .ok_or_else(|| PdfError::InvalidInput("PDF has no catalog root".to_string()))?;
            let catalog = doc.get_dictionary_mut(root).map_err(lopdf_err)?;
            match catalog.get_mut(b"OCProperties").map_err(lopdf_err)? {
                Object::Dictionary(d) => Ok(d),
                _ => Err(PdfError::InvalidInput(
                    "malformed /OCProperties entry".to_string(),
                )),
            }
        }
    }
}

/// Write the new `/ON` and `/OFF` arrays into `/OCProperties//D`,
/// creating `/D` when absent. Other `/D` keys (`/BaseState`, `/Order`,
/// ...) and `/Configs` are preserved.
fn write_visibility_arrays(doc: &mut Document, on: Vec<Object>, off: Vec<Object>) -> PdfResult<()> {
    let oc_shape = oc_props_shape(doc).ok_or_else(|| {
        PdfError::InvalidInput("document has no /OCProperties dictionary".to_string())
    })?;
    let d_shape = d_shape(doc).unwrap_or(DShape::Missing);
    if matches!(d_shape, DShape::Malformed) {
        return Err(PdfError::InvalidInput(
            "malformed /OCProperties//D entry".to_string(),
        ));
    }

    // Indirect /D can be updated without touching /OCProperties.
    if let DShape::Indirect(id) = d_shape {
        let d = doc.get_dictionary_mut(id).map_err(lopdf_err)?;
        d.set("ON", Object::Array(on));
        d.set("OFF", Object::Array(off));
        return Ok(());
    }

    let ocprops = ocprops_dict_mut(doc, oc_shape)?;
    match d_shape {
        DShape::Direct => {
            let d_obj = ocprops.get_mut(b"D").map_err(lopdf_err)?;
            let d = d_obj.as_dict_mut().map_err(lopdf_err)?;
            d.set("ON", Object::Array(on));
            d.set("OFF", Object::Array(off));
        }
        DShape::Missing => {
            let mut d = Dictionary::new();
            d.set("ON", Object::Array(on));
            d.set("OFF", Object::Array(off));
            ocprops.set("D", Object::Dictionary(d));
        }
        DShape::Indirect(_) | DShape::Malformed => unreachable!(),
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::crates::pdf_engine::error::PdfError;
    use crate::crates::pdf_engine::ops::test_utils::pdf_with_pages;
    use lopdf::dictionary;

    /// Build a one-page PDF whose catalog carries an `/OCProperties`
    /// dictionary with two indirect OCGs ("Layer A", "Layer B"), a
    /// `/Usage` entry on A, and a `/D` config with B in `/OFF`.
    /// Returns (pdf bytes, ocg_a_id, ocg_b_id).
    fn pdf_with_layers() -> (Vec<u8>, ObjectId, ObjectId) {
        let mut doc = Document::load_mem(&pdf_with_pages(&["layered"])).unwrap();
        let root_id = doc.trailer.get(b"Root").unwrap().as_reference().unwrap();

        let ocg_a = doc.add_object(dictionary! {
            "Type" => "OCG",
            "Name" => Object::string_literal("Layer A"),
            "Usage" => dictionary! {
                "View" => dictionary! {
                    "Usage" => Object::Array(vec![
                        Object::Name(b"Artwork".to_vec()),
                        Object::Name(b"Image".to_vec()),
                    ]),
                },
            },
        });
        let ocg_b = doc.add_object(dictionary! {
            "Type" => "OCG",
            "Name" => Object::string_literal("Layer B"),
            "Usage" => dictionary! {
                "Creator" => Object::string_literal("TestSuite"),
            },
        });

        let ocprops = dictionary! {
            "OCGs" => Object::Array(vec![
                Object::Reference(ocg_a),
                Object::Reference(ocg_b),
            ]),
            "D" => dictionary! {
                "BaseState" => true,
                "Order" => Object::Array(vec![
                    Object::Reference(ocg_a),
                    Object::Reference(ocg_b),
                ]),
                "ON" => Object::Array(vec![]),
                "OFF" => Object::Array(vec![Object::Reference(ocg_b)]),
            },
        };
        doc.get_dictionary_mut(root_id)
            .unwrap()
            .set("OCProperties", ocprops);

        let mut buf = Vec::new();
        doc.save_to(&mut buf).unwrap();
        (buf, ocg_a, ocg_b)
    }

    /// Read back the `/ON` and `/OFF` arrays of `/OCProperties//D`.
    fn on_off(pdf: &[u8]) -> (Vec<ObjectId>, Vec<ObjectId>) {
        let doc = Document::load_mem(pdf).unwrap();
        let ocprops = oc_props_dict(&doc).expect("OCProperties present");
        let d = default_config_dict(&doc, ocprops).expect("/D present");
        let ids = |key: &[u8]| -> Vec<ObjectId> {
            let Ok(raw) = d.get(key) else {
                return Vec::new();
            };
            let Ok((_, obj)) = doc.dereference(raw) else {
                return Vec::new();
            };
            obj.as_array()
                .map(|a| a.iter().filter_map(|o| o.as_reference().ok()).collect())
                .unwrap_or_default()
        };
        (ids(b"ON"), ids(b"OFF"))
    }

    #[test]
    fn list_layers_reports_names_and_defaults() {
        let (pdf, ocg_a, ocg_b) = pdf_with_layers();
        let layers = list_layers(&pdf, &EngineLimits::default()).unwrap();

        assert_eq!(layers.len(), 2);
        assert_eq!(layers[0].name, "Layer A");
        assert_eq!(layers[0].id, Some(ocg_a));
        assert!(layers[0].visible_default, "A not in OFF, BaseState true");
        assert_eq!(layers[0].usage.as_deref(), Some("Artwork, Image"));

        assert_eq!(layers[1].name, "Layer B");
        assert_eq!(layers[1].id, Some(ocg_b));
        assert!(!layers[1].visible_default, "B is in /OFF");
        assert_eq!(layers[1].usage.as_deref(), Some("TestSuite"));
    }

    #[test]
    fn list_layers_empty_without_ocproperties() {
        let plain = pdf_with_pages(&["no layers"]);
        assert!(
            list_layers(&plain, &EngineLimits::default())
                .unwrap()
                .is_empty()
        );
    }

    #[test]
    fn set_layer_visibility_rewrites_on_off() {
        let (pdf, ocg_a, ocg_b) = pdf_with_layers();
        // Hide A only.
        let out = set_layer_visibility(
            &pdf,
            &["Layer A".to_string()],
            false,
            &EngineLimits::default(),
        )
        .unwrap();
        let (on, off) = on_off(&out);
        assert!(off.contains(&ocg_a), "A moved to /OFF");
        assert!(off.contains(&ocg_b), "B stays in /OFF");
        assert!(!on.contains(&ocg_a));

        // Show B (removes it from /OFF, adds to /ON).
        let out = set_layer_visibility(
            &out,
            &["Layer B".to_string()],
            true,
            &EngineLimits::default(),
        )
        .unwrap();
        let (on, off) = on_off(&out);
        assert!(on.contains(&ocg_b), "B moved to /ON");
        assert!(!off.contains(&ocg_b), "B removed from /OFF");
        assert!(off.contains(&ocg_a), "A still hidden");
    }

    #[test]
    fn set_layer_visibility_rejects_unknown_names() {
        let (pdf, _, _) = pdf_with_layers();
        let err = set_layer_visibility(
            &pdf,
            &["Layer A".to_string(), "No Such Layer".to_string()],
            true,
            &EngineLimits::default(),
        )
        .unwrap_err();
        assert!(
            matches!(err, PdfError::InvalidInput(ref m) if m.contains("No Such Layer")),
            "unexpected error: {err:?}"
        );
    }

    #[test]
    fn remove_oc_properties_drops_catalog_entry() {
        let (pdf, _, _) = pdf_with_layers();
        let out = remove_oc_properties(&pdf, &EngineLimits::default()).unwrap();
        let doc = Document::load_mem(&out).unwrap();
        let root_id = doc.trailer.get(b"Root").unwrap().as_reference().unwrap();
        let catalog = doc.get_dictionary(root_id).unwrap();
        assert!(catalog.get(b"OCProperties").is_err());
        assert!(
            list_layers(&out, &EngineLimits::default())
                .unwrap()
                .is_empty()
        );
    }

    #[test]
    fn transforms_reject_encrypted() {
        let plain = pdf_with_pages(&["x"]);
        let encrypted = crate::crates::pdf_engine::ops::encrypt::encrypt(
            &plain,
            "pw",
            None,
            crate::crates::pdf_engine::ops::encrypt::PdfPermissions::default(),
            &EngineLimits::default(),
        )
        .unwrap();
        assert!(matches!(
            set_layer_visibility(
                &encrypted,
                &["a".to_string()],
                true,
                &EngineLimits::default()
            ),
            Err(PdfError::Encrypted)
        ));
        assert!(matches!(
            remove_oc_properties(&encrypted, &EngineLimits::default()),
            Err(PdfError::Encrypted)
        ));
    }
}
