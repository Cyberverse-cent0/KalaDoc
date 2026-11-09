//! PDF "articles" — the legacy magazine-style threading feature
//! (ISO 32000-1 §12.4.9, dropped in PDF 2.0).
//!
//! A document can carry a catalog `/Threads` array of *article thread*
//! dictionaries. Each thread dict has:
//!
//! - `/F` — the first *bead* dictionary (required by the spec);
//! - `/I` — an optional thread-information dictionary, conventionally
//!   carrying a `/Title` text string;
//! - `/Type /Thread` and `/D` (a destination, rarely used).
//!
//! Each *bead* dictionary (`/Type /Bead`) has:
//!
//! - `/T` — back-reference to its thread dictionary;
//! - `/N` / `/V` — next / previous bead (the chain is doubly linked);
//! - `/P` — the page dictionary the bead sits on;
//! - `/R` — the bead's rectangle `[x0 y0 x1 y1]` in default user space.
//!
//! Page dictionaries additionally carry a `/B` array of indirect
//! references to the beads on that page — a redundant index the spec
//! requires writers to maintain.
//!
//! - [`list_articles`] walks catalog `/Threads` → each thread's `/F`
//!   bead chain (`/N` links) and returns one [`ArticleInfo`] per
//!   thread: optional `/Title` from the `/I` info dict, the deduplicated
//!   1-based page numbers covered by its beads, and the bead count.
//!   Bead `/R` rectangles are not surfaced (callers that need geometry
//!   can read the beads themselves); malformed threads or beads are
//!   skipped rather than failing the whole listing. Encrypted input is
//!   tolerated: structure is still reported but titles decode to
//!   `None` because encrypted strings cannot be read.
//!
//! - [`remove_articles`] deletes the catalog `/Threads` entry, strips
//!   `/B` from every page dictionary, and best-effort deletes the
//!   now-unreachable thread, info and bead objects. This removes
//!   *threading metadata only* — it does not touch page content,
//!   annotations, or any other structure.
//!
//! Like every transforming op, [`remove_articles`] rejects encrypted
//! input and enforces [`EngineLimits`] through
//! [`load_pdf`](super::load::load_pdf).

use std::collections::{HashMap, HashSet};

use lopdf::{Document, Object, ObjectId, decode_text_string};
use serde::{Deserialize, Serialize};

use crate::crates::core::engine::EngineLimits;

use crate::crates::pdf_engine::error::PdfResult;

use super::load::{doc_is_encrypted, load_pdf, reject_encrypted, save_pdf};

/// One article thread found in a document, returned by
/// [`list_articles`].
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ArticleInfo {
    /// `/Title` from the thread's `/I` information dictionary, when
    /// present and decodable. `None` for encrypted documents.
    pub title: Option<String>,
    /// 1-based page numbers covered by the thread's beads, deduplicated
    /// and in first-seen (chain) order. Empty when no bead carried a
    /// resolvable `/P` page reference.
    pub pages: Vec<u32>,
    /// Number of bead dictionaries walked in the `/F` → `/N` chain.
    pub bead_count: u32,
}

/// Maximum beads walked across *all* threads in one [`list_articles`]
/// call — a guard against cyclic or enormous chains.
const MAX_BEADS_TOTAL: usize = 100_000;

/// Maximum objects [`remove_articles`] will try to delete.
const MAX_DELETE_IDS: usize = 100_000;

// ---------------------------------------------------------------------------
// list_articles
// ---------------------------------------------------------------------------

/// Walk the document's article threads and return their [`ArticleInfo`]
/// rows in `/Threads` array order. Documents without a `/Threads`
/// catalog entry return an empty list; so do encrypted documents whose
/// threads cannot be decoded (titles are always `None` when encrypted).
pub fn list_articles(input: &[u8], limits: &EngineLimits) -> PdfResult<Vec<ArticleInfo>> {
    let doc = load_pdf(input, limits)?;
    let encrypted = doc_is_encrypted(&doc);

    let catalog = match doc.catalog() {
        Ok(c) => c,
        Err(_) => return Ok(Vec::new()),
    };
    let Ok(threads_obj) = catalog.get(b"Threads") else {
        return Ok(Vec::new());
    };
    let Ok((_, threads_resolved)) = doc.dereference(threads_obj) else {
        return Ok(Vec::new());
    };
    let Ok(threads) = threads_resolved.as_array() else {
        return Ok(Vec::new());
    };

    let page_ids: HashMap<ObjectId, u32> =
        doc.get_pages().iter().map(|(&n, &id)| (id, n)).collect();

    let mut out = Vec::new();
    let mut bead_budget = MAX_BEADS_TOTAL;
    for thread_ref in threads {
        if bead_budget == 0 {
            break;
        }
        if let Some(info) = article_info(&doc, thread_ref, encrypted, &page_ids, &mut bead_budget) {
            out.push(info);
        }
    }
    Ok(out)
}

