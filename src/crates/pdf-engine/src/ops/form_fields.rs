//! AcroForm field inspection, creation and deletion.
//!
//! [`list_form_fields`] walks the catalog `/AcroForm /Fields` tree
//! recursively (`/Kids`) and reports every terminal field: its full
//! dotted name (the `/T` partial names joined along the field
//! hierarchy), its field type (`/FT`: `Tx`, `Btn`, `Ch`, `Sig`),
//! current `/V` value, `/Opt` choice options, the read-only flag
//! (`/Ff` bit 1) and — when it can be determined — the 1-based page
//! number the field's widget annotation lives on.
//!
//! [`create_text_field`] appends a **merged field/widget** dictionary
//! (one object carrying both `/FT /Tx` field keys and `/Subtype
//! /Widget` annotation keys) to `/AcroForm /Fields` and to the target
//! page's `/Annots`. A merged widget is the most compact and widely
//! supported representation; separate field + widget kid objects are
//! not produced. `/AcroForm`, `/Fields` and the default-resource
//! `/DR << /Font << /Helv … >> >>` entries are created on demand so the
//! new field's `/DA` (`0 0 0 rg /Helv 12 Tf`) always resolves.
//!
//! [`delete_form_field`] removes the named field (first match in
//! document order when names are duplicated) from its parent `/Fields`
//! or `/Kids` array — which removes the whole subtree — and removes
//! every widget annotation in that subtree from every page's `/Annots`.
//! Parent nodes left with no children and no value/options are pruned
//! so they do not reappear as ghost fields.
//!
//! # Honest limitations
//!
//! - **Page resolution** uses the widget's `/P` entry first, then a
//!   scan of page `/Annots` arrays keyed by indirect object id.
//!   Widget annotations stored as *direct* (inline) dictionaries inside
//!   `/Annots` cannot be mapped back to a page.
//! - **`/Opt` export values**: choice options given as `[export,
//!   display]` pairs are reported by their *export* value (the first
//!   element, per ISO 32000), which is also what `/V` must contain.
//! - **Field flags** (`/Ff`) and field type (`/FT`) are inheritable in
//!   PDF; the effective value from the nearest ancestor is used.
//! - Deleting a field whose name matches several nodes removes only
//!   the first in document order.

use std::collections::{HashMap, HashSet, VecDeque};

use lopdf::{Document, Object, ObjectId, decode_text_string, dictionary, text_string};
use serde::{Deserialize, Serialize};

use crate::crates::core::engine::EngineLimits;

use crate::crates::pdf_engine::error::{PdfError, PdfResult};

use super::load::{load_pdf, lopdf_err, reject_encrypted, save_pdf};

/// Safety caps for pathological AcroForm trees.
const MAX_FIELD_DEPTH: usize = 64;
const MAX_FIELDS: usize = 4096;
const MAX_ARRAYS: usize = 65_536;
const MAX_PRUNE_ROUNDS: usize = 512;
const MAX_NAME_BYTES: usize = 8 * 1024;
const MAX_VALUE_BYTES: usize = 1024 * 1024;
const MAX_OPTIONS: usize = 1024;

/// Description of one AcroForm field.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct FieldInfo {
    /// Full dotted field name built from the `/T` hierarchy
    /// (e.g. `form1.name`). Fields without any `/T` in their ancestry
    /// may produce an empty string.
    pub name: String,
    /// Field type from `/FT` (`Tx`, `Btn`, `Ch`, `Sig`), inheritable
    /// from parent fields. Empty when the document does not declare one.
    pub type_name: String,
    /// 1-based page number of the field's widget, when resolvable.
    pub page: Option<u32>,
    /// Current `/V` value decoded as a text string. For button fields
    /// this is the state name (e.g. `Yes`, `/Off` values appear without
    /// the slash as a name string).
    pub value: Option<String>,
    /// Choice options from `/Opt` for `Ch` fields — the *export* value
    /// of each option (first element of `[export, display]` pairs).
    /// Empty for other field types.
    pub options: Vec<String>,
    /// Whether the field is read-only (`/Ff` bit 1 set, inherited).
    pub read_only: bool,
}

/// Specification for [`create_text_field`].
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct TextFieldSpec {
    /// Full field name, stored literally in `/T`. Dots are *not*
    /// expanded into a field hierarchy — the name is kept as a single
    /// partial name, which is how most producers write flat names.
    pub name: String,
    /// 1-based page number that receives the widget annotation.
    pub page: u32,
    /// Widget rectangle `[x0, y0, x1, y1]` in default user space
    /// units. Must satisfy `x1 > x0` and `y1 > y0`.
    pub rect: [f64; 4],
    /// Initial `/V` value. `None` leaves the field empty.
    pub value: Option<String>,
    /// Sets the read-only flag (`/Ff` bit 1).
    pub read_only: bool,
}

/// A widget annotation referenced by a field, as discovered during
/// traversal. Shared with sibling ops (`fill_form`) as `pub(crate)`.
#[derive(Debug, Clone)]
pub(crate) struct WidgetRef {
    /// Indirect object id of the widget dictionary, when it is one.
    pub id: Option<ObjectId>,
    /// The widget's `/Rect`, when present and well-formed.
    pub rect: Option<[f64; 4]>,
}

/// Internal per-field record produced by [`collect_fields`]. Carries
/// everything [`FieldInfo`] exposes plus the object id and widget list
/// needed for mutation by sibling ops.
#[derive(Debug, Clone)]
pub(crate) struct FieldEntry {
    pub name: String,
    pub type_name: String,
    pub value: Option<String>,
    pub options: Vec<String>,
    pub read_only: bool,
    pub page: Option<u32>,
    /// Indirect object id of the field dictionary (`None` only for
    /// direct/inline dictionaries, which [`normalize_acroform`] removes).
    pub id: Option<ObjectId>,
    /// Widget annotations belonging to this field (merged self-widget
    /// first, then `/Kids` widgets in order).
    pub widgets: Vec<WidgetRef>,
}

// ---------------------------------------------------------------------------
// Public operations
// ---------------------------------------------------------------------------

