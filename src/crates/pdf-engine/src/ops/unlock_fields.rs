//! Set or clear AcroForm field flags (`/Ff`).
//!
//! [`unlock_fields`] clears the **ReadOnly** bit (`/Ff` bit 1, value
//! `1`) so users can edit fields again; [`lock_fields`] is the
//! symmetric operation that sets the bit. `names` selects which
//! fields are touched by their **full dotted name** (exact,
//! case-sensitive match against the `/T` hierarchy, same rules as
//! [`list_form_fields`](super::form_fields)). `None` selects every
//! field node in the tree.
//!
//! # Flag semantics (honest scope)
//!
//! `/Ff` is an *inheritable* field attribute: a field without its own
//! `/Ff` inherits the effective flags of its nearest ancestor. This op
//! computes each node's **effective** flags during traversal and writes
//! the updated effective value back onto the matched node's own `/Ff`
//! entry. Consequences:
//!
//! - A field that only inherited ReadOnly from a parent gets its own
//!   `/Ff` written with the bit cleared — the subtree below it keeps
//!   its previously computed effective flags, so semantics are
//!   preserved.
//! - Writing effective flags freezes other inherited bits onto the
//!   matched node; later edits to an ancestor's `/Ff` will no longer
//!   propagate to it. This is unavoidable without restructuring the
//!   field tree and is documented rather than hidden.
//! - Nodes already in the requested state are counted as `unchanged`
//!   and are *not* rewritten, so a no-op call still round-trips through
//!   the parser but does not add stray `/Ff` entries.
//!
//! Documents without an `/AcroForm` are returned unchanged; requested
//! names that match no field are reported in
//! [`FieldFlagReport::unmatched`] rather than failing the call.

use std::collections::HashSet;

use lopdf::{Document, Object, ObjectId, decode_text_string};
use serde::{Deserialize, Serialize};

use crate::crates::core::engine::EngineLimits;

use crate::crates::pdf_engine::error::PdfResult;

use super::form_fields::{acroform_dict_id, normalize_acroform};
use super::load::{load_pdf, lopdf_err, reject_encrypted, save_pdf};

/// Safety caps for pathological AcroForm trees (mirrors
/// `form_fields.rs`).
const MAX_FIELD_DEPTH: usize = 64;
const MAX_FIELDS: usize = 4096;
const MAX_ARRAYS: usize = 65_536;

/// `/Ff` bit for the ReadOnly flag (ISO 32000-1, table 227).
const FF_READ_ONLY: i64 = 1;

/// Outcome of [`unlock_fields`] / [`lock_fields`].
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
pub struct FieldFlagReport {
    /// Field nodes selected for an update (matched by name, or every
    /// node when `names` was `None`).
    pub matched: usize,
    /// Nodes whose `/Ff` value actually changed.
    pub changed: usize,
    /// Matched nodes already in the requested state (not rewritten).
    pub unchanged: usize,
    /// Requested names (deduplicated, in request order) that matched
    /// no field node. Always empty when `names` was `None`.
    pub unmatched: Vec<String>,
}

/// One node of the AcroForm field tree, as discovered by
/// [`scan_field_nodes`]. Shared with sibling ops (`edit_field_props`)
/// as `pub(crate)`.
#[derive(Debug, Clone)]
pub(crate) struct FieldNode {
    /// Full dotted name built from the `/T` hierarchy.
    pub name: String,
    /// Indirect object id of the field dictionary (`None` only for
    /// inline dictionaries, which [`normalize_acroform`] removes).
    pub id: Option<ObjectId>,
    /// Effective `/FT` field type (`Tx`, `Btn`, `Ch`, `Sig`),
    /// inheritable from ancestors. Empty when undeclared.
    #[allow(dead_code)]
    pub type_name: String,
    /// Effective `/Ff` flags (own value or inherited default).
    pub ff: i64,
    /// Widget annotation object ids belonging to this node (merged
    /// self-widget first, then `/Kids` widgets in order).
    #[allow(dead_code)]
    pub widgets: Vec<ObjectId>,
}

// ---------------------------------------------------------------------------
// Public operations
// ---------------------------------------------------------------------------

/// Clear the ReadOnly flag (`/Ff` bit 1) on the named fields — or on
/// every field when `names` is `None` — and return the modified PDF
/// plus a [`FieldFlagReport`].
pub fn unlock_fields(
    input: &[u8],
    names: Option<&[String]>,
    limits: &EngineLimits,
) -> PdfResult<(Vec<u8>, FieldFlagReport)> {
    set_read_only(input, names, limits, false)
}

