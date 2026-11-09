//! Best-effort text extraction.
//!
//! `lopdf`'s extractor handles the common case — simple content streams
//! with standard encodings — but does not perform font subsetting
//! analysis, ToUnicode CMap reconstruction for every font type, or OCR.
//! Text from complex PDFs (custom encodings, Type0 fonts without usable
//! ToUnicode maps, scanned pages) may be partial or empty. This is
//! reported honestly: extraction never fails silently, per-page errors
//! surface as [`PdfError`]s, and empty results are legitimate output.

use serde::{Deserialize, Serialize};

use crate::crates::core::engine::EngineLimits;

use crate::crates::pdf_engine::error::PdfResult;

use super::load::{load_pdf, lopdf_err, reject_encrypted};
use super::pages::parse_page_range;

/// Per-page extracted text.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct PageText {
    /// 1-based page number, matching the CLI/MCP contract.
    pub page: u32,
    /// Extracted text for this page. May be empty for pages without
    /// extractable text (images, unsupported encodings).
    pub text: String,
}

/// Decompression bound handed to `lopdf` per page — its own default, so
/// behavior matches a plain `extract_text` call.
use lopdf::DEFAULT_MAX_DECOMPRESSED_SIZE as MAX_PAGE_CONTENT_BYTES;

/// Extract text from `input`, one [`PageText`] per selected page.
///
/// `pages` is an optional range spec (`"1-3,8"`, `"all"`); `None`
/// extracts every page. Pages are returned in ascending order.
/// Encrypted documents are rejected — their strings cannot be read.
pub fn extract_text(
    input: &[u8],
    pages: Option<&str>,
    limits: &EngineLimits,
) -> PdfResult<Vec<PageText>> {
    let doc = load_pdf(input, limits)?;
    reject_encrypted(&doc)?;

    let page_count = doc.get_pages().len() as u32;
    let selected = match pages {
        Some(spec) => parse_page_range(spec, page_count)?,
        None => (1..=page_count).collect(),
    };

    let mut out = Vec::with_capacity(selected.len());
    for page_number in selected {
        let text = doc
            .extract_text_with_limit(&[page_number], MAX_PAGE_CONTENT_BYTES)
            .map_err(lopdf_err)?;
        out.push(PageText {
            page: page_number,
            text,
        });
    }
    Ok(out)
}