/// List every AcroForm field in `input`.
pub fn list_form_fields(input: &[u8], limits: &EngineLimits) -> PdfResult<Vec<FieldInfo>> {
    let doc = load_pdf(input, limits)?;
    reject_encrypted(&doc)?;
    Ok(collect_fields(&doc)
        .into_iter()
        .map(|e| FieldInfo {
            name: e.name,
            type_name: e.type_name,
            page: e.page,
            value: e.value,
            options: e.options,
            read_only: e.read_only,
        })
        .collect())
}

/// Create a text (`Tx`) form field on `spec.page` and return the
/// modified PDF.
pub fn create_text_field(
    input: &[u8],
    spec: &TextFieldSpec,
    limits: &EngineLimits,
) -> PdfResult<Vec<u8>> {
    let mut doc = load_pdf(input, limits)?;
    reject_encrypted(&doc)?;

    if spec.name.is_empty() {
        return Err(PdfError::InvalidInput(
            "form field name must not be empty".to_string(),
        ));
    }
    if spec.name.len() > MAX_NAME_BYTES {
        return Err(PdfError::InvalidInput(format!(
            "form field name exceeds {MAX_NAME_BYTES} bytes"
        )));
    }
    if !spec.rect.iter().all(|v| v.is_finite())
        || spec.rect[2] <= spec.rect[0]
        || spec.rect[3] <= spec.rect[1]
    {
        return Err(PdfError::InvalidInput(
            "field rect must be [x0, y0, x1, y1] with x1 > x0 and y1 > y0".to_string(),
        ));
    }
    if let Some(value) = &spec.value
        && value.len() > MAX_VALUE_BYTES
    {
        return Err(PdfError::InvalidInput(format!(
            "field value exceeds {MAX_VALUE_BYTES} bytes"
        )));
    }

    let pages = doc.get_pages();
    let Some(&page_id) = pages.get(&spec.page) else {
        return Err(PdfError::InvalidPageRange(format!(
            "page {} out of range (document has {} pages)",
            spec.page,
            pages.len()
        )));
    };

    let acro_id = ensure_acroform(&mut doc)?;
    ensure_fields_array(&mut doc, acro_id)?;
    // Creates /DR << /Font << /Helv … >> >> when missing and sets
    // /NeedAppearances so viewers regenerate appearances.
    ensure_helv_font(&mut doc, acro_id)?;

    // Merged field + widget annotation dictionary.
    let ff: i64 = i64::from(spec.read_only);
    let rect: Vec<Object> = spec.rect.iter().map(|v| Object::from(*v)).collect();
    let mut field = dictionary! {
        "Type" => "Annot",
        "Subtype" => "Widget",
        "FT" => "Tx",
        "T" => text_string(&spec.name),
        "Rect" => rect,
        "P" => page_id,
        "F" => 4,
        "Ff" => ff,
        "DA" => text_string("0 0 0 rg /Helv 12 Tf"),
        "Q" => 0,
        "MK" => dictionary! {
            "BG" => vec![Object::Integer(1), Object::Integer(1), Object::Integer(1)],
            "BC" => vec![Object::Integer(0), Object::Integer(0), Object::Integer(0)],
        },
        "BS" => dictionary! {
            "W" => 1,
            "S" => "S",
        },
    };
    if let Some(value) = &spec.value {
        field.set("V", text_string(value));
    }
    let field_id = doc.add_object(Object::Dictionary(field));

    let fields_loc = named_array_loc(&doc, acro_id, b"Fields")
        .ok_or_else(|| PdfError::InvalidInput("AcroForm /Fields is not an array".to_string()))?;
    push_array_element(&mut doc, &fields_loc, Object::Reference(field_id))?;

    match named_array_loc(&doc, page_id, b"Annots") {
        Some(annots_loc) => {
            push_array_element(&mut doc, &annots_loc, Object::Reference(field_id))?;
        }
        None => {
            let has_annots = doc
                .get_dictionary(page_id)
                .map_err(lopdf_err)?
                .has(b"Annots");
            if has_annots {
                return Err(PdfError::InvalidInput(
                    "page /Annots entry is not an array".to_string(),
                ));
            }
            doc.get_dictionary_mut(page_id)
                .map_err(lopdf_err)?
                .set("Annots", Object::Array(vec![Object::Reference(field_id)]));
        }
    }

    save_pdf(&mut doc)
}

/// Delete the form field named `name` (full dotted name, exact match)
/// and return the modified PDF.
pub fn delete_form_field(input: &[u8], name: &str, limits: &EngineLimits) -> PdfResult<Vec<u8>> {
    let mut doc = load_pdf(input, limits)?;
    reject_encrypted(&doc)?;

    if name.is_empty() {
        return Err(PdfError::InvalidInput(
            "form field name must not be empty".to_string(),
        ));
    }
    let Some(acro_id) = acroform_dict_id(&mut doc)? else {
        return Err(PdfError::InvalidInput(
            "document has no AcroForm fields".to_string(),
        ));
    };
    // Promote every field/widget dictionary to an indirect object so
    // container arrays can be addressed by id.
    normalize_acroform(&mut doc)?;

    let nodes = scan_fields(&doc, acro_id)?;
    let Some(node) = nodes.iter().find(|n| n.name == name) else {
        return Err(PdfError::InvalidInput(format!(
            "no form field named {name:?}"
        )));
    };
    let (array_loc, index, field_id) = (node.loc.clone(), node.index, node.id);
    let mut widget_ids = Vec::new();
    collect_subtree_widgets(&doc, field_id, is_widget_dict(&doc, &node.dict), &mut widget_ids, 0);

    // Best effort: drop the widget annotations from every page.
    for widget_id in widget_ids {
        remove_annot_from_pages(&mut doc, widget_id);
    }

    remove_array_element(&mut doc, &array_loc, index)?;
    prune_empty_fields(&mut doc)?;

    save_pdf(&mut doc)
}

// ---------------------------------------------------------------------------
// Field-tree traversal (read side)
// ---------------------------------------------------------------------------

