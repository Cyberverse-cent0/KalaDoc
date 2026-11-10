//! Split a PDF into one output file per outline (bookmark) section.
//!
//! The document's `/Outlines` tree is walked exactly like
//! [`list_outline`](super::outline::list_outline) does — depth-first,
//! pre-order, with `/Dest` arrays, `/A` `/GoTo` actions and named
//! destinations resolved to 1-based page numbers — but only items at
//! the requested `level` (`0` = top-level bookmarks) become split
//! boundaries.
//!
//! # Chunk semantics
//!
//! Boundary pages are the sorted, deduplicated set of target pages of
//! the level-`level` items whose destination resolves to a local page.
//! Items with unresolvable destinations (`/GoToR`, `/URI`, missing or
//! unresolvable named destinations) are skipped — they simply do not
//! start a new file. With boundaries `b_1 < b_2 < … < b_n` and page
//! count `N`, output *i* contains:
//!
//! - pages `b_i ..= b_{i+1} - 1` for `i < n` (i.e. from the bookmark's
//!   page up to, but not including, the next bookmark's page), and
//! - pages `b_n ..= N` for the last output.
//!
//! Pages **before** `b_1` are not dropped: they are folded into the
//! first output, which therefore spans `1 ..= b_2 - 1` (or the whole
//! document when `n == 1`). Every output is an independent, valid PDF
//! produced with the same clone-and-`delete_pages` technique as
//! [`split`](super::split::split).
//!
//! # Edge cases
//!
//! - A document with no `/Outlines`, no items at `level`, or no
//!   resolvable destinations returns a **single output identical to
//!   the input** — there is nothing to split, and this mirrors
//!   [`split`](super::split::split) returning one chunk.
//! - `max_outputs` is enforced *before* any chunk is produced:
//!   `LimitExceeded` when the boundary count would exceed it.
//! - Encrypted input is rejected with [`PdfError::Encrypted`], like
//!   every other transforming op.

use std::collections::{BTreeSet, HashMap, HashSet};

use lopdf::{Document, Object, ObjectId, decode_text_string};

use crate::crates::core::engine::EngineLimits;

use crate::crates::pdf_engine::error::{PdfError, PdfResult};

use super::load::{load_pdf, reject_encrypted, save_pdf};

/// Maximum outline items inspected while collecting boundaries;
/// hostile/cyclic trees are truncated rather than failing. Matches
/// `outline::MAX_LIST_ITEMS`.
const MAX_WALK_ITEMS: usize = 4096;

/// Maximum nesting depth descended into while walking the outline.
/// Matches `outline::MAX_OUTLINE_DEPTH`.
const MAX_WALK_DEPTH: u32 = 64;

/// Split `input` at the document's outline items at nesting `level`
/// and return one PDF per section. See the module docs for the exact
/// chunk semantics.
///
/// `level` is 0-based: `0` splits at top-level bookmarks, `1` at their
/// children, and so on. Levels deeper than the outline simply yield no
/// boundaries and therefore a single output.
pub fn split_at_outline(
    input: &[u8],
    level: u32,
    limits: &EngineLimits,
) -> PdfResult<Vec<Vec<u8>>> {
    let doc = load_pdf(input, limits)?;
    reject_encrypted(&doc)?;

    let page_count = doc.get_pages().len() as u32;
    if page_count == 0 {
        return Err(PdfError::InvalidInput(
            "document has no pages to split".to_string(),
        ));
    }

    let boundaries = collect_boundary_pages(&doc, level);
    if boundaries.is_empty() {
        // Nothing to split on — hand back the input unchanged rather
        // than failing, so callers can treat "no bookmarks" and
        // "one section" uniformly.
        return Ok(vec![input.to_vec()]);
    }

    let chunk_count = boundaries.len();
    if let Some(max) = limits.max_outputs
        && chunk_count > max as usize
    {
        return Err(PdfError::LimitExceeded(format!(
            "split at outline level {level} would produce {chunk_count} files, \
             exceeding max_outputs {max}"
        )));
    }

    let bounds: Vec<u32> = boundaries.into_iter().collect();
    let mut outputs = Vec::with_capacity(chunk_count);
    for (i, &start_page) in bounds.iter().enumerate() {
        // The first chunk also carries any pages before the first
        // bookmark so no content is silently dropped.
        let start = if i == 0 { 1 } else { start_page };
        let end = bounds
            .get(i + 1)
            .map(|next| next.saturating_sub(1))
            .unwrap_or(page_count)
            .min(page_count);

        let mut chunk = doc.clone();
        let delete: Vec<u32> = (1..=page_count)
            .filter(|p| *p < start || *p > end)
            .collect();
        chunk.delete_pages(&delete);
        outputs.push(save_pdf(&mut chunk)?);
    }
    Ok(outputs)
}

