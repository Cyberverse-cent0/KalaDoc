//! Error type for the video engine.

/// Result alias for video engine operations.
pub type VideoResult<T> = Result<T, VideoError>;

/// Errors produced by [`VideoEngine`](crate::crates::video_engine::VideoEngine)
/// and the `ops` functions.
///
/// Every failure mode is mapped into a dedicated variant so callers can
/// distinguish "the required ffmpeg/ffprobe binary is absent" from
/// "the input is not usable media", "a resource limit was hit" and
/// "the external tool itself failed" without matching on strings.
#[derive(Debug, thiserror::Error)]
pub enum VideoError {
    /// A required external binary (`ffmpeg` or `ffprobe`) was not found
    /// on `PATH`. Operations that need it fail with this variant rather
    /// than faking a result.
    #[error("required tool not found: {tool}")]
    MissingTool {
        /// Name of the binary that could not be resolved.
        tool: String,
    },

    /// The input bytes are empty, malformed, or not a media container
    /// the tools can read.
    #[error("invalid input: {0}")]
    InvalidInput(String),

    /// A configured [`EngineLimits`](crate::crates::core::engine::EngineLimits)
    /// bound (or the engine's own hard input-size cap) was exceeded.
    /// The message names the limit and the observed value.
    #[error("limit exceeded: {0}")]
    LimitExceeded(String),

    /// `ffmpeg`/`ffprobe` ran but exited non-zero, timed out, or
    /// produced unusable output. `detail` carries the tail of the
    /// tool's stderr (trimmed) so callers can diagnose without log
    /// access.
    #[error("external tool {tool} failed: {detail}")]
    ExternalTool {
        /// Binary that failed (`"ffmpeg"` or `"ffprobe"`).
        tool: String,
        /// Trimmed stderr tail or failure description.
        detail: String,
    },

    /// Filesystem or subprocess I/O failed outside the tool itself
    /// (tempdir creation, reading back output bytes, ...).
    #[error(transparent)]
    Io(#[from] std::io::Error),

    /// `ffprobe` JSON output could not be parsed.
    #[error(transparent)]
    Json(#[from] serde_json::Error),
}
