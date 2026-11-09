//! Read and replace the document outline (bookmarks).
//!
//! The outline lives in the catalog's `/Outlines` entry: a root
//! dictionary (`/Type /Outlines`, `/First`, `/Last`, `/Count`) whose
//! items form a doubly-linked tree via `/First`/`/Last` (children),
//! `/Next`/`/Prev` (siblings) and `/Parent`, each item carrying a
//! `/Title` text string and either a `/Dest` destination array or an
//! `/A` action dictionary.
//!
//! - [`set_outline`] **replaces** the whole outline. The flat
//!   `(title, page, level, …)` spec is first sorted *stably* by page
//!   number (ties keep their input order — Rust's `sort_by_key` is
//!   stable), then validated and woven into a proper tree. Every item
//!   gets a `/Dest` of `[page /Fit]` — viewers jump to the top-left of
//!   the page; per-item fit modes are not supported. `/F` flag bits
//!   are set for bold (bit 2, value 2) and italic (bit 1, value 1).
//!
//!   **Level rules** (documented, enforced with
//!   [`PdfError::InvalidInput`]): the first item *after sorting* must
//!   have level `0` or `1`; levels are then treated relative to that
//!   first level, so a spec written entirely at level `1` is fine.
//!   A level may never skip — an item at effective level `N` requires
//!   an enclosing ancestor at level `N-1` (checked with a stack, so
//!   `0,1,1,0,2` fails at the last item). Effective depth is capped at
//!   [`MAX_OUTLINE_DEPTH`]. `page` is 1-based and must exist, else
//!   [`PdfError::InvalidPageRange`]. An **empty** `items` slice is
//!   equivalent to [`remove_outline`].
//!
//! - [`remove_outline`] deletes the `/Outlines` entry from the catalog
//!   and drops the now-unreachable outline objects from the file.
//!
//! - [`list_outline`] walks an existing outline depth-first
//!   (pre-order) and returns [`OutlineEntry`] rows. Titles come from
//!   `/Title` via `decode_text_string`. The target page is resolved
//!   from `/Dest` (explicit arrays and named destinations looked up in
//!   the catalog `/Dests` dictionary and the `/Names` → `/Dests` name
//!   tree) or from an `/A` `/GoTo` action; `/GoToR`, `/URI` and
//!   unresolvable named destinations yield `page: None`. Encrypted
//!   input is tolerated (like `extract_metadata`): the result is an
//!   empty list because encrypted strings cannot be decoded.
//!
//! Like every transforming op, [`set_outline`]/[`remove_outline`]
//! reject encrypted input and enforce [`EngineLimits`] through
//! [`load_pdf`](super::load::load_pdf).

use std::collections::{HashMap, HashSet};

use lopdf::{Document, Object, ObjectId, decode_text_string, dictionary, text_string};
use serde::{Deserialize, Serialize};

use crate::crates::core::engine::EngineLimits;

use crate::crates::pdf_engine::error::{PdfError, PdfResult};

use super::load::{doc_is_encrypted, load_pdf, lopdf_err, reject_encrypted, save_pdf};

/// Maximum nesting depth accepted by [`set_outline`]. Deeper trees are
/// rejected as [`PdfError::InvalidInput`]; real documents essentially
/// never exceed a handful of levels.
const MAX_OUTLINE_DEPTH: u32 = 64;

/// Maximum number of items [`set_outline`] will write — a guard
/// against accidentally enormous specs.
const MAX_SET_ITEMS: usize = 100_000;

/// Maximum entries [`list_outline`] returns; hostile/cyclic trees are
/// truncated rather than failing.
const MAX_LIST_ITEMS: usize = 4096;

/// Maximum object ids [`remove_outline`] will try to delete.
const MAX_DELETE_IDS: usize = 100_000;

/// One row of the flat outline spec accepted by [`set_outline`].
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct OutlineItemSpec {
    /// Bookmark title shown in the viewer's outline pane.
    pub title: String,
    /// Target page, **1-based**.
    pub page: u32,
    /// Nesting level. See the module docs for the exact rules.
    pub level: u32,
    /// Render the title bold (`/F` bit 2).
    pub bold: bool,
    /// Render the title italic (`/F` bit 1).
    pub italic: bool,
}

