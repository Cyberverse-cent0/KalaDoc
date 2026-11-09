//! Error types for the pipeline engine.

use crate::crates::pdf_engine::PdfError;
use crate::crates::security_engine::SecurityError;

/// Result alias for pipeline operations.
pub type PipelineResult<T> = Result<T, PipelineError>;

/// Errors produced by [`PipelineEngine`](crate::crates::pipeline_engine::PipelineEngine).
///
/// Definition problems (empty steps, unknown engine/operation) are
/// reported *before* any step runs, so a bad pipeline never partially
/// executes. Runtime failures inside a step surface as
/// [`PipelineError::StepFailed`] with full step context.
#[derive(Debug, thiserror::Error)]
pub enum PipelineError {
    /// The definition itself is unusable: no steps, `params` not an
    /// object, a required parameter missing or malformed, or a step
    /// tried to consume a multi-output artifact as a single stream.
    #[error("invalid pipeline definition: {0}")]
    InvalidDefinition(String),

    /// A configured [`PipelineLimits`](crate::crates::pipeline_engine::PipelineLimits)
    /// bound was exceeded — per-step input size, cumulative bytes, or
    /// step count.
    #[error("pipeline resource limit exceeded: {0}")]
    LimitExceeded(String),

    /// A step named an engine the pipeline does not dispatch to.
    #[error("pipeline step {step_index} `{step_name}`: unknown engine `{engine}`")]
    UnknownEngine {
        /// 0-based step position.
        step_index: usize,
        /// Step name from the definition.
        step_name: String,
        /// The unrecognized engine name.
        engine: String,
    },

    /// A step named an operation the target engine does not support
    /// in a pipeline. Detected up front, before any step executes.
    #[error(
        "pipeline step {step_index} `{step_name}`: engine `{engine}` does not support operation `{operation}`"
    )]
    UnsupportedOperation {
        /// 0-based step position.
        step_index: usize,
        /// Step name from the definition.
        step_name: String,
        /// Target engine name.
        engine: String,
        /// The unrecognized operation name.
        operation: String,
    },

    /// [`PipelineEngine::run_named`](crate::crates::pipeline_engine::PipelineEngine::run_named)
    /// was called with a name that is not a built-in pipeline.
    #[error("no built-in pipeline named `{0}`")]
    PipelineNotFound(String),

    /// A step failed at run time. The pipeline aborts immediately —
    /// fail-fast, no partial output escapes. `source` preserves the
    /// underlying engine error for logs.
    #[error("pipeline step {index} `{name}` ({engine}.{operation}) failed: {source}")]
    StepFailed {
        /// 0-based step position.
        index: usize,
        /// Step name from the definition.
        name: String,
        /// Engine that ran the step.
        engine: String,
        /// Operation that ran.
        operation: String,
        /// The underlying failure.
        #[source]
        source: StepError,
    },
}

/// Errors that can originate inside a single pipeline step.
///
/// Wraps the concrete engine error types so callers can still match on
/// [`PdfError`] / [`SecurityError`] via [`StepError::source`] when
/// needed, while [`PipelineError::StepFailed`] carries the step
/// context.
#[derive(Debug, thiserror::Error)]
pub enum StepError {
    /// The PDF engine rejected or failed the operation.
    #[error("{0}")]
    Pdf(#[from] PdfError),

    /// The security engine rejected the input.
    #[error("{0}")]
    Security(#[from] SecurityError),

    /// A step parameter was missing, had the wrong type, or failed
    /// operation-specific validation (e.g. bad page range is reported
    /// by the engine instead; this covers params the engine never saw).
    #[error("invalid step parameters: {0}")]
    Params(String),

    /// The artifact flowing into the step was not usable by it —
    /// e.g. a `Multiple` artifact fed into a step that needs a single
    /// byte stream.
    #[error("invalid pipeline state: {0}")]
    InvalidState(String),
}
