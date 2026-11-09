//! Error type for the conversion engine.
//!
//! All variants are safe to surface to users and API consumers:
//! messages describe *what* failed and *which* backend, never raw
//! document content or absolute server paths.

use thiserror::Error;

use super::model::Format;

/// Result alias for conversion operations.
pub type ConvertResult<T> = Result<T, ConvertError>;

/// Errors produced by [`ConversionEngine`](crate::crates::conversion_engine::ConversionEngine)
/// and its [`ConversionBackend`](crate::crates::conversion_engine::ConversionBackend)s.
#[derive(Debug, Error)]
pub enum ConvertError {
    /// No backend can perform the requested `from` → `to` conversion.
    #[error("conversion from {} to {} is not supported", from.as_str(), to.as_str())]
    UnsupportedConversion { from: Format, to: Format },

    /// Backends that support the pair exist but none can run in this
    /// environment. `reason` must be user-safe (no paths, no traces).
    #[error("conversion backend {backend} is unavailable: {reason}")]
    BackendUnavailable { backend: String, reason: String },

    /// A backend attempted the conversion and failed. `message` is a
    /// short, sanitized description — never document content.
    #[error("conversion backend {backend} failed: {message}")]
    BackendFailed { backend: String, message: String },

    /// The request itself is malformed (empty input, bad parameters).
    #[error("invalid conversion input: {reason}")]
    InvalidInput { reason: String },

    /// Input exceeds the engine's size cap. `limit` is a human-readable
    /// description (e.g. `"256 MiB"`); `actual` is the byte count.
    #[error("input too large: limit {limit}, got {actual} bytes")]
    TooLarge { limit: String, actual: u64 },

    /// A backend exceeded its time budget.
    #[error("conversion backend {backend} timed out after {seconds}s")]
    Timeout { backend: String, seconds: u64 },

    /// Underlying I/O failure (spool files, pipes, ...). The source
    /// error's Display is propagated; backends must sanitize paths
    /// before wrapping into this variant where they surface them.
    #[error("conversion I/O error: {0}")]
    Io(#[from] std::io::Error),
}