/// One row of an existing outline, returned by [`list_outline`].
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct OutlineEntry {
    /// Item title (`/Title` decoded; empty when absent/undecodable).
    pub title: String,
    /// Resolved 1-based target page, or `None` when the destination
    /// is remote (`/GoToR`), a `/URI` action, or a named destination
    /// this op could not resolve.
    pub page: Option<u32>,
    /// Nesting depth: `0` for top-level items, `+1` per level of
    /// `/First` children.
    pub level: u32,
}

// ---------------------------------------------------------------------------
// set_outline
// ---------------------------------------------------------------------------

/// Replace the document's entire outline with `items` and return the
/// serialized PDF.
///
/// Items are sorted stably by `page` before the tree is built — see
/// the module docs for the level rules and the empty-slice behavior.
pub fn set_outline(
    input: &[u8],
    items: &[OutlineItemSpec],
    limits: &EngineLimits,
) -> PdfResult<Vec<u8>> {
    let mut doc = load_pdf(input, limits)?;
    reject_encrypted(&doc)?;

    if items.is_empty() {
        remove_outline_tree(&mut doc);
        return save_pdf(&mut doc);
    }
    if items.len() > MAX_SET_ITEMS {
        return Err(PdfError::LimitExceeded(format!(
            "outline has {} items, exceeding the {} item cap",
            items.len(),
            MAX_SET_ITEMS
        )));
    }

    let page_count = doc.get_pages().len() as u32;
    let pages = doc.get_pages(); // BTreeMap<u32, ObjectId>, 1-based keys

    // Stable sort by page number: bookmarks conventionally follow page
    // order, and ties keep the caller's ordering.
    let mut order: Vec<usize> = (0..items.len()).collect();
    order.sort_by_key(|&i| items[i].page);

    // Validate pages and compute effective levels with a stack. The
    // stack holds indices into `order` (positions in sorted order).
    let mut eff_level = vec![0u32; items.len()];
    let mut parent: Vec<Option<usize>> = vec![None; items.len()]; // index into `order`
    let mut children: Vec<Vec<usize>> = vec![Vec::new(); items.len()];
    let mut stack: Vec<usize> = Vec::new();
    let base_level = items[order[0]].level;
    if base_level > 1 {
        return Err(PdfError::InvalidInput(format!(
            "first outline item must have level 0 or 1, got {}",
            items[order[0]].level
        )));
    }

    for (pos, &src) in order.iter().enumerate() {
        let spec = &items[src];
        if spec.page == 0 || spec.page > page_count {
            return Err(PdfError::InvalidPageRange(format!(
                "outline item {:?} targets page {}, but the document has {} page(s)",
                spec.title, spec.page, page_count
            )));
        }
        if spec.level < base_level {
            return Err(PdfError::InvalidInput(format!(
                "outline level {} drops below the first item's level {}",
                spec.level, base_level
            )));
        }
        let level = spec.level - base_level;
        if level > MAX_OUTLINE_DEPTH {
            return Err(PdfError::InvalidInput(format!(
                "outline nesting deeper than {MAX_OUTLINE_DEPTH} levels is not supported"
            )));
        }
        eff_level[pos] = level;

        // Pop ancestors at the same or deeper level; what remains must
        // be exactly `level` deep so the parent sits at level-1.
        while let Some(&top) = stack.last()
            && eff_level[top] >= level
        {
            stack.pop();
        }
        if stack.len() != level as usize {
            return Err(PdfError::InvalidInput(format!(
                "outline level {} has no enclosing item at level {}",
                spec.level,
                base_level + level.saturating_sub(1)
            )));
        }
        parent[pos] = stack.last().copied();
        if let Some(&p) = stack.last() {
            children[p].push(pos);
        }
        stack.push(pos);
    }

    // Total visible descendants per node (children always appear later
    // in sorted order, so a reverse pass computes counts bottom-up).
    let mut descendant_count = vec![0i64; items.len()];
    for pos in (0..items.len()).rev() {
        descendant_count[pos] = children[pos]
            .iter()
            .map(|&c| 1 + descendant_count[c])
            .sum();
    }
    let roots: Vec<usize> = (0..items.len()).filter(|&p| parent[p].is_none()).collect();

    // Drop any pre-existing outline before writing the new one.
    remove_outline_tree(&mut doc);

    // Create one indirect object per item.
    let mut ids: Vec<ObjectId> = Vec::with_capacity(items.len());
    for &src in &order {
        let spec = &items[src];
        let page_id = pages[&spec.page];
        let mut dict = dictionary! {
            "Title" => text_string(&spec.title),
            "Dest" => Object::Array(vec![
                Object::Reference(page_id),
                Object::Name(b"Fit".to_vec()),
            ]),
        };
        let flags = (spec.bold as i64) << 1 | (spec.italic as i64);
        if flags != 0 {
            dict.set("F", flags);
        }
        ids.push(doc.try_add_object(dict).map_err(lopdf_err)?);
    }

    // Root /Outlines dictionary.
    let outlines_id = doc
        .try_add_object(dictionary! { "Type" => "Outlines" })
        .map_err(lopdf_err)?;

    // Wire /Parent, sibling links and child ranges.
    for pos in 0..items.len() {
        let id = ids[pos];
        let dict = doc.get_dictionary_mut(id).map_err(lopdf_err)?;
        let parent_id = match parent[pos] {
            Some(p) => ids[p],
            None => outlines_id,
        };
        dict.set("Parent", parent_id);
    }

    // Link each sibling chain and set /First//Last//Count on parents.
    link_children(&mut doc, outlines_id, &roots, &ids, None)?;
    for pos in 0..items.len() {
        if !children[pos].is_empty() {
            link_children(
                &mut doc,
                ids[pos],
                &children[pos],
                &ids,
                Some(descendant_count[pos]),
            )?;
        }
    }

    // Root count = total number of visible items.
    {
        let root = doc.get_dictionary_mut(outlines_id).map_err(lopdf_err)?;
        root.set("Count", items.len() as i64);
    }

    doc.catalog_mut()
        .map_err(lopdf_err)?
        .set("Outlines", outlines_id);

    save_pdf(&mut doc)
}

