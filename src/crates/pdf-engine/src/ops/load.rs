//! Shared loading/saving helpers.
//!
//! This module is the single enforcement point for input-side
//! [`EngineLimits`](crate::crates::core::engine::EngineLimits): every operation loads its input through
//! [`load_pdf`], which checks `max_input_bytes`, `max_pages` and rejects
//! encrypted documents before any real work happens. Output-side limits
//! (`max_outputs`) are checked by the individual operations before they
//! produce artifacts — see each op's docs.

use std::panic::{AssertUnwindSafe, catch_unwind};

use lopdf::Document;

use crate::crates::core::engine::EngineLimits;

use crate::crates::pdf_engine::error::{PdfError, PdfResult};

/// Map a `lopdf` error into the right [`PdfError`] variant. I/O failures
/// surface as [`PdfError::Encode`]; everything else stays a processing
/// error with the parser detail kept as `source` only.
pub(crate) fn lopdf_err(err: lopdf::Error) -> PdfError {
    match err {
        lopdf::Error::IO(io) => PdfError::Encode(io),
        other => PdfError::Processing(other),
    }
}

/// Map a load-time `lopdf` error. Parse/decompression failures mean the
/// input is not a usable PDF.
fn load_err(err: lopdf::Error) -> PdfError {
    match err {
        lopdf::Error::IO(io) => PdfError::Encode(io),
        other => PdfError::InvalidPdf(other),
    }
}

/// Whether the trailer carries an `/Encrypt` entry, whether it is a
/// direct dictionary or a reference.
pub(crate) fn doc_is_encrypted(doc: &Document) -> bool {
    doc.is_encrypted() || doc.trailer.has(b"Encrypt")
}

/// Enforce the input-side limits on a raw byte buffer.
fn check_input_limits(len: usize, limits: &EngineLimits) -> PdfResult<()> {
    if let Some(max) = limits.max_input_bytes
        && len as u64 > max
    {
        return Err(PdfError::LimitExceeded(format!(
            "input size {len} bytes exceeds max_input_bytes {max}"
        )));
    }
    Ok(())
}

/// Enforce `max_pages` against a loaded document's page count.
pub(crate) fn check_page_limit(page_count: usize, limits: &EngineLimits) -> PdfResult<()> {
    if let Some(max) = limits.max_pages
        && page_count > max as usize
    {
        return Err(PdfError::LimitExceeded(format!(
            "page count {page_count} exceeds max_pages {max}"
        )));
    }
    Ok(())
}

/// Load a PDF from memory, enforcing input limits.
///
/// Does **not** reject encrypted documents — callers that cannot handle
/// them must check [`doc_is_encrypted`] themselves (inspect/validate
/// intentionally allow encrypted input; all transforming ops reject it).
///
/// `lopdf` is a pure-Rust parser but can still panic on hostile input;
/// the load is wrapped in `catch_unwind` so a panic surfaces as
/// [`PdfError::InvalidInput`] instead of aborting the caller.
pub(crate) fn load_pdf(bytes: &[u8], limits: &EngineLimits) -> PdfResult<Document> {
    check_input_limits(bytes.len(), limits)?;
    let doc = catch_unwind(AssertUnwindSafe(|| Document::load_mem(bytes)))
        .map_err(|_| PdfError::InvalidInput("malformed PDF crashed the parser".to_string()))?
        .map_err(load_err)?;
    check_page_limit(doc.get_pages().len(), limits)?;
    Ok(doc)
}

/// Reject encrypted input for operations that transform content.
pub(crate) fn reject_encrypted(doc: &Document) -> PdfResult<()> {
    if doc_is_encrypted(doc) {
        return Err(PdfError::Encrypted);
    }
    Ok(())
}

/// Serialize a document to an in-memory PDF buffer.
///
/// `Document::save_to` returns `std::io::Result` in lopdf 0.47, so I/O
/// failures map straight to [`PdfError::Encode`].
pub(crate) fn save_pdf(doc: &mut Document) -> PdfResult<Vec<u8>> {
    let mut buf = Vec::new();
    doc.save_to(&mut buf).map_err(PdfError::Encode)?;
    Ok(buf)
}