/// Resolve an object to `(object id, dictionary)` following
/// references. The id is `None` for inline dictionaries.
fn resolve_dict<'a>(
    doc: &'a Document,
    obj: &'a Object,
) -> Option<(Option<ObjectId>, &'a lopdf::Dictionary)> {
    let (id, resolved) = doc.dereference(obj).ok()?;
    Some((id, resolved.as_dict().ok()?))
}

/// Decode a PDF text string or name object to `String`.
pub(crate) fn object_text(doc: &Document, obj: &Object) -> Option<String> {
    let (_, resolved) = doc.dereference(obj).ok()?;
    if let Ok(s) = decode_text_string(resolved) {
        return Some(s);
    }
    resolved
        .as_name()
        .ok()
        .map(|n| String::from_utf8_lossy(n).into_owned())
}

/// `/T` partial field name.
fn partial_name(doc: &Document, dict: &lopdf::Dictionary) -> Option<String> {
    dict.get(b"T")
        .ok()
        .and_then(|o| object_text(doc, o))
}

/// `/FT` field type name (`Tx`, `Btn`, ...).
fn field_type_name(doc: &Document, dict: &lopdf::Dictionary) -> Option<String> {
    dict.get(b"FT")
        .ok()
        .and_then(|o| doc.dereference(o).ok())
        .and_then(|(_, o)| o.as_name().ok())
        .map(|n| String::from_utf8_lossy(n).into_owned())
}

/// Effective `/Ff` flags (own value or inherited default).
fn field_flags(doc: &Document, dict: &lopdf::Dictionary) -> Option<i64> {
    dict.get(b"Ff")
        .ok()
        .and_then(|o| doc.dereference(o).ok())
        .and_then(|(_, o)| o.as_i64().ok())
}

/// `/V` field value decoded as text (names decode too, for `Btn`).
fn field_value(doc: &Document, dict: &lopdf::Dictionary) -> Option<String> {
    dict.get(b"V")
        .ok()
        .and_then(|o| object_text(doc, o))
}

/// `/Opt` choice options — export values only (first element of
/// `[export, display]` pairs), capped at [`MAX_OPTIONS`].
fn field_options(doc: &Document, dict: &lopdf::Dictionary) -> Vec<String> {
    let mut out = Vec::new();
    let Ok(opt_obj) = dict.get(b"Opt") else {
        return out;
    };
    let Ok((_, opt)) = doc.dereference(opt_obj) else {
        return out;
    };
    let Ok(items) = opt.as_array() else {
        return out;
    };
    for item in items.iter().take(MAX_OPTIONS) {
        let Ok((_, resolved)) = doc.dereference(item) else {
            continue;
        };
        match resolved {
            Object::Array(pair) => {
                if let Some(first) = pair.first()
                    && let Ok((_, fo)) = doc.dereference(first)
                    && let Ok(s) = decode_text_string(fo)
                {
                    out.push(s);
                }
            }
            _ => {
                if let Ok(s) = decode_text_string(resolved) {
                    out.push(s);
                }
            }
        }
    }
    out
}

