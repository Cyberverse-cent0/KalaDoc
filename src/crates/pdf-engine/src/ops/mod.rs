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

mod booklet;
mod compare;
mod compress;
mod convert;
mod crop;
mod decrypt;
mod draw_text;
mod duplicate_pages;
mod edit_text;
mod encrypt;
mod extract;
mod extract_images;
mod extract_metadata;
mod flatten;
mod garbage_collect;
mod grayscale;
mod headers_footers;
mod info;
mod insert_blank;
mod interleave;
mod invert_colors;
mod linearize;
mod load;
mod merge;
mod nup;
mod ocr;
mod outline;
mod overlay;
mod page_count;
mod page_labels;
mod pages;
mod pdfa;
mod poster;
pub(crate) mod raster;
mod redact;
mod remove_blank_pages;
mod remove_images;
mod remove_pages;
mod render;
mod reorder;
mod repair;
mod replace_image;
mod resize;
mod rotate;
mod sanitize;
mod set_metadata;
mod sign;
mod single_page;
mod split;
mod stamp;
mod text;
mod text_markup;
mod validate;
mod verify_signature;
mod watermark;

pub use booklet::{BookletMode, BookletSize, BookletSpec, booklet};
pub use compare::{CompareMode, CompareReport, DiffEntry, DiffKind, compare};
pub use compress::compress;
pub use convert::{ConvertTarget, convert};
pub use crop::crop;
pub use decrypt::decrypt;
pub use draw_text::{DrawTextSpec, draw_text};
pub use encrypt::{PdfPermissions, encrypt};
pub use extract::extract_pages;
pub use extract_images::{EmbeddedImage, extract_images};
pub use extract_metadata::{PdfDate, PdfMetadata, XmpMetadata, extract_metadata};
pub use flatten::flatten;
pub use garbage_collect::{GcReport, garbage_collect};
pub use headers_footers::{HeaderFooterSpec, PageNumberStyle, add_page_numbers, headers_footers};
pub use info::{PdfInfo, inspect};
pub use linearize::{LinearizeReport, linearize};
pub use merge::merge;
pub use nup::nup;
pub use ocr::{ocr, ocr_languages};
pub use overlay::overlay;
pub use page_count::page_count;
pub use page_labels::{clear_page_labels, get_page_labels, set_page_labels};
pub use pages::parse_page_range;
pub use pdfa::{PdfaLevel, PdfaReport, to_pdfa};
pub use poster::poster;
pub use raster::{PageImage, PageImageReport};
pub use redact::redact;
pub use remove_images::{RemoveReport, remove_images};
pub use remove_pages::remove_pages;
pub use render::{RenderFormat, render};
pub use reorder::reorder;
pub use repair::{RepairReport, repair};
pub use replace_image::{ImageInputFormat, replace_image};
pub use resize::{ResizeSpec, resize};
pub use rotate::{RotateDegrees, rotate};
pub use sanitize::sanitize;
pub use sign::{SignCredentials, SignOptions, remove_signatures, sign};
pub use single_page::single_page;
pub use split::split;
pub use stamp::{StampOptions, stamp};
pub use text::{PageText, extract_text};
pub use validate::{ValidationReport, validate};
pub use verify_signature::{
    CryptoSignatureInfo, CryptoSignatureReport, SignatureInfo, SignatureReport, verify_signatures,
    verify_signatures_crypto,
};
pub use watermark::{MarkPosition, WatermarkOptions, watermark};

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