// ---------------------------------------------------------------------------
// Outline walking — mirrors ops/outline.rs's list_outline helpers, which
// are private to that module.
// ---------------------------------------------------------------------------

/// Collect the sorted set of 1-based target pages of outline items at
/// exactly `level` depth whose destination resolves to a local page.
fn collect_boundary_pages(doc: &Document, level: u32) -> BTreeSet<u32> {
    let mut out = BTreeSet::new();
    if level > MAX_WALK_DEPTH {
        return out;
    }
    let pages = doc.get_pages();
    let page_count = pages.len() as u32;
    let page_ids: HashMap<ObjectId, u32> = pages.iter().map(|(&n, &id)| (id, n)).collect();
    let Ok(catalog) = doc.catalog() else {
        return out;
    };
    let Ok(outlines) = catalog.get(b"Outlines") else {
        return out;
    };
    let Ok((_, outlines_obj)) = doc.dereference(outlines) else {
        return out;
    };
    let Ok(outlines_dict) = outlines_obj.as_dict() else {
        return out;
    };
    let Ok(first) = outlines_dict.get(b"First") else {
        return out;
    };

    let mut visited: HashSet<ObjectId> = HashSet::new();
    walk_outline_level(
        doc,
        catalog,
        first,
        0,
        level,
        page_count,
        &page_ids,
        &mut out,
        &mut visited,
    );
    out
}

/// Walk a `/Next` sibling chain, recording pages of items at
/// `target_level` and recursing into children only while the current
/// depth is below it (deeper levels can never contain boundaries).
#[allow(clippy::too_many_arguments)]
fn walk_outline_level(
    doc: &Document,
    catalog: &lopdf::Dictionary,
    first: &Object,
    depth: u32,
    target_level: u32,
    page_count: u32,
    page_ids: &HashMap<ObjectId, u32>,
    out: &mut BTreeSet<u32>,
    visited: &mut HashSet<ObjectId>,
) {
    let mut current = Some(first.clone());
    let mut guard = 0usize;
    while let Some(obj) = current.take() {
        if out.len() >= MAX_WALK_ITEMS || guard > MAX_WALK_ITEMS {
            return;
        }
        guard += 1;

        let Some(dict) = resolve_dict(doc, &obj) else {
            return;
        };
        if let Ok(id) = obj.as_reference()
            && !visited.insert(id)
        {
            return;
        }

        if depth == target_level
            && let Some(page) = outline_item_page(doc, catalog, dict, page_count, page_ids)
        {
            out.insert(page);
        }

        if depth < target_level
            && depth < MAX_WALK_DEPTH
            && let Ok(child_first) = dict.get(b"First")
        {
            walk_outline_level(
                doc,
                catalog,
                child_first,
                depth + 1,
                target_level,
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

/// Resolve an outline item's target page from `/Dest` or an `/A`
/// `/GoTo` action. Identical rules to `outline::outline_item_page`.
fn outline_item_page(
    doc: &Document,
    catalog: &lopdf::Dictionary,
    item: &lopdf::Dictionary,
    page_count: u32,
    page_ids: &HashMap<ObjectId, u32>,
) -> Option<u32> {
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
        if is_goto && let Ok(d) = action_dict.get(b"D") {
            return dest_page_number(doc, catalog, d, page_count, page_ids, 0);
        }
    }
    None
}

/// Extract the 1-based page number from a destination object:
/// `[page /Fit …]` arrays, named destinations (resolved through the
/// catalog `/Dests` dictionary or the `/Names` → `/Dests` name tree),
/// and dictionaries wrapping a `/D` entry.
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
                Object::Integer(n) if *n >= 1 && *n <= i64::from(page_count) => Some(*n as u32),
                Object::String(..) | Object::Name(_) => dest_name(first).and_then(|name| {
                    named_destination(doc, catalog, &name).and_then(|obj| {
                        dest_page_number(doc, catalog, obj, page_count, page_ids, depth + 1)
                    })
                }),
                _ => None,
            }
        }
        Object::String(..) | Object::Name(_) => {
            let name = dest_name(resolved)?;
            let target = named_destination(doc, catalog, &name)?;
            dest_page_number(doc, catalog, target, page_count, page_ids, depth + 1)
        }
        Object::Dictionary(dict) => {
            let inner = dict.get(b"D").ok()?;
            dest_page_number(doc, catalog, inner, page_count, page_ids, depth + 1)
        }
        _ => None,
    }
}

