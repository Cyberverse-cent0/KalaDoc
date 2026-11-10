//! Named destinations — the catalog `/Dests` name tree.
//!
//! A *named destination* is a reusable `[page /Fit …]` target that other
//! PDF objects reference by name (`/Dest /name`, `/A << /S /GoTo /D /name >>`,
//! `/Names` name-tree lookups, …). Two catalog locations are legal:
//!
//! - `/Dests` — either a flat dictionary of `/name → dest` entries or a
//!   *name tree* node (`/Names` array of alternating key/destination
//!   pairs, or `/Kids` intermediate nodes with `/Limits`).
//! - `/Names` → `/Dests` — the same name-tree shapes nested inside the
//!   catalog's name dictionary (which may also hold `/JavaScript`,
//!   `/EmbeddedFiles`, … siblings this module never touches).
//!
//! - [`add_named_destinations`] **upserts** entries: existing names are
//!   collected from *both* locations (the `/Names` → `/Dests` tree first,
//!   then `/Dests`, so the latter wins on collisions), the additions are
//!   merged in — later additions replace earlier ones with the same
//!   name — and the whole set is rewritten as a single sorted flat
//!   `/Dests << /Names [(name) dest …] >>` tree. The now-redundant
//!   `/Names` → `/Dests` subtree is removed from the catalog's `/Names`
//!   dictionary so there is exactly one source of truth; superseded
//!   objects are left for [`garbage_collect`](super::garbage_collect).
//!
//! - [`list_named_destinations`] walks `/Names` arrays *and* `/Kids`
//!   subtrees (depth- and count-capped for hostile input) in both
//!   locations and resolves each destination's 1-based page number via
//!   `Document::get_pages`. Unresolvable destinations report
//!   `page: None`. Encrypted input yields an empty list (strings cannot
//!   be decoded), matching `list_outline`'s tolerance.
//!
//! - [`remove_named_destination`] deletes one name using the same
//!   merge-and-rewrite path; removing the last entry drops `/Dests`
//!   from the catalog entirely. A missing name is a no-op (the document
//!   is still re-serialized, like `remove_outline`).
//!
//! Transforming ops reject encrypted input via
//! [`reject_encrypted`](super::load::reject_encrypted) and enforce
//! [`EngineLimits`] through [`load_pdf`](super::load::load_pdf).

use std::collections::{BTreeMap, HashMap};

use lopdf::{Document, Object, ObjectId, decode_text_string, dictionary};
use serde::{Deserialize, Serialize};

use crate::crates::core::engine::EngineLimits;

use crate::crates::pdf_engine::error::{PdfError, PdfResult};

use super::crop::number_object;
use super::load::{doc_is_encrypted, load_pdf, lopdf_err, reject_encrypted, save_pdf};

/// Maximum name-tree depth [`collect_name_pairs`] will descend.
const MAX_TREE_DEPTH: u32 = 32;

/// Maximum entries [`list_named_destinations`] / the rewrite path will
/// collect; hostile trees are truncated rather than failing.
const MAX_COLLECTED_DESTS: usize = 100_000;

/// Maximum destinations a single [`add_named_destinations`] call may
/// carry.
const MAX_ADD_ITEMS: usize = 10_000;

/// Maximum total destinations after merging additions with existing
/// ones — guards against unbounded `/Names` arrays.
const MAX_TOTAL_DESTS: usize = 100_000;

/// Maximum byte length of a destination name accepted by
/// [`add_named_destinations`].
const MAX_NAME_BYTES: usize = 4096;

/// Maximum nested named-destination indirections
/// [`named_dest_page`] will follow.
const MAX_DEST_DEPTH: u32 = 8;

/// One named destination to write, as accepted by
/// [`add_named_destinations`]. `page` is **1-based**.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct NamedDest {
    /// Destination name — the key lookups use (`/Dest /name`).
    pub name: String,
    /// Target page, 1-based.
    pub page: u32,
    /// View fit applied when the destination is followed.
    pub fit: FitMode,
}

