//! Error type for the PDF engine.

use std::io;

/// Result alias for PDF engine operations.
pub type PdfResult<T> = Result<T, PdfError>;

/// Errors produced by [`PdfEngine`](crate::crates::pdf_engine::PdfEngine) and the `ops` functions.
///
/// `lopdf` failures are mapped into dedicated variants so callers can
/// distinguish "the input is not a usable PDF" from resource-limit and
/// unsupported-operation failures without matching on strings.
#[derive(Debug, thiserror::Error)]
pub enum PdfError {
    /// The input could not be parsed as a PDF document.
    ///
    /// The `Display` impl deliberately does not include the underlying
    /// parser detail; the source is preserved for logs via
    /// [`std::error::Error::source`].
    #[error("invalid or unreadable PDF document")]
    InvalidPdf(#[source] lopdf::Error),

    /// A lopdf operation failed after the document was loaded
    /// (e.g. while rewriting or saving it).
    #[error("PDF processing failed")]
    Processing(#[source] lopdf::Error),

    /// Serializing the output document failed.
    #[error("failed to encode PDF output")]
    Encode(#[source] io::Error),

    /// The page range specifier was malformed or selected pages that do
    /// not exist. The message always states the expected form
    /// (`1-3,8,10-12`, 1-based) so callers can fix the input.
    #[error("invalid page range or selection: {0}")]
    InvalidPageRange(String),

    /// A non-page input parameter was invalid (empty merge input list,
    /// zero pages-per-file, ...).
    #[error("invalid input: {0}")]
    InvalidInput(String),

    /// A configured [`EngineLimits`](crate::crates::core::engine::EngineLimits)(crate::crates::core::engine::EngineLimits)
    /// bound was exceeded. The message names the limit and the observed
    /// value.
    #[error("resource limit exceeded: {0}")]
    LimitExceeded(String),

    /// The engine does not support the requested behavior at all —
    /// reported instead of silently approximating.
    #[error("unsupported operation: {0}")]
    UnsupportedOperation(String),

    /// The document is encrypted. This engine never attempts
    /// decryption; callers must supply an already-decrypted PDF.
    #[error("encrypted PDF documents are not supported")]
    Encrypted,
}