/// Set `/Prev`, `/Next`, `/First`, `/Last` and `/Count` for one parent
/// node's child chain. `parent_count` is `Some(descendants)` for item
/// parents (positive = expanded) and `None` for the `/Outlines` root,
/// whose `/Count` is set separately.
fn link_children(
    doc: &mut Document,
    parent_id: ObjectId,
    kids: &[usize],
    ids: &[ObjectId],
    parent_count: Option<i64>,
) -> PdfResult<()> {
    if kids.is_empty() {
        return Ok(());
    }
    {
        let parent = doc.get_dictionary_mut(parent_id).map_err(lopdf_err)?;
        parent.set("First", Object::Reference(ids[kids[0]]));
        parent.set("Last", Object::Reference(ids[kids[kids.len() - 1]]));
        if let Some(count) = parent_count {
            parent.set("Count", count);
        }
    }
    for (j, &kid_pos) in kids.iter().enumerate() {
        let kid_id = ids[kid_pos];
        let dict = doc.get_dictionary_mut(kid_id).map_err(lopdf_err)?;
        if j == 0 {
            dict.remove(b"Prev");
        } else {
            dict.set("Prev", Object::Reference(ids[kids[j - 1]]));
        }
        if j + 1 == kids.len() {
            dict.remove(b"Next");
        } else {
            dict.set("Next", Object::Reference(ids[kids[j + 1]]));
        }
    }
    Ok(())
}

// ---------------------------------------------------------------------------
// remove_outline
// ---------------------------------------------------------------------------

