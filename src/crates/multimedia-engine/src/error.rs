//! Error type for the multimedia engine.

/// Result alias for multimedia engine operations.
pub type MediaResult<T> = Result<T, MediaError>;

/// Errors produced by [`MediaEngine`](crate::crates::multimedia_engine::MediaEngine)
/// and the `ops` functions.
///
/// FFmpeg/ffprobe process failures are mapped into dedicated variants so
/// callers can distinguish "the tool is missing", "the media is
/// unreadable", "the requested codec/filter is unavailable in this
/// build", and resource-limit failures without matching on strings.
#[derive(Debug, thiserror::Error)]
pub enum MediaError {
    /// `ffmpeg` or `ffprobe` was not found on `PATH` (or the configured
    /// binary path does not exist).
    #[error("required multimedia tool not found: {0}")]
    MissingTool(String),

    /// A tool ran but exited non-zero. `stderr` holds the tool's own
    /// diagnostic tail — FFmpeg reports codec/filter/format problems
    /// there.
    #[error("{tool} failed ({status}): {detail}")]
    ToolFailed {
        /// Tool name (`"ffmpeg"`, `"ffprobe"`, ...).
        tool: &'static str,
        /// Process exit description.
        status: String,
        /// Trimmed stderr/stdout diagnostic.
        detail: String,
    },

    /// The tool exceeded its timeout and was killed.
    #[error("{tool} timed out after {timeout:?}")]
    Timeout {
        /// Tool name.
        tool: &'static str,
        /// Configured timeout.
        timeout: std::time::Duration,
    },

    /// The input could not be probed or is not a recognized media file.
    #[error("unreadable or unsupported media input: {0}")]
    InvalidInput(String),

    /// A requested codec, muxer, demuxer, filter or protocol is not
    /// compiled into the detected FFmpeg build. Never silently fall
    /// back to a different codec — callers must know.
    #[error("unsupported by this FFmpeg build: {0}")]
    Unsupported(String),

    /// A configured [`MediaLimits`](crate::crates::multimedia_engine::MediaLimits)
    /// bound was exceeded. The message names the limit and the value.
    #[error("resource limit exceeded: {0}")]
    LimitExceeded(String),

    /// The one-time capability probe (`-encoders`, `-filters`, ...) failed.
    #[error("capability probe failed: {0}")]
    Probe(String),

    /// Filesystem or process-spawn failure.
    #[error("io error: {0}")]
    Io(#[from] std::io::Error),
}
