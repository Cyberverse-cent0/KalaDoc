//! Video engine — ffmpeg/ffprobe-backed media operations.
//!
//! This crate implements a
//! [`ProcessingEngine`](crate::crates::core::engine::ProcessingEngine)
//! for video/audio containers, backed by the external `ffmpeg` and
//! `ffprobe` binaries resolved on `PATH`. There is no in-process
//! codec — every operation shells out, operates on in-memory byte
//! buffers, and returns `Vec<u8>` (or a structured [`VideoInfo`]
//! report).
//!
//! # Supported containers
//!
//! Inputs and outputs cover the common ffmpeg container families:
//! `mp4`, `mkv`, `mov`, `webm`, `avi`. Anything ffprobe cannot read is
//! rejected as [`VideoError::InvalidInput`]; unsupported *target*
//! containers/extensions are rejected before ffmpeg runs.
//!
//! # Operations
//!
//! | Engine operation | Method | Notes |
//! |---|---|---|
//! | `inspect`  | [`VideoEngine::probe`] | [`VideoInfo`] via `ffprobe -show_format -show_streams` |
//! | `validate` | (same probe path) | non-media input fails `probe` with `InvalidInput` |
//! | `thumbnail`| [`VideoEngine::thumbnail`] | PNG frame at a timestamp (default ~10% of duration) |
//! | `trim`     | [`VideoEngine::trim`] | stream-copy by default; `reencode` for frame accuracy |
//! | `transcode`| [`VideoEngine::transcode`] | `mp4`→H.264/AAC, `webm`→VP9/Opus, `mkv`/`avi`→copy |
//! | `concat`   | [`VideoEngine::concat`] | concat demuxer; `-c copy` for same containers |
//! | `extract_audio` | [`VideoEngine::extract_audio`] | `mp3`/`wav`/`aac`/`m4a`/`ogg` |
//!
//! Unsupported operations (`merge`, `split`, `rotate`, `resize`,
//! `compress`, `watermark`, `ocr`, `encrypt`, `decrypt`, ...) are
//! **not** advertised in
//! [`EngineCapabilities`](crate::crates::core::engine::EngineCapabilities) and have no entry
//! points here — the engine never silently approximates unsupported
//! behavior.
//!
//! # Contracts
//!
//! - **Tools**: `ffmpeg` and `ffprobe` must be on `PATH`.
//!   [`VideoEngine::health`] reports `Healthy`/`Degraded`/`Unavailable`
//!   accordingly; operations needing a missing binary fail with
//!   [`VideoError::MissingTool`].
//! - **Temp files**: inputs are written to a unique scratch directory
//!   under the system temp dir and removed on every exit path.
//! - **Timeouts**: each tool invocation is bounded (10 min processing,
//!   60 s probing); expiry kills the child and reports
//!   [`VideoError::ExternalTool`].
//! - **Limits**: `EngineLimits::max_input_bytes`,
//!   `max_duration_secs` (probed duration) and `max_pixels` (thumbnail
//!   frame size) are enforced per call; a hard 32 GiB input cap always
//!   applies.
//! - **No panics**: malformed input returns [`VideoError`].

mod engine;
mod error;
mod model;
mod ops;
mod runtime;
#[cfg(test)]
mod tests;

pub use engine::VideoEngine;
pub use error::{VideoError, VideoResult};
pub use model::VideoInfo;