/// Delete the catalog `/Outlines` entry and its object tree, returning
/// the serialized PDF. Documents without an outline are returned
/// unchanged (still re-serialized through `save_pdf`).
pub fn remove_outline(input: &[u8], limits: &EngineLimits) -> PdfResult<Vec<u8>> {
    let mut doc = load_pdf(input, limits)?;
    reject_encrypted(&doc)?;
    remove_outline_tree(&mut doc);
    save_pdf(&mut doc)
}

/// Remove `/Outlines` from the catalog and best-effort delete the
/// outline objects. Tolerant: a missing or malformed outline is not an
/// error — the catalog key is removed either way.
fn remove_outline_tree(doc: &mut Document) {
    // Grab the /Outlines object, then drop the catalog entry while the
    // borrow on `doc` is released.
    let outlines_obj = doc
        .catalog()
        .ok()
        .and_then(|c| c.get(b"Outlines").ok().cloned());
    if let Some(catalog) = doc.catalog_mut().ok() {
        catalog.remove(b"Outlines");
    }
    let Some(outlines_obj) = outlines_obj else {
        return;
    };

    // Collect every object id reachable from the outline root through
    // /First and /Next links, then delete them all.
    let mut ids: Vec<ObjectId> = Vec::new();
    let mut visited: HashSet<ObjectId> = HashSet::new();
    let mut stack: Vec<Object> = vec![outlines_obj];
    while let Some(obj) = stack.pop() {
        if ids.len() >= MAX_DELETE_IDS {
            break;
        }
        let (_, resolved) = match doc.dereference(&obj) {
            Ok(pair) => pair,
            Err(_) => continue,
        };
        let Some(dict) = resolved.as_dict().ok() else {
            continue;
        };
        // Record this object's own id when it is indirect.
        if let Ok(id) = obj.as_reference()
            && visited.insert(id)
        {
            ids.push(id);
        }
        for key in [b"First".as_slice(), b"Next".as_slice()] {
            if let Ok(child) = dict.get(key)
                && let Ok(child_id) = child.as_reference()
                && visited.contains(&child_id)
            {
                continue;
            }
            if let Ok(child) = dict.get(key) {
                stack.push(child.clone());
            }
        }
    }
    for id in ids {
        doc.delete_object(id);
    }
}

// ---------------------------------------------------------------------------
// list_outline
// ---------------------------------------------------------------------------

/// Walk the document outline and return its entries in document order
/// (depth-first, pre-order). See the module docs for resolution rules.
pub fn list_outline(input: &[u8], limits: &EngineLimits) -> PdfResult<Vec<OutlineEntry>> {
    let doc = load_pdf(input, limits)?;
    if doc_is_encrypted(&doc) {
        // Encrypted strings cannot be decoded — report an empty outline
        // rather than garbage, matching extract_metadata's tolerance.
        return Ok(Vec::new());
    }

    let pages = doc.get_pages();
    let page_count = pages.len() as u32;
    let page_ids: HashMap<ObjectId, u32> = pages.iter().map(|(&n, &id)| (id, n)).collect();
    let catalog = match doc.catalog() {
        Ok(c) => c,
        Err(_) => return Ok(Vec::new()),
    };
    let Some(outlines) = catalog.get(b"Outlines").ok() else {
        return Ok(Vec::new());
    };
    let Ok((_, outlines_obj)) = doc.dereference(outlines) else {
        return Ok(Vec::new());
    };
    let Ok(outlines_dict) = outlines_obj.as_dict() else {
        return Ok(Vec::new());
    };
    let Ok(first) = outlines_dict.get(b"First") else {
        return Ok(Vec::new());
    };

    let mut out = Vec::new();
    let mut visited: HashSet<ObjectId> = HashSet::new();
    walk_outline_chain(
        &doc,
        catalog,
        first,
        0,
        page_count,
        &page_ids,
        &mut out,
        &mut visited,
    );
    Ok(out)
}

