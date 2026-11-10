//! Remove document-level named objects — the subtrees of the catalog's
//! `/Names` dictionary.
//!
//! The PDF catalog `/Names` dictionary maps category names to *name
//! trees* (dictionaries with `/Names` key/value pair arrays and/or
//! `/Kids` sub-trees). Well-known categories include `EmbeddedFiles`,
//! `JavaScript`, `Dests`, `IDTree`, `URLS` and `RichMedia`. This op
//! removes whole category subtrees — it does not reach into them to
//! delete individual named destinations or files.
//!
//! What is covered:
//!
//! - `/Names` given as a **direct dictionary** on the catalog or as an
//!   **indirect reference** — both forms are handled.
//! - Category keys are matched after trimming whitespace and an
//!   optional leading `/`, case-insensitively against the canonical
//!   names below; unknown names are tried verbatim against the
//!   dictionary keys (PDF names are case-sensitive, so pass the exact
//!   key for non-standard categories).
//!
//! | Accepted name (any case) | `/Names` key removed |
//! |---|---|
//! | `embeddedfiles`, `embedded_files`, `files` | `EmbeddedFiles` |
//! | `javascript`, `js` | `JavaScript` |
//! | `dests`, `destinations` | `Dests` |
//! | `richmedia`, `rich_media` | `RichMedia` |
//! | `idtree` | `IDTree` |
//! | *(anything else)* | used verbatim as the dictionary key |
//!
//! Concrete ops:
//!
//! - [`remove_embedded_files`] — strips the `EmbeddedFiles` name tree
//!   (embedded file attachments).
//! - [`remove_javascript_names`] — strips the `JavaScript` name tree
//!   (document-level JavaScript name→action bindings).
//!
//! Honest limits (same guarantee level as [`sanitize`][super::sanitize::sanitize]):
//! removal deletes the name-tree *entries*, orphaning the payload
//! objects (embedded file streams, JS action dictionaries) as
//! unreferenced data still present in the file — no object-graph
//! garbage collection is performed. It does **not** remove JavaScript
//! carried in page `/AA` tables, `/OpenAction`, outline actions or
//! annotation actions — use [`sanitize`](super::sanitize::sanitize)
//! for that. A `/Names` dictionary left empty is kept in place
//! (harmless). Encrypted input is rejected.

use lopdf::{Dictionary, Document, Object, ObjectId};

use crate::crates::core::engine::EngineLimits;

use crate::crates::pdf_engine::error::{PdfError, PdfResult};

use super::load::{load_pdf, reject_encrypted, save_pdf};

/// Maximum `/Parent`-style hops when walking name-tree `/Kids` —
/// guards against cyclic trees.
const MAX_TREE_DEPTH: u32 = 32;

/// Friendly/alias spellings mapped to canonical `/Names` keys
/// (comparison is case-insensitive on the alias side).
const CATEGORY_ALIASES: &[(&str, &str)] = &[
    ("embeddedfiles", "EmbeddedFiles"),
    ("embedded_files", "EmbeddedFiles"),
    ("files", "EmbeddedFiles"),
    ("javascript", "JavaScript"),
    ("js", "JavaScript"),
    ("dests", "Dests"),
    ("destinations", "Dests"),
    ("richmedia", "RichMedia"),
    ("rich_media", "RichMedia"),
    ("idtree", "IDTree"),
];

/// What [`remove_named_objects`] did.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct RemoveObjectsReport {
    /// Canonical `/Names` keys that were present and removed, in the
    /// order requested.
    pub removed_categories: Vec<String>,
    /// `(category, entry_count)` pairs parallel to
    /// [`removed_categories`](Self::removed_categories): how many
    /// name/value leaf pairs the removed subtree contained (0 for empty
    /// or non-name-tree values).
    pub counts: Vec<(String, u32)>,
}

/// Normalize a requested category name: trim whitespace, strip one
/// leading `/`, then map known aliases (case-insensitive) to the
/// canonical `/Names` key. Returns `None` for empty input.
fn canonical_category(name: &str) -> Option<String> {
    let trimmed = name.trim().trim_start_matches('/').trim();
    if trimmed.is_empty() {
        return None;
    }
    let lower = trimmed.to_ascii_lowercase();
    for (alias, canonical) in CATEGORY_ALIASES {
        if lower == *alias {
            return Some((*canonical).to_string());
        }
    }
    Some(trimmed.to_string())
}