/// Resolve one `/Threads` entry (a thread dictionary) into an
/// [`ArticleInfo`], walking its `/F` → `/N` bead chain.
fn article_info(
    doc: &Document,
    thread_obj: &Object,
    encrypted: bool,
    page_ids: &HashMap<ObjectId, u32>,
    bead_budget: &mut usize,
) -> Option<ArticleInfo> {
    let (_, thread_resolved) = doc.dereference(thread_obj).ok()?;
    let thread = thread_resolved.as_dict().ok()?;

    let title = thread
        .get(b"I")
        .ok()
        .and_then(|info| doc.dereference(info).ok())
        .and_then(|(_, o)| o.as_dict().ok())
        .and_then(|info| info.get(b"Title").ok())
        .and_then(|t| doc.dereference(t).ok())
        .and_then(|(_, o)| {
            if encrypted {
                None
            } else {
                decode_text_string(o).ok()
            }
        });

    let first_bead = thread.get(b"F").ok()?.clone();
    let mut pages: Vec<u32> = Vec::new();
    let mut bead_count = 0u32;
    let mut visited: HashSet<ObjectId> = HashSet::new();
    let mut current = Some(first_bead);

    while let Some(bead_obj) = current.take() {
        if *bead_budget == 0 {
            break;
        }
        *bead_budget -= 1;

        let (_, bead_resolved) = match doc.dereference(&bead_obj) {
            Ok(pair) => pair,
            Err(_) => break,
        };
        let Ok(bead) = bead_resolved.as_dict() else {
            break;
        };
        // Cycle guard for indirect beads.
        if let Ok(id) = bead_obj.as_reference()
            && !visited.insert(id)
        {
            break;
        }

        bead_count += 1;
        if let Some(page) = bead_page(doc, bead, page_ids)
            && !pages.contains(&page)
        {
            pages.push(page);
        }
        current = bead.get(b"N").ok().cloned();
    }

    Some(ArticleInfo {
        title,
        pages,
        bead_count,
    })
}

/// Resolve a bead's `/P` page reference to a 1-based page number.
fn bead_page(
    doc: &Document,
    bead: &lopdf::Dictionary,
    page_ids: &HashMap<ObjectId, u32>,
) -> Option<u32> {
    let page_obj = bead.get(b"P").ok()?;
    let (id, _) = doc.dereference(page_obj).ok()?;
    page_ids.get(&id?).copied()
}

// ---------------------------------------------------------------------------
// remove_articles
// ---------------------------------------------------------------------------

/// Delete all article-thread metadata and return the serialized PDF:
/// the catalog `/Threads` array, every page's `/B` bead index, and the
/// unreachable thread/info/bead objects themselves. Page content is
/// untouched — only threading metadata is removed. Documents without
/// articles are returned unchanged (still re-serialized).
pub fn remove_articles(input: &[u8], limits: &EngineLimits) -> PdfResult<Vec<u8>> {
    let mut doc = load_pdf(input, limits)?;
    reject_encrypted(&doc)?;

    // Snapshot /Threads before dropping the catalog entry.
    let threads_obj = doc
        .catalog()
        .ok()
        .and_then(|c| c.get(b"Threads").ok().cloned());
    if let Ok(catalog) = doc.catalog_mut() {
        catalog.remove(b"Threads");
    }

    // Strip the per-page /B bead index.
    for page_id in doc.get_pages().values().copied().collect::<Vec<_>>() {
        if let Ok(page) = doc.get_dictionary_mut(page_id) {
            page.remove(b"B");
        }
    }

    // Best-effort delete of the thread graph objects.
    if let Some(threads_obj) = threads_obj {
        for id in collect_article_ids(&doc, &threads_obj) {
            doc.delete_object(id);
        }
    }

    save_pdf(&mut doc)
}