/// Parse a `/Rect`-style 4-number array.
fn rect_from_dict(dict: &lopdf::Dictionary) -> Option<[f64; 4]> {
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

/// Whether a dictionary is a widget annotation rather than a
/// sub-field: explicit `/Subtype /Widget`, or an annotation-looking
/// dict (`/Rect`, no `/T`, `/FT` or `/Kids`).
fn is_widget_dict(doc: &Document, dict: &lopdf::Dictionary) -> bool {
    let subtype_widget = dict
        .get(b"Subtype")
        .ok()
        .and_then(|o| doc.dereference(o).ok())
        .and_then(|(_, o)| o.as_name().ok())
        .map(|n| n == b"Widget")
        .unwrap_or(false);
    if subtype_widget {
        return true;
    }
    dict.has(b"Rect") && !dict.has(b"T") && !dict.has(b"FT") && !dict.has(b"Kids")
}

/// Join a parent path and a `/T` partial name into a dotted path.
fn join_name(path: &str, t: Option<&str>) -> String {
    match t {
        None | Some("") => path.to_string(),
        Some(t) if path.is_empty() => t.to_string(),
        Some(t) => format!("{path}.{t}"),
    }
}

/// Page lookup tables: page object id → 1-based page number, and
/// widget annotation object id → page number.
struct PageMaps {
    by_id: HashMap<ObjectId, u32>,
    annot: HashMap<ObjectId, u32>,
}

fn page_maps(doc: &Document) -> PageMaps {
    let pages = doc.get_pages();
    let by_id: HashMap<ObjectId, u32> = pages.iter().map(|(&n, &id)| (id, n)).collect();
    let mut annot = HashMap::new();
    for (&n, &pid) in &pages {
        let Ok(dict) = doc.get_dictionary(pid) else {
            continue;
        };
        let Ok(annots) = dict.get(b"Annots") else {
            continue;
        };
        let Ok((_, arr)) = doc.dereference(annots) else {
            continue;
        };
        let Ok(items) = arr.as_array() else {
            continue;
        };
        for entry in items {
            if let Ok((Some(eid), _)) = doc.dereference(entry) {
                annot.insert(eid, n);
            }
        }
    }
    PageMaps { by_id, annot }
}

/// Resolve the page a widget annotation sits on: `/P` first, then the
/// `/Annots` scan.
fn widget_page(doc: &Document, maps: &PageMaps, widget: &WidgetRef) -> Option<u32> {
    let id = widget.id?;
    if let Ok(dict) = doc.get_dictionary(id)
        && let Ok(p) = dict.get(b"P")
        && let Ok((Some(pid), _)) = doc.dereference(p)
        && let Some(&n) = maps.by_id.get(&pid)
    {
        return Some(n);
    }
    maps.annot.get(&id).copied()
}

/// Depth-first walk of `/AcroForm /Fields` collecting [`FieldEntry`]s.
pub(crate) fn collect_fields(doc: &Document) -> Vec<FieldEntry> {
    let mut out = Vec::new();
    let Ok(catalog) = doc.catalog() else {
        return out;
    };
    let Ok(af) = catalog.get(b"AcroForm") else {
        return out;
    };
    let Ok((_, af_obj)) = doc.dereference(af) else {
        return out;
    };
    let Ok(af_dict) = af_obj.as_dict() else {
        return out;
    };
    let Ok(fields) = af_dict.get(b"Fields") else {
        return out;
    };
    let Ok((_, fobj)) = doc.dereference(fields) else {
        return out;
    };
    let Ok(arr) = fobj.as_array() else {
        return out;
    };
    let maps = page_maps(doc);
    let mut visited = HashSet::new();
    for field in arr {
        visit_node(doc, field, "", None, 0, &maps, &mut out, &mut visited, 0);
    }
    out
}

#[allow(clippy::too_many_arguments)]
fn visit_node<'a>(
    doc: &'a Document,
    node: &'a Object,
    path: &str,
    inherited_ft: Option<&str>,
    inherited_ff: i64,
    maps: &PageMaps,
    out: &mut Vec<FieldEntry>,
    visited: &mut HashSet<ObjectId>,
    depth: usize,
) {
    if depth > MAX_FIELD_DEPTH || out.len() >= MAX_FIELDS {
        return;
    }
    let Some((id, dict)) = resolve_dict(doc, node) else {
        return;
    };
    if let Some(id) = id
        && !visited.insert(id)
    {
        return;
    }

    let t = partial_name(doc, dict);
    let name = join_name(path, t.as_deref());
    let own_ft = field_type_name(doc, dict);
    let ft = own_ft.as_deref().or(inherited_ft).map(str::to_string);
    let own_ff = field_flags(doc, dict).unwrap_or(inherited_ff);
    let has_v = dict.get(b"V").is_ok();
    let has_opt = dict.has(b"Opt");

    // Classify `/Kids`: widget annotations vs. sub-fields.
    let mut subfields: Vec<&'a Object> = Vec::new();
    let mut widgets: Vec<WidgetRef> = Vec::new();
    if let Ok(kids_obj) = dict.get(b"Kids")
        && let Ok((_, kids)) = doc.dereference(kids_obj)
        && let Ok(kids_arr) = kids.as_array()
    {
        for kid in kids_arr {
            let Some((kid_id, kid_dict)) = resolve_dict(doc, kid) else {
                continue;
            };
            if is_widget_dict(doc, kid_dict) {
                widgets.push(WidgetRef {
                    id: kid_id,
                    rect: rect_from_dict(kid_dict),
                });
            } else {
                subfields.push(kid);
            }
        }
    }
    let self_widget = is_widget_dict(doc, dict);
    if self_widget {
        widgets.insert(
            0,
            WidgetRef {
                id,
                rect: rect_from_dict(dict),
            },
        );
    }

    let terminal = subfields.is_empty();
    let report = (terminal
        && (t.is_some()
            || own_ft.is_some()
            || has_v
            || has_opt
            || self_widget
            || !widgets.is_empty()))
        || has_v
        || has_opt;

    if report {
        let page = widgets.iter().find_map(|w| widget_page(doc, maps, w));
        out.push(FieldEntry {
            name: name.clone(),
            type_name: ft.clone().unwrap_or_default(),
            value: field_value(doc, dict),
            options: field_options(doc, dict),
            read_only: own_ff & 1 != 0,
            page,
            id,
            widgets,
        });
    }

    for sub in subfields {
        visit_node(
            doc,
            sub,
            &name,
            ft.as_deref(),
            own_ff,
            maps,
            out,
            visited,
            depth + 1,
        );
    }
}

// ---------------------------------------------------------------------------
// AcroForm structure helpers (shared with sibling ops as pub(crate))
// ---------------------------------------------------------------------------

/// Locate the catalog's `/AcroForm` dictionary, promoting it to an
/// indirect object when it is stored inline. Returns `None` when the
/// catalog has no `/AcroForm` entry; `InvalidInput` when the entry is
/// not a dictionary.
pub(crate) fn acroform_dict_id(doc: &mut Document) -> PdfResult<Option<ObjectId>> {
    let direct_dict = {
        let catalog = doc.catalog().map_err(lopdf_err)?;
        match catalog.get(b"AcroForm") {
            Ok(Object::Dictionary(_)) => true,
            Ok(_) => false,
            Err(_) => return Ok(None),
        }
    };
    if direct_dict {
        let dict = {
            let catalog = doc.catalog().map_err(lopdf_err)?;
            catalog
                .get(b"AcroForm")
                .and_then(|o| o.as_dict().cloned())
                .map_err(lopdf_err)?
        };
        let id = doc.add_object(Object::Dictionary(dict));
        doc.catalog_mut()
            .map_err(lopdf_err)?
            .set("AcroForm", id);
        return Ok(Some(id));
    }

    let acro = {
        let catalog = doc.catalog().map_err(lopdf_err)?;
        match catalog.get(b"AcroForm") {
            Ok(o) => o.clone(),
            Err(_) => return Ok(None),
        }
    };
    let (id, resolved) = doc.dereference(&acro).map_err(lopdf_err)?;
    if resolved.as_dict().is_err() {
        return Err(PdfError::InvalidInput(
            "AcroForm entry is not a dictionary".to_string(),
        ));
    }
    Ok(id)
}

/// Ensure the catalog has an `/AcroForm` dictionary (created empty
/// when absent) and return its object id.
fn ensure_acroform(doc: &mut Document) -> PdfResult<ObjectId> {
    let exists = {
        let catalog = doc.catalog().map_err(lopdf_err)?;
        catalog.get(b"AcroForm").is_ok()
    };
    if !exists {
        let id = doc.add_object(dictionary! {});
        doc.catalog_mut()
            .map_err(lopdf_err)?
            .set("AcroForm", id);
        return Ok(id);
    }
    acroform_dict_id(doc)?.ok_or_else(|| {
        PdfError::InvalidInput("AcroForm dictionary could not be created".to_string())
    })
}

