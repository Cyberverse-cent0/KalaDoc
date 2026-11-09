//! [`VideoEngine`] — the `ProcessingEngine` implementation backed by
//! the external `ffmpeg`/`ffprobe` binaries.
//!
//! All operations are synchronous, subprocess-bound functions over
//! in-memory byte buffers (see the `ops` module). They are exposed as
//! plain methods rather than futures: callers running them on an async
//! runtime should wrap them in `spawn_blocking` themselves. The engine
//! holds no state — every call receives its own
//! [`EngineLimits`](crate::crates::core::engine::EngineLimits) — so it
//! is trivially `Send + Sync`.
//!
//! # External tools
//!
//! [`ProcessingEngine::health`] probes `PATH` for `ffmpeg` and
//! `ffprobe`: both present → [`EngineHealthStatus::Healthy`], exactly
//! one → `Degraded`, neither → `Unavailable`. Operations that need a
//! missing binary fail with [`VideoError::MissingTool`] — results are
//! never approximated.

use crate::crates::core::engine::{
    EngineCapabilities, EngineFuture, EngineHealth, EngineHealthStatus, EngineKind, EngineLimits,
    EngineOperation, ProcessingEngine,
};

use crate::crates::video_engine::error::VideoResult;
use crate::crates::video_engine::model::VideoInfo;
use crate::crates::video_engine::ops;
use crate::crates::video_engine::ops::{FFMPEG, FFPROBE, tool_available};

/// Media containers this engine accepts as input and can produce as
/// output (the transcode/trim/concat target set).
const MEDIA_FORMATS: &[&str] = &["mp4", "mkv", "mov", "webm", "avi"];

/// The ffmpeg-backed video engine.
///
/// Stateless: construct with [`VideoEngine::new`] (or
/// [`Default`]) and pass per-call [`EngineLimits`] to each operation.
#[derive(Debug, Clone, Copy, Default)]
pub struct VideoEngine;

impl VideoEngine {
    /// Create a video engine. Holds no configuration — limits are
    /// supplied per operation.
    pub fn new() -> Self {
        Self
    }

    /// Inspect `input` via ffprobe: duration, dimensions, codecs,
    /// frame rate, format name, size, audio presence.
    ///
    /// See [`ops::probe`]. No limits parameter — inspection is cheap
    /// and bounded by the engine's hard input-size cap.
    pub fn probe(&self, input: &[u8]) -> VideoResult<VideoInfo> {
        ops::probe::probe(input)
    }

    /// Extract a PNG thumbnail at `at_secs` (`None` → ~10% of duration,
    /// fallback 1s).
    ///
    /// `limits.max_pixels` bounds the probed frame size;
    /// `limits.max_duration_secs`/`max_input_bytes` bound the input.
    /// See [`ops::thumbnail`].
    pub fn thumbnail(
        &self,
        input: &[u8],
        at_secs: Option<f64>,
        limits: &EngineLimits,
    ) -> VideoResult<Vec<u8>> {
        ops::thumbnail::thumbnail(input, at_secs, limits)
    }

    /// Cut `input` to `[start_secs, end_secs)` (`None` → to end).
    ///
    /// `reencode = false` stream-copies; `true` re-encodes H.264/AAC
    /// for frame accuracy. Output keeps the input's container family.
    /// See [`ops::trim`].
    pub fn trim(
        &self,
        input: &[u8],
        start_secs: f64,
        end_secs: Option<f64>,
        reencode: bool,
        limits: &EngineLimits,
    ) -> VideoResult<Vec<u8>> {
        ops::trim::trim(input, start_secs, end_secs, reencode, limits)
    }

    /// Convert `input` to the container named by `target_ext`
    /// (`mp4`, `mkv`, `webm`, `avi`). See [`ops::transcode`].
    pub fn transcode(
        &self,
        input: &[u8],
        target_ext: &str,
        limits: &EngineLimits,
    ) -> VideoResult<Vec<u8>> {
        ops::transcode::transcode(input, target_ext, limits)
    }

    /// Concatenate `inputs` into `target_ext`. See [`ops::concat`].
    pub fn concat(
        &self,
        inputs: &[Vec<u8>],
        target_ext: &str,
        limits: &EngineLimits,
    ) -> VideoResult<Vec<u8>> {
        ops::concat::concat(inputs, target_ext, limits)
    }

    /// Extract the audio track into `target_ext`
    /// (`mp3`, `wav`, `aac`, `m4a`, `ogg`). See [`ops::extract_audio`].
    pub fn extract_audio(
        &self,
        input: &[u8],
        target_ext: &str,
        limits: &EngineLimits,
    ) -> VideoResult<Vec<u8>> {
        ops::extract_audio::extract_audio(input, target_ext, limits)
    }
}

impl ProcessingEngine for VideoEngine {
    fn name(&self) -> &'static str {
        "video"
    }

    fn version(&self) -> &'static str {
        // Pinned per the engine contract; the crate version lives in
        // Cargo.toml but the engine API reports its own semver.
        "0.1.0"
    }

    fn kind(&self) -> EngineKind {
        EngineKind::Video
    }

    fn capabilities(&self) -> EngineCapabilities {
        EngineCapabilities {
            operations: vec![
                EngineOperation::Inspect,
                EngineOperation::Validate,
                EngineOperation::Thumbnail,
                EngineOperation::Trim,
                EngineOperation::Transcode,
                EngineOperation::Concat,
                EngineOperation::ExtractAudio,
            ],
            input_formats: MEDIA_FORMATS.iter().map(|s| s.to_string()).collect(),
            output_formats: MEDIA_FORMATS.iter().map(|s| s.to_string()).collect(),
            // The engine enforces no limits of its own — callers pass
            // EngineLimits per operation.
            limits: EngineLimits::default(),
        }
    }

    fn health<'a>(&'a self) -> EngineFuture<'a, EngineHealth> {
        Box::pin(async move {
            let ffmpeg = tool_available(FFMPEG);
            let ffprobe = tool_available(FFPROBE);
            match (ffmpeg, ffprobe) {
                (true, true) => EngineHealth {
                    status: EngineHealthStatus::Healthy,
                    message: None,
                },
                (false, false) => EngineHealth {
                    status: EngineHealthStatus::Unavailable,
                    message: Some(
                        "neither ffmpeg nor ffprobe found on PATH; video operations unavailable"
                            .to_string(),
                    ),
                },
                _ => {
                    let missing = if ffmpeg { FFPROBE } else { FFMPEG };
                    EngineHealth {
                        status: EngineHealthStatus::Degraded,
                        message: Some(format!(
                            "{missing} not found on PATH; some video operations unavailable"
                        )),
                    }
                }
            }
        })
    }
}