/// The `/Fit` family of destination fit modes. Each variant maps to the
/// eponymous PDF fit operator and its operands.
#[derive(Debug, Clone, Copy, PartialEq, Serialize, Deserialize)]
pub enum FitMode {
    /// `/Fit` — fit the whole page in the window.
    Fit,
    /// `/FitH y` — fit horizontally, top at `y`.
    FitH(f64),
    /// `/FitV x` — fit vertically, left edge at `x`.
    FitV(f64),
    /// `/FitR left bottom right top` — fit the given rectangle.
    FitR([f64; 4]),
    /// `/FitB` — fit the bounding box of the page's annotations.
    FitB,
    /// `/FitBH y` — like `/FitH` but on the bounding box.
    FitBH(f64),
    /// `/FitBV x` — like `/FitV` but on the bounding box.
    FitBV(f64),
}

/// One row returned by [`list_named_destinations`].
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct NamedDestInfo {
    /// Destination name (name-tree key decoded to text).
    pub name: String,
    /// Resolved 1-based target page, or `None` when the destination
    /// is remote, malformed, or points at a name that does not resolve.
    pub page: Option<u32>,
}

// ---------------------------------------------------------------------------
// add_named_destinations
// ---------------------------------------------------------------------------

/// Merge `additions` into the document's named destinations and return
/// the serialized PDF.
///
/// See the module docs for the storage layout: everything is collected
/// and rewritten as one sorted `/Dests << /Names […] >>` name tree.
/// `page` is 1-based and must exist — violations are
/// [`PdfError::InvalidPageRange`]; empty/overlong names and non-finite
/// fit coordinates are [`PdfError::InvalidInput`].
pub fn add_named_destinations(
    input: &[u8],
    additions: &[NamedDest],
    limits: &EngineLimits,
) -> PdfResult<Vec<u8>> {
    let mut doc = load_pdf(input, limits)?;
    reject_encrypted(&doc)?;

    if additions.len() > MAX_ADD_ITEMS {
        return Err(PdfError::LimitExceeded(format!(
            "{} named destinations supplied, exceeding the {MAX_ADD_ITEMS} cap",
            additions.len()
        )));
    }

    let pages = doc.get_pages(); // BTreeMap<u32, ObjectId>, 1-based keys
    let page_count = pages.len() as u32;

    // Validate everything before touching the document.
    let mut new_dests: Vec<(String, Object)> = Vec::with_capacity(additions.len());
    for add in additions {
        if add.name.is_empty() {
            return Err(PdfError::InvalidInput(
                "named destination name must not be empty".to_string(),
            ));
        }
        if add.name.len() > MAX_NAME_BYTES {
            return Err(PdfError::InvalidInput(format!(
                "named destination name exceeds {MAX_NAME_BYTES} bytes"
            )));
        }
        if add.page == 0 || add.page > page_count {
            return Err(PdfError::InvalidPageRange(format!(
                "named destination {:?} targets page {}, but the document has {} page(s)",
                add.name, add.page, page_count
            )));
        }
        if !fit_is_finite(&add.fit) {
            return Err(PdfError::InvalidInput(format!(
                "named destination {:?} has a non-finite fit coordinate",
                add.name
            )));
        }
        let Some(&page_id) = pages.get(&add.page) else {
            return Err(PdfError::InvalidPageRange(format!(
                "named destination {:?} targets page {}, but the document has {} page(s)",
                add.name, add.page, page_count
            )));
        };
        new_dests.push((add.name.clone(), build_dest_array(page_id, add.fit)));
    }

    let mut entries = gather_named_dests(&doc);
    if entries.len().saturating_add(new_dests.len()) > MAX_TOTAL_DESTS {
        return Err(PdfError::LimitExceeded(format!(
            "named destinations would exceed the {MAX_TOTAL_DESTS} cap"
        )));
    }
    // Dedupe-replace: later additions win over earlier ones and over
    // any pre-existing entry of the same name.
    for (name, dest) in new_dests {
        entries.insert(name, dest);
    }

    write_named_dests(&mut doc, &entries)?;
    save_pdf(&mut doc)
}

// ---------------------------------------------------------------------------
// list_named_destinations
// ---------------------------------------------------------------------------

