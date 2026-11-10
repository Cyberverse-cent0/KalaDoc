//! error type for the security engine.
//!
//! All fallible security-engine operations return
//! [`SecurityResult`]. Variants carry the numbers or categories a caller
//! needs to log or map to a user-facing message — and never embed the
//! raw, untrusted user filename (see
//! [`crate::crates::security_engine::filename`]).

use super::sniff::SniffedFormat;

/// Result alias for security-engine operations.
pub type SecurityResult<T> = Result<T, SecurityError>;

/// Failures produced while validating uploads and enforcing resource
/// limits.
///
/// `InvalidFilename` carries a static `reason` describing *which* rule
/// was violated; the offending name is deliberately not echoed because
/// it is untrusted input that may itself be hostile (control chars,
/// misleading unicode, etc.).
#[derive(Debug, thiserror::Error)]
pub enum SecurityError {
    /// The supplied filename failed
    /// [`sanitize_filename`](crate::crates::security_engine::sanitize_filename).
    #[error("invalid filename: {reason}")]
    InvalidFilename {
        /// Static description of the violated rule.
        reason: &'static str,
    },

    /// The upload contained no bytes.
    #[error("empty file: upload has no content")]
    EmptyFile,

    /// Declared size and actual content length disagree.
    #[error("size mismatch: declared {declared} bytes but content is {actual} bytes")]
    SizeMismatch {
        /// Size the caller declared.
        declared: u64,
        /// Actual length of the supplied content.
        actual: u64,
    },

    /// Content exceeded a byte limit.
    #[error("file too large: {size} bytes exceeds limit of {max} bytes")]
    FileTooLarge {
        /// Observed size in bytes.
        size: u64,
        /// Configured maximum in bytes.
        max: u64,
    },

    /// An operation's intermediate/output budget was exceeded.
    #[error("operation too large: {size} bytes exceeds limit of {max} bytes")]
    OperationTooLarge {
        /// Observed size in bytes.
        size: u64,
        /// Configured maximum in bytes.
        max: u64,
    },

    /// A document exceeded the configured page limit.
    #[error("too many pages: {pages} exceeds limit of {max}")]
    TooManyPages {
        /// Observed page count.
        pages: u32,
        /// Configured maximum.
        max: u32,
    },

    /// A raster image exceeded the configured pixel-count limit —
    /// the decompression-bomb guard.
    #[error("image too large: {pixels} pixels exceeds limit of {max}")]
    TooManyPixels {
        /// Observed width × height pixel count.
        pixels: u64,
        /// Configured maximum.
        max: u64,
    },

    /// An operation would produce more outputs than allowed.
    #[error("too many outputs: {count} exceeds limit of {max}")]
    TooManyOutputs {
        /// Requested output count.
        count: u32,
        /// Configured maximum.
        max: u32,
    },

    /// Sniffed content type contradicts the expected/declared format.
    #[error("content type mismatch: expected {expected}, found {found}")]
    ContentTypeMismatch {
        /// The format the caller required.
        expected: SniffedFormat,
        /// The format detected from magic bytes.
        found: SniffedFormat,
    },

    /// No `%PDF-` header found within the PDF spec's leading-junk window.
    /// Distinct from [`SecurityError::ContentTypeMismatch`]: the input
    /// may contain a PDF header beyond offset 0, which offset-0 sniffing
    /// cannot see.
    #[error("input is not a PDF document")]
    NotPdf,

    /// The bytes carry a PDF header but `lopdf` could not parse the
    /// structure — malformed xref, truncated objects, bad syntax.
    #[error("malformed PDF: {reason}")]
    Malformed {
        /// Parser-reported reason.
        reason: String,
    },

    /// The document declares encryption in its trailer. Rejected by
    /// policy: a validator that cannot see inside an encrypted container
    /// cannot vouch for it, and downstream engines would fail anyway
    /// without a password.
    #[error("PDF is encrypted")]
    Encrypted,
}
