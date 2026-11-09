//! Remove image XObjects from selected pages.
//!
//! For every selected page, the effective `/Resources` dictionary (own or
//! inherited from `/Pages` ancestors) is scanned for `/XObject` entries
//! whose dereferenced object is a stream with `/Subtype /Image`. Each
//! matching resource entry is removed, and every `Do` operator in the
//! page's own content streams that references a removed resource name
//! is stripped, so no dangling XObject invocations remain.
//!
//! # Scope and honesty notes
//!
//! - **Resource isolation**: `/Resources` (and its `/XObject`
//!   sub-dictionary) may be shared between pages through indirect
//!   objects or ancestor inheritance. When entries are removed, a fresh
//!   resources dictionary is written onto the page — shared dictionaries
//!   are never mutated, so unselected sibling pages keep their images.
//! - **`Do` stripping**: each `/Contents` stream of the page is decoded
//!   (bounded by the same 64 MiB guard used by `watermark`/`redact`) and
//!   re-encoded with `Do` operations naming removed resources filtered
//!   out. A stream that fails to decode is left untouched — its `Do`
//!   operators become dangling references that viewers render as
//!   nothing (the image data itself is no longer reachable through the
//!   page's resources).
//! - **Form XObjects**: images inside Form XObject content streams, and
//!   `Do` invocations *inside* those forms, are not touched — only the
//!   page's own `/Contents` streams are rewritten.
//! - **Inline images** (`BI`/`ID`/`EI` in content streams) are a
//!   separate mechanism that does not use named resources and are **not**
//!   removed.
//! - **Orphaned objects**: the removed image stream objects are left in
//!   the file (lopdf does not garbage-collect on save). They are no
//!   longer referenced by the affected pages but still occupy space;
//!   `compress`/`sanitize` passes or a re-save through another tool can
//!   drop them.
//! - `images_removed` counts **resource entries removed** — an image
//!   XObject shared by two selected pages counts once per page.
//! - Encrypted documents are rejected.

use std::collections::BTreeSet;

use lopdf::content::Content;
use lopdf::{Document, Object};
use serde::{Deserialize, Serialize};

use crate::crates::core::engine::EngineLimits;

use crate::crates::pdf_engine::error::PdfResult;

use super::load::{load_pdf, lopdf_err, reject_encrypted, save_pdf};
use super::watermark::{effective_resources_dict, selected_pages};

/// Maximum decompressed size accepted from a single existing content
/// stream when stripping `Do` operators — same guard as `watermark`.
const MAX_CONTENT_STREAM: usize = 64 * 1024 * 1024;

/// Result of [`remove_images`].
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct RemoveReport {
    /// Number of `/XObject` resource entries removed across all
    /// selected pages (a shared image counts once per referencing page).
    pub images_removed: u32,
    /// 1-based page numbers on which at least one image was removed,
    /// sorted ascending.
    pub pages_affected: Vec<u32>,
    /// The rewritten PDF document.
    pub bytes: Vec<u8>,
}

/// Remove image XObjects from the pages selected by `pages`
/// (`None` = every page; otherwise a 1-based range spec like `"1-3,8"`).
///
/// Returns a [`RemoveReport`] describing how many resource entries were
/// removed and which pages were affected, plus the rewritten PDF bytes.
/// Encrypted documents are rejected.
pub fn remove_images(
    input: &[u8],
    pages: Option<&str>,
    limits: &EngineLimits,
) -> PdfResult<RemoveReport> {
    let mut doc = load_pdf(input, limits)?;
    reject_encrypted(&doc)?;

    let selected = selected_pages(&doc, pages)?;
    let pages_map = doc.get_pages();

    let mut images_removed = 0u32;
    let mut pages_affected: Vec<u32> = Vec::new();

    for page_number in selected {
        let Some(&page_id) = pages_map.get(&page_number) else {
            continue;
        };
        let removed_names = remove_page_images(&mut doc, page_id)?;
        if removed_names.is_empty() {
            continue;
        }
        images_removed += removed_names.len() as u32;
        pages_affected.push(page_number);
        strip_do_operators(&mut doc, page_id, &removed_names)?;
    }

    let bytes = save_pdf(&mut doc)?;
    Ok(RemoveReport {
        images_removed,
        pages_affected,
        bytes,
    })
}