/// Return all named destinations in name order.
///
/// See the module docs for which catalog locations are searched and how
/// pages are resolved. Encrypted input returns an empty list.
pub fn list_named_destinations(
    input: &[u8],
    limits: &EngineLimits,
) -> PdfResult<Vec<NamedDestInfo>> {
    let doc = load_pdf(input, limits)?;
    if doc_is_encrypted(&doc) {
        // Encrypted strings cannot be decoded — report an empty list
        // rather than garbage, matching list_outline's tolerance.
        return Ok(Vec::new());
    }

    let entries = gather_named_dests(&doc);
    let pages = doc.get_pages();
    let page_count = pages.len() as u32;
    let page_ids: HashMap<ObjectId, u32> = pages.iter().map(|(&n, &id)| (id, n)).collect();

    let mut out = Vec::with_capacity(entries.len());
    for (name, dest) in &entries {
        let page = named_dest_page(&doc, &entries, dest, &page_ids, page_count, 0);
        out.push(NamedDestInfo {
            name: name.clone(),
            page,
        });
    }
    Ok(out)
}

// ---------------------------------------------------------------------------
// remove_named_destination
// ---------------------------------------------------------------------------

/// Remove the named destination `name` and return the serialized PDF.
///
/// Uses the same merge-and-rewrite path as [`add_named_destinations`],
/// so the name is dropped from *both* catalog locations. Removing the
/// last destination removes `/Dests` from the catalog. A name that is
/// not present leaves the destinations unchanged.
pub fn remove_named_destination(
    input: &[u8],
    name: &str,
    limits: &EngineLimits,
) -> PdfResult<Vec<u8>> {
    let mut doc = load_pdf(input, limits)?;
    reject_encrypted(&doc)?;

    if name.is_empty() {
        return Err(PdfError::InvalidInput(
            "named destination name must not be empty".to_string(),
        ));
    }

    let mut entries = gather_named_dests(&doc);
    entries.remove(name);

    write_named_dests(&mut doc, &entries)?;
    save_pdf(&mut doc)
}

// ---------------------------------------------------------------------------
// internals
// ---------------------------------------------------------------------------

/// Whether every coordinate carried by `fit` is finite.
fn fit_is_finite(fit: &FitMode) -> bool {
    match *fit {
        FitMode::Fit | FitMode::FitB => true,
        FitMode::FitH(y) | FitMode::FitV(y) | FitMode::FitBH(y) | FitMode::FitBV(y) => {
            y.is_finite()
        }
        FitMode::FitR(rect) => rect.iter().all(|v| v.is_finite()),
    }
}

/// Build `[page_ref /Fit …]` — the operands after the page reference
/// for the given fit mode.
fn fit_tail(fit: FitMode) -> Vec<Object> {
    let fit_name = |s: &[u8]| Object::Name(s.to_vec());
    match fit {
        FitMode::Fit => vec![fit_name(b"Fit")],
        FitMode::FitH(y) => vec![fit_name(b"FitH"), number_object(y)],
        FitMode::FitV(x) => vec![fit_name(b"FitV"), number_object(x)],
        FitMode::FitR(rect) => {
            let mut v = vec![fit_name(b"FitR")];
            v.extend(rect.iter().map(|&n| number_object(n)));
            v
        }
        FitMode::FitB => vec![fit_name(b"FitB")],
        FitMode::FitBH(y) => vec![fit_name(b"FitBH"), number_object(y)],
        FitMode::FitBV(x) => vec![fit_name(b"FitBV"), number_object(x)],
    }
}

/// Build the full destination array `[page /Fit …]` for `page_id`.
fn build_dest_array(page_id: ObjectId, fit: FitMode) -> Object {
    let mut items = vec![Object::Reference(page_id)];
    items.extend(fit_tail(fit));
    Object::Array(items)
}