/// Walk a `/Next` sibling chain starting at `first`, emitting each
/// item and recursing into its `/First` children.
#[allow(clippy::too_many_arguments)]
fn walk_outline_chain(
    doc: &Document,
    catalog: &lopdf::Dictionary,
    first: &Object,
    level: u32,
    page_count: u32,
    page_ids: &HashMap<ObjectId, u32>,
    out: &mut Vec<OutlineEntry>,
    visited: &mut HashSet<ObjectId>,
) {
    let mut current = Some(first.clone());
    let mut depth_guard = 0usize;
    while let Some(obj) = current.take() {
        if out.len() >= MAX_LIST_ITEMS || depth_guard > MAX_LIST_ITEMS {
            return;
        }
        depth_guard += 1;

        let Some(dict) = resolve_dict(doc, &obj) else {
            return;
        };
        // Cycle guard for indirect items.
        if let Ok(id) = obj.as_reference() {
            if !visited.insert(id) {
                return;
            }
        }

        let title = dict
            .get(b"Title")
            .ok()
            .and_then(|o| doc.dereference(o).ok())
            .and_then(|(_, o)| decode_text_string(o).ok())
            .unwrap_or_default();
        let page = outline_item_page(doc, catalog, dict, page_count, page_ids);
        out.push(OutlineEntry { title, page, level });

        // Children first (pre-order), then the next sibling.
        if level < MAX_OUTLINE_DEPTH
            && let Ok(child_first) = dict.get(b"First")
        {
            walk_outline_chain(
                doc,
                catalog,
                child_first,
                level + 1,
                page_count,
                page_ids,
                out,
                visited,
            );
        }
        current = dict.get(b"Next").ok().cloned();
    }
}

/// Dereference `obj` to a dictionary, whether it is an indirect
/// reference or an inline dictionary.
fn resolve_dict<'a>(doc: &'a Document, obj: &'a Object) -> Option<&'a lopdf::Dictionary> {
    let (_, resolved) = doc.dereference(obj).ok()?;
    resolved.as_dict().ok()
}

/// Resolve an outline item's target page from `/Dest` or `/A`.
fn outline_item_page(
    doc: &Document,
    catalog: &lopdf::Dictionary,
    item: &lopdf::Dictionary,
    page_count: u32,
    page_ids: &HashMap<ObjectId, u32>,
) -> Option<u32> {
    // Prefer an explicit /Dest; fall back to a /GoTo action.
    if let Ok(dest) = item.get(b"Dest")
        && let Some(page) = dest_page_number(doc, catalog, dest, page_count, page_ids, 0)
    {
        return Some(page);
    }
    if let Ok(action) = item.get(b"A")
        && let Ok((_, action_obj)) = doc.dereference(action)
        && let Ok(action_dict) = action_obj.as_dict()
    {
        let is_goto = action_dict
            .get(b"S")
            .ok()
            .and_then(|o| doc.dereference(o).ok())
            .and_then(|(_, o)| o.as_name().ok())
            .is_some_and(|name| name == b"GoTo");
        if is_goto
            && let Ok(d) = action_dict.get(b"D")
        {
            return dest_page_number(doc, catalog, d, page_count, page_ids, 0);
        }
        // /GoToR, /URI and friends carry no local page.
    }
    None
}

/// Extract the 1-based page number from a destination object:
/// `[page /Fit …]` arrays, named destinations (resolved through the
/// catalog `/Dests` dictionary or the `/Names` → `/Dests` name tree),
/// and (defensively) dictionaries wrapping a `/D` entry.
fn dest_page_number(
    doc: &Document,
    catalog: &lopdf::Dictionary,
    dest: &Object,
    page_count: u32,
    page_ids: &HashMap<ObjectId, u32>,
    depth: u32,
) -> Option<u32> {
    if depth > 8 {
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
                    dest_name(first).and_then(|name| {
                        named_destination(doc, catalog, &name)
                            .and_then(|obj| dest_page_number(doc, catalog, obj, page_count, page_ids, depth + 1))
                    })
                }
                _ => None,
            }
        }
        Object::String(..) | Object::Name(_) => {
            let name = dest_name(resolved)?;
            let target = named_destination(doc, catalog, &name)?;
            dest_page_number(doc, catalog, target, page_count, page_ids, depth + 1)
        }
        Object::Dictionary(dict) => {
            // Rare, but a destination dictionary may wrap /D.
            let inner = dict.get(b"D").ok()?;
            dest_page_number(doc, catalog, inner, page_count, page_ids, depth + 1)
        }
        _ => None,
    }
}