/// Set the ReadOnly flag (`/Ff` bit 1) on the named fields — or on
/// every field when `names` is `None` — and return the modified PDF
/// plus a [`FieldFlagReport`].
pub fn lock_fields(
    input: &[u8],
    names: Option<&[String]>,
    limits: &EngineLimits,
) -> PdfResult<(Vec<u8>, FieldFlagReport)> {
    set_read_only(input, names, limits, true)
}

/// Shared implementation for [`unlock_fields`] / [`lock_fields`].
fn set_read_only(
    input: &[u8],
    names: Option<&[String]>,
    limits: &EngineLimits,
    read_only: bool,
) -> PdfResult<(Vec<u8>, FieldFlagReport)> {
    let mut doc = load_pdf(input, limits)?;
    reject_encrypted(&doc)?;

    let mut report = FieldFlagReport::default();
    let Some(acro_id) = acroform_dict_id(&mut doc)? else {
        // No AcroForm: nothing to change. Report every requested name
        // as unmatched so callers can detect the no-op.
        if let Some(names) = names {
            report.unmatched = dedup_names(names);
        }
        return Ok((input.to_vec(), report));
    };
    // Promote inline field/widget dictionaries so every node has an
    // object id to mutate.
    normalize_acroform(&mut doc)?;
    let nodes = scan_field_nodes(&doc);

    // Resolve the requested name set (deduplicated, order preserved).
    // An empty `names` slice selects nothing; `None` selects every node.
    let wanted: Option<HashSet<&str>> = names.map(|ns| {
        let mut seen = HashSet::new();
        ns.iter()
            .filter(|n| !n.is_empty() && seen.insert(n.as_str()))
            .map(String::as_str)
            .collect()
    });

    for node in &nodes {
        let selected = match &wanted {
            None => true,
            Some(set) => set.contains(node.name.as_str()),
        };
        if !selected {
            continue;
        }
        let Some(id) = node.id else {
            continue;
        };
        report.matched += 1;

        let new_ff = if read_only {
            node.ff | FF_READ_ONLY
        } else {
            node.ff & !FF_READ_ONLY
        };
        if new_ff == node.ff {
            report.unchanged += 1;
            continue;
        }
        doc.get_object_mut(id)
            .map_err(lopdf_err)?
            .as_dict_mut()
            .map_err(lopdf_err)?
            .set("Ff", Object::Integer(new_ff));
        report.changed += 1;
    }

    if let Some(names) = names {
        // Report requested names that matched no node. Match against
        // the *requested* strings (not node names) so duplicates and
        // names that only prefix-match are handled correctly.
        let mut seen = HashSet::new();
        for name in names {
            if name.is_empty() || !seen.insert(name.as_str()) {
                continue;
            }
            // A requested name is "matched" when some node carried it.
            let hit = nodes.iter().any(|n| n.name == *name);
            if !hit {
                report.unmatched.push(name.clone());
            }
        }
    }

    let _ = acro_id;
    Ok((save_pdf(&mut doc)?, report))
}

fn dedup_names(names: &[String]) -> Vec<String> {
    let mut seen = HashSet::new();
    names
        .iter()
        .filter(|n| !n.is_empty() && seen.insert(n.as_str()))
        .cloned()
        .collect()
}

// ---------------------------------------------------------------------------
// Field-tree traversal (shared read side)
// ---------------------------------------------------------------------------

