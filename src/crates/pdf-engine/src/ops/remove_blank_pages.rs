//! Remove blank pages from a PDF document.
//!
//! A page is considered blank when its content — the concatenation of
//! all its content streams, decoded — contains no operations other
//! than whitespace and graphics-state operators (`q`, `Q`, `w`, `J`,
//! `j`, `M`, `d`, `ri`, `i`, `gs`), or when it has no content at all.
//! A page with *no* content stream is still kept when it carries
//! annotations, since annotations are page-visible content the stream
//! does not describe.
//!
//! Detection is conservative: undecodable content, missing dictionaries
//! or unexpected structures all count as "not blank", so the op never
//! deletes a page it cannot prove empty.
//!
//! Like `remove_pages`, the document must keep at least one page — if
//! every page is blank the op fails with [`PdfError::InvalidInput`]
//! rather than emitting a page-less PDF.

use lopdf::{Document, Object, ObjectId};

use crate::crates::core::engine::EngineLimits;

use crate::crates::pdf_engine::error::{PdfError, PdfResult};

use super::load::{load_pdf, reject_encrypted, save_pdf};

/// Upper bound for decompressing a single page's content during blank
/// detection — same 64 MiB budget `validate` uses per page.
const MAX_CONTENT_BYTES: usize = 64 * 1024 * 1024;

/// Content-stream operators that cannot paint anything: graphics-state
/// and graphics-state-save/restore only. Anything else (path, text,
/// image `Do`, shading, marked content, ...) makes the page non-blank.
const BLANK_OPERATORS: &[&str] = &["q", "Q", "w", "J", "j", "M", "d", "ri", "i", "gs"];

/// Result of [`remove_blank_pages`]: the rewritten document plus a
/// per-page account of what was kept and removed.
#[derive(Debug)]
pub struct BlankReport {
    /// The output PDF bytes (all non-blank pages, original order).
    pub pdf: Vec<u8>,
    /// 1-based page numbers that were removed, ascending.
    pub removed: Vec<u32>,
    /// 1-based page numbers that were kept, ascending (pre-removal
    /// numbering).
    pub kept: Vec<u32>,
    /// Page count of the input document.
    pub total_before: u32,
    /// Page count of the output document.
    pub total_after: u32,
}

/// Remove every provably blank page from `input`.
///
/// See the module docs for the blank-page definition. Fails with
/// [`PdfError::InvalidInput`] when all pages are blank (the document
/// must keep at least one page).
pub fn remove_blank_pages(input: &[u8], limits: &EngineLimits) -> PdfResult<BlankReport> {
    let mut doc = load_pdf(input, limits)?;
    reject_encrypted(&doc)?;

    let pages = doc.get_pages();
    let total_before = pages.len() as u32;
    if total_before == 0 {
        return Err(PdfError::InvalidInput("document has no pages".to_string()));
    }

    let mut removed = Vec::new();
    let mut kept = Vec::new();
    for (num, page_id) in &pages {
        if is_blank_page(&doc, *page_id) {
            removed.push(*num);
        } else {
            kept.push(*num);
        }
    }

    if removed.is_empty() {
        // Nothing to do — still return a valid re-serialized document
        // so callers get a uniform result shape.
        let pdf = save_pdf(&mut doc)?;
        return Ok(BlankReport {
            pdf,
            removed,
            kept,
            total_before,
            total_after: total_before,
        });
    }
    if kept.is_empty() {
        return Err(PdfError::InvalidInput(
            "every page is blank; the document must keep at least one page".to_string(),
        ));
    }

    doc.delete_pages(&removed);
    let pdf = save_pdf(&mut doc)?;
    let total_after = kept.len() as u32;
    Ok(BlankReport {
        pdf,
        removed,
        kept,
        total_before,
        total_after,
    })
}

/// Whether `page_id` is provably blank. Any doubt resolves to `false`.
pub(crate) fn is_blank_page(doc: &Document, page_id: ObjectId) -> bool {
    if has_annotations(doc, page_id) {
        return false;
    }
    blank_content(doc, page_id)
}

/// A page with annotations is never blank — annotations are visible
/// page content outside the content stream.
fn has_annotations(doc: &Document, page_id: ObjectId) -> bool {
    let Ok(dict) = doc.get_dictionary(page_id) else {
        return true; // cannot prove blank
    };
    match dict.get(b"Annots") {
        Ok(Object::Array(items)) => !items.is_empty(),
        Ok(Object::Reference(id)) => matches!(
            doc.get_object(*id).and_then(Object::as_array),
            Ok(items) if !items.is_empty()
        ),
        // Missing or malformed Annots: not evidence of content.
        _ => false,
    }
}

