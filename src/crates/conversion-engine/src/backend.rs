//! The [`ConversionBackend`] trait — one implementation per
//! conversion strategy (pure-Rust, external CLI tool, ...).

use super::error::ConvertResult;
use super::model::{ConvertOutput, ConvertRequest, Format};

/// Whether a backend can run in this environment.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum BackendAvailability {
    /// Backend is usable; carries its reported version string when
    /// the tool exposes one (`"pandoc 3.1.11"`).
    Available { version: Option<String> },
    /// Backend cannot be used; `reason` is safe to show users (no
    /// paths to binaries, no stack traces).
    Unavailable { reason: String },
}

/// One conversion strategy.
///
/// Implementations must:
/// - be `Send + Sync` (stored as `Arc<dyn ConversionBackend>`)
/// - advertise support via [`Self::supports`] honestly — the engine
///   relies on it for dispatch and capability reporting
/// - never panic on hostile input; return [`ConvertError`](crate::crates::conversion_engine::ConvertError)
/// - bound their work with the request's [`ConvertOptions`] and the
///   engine's limits where applicable
pub trait ConversionBackend: Send + Sync {
    /// Stable backend identifier (`"rust"`, `"pandoc"`, ...).
    fn name(&self) -> &'static str;

    /// Point-in-time availability. Must be cheap — cache probe
    /// results at construction if probing is expensive.
    fn availability(&self) -> BackendAvailability;

    /// Whether this backend can convert `from` → `to`.
    fn supports(&self, from: Format, to: Format) -> bool;

    /// Perform the conversion. Called only for pairs [`Self::supports`]
    /// accepts, but must still validate input defensively.
    fn convert(&self, req: &ConvertRequest) -> ConvertResult<ConvertOutput>;
}