/// Collect the object ids of every thread dictionary, its `/I` info
/// dictionary and every bead reachable through `/F` → `/N` chains.
fn collect_article_ids(doc: &Document, threads_obj: &Object) -> Vec<ObjectId> {
    let mut ids: Vec<ObjectId> = Vec::new();
    let mut visited: HashSet<ObjectId> = HashSet::new();
    let Ok((_, threads_resolved)) = doc.dereference(threads_obj) else {
        return ids;
    };
    let Ok(threads) = threads_resolved.as_array() else {
        return ids;
    };

    for thread_ref in threads {
        if ids.len() >= MAX_DELETE_IDS {
            break;
        }
        // The thread dictionary itself.
        if let Ok(id) = thread_ref.as_reference()
            && visited.insert(id)
        {
            ids.push(id);
        }
        let Ok((_, thread_resolved)) = doc.dereference(thread_ref) else {
            continue;
        };
        let Ok(thread) = thread_resolved.as_dict() else {
            continue;
        };
        // The optional /I information dictionary.
        if let Ok(info) = thread.get(b"I")
            && let Ok(id) = info.as_reference()
            && visited.insert(id)
        {
            ids.push(id);
        }
        // Bead chain: /F then /N links.
        let Ok(first) = thread.get(b"F") else {
            continue;
        };
        let mut current = Some(first.clone());
        while let Some(bead_obj) = current.take() {
            if ids.len() >= MAX_DELETE_IDS {
                break;
            }
            if let Ok(id) = bead_obj.as_reference()
                && visited.insert(id)
            {
                ids.push(id);
            }
            let Ok((_, bead_resolved)) = doc.dereference(&bead_obj) else {
                break;
            };
            let Ok(bead) = bead_resolved.as_dict() else {
                break;
            };
            current = bead.get(b"N").ok().cloned();
        }
    }
    ids
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::crates::pdf_engine::ops::test_utils::{page_count, pdf_with_pages};
    use lopdf::{Document, dictionary, text_string};

    /// Build a 2-page PDF carrying one article thread with three beads
    /// (pages 1, 2, 1) and a titled `/I` info dictionary.
    fn pdf_with_article() -> Vec<u8> {
        let mut doc = Document::load_mem(&pdf_with_pages(&["page one", "page two"])).unwrap();
        let pages = doc.get_pages();
        let p1 = pages[&1];
        let p2 = pages[&2];

        // Bead chain: b1 (page 1) -> b2 (page 2) -> b3 (page 1).
        let b3 = doc.add_object(dictionary! {
            "Type" => "Bead",
            "P" => Object::Reference(p1),
            "R" => vec![10.into(), 10.into(), 100.into(), 50.into()],
        });
        let b2 = doc.add_object(dictionary! {
            "Type" => "Bead",
            "N" => Object::Reference(b3),
            "P" => Object::Reference(p2),
            "R" => vec![10.into(), 60.into(), 100.into(), 100.into()],
        });
        let b1 = doc.add_object(dictionary! {
            "Type" => "Bead",
            "N" => Object::Reference(b2),
            "V" => Object::Null,
            "P" => Object::Reference(p1),
            "R" => vec![10.into(), 10.into(), 100.into(), 50.into()],
        });
        let info = doc.add_object(dictionary! {
            "Title" => text_string("Feature Story"),
        });
        let thread = doc.add_object(dictionary! {
            "Type" => "Thread",
            "F" => Object::Reference(b1),
            "I" => Object::Reference(info),
        });
        // Fix /T back-references and sibling /V links properly.
        doc.get_dictionary_mut(b1)
            .unwrap()
            .set("T", Object::Reference(thread));
        doc.get_dictionary_mut(b2)
            .unwrap()
            .set("T", Object::Reference(thread));
        doc.get_dictionary_mut(b2)
            .unwrap()
            .set("V", Object::Reference(b1));
        doc.get_dictionary_mut(b3)
            .unwrap()
            .set("T", Object::Reference(thread));
        doc.get_dictionary_mut(b3)
            .unwrap()
            .set("V", Object::Reference(b2));
        // Page /B indexes.
        doc.get_dictionary_mut(p1)
            .unwrap()
            .set("B", vec![Object::Reference(b1), Object::Reference(b3)]);
        doc.get_dictionary_mut(p2)
            .unwrap()
            .set("B", vec![Object::Reference(b2)]);

        doc.catalog_mut()
            .unwrap()
            .set("Threads", vec![Object::Reference(thread)]);

        let mut buf = Vec::new();
        doc.save_to(&mut buf).unwrap();
        buf
    }

    #[test]
    fn list_articles_reports_thread() {
        let pdf = pdf_with_article();
        let articles = list_articles(&pdf, &EngineLimits::default()).unwrap();
        assert_eq!(articles.len(), 1);
        let a = &articles[0];
        assert_eq!(a.title.as_deref(), Some("Feature Story"));
        assert_eq!(a.pages, vec![1, 2]);
        assert_eq!(a.bead_count, 3);
    }

    #[test]
    fn list_articles_empty_without_threads() {
        let pdf = pdf_with_pages(&["no threads here"]);
        assert!(
            list_articles(&pdf, &EngineLimits::default())
                .unwrap()
                .is_empty()
        );
    }

    #[test]
    fn remove_articles_strips_threading_only() {
        let pdf = pdf_with_article();
        let out = remove_articles(&pdf, &EngineLimits::default()).unwrap();
        assert_eq!(page_count(&out), 2);

        // /Threads gone from catalog, /B gone from pages, and the
        // listing now returns empty.
        let doc = Document::load_mem(&out).unwrap();
        assert!(doc.catalog().unwrap().get(b"Threads").is_err());
        for (_, page_id) in doc.get_pages() {
            assert!(doc.get_dictionary(page_id).unwrap().get(b"B").is_err());
        }
        assert!(
            list_articles(&out, &EngineLimits::default())
                .unwrap()
                .is_empty()
        );
        // Page content survives.
        let texts =
            crate::crates::pdf_engine::ops::extract_text(&out, None, &EngineLimits::default())
                .unwrap();
        assert!(texts[0].text.contains("page one"));
    }

    #[test]
    fn remove_articles_noop_on_plain_pdf() {
        let pdf = pdf_with_pages(&["plain"]);
        let out = remove_articles(&pdf, &EngineLimits::default()).unwrap();
        assert_eq!(page_count(&out), 1);
    }

    #[test]
    fn ops_reject_garbage() {
        assert!(list_articles(b"not a pdf", &EngineLimits::default()).is_err());
        assert!(remove_articles(b"not a pdf", &EngineLimits::default()).is_err());
    }
}