/// Ensure `/AcroForm /Fields` exists as an array.
fn ensure_fields_array(doc: &mut Document, acro_id: ObjectId) -> PdfResult<()> {
    let state = {
        let dict = doc.get_dictionary(acro_id).map_err(lopdf_err)?;
        match dict.get(b"Fields") {
            Err(_) => 0,
            Ok(v) => {
                let (_, resolved) = doc.dereference(v).map_err(lopdf_err)?;
                if resolved.as_array().is_ok() {
                    1
                } else {
                    2
                }
            }
        }
    };
    match state {
        0 => {
            doc.get_dictionary_mut(acro_id)
                .map_err(lopdf_err)?
                .set("Fields", Object::Array(Vec::new()));
            Ok(())
        }
        1 => Ok(()),
        _ => Err(PdfError::InvalidInput(
            "AcroForm /Fields entry is not an array".to_string(),
        )),
    }
}

/// Ensure `/AcroForm /DR /Font /Helv` names a font object, creating a
/// base-14 Helvetica Type1 font (and the `/DR` structure) when
/// missing. Existing `/DR` content is preserved. Also sets
/// `/NeedAppearances true`. Returns the `/Helv` font object id.
pub(crate) fn ensure_helv_font(doc: &mut Document, acro_id: ObjectId) -> PdfResult<ObjectId> {
    let probe = {
        let dict = doc.get_dictionary(acro_id).map_err(lopdf_err)?;
        let mut dr = None;
        let mut font = None;
        let mut helv = None;
        if let Ok(dr_obj) = dict.get(b"DR")
            && let Ok((_, dr_res)) = doc.dereference(dr_obj)
            && let Ok(dr_dict) = dr_res.as_dict()
        {
            dr = Some(dr_dict.clone());
            if let Ok(font_obj) = dr_dict.get(b"Font")
                && let Ok((_, font_res)) = doc.dereference(font_obj)
                && let Ok(font_dict) = font_res.as_dict()
            {
                font = Some(font_dict.clone());
                if let Ok(helv_obj) = font_dict.get(b"Helv")
                    && let Ok((hid, helv_res)) = doc.dereference(helv_obj)
                    && helv_res.as_dict().is_ok()
                {
                    helv = Some(hid);
                }
            }
        }
        (dr, font, helv)
    };

    let font_id = match probe.2 {
        Some(id) => id,
        None => {
            let fid = doc.add_object(dictionary! {
                "Type" => "Font",
                "Subtype" => "Type1",
                "BaseFont" => "Helvetica",
                "Encoding" => "WinAnsiEncoding",
            });
            let mut font_dict = probe.1.unwrap_or_else(lopdf::Dictionary::new);
            font_dict.set("Helv", fid);
            let mut dr_dict = probe.0.unwrap_or_else(lopdf::Dictionary::new);
            dr_dict.set("Font", Object::Dictionary(font_dict));
            doc.get_dictionary_mut(acro_id)
                .map_err(lopdf_err)?
                .set("DR", Object::Dictionary(dr_dict));
            fid
        }
    };
    set_need_appearances(doc, acro_id);
    Ok(font_id)
}

/// Set `/AcroForm /NeedAppearances true` — a hint that viewers should
/// regenerate field appearances (used after `/V` changes).
pub(crate) fn set_need_appearances(doc: &mut Document, acro_id: ObjectId) {
    if let Ok(dict) = doc.get_dictionary_mut(acro_id) {
        dict.set("NeedAppearances", Object::Boolean(true));
    }
}

/// Escape a string for use inside a PDF literal string `(...)`.
pub(crate) fn escape_pdf_literal(text: &str) -> Vec<u8> {
    let mut out = Vec::with_capacity(text.len() + 8);
    for ch in text.chars() {
        match ch {
            '\\' => out.extend_from_slice(b"\\\\"),
            '(' => out.extend_from_slice(b"\\("),
            ')' => out.extend_from_slice(b"\\)"),
            '\n' => out.extend_from_slice(b"\\n"),
            '\r' => out.extend_from_slice(b"\\r"),
            '\t' => out.extend_from_slice(b"\\t"),
            c => {
                let mut buf = [0u8; 4];
                out.extend_from_slice(c.encode_utf8(&mut buf).as_bytes());
            }
        }
    }
    out
}

// ---------------------------------------------------------------------------
// Indirect-object normalization (delete path)
// ---------------------------------------------------------------------------

/// Address of an array that lives either as a standalone indirect
/// object or inline inside a dictionary.
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub(crate) enum ArrayLoc {
    /// The array is the object with this id.
    Obj(ObjectId),
    /// The array is stored directly under `key` of the dictionary
    /// with this id.
    Dict(ObjectId, Vec<u8>),
}

/// Resolve `dict[key]` to an [`ArrayLoc`] when it is (or references)
/// an array.
fn array_loc_of(doc: &Document, value: &Object, dict_id: ObjectId, key: &[u8]) -> Option<ArrayLoc> {
    let (id, resolved) = doc.dereference(value).ok()?;
    resolved.as_array().ok()?;
    Some(match id {
        Some(id) => ArrayLoc::Obj(id),
        None => ArrayLoc::Dict(dict_id, key.to_vec()),
    })
}

/// [`ArrayLoc`] of `dict_id`'s `key` array entry.
fn named_array_loc(doc: &Document, dict_id: ObjectId, key: &[u8]) -> Option<ArrayLoc> {
    let dict = doc.get_dictionary(dict_id).ok()?;
    let value = dict.get(key).ok()?;
    array_loc_of(doc, value, dict_id, key)
}

/// [`ArrayLoc`] of a field dictionary's `/Kids` array.
fn kids_array_loc(doc: &Document, dict: &lopdf::Dictionary, dict_id: ObjectId) -> Option<ArrayLoc> {
    let value = dict.get(b"Kids").ok()?;
    array_loc_of(doc, value, dict_id, b"Kids")
}

/// Clone the elements of an [`ArrayLoc`] array.
fn read_array_clone(doc: &Document, loc: &ArrayLoc) -> PdfResult<Vec<Object>> {
    match loc {
        ArrayLoc::Obj(id) => doc
            .get_object(*id)
            .and_then(|o| o.as_array().map(|a| a.clone()))
            .map_err(lopdf_err),
        ArrayLoc::Dict(id, key) => {
            let dict = doc.get_dictionary(*id).map_err(lopdf_err)?;
            let value = dict.get(key).map_err(lopdf_err)?;
            let (_, resolved) = doc.dereference(value).map_err(lopdf_err)?;
            resolved
                .as_array()
                .map(|a| a.clone())
                .map_err(lopdf_err)
        }
    }
}

