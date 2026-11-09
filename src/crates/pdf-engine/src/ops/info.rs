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
