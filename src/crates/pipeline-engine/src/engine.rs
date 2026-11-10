//! [`PipelineEngine`] — a [`ProcessingEngine`] that chains operations
//! across the concrete engine types.
//!
//! # Why direct dispatch?
//!
//! [`ProcessingEngine`](crate::crates::core::engine::ProcessingEngine)
//! is metadata-only: it exposes `name`/`version`/`kind`/
//! `capabilities`/`health` but **no execute method**, and
//! [`EngineRegistry`](crate::crates::engine_registry::EngineRegistry)
//! stores `Arc<dyn ProcessingEngine>` for discovery, not invocation.
//! Engines here are in-process libraries, so a pipeline composes them
//! by holding concrete [`PdfEngine`] and [`SecurityEngine`] instances
//! and calling their typed methods directly. Adding engine support
//! means adding a dispatch arm in [`PipelineEngine::dispatch`] — the
//! compiler, not a registry lookup, enforces correctness.

use serde_json::Value;

use crate::crates::core::engine::{
    EngineCapabilities, EngineFuture, EngineHealth, EngineHealthStatus, EngineKind, EngineLimits,
    EngineOperation, ProcessingEngine,
};
use crate::crates::pdf_engine::PdfEngine;
use crate::crates::security_engine::{
    AudioContainer, ResourceLimits, SecurityEngine, SniffedFormat, VideoContainer,
};

use super::error::{PipelineError, PipelineResult, StepError};
use super::limits::PipelineLimits;
use super::model::{
    PipelineArtifact, PipelineDefinition, PipelineOutput, PipelineStep, StepReport, engine_name,
    operation,
};

/// The pipeline engine.
///
/// Holds concrete instances of the engines it dispatches to, each
/// configured with limits derived from [`PipelineLimits`]. Stateless
/// apart from that configuration, so it is trivially `Send + Sync`.
///
/// All operations are synchronous CPU-bound work over in-memory
/// buffers; async callers should wrap [`run_pipeline`](Self::run_pipeline)
/// in `spawn_blocking`, matching the wrapped engines' contract.
#[derive(Debug)]
pub struct PipelineEngine {
    pdf: PdfEngine,
    security: SecurityEngine,
    limits: PipelineLimits,
    security_limits: ResourceLimits,
}

/// What one dispatched step produced.
struct StepOutcome {
    /// The artifact flowing to the next step (or the final output).
    /// `None` means "keep the current artifact" — report steps pass
    /// the byte stream through unchanged.
    artifact: Option<PipelineArtifact>,
    /// Structured report for report steps; `None` for transforms.
    report: Option<Value>,
    /// Bytes newly materialized by this step — counted against
    /// `max_total_bytes`. `0` for report steps that pass bytes
    /// through unchanged.
    produced_bytes: u64,
}

impl PipelineEngine {
    /// Create an engine with [`PipelineLimits::default`] and default
    /// [`ResourceLimits`] for security steps.
    pub fn new() -> Self {
        Self::with_limits(PipelineLimits::default(), ResourceLimits::default())
    }

    /// Create an engine enforcing `limits` on the pipeline itself and
    /// `security_limits` on `security.validate_upload` steps.
    pub fn with_limits(limits: PipelineLimits, security_limits: ResourceLimits) -> Self {
        let pdf_limits = EngineLimits {
            max_input_bytes: Some(limits.max_input_bytes),
            max_pages: limits.max_pages,
            max_pixels: None,
            max_outputs: limits.max_outputs,
            max_duration_secs: None,
        };
        Self {
            pdf: PdfEngine::with_limits(pdf_limits),
            security: SecurityEngine::new(),
            limits,
            security_limits,
        }
    }

    /// The limits this engine enforces.
    pub fn limits(&self) -> &PipelineLimits {
        &self.limits
    }