/// The name carried by a destination object (`/Dest /name` or
/// `/Dest (name)`), as text for name-tree lookups.
fn dest_name(obj: &Object) -> Option<String> {
    match obj {
        Object::Name(bytes) => Some(String::from_utf8_lossy(bytes).into_owned()),
        Object::String(bytes, _) => decode_text_string(obj)
            .ok()
            .or_else(|| Some(String::from_utf8_lossy(bytes).into_owned())),
        _ => None,
    }
}

/// Find a named destination in the catalog `/Dests` dictionary or the
/// `/Names` → `/Dests` name tree.
fn named_destination<'a>(
    doc: &'a Document,
    catalog: &'a lopdf::Dictionary,
    name: &str,
) -> Option<&'a Object> {
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
        for pair in items.as_chunks::<2>().0 {
            let key_text = decode_text_string(&pair[0]).ok().or_else(|| {
                pair[0]
                    .as_str()
                    .ok()
                    .map(|b| String::from_utf8_lossy(b).into_owned())
            });
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
    use crate::crates::pdf_engine::ops::encrypt::{PdfPermissions, encrypt};
    use crate::crates::pdf_engine::ops::outline::{OutlineItemSpec, set_outline};
    use crate::crates::pdf_engine::ops::test_utils::{page_count, pdf_with_pages};

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

    /// Six pages; top-level bookmarks on pages 2 and 5, a level-1
    /// bookmark on page 3.
    fn pdf_with_bookmarks() -> Vec<u8> {
        let pdf = pdf_with_pages(&["a", "b", "c", "d", "e", "f"]);
        let items = vec![
            spec("Part One", 2, 0),
            spec("Section 1.1", 3, 1),
            spec("Part Two", 5, 0),
        ];
        set_outline(&pdf, &items, &limits()).unwrap()
    }

    #[test]
    fn splits_at_top_level_bookmarks() {
        let pdf = pdf_with_bookmarks();
        let outs = split_at_outline(&pdf, 0, &limits()).unwrap();
        assert_eq!(outs.len(), 2);
        // Chunk 1: pages 1..=4 (leading page 1 folded in, up to before
        // the page-5 bookmark). Chunk 2: pages 5..=6.
        assert_eq!(page_count(&outs[0]), 4);
        assert_eq!(page_count(&outs[1]), 2);
    }

    #[test]
    fn splits_at_nested_level() {
        let pdf = pdf_with_bookmarks();
        let outs = split_at_outline(&pdf, 1, &limits()).unwrap();
        // Only "Section 1.1" at level 1 → one boundary → whole doc.
        assert_eq!(outs.len(), 1);
        assert_eq!(page_count(&outs[0]), 6);
    }

    #[test]
    fn level_deeper_than_outline_returns_single_output() {
        let pdf = pdf_with_bookmarks();
        let outs = split_at_outline(&pdf, 7, &limits()).unwrap();
        assert_eq!(outs.len(), 1);
        assert_eq!(page_count(&outs[0]), 6);
    }

    #[test]
    fn no_outline_returns_single_output() {
        let pdf = pdf_with_pages(&["a", "b", "c"]);
        let outs = split_at_outline(&pdf, 0, &limits()).unwrap();
        assert_eq!(outs.len(), 1);
        assert_eq!(outs[0], pdf);
    }

    #[test]
    fn adjacent_bookmarks_still_produce_nonempty_chunks() {
        let pdf = pdf_with_pages(&["a", "b", "c", "d"]);
        let items = vec![spec("x", 2, 0), spec("y", 3, 0), spec("z", 4, 0)];
        let pdf = set_outline(&pdf, &items, &limits()).unwrap();
        let outs = split_at_outline(&pdf, 0, &limits()).unwrap();
        assert_eq!(outs.len(), 3);
        // [1..=2], [3..=3], [4..=4]
        assert_eq!(page_count(&outs[0]), 2);
        assert_eq!(page_count(&outs[1]), 1);
        assert_eq!(page_count(&outs[2]), 1);
    }

    #[test]
    fn enforces_max_outputs() {
        let pdf = pdf_with_bookmarks();
        let lim = EngineLimits {
            max_outputs: Some(1),
            ..EngineLimits::default()
        };
        let err = split_at_outline(&pdf, 0, &lim).unwrap_err();
        assert!(matches!(err, PdfError::LimitExceeded(_)));
    }

    #[test]
    fn rejects_encrypted_input() {
        let pdf = pdf_with_bookmarks();
        let enc = encrypt(&pdf, "user", None, PdfPermissions::default(), &limits()).unwrap();
        let err = split_at_outline(&enc, 0, &limits()).unwrap_err();
        assert!(matches!(err, PdfError::Encrypted));
    }

    #[test]
    fn rejects_garbage() {
        assert!(split_at_outline(b"junk", 0, &limits()).is_err());
    }
}