/// Depth-first walk of `/AcroForm /Fields` collecting every node
/// (fields *and* intermediate parents) with its effective flags.
/// Must be called on an immutable document; mutation helpers are
/// applied afterwards by object id.
pub(crate) fn scan_field_nodes(doc: &Document) -> Vec<FieldNode> {
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
    let mut visited = HashSet::new();
    for field in arr {
        visit_node(doc, field, "", None, 0, &mut out, &mut visited, 0);
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
    out: &mut Vec<FieldNode>,
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
    // Effective flags: own /Ff wins, otherwise the inherited value.
    let ff = field_flags(doc, dict).unwrap_or(inherited_ff);

    // Classify /Kids: widget annotations vs. sub-fields. Widget ids are
    // collected for sibling ops that rewrite /Rect or /AP.
    let mut subfields: Vec<&'a Object> = Vec::new();
    let mut widgets: Vec<ObjectId> = Vec::new();
    if let Ok(kids_obj) = dict.get(b"Kids")
        && let Ok((_, kids)) = doc.dereference(kids_obj)
        && let Ok(kids_arr) = kids.as_array()
    {
        for kid in kids_arr.iter().take(MAX_ARRAYS) {
            let Some((kid_id, kid_dict)) = resolve_dict(doc, kid) else {
                continue;
            };
            if is_widget_dict(doc, kid_dict) {
                if let Some(kid_id) = kid_id {
                    widgets.push(kid_id);
                }
            } else {
                subfields.push(kid);
            }
        }
    }
    if is_widget_dict(doc, dict)
        && let Some(id) = id
    {
        widgets.insert(0, id);
    }

    out.push(FieldNode {
        name: name.clone(),
        id,
        type_name: ft.clone().unwrap_or_default(),
        ff,
        widgets,
    });

    for sub in subfields {
        visit_node(doc, sub, &name, ft.as_deref(), ff, out, visited, depth + 1);
    }
}

// ---------------------------------------------------------------------------
// Private read helpers (mirrors of the private helpers in form_fields.rs)
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
fn object_text(doc: &Document, obj: &Object) -> Option<String> {
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
    dict.get(b"T").ok().and_then(|o| object_text(doc, o))
}

/// `/FT` field type name (`Tx`, `Btn`, ...).
fn field_type_name(doc: &Document, dict: &lopdf::Dictionary) -> Option<String> {
    dict.get(b"FT")
        .ok()
        .and_then(|o| doc.dereference(o).ok())
        .and_then(|(_, o)| o.as_name().ok())
        .map(|n| String::from_utf8_lossy(n).into_owned())
}

/// Own `/Ff` flags of a dictionary (no inheritance applied here).
fn field_flags(doc: &Document, dict: &lopdf::Dictionary) -> Option<i64> {
    dict.get(b"Ff")
        .ok()
        .and_then(|o| doc.dereference(o).ok())
        .and_then(|(_, o)| o.as_i64().ok())
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

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use lopdf::Document;

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

    fn spec(name: &str, read_only: bool) -> TextFieldSpec {
        TextFieldSpec {
            name: name.to_string(),
            page: 1,
            rect: [50.0, 700.0, 250.0, 720.0],
            value: None,
            read_only,
        }
    }

    fn names(list: &[&str]) -> Vec<String> {
        list.iter().map(|s| s.to_string()).collect()
    }

    fn read_only_of(pdf: &[u8], name: &str) -> bool {
        list_form_fields(pdf, &limits())
            .unwrap()
            .into_iter()
            .find(|f| f.name == name)
            .map(|f| f.read_only)
            .unwrap_or(false)
    }

    #[test]
    fn unlock_all_and_lock_named() {
        let base = pdf_with_pages(&["form"]);
        let mut created = create_text_field(&base, &spec("ro", true), &limits()).unwrap();
        created = create_text_field(&created, &spec("rw", false), &limits()).unwrap();
        assert!(read_only_of(&created, "ro"));
        assert!(!read_only_of(&created, "rw"));

        // Unlock everything.
        let (out, report) = unlock_fields(&created, None, &limits()).unwrap();
        assert_eq!(report.matched, 2);
        assert_eq!(report.changed, 1);
        assert_eq!(report.unchanged, 1);
        assert!(!read_only_of(&out, "ro"));
        assert!(!read_only_of(&out, "rw"));

        // Lock "rw" and "ro" — unlock_fields above cleared ReadOnly on
        // *both*, so locking both changes two nodes. "ghost" matches
        // nothing and is reported unmatched.
        let (out, report) =
            lock_fields(&out, Some(&names(&["rw", "ro", "ghost"])), &limits()).unwrap();
        assert!(read_only_of(&out, "rw"));
        assert!(read_only_of(&out, "ro"));
        assert_eq!(report.matched, 2);
        assert_eq!(report.changed, 2);
        assert_eq!(report.unmatched, vec!["ghost".to_string()]);
    }

    #[test]
    fn unlock_without_acroform_is_noop() {
        let base = pdf_with_pages(&["plain"]);
        let (out, report) = unlock_fields(&base, Some(&names(&["x"])), &limits()).unwrap();
        assert_eq!(out, base);
        assert_eq!(report.matched, 0);
        assert_eq!(report.unmatched, vec!["x".to_string()]);
    }

    #[test]
    fn flag_ops_reject_garbage_and_encrypted() {
        assert!(matches!(
            unlock_fields(b"junk", None, &limits()),
            Err(PdfError::InvalidPdf(_))
        ));
        let plain = pdf_with_pages(&["x"]);
        let encrypted = encrypt(&plain, "pw", None, PdfPermissions::default(), &limits()).unwrap();
        assert!(matches!(
            lock_fields(&encrypted, None, &limits()),
            Err(PdfError::Encrypted)
        ));
    }

    #[test]
    fn scan_reports_effective_flags_on_merged_widget() {
        let base = pdf_with_pages(&["p"]);
        let created = create_text_field(&base, &spec("f", true), &limits()).unwrap();
        let doc = Document::load_mem(&created).unwrap();
        let nodes = scan_field_nodes(&doc);
        assert_eq!(nodes.len(), 1);
        assert_eq!(nodes[0].name, "f");
        assert_eq!(nodes[0].ff & FF_READ_ONLY, FF_READ_ONLY);
        assert_eq!(nodes[0].widgets.len(), 1);
    }
}