    /// Run `def` against `input`, failing fast on the first step error.
    ///
    /// The input bytes seed the pipeline as a [`PipelineArtifact::Bytes`];
    /// each step either transforms the artifact or appends a report.
    /// Definition problems (empty, too many steps, unknown engine or
    /// operation, malformed `params`) are detected **before** any step
    /// runs.
    ///
    /// # Errors
    /// - [`PipelineError::InvalidDefinition`] / [`PipelineError::UnknownEngine`] /
    ///   [`PipelineError::UnsupportedOperation`] for bad definitions;
    /// - [`PipelineError::LimitExceeded`] when a size/count budget is
    ///   exceeded (checked before and after every step);
    /// - [`PipelineError::StepFailed`] wrapping the concrete engine
    ///   error with step index, name, engine, and operation.
    pub fn run_pipeline(
        &self,
        def: &PipelineDefinition,
        input: &[u8],
    ) -> PipelineResult<PipelineOutput> {
        self.check_definition(def)?;

        let mut artifact = PipelineArtifact::Bytes(input.to_vec());
        // The input itself counts toward the cumulative budget.
        let mut total_bytes = input.len() as u64;
        self.limits
            .check_total(total_bytes)
            .map_err(PipelineError::LimitExceeded)?;

        let mut reports = Vec::with_capacity(def.steps.len());
        for (index, step) in def.steps.iter().enumerate() {
            // Every supported step consumes a single byte stream;
            // a Multiple artifact here means a non-terminal `split`.
            let bytes = artifact
                .as_bytes()
                .ok_or_else(|| PipelineError::StepFailed {
                    index,
                    name: step.name.clone(),
                    engine: step.engine.clone(),
                    operation: step.operation.clone(),
                    source: StepError::InvalidState(
                        "step requires a single byte stream but the artifact is multi-output"
                            .to_string(),
                    ),
                })?;

            self.limits
                .check_step_input(bytes.len() as u64)
                .map_err(PipelineError::LimitExceeded)?;

            let outcome =
                self.dispatch(step, bytes)
                    .map_err(|source| PipelineError::StepFailed {
                        index,
                        name: step.name.clone(),
                        engine: step.engine.clone(),
                        operation: step.operation.clone(),
                        source,
                    })?;

            total_bytes = total_bytes.saturating_add(outcome.produced_bytes);
            self.limits
                .check_total(total_bytes)
                .map_err(PipelineError::LimitExceeded)?;

            if let Some(next) = outcome.artifact {
                artifact = next;
            }
            reports.push(StepReport {
                index,
                name: step.name.clone(),
                engine: step.engine.clone(),
                operation: step.operation.clone(),
                output: outcome.report,
            });
        }

        Ok(PipelineOutput {
            pipeline: def.name.clone(),
            artifact,
            steps: reports,
        })
    }

    /// Run a built-in pipeline by name (see [`builtin_definitions`]).
    ///
    /// # Errors
    /// [`PipelineError::PipelineNotFound`] when `name` matches no
    /// built-in, otherwise whatever [`run_pipeline`](Self::run_pipeline)
    /// returns.
    pub fn run_named(&self, name: &str, input: &[u8]) -> PipelineResult<PipelineOutput> {
        let def = super::builtin::find_builtin(name)
            .ok_or_else(|| PipelineError::PipelineNotFound(name.to_string()))?;
        self.run_pipeline(&def, input)
    }

    /// Validate a definition without executing it: non-empty, within
    /// `max_steps`, every `(engine, operation)` pair supported, and
    /// every `params` value an object.
    fn check_definition(&self, def: &PipelineDefinition) -> PipelineResult<()> {
        if def.steps.is_empty() {
            return Err(PipelineError::InvalidDefinition(format!(
                "pipeline `{}` has no steps",
                def.name
            )));
        }
        if def.steps.len() as u64 > self.limits.max_steps as u64 {
            return Err(PipelineError::LimitExceeded(format!(
                "pipeline `{}` has {} steps, exceeding max_steps ({})",
                def.name,
                def.steps.len(),
                self.limits.max_steps
            )));
        }
        for (index, step) in def.steps.iter().enumerate() {
            if !is_known_engine(&step.engine) {
                return Err(PipelineError::UnknownEngine {
                    step_index: index,
                    step_name: step.name.clone(),
                    engine: step.engine.clone(),
                });
            }
            if !is_supported(&step.engine, &step.operation) {
                return Err(PipelineError::UnsupportedOperation {
                    step_index: index,
                    step_name: step.name.clone(),
                    engine: step.engine.clone(),
                    operation: step.operation.clone(),
                });
            }
            if !step.params.is_object() && !step.params.is_null() {
                return Err(PipelineError::InvalidDefinition(format!(
                    "pipeline `{}` step {} `{}`: params must be a JSON object",
                    def.name, index, step.name
                )));
            }
        }
        Ok(())
    }

