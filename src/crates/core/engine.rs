//! Shared contracts for processing engines.
//!
//! Every engine (PDF, image, conversion, ...) implements
//! [`ProcessingEngine`]. The engine registry stores trait objects,
//! and the CLI/MCP layers discover capabilities through it — engines
//! never get called directly by adapters.

use std::future::Future;
use std::pin::Pin;

use serde::{Deserialize, Serialize};

/// Broad category an engine belongs to. Used for filtering in
/// `engine list` / registry lookups.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum EngineKind {
    Pdf,
    Image,
    Conversion,
    Security,
    Pipeline,
    Video,
}

/// Operations an engine can perform.
///
/// Serialized as `snake_case` in JSON output so CLI/MCP responses are
/// stable and scriptable. Add variants here when a new operation is
/// introduced; engines advertise only the subset they implement.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum EngineOperation {
    Merge,
    Split,
    Rotate,
    Resize,
    Adjust,
    Flip,
    Thumbnail,
    ExtractPages,
    RemovePages,
    Reorder,
    Crop,
    Compress,
    Repair,
    Flatten,
    Overlay,
    Watermark,
    Stamp,
    Redact,
    Sanitize,
    Sign,
    VerifySignature,
    Validate,
    Inspect,
    PageCount,
    ExtractMetadata,
    ExtractImages,
    ExtractText,
    Render,
    Compare,
    Convert,
    Ocr,
    Encrypt,
    Decrypt,
    Thumbnail,
    Trim,
    Transcode,
    Concat,
    ExtractAudio,
}

impl EngineOperation {
    /// Stable string form used in CLI output and job payloads.
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Merge => "merge",
            Self::Split => "split",
            Self::Rotate => "rotate",
            Self::Resize => "resize",
            Self::Adjust => "adjust",
            Self::Flip => "flip",
            Self::Thumbnail => "thumbnail",
            Self::ExtractPages => "extract_pages",
            Self::RemovePages => "remove_pages",
            Self::Reorder => "reorder",
            Self::Crop => "crop",
            Self::Compress => "compress",
            Self::Repair => "repair",
            Self::Flatten => "flatten",
            Self::Overlay => "overlay",
            Self::Watermark => "watermark",
            Self::Stamp => "stamp",
            Self::Redact => "redact",
            Self::Sanitize => "sanitize",
            Self::Sign => "sign",
            Self::VerifySignature => "verify_signature",
            Self::Validate => "validate",
            Self::Inspect => "inspect",
            Self::PageCount => "page_count",
            Self::ExtractMetadata => "extract_metadata",
            Self::ExtractImages => "extract_images",
            Self::ExtractText => "extract_text",
            Self::Render => "render",
            Self::Compare => "compare",
            Self::Convert => "convert",
            Self::Ocr => "ocr",
            Self::Encrypt => "encrypt",
            Self::Decrypt => "decrypt",
            Self::Thumbnail => "thumbnail",
            Self::Trim => "trim",
            Self::Transcode => "transcode",
            Self::Concat => "concat",
            Self::ExtractAudio => "extract_audio",
        }
    }
}

/// Resource limits an engine enforces on a single operation.
///
/// `None` means the engine does not enforce that limit itself —
/// global application limits still apply upstream.
#[derive(Debug, Clone, Default)]
pub struct EngineLimits {
    /// Maximum size of a single input document in bytes.
    pub max_input_bytes: Option<u64>,
    /// Maximum page count the engine will process.
    pub max_pages: Option<u32>,
    /// Maximum decoded pixel count (width × height) the engine will
    /// materialize for a single image/raster input — a decompression
    /// bomb guard. Ignored by engines without raster inputs.
    pub max_pixels: Option<u64>,
    /// Maximum number of output artifacts produced by one operation.
    pub max_outputs: Option<u32>,
    /// Maximum media duration in seconds an engine will process
    /// (e.g. video/audio length). `None` = no limit. Ignored by
    /// engines without timed media inputs.
    pub max_duration_secs: Option<u64>,
}

/// Static description of what an engine supports.
#[derive(Debug, Clone)]
pub struct EngineCapabilities {
    /// Operations the engine implements. Must be non-empty for a
    /// usable engine.
    pub operations: Vec<EngineOperation>,
    /// Input format identifiers the engine accepts (e.g. `"pdf"`).
    pub input_formats: Vec<String>,
    /// Output format identifiers the engine produces.
    pub output_formats: Vec<String>,
    /// Per-operation resource limits.
    pub limits: EngineLimits,
}

/// Health of an engine at a point in time.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum EngineHealthStatus {
    Healthy,
    Degraded,
    Unavailable,
}

/// Result of an engine health check.
#[derive(Debug, Clone)]
pub struct EngineHealth {
    pub status: EngineHealthStatus,
    /// Human-readable detail when not fully healthy. Never contains
    /// secrets, paths outside the storage root, or stack traces.
    pub message: Option<String>,
}

/// Boxed future returned by trait methods — keeps [`ProcessingEngine`]
/// object-safe (`dyn ProcessingEngine`) without an async-trait crate.
/// Same pattern as `job_engine::JobHandler`.
pub type EngineFuture<'a, T> = Pin<Box<dyn Future<Output = T> + Send + 'a>>;

/// Contract every processing engine implements.
///
/// Methods are synchronous where possible (`name`, `capabilities`)
/// because they describe static metadata. `health` is async to allow
/// engines to probe external resources (binaries, services, license
/// state) without blocking the caller.
///
/// Implementations must:
/// - be `Send + Sync` (stored as `Arc<dyn ProcessingEngine>`)
/// - never panic across the trait boundary — report failure via
///   [`EngineHealth`] or their own error type inside operations
/// - report capabilities honestly: an operation listed here must
///   actually work, and unsupported operations must be rejected by
///   the engine, not silently approximated
pub trait ProcessingEngine: Send + Sync {
    /// Stable engine identifier, e.g. `"pdf"`. Unique within a registry.
    fn name(&self) -> &'static str;

    /// Engine version string (semver-ish, informational).
    fn version(&self) -> &'static str;

    /// Category this engine belongs to.
    fn kind(&self) -> EngineKind;

    /// What the engine supports. Called often — keep it cheap and
    /// side-effect free.
    fn capabilities(&self) -> EngineCapabilities;

    /// Point-in-time health check. Must not mutate engine state and
    /// must complete promptly; long probes belong in a background task.
    fn health<'a>(&'a self) -> EngineFuture<'a, EngineHealth>;
}
