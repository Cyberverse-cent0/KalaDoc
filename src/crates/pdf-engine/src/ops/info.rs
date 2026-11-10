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
    /// Number of pages in the document's page tree. For a
    /// password-protected encrypted document the page tree cannot be
    /// loaded without the key, so this is a raw-structure estimate
    /// (see [`inspect`]).
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
/// string fields are reported as `None` in that case. A
/// password-protected document loads with only its trailer — lopdf
/// cannot parse ciphertext without the key — so `page_count` falls
/// back to a raw `/Type /Page` scan, an estimate that may undercount
/// pages stored inside object streams.
pub fn inspect(input: &[u8], limits: &EngineLimits) -> PdfResult<PdfInfo> {
    let doc = load_pdf(input, limits)?;
    let encrypted = doc_is_encrypted(&doc);

    // lopdf decrypts transparently when the user password is empty;
    // otherwise a password-protected document yields no page objects
    // at all, and the raw scan is the only count available.
    let page_count = match doc.get_pages().len() {
        0 if encrypted => raw_page_count(input),
        n => n as u32,
    };

    Ok(PdfInfo {
        page_count,
        title: info_string(&doc, b"Title", encrypted),
        author: info_string(&doc, b"Author", encrypted),
        creator: info_string(&doc, b"Creator", encrypted),
        producer: info_string(&doc, b"Producer", encrypted),
        encrypted,
        file_size_bytes: input.len() as u64,
    })
}

/// PDF whitespace bytes (white-space character set, PDF 32000-1 §7.2.2).
fn is_pdf_white(b: u8) -> bool {
    matches!(b, 0 | 9 | 10 | 12 | 13 | 32)
}

/// Bytes that may delimit a name token: whitespace plus the structural
/// delimiters. Used to prove `/Page` is a complete name (not the prefix
/// of `/Pages`, `/PageMode`, ...) and that a candidate `/Type` is a
/// complete key.
fn is_pdf_delimiter(b: u8) -> bool {
    is_pdf_white(b)
        || matches!(
            b,
            b'/' | b'<' | b'>' | b'[' | b']' | b'(' | b')' | b'{' | b'}'
        )
}

/// Best-effort page count for encrypted documents whose objects lopdf
/// could not load: count `/Type` → `/Page` name pairs in the raw bytes.
///
/// Encryption protects strings and streams, not dictionaries, so page
/// dictionaries remain visible as plaintext `<< /Type /Page ... >>`.
/// The `/Page` must be a complete name token (rejecting `/Pages` and
/// friends) and a `/Type` key must precede it with only whitespace in
/// between (`/Type/Page` adjacency is legal PDF). Pages packed inside
/// object streams are invisible to this scan and go uncounted — the
/// result is an estimate, which is acceptable because it is only used
/// for encrypted input whose `encrypted` flag warns the caller.
fn raw_page_count(input: &[u8]) -> u32 {
    let mut count = 0u32;
    let mut i = 0usize;
    while i + 5 <= input.len() {
        if &input[i..i + 5] == b"/Page" {
            let end_ok = i + 5 >= input.len() || is_pdf_delimiter(input[i + 5]);
            if end_ok {
                // Look back a bounded window for the nearest `/Type`.
                let window_start = i.saturating_sub(48);
                let mut j = i;
                let mut typed = false;
                while j > window_start {
                    j -= 1;
                    if j + 5 <= i
                        && &input[j..j + 5] == b"/Type"
                        && is_pdf_delimiter(input[j + 5])
                        && input[j + 5..i].iter().all(|&b| is_pdf_white(b))
                    {
                        typed = true;
                        break;
                    }
                }
                if typed {
                    count += 1;
                }
            }
        }
        i += 1;
    }
    count
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