/// Resolve a name-tree value (possibly a reference) to its dictionary.
fn tree_dict<'a>(doc: &'a Document, obj: &'a Object) -> Option<&'a Dictionary> {
    match obj {
        Object::Dictionary(d) => Some(d),
        Object::Reference(id) => doc.get_object(*id).ok()?.as_dict().ok(),
        _ => None,
    }
}

/// Count leaf name/value pairs in a name-tree dictionary: `/Names`
/// arrays contribute `len / 2` each; `/Kids` sub-trees recurse.
/// Depth-capped; non-dictionary or malformed entries count as 0.
fn count_tree_entries(doc: &Document, dict: &Dictionary, depth: u32) -> u32 {
    if depth > MAX_TREE_DEPTH {
        return 0;
    }
    let mut count = 0u32;
    if let Ok(names) = dict.get(b"Names").and_then(Object::as_array) {
        count += (names.len() / 2) as u32;
    }
    if let Ok(kids) = dict.get(b"Kids").and_then(Object::as_array) {
        for kid in kids {
            if let Some(kid_dict) = tree_dict(doc, kid) {
                count += count_tree_entries(doc, kid_dict, depth + 1);
            }
        }
    }
    count
}

/// Count the entries of a `/Names` category value before removal.
fn count_category(doc: &Document, value: &Object) -> u32 {
    tree_dict(doc, value).map_or(0, |d| count_tree_entries(doc, d, 0))
}

/// The shape of the catalog's `/Names` entry — inspect before mutating
/// (same pattern as `sanitize::strip_names_tree`).
enum NamesShape {
    Direct,
    Indirect(ObjectId),
    Absent,
}

fn names_shape(doc: &Document) -> NamesShape {
    let Ok(root_id) = doc.trailer.get(b"Root").and_then(Object::as_reference) else {
        return NamesShape::Absent;
    };
    doc.get_dictionary(root_id)
        .ok()
        .and_then(|catalog| catalog.get(b"Names").ok())
        .map(|names| match names {
            Object::Dictionary(_) => NamesShape::Direct,
            Object::Reference(id) => NamesShape::Indirect(*id),
            _ => NamesShape::Absent,
        })
        .unwrap_or(NamesShape::Absent)
}

/// Immutable view of the `/Names` dictionary for the given shape.
fn names_dict<'a>(doc: &'a Document, shape: &NamesShape) -> Option<&'a Dictionary> {
    match shape {
        NamesShape::Direct => {
            let root_id = doc.trailer.get(b"Root").ok()?.as_reference().ok()?;
            let catalog = doc.get_dictionary(root_id).ok()?;
            catalog.get(b"Names").ok()?.as_dict().ok()
        }
        NamesShape::Indirect(id) => doc.get_dictionary(*id).ok(),
        NamesShape::Absent => None,
    }
}

/// Apply the requested `/Names` category removals to an already-loaded,
/// decrypted document; returns the report. Shared by
/// [`remove_named_objects`] and [`remove_named_and_save`].
fn apply_removals(doc: &mut Document, names: &[String]) -> PdfResult<RemoveObjectsReport> {
    let mut report = RemoveObjectsReport::default();
    // Dedup canonical keys, preserving first-request order so the
    // report lists categories in the order the caller asked for them.
    let mut requested: Vec<String> = Vec::new();
    for name in names {
        if let Some(canonical) = canonical_category(name)
            && !requested.contains(&canonical)
        {
            requested.push(canonical);
        }
    }
    if requested.is_empty() {
        return Ok(report);
    }

    let shape = names_shape(doc);
    // Phase 1 (immutable): find which requested keys exist and count
    // their subtree entries before removal.
    let mut found: Vec<(String, u32)> = Vec::new();
    if let Some(dict) = names_dict(doc, &shape) {
        for key in &requested {
            if let Ok(value) = dict.get(key.as_bytes()) {
                found.push((key.clone(), count_category(doc, value)));
            }
        }
    }

    // Phase 2 (mutable): remove the keys.
    if !found.is_empty() {
        match shape {
            NamesShape::Direct => {
                let root_id = doc
                    .trailer
                    .get(b"Root")
                    .and_then(Object::as_reference)
                    .map_err(|_| {
                        PdfError::InvalidInput("PDF has no catalog (/Root)".to_string())
                    })?;
                if let Ok(catalog) = doc.get_dictionary_mut(root_id)
                    && let Ok(Object::Dictionary(names_dict_mut)) = catalog.get_mut(b"Names")
                {
                    for (key, _) in &found {
                        names_dict_mut.remove(key.as_bytes());
                    }
                }
            }
            NamesShape::Indirect(id) => {
                if let Ok(dict) = doc.get_dictionary_mut(id) {
                    for (key, _) in &found {
                        dict.remove(key.as_bytes());
                    }
                }
            }
            NamesShape::Absent => {}
        }
        for (key, count) in found {
            report.removed_categories.push(key.clone());
            report.counts.push((key, count));
        }
    }
    Ok(report)
}