/// Remove element `index` from an [`ArrayLoc`] array. The index is
/// bounds-checked — out-of-range is an error, never a panic.
fn remove_array_element(doc: &mut Document, loc: &ArrayLoc, index: usize) -> PdfResult<()> {
    match loc {
        ArrayLoc::Obj(id) => {
            let arr = doc
                .get_object_mut(*id)
                .map_err(lopdf_err)?
                .as_array_mut()
                .map_err(lopdf_err)?;
            if index >= arr.len() {
                return Err(PdfError::InvalidInput(
                    "form field array index out of range".to_string(),
                ));
            }
            arr.remove(index);
        }
        ArrayLoc::Dict(id, key) => {
            let arr = doc
                .get_dictionary_mut(*id)
                .map_err(lopdf_err)?
                .get_mut(key)
                .map_err(lopdf_err)?
                .as_array_mut()
                .map_err(lopdf_err)?;
            if index >= arr.len() {
                return Err(PdfError::InvalidInput(
                    "form field array index out of range".to_string(),
                ));
            }
            arr.remove(index);
        }
    }
    Ok(())
}

/// Append `elem` to an [`ArrayLoc`] array.
fn push_array_element(doc: &mut Document, loc: &ArrayLoc, elem: Object) -> PdfResult<()> {
    match loc {
        ArrayLoc::Obj(id) => {
            doc.get_object_mut(*id)
                .map_err(lopdf_err)?
                .as_array_mut()
                .map_err(lopdf_err)?
                .push(elem);
        }
        ArrayLoc::Dict(id, key) => {
            doc.get_dictionary_mut(*id)
                .map_err(lopdf_err)?
                .get_mut(key)
                .map_err(lopdf_err)?
                .as_array_mut()
                .map_err(lopdf_err)?
                .push(elem);
        }
    }
    Ok(())
}

/// Promote every direct (inline) dictionary inside `/Fields` and
/// `/Kids` arrays to an indirect object, so the whole field tree is
/// addressable by [`ObjectId`]. Returns `false` when the document has
/// no usable `/AcroForm`.
pub(crate) fn normalize_acroform(doc: &mut Document) -> PdfResult<bool> {
    let Some(acro_id) = acroform_dict_id(doc)? else {
        return Ok(false);
    };
    let mut visited: HashSet<ArrayLoc> = HashSet::new();
    let mut queue: VecDeque<ArrayLoc> = VecDeque::new();
    if let Some(loc) = named_array_loc(doc, acro_id, b"Fields") {
        queue.push_back(loc);
    }
    let mut rounds = 0usize;
    while let Some(loc) = queue.pop_front() {
        rounds += 1;
        if rounds > MAX_ARRAYS {
            return Ok(true);
        }
        if !visited.insert(loc.clone()) {
            continue;
        }
        for child in process_array(doc, &loc)? {
            queue.push_back(child);
        }
    }
    Ok(true)
}

/// Scan one field-tree array: promote inline dictionaries to indirect
/// objects (recording their `/Kids` locations for the BFS) and return
/// the child array locations discovered.
fn process_array(doc: &mut Document, loc: &ArrayLoc) -> PdfResult<Vec<ArrayLoc>> {
    let elements = read_array_clone(doc, loc)?;
    let mut changed = false;
    let mut out_elements = Vec::with_capacity(elements.len());
    let mut child_locs = Vec::new();

    for elem in &elements {
        match elem {
            Object::Dictionary(_) => {
                let new_id = doc.add_object(elem.clone());
                changed = true;
                if let Ok(dict) = elem.as_dict()
                    && let Some(kids_loc) = kids_array_loc(doc, dict, new_id)
                {
                    child_locs.push(kids_loc);
                }
                out_elements.push(Object::Reference(new_id));
            }
            _ => {
                out_elements.push(elem.clone());
                let Ok((resolved_id, resolved)) = doc.dereference(elem).map_err(lopdf_err) else {
                    continue;
                };
                let (Some(dict_id), Ok(dict)) = (resolved_id, resolved.as_dict()) else {
                    continue;
                };
                if let Some(kids_loc) = kids_array_loc(doc, dict, dict_id) {
                    child_locs.push(kids_loc);
                }
            }
        }
    }

    if changed {
        match loc {
            ArrayLoc::Obj(id) => {
                let arr = doc
                    .get_object_mut(*id)
                    .map_err(lopdf_err)?
                    .as_array_mut()
                    .map_err(lopdf_err)?;
                *arr = out_elements;
            }
            ArrayLoc::Dict(id, key) => {
                let arr = doc
                    .get_dictionary_mut(*id)
                    .map_err(lopdf_err)?
                    .get_mut(key)
                    .map_err(lopdf_err)?
                    .as_array_mut()
                    .map_err(lopdf_err)?;
                *arr = out_elements;
            }
        }
    }
    Ok(child_locs)
}

// ---------------------------------------------------------------------------
// Delete-path scan (field nodes with container locations)
// ---------------------------------------------------------------------------

/// A field-tree node with everything needed to remove it.
struct ScannedNode {
    /// Array containing this node's entry.
    loc: ArrayLoc,
    /// Index of this node's entry inside `loc`.
    index: usize,
    /// Indirect object id of the field dictionary.
    id: ObjectId,
    /// Full dotted name (same rules as [`collect_fields`]).
    name: String,
    /// Cloned field dictionary.
    dict: lopdf::Dictionary,
}

/// Scan the whole field tree, returning every node (fields *and*
/// widget annotations) with its container location.
fn scan_fields(doc: &Document, acro_id: ObjectId) -> PdfResult<Vec<ScannedNode>> {
    let mut out = Vec::new();
    let Some(loc) = named_array_loc(doc, acro_id, b"Fields") else {
        return Ok(out);
    };
    scan_array(doc, &loc, "", None, 0, &mut out)?;
    Ok(out)
}

