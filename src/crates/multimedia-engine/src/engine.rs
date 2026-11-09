//! [`MediaEngine`] — the `ProcessingEngine` implementation backed by
//! managed `ffmpeg`/`ffprobe` processes.
//!
//! The engine is a thin orchestration layer: every operation writes
//! its byte inputs to a temp dir, invokes the tool with an argv
//! vector, enforces [`MediaLimits`], and reads outputs back. It holds
//! no mutable state — a lazily-discovered [`Runtime`] plus a limits
//! snapshot — so it is `Send + Sync`.
//!
//! Async callers should wrap ops in `spawn_blocking`: FFmpeg work is
//! CPU/IO-bound and process-backed.

use std::sync::OnceLock;

use crate::crates::core::engine::{
    EngineCapabilities, EngineFuture, EngineHealth, EngineHealthStatus, EngineKind, EngineLimits,
    EngineOperation, ProcessingEngine,
};

use crate::crates::multimedia_engine::error::MediaResult;
use crate::crates::multimedia_engine::model::MediaLimits;
use crate::crates::multimedia_engine::runtime::discovery::{self, Runtime};

/// The multimedia (audio/video) processing engine.
///
/// # Limits
///
/// [`MediaEngine::new`] uses [`MediaLimits::default`]. Use
/// [`MediaEngine::with_limits`] to tighten input/output byte caps,
/// per-invocation timeouts, or the max output-file count.
#[derive(Debug)]
pub struct MediaEngine {
    limits: MediaLimits,
    runtime: OnceLock<Result<Runtime, String>>,
}

impl MediaEngine {
    /// Create an engine with default limits; tools are discovered on
    /// first use.
    pub fn new() -> Self {
        Self::with_limits(MediaLimits::default())
    }

    /// Create an engine enforcing the given [`MediaLimits`].
    pub fn with_limits(limits: MediaLimits) -> Self {
        Self {
            limits,
            runtime: OnceLock::new(),
        }
    }

    /// The limits this engine enforces.
    pub fn limits(&self) -> &MediaLimits {
        &self.limits
    }

    /// The discovered FFmpeg runtime, or the discovery error.
    /// Probes `PATH` once and caches the outcome.
    pub fn runtime(&self) -> MediaResult<&Runtime> {
        match self
            .runtime
            .get_or_init(|| Runtime::discover().map_err(|e| e.to_string()))
        {
            Ok(rt) => Ok(rt),
            Err(e) => {
                Err(crate::crates::multimedia_engine::error::MediaError::MissingTool(e.clone()))
            }
        }
    }

    /// FFmpeg/ffprobe capability sets (codecs, filters, hwaccels),
    /// probed once per process.
    pub fn capabilities_detail(&self) -> MediaResult<&discovery::RuntimeCapabilities> {
        Ok(self.runtime()?.capabilities()?)
    }
}

impl Default for MediaEngine {
    fn default() -> Self {
        Self::new()
    }
}

impl Clone for MediaEngine {
    fn clone(&self) -> Self {
        // The runtime OnceLock is shared intentionally: discovery is
        // idempotent and cheap to share across clones.
        Self {
            limits: self.limits.clone(),
            runtime: OnceLock::new(),
        }
    }
}

impl ProcessingEngine for MediaEngine {
    fn name(&self) -> &'static str {
        "multimedia"
    }

    fn version(&self) -> &'static str {
        env!("CARGO_PKG_VERSION")
    }

    fn kind(&self) -> EngineKind {
        EngineKind::Multimedia
    }

    fn capabilities(&self) -> EngineCapabilities {
        // Advertise only what is actually implemented. The op modules
        // under probe/, conversion/, video/, audio/, frames/,
        // subtitles/, analysis/, packaging/, metadata/, encoding/
        // and filters/ cover the operations listed here; finer-grained
        // advertisement happens per-op via the runtime capability
        // registry (codec/filter availability varies by FFmpeg build).
        EngineCapabilities {
            operations: vec![
                EngineOperation::Inspect,
                EngineOperation::Convert,
                EngineOperation::ExtractMetadata,
                EngineOperation::Merge,
                EngineOperation::Split,
                EngineOperation::Overlay,
                EngineOperation::Watermark,
                EngineOperation::Thumbnail,
                EngineOperation::Compress,
                EngineOperation::Compare,
            ],
            input_formats: vec![
                "mp4".into(),
                "mkv".into(),
                "mov".into(),
                "avi".into(),
                "webm".into(),
                "mp3".into(),
                "wav".into(),
                "flac".into(),
                "aac".into(),
                "ogg".into(),
                "opus".into(),
                "png".into(),
                "jpeg".into(),
            ],
            output_formats: vec![
                "mp4".into(),
                "mkv".into(),
                "webm".into(),
                "mp3".into(),
                "wav".into(),
                "flac".into(),
                "aac".into(),
                "ogg".into(),
                "opus".into(),
                "png".into(),
                "jpeg".into(),
                "webp".into(),
            ],
            limits: EngineLimits {
                max_input_bytes: self.limits.max_input_bytes,
                max_pages: None,
                max_pixels: None,
                max_outputs: self.limits.max_output_files,
            },
        }
    }

    fn health<'a>(&'a self) -> EngineFuture<'a, EngineHealth> {
        Box::pin(async move {
            match self.runtime() {
                Ok(rt) => {
                    let version = rt
                        .capabilities()
                        .ok()
                        .and_then(|c| c.ffmpeg_version.clone());
                    EngineHealth {
                        status: EngineHealthStatus::Healthy,
                        message: version,
                    }
                }
                Err(e) => EngineHealth {
                    status: EngineHealthStatus::Unavailable,
                    message: Some(e.to_string()),
                },
            }
        })
    }
}
