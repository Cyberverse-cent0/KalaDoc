//! Multimedia engine — audio, video, image-sequence and streaming-media
//! processing backed by managed `ffmpeg`/`ffprobe` processes.
//!
//! This crate implements a
//! [`ProcessingEngine`](crate::crates::core::engine::ProcessingEngine)
//! for multimedia content. All operations are synchronous, process-
//! backed functions over in-memory byte buffers: inputs are written to
//! a temp dir (ffmpeg needs seekable paths), the tool runs with an
//! argv vector under [`MediaLimits`](model::MediaLimits), and outputs
//! are read back under byte/file-count caps.
//!
//! # Contracts
//!
//! - **Honest capabilities**: ops must check the runtime capability
//!   registry ([`runtime::discovery`]) before advertising or invoking
//!   a codec/filter — FFmpeg features vary by build. Never silently
//!   fall back to a different codec; return
//!   [`MediaError::Unsupported`] instead.
//! - **Limits**: `max_input_bytes` bounds each input buffer;
//!   `max_output_bytes` bounds bytes read back; `timeout` kills hung
//!   tools; `max_output_files` caps produced files.
//! - **No panics**: malformed input or tool failure returns
//!   [`MediaError`].
//! - **Input immutability**: inputs are only ever read.
//! - **Output naming**: primary output names carry the real container
//!   extension (`"output.mp4"`) so downstream sniffing works.

pub mod analysis;
pub mod audio;
pub mod container;
pub mod conversion;
pub mod encoding;
pub mod engine;
pub mod error;
pub mod extraction;
pub mod filters;
pub mod frames;
pub mod metadata;
pub mod model;
pub mod packaging;
pub mod probe;
pub mod runtime;
pub mod subtitles;
pub mod video;

pub use engine::MediaEngine;
pub use error::{MediaError, MediaResult};
pub use model::{MediaFile, MediaInput, MediaLimits, MediaOutput};