    /// Dispatch one step to its concrete engine. `input` is the current
    /// byte stream; the returned [`StepOutcome`] carries the next
    /// artifact, an optional report, and newly produced bytes.
    fn dispatch(&self, step: &PipelineStep, input: &[u8]) -> Result<StepOutcome, StepError> {
        match (step.engine.as_str(), step.operation.as_str()) {
            // ---- security engine ----
            (engine_name::SECURITY, operation::VALIDATE) => {
                let max_bytes =
                    optional_u64(&step.params, "max_bytes").unwrap_or(self.limits.max_input_bytes);
                self.security.validate(input, max_bytes)?;
                Ok(report_step(
                    serde_json::json!({ "validated": true, "size_bytes": input.len() }),
                ))
            }
            (engine_name::SECURITY, operation::VALIDATE_UPLOAD) => {
                let filename = required_str(&step.params, "filename")?;
                let expected = match optional_str(&step.params, "expected_format") {
                    Some(s) => Some(parse_sniffed_format(s)?),
                    None => None,
                };
                let report = crate::crates::security_engine::validate_upload(
                    filename,
                    input.len() as u64,
                    input,
                    &self.security_limits,
                    expected,
                )?;
                Ok(report_step(
                    serde_json::to_value(&report).map_err(json_err)?,
                ))
            }
            // ---- pdf engine: report steps (bytes pass through) ----
            (engine_name::PDF, operation::PDF_VALIDATE) => {
                let report = self.pdf.validate(input)?;
                Ok(report_step(
                    serde_json::to_value(&report).map_err(json_err)?,
                ))
            }
            (engine_name::PDF, operation::PDF_INSPECT) => {
                let info = self.pdf.inspect(input)?;
                Ok(report_step(serde_json::to_value(&info).map_err(json_err)?))
            }
            (engine_name::PDF, operation::PDF_EXTRACT_TEXT) => {
                let pages = optional_str(&step.params, "pages");
                let texts = self.pdf.extract_text(input, pages)?;
                Ok(report_step(serde_json::to_value(&texts).map_err(json_err)?))
            }
            // ---- pdf engine: byte transforms ----
            (engine_name::PDF, operation::PDF_EXTRACT_PAGES) => {
                let pages = required_str(&step.params, "pages")?;
                let out = self.pdf.extract_pages(input, pages)?;
                Ok(bytes_step(PipelineArtifact::Bytes(out)))
            }
            (engine_name::PDF, operation::PDF_ROTATE) => {
                let pages = required_str(&step.params, "pages")?;
                let degrees = required_u64(&step.params, "degrees")?;
                let degrees = u16::try_from(degrees).map_err(|_| {
                    StepError::Params(format!("degrees {degrees} does not fit in u16"))
                })?;
                let out = self.pdf.rotate_degrees(input, pages, degrees)?;
                Ok(bytes_step(PipelineArtifact::Bytes(out)))
            }
            (engine_name::PDF, operation::PDF_SPLIT) => {
                let pages_per_file = required_u64(&step.params, "pages_per_file")?;
                let pages_per_file = u32::try_from(pages_per_file).map_err(|_| {
                    StepError::Params(format!(
                        "pages_per_file {pages_per_file} does not fit in u32"
                    ))
                })?;
                let outs = self.pdf.split(input, pages_per_file)?;
                Ok(bytes_step(PipelineArtifact::Multiple(outs)))
            }
            // Unreachable: check_definition rejects unsupported pairs
            // before dispatch. Kept as a defensive arm so a future
            // caller bypassing validation still gets an error, not a
            // panic.
            _ => Err(StepError::Params(format!(
                "unsupported engine/operation pair `{}.{}`",
                step.engine, step.operation
            ))),
        }
    }
}

