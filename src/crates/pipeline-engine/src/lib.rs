//! Pipeline engine — chains operations across processing engines.
//!
//! A [`PipelineDefinition`] is a named, ordered sequence of
//! [`PipelineStep`]s. Each step names a target engine (`"pdf"` or
//! `"security"`), an operation, and a JSON `params` object. Running a
//! definition feeds the input bytes through the steps in order: byte
//! transforms replace the current document, report steps (`validate`,
//! `inspect`, `extract_text`, ...) leave the bytes untouched and record
//! a structured report in the [`PipelineOutput`].
//!
//! # Dispatch model
//!
//! [`ProcessingEngine`](crate::crates::core::engine::ProcessingEngine)
//! is a metadata-only trait — it has no `execute` method, and the
//! [`EngineRegistry`](crate::crates::engine_registry::EngineRegistry)
//! stores trait objects for discovery, not invocation. Pipelines
//! therefore dispatch **directly to the concrete engine types**
//! ([`PdfEngine`](crate::crates::pdf_engine::PdfEngine),
//! [`SecurityEngine`](crate::crates::security_engine::SecurityEngine))
//! held inside [`PipelineEngine`]. This is deliberate: engines are
//! in-process libraries here, not remote services, so a pipeline is a
//! typed composition, not a dynamic plugin call. Adding an engine means
//! adding a dispatch arm — the compiler enforces exhaustiveness.
//!
//! # Limits
//!
//! [`PipelineLimits`] bounds every run: per-step input size
//! (`max_input_bytes`, also forwarded to the wrapped engines), total
//! bytes materialized across the whole run (`max_total_bytes`), step
//! count (`max_steps`), and page/output budgets handed to the PDF
//! engine. The first failing step aborts the run with
//! [`PipelineError::StepFailed`] carrying the step index, name, engine,
//! and operation — no partial state escapes.
//!
//! # Built-in pipelines
//!
//! [`builtin_definitions`] ships `pdf.intake` (security gate → PDF
//! validate → inspect → extract_text) and `pdf.extract-text`. Use
//! [`PipelineEngine::run_named`] to execute a built-in by name;
//! [`PipelineEngine::run_pipeline`] runs any caller-supplied
//! definition. Definitions are plain serde types — JSON/YAML configs
//! can describe custom pipelines.

mod builtin;
/// Base64 helpers for byte artifacts in JSON output.
mod b64_serde;
mod engine;
mod error;
mod limits;
mod model;
#[cfg(test)]
mod tests;

pub use builtin::{builtin_definitions, pdf_extract_text, pdf_intake};
pub use engine::PipelineEngine;
pub use error::{PipelineError, PipelineResult, StepError};
pub use limits::PipelineLimits;
pub use model::{
    PipelineArtifact, PipelineDefinition, PipelineOutput, PipelineStep, StepReport, engine_name,
    operation,
};