/// Remove every `/Subtype /Image` entry from the page's effective
/// `/Resources` → `/XObject` dictionary, writing a fresh (page-private)
/// resources dictionary. Returns the removed resource names (raw name
/// bytes, no leading slash); empty when the page has no image XObjects.
fn remove_page_images(
    doc: &mut Document,
    page_id: lopdf::ObjectId,
) -> PdfResult<BTreeSet<Vec<u8>>> {
    let Some(mut resources) = effective_resources_dict(doc, page_id) else {
        return Ok(BTreeSet::new());
    };

    // Resolve the /XObject sub-dictionary. It may be a direct dict or an
    // indirect reference; either way we clone it so shared dictionaries
    // are never mutated.
    let Ok(xobjects_obj) = resources.get(b"XObject") else {
        return Ok(BTreeSet::new());
    };
    let Ok((_, xobjects_resolved)) = doc.dereference(xobjects_obj) else {
        return Ok(BTreeSet::new());
    };
    let Ok(xobjects) = xobjects_resolved.as_dict() else {
        return Ok(BTreeSet::new());
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
        return Ok(BTreeSet::new());
    }

    // Clone the XObject dictionary, drop the image entries, and store it
    // as a direct dictionary inside the cloned resources. Direct storage
    // is deliberate: the resources object written below is page-private,
    // so no shared dictionary is ever touched.
    let mut new_xobjects = xobjects.clone();
    for name in &image_names {
        new_xobjects.remove(name);
    }
    resources.set(b"XObject", Object::Dictionary(new_xobjects));

    // Always write a *new* indirect resources dictionary onto the page —
    // never update a possibly-shared resources object in place.
    let resources_id = doc
        .try_add_object(Object::Dictionary(resources))
        .map_err(lopdf_err)?;
    doc.get_dictionary_mut(page_id)
        .map_err(lopdf_err)?
        .set("Resources", Object::Reference(resources_id));

    Ok(image_names.into_iter().collect())
}

/// Whether a stream dictionary declares `/Subtype /Image`.
fn is_image_stream(doc: &Document, dict: &lopdf::Dictionary) -> bool {
    dict.get(b"Subtype")
        .ok()
        .and_then(|o| doc.dereference(o).ok())
        .and_then(|(_, o)| o.as_name().ok())
        .is_some_and(|name| name == b"Image")
}

/// Rewrite every content stream of the page with `Do` operations that
/// reference `removed_names` filtered out. Streams that fail to decode
/// or re-encode are left untouched (see module docs).
fn strip_do_operators(
    doc: &mut Document,
    page_id: lopdf::ObjectId,
    removed_names: &BTreeSet<Vec<u8>>,
) -> PdfResult<()> {
    for stream_id in doc.get_page_contents(page_id) {
        strip_stream_do(doc, stream_id, removed_names)?;
    }
    Ok(())
}

/// Decode, filter and rewrite a single content stream in place.
fn strip_stream_do(
    doc: &mut Document,
    stream_id: lopdf::ObjectId,
    removed_names: &BTreeSet<Vec<u8>>,
) -> PdfResult<()> {
    let Ok(object) = doc.get_object_mut(stream_id) else {
        return Ok(());
    };
    let Ok(stream) = object.as_stream_mut() else {
        return Ok(());
    };

    let raw = match stream.decompressed_content_with_limit(MAX_CONTENT_STREAM) {
        Ok(data) => data,
        Err(_) if !stream.dict.has(b"Filter") => stream.content.clone(),
        Err(_) => return Ok(()), // cannot safely decode — leave untouched
    };
    let Ok(content) = Content::decode(&raw) else {
        return Ok(());
    };

    let original_len = content.operations.len();
    let kept: Vec<lopdf::content::Operation> = content
        .operations
        .into_iter()
        .filter(|op| !is_removed_do(op, removed_names))
        .collect();
    if kept.len() == original_len {
        return Ok(());
    }

    let encoded = Content { operations: kept }.encode().map_err(lopdf_err)?;
    stream.content = encoded;
    // The new bytes are uncompressed: drop the filter entries so readers
    // do not try to decode them. /Length is rewritten on save.
    stream.dict.remove(b"Filter");
    stream.dict.remove(b"DecodeParms");
    Ok(())
}

