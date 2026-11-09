//! [`PdfEngine`] — the `ProcessingEngine` implementation backed by the
//! in-process `lopdf` library.
//!
//! All operations are synchronous, CPU-bound functions over in-memory
//! byte buffers (see the `ops` module). They are exposed here as plain
//! methods rather than futures: callers running them on an async
//! runtime should wrap them in `spawn_blocking` themselves. The engine
//! itself holds no mutable state — only an [`EngineLimits`](crate::crates::core::engine::EngineLimits) snapshot —
//! so it is trivially `Send + Sync`.

use crate::crates::core::engine::{
    EngineCapabilities, EngineFuture, EngineHealth, EngineHealthStatus, EngineKind, EngineLimits,
    EngineOperation, ProcessingEngine,
};

use crate::crates::pdf_engine::error::PdfResult;
use crate::crates::pdf_engine::ops::{self, PageText, PdfInfo, RotateDegrees, ValidationReport};

/// The PDF processing engine.
///
/// Backed by `lopdf`, which is compiled into this crate — there is no
/// external binary or service to probe, so [`ProcessingEngine::health`]
/// always reports [`EngineHealthStatus::Healthy`].
///
/// # Limits
///
/// [`PdfEngine::new`] uses [`EngineLimits::default`] (no engine-side
/// limits; upstream application limits still apply). Use
/// [`PdfEngine::with_limits`] to enforce `max_input_bytes`, `max_pages`
/// and `max_outputs`. Enforcement happens inside the ops — see
/// the `ops` module for details.
#[derive(Debug, Clone)]
pub struct PdfEngine {
    limits: EngineLimits,
}

impl PdfEngine {
    /// Create an engine with no engine-side resource limits.
    pub fn new() -> Self {
        Self::with_limits(EngineLimits::default())
    }

    /// Create an engine enforcing the given [`EngineLimits`](crate::crates::core::engine::EngineLimits).
    pub fn with_limits(limits: EngineLimits) -> Self {
        Self { limits }
    }

    /// The limits this engine enforces.
    pub fn limits(&self) -> &EngineLimits {
        &self.limits
    }

    /// Merge multiple PDFs into one, in input order.
    ///
    /// See [`ops::merge`] for semantics and limitations (outlines,
    /// signatures and other document-level structures are not carried
    /// over).
    pub fn merge(&self, inputs: &[&[u8]]) -> PdfResult<Vec<u8>> {
        ops::merge(inputs, &self.limits)
    }

    /// Split a PDF into files of at most `pages_per_file` pages.
    ///
    /// See [`ops::split`].
    pub fn split(&self, input: &[u8], pages_per_file: u32) -> PdfResult<Vec<Vec<u8>>> {
        ops::split(input, pages_per_file, &self.limits)
    }

    /// Rotate pages selected by a 1-based range spec
    /// (`"1-3,8"`, `"all"`) by `degrees`.
    ///
    /// `degrees` may also be given as a raw `u16` via
    /// [`RotateDegrees::try_from`], which rejects anything but
    /// 90/180/270.
    pub fn rotate(&self, input: &[u8], pages: &str, degrees: RotateDegrees) -> PdfResult<Vec<u8>> {
        ops::rotate(input, pages, degrees, &self.limits)
    }

    /// Convenience wrapper accepting raw degree values.
    pub fn rotate_degrees(&self, input: &[u8], pages: &str, degrees: u16) -> PdfResult<Vec<u8>> {
        self.rotate(input, pages, RotateDegrees::try_from(degrees)?)
    }

    /// Extract the pages selected by a 1-based range spec into a new PDF.
    pub fn extract_pages(&self, input: &[u8], pages: &str) -> PdfResult<Vec<u8>> {
        ops::extract_pages(input, pages, &self.limits)
    }

    /// Inspect a PDF: page count, metadata, encryption state, size.
    ///
    /// Tolerates encrypted input (reported via [`PdfInfo::encrypted`]).
    pub fn inspect(&self, input: &[u8]) -> PdfResult<PdfInfo> {
        ops::inspect(input, &self.limits)
    }

    /// Validate PDF structure; returns a report rather than failing on
    /// the first anomaly. Malformed/unloadable input is still an error.
    pub fn validate(&self, input: &[u8]) -> PdfResult<ValidationReport> {
        ops::validate(input, &self.limits)
    }

    /// Best-effort text extraction, one [`PageText`] per page.
    ///
    /// `pages` is an optional range spec; `None` extracts every page.
    pub fn extract_text(&self, input: &[u8], pages: Option<&str>) -> PdfResult<Vec<PageText>> {
        ops::extract_text(input, pages, &self.limits)
    }
}

impl Default for PdfEngine {
    fn default() -> Self {
        Self::new()
    }
}

impl ProcessingEngine for PdfEngine {
    fn name(&self) -> &'static str {
        "pdf"
    }

    fn version(&self) -> &'static str {
        env!("CARGO_PKG_VERSION")
    }

    fn kind(&self) -> EngineKind {
        EngineKind::Pdf
    }

    fn capabilities(&self) -> EngineCapabilities {
        EngineCapabilities {
            operations: vec![
                EngineOperation::Merge,
                EngineOperation::Split,
                EngineOperation::Rotate,
                EngineOperation::ExtractPages,
                EngineOperation::Validate,
                EngineOperation::Inspect,
                EngineOperation::ExtractText,
            ],
            input_formats: vec!["pdf".to_string()],
            output_formats: vec!["pdf".to_string()],
            limits: self.limits.clone(),
        }
    }

    fn health<'a>(&'a self) -> EngineFuture<'a, EngineHealth> {
        Box::pin(async move {
            // lopdf is embedded in this crate — there is no external
            // process, service, or license to probe.
            EngineHealth {
                status: EngineHealthStatus::Healthy,
                message: None,
            }
        })
    }
}