/// Collect every named destination visible to readers, keyed by name.
///
/// Both catalog locations are searched; on a name collision the
/// `/Dests` entry wins (it is collected second). Values are the raw
/// destination objects (arrays, dictionaries or references) so indirect
/// destinations survive the rewrite untouched.
fn gather_named_dests(doc: &Document) -> BTreeMap<String, Object> {
    let mut map = BTreeMap::new();
    let Ok(catalog) = doc.catalog() else {
        return map;
    };

    // Lower precedence first: catalog /Names → /Dests name tree.
    if let Ok(names) = catalog.get(b"Names")
        && let Ok((_, names_obj)) = doc.dereference(names)
        && let Ok(names_dict) = names_obj.as_dict()
        && let Ok(dests) = names_dict.get(b"Dests")
    {
        collect_name_pairs(doc, dests, &mut map, 0);
    }

    // Catalog /Dests — flat dictionary or name tree.
    if let Ok(dests) = catalog.get(b"Dests") {
        collect_name_pairs(doc, dests, &mut map, 0);
    }

    map
}

/// Walk one `/Dests` value — a name-tree node (`/Names` leaf or `/Kids`
/// intermediate) or a plain `/name → dest` dictionary — inserting
/// `name → dest object` pairs into `out`.
fn collect_name_pairs(
    doc: &Document,
    value: &Object,
    out: &mut BTreeMap<String, Object>,
    depth: u32,
) {
    if depth > MAX_TREE_DEPTH || out.len() >= MAX_COLLECTED_DESTS {
        return;
    }
    let Ok((_, resolved)) = doc.dereference(value) else {
        return;
    };
    let Ok(dict) = resolved.as_dict() else {
        return;
    };

    // Name-tree leaf: /Names [(key) dest …].
    if dict.has(b"Names") {
        if let Ok(names) = dict.get(b"Names")
            && let Ok((_, names_obj)) = doc.dereference(names)
            && let Ok(items) = names_obj.as_array()
        {
            for pair in items.as_chunks::<2>().0 {
                if let Some(name) = pair_key_name(doc, &pair[0]) {
                    out.insert(name, pair[1].clone());
                }
            }
        }
        return;
    }

    // Name-tree intermediate node: /Kids [node …].
    if dict.has(b"Kids") {
        if let Ok(kids) = dict.get(b"Kids")
            && let Ok((_, kids_obj)) = doc.dereference(kids)
            && let Ok(kid_list) = kids_obj.as_array()
        {
            for kid in kid_list {
                collect_name_pairs(doc, kid, out, depth + 1);
            }
        }
        return;
    }

    // Plain dictionary form: /Dests << /name dest … >>. Name-tree node
    // dictionaries always carry /Names or /Kids, so anything reaching
    // here is a flat map; /Limits is filtered defensively.
    for (key, value) in dict.iter() {
        if key.as_slice() == b"Limits".as_slice() {
            continue;
        }
        out.insert(String::from_utf8_lossy(key).into_owned(), value.clone());
    }
}

/// Decode a name-tree pair key (a text string or, defensively, a name)
/// to the destination name.
fn pair_key_name(doc: &Document, key: &Object) -> Option<String> {
    let (_, resolved) = doc.dereference(key).ok()?;
    match resolved {
        Object::String(..) => decode_text_string(resolved).ok().or_else(|| {
            resolved
                .as_str()
                .ok()
                .map(|b| String::from_utf8_lossy(b).into_owned())
        }),
        Object::Name(bytes) => Some(String::from_utf8_lossy(bytes).into_owned()),
        _ => None,
    }
}

/// Rewrite the catalog's named destinations from `entries`.
///
/// Writes `/Dests << /Names [(name) dest …] >>` sorted by name (the
/// `BTreeMap` iteration order — byte-wise on the UTF-8 text, which
/// matches PDF name-tree ordering for these keys). When `entries` is
/// empty the `/Dests` key is removed instead. In both cases the
/// `/Names` → `/Dests` subtree is dropped from the catalog `/Names`
/// dictionary so no stale duplicate survives; the orphaned objects are
/// left for garbage collection.
fn write_named_dests(doc: &mut Document, entries: &BTreeMap<String, Object>) -> PdfResult<()> {
    // Single source of truth: drop the /Names → /Dests name tree first
    // so the rewrite below is the only copy.
    remove_names_tree_dests(doc)?;

    if entries.is_empty() {
        doc.catalog_mut().map_err(lopdf_err)?.remove(b"Dests");
        return Ok(());
    }

    let mut pairs = Vec::with_capacity(entries.len() * 2);
    for (name, dest) in entries {
        pairs.push(Object::string_literal(name.as_str()));
        pairs.push(dest.clone());
    }
    let dests_id = doc
        .try_add_object(dictionary! { "Names" => Object::Array(pairs) })
        .map_err(lopdf_err)?;
    doc.catalog_mut()
        .map_err(lopdf_err)?
        .set("Dests", Object::Reference(dests_id));
    Ok(())
}