impl Default for PipelineEngine {
    fn default() -> Self {
        Self::new()
    }
}

impl ProcessingEngine for PipelineEngine {
    fn name(&self) -> &'static str {
        "pipeline"
    }

    fn version(&self) -> &'static str {
        env!("CARGO_PKG_VERSION")
    }

    fn kind(&self) -> EngineKind {
        EngineKind::Pipeline
    }

    fn capabilities(&self) -> EngineCapabilities {
        EngineCapabilities {
            // The union of operations dispatchable through a pipeline.
            // `merge` is excluded — it needs multiple input documents,
            // which a single-stream pipeline cannot express.
            operations: vec![
                EngineOperation::Validate,
                EngineOperation::Inspect,
                EngineOperation::ExtractText,
                EngineOperation::ExtractPages,
                EngineOperation::Rotate,
                EngineOperation::Split,
            ],
            input_formats: vec!["pdf".to_string()],
            output_formats: vec!["pdf".to_string()],
            limits: EngineLimits {
                max_input_bytes: Some(self.limits.max_input_bytes),
                max_pages: self.limits.max_pages,
                max_pixels: None,
                max_outputs: self.limits.max_outputs,
                max_duration_secs: None,
            },
        }
    }

    fn health<'a>(&'a self) -> EngineFuture<'a, EngineHealth> {
        Box::pin(async move {
            // Both wrapped engines are in-process libraries; a deeper
            // check could probe their health, but they are compiled in
            // and stateless, so the pipeline is healthy whenever they
            // are.
            EngineHealth {
                status: EngineHealthStatus::Healthy,
                message: None,
            }
        })
    }
}

// ---- dispatch helpers ----

/// Engines the dispatcher knows how to reach.
fn is_known_engine(engine: &str) -> bool {
    matches!(engine, engine_name::PDF | engine_name::SECURITY)
}

/// `(engine, operation)` pairs [`PipelineEngine::dispatch`] handles.
/// Must stay in sync with the match arms — `check_definition` uses it
/// for up-front validation.
fn is_supported(engine: &str, operation_name: &str) -> bool {
    match engine {
        engine_name::SECURITY => matches!(
            operation_name,
            operation::VALIDATE | operation::VALIDATE_UPLOAD
        ),
        engine_name::PDF => matches!(
            operation_name,
            operation::PDF_VALIDATE
                | operation::PDF_INSPECT
                | operation::PDF_EXTRACT_TEXT
                | operation::PDF_EXTRACT_PAGES
                | operation::PDF_ROTATE
                | operation::PDF_SPLIT
        ),
        _ => false,
    }
}

/// Optional string param; `None` when absent or JSON null.
fn optional_str<'a>(params: &'a Value, key: &str) -> Option<&'a str> {
    params.get(key).and_then(Value::as_str)
}

/// Required string param.
fn required_str<'a>(params: &'a Value, key: &str) -> Result<&'a str, StepError> {
    optional_str(params, key)
        .ok_or_else(|| StepError::Params(format!("missing required string parameter `{key}`")))
}

/// Optional unsigned-integer param.
fn optional_u64(params: &Value, key: &str) -> Option<u64> {
    params.get(key).and_then(Value::as_u64)
}

/// Required unsigned-integer param.
fn required_u64(params: &Value, key: &str) -> Result<u64, StepError> {
    optional_u64(params, key)
        .ok_or_else(|| StepError::Params(format!("missing required integer parameter `{key}`")))
}

