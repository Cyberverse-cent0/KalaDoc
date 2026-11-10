//! FFmpeg runtime discovery: tool paths, versions, and the full
//! capability surface of the installed ffmpeg/ffprobe builds.
//!
//! Everything here shells out read-only (`-version`, `-encoders`,
//! `-filters`, ...) through the shared [`crate::crates::video_engine::ops`]
//! helpers — no media is processed here.

pub mod capabilities;
pub(crate) mod discovery;
pub(crate) mod hardware;
pub(crate) mod version;

pub use capabilities::{MediaCapabilities, capabilities};

pub(crate) use version::{ffmpeg_version, ffprobe_version};
