//! Error type for the image engine.

/// Result alias for image engine operations.
pub type ImageResult<T> = Result<T, ImageError>;

/// Errors produced by [`ImageEngine`](crate::crates::image_engine::ImageEngine) and the `ops` functions.
///
/// `image`-crate failures are mapped into dedicated variants so callers
/// can distinguish "the input is not a usable image" from
/// resource-limit and invalid-argument failures without matching on
/// strings.
#[derive(Debug, thiserror::Error)]
pub enum ImageError {
    /// The image header or pixel data could not be decoded.
    ///
    /// The `Display` impl deliberately does not include the underlying
    /// decoder detail; the source is preserved for logs via
    /// [`std::error::Error::source`].
    #[error("invalid or unreadable image")]
    Decode(#[source] image::ImageError),

    /// The image was decoded but re-encoding it failed.
    #[error("image encoding failed")]
    Encode(#[source] image::ImageError),

    /// The input's format could not be sniffed, or the codec is not
    /// compiled into this engine.
    #[error("unsupported image format: {0}")]
    UnsupportedFormat(String),

    /// A non-format input parameter was invalid (empty resize box,
    /// unknown target format name, ...).
    #[error("invalid input: {0}")]
    InvalidInput(String),

    /// A configured [`EngineLimits`](crate::crates::core::engine::EngineLimits)
    /// bound was exceeded. The message names the limit and the observed
    /// value.
    #[error("resource limit exceeded: {0}")]
    LimitExceeded(String),

    /// Filesystem I/O outside the codec itself failed (temp files,
    /// external-tool scratch space).
    #[error("io error")]
    Io(#[source] std::io::Error),

    /// A required external tool (e.g. `tesseract`) is not on `PATH` or
    /// exited non-zero. `detail` says which case occurred.
    #[error("external tool `{tool}` failed: {detail}")]
    ExternalTool {
        /// Tool name.
        tool: String,
        /// Failure detail ("not found on PATH", exit status, ...).
        detail: String,
    },
}