/// Remove the `Dests` key from the catalog's `/Names` dictionary,
/// whether it is an indirect dictionary or an inline one. Tolerant: a
/// missing or non-dictionary `/Names` is not an error.
fn remove_names_tree_dests(doc: &mut Document) -> PdfResult<()> {
    let names_obj = match doc
        .catalog()
        .ok()
        .and_then(|c| c.get(b"Names").ok().cloned())
    {
        Some(obj) => obj,
        None => return Ok(()),
    };

    if let Ok(id) = names_obj.as_reference() {
        if let Ok(dict) = doc.get_dictionary_mut(id) {
            dict.remove(b"Dests");
        }
    } else if let Ok(catalog) = doc.catalog_mut()
        && let Ok(slot) = catalog.get_mut(b"Names")
        && let Ok(dict) = slot.as_dict_mut()
    {
        dict.remove(b"Dests");
    }
    Ok(())
}

/// Resolve a destination object to a 1-based page number.
///
/// Handles `[page /Fit …]` arrays (page as indirect reference or, like
/// `outline`, a bare integer), destinations that are themselves names
/// (resolved through `entries`, depth-capped), and dictionaries
/// wrapping a `/D` entry.
#[allow(clippy::too_many_arguments)]
fn named_dest_page(
    doc: &Document,
    entries: &BTreeMap<String, Object>,
    dest: &Object,
    page_ids: &HashMap<ObjectId, u32>,
    page_count: u32,
    depth: u32,
) -> Option<u32> {
    if depth > MAX_DEST_DEPTH {
        return None;
    }
    let (_, resolved) = doc.dereference(dest).ok()?;
    match resolved {
        Object::Array(items) => {
            let first = items.first()?;
            match first {
                Object::Reference(id) => page_ids.get(id).copied(),
                // Some writers emit the page number directly.
                Object::Integer(n) if *n >= 1 && *n <= i64::from(page_count) => Some(*n as u32),
                Object::String(..) | Object::Name(_) => {
                    let name = pair_key_name(doc, first)?;
                    entries.get(&name).and_then(|inner| {
                        named_dest_page(doc, entries, inner, page_ids, page_count, depth + 1)
                    })
                }
                _ => None,
            }
        }
        Object::String(..) | Object::Name(_) => {
            let name = pair_key_name(doc, resolved)?;
            entries.get(&name).and_then(|inner| {
                named_dest_page(doc, entries, inner, page_ids, page_count, depth + 1)
            })
        }
        Object::Dictionary(dict) => {
            let inner = dict.get(b"D").ok()?;
            named_dest_page(doc, entries, inner, page_ids, page_count, depth + 1)
        }
        _ => None,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::crates::pdf_engine::ops::encrypt::{self, PdfPermissions};
    use crate::crates::pdf_engine::ops::test_utils::{page_count, pdf_with_pages};
    use lopdf::{Document, dictionary};

    fn limits() -> EngineLimits {
        EngineLimits::default()
    }

    fn nd(name: &str, page: u32, fit: FitMode) -> NamedDest {
        NamedDest {
            name: name.to_string(),
            page,
            fit,
        }
    }

    /// Extract `(name, dest array)` pairs straight from the catalog
    /// `/Dests` name tree of a serialized PDF, for structural asserts.
    fn dests_pairs(pdf: &[u8]) -> Vec<(String, Vec<Object>)> {
        let doc = Document::load_mem(pdf).unwrap();
        let catalog = doc.catalog().unwrap();
        let (_, dests_obj) = doc.dereference(catalog.get(b"Dests").unwrap()).unwrap();
        let dests = dests_obj.as_dict().unwrap();
        let (_, names_obj) = doc.dereference(dests.get(b"Names").unwrap()).unwrap();
        let items = names_obj.as_array().unwrap();
        items
            .as_chunks::<2>()
            .0
            .iter()
            .map(|pair| {
                let name = String::from_utf8_lossy(pair[0].as_str().unwrap()).into_owned();
                let (_, dest) = doc.dereference(&pair[1]).unwrap();
                (name, dest.as_array().unwrap().clone())
            })
            .collect()
    }

    #[test]
    fn add_then_list_roundtrip() {
        let pdf = pdf_with_pages(&["a", "b", "c"]);
        let out = add_named_destinations(
            &pdf,
            &[
                nd("chap1", 1, FitMode::Fit),
                nd("chap3", 3, FitMode::FitH(400.5)),
            ],
            &limits(),
        )
        .unwrap();
        assert_eq!(page_count(&out), 3);

        let entries = list_named_destinations(&out, &limits()).unwrap();
        assert_eq!(entries.len(), 2);
        assert_eq!(entries[0].name, "chap1");
        assert_eq!(entries[0].page, Some(1));
        assert_eq!(entries[1].name, "chap3");
        assert_eq!(entries[1].page, Some(3));

        // Structure: catalog /Dests is a name tree with a /Names array.
        let pairs = dests_pairs(&out);
        assert_eq!(pairs.len(), 2);
        assert_eq!(
            pairs[0].1,
            vec![pairs[0].1[0].clone(), Object::Name(b"Fit".to_vec())]
        );
        // /FitH carries its y operand.
        assert_eq!(pairs[1].1[1], Object::Name(b"FitH".to_vec()));
        assert_eq!(pairs[1].1[2].as_float().unwrap(), 400.5);
    }

    #[test]
    fn fit_modes_serialize() {
        let pdf = pdf_with_pages(&["x"]);
        let fits = [
            ("f", FitMode::Fit),
            ("fh", FitMode::FitH(1.5)),
            ("fv", FitMode::FitV(2.5)),
            ("fr", FitMode::FitR([0.0, 1.0, 2.0, 3.0])),
            ("fb", FitMode::FitB),
            ("fbh", FitMode::FitBH(4.5)),
            ("fbv", FitMode::FitBV(5.5)),
        ];
        let dests: Vec<NamedDest> = fits.iter().map(|(n, f)| nd(n, 1, *f)).collect();
        let out = add_named_destinations(&pdf, &dests, &limits()).unwrap();
        let pairs = dests_pairs(&out);
        assert_eq!(pairs.len(), 7);

        let expect = [
            ("f", "Fit", 1usize),
            ("fh", "FitH", 2),
            ("fv", "FitV", 2),
            ("fr", "FitR", 5),
            ("fb", "FitB", 1),
            ("fbh", "FitBH", 2),
            ("fbv", "FitBV", 2),
        ];
        // add_named_destinations sorts by name, so index by name
        // rather than relying on insertion order.
        let by_name: std::collections::HashMap<&str, &Vec<Object>> =
            pairs.iter().map(|(n, a)| (n.as_str(), a)).collect();
        for (ename, fit_name, arity) in expect.iter() {
            let arr = by_name
                .get(*ename)
                .unwrap_or_else(|| panic!("missing dest {ename}"));
            // [page, /FitX, operands…]
            assert_eq!(arr.len(), arity + 1, "{ename}");
            let Object::Name(fit) = &arr[1] else {
                panic!("expected fit name")
            };
            assert_eq!(fit, fit_name.as_bytes(), "{ename}");
        }
        // FitR operands in order.
        let fr = pairs.iter().find(|(n, _)| n == "fr").unwrap();
        let vals: Vec<f32> = fr.1[2..].iter().map(|o| o.as_float().unwrap()).collect();
        assert_eq!(vals, vec![0.0, 1.0, 2.0, 3.0]);
    }

    #[test]
    fn add_replaces_existing_name() {
        let pdf = pdf_with_pages(&["a", "b"]);
        let first = add_named_destinations(&pdf, &[nd("x", 1, FitMode::Fit)], &limits()).unwrap();
        let second = add_named_destinations(
            &first,
            &[nd("x", 2, FitMode::FitV(9.0)), nd("y", 2, FitMode::FitB)],
            &limits(),
        )
        .unwrap();

        let entries = list_named_destinations(&second, &limits()).unwrap();
        assert_eq!(entries.len(), 2);
        let x = entries.iter().find(|e| e.name == "x").unwrap();
        assert_eq!(x.page, Some(2));

        let pairs = dests_pairs(&second);
        assert_eq!(pairs.len(), 2);
        let xpair = pairs.iter().find(|(n, _)| n == "x").unwrap();
        assert_eq!(xpair.1[1], Object::Name(b"FitV".to_vec()));
    }

    #[test]
    fn remove_drops_entries() {
        let pdf = pdf_with_pages(&["a", "b"]);
        let with = add_named_destinations(
            &pdf,
            &[nd("keep", 1, FitMode::Fit), nd("drop", 2, FitMode::Fit)],
            &limits(),
        )
        .unwrap();

        let out = remove_named_destination(&with, "drop", &limits()).unwrap();
        let entries = list_named_destinations(&out, &limits()).unwrap();
        assert_eq!(entries.len(), 1);
        assert_eq!(entries[0].name, "keep");
        assert_eq!(entries[0].page, Some(1));

        // Removing the last one drops /Dests from the catalog.
        let out = remove_named_destination(&out, "keep", &limits()).unwrap();
        assert!(list_named_destinations(&out, &limits()).unwrap().is_empty());
        let doc = Document::load_mem(&out).unwrap();
        assert!(!doc.catalog().unwrap().has(b"Dests"));

        // Removing a missing name is a no-op.
        let again = remove_named_destination(&out, "ghost", &limits()).unwrap();
        assert!(
            list_named_destinations(&again, &limits())
                .unwrap()
                .is_empty()
        );
    }

    /// PDF whose catalog uses the *plain dictionary* /Dests form and a
    /// `/Names` → `/Dests` name tree, plus a `/Kids` intermediate node.
    fn pdf_with_exotic_dests() -> Vec<u8> {
        let mut doc = Document::with_version("1.5");
        let pages_id = doc.new_object_id();
        let mut kids = Vec::new();
        for _ in 0..2 {
            let page_id = doc.add_object(dictionary! {
                "Type" => "Page",
                "Parent" => pages_id,
                "MediaBox" => vec![0.into(), 0.into(), 595.into(), 842.into()],
            });
            kids.push(Object::Reference(page_id));
        }
        doc.objects.insert(
            pages_id,
            Object::Dictionary(dictionary! {
                "Type" => "Pages",
                "Kids" => kids.clone(),
                "Count" => 2i64,
            }),
        );
        let page1 = kids[0].as_reference().unwrap();
        let page2 = kids[1].as_reference().unwrap();

        // /Dests as a flat dictionary: /flat → page 1.
        let dests_flat_id = doc.add_object(dictionary! {
            "flat" => Object::Array(vec![
                Object::Reference(page1),
                Object::Name(b"Fit".to_vec()),
            ]),
        });

        // /Names → /Dests name tree with a /Kids intermediate node:
        //   kid1 /Names [(kidname) → page2], kid2 /Names [(other) → page1].
        let leaf1_id = doc.add_object(dictionary! {
            "Names" => Object::Array(vec![
                Object::string_literal("kidname"),
                Object::Array(vec![
                    Object::Reference(page2),
                    Object::Name(b"Fit".to_vec()),
                ]),
            ]),
            "Limits" => Object::Array(vec![
                Object::string_literal("kidname"),
                Object::string_literal("kidname"),
            ]),
        });
        let leaf2_id = doc.add_object(dictionary! {
            "Names" => Object::Array(vec![
                Object::string_literal("other"),
                Object::Array(vec![
                    Object::Reference(page1),
                    Object::Name(b"Fit".to_vec()),
                ]),
            ]),
        });
        let tree_id = doc.add_object(dictionary! {
            "Kids" => Object::Array(vec![
                Object::Reference(leaf1_id),
                Object::Reference(leaf2_id),
            ]),
            "Limits" => Object::Array(vec![
                Object::string_literal("kidname"),
                Object::string_literal("other"),
            ]),
        });
        let names_id = doc.add_object(dictionary! {
            "Dests" => Object::Reference(tree_id),
            "EmbeddedFiles" => dictionary! {},
        });

        let catalog_id = doc.add_object(dictionary! {
            "Type" => "Catalog",
            "Pages" => pages_id,
            "Dests" => dests_flat_id,
            "Names" => names_id,
        });
        doc.trailer.set("Root", catalog_id);
        let mut buf = Vec::new();
        doc.save_to(&mut buf).unwrap();
        buf
    }

    #[test]
    fn list_walks_kids_and_both_locations() {
        let pdf = pdf_with_exotic_dests();
        let entries = list_named_destinations(&pdf, &limits()).unwrap();
        let names: Vec<&str> = entries.iter().map(|e| e.name.as_str()).collect();
        assert_eq!(names, vec!["flat", "kidname", "other"]);
        assert_eq!(entries[0].page, Some(1));
        assert_eq!(entries[1].page, Some(2));
        assert_eq!(entries[2].page, Some(1));
    }

    #[test]
    fn add_merges_and_normalizes_exotic_dests() {
        let pdf = pdf_with_exotic_dests();
        let out = add_named_destinations(&pdf, &[nd("new", 2, FitMode::FitR([0.0; 4]))], &limits())
            .unwrap();
        let entries = list_named_destinations(&out, &limits()).unwrap();
        let names: Vec<&str> = entries.iter().map(|e| e.name.as_str()).collect();
        assert_eq!(names, vec!["flat", "kidname", "new", "other"]);

        // /Names → /Dests must be gone; sibling /EmbeddedFiles preserved.
        let doc = Document::load_mem(&out).unwrap();
        let catalog = doc.catalog().unwrap();
        let (_, names_obj) = doc.dereference(catalog.get(b"Names").unwrap()).unwrap();
        let names_dict = names_obj.as_dict().unwrap();
        assert!(!names_dict.has(b"Dests"));
        assert!(names_dict.has(b"EmbeddedFiles"));

        // Remove one migrated name.
        let out = remove_named_destination(&out, "kidname", &limits()).unwrap();
        let names: Vec<String> = list_named_destinations(&out, &limits())
            .unwrap()
            .into_iter()
            .map(|e| e.name)
            .collect();
        assert_eq!(names, vec!["flat", "new", "other"]);
    }

    #[test]
    fn add_rejects_bad_input() {
        let pdf = pdf_with_pages(&["a", "b"]);
        // Page out of range.
        let err = add_named_destinations(&pdf, &[nd("x", 3, FitMode::Fit)], &limits()).unwrap_err();
        assert!(matches!(err, PdfError::InvalidPageRange(_)));
        // Empty name.
        let err = add_named_destinations(&pdf, &[nd("", 1, FitMode::Fit)], &limits()).unwrap_err();
        assert!(matches!(err, PdfError::InvalidInput(_)));
        // Non-finite coordinate.
        let err = add_named_destinations(&pdf, &[nd("x", 1, FitMode::FitH(f64::NAN))], &limits())
            .unwrap_err();
        assert!(matches!(err, PdfError::InvalidInput(_)));
        // Empty name on remove.
        let err = remove_named_destination(&pdf, "", &limits()).unwrap_err();
        assert!(matches!(err, PdfError::InvalidInput(_)));
    }

    #[test]
    fn transforms_reject_encrypted() {
        let pdf = pdf_with_pages(&["a"]);
        let enc =
            encrypt::encrypt(&pdf, "user", None, PdfPermissions::default(), &limits()).unwrap();
        let dest = [nd("x", 1, FitMode::Fit)];
        assert!(matches!(
            add_named_destinations(&enc, &dest, &limits()),
            Err(PdfError::Encrypted)
        ));
        assert!(matches!(
            remove_named_destination(&enc, "x", &limits()),
            Err(PdfError::Encrypted)
        ));
        // Listing tolerates encryption (empty result).
        assert!(list_named_destinations(&enc, &limits()).unwrap().is_empty());
    }

    #[test]
    fn list_empty_without_dests() {
        let pdf = pdf_with_pages(&["a"]);
        assert!(list_named_destinations(&pdf, &limits()).unwrap().is_empty());
    }
}