/// Remove `names` (canonical `/Names` keys) from the document's
/// `/Names` dictionary and return a [`RemoveObjectsReport`].
///
/// Performs the removal on the loaded document in memory; use
/// [`remove_embedded_files`] or [`remove_javascript_names`] to get the
/// transformed PDF bytes back. Requested categories that are absent
/// from `/Names` are silently skipped (not an error).
pub fn remove_named_objects(
    input: &[u8],
    names: &[String],
    limits: &EngineLimits,
) -> PdfResult<RemoveObjectsReport> {
    let mut doc = load_pdf(input, limits)?;
    reject_encrypted(&doc)?;
    apply_removals(&mut doc, names)
}

/// Remove the document's `EmbeddedFiles` name tree (embedded file
/// attachments) and return the rewritten PDF.
///
/// See the module docs for coverage and limits: the name-tree entries
/// are removed; the orphaned file streams remain as unreferenced
/// objects. JavaScript in actions/AA tables is *not* touched — use
/// [`sanitize`](super::sanitize::sanitize) for full active-content
/// stripping.
pub fn remove_embedded_files(input: &[u8], limits: &EngineLimits) -> PdfResult<Vec<u8>> {
    let names = vec!["EmbeddedFiles".to_string()];
    remove_named_and_save(input, &names, limits).map(|(bytes, _)| bytes)
}

/// Remove the document's `JavaScript` name tree (document-level
/// JavaScript name→action bindings) and return the rewritten PDF.
///
/// Same limits as [`remove_embedded_files`]: only the `/Names`
/// subtree is removed; `/OpenAction`, `/AA`, outline and annotation
/// JavaScript actions are left for [`sanitize`](super::sanitize::sanitize).
pub fn remove_javascript_names(input: &[u8], limits: &EngineLimits) -> PdfResult<Vec<u8>> {
    let names = vec!["JavaScript".to_string()];
    remove_named_and_save(input, &names, limits).map(|(bytes, _)| bytes)
}