fn scan_array(
    doc: &Document,
    loc: &ArrayLoc,
    path: &str,
    inherited_ft: Option<&str>,
    inherited_ff: i64,
    out: &mut Vec<ScannedNode>,
) -> PdfResult<()> {
    if out.len() >= MAX_FIELDS {
        return Ok(());
    }
    let elements = read_array_clone(doc, loc)?;
    for (index, elem) in elements.iter().enumerate() {
        let Some((resolved_id, resolved)) = doc.dereference(elem).map_err(lopdf_err).ok() else {
            continue;
        };
        let Some(id) = resolved_id else {
            // Inline dictionary — should not happen after
            // normalize_acroform; skip rather than fail.
            continue;
        };
        let Ok(dict) = resolved.as_dict() else {
            continue;
        };
        let t = partial_name(doc, dict);
        let name = join_name(path, t.as_deref());
        let own_ft = field_type_name(doc, dict);
        let ft = own_ft.as_deref().or(inherited_ft).map(str::to_string);
        let own_ff = field_flags(doc, dict).unwrap_or(inherited_ff);

        let mut sub_locs = Vec::new();
        if let Ok(kids_obj) = dict.get(b"Kids")
            && let Ok((_, kids)) = doc.dereference(kids_obj)
            && let Ok(kids_arr) = kids.as_array()
        {
            for kid in kids_arr {
                let Some((kid_id, kid_dict)) = resolve_dict(doc, kid) else {
                    continue;
                };
                let Some(kid_id) = kid_id else {
                    continue;
                };
                if !is_widget_dict(doc, kid_dict)
                    && let Some(kids_loc) = kids_array_loc(doc, kid_dict, kid_id)
                {
                    sub_locs.push(kids_loc);
                }
            }
        }

        out.push(ScannedNode {
            loc: loc.clone(),
            index,
            id,
            name: name.clone(),
            dict: dict.clone(),
        });

        for kids_loc in sub_locs {
            scan_array(doc, &kids_loc, &name, ft.as_deref(), own_ff, out)?;
        }
    }
    Ok(())
}

/// Collect every widget annotation object id inside the field
/// subtree rooted at `dict_id` (including `dict_id` itself when
/// `include_self`).
fn collect_subtree_widgets(
    doc: &Document,
    dict_id: ObjectId,
    include_self: bool,
    out: &mut Vec<ObjectId>,
    depth: usize,
) {
    if depth > MAX_FIELD_DEPTH || out.len() >= MAX_FIELDS {
        return;
    }
    let Ok(dict) = doc.get_dictionary(dict_id) else {
        return;
    };
    if include_self {
        out.push(dict_id);
    }
    let Ok(kids_obj) = dict.get(b"Kids") else {
        return;
    };
    let Ok((_, kids)) = doc.dereference(kids_obj) else {
        return;
    };
    let Ok(kids_arr) = kids.as_array() else {
        return;
    };
    for kid in kids_arr {
        let Some((kid_id, kid_dict)) = resolve_dict(doc, kid) else {
            continue;
        };
        let Some(kid_id) = kid_id else {
            continue;
        };
        if is_widget_dict(doc, kid_dict) {
            out.push(kid_id);
        } else {
            collect_subtree_widgets(doc, kid_id, false, out, depth + 1);
        }
    }
}

/// Remove `widget_id` from every page's `/Annots` array. Best effort:
/// pages whose `/Annots` cannot be read or rewritten are skipped, and
/// annotations stored as inline dictionaries cannot be matched.
fn remove_annot_from_pages(doc: &mut Document, widget_id: ObjectId) {
    for (_, page_id) in doc.get_pages() {
        let _ = remove_annot_from_page(doc, page_id, widget_id);
    }
}

fn remove_annot_from_page(doc: &mut Document, page_id: ObjectId, widget_id: ObjectId) -> PdfResult<()> {
    // Clone the entries and decide whether anything matches.
    let (entries, kind) = {
        let dict = doc.get_dictionary(page_id).map_err(lopdf_err)?;
        let value = match dict.get(b"Annots") {
            Ok(v) => v,
            Err(_) => return Ok(()),
        };
        let kind = match value {
            Object::Reference(r) => ArrayLoc::Obj(*r),
            Object::Array(_) => ArrayLoc::Dict(page_id, b"Annots".to_vec()),
            _ => return Ok(()),
        };
        let (_, resolved) = doc.dereference(value).map_err(lopdf_err)?;
        let items = resolved.as_array().map_err(lopdf_err)?.clone();
        (items, kind)
    };

    let mut kept = Vec::with_capacity(entries.len());
    let mut changed = false;
    for entry in entries {
        let matches = doc
            .dereference(&entry)
            .ok()
            .and_then(|(id, _)| id)
            == Some(widget_id);
        if matches {
            changed = true;
        } else {
            kept.push(entry);
        }
    }
    if !changed {
        return Ok(());
    }

    match kind {
        ArrayLoc::Obj(id) => {
            let arr = doc
                .get_object_mut(id)
                .map_err(lopdf_err)?
                .as_array_mut()
                .map_err(lopdf_err)?;
            *arr = kept;
        }
        ArrayLoc::Dict(_, _) => {
            let dict = doc.get_dictionary_mut(page_id).map_err(lopdf_err)?;
            if kept.is_empty() {
                dict.remove(b"Annots");
            } else {
                dict.get_mut(b"Annots")
                    .map_err(lopdf_err)?
                    .as_array_mut()
                    .map_err(lopdf_err)?
                    .clone_from(&kept);
            }
        }
    }
    Ok(())
}

/// Whether a scanned node is a leftover pure container: not a widget,
/// carries no value/options/rect, and has no (non-empty) `/Kids`.
fn is_prunable(doc: &Document, node: &ScannedNode) -> bool {
    let dict = &node.dict;
    if is_widget_dict(doc, dict) {
        return false;
    }
    if dict.has(b"V") || dict.has(b"Opt") || dict.has(b"Rect") {
        return false;
    }
    if let Ok(kids_obj) = dict.get(b"Kids")
        && let Ok((_, kids)) = doc.dereference(kids_obj)
        && let Ok(arr) = kids.as_array()
        && !arr.is_empty()
    {
        return false;
    }
    true
}

