//! PDF operations — synchronous functions over in-memory byte buffers.
//!
//! # Limit enforcement
//!
//! [`EngineLimits`](crate::crates::core::engine::EngineLimits) are enforced in exactly two places:
//!
//! 1. **Input side** — every op loads through [`load::load_pdf`], which
//!    rejects oversized inputs (`max_input_bytes`) and documents over
//!    `max_pages` before any work happens.
//! 2. **Output side** — operations that produce multiple artifacts
//!    (`split`) check `max_outputs` before producing them.
//!
//! `merge` additionally enforces `max_pages` against the *combined*
//! page count of all inputs.
//!
//! All ops return [`PdfError`] on malformed input and never panic;
//! the `lopdf` load is wrapped in `catch_unwind` for hostile PDFs.

mod extract;
mod info;
mod load;
mod merge;
mod pages;
mod rotate;
mod split;
mod text;
mod validate;

pub use extract::extract_pages;
pub use info::{PdfInfo, inspect};
pub use merge::merge;
pub use pages::parse_page_range;
pub use rotate::{RotateDegrees, rotate};
pub use split::split;
pub use text::{PageText, extract_text};
pub use validate::{ValidationReport, validate};

#[cfg(test)]
pub(crate) mod test_utils {
    //! Helpers that build tiny valid PDFs with `lopdf` itself — no
    //! fixture files needed.

    use lopdf::content::{Content, Operation};
    use lopdf::{Document, Object, Stream, dictionary};

    /// Build a PDF with one text page per entry in `page_texts`.
    /// Returns the serialized bytes.
    pub(crate) fn pdf_with_pages(page_texts: &[&str]) -> Vec<u8> {
        pdf_with_pages_and_title(page_texts, "KalaDoc test document")
    }

    /// Like [`pdf_with_pages`] but with a caller-chosen `/Title`.
    pub(crate) fn pdf_with_pages_and_title(page_texts: &[&str], title: &str) -> Vec<u8> {
        let mut doc = Document::with_version("1.5");

        let pages_id = doc.new_object_id();
        let font_id = doc.add_object(dictionary! {
            "Type" => "Font",
            "Subtype" => "Type1",
            "BaseFont" => "Courier",
        });
        let resources_id = doc.add_object(dictionary! {
            "Font" => dictionary! {
                "F1" => font_id,
            },
        });

        let mut kids = Vec::with_capacity(page_texts.len());
        for text in page_texts {
            let content = Content {
                operations: vec![
                    Operation::new("BT", vec![]),
                    Operation::new("Tf", vec!["F1".into(), 12.into()]),
                    Operation::new("Td", vec![100.into(), 700.into()]),
                    Operation::new("Tj", vec![Object::string_literal(*text)]),
                    Operation::new("ET", vec![]),
                ],
            };
            let content_id = doc.add_object(Stream::new(dictionary! {}, content.encode().unwrap()));
            let page_id = doc.add_object(dictionary! {
                "Type" => "Page",
                "Parent" => pages_id,
                "Contents" => content_id,
                "Resources" => resources_id,
                "MediaBox" => vec![0.into(), 0.into(), 595.into(), 842.into()],
            });
            kids.push(Object::Reference(page_id));
        }

        let pages = dictionary! {
            "Type" => "Pages",
            "Kids" => kids,
            "Count" => page_texts.len() as i64,
            "Resources" => resources_id,
            "MediaBox" => vec![0.into(), 0.into(), 595.into(), 842.into()],
        };
        doc.objects.insert(pages_id, Object::Dictionary(pages));

        let catalog_id = doc.add_object(dictionary! {
            "Type" => "Catalog",
            "Pages" => pages_id,
        });
        let info_id = doc.add_object(dictionary! {
            "Title" => Object::string_literal(title),
            "Author" => Object::string_literal("KalaDoc Tests"),
            "Creator" => Object::string_literal("kaladoc-test-utils"),
            "Producer" => Object::string_literal("lopdf"),
        });
        doc.trailer.set("Root", catalog_id);
        doc.trailer.set("Info", info_id);

        let mut buf = Vec::new();
        doc.save_to(&mut buf).unwrap();
        buf
    }

    /// Page count of a serialized PDF, for assertions.
    pub(crate) fn page_count(pdf: &[u8]) -> usize {
        Document::load_mem(pdf).unwrap().get_pages().len()
    }
}