/// Parse a `SniffedFormat` name for `validate_upload.expected_format`.
///
/// Accepts every variant [`sniff_format`](crate::crates::security_engine::sniff_format)
/// can produce: the scalar formats by their serde names, video and
/// audio containers by their [`VideoContainer`]/[`AudioContainer`]
/// serde names (which are also their conventional extensions in most
/// cases), plus a few common aliases (`jpg`, `mkv`, `ts`, `mpg`,
/// `wmv`, …). Matching is exact and case-sensitive — pipeline JSON
/// uses lowercase names.
fn parse_sniffed_format(s: &str) -> Result<SniffedFormat, StepError> {
    match s {
        "pdf" => Ok(SniffedFormat::Pdf),
        "png" => Ok(SniffedFormat::Png),
        "jpeg" | "jpg" => Ok(SniffedFormat::Jpeg),
        "zip" => Ok(SniffedFormat::Zip),
        "gif" => Ok(SniffedFormat::Gif),
        "unknown" => Ok(SniffedFormat::Unknown),
        // Video containers — `VideoContainer` serde names plus aliases.
        "mp4" | "mov" => Ok(SniffedFormat::Video {
            container: VideoContainer::Mp4,
        }),
        "matroska" | "mkv" => Ok(SniffedFormat::Video {
            container: VideoContainer::Matroska,
        }),
        "webm" => Ok(SniffedFormat::Video {
            container: VideoContainer::Webm,
        }),
        "avi" => Ok(SniffedFormat::Video {
            container: VideoContainer::Avi,
        }),
        "mpegts" | "mpeg-ts" | "ts" => Ok(SniffedFormat::Video {
            container: VideoContainer::MpegTs,
        }),
        "mpegps" | "mpeg-ps" | "mpg" | "mpeg" => Ok(SniffedFormat::Video {
            container: VideoContainer::MpegPs,
        }),
        "flv" => Ok(SniffedFormat::Video {
            container: VideoContainer::Flv,
        }),
        "asf" | "wmv" | "wma" => Ok(SniffedFormat::Video {
            container: VideoContainer::Asf,
        }),
        "mpegvideo" | "m2v" => Ok(SniffedFormat::Video {
            container: VideoContainer::MpegVideo,
        }),
        "mxf" => Ok(SniffedFormat::Video {
            container: VideoContainer::Mxf,
        }),
        "f4v" => Ok(SniffedFormat::Video {
            container: VideoContainer::F4v,
        }),
        "ogv" => Ok(SniffedFormat::Video {
            container: VideoContainer::Ogg,
        }),
        // Audio containers — `AudioContainer` serde names plus aliases.
        "mp3" => Ok(SniffedFormat::Audio {
            container: AudioContainer::Mp3,
        }),
        "wav" | "wave" => Ok(SniffedFormat::Audio {
            container: AudioContainer::Wav,
        }),
        "flac" => Ok(SniffedFormat::Audio {
            container: AudioContainer::Flac,
        }),
        // "ogg" is deliberately the *audio* container — Ogg audio is the
        // common case and `sniff_ogg` defaults ambiguous streams to it;
        // video Ogg is spelled "ogv".
        "ogg" | "oggaudio" | "ogg-audio" => Ok(SniffedFormat::Audio {
            container: AudioContainer::OggAudio,
        }),
        "aac" => Ok(SniffedFormat::Audio {
            container: AudioContainer::Aac,
        }),
        "aiff" | "aif" | "aifc" => Ok(SniffedFormat::Audio {
            container: AudioContainer::Aiff,
        }),
        "au" | "snd" => Ok(SniffedFormat::Audio {
            container: AudioContainer::Au,
        }),
        "amr" => Ok(SniffedFormat::Audio {
            container: AudioContainer::Amr,
        }),
        "caf" => Ok(SniffedFormat::Audio {
            container: AudioContainer::Caf,
        }),
        "ac3" | "eac3" => Ok(SniffedFormat::Audio {
            container: AudioContainer::Ac3,
        }),
        other => Err(StepError::Params(format!(
            "unknown expected_format `{other}` (expected one of: pdf|png|jpeg|jpg|zip|gif|unknown|\
             mp4|mov|matroska|mkv|webm|avi|mpegts|ts|mpegps|mpg|mpeg|flv|asf|wmv|wma|\
             mpegvideo|m2v|mxf|f4v|ogv|mp3|wav|wave|flac|ogg|oggaudio|aac|aiff|aif|aifc|\
             au|snd|amr|caf|ac3|eac3)"
        ))),
    }
}