/// Whether the page's content streams contain only no-op operators.
fn blank_content(doc: &Document, page_id: ObjectId) -> bool {
    if doc.get_page_contents(page_id).is_empty() {
        // No content streams at all: blank unless annotations (checked
        // by the caller) say otherwise.
        return true;
    }
    let Ok(raw) = doc.get_page_content_with_limit(page_id, MAX_CONTENT_BYTES) else {
        return false; // cannot decode within budget — assume content
    };
    let Ok(content) = lopdf::content::Content::decode(&raw) else {
        return false; // unparseable content — assume it paints
    };
    content
        .operations
        .iter()
        .all(|op| BLANK_OPERATORS.contains(&op.operator.as_str()))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::crates::pdf_engine::ops::test_utils::{
        page_count as test_page_count, pdf_with_pages,
    };
    use lopdf::content::{Content, Operation};
    use lopdf::{Document, Stream, dictionary};

    /// Build a PDF whose pages have the given raw content streams
    /// (`None` = no /Contents entry at all).
    fn pdf_with_raw_pages(contents: &[Option<&[u8]>]) -> Vec<u8> {
        let mut doc = Document::with_version("1.5");
        let pages_id = doc.new_object_id();
        let mut kids = Vec::new();
        for content in contents {
            let mut dict = dictionary! {
                "Type" => "Page",
                "Parent" => pages_id,
                "MediaBox" => vec![0.into(), 0.into(), 595.into(), 842.into()],
            };
            if let Some(bytes) = content {
                let stream_id = doc.add_object(Stream::new(dictionary! {}, bytes.to_vec()));
                dict.set("Contents", stream_id);
            }
            let page_id = doc.add_object(dict);
            kids.push(Object::Reference(page_id));
        }
        doc.objects.insert(
            pages_id,
            Object::Dictionary(dictionary! {
                "Type" => "Pages",
                "Kids" => kids,
                "Count" => contents.len() as i64,
            }),
        );
        let catalog_id = doc.add_object(dictionary! {
            "Type" => "Catalog",
            "Pages" => pages_id,
        });
        doc.trailer.set("Root", catalog_id);
        let mut buf = Vec::new();
        doc.save_to(&mut buf).unwrap();
        buf
    }

    fn blank_stream() -> Vec<u8> {
        Content {
            operations: vec![Operation::new("q", vec![]), Operation::new("Q", vec![])],
        }
        .encode()
        .unwrap()
    }

    #[test]
    fn removes_only_blank_pages() {
        // pdf_with_pages gives every page text; mix in a raw PDF is
        // not possible, so build a doc: text page via helper is not
        // mixable — instead use raw pages: one with text ops, one
        // blank, one empty.
        let text = Content {
            operations: vec![
                Operation::new("BT", vec![]),
                Operation::new("Tj", vec![Object::string_literal("hi")]),
                Operation::new("ET", vec![]),
            ],
        }
        .encode()
        .unwrap();
        let blank = blank_stream();
        let pdf = pdf_with_raw_pages(&[Some(&text), Some(&blank), None, Some(&text)]);
        let report = remove_blank_pages(&pdf, &EngineLimits::default()).unwrap();
        assert_eq!(report.total_before, 4);
        assert_eq!(report.total_after, 2);
        assert_eq!(report.removed, vec![2, 3]);
        assert_eq!(report.kept, vec![1, 4]);
        assert_eq!(test_page_count(&report.pdf), 2);
    }

    #[test]
    fn keeps_pages_with_annotations() {
        // A page with no content but an /Annots entry must be kept.
        let mut doc = Document::with_version("1.5");
        let pages_id = doc.new_object_id();
        let annot_id = doc.add_object(dictionary! {
            "Type" => "Annot",
            "Subtype" => "Text",
            "Rect" => vec![0.into(), 0.into(), 10.into(), 10.into()],
        });
        let page_id = doc.add_object(dictionary! {
            "Type" => "Page",
            "Parent" => pages_id,
            "MediaBox" => vec![0.into(), 0.into(), 595.into(), 842.into()],
            "Annots" => vec![Object::Reference(annot_id)],
        });
        doc.objects.insert(
            pages_id,
            Object::Dictionary(dictionary! {
                "Type" => "Pages",
                "Kids" => vec![Object::Reference(page_id)],
                "Count" => 1,
            }),
        );
        let catalog_id = doc.add_object(dictionary! {
            "Type" => "Catalog",
            "Pages" => pages_id,
        });
        doc.trailer.set("Root", catalog_id);
        let mut pdf = Vec::new();
        doc.save_to(&mut pdf).unwrap();

        // Single annotated page: nothing is blank-removable, and even
        // if it were, removing all pages is rejected.
        let report = remove_blank_pages(&pdf, &EngineLimits::default()).unwrap();
        assert!(report.removed.is_empty());
        assert_eq!(report.total_after, 1);
    }

    #[test]
    fn rejects_when_all_pages_blank() {
        let blank = blank_stream();
        let pdf = pdf_with_raw_pages(&[Some(&blank), None]);
        let err = remove_blank_pages(&pdf, &EngineLimits::default()).unwrap_err();
        assert!(matches!(err, PdfError::InvalidInput(_)), "got {err:?}");
    }

    #[test]
    fn text_pages_are_never_blank() {
        let pdf = pdf_with_pages(&["one", "two"]);
        let report = remove_blank_pages(&pdf, &EngineLimits::default()).unwrap();
        assert!(report.removed.is_empty());
        assert_eq!(report.total_after, 2);
    }

    #[test]
    fn rejects_bad_input() {
        assert!(remove_blank_pages(b"junk", &EngineLimits::default()).is_err());
    }
}