/// Whether `op` is a `Do` invocation of one of the removed resource
/// names.
fn is_removed_do(op: &lopdf::content::Operation, removed_names: &BTreeSet<Vec<u8>>) -> bool {
    if op.operator != "Do" {
        return false;
    }
    matches!(
        op.operands.first(),
        Some(Object::Name(name)) if removed_names.contains(name)
    )
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;
    use crate::crates::pdf_engine::ops::extract_images;
    use crate::crates::pdf_engine::ops::extract_text;
    use crate::crates::pdf_engine::ops::test_utils::pdf_with_pages;
    use lopdf::content::Operation;
    use lopdf::{Document, Stream, dictionary};

    fn limits() -> EngineLimits {
        EngineLimits::default()
    }

    /// Build a two-page PDF. Page 1 has an image XObject `/Im1` invoked
    /// via `Do` plus some text; page 2 is a plain text page.
    fn pdf_with_invoked_image() -> Vec<u8> {
        let mut doc = Document::with_version("1.5");
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
                Operation::new(
                    "cm",
                    vec![
                        Object::Real(10.0),
                        Object::Real(0.0),
                        Object::Real(0.0),
                        Object::Real(10.0),
                        Object::Real(50.0),
                        Object::Real(700.0),
                    ],
                ),
                Operation::new("Do", vec![Object::Name(b"Im1".to_vec())]),
                Operation::new("Q", vec![]),
                Operation::new("BT", vec![]),
                Operation::new("Tf", vec!["F1".into(), 12.into()]),
                Operation::new("Td", vec![100.into(), 700.into()]),
                Operation::new("Tj", vec![Object::string_literal("keep me")]),
                Operation::new("ET", vec![]),
            ],
        };
        let content_id = doc.add_object(Stream::new(dictionary! {}, content.encode().unwrap()));
        let page1_id = doc.add_object(dictionary! {
            "Type" => "Page", "Parent" => pages_id,
            "Contents" => content_id, "Resources" => resources_id,
            "MediaBox" => vec![0.into(), 0.into(), 595.into(), 842.into()],
        });

        // Page 2: plain text page sharing nothing.
        let text = lopdf::content::Content {
            operations: vec![
                Operation::new("BT", vec![]),
                Operation::new("Tf", vec!["F1".into(), 12.into()]),
                Operation::new("Td", vec![100.into(), 700.into()]),
                Operation::new("Tj", vec![Object::string_literal("page two")]),
                Operation::new("ET", vec![]),
            ],
        };
        let content2_id = doc.add_object(Stream::new(dictionary! {}, text.encode().unwrap()));
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
        buf
    }

    #[test]
    fn removes_image_and_strips_do() {
        let pdf = pdf_with_invoked_image();
        assert_eq!(extract_images(&pdf, None, &limits()).unwrap().len(), 1);

        let report = remove_images(&pdf, None, &limits()).unwrap();
        assert_eq!(report.images_removed, 1);
        assert_eq!(report.pages_affected, vec![1]);

        // Page 2 shares the same /Resources object — it must keep the
        // image entry (page-private resources are written per page).
        let images = extract_images(&report.bytes, None, &limits()).unwrap();
        assert_eq!(images.len(), 1);
        assert_eq!(images[0].page, 2);

        // The rewritten page-1 content must not contain a Do for Im1.
        let doc = Document::load_mem(&report.bytes).unwrap();
        let pages = doc.get_pages();
        let page1 = pages.get(&1).copied().unwrap();
        let mut saw_do = false;
        for stream_id in doc.get_page_contents(page1) {
            let obj = doc.get_object(stream_id).unwrap();
            let stream = obj.as_stream().unwrap();
            let raw = stream
                .decompressed_content()
                .unwrap_or_else(|_| stream.content.clone());
            if let Ok(content) = Content::decode(&raw) {
                saw_do |= content.operations.iter().any(|op| {
                    op.operator == "Do"
                        && matches!(op.operands.first(), Some(Object::Name(n)) if n == b"Im1")
                });
            }
        }
        assert!(!saw_do, "page 1 still invokes /Im1 via Do");

        // Text on page 1 survives the content rewrite.
        let texts = extract_text(&report.bytes, None, &limits()).unwrap();
        assert!(texts[0].text.contains("keep me"));
    }

    #[test]
    fn page_range_limits_removal() {
        let pdf = pdf_with_invoked_image();
        let report = remove_images(&pdf, Some("2"), &limits()).unwrap();
        assert_eq!(report.images_removed, 0);
        assert!(report.pages_affected.is_empty());
        assert_eq!(
            extract_images(&report.bytes, None, &limits())
                .unwrap()
                .len(),
            1
        );
    }

    #[test]
    fn imageless_document_is_noop() {
        let pdf = pdf_with_pages(&["a", "b"]);
        let report = remove_images(&pdf, None, &limits()).unwrap();
        assert_eq!(report.images_removed, 0);
        assert!(report.pages_affected.is_empty());
        Document::load_mem(&report.bytes).unwrap();
    }

    #[test]
    fn rejects_garbage_and_bad_range() {
        assert!(remove_images(b"junk", None, &limits()).is_err());
        let pdf = pdf_with_pages(&["a"]);
        assert!(remove_images(&pdf, Some("9"), &limits()).is_err());
    }
}