/// Load, remove the requested `/Names` categories and save — the
/// shared body behind the concrete ops.
fn remove_named_and_save(
    input: &[u8],
    names: &[String],
    limits: &EngineLimits,
) -> PdfResult<(Vec<u8>, RemoveObjectsReport)> {
    let mut doc = load_pdf(input, limits)?;
    reject_encrypted(&doc)?;
    let report = apply_removals(&mut doc, names)?;
    let bytes = save_pdf(&mut doc)?;
    Ok((bytes, report))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::crates::pdf_engine::ops::test_utils::pdf_with_pages;
    use lopdf::{Document, Stream, dictionary};

    /// A PDF whose catalog has a direct `/Names` dict containing an
    /// `EmbeddedFiles` name tree (two entries) and a `JavaScript` name
    /// tree (one entry), plus a benign `Dests` tree that must survive.
    fn pdf_with_names() -> Vec<u8> {
        let mut doc = Document::load_mem(&pdf_with_pages(&["x"])).unwrap();
        let payload_id = doc.add_object(Stream::new(
            dictionary! { "Type" => "EmbeddedFile" },
            b"file-bytes".to_vec(),
        ));
        let root_id = doc.trailer.get(b"Root").unwrap().as_reference().unwrap();
        {
            let catalog = doc.get_dictionary_mut(root_id).unwrap();
            catalog.set(
                "Names",
                dictionary! {
                    "EmbeddedFiles" => dictionary! {
                        "Names" => Object::Array(vec![
                            Object::string_literal("a.txt"),
                            Object::Reference(payload_id),
                            Object::string_literal("b.txt"),
                            Object::Reference(payload_id),
                        ]),
                    },
                    "JavaScript" => dictionary! {
                        "Names" => Object::Array(vec![
                            Object::string_literal("init"),
                            Object::Reference(payload_id),
                        ]),
                    },
                    "Dests" => dictionary! {
                        "Names" => Object::Array(vec![
                            Object::string_literal("chap1"),
                            Object::Array(vec![1.into(), "Fit".into()]),
                        ]),
                    },
                },
            );
        }
        let mut buf = Vec::new();
        doc.save_to(&mut buf).unwrap();
        buf
    }

    fn names_keys(pdf: &[u8]) -> Vec<String> {
        let doc = Document::load_mem(pdf).unwrap();
        let root = doc.trailer.get(b"Root").unwrap().as_reference().unwrap();
        let catalog = doc.get_dictionary(root).unwrap();
        let names = catalog.get(b"Names").unwrap().as_dict().unwrap();
        names
            .iter()
            .map(|(k, _)| String::from_utf8_lossy(k).into_owned())
            .collect()
    }

    #[test]
    fn removes_embedded_files_and_reports() {
        let pdf = pdf_with_names();
        let out = remove_embedded_files(&pdf, &EngineLimits::default()).unwrap();
        let keys = names_keys(&out);
        assert!(!keys.iter().any(|k| k == "EmbeddedFiles"), "keys: {keys:?}");
        assert!(keys.iter().any(|k| k == "JavaScript"));
        assert!(keys.iter().any(|k| k == "Dests"));

        // Generic entry point reports the same removal with counts.
        let report = remove_named_objects(
            &pdf,
            &["embedded files".to_string(), "EmbeddedFiles".to_string()],
            &EngineLimits::default(),
        )
        .unwrap();
        assert_eq!(report.removed_categories, vec!["EmbeddedFiles"]);
        assert_eq!(report.counts, vec![("EmbeddedFiles".to_string(), 2)]);
    }

    #[test]
    fn removes_javascript_names() {
        let pdf = pdf_with_names();
        let out = remove_javascript_names(&pdf, &EngineLimits::default()).unwrap();
        let keys = names_keys(&out);
        assert!(!keys.iter().any(|k| k == "JavaScript"), "keys: {keys:?}");
        assert!(keys.iter().any(|k| k == "EmbeddedFiles"));
        assert!(keys.iter().any(|k| k == "Dests"));
    }

    #[test]
    fn unknown_and_absent_categories_are_skipped() {
        let pdf = pdf_with_names();
        let report = remove_named_objects(
            &pdf,
            &["NoSuchCategory".to_string(), "   ".to_string()],
            &EngineLimits::default(),
        )
        .unwrap();
        assert!(report.removed_categories.is_empty());
        assert!(report.counts.is_empty());
    }

    #[test]
    fn handles_missing_names_dict() {
        let pdf = pdf_with_pages(&["plain"]);
        let report =
            remove_named_objects(&pdf, &["JavaScript".to_string()], &EngineLimits::default())
                .unwrap();
        assert!(report.removed_categories.is_empty());
        // Concrete op still round-trips a valid PDF.
        let out = remove_javascript_names(&pdf, &EngineLimits::default()).unwrap();
        assert_eq!(Document::load_mem(&out).unwrap().get_pages().len(), 1);
    }

    #[test]
    fn rejects_garbage_and_encrypted() {
        assert!(matches!(
            remove_named_objects(
                b"junk",
                &["JavaScript".to_string()],
                &EngineLimits::default()
            ),
            Err(PdfError::InvalidPdf(_))
        ));
        let pdf = pdf_with_names();
        let encrypted = crate::crates::pdf_engine::ops::encrypt::encrypt(
            &pdf,
            "pw",
            None,
            crate::crates::pdf_engine::ops::encrypt::PdfPermissions::default(),
            &EngineLimits::default(),
        )
        .unwrap();
        assert!(matches!(
            remove_named_objects(
                &encrypted,
                &["JavaScript".to_string()],
                &EngineLimits::default()
            ),
            Err(PdfError::Encrypted)
        ));
        assert!(matches!(
            remove_embedded_files(&encrypted, &EngineLimits::default()),
            Err(PdfError::Encrypted)
        ));
    }
}
