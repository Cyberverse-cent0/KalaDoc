//! Shared request/response types for multimedia ops.
//!
//! Ops are synchronous, process-backed functions. Inputs arrive as
//! in-memory bytes (the engine contract is byte buffers); each op
//! writes them to a temp file because ffmpeg/ffprobe need seekable
//! paths, runs the tool, and reads the produced file(s) back under
//! [`MediaLimits::max_output_bytes`].
//!
//! Multi-input ops (concat, merge, comparisons) take `&[MediaInput]`
//! or `&[&[u8]]` per their own signatures — there is no single shared
//! input envelope.

use serde::{Deserialize, Serialize};

/// Per-operation resource limits for the multimedia engine.
///
/// `None` means unlimited for that bound. `timeout` always applies —
/// a hung ffmpeg must never block a worker forever.
#[derive(Debug, Clone)]
pub struct MediaLimits {
    /// Maximum input buffer size in bytes (per input file).
    pub max_input_bytes: Option<u64>,
    /// Maximum total bytes read back from produced output file(s).
    pub max_output_bytes: Option<u64>,
    /// Wall-clock timeout for a single tool invocation.
    pub timeout: std::time::Duration,
    /// Maximum number of output files an op may produce (frame
    /// extraction, HLS segments, ...).
    pub max_output_files: Option<u32>,
}

impl Default for MediaLimits {
    fn default() -> Self {
        Self {
            max_input_bytes: Some(8 * 1024 * 1024 * 1024), // 8 GiB
            max_output_bytes: Some(16 * 1024 * 1024 * 1024), // 16 GiB
            timeout: std::time::Duration::from_secs(30 * 60), // 30 min
            max_output_files: Some(10_000),
        }
    }
}

/// One produced file from a multimedia op.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct MediaFile {
    /// Suggested file name (extension-bearing), e.g. `"output.mp4"`.
    pub name: String,
    /// Raw file bytes.
    pub bytes: Vec<u8>,
    /// MIME type when known (e.g. `"video/mp4"`), else `None`.
    pub content_type: Option<String>,
}

/// Result of a multimedia op that may produce more than one file
/// (image sequences, HLS packages, extracted streams).
#[derive(Debug, Clone, Default)]
pub struct MediaOutput {
    /// The primary output — what a single-file caller wants.
    pub primary: Option<MediaFile>,
    /// Additional outputs (segments, sidecar subtitles, ...).
    pub extra: Vec<MediaFile>,
    /// Non-fatal diagnostics from the tool (stream copy notes,
    /// ignored-option warnings, ...).
    pub warnings: Vec<String>,
}

impl MediaOutput {
    /// Single-file convenience constructor.
    pub fn single(name: impl Into<String>, bytes: Vec<u8>) -> Self {
        Self {
            primary: Some(MediaFile {
                name: name.into(),
                bytes,
                content_type: None,
            }),
            extra: Vec::new(),
            warnings: Vec::new(),
        }
    }

    /// All files, primary first.
    pub fn files(&self) -> impl Iterator<Item = &MediaFile> {
        self.primary.iter().chain(self.extra.iter())
    }
}

/// A named byte input for multi-input ops.
#[derive(Debug, Clone)]
pub struct MediaInput {
    /// Display name used for temp-file extension and concat lists
    /// (e.g. `"clip1.mp4"`). The extension should match the real
    /// container so ffmpeg probes correctly.
    pub name: String,
    /// File bytes.
    pub data: Vec<u8>,
}
