//! Document inspection: metadata and page count.

use lopdf::{Document, decode_text_string};
use serde::{Deserialize, Serialize};

use crate::crates::core::engine::EngineLimits;

use crate::crates::pdf_engine::error::PdfResult;

use super::load::{doc_is_encrypted, load_pdf};

/// Structured summary of a PDF document, returned by
/// [`inspect`] and serializable for CLI/MCP output.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct PdfInfo {
    /// Number of pages in the document's page tree.
    pub page_count: u32,
    /// `/Title` from the document information dictionary, if present
    /// and decodable. `None` for encrypted documents (strings cannot be
    /// read without decrypting, which this engine does not do).
    pub title: Option<String>,
    /// `/Author` from the information dictionary.
    pub author: Option<String>,
    /// `/Creator` from the information dictionary.
    pub creator: Option<String>,
    /// `/Producer` from the information dictionary.
    pub producer: Option<String>,
    /// Whether the trailer carries an `/Encrypt` entry.
    pub encrypted: bool,
    /// Size of the input buffer in bytes.
    pub file_size_bytes: u64,
}

/// Inspect `input` and return its [`PdfInfo`].
///
/// Unlike the transforming ops, inspect tolerates encrypted documents so
/// callers can at least learn page count and encryption state; metadata
/// string fields are reported as `None` in that case.
pub fn inspect(input: &[u8], limits: &EngineLimits) -> PdfResult<PdfInfo> {
    let doc = load_pdf(input, limits)?;
    let encrypted = doc_is_encrypted(&doc);

    Ok(PdfInfo {
        page_count: doc.get_pages().len() as u32,
        title: info_string(&doc, b"Title", encrypted),
        author: info_string(&doc, b"Author", encrypted),
        creator: info_string(&doc, b"Creator", encrypted),
        producer: info_string(&doc, b"Producer", encrypted),
        encrypted,
        file_size_bytes: input.len() as u64,
    })
}

fn info_string(doc: &Document, key: &[u8], encrypted: bool) -> Option<String> {
    if encrypted {
        return None;
    }
    let info = doc.trailer.get(b"Info").ok()?;
    let (_, object) = doc.dereference(info).ok()?;
    let dict = object.as_dict().ok()?;
    let value = dict.get(key).ok()?;
    decode_text_string(value).ok()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::crates::core::engine::EngineLimits;
    use crate::crates::pdf_engine::error::PdfError;
    use crate::crates::pdf_engine::ops::encrypt::{PdfPermissions, encrypt};
    use crate::crates::pdf_engine::ops::test_utils::{
        page_count, pdf_with_pages, pdf_with_pages_and_title,
    };
    use lopdf::dictionary;

    #[test]
    fn reports_metadata_and_page_count() {
        let pdf = pdf_with_pages_and_title(&["one", "two", "three"], "My Doc Title");
        let info = inspect(&pdf, &EngineLimits::default()).unwrap();
        assert_eq!(info.page_count, 3);
        assert_eq!(page_count(&pdf), 3);
        assert_eq!(info.title.as_deref(), Some("My Doc Title"));
        assert_eq!(info.author.as_deref(), Some("KalaDoc Tests"));
        assert_eq!(info.creator.as_deref(), Some("kaladoc-test-utils"));
        assert_eq!(info.producer.as_deref(), Some("lopdf"));
        assert!(!info.encrypted);
        assert_eq!(info.file_size_bytes, pdf.len() as u64);
    }

    #[test]
    fn reports_missing_metadata_as_none() {
        // A bare document with no /Info dictionary.
        let mut doc = lopdf::Document::with_version("1.4");
        let pages_id = doc.new_object_id();
        doc.objects.insert(
            pages_id,
            lopdf::Object::Dictionary(dictionary! {
                "Type" => "Pages",
                "Kids" => Vec::<lopdf::Object>::new(),
                "Count" => 0,
            }),
        );
        let catalog_id = doc.add_object(dictionary! {
            "Type" => "Catalog",
            "Pages" => pages_id,
        });
        doc.trailer.set("Root", catalog_id);
        let mut pdf = Vec::new();
        doc.save_to(&mut pdf).unwrap();

        let info = inspect(&pdf, &EngineLimits::default()).unwrap();
        assert_eq!(info.page_count, 0);
        assert_eq!(info.title, None);
        assert_eq!(info.author, None);
        assert!(!info.encrypted);
    }

    #[test]
    fn tolerates_encrypted_input() {
        let plain = pdf_with_pages_and_title(&["secret"], "Hidden Title");
        let encrypted = encrypt(
            &plain,
            "pw",
            None,
            PdfPermissions::default(),
            &EngineLimits::default(),
        )
        .unwrap();
        let info = inspect(&encrypted, &EngineLimits::default()).unwrap();
        assert!(info.encrypted);
        // Metadata strings cannot be decoded without decrypting.
        assert_eq!(info.title, None);
        assert_eq!(info.author, None);
        assert_eq!(info.page_count, 1);
    }

    #[test]
    fn rejects_unloadable_input_and_enforces_limits() {
        assert!(inspect(b"junk", &EngineLimits::default()).is_err());

        let pdf = pdf_with_pages(&["one", "two"]);
        let limits = EngineLimits {
            max_pages: Some(1),
            ..EngineLimits::default()
        };
        let err = inspect(&pdf, &limits).unwrap_err();
        assert!(matches!(err, PdfError::LimitExceeded(_)), "got {err:?}");
    }
}