/// The name carried by a destination object (`/Dest /name` or
/// `/Dest (name)`), as display text for name-tree lookups.
fn dest_name(obj: &Object) -> Option<String> {
    match obj {
        Object::Name(bytes) => Some(String::from_utf8_lossy(bytes).into_owned()),
        Object::String(bytes, _) => decode_text_string(obj)
            .ok()
            .or_else(|| Some(String::from_utf8_lossy(bytes).into_owned())),
        _ => None,
    }
}

/// Find a named destination: first the catalog `/Dests` dictionary
/// (keys are names), then the `/Names` → `/Dests` number-tree-style
/// `/Names` array of `[key dest …]` pairs. Returns the raw destination
/// object (array, dictionary or reference) for [`dest_page_number`].
fn named_destination<'a>(
    doc: &'a Document,
    catalog: &'a lopdf::Dictionary,
    name: &str,
) -> Option<&'a Object> {
    // /Dests << /name [page /Fit] … >>
    if let Ok(dests) = catalog.get(b"Dests")
        && let Ok((_, dests_obj)) = doc.dereference(dests)
        && let Ok(dests_dict) = dests_obj.as_dict()
    {
        for (key, value) in dests_dict.iter() {
            if key_text_matches(key, name) {
                return Some(value);
            }
        }
    }

    // /Names << /Dests << /Names [(name) dest …] >> >>
    if let Ok(names) = catalog.get(b"Names")
        && let Ok((_, names_obj)) = doc.dereference(names)
        && let Ok(names_dict) = names_obj.as_dict()
        && let Ok(dests) = names_dict.get(b"Dests")
        && let Ok((_, dests_obj)) = doc.dereference(dests)
        && let Ok(dests_dict) = dests_obj.as_dict()
        && let Ok(pairs) = dests_dict.get(b"Names")
        && let Ok((_, pairs_obj)) = doc.dereference(pairs)
        && let Ok(items) = pairs_obj.as_array()
    {
        for pair in items.chunks_exact(2) {
            let key_text = decode_text_string(&pair[0])
                .ok()
                .or_else(|| pair[0].as_str().ok().map(|b| String::from_utf8_lossy(b).into_owned()));
            if key_text.as_deref() == Some(name) {
                return Some(&pair[1]);
            }
        }
    }
    None
}