/// Repeatedly remove field nodes left empty after a deletion (pure
/// `/T` name containers whose `/Kids` are gone), until the tree is
/// stable.
fn prune_empty_fields(doc: &mut Document) -> PdfResult<()> {
    for _ in 0..MAX_PRUNE_ROUNDS {
        let Some(acro_id) = acroform_dict_id(doc)? else {
            return Ok(());
        };
        let nodes = scan_fields(doc, acro_id)?;
        let Some(node) = nodes.iter().find(|n| is_prunable(doc, n)) else {
            return Ok(());
        };
        let (loc, index) = (node.loc.clone(), node.index);
        remove_array_element(doc, &loc, index)?;
    }
    Ok(())
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use lopdf::{Document, dictionary};

    use super::*;
    use crate::crates::pdf_engine::ops::test_utils::pdf_with_pages;

    fn limits() -> EngineLimits {
        EngineLimits::default()
    }

    fn spec(name: &str, page: u32) -> TextFieldSpec {
        TextFieldSpec {
            name: name.to_string(),
            page,
            rect: [100.0, 700.0, 300.0, 720.0],
            value: None,
            read_only: false,
        }
    }

    #[test]
    fn create_list_delete_round_trip() {
        let base = pdf_with_pages(&["page one", "page two"]);
        let mut s = spec("name", 2);
        s.value = Some("Alice".to_string());
        s.read_only = true;
        let created = create_text_field(&base, &s, &limits()).unwrap();

        let fields = list_form_fields(&created, &limits()).unwrap();
        assert_eq!(fields.len(), 1);
        let f = &fields[0];
        assert_eq!(f.name, "name");
        assert_eq!(f.type_name, "Tx");
        assert_eq!(f.page, Some(2));
        assert_eq!(f.value.as_deref(), Some("Alice"));
        assert!(f.read_only);
        assert!(f.options.is_empty());

        let deleted = delete_form_field(&created, "name", &limits()).unwrap();
        assert!(list_form_fields(&deleted, &limits()).unwrap().is_empty());
    }

    #[test]
    fn create_rejects_bad_page_and_empty_name() {
        let base = pdf_with_pages(&["only"]);
        assert!(matches!(
            create_text_field(&base, &spec("x", 9), &limits()),
            Err(PdfError::InvalidPageRange(_))
        ));
        assert!(matches!(
            create_text_field(&base, &spec("", 1), &limits()),
            Err(PdfError::InvalidInput(_))
        ));
        let mut bad_rect = spec("x", 1);
        bad_rect.rect = [0.0, 0.0, -5.0, 10.0];
        assert!(matches!(
            create_text_field(&base, &bad_rect, &limits()),
            Err(PdfError::InvalidInput(_))
        ));
    }

    #[test]
    fn delete_missing_field_is_invalid_input() {
        let base = pdf_with_pages(&["p"]);
        let created = create_text_field(&base, &spec("a", 1), &limits()).unwrap();
        assert!(matches!(
            delete_form_field(&created, "nope", &limits()),
            Err(PdfError::InvalidInput(_))
        ));
        // No AcroForm at all.
        assert!(matches!(
            delete_form_field(&base, "a", &limits()),
            Err(PdfError::InvalidInput(_))
        ));
    }

    #[test]
    fn nested_fields_get_dotted_names() {
        // Hand-build a parent field "form1" with two Tx children.
        let mut doc = Document::with_version("1.5");
        let pages_id = doc.new_object_id();
        let page_id = doc.add_object(dictionary! {
            "Type" => "Page",
            "Parent" => pages_id,
            "MediaBox" => vec![0.into(), 0.into(), 595.into(), 842.into()],
        });
        doc.objects.insert(
            pages_id,
            Object::Dictionary(dictionary! {
                "Type" => "Pages",
                "Kids" => vec![Object::Reference(page_id)],
                "Count" => 1,
                "MediaBox" => vec![0.into(), 0.into(), 595.into(), 842.into()],
            }),
        );
        let catalog_id = doc.add_object(dictionary! {
            "Type" => "Catalog",
            "Pages" => pages_id,
        });
        doc.trailer.set("Root", catalog_id);

        let child1 = doc.add_object(dictionary! {
            "FT" => "Tx", "T" => text_string("first"), "V" => text_string("1"),
        });
        let child2 = doc.add_object(dictionary! {
            "FT" => "Tx", "T" => text_string("second"),
        });
        let parent = doc.add_object(dictionary! {
            "T" => text_string("form1"),
            "Kids" => vec![Object::Reference(child1), Object::Reference(child2)],
        });
        let acro = doc.add_object(dictionary! {
            "Fields" => vec![Object::Reference(parent)],
        });
        doc.get_dictionary_mut(catalog_id)
            .unwrap()
            .set("AcroForm", acro);

        let mut buf = Vec::new();
        doc.save_to(&mut buf).unwrap();

        let fields = list_form_fields(&buf, &limits()).unwrap();
        let names: Vec<&str> = fields.iter().map(|f| f.name.as_str()).collect();
        assert_eq!(names, vec!["form1.first", "form1.second"]);
        assert_eq!(fields[0].value.as_deref(), Some("1"));
        assert_eq!(fields[0].type_name, "Tx");
    }

    #[test]
    fn encrypted_input_is_rejected() {
        use crate::crates::pdf_engine::ops::encrypt::{PdfPermissions, encrypt};
        let base = pdf_with_pages(&["secret"]);
        let encrypted = encrypt(&base, "pw", None, PdfPermissions::default(), &limits()).unwrap();
        assert!(matches!(
            list_form_fields(&encrypted, &limits()),
            Err(PdfError::Encrypted)
        ));
        assert!(matches!(
            create_text_field(&encrypted, &spec("x", 1), &limits()),
            Err(PdfError::Encrypted)
        ));
        assert!(matches!(
            delete_form_field(&encrypted, "x", &limits()),
            Err(PdfError::Encrypted)
        ));
    }
}