/// Map a `serde_json` failure to a step error.
fn json_err(e: serde_json::Error) -> StepError {
    StepError::Params(format!("failed to serialize step report: {e}"))
}

/// Outcome for a report step: bytes pass through unchanged.
fn report_step(report: Value) -> StepOutcome {
    StepOutcome {
        artifact: None,
        report: Some(report),
        produced_bytes: 0,
    }
}

/// Outcome for a byte-transform step.
fn bytes_step(artifact: PipelineArtifact) -> StepOutcome {
    let produced_bytes = artifact.len_bytes();
    StepOutcome {
        artifact: Some(artifact),
        report: None,
        produced_bytes,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parse_sniffed_format_accepts_scalar_formats() {
        assert_eq!(parse_sniffed_format("pdf").unwrap(), SniffedFormat::Pdf);
        assert_eq!(parse_sniffed_format("png").unwrap(), SniffedFormat::Png);
        assert_eq!(parse_sniffed_format("jpeg").unwrap(), SniffedFormat::Jpeg);
        assert_eq!(parse_sniffed_format("jpg").unwrap(), SniffedFormat::Jpeg);
        assert_eq!(parse_sniffed_format("zip").unwrap(), SniffedFormat::Zip);
        assert_eq!(parse_sniffed_format("gif").unwrap(), SniffedFormat::Gif);
        assert_eq!(
            parse_sniffed_format("unknown").unwrap(),
            SniffedFormat::Unknown
        );
    }

    #[test]
    fn parse_sniffed_format_accepts_every_video_container() {
        let cases = [
            ("mp4", VideoContainer::Mp4),
            ("mov", VideoContainer::Mp4),
            ("matroska", VideoContainer::Matroska),
            ("mkv", VideoContainer::Matroska),
            ("webm", VideoContainer::Webm),
            ("avi", VideoContainer::Avi),
            ("mpegts", VideoContainer::MpegTs),
            ("ts", VideoContainer::MpegTs),
            ("mpegps", VideoContainer::MpegPs),
            ("mpg", VideoContainer::MpegPs),
            ("flv", VideoContainer::Flv),
            ("asf", VideoContainer::Asf),
            ("wmv", VideoContainer::Asf),
            ("mpegvideo", VideoContainer::MpegVideo),
            ("m2v", VideoContainer::MpegVideo),
            ("mxf", VideoContainer::Mxf),
            ("f4v", VideoContainer::F4v),
            ("ogv", VideoContainer::Ogg),
        ];
        for (name, container) in cases {
            assert_eq!(
                parse_sniffed_format(name).unwrap(),
                SniffedFormat::Video { container },
                "expected_format `{name}`"
            );
        }
    }

    #[test]
    fn parse_sniffed_format_accepts_every_audio_container() {
        let cases = [
            ("mp3", AudioContainer::Mp3),
            ("wav", AudioContainer::Wav),
            ("flac", AudioContainer::Flac),
            ("ogg", AudioContainer::OggAudio),
            ("oggaudio", AudioContainer::OggAudio),
            ("aac", AudioContainer::Aac),
            ("aiff", AudioContainer::Aiff),
            ("au", AudioContainer::Au),
            ("amr", AudioContainer::Amr),
            ("caf", AudioContainer::Caf),
            ("ac3", AudioContainer::Ac3),
            ("eac3", AudioContainer::Ac3),
        ];
        for (name, container) in cases {
            assert_eq!(
                parse_sniffed_format(name).unwrap(),
                SniffedFormat::Audio { container },
                "expected_format `{name}`"
            );
        }
    }

    #[test]
    fn parse_sniffed_format_rejects_unknown_names() {
        let err = parse_sniffed_format("docx").unwrap_err();
        let msg = err.to_string();
        assert!(msg.contains("unknown expected_format `docx`"), "{msg}");
        // The error must enumerate the accepted values.
        assert!(msg.contains("mp4") && msg.contains("mp3"), "{msg}");
    }
}