/// Compare a `/Dests` dictionary key (raw bytes, usually a PDF name)
/// against a destination name string.
fn key_text_matches(key: &[u8], name: &str) -> bool {
    key == name.as_bytes() || String::from_utf8_lossy(key) == name
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::crates::pdf_engine::ops::test_utils::{page_count, pdf_with_pages};
    use lopdf::{Document, dictionary};

    fn limits() -> EngineLimits {
        EngineLimits::default()
    }

    fn spec(title: &str, page: u32, level: u32) -> OutlineItemSpec {
        OutlineItemSpec {
            title: title.to_string(),
            page,
            level,
            bold: false,
            italic: false,
        }
    }

    #[test]
    fn set_then_list_roundtrip() {
        let pdf = pdf_with_pages(&["p1", "p2", "p3"]);
        let items = vec![
            OutlineItemSpec { bold: true, ..spec("Chapter 1", 1, 0) },
            OutlineItemSpec { italic: true, ..spec("Section 1.1", 2, 1) },
            spec("Chapter 2", 3, 0),
        ];
        let out = set_outline(&pdf, &items, &limits()).unwrap();
        assert_eq!(page_count(&out), 3);

        let entries = list_outline(&out, &limits()).unwrap();
        assert_eq!(entries.len(), 3);
        assert_eq!(entries[0].title, "Chapter 1");
        assert_eq!(entries[0].page, Some(1));
        assert_eq!(entries[0].level, 0);
        assert_eq!(entries[1].title, "Section 1.1");
        assert_eq!(entries[1].page, Some(2));
        assert_eq!(entries[1].level, 1);
        assert_eq!(entries[2].title, "Chapter 2");
        assert_eq!(entries[2].level, 0);

        // The catalog must reference an /Outlines tree.
        let doc = Document::load_mem(&out).unwrap();
        let catalog = doc.catalog().unwrap();
        assert!(catalog.has(b"Outlines"));
        let (_, outlines) = doc.dereference(catalog.get(b"Outlines").unwrap()).unwrap();
        let outlines = outlines.as_dict().unwrap();
        assert_eq!(outlines.get(b"Count").unwrap().as_i64().unwrap(), 3);
        // Top-level chain has two items; first has one child.
        let first = outlines.get(b"First").unwrap();
        let (_, first) = doc.dereference(first).unwrap();
        let first = first.as_dict().unwrap();
        assert!(first.has(b"Next"));
        assert!(first.has(b"First")); // child "Section 1.1"
        assert!(first.has(b"Count"));
        // Bold/italic flags: item 0 bold → F=2, item 1 italic → F=1.
        assert_eq!(first.get(b"F").unwrap().as_i64().unwrap(), 2);
    }

    #[test]
    fn set_outline_sorts_stably_by_page() {
        let pdf = pdf_with_pages(&["a", "b", "c"]);
        // Given out of page order; equal pages keep input order.
        let items = vec![
            spec("on page 3", 3, 0),
            spec("on page 1 first", 1, 0),
            spec("on page 1 second", 1, 0),
            spec("on page 2", 2, 0),
        ];
        let out = set_outline(&pdf, &items, &limits()).unwrap();
        let titles: Vec<String> = list_outline(&out, &limits())
            .unwrap()
            .into_iter()
            .map(|e| e.title)
            .collect();
        assert_eq!(
            titles,
            vec!["on page 1 first", "on page 1 second", "on page 2", "on page 3"]
        );
    }

    #[test]
    fn set_outline_level_one_base_is_allowed() {
        let pdf = pdf_with_pages(&["a", "b"]);
        let items = vec![spec("top", 1, 1), spec("child", 2, 2)];
        let out = set_outline(&pdf, &items, &limits()).unwrap();
        let entries = list_outline(&out, &limits()).unwrap();
        assert_eq!(entries[0].level, 0);
        assert_eq!(entries[1].level, 1);
    }

    #[test]
    fn set_outline_rejects_level_skips() {
        let pdf = pdf_with_pages(&["a", "b", "c"]);
        // 0 → 2 skips level 1.
        let bad = vec![spec("a", 1, 0), spec("b", 2, 2)];
        let err = set_outline(&pdf, &bad, &limits()).unwrap_err();
        assert!(matches!(err, PdfError::InvalidInput(_)));

        // First item deeper than 1.
        let bad = vec![spec("a", 1, 3)];
        assert!(matches!(
            set_outline(&pdf, &bad, &limits()).unwrap_err(),
            PdfError::InvalidInput(_)
        ));

        // Level below the first item's level.
        let bad = vec![spec("a", 1, 1), spec("b", 2, 0)];
        assert!(matches!(
            set_outline(&pdf, &bad, &limits()).unwrap_err(),
            PdfError::InvalidInput(_)
        ));
    }

    #[test]
    fn set_outline_rejects_page_out_of_range() {
        let pdf = pdf_with_pages(&["a"]);
        let bad = vec![spec("ghost", 2, 0)];
        let err = set_outline(&pdf, &bad, &limits()).unwrap_err();
        assert!(matches!(err, PdfError::InvalidPageRange(_)));
    }

    #[test]
    fn replace_and_remove_outline() {
        let pdf = pdf_with_pages(&["a", "b"]);
        let first = set_outline(&pdf, &[spec("one", 1, 0)], &limits()).unwrap();
        assert_eq!(list_outline(&first, &limits()).unwrap().len(), 1);

        let second = set_outline(&first, &[spec("two", 2, 0)], &limits()).unwrap();
        let entries = list_outline(&second, &limits()).unwrap();
        assert_eq!(entries.len(), 1);
        assert_eq!(entries[0].title, "two");
        assert_eq!(entries[0].page, Some(2));

        let removed = remove_outline(&second, &limits()).unwrap();
        assert!(list_outline(&removed, &limits()).unwrap().is_empty());
        let doc = Document::load_mem(&removed).unwrap();
        assert!(!doc.catalog().unwrap().has(b"Outlines"));

        // Removing twice is fine.
        let again = remove_outline(&removed, &limits()).unwrap();
        assert!(list_outline(&again, &limits()).unwrap().is_empty());
    }

    #[test]
    fn empty_spec_removes_outline() {
        let pdf = pdf_with_pages(&["a"]);
        let with = set_outline(&pdf, &[spec("x", 1, 0)], &limits()).unwrap();
        let cleared = set_outline(&with, &[], &limits()).unwrap();
        assert!(list_outline(&cleared, &limits()).unwrap().is_empty());
    }

    /// Build a PDF whose outline uses /A GoTo actions and a named
    /// destination stored in the catalog /Dests dictionary.
    fn pdf_with_exotic_outline() -> Vec<u8> {
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

        // Named destination "chap2" → page 2, stored in /Dests.
        let dests_id = doc.add_object(dictionary! {
            "chap2" => Object::Array(vec![
                Object::Reference(page2),
                Object::Name(b"Fit".to_vec()),
            ]),
        });

        // Items: item1 uses /A GoTo with an explicit dest array;
        // item2 uses /Dest as a *name* resolved via /Dests.
        let item2_id = doc.add_object(dictionary! {
            "Title" => Object::string_literal("Named"),
            "Dest" => Object::Name(b"chap2".to_vec()),
        });
        let item1_id = doc.add_object(dictionary! {
            "Title" => Object::string_literal("Action"),
            "A" => dictionary! {
                "S" => "GoTo",
                "D" => Object::Array(vec![
                    Object::Reference(page1),
                    Object::Name(b"Fit".to_vec()),
                ]),
            },
            "Next" => Object::Reference(item2_id),
        });
        // Fix item2's Prev.
        if let Some(Object::Dictionary(d)) = doc.objects.get_mut(&item2_id) {
            d.set("Prev", Object::Reference(item1_id));
        }
        let outlines_id = doc.add_object(dictionary! {
            "Type" => "Outlines",
            "First" => Object::Reference(item1_id),
            "Last" => Object::Reference(item2_id),
            "Count" => 2i64,
        });
        for id in [item1_id, item2_id] {
            if let Some(Object::Dictionary(d)) = doc.objects.get_mut(&id) {
                d.set("Parent", Object::Reference(outlines_id));
            }
        }
        let catalog_id = doc.add_object(dictionary! {
            "Type" => "Catalog",
            "Pages" => pages_id,
            "Outlines" => outlines_id,
            "Dests" => dests_id,
        });
        doc.trailer.set("Root", catalog_id);
        let mut buf = Vec::new();
        doc.save_to(&mut buf).unwrap();
        buf
    }

    #[test]
    fn list_outline_reads_actions_and_named_dests() {
        let pdf = pdf_with_exotic_outline();
        let entries = list_outline(&pdf, &limits()).unwrap();
        assert_eq!(entries.len(), 2);
        assert_eq!(entries[0].title, "Action");
        assert_eq!(entries[0].page, Some(1));
        assert_eq!(entries[0].level, 0);
        assert_eq!(entries[1].title, "Named");
        assert_eq!(entries[1].page, Some(2));
    }

    #[test]
    fn list_outline_empty_without_outline() {
        let pdf = pdf_with_pages(&["a"]);
        assert!(list_outline(&pdf, &limits()).unwrap().is_empty());
    }
}
