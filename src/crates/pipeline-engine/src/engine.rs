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

use serde::de::DeserializeOwned;
use serde_json::Value;

use crate::crates::core::engine::{
    EngineCapabilities, EngineFuture, EngineHealth, EngineHealthStatus, EngineKind, EngineLimits,
    EngineOperation, ProcessingEngine,
};
use crate::crates::pdf_engine::{self as pdf_ops, PdfEngine};
use crate::crates::security_engine::{ResourceLimits, SecurityEngine, SniffedFormat};

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
            // ---- pdf engine: additional report steps ----
            (engine_name::PDF, operation::PDF_PAGE_COUNT) => {
                let count = self.pdf.page_count(input)?;
                Ok(report_step(serde_json::json!({ "page_count": count })))
            }
            (engine_name::PDF, operation::PDF_EXTRACT_METADATA) => {
                let meta = self.pdf.extract_metadata(input)?;
                Ok(report_step(serde_json::to_value(&meta).map_err(json_err)?))
            }
            (engine_name::PDF, operation::PDF_EXTRACT_IMAGES) => {
                let images = self
                    .pdf
                    .extract_images(input, optional_str(&step.params, "pages"))?;
                Ok(report_step(
                    serde_json::to_value(&images).map_err(json_err)?,
                ))
            }
            (engine_name::PDF, operation::PDF_COMPARE) => {
                let other = required_b64(&step.params, "other_input")?;
                let mode = match optional_str(&step.params, "mode") {
                    Some(s) => parse_compare_mode(s)?,
                    None => pdf_ops::CompareMode::Text,
                };
                let report = self.pdf.compare(input, &other, mode)?;
                Ok(StepOutcome {
                    produced_bytes: other.len() as u64,
                    ..report_step(serde_json::to_value(&report).map_err(json_err)?)
                })
            }
            (engine_name::PDF, operation::PDF_OCR) => {
                let texts = self.pdf.ocr(input, optional_str(&step.params, "pages"))?;
                Ok(report_step(serde_json::to_value(&texts).map_err(json_err)?))
            }
            (engine_name::PDF, operation::PDF_OCR_LANGUAGES) => {
                let langs = self.pdf.ocr_languages()?;
                Ok(report_step(serde_json::to_value(&langs).map_err(json_err)?))
            }
            (engine_name::PDF, operation::PDF_VERIFY_SIGNATURES) => {
                let report = self.pdf.verify_signatures(input)?;
                Ok(report_step(
                    serde_json::to_value(&report).map_err(json_err)?,
                ))
            }
            (engine_name::PDF, operation::PDF_VERIFY_SIGNATURES_CRYPTO) => {
                let anchors = optional_b64(&step.params, "trust_anchors")?;
                let report = self
                    .pdf
                    .verify_signatures_crypto(input, anchors.as_deref())?;
                Ok(report_step(
                    serde_json::to_value(&report).map_err(json_err)?,
                ))
            }
            (engine_name::PDF, operation::PDF_LIST_FORM_FIELDS) => {
                let fields = pdf_ops::list_form_fields(input, self.pdf.limits())?;
                Ok(report_step(
                    serde_json::to_value(&fields).map_err(json_err)?,
                ))
            }
            (engine_name::PDF, operation::PDF_LIST_OUTLINE) => {
                let entries = pdf_ops::list_outline(input, self.pdf.limits())?;
                Ok(report_step(
                    serde_json::to_value(&entries).map_err(json_err)?,
                ))
            }
            (engine_name::PDF, operation::PDF_LIST_ANNOTATIONS) => {
                let annots = pdf_ops::list_annotations(input, self.pdf.limits())?;
                Ok(report_step(
                    serde_json::to_value(&annots).map_err(json_err)?,
                ))
            }
            (engine_name::PDF, operation::PDF_LIST_INK) => {
                let ink = pdf_ops::list_ink(input, self.pdf.limits())?;
                Ok(report_step(serde_json::to_value(&ink).map_err(json_err)?))
            }
            (engine_name::PDF, operation::PDF_LIST_ARTICLES) => {
                let articles = pdf_ops::list_articles(input, self.pdf.limits())?;
                Ok(report_step(
                    serde_json::to_value(&articles).map_err(json_err)?,
                ))
            }
            (engine_name::PDF, operation::PDF_LIST_PORTFOLIO) => {
                // PortfolioEntry has no Serialize impl; project the
                // two public fields manually.
                let entries = pdf_ops::list_portfolio(input, self.pdf.limits())?;
                let list: Vec<Value> = entries
                    .iter()
                    .map(|e| serde_json::json!({ "name": e.name, "page": e.page }))
                    .collect();
                Ok(report_step(Value::Array(list)))
            }
            (engine_name::PDF, operation::PDF_GET_PAGE_LABELS) => {
                let labels = pdf_ops::get_page_labels(input, self.pdf.limits())?;
                Ok(report_step(
                    serde_json::to_value(&labels).map_err(json_err)?,
                ))
            }
            (engine_name::PDF, operation::PDF_GET_PAGE_ROTATIONS) => {
                let rotations = pdf_ops::get_page_rotations(input, self.pdf.limits())?;
                let list: Vec<Value> = rotations
                    .iter()
                    .map(|(page, deg)| serde_json::json!({ "page": page, "degrees": deg }))
                    .collect();
                Ok(report_step(Value::Array(list)))
            }
            (engine_name::PDF, operation::PDF_GET_XMP) => {
                let xmp = pdf_ops::get_xmp(input, self.pdf.limits())?;
                Ok(report_step(serde_json::json!({
                    "xmp": String::from_utf8_lossy(&xmp)
                })))
            }
            (engine_name::PDF, operation::PDF_DOCUMENT_DIGEST) => {
                let algorithm = match optional_str(&step.params, "algorithm") {
                    Some(s) => parse_timestamp_algorithm(s)?,
                    None => pdf_ops::TimestampAlgorithm::Sha256,
                };
                let report = pdf_ops::document_digest(input, algorithm, self.pdf.limits())?;
                Ok(report_step(
                    serde_json::to_value(&report).map_err(json_err)?,
                ))
            }
            // ---- pdf engine: multi-input byte transforms ----
            (engine_name::PDF, operation::PDF_MERGE) => {
                let others = other_inputs(&step.params)?;
                let mode = match optional_str(&step.params, "mode") {
                    Some(s) => parse_merge_mode(s)?,
                    None => pdf_ops::MergeMode::Sequential,
                };
                let mut inputs: Vec<&[u8]> = Vec::with_capacity(others.len() + 1);
                inputs.push(input);
                for b in &others {
                    inputs.push(b.as_slice());
                }
                let out = pdf_ops::merge_mode(&inputs, mode, self.pdf.limits())?;
                let produced =
                    out.len() as u64 + others.iter().map(|b| b.len() as u64).sum::<u64>();
                Ok(StepOutcome {
                    produced_bytes: produced,
                    ..bytes_step(PipelineArtifact::Bytes(out))
                })
            }
            (engine_name::PDF, operation::PDF_INTERLEAVE) => {
                let others = required_other_inputs(&step.params)?;
                let mut inputs: Vec<&[u8]> = Vec::with_capacity(others.len() + 1);
                inputs.push(input);
                for b in &others {
                    inputs.push(b.as_slice());
                }
                let out = pdf_ops::interleave(&inputs, self.pdf.limits())?;
                let produced =
                    out.len() as u64 + others.iter().map(|b| b.len() as u64).sum::<u64>();
                Ok(StepOutcome {
                    produced_bytes: produced,
                    ..bytes_step(PipelineArtifact::Bytes(out))
                })
            }
            (engine_name::PDF, operation::PDF_OVERLAY) => {
                let other = required_b64(&step.params, "other_input")?;
                let pages = optional_str(&step.params, "pages");
                let out = self.pdf.overlay(input, &other, pages)?;
                Ok(StepOutcome {
                    produced_bytes: out.len() as u64 + other.len() as u64,
                    ..bytes_step(PipelineArtifact::Bytes(out))
                })
            }
            (engine_name::PDF, operation::PDF_CREATE_PORTFOLIO) => {
                let others = other_inputs(&step.params)?;
                let names = string_array(&step.params, "names").unwrap_or_default();
                let mut inputs: Vec<&[u8]> = Vec::with_capacity(others.len() + 1);
                inputs.push(input);
                for b in &others {
                    inputs.push(b.as_slice());
                }
                let out = pdf_ops::create_portfolio(&inputs, &names, self.pdf.limits())?;
                let produced =
                    out.len() as u64 + others.iter().map(|b| b.len() as u64).sum::<u64>();
                Ok(StepOutcome {
                    produced_bytes: produced,
                    ..bytes_step(PipelineArtifact::Bytes(out))
                })
            }
            // ---- pdf engine: page-structure transforms ----
            (engine_name::PDF, operation::PDF_REMOVE_PAGES) => {
                let pages = required_str(&step.params, "pages")?;
                let out = self.pdf.remove_pages(input, pages)?;
                Ok(bytes_step(PipelineArtifact::Bytes(out)))
            }
            (engine_name::PDF, operation::PDF_REORDER) => {
                let order = required_str(&step.params, "order")?;
                let out = self.pdf.reorder(input, order)?;
                Ok(bytes_step(PipelineArtifact::Bytes(out)))
            }
            (engine_name::PDF, operation::PDF_SPLIT_MODE) => {
                let mode = parse_split_mode(&step.params)?;
                let outs = pdf_ops::split_mode(input, mode, self.pdf.limits())?;
                Ok(bytes_step(PipelineArtifact::Multiple(outs)))
            }
            (engine_name::PDF, operation::PDF_SPLIT_AT_OUTLINE) => {
                let level = required_u32(&step.params, "level")?;
                let outs = pdf_ops::split_at_outline(input, level, self.pdf.limits())?;
                Ok(bytes_step(PipelineArtifact::Multiple(outs)))
            }
            (engine_name::PDF, operation::PDF_INSERT_BLANK) => {
                let position = parse_insert_pos(&step.params)?;
                let count = required_u32(&step.params, "count")?;
                let size = match optional_str(&step.params, "size") {
                    Some(s) => Some(parse_page_size(s)?),
                    None => None,
                };
                let (out, pages) =
                    pdf_ops::insert_blank(input, position, count, size, self.pdf.limits())?;
                Ok(transform_report_step(
                    PipelineArtifact::Bytes(out),
                    serde_json::json!({ "inserted_pages": pages }),
                ))
            }
            (engine_name::PDF, operation::PDF_DUPLICATE_PAGES) => {
                let pages = required_str(&step.params, "pages")?;
                let copies = required_u32(&step.params, "copies")?;
                let position = parse_dup_pos(&step.params)?;
                let (out, inserted) =
                    pdf_ops::duplicate_pages(input, pages, copies, position, self.pdf.limits())?;
                Ok(transform_report_step(
                    PipelineArtifact::Bytes(out),
                    serde_json::json!({ "inserted_pages": inserted }),
                ))
            }
            // ---- pdf engine: marks / content transforms ----
            (engine_name::PDF, operation::PDF_WATERMARK) => {
                let opts = watermark_options(&step.params)?;
                let out = self.pdf.watermark(input, &opts)?;
                Ok(bytes_step(PipelineArtifact::Bytes(out)))
            }
            (engine_name::PDF, operation::PDF_STAMP) => {
                let opts = stamp_options(&step.params)?;
                let out = self.pdf.stamp(input, &opts)?;
                Ok(bytes_step(PipelineArtifact::Bytes(out)))
            }
            (engine_name::PDF, operation::PDF_DRAW_TEXT) => {
                let spec = draw_text_spec(&step.params)?;
                let out = self.pdf.draw_text(input, &spec)?;
                Ok(bytes_step(PipelineArtifact::Bytes(out)))
            }
            (engine_name::PDF, operation::PDF_DRAW_SHAPES) => {
                let spec = draw_spec(&step.params)?;
                let out = pdf_ops::draw_shapes(input, &spec, self.pdf.limits())?;
                Ok(bytes_step(PipelineArtifact::Bytes(out)))
            }
            (engine_name::PDF, operation::PDF_TEXT_MARKUP) => {
                let kind = required_str(&step.params, "kind").and_then(|s| parse_markup_kind(s))?;
                let pages = optional_str(&step.params, "pages");
                let color = optional_u8_array(&step.params, "color")?;
                let out = pdf_ops::text_markup(input, kind, pages, color, self.pdf.limits())?;
                Ok(bytes_step(PipelineArtifact::Bytes(out)))
            }
            (engine_name::PDF, operation::PDF_EDIT_TEXT) => {
                let edit: pdf_ops::TextEdit = spec_from_params(&step.params)?;
                let outcome = pdf_ops::edit_text(input, &edit, self.pdf.limits())?;
                Ok(transform_report_step(
                    PipelineArtifact::Bytes(outcome.bytes),
                    serde_json::to_value(&outcome.report).map_err(json_err)?,
                ))
            }
            (engine_name::PDF, operation::PDF_HEADERS_FOOTERS) => {
                let spec = header_footer_spec(&step.params)?;
                let pages = optional_str(&step.params, "pages");
                let out = pdf_ops::headers_footers(input, &spec, pages, self.pdf.limits())?;
                Ok(bytes_step(PipelineArtifact::Bytes(out)))
            }
            (engine_name::PDF, operation::PDF_ADD_PAGE_NUMBERS) => {
                let style_raw = required_str(&step.params, "style")?;
                let style: pdf_ops::PageNumberStyle =
                    serde_json::from_value(Value::String(style_raw.to_string())).map_err(|e| {
                        StepError::Params(format!("invalid style `{style_raw}`: {e}"))
                    })?;
                let out = pdf_ops::add_page_numbers(input, style, self.pdf.limits())?;
                Ok(bytes_step(PipelineArtifact::Bytes(out)))
            }
            (engine_name::PDF, operation::PDF_NUP) => {
                let cols = required_u32(&step.params, "cols")?;
                let rows = required_u32(&step.params, "rows")?;
                let pages = optional_str(&step.params, "pages");
                let out = pdf_ops::nup(input, cols, rows, pages, self.pdf.limits())?;
                Ok(bytes_step(PipelineArtifact::Bytes(out)))
            }
            (engine_name::PDF, operation::PDF_BOOKLET) => {
                let sheet_size = match optional_str(&step.params, "sheet_size") {
                    Some(s) => parse_booklet_size(s)?,
                    None => pdf_ops::BookletSize::SameAsInput,
                };
                let imposition = match optional_str(&step.params, "imposition") {
                    Some(s) => parse_booklet_mode(s)?,
                    None => pdf_ops::BookletMode::LeftRight,
                };
                let spec = pdf_ops::BookletSpec {
                    sheet_size,
                    imposition,
                };
                let out = pdf_ops::booklet(input, spec, self.pdf.limits())?;
                Ok(bytes_step(PipelineArtifact::Bytes(out)))
            }
            (engine_name::PDF, operation::PDF_SINGLE_PAGE) => {
                let pages = optional_str(&step.params, "pages");
                let cols = optional_u32(&step.params, "cols")?;
                let out = pdf_ops::single_page(input, pages, cols, self.pdf.limits())?;
                Ok(bytes_step(PipelineArtifact::Bytes(out)))
            }
            (engine_name::PDF, operation::PDF_POSTER) => {
                let tile_cols = required_u32(&step.params, "tile_cols")?;
                let tile_rows = required_u32(&step.params, "tile_rows")?;
                let pages = optional_str(&step.params, "pages");
                let out = pdf_ops::poster(input, pages, tile_cols, tile_rows, self.pdf.limits())?;
                Ok(bytes_step(PipelineArtifact::Bytes(out)))
            }
            // ---- pdf engine: geometry / page-box transforms ----
            (engine_name::PDF, operation::PDF_CROP) => {
                let pages = required_str(&step.params, "pages")?;
                let crop_box = rect_param(&step.params, "crop box")?;
                let box4: [f32; 4] = [
                    crop_box[0] as f32,
                    crop_box[1] as f32,
                    crop_box[2] as f32,
                    crop_box[3] as f32,
                ];
                let out = self.pdf.crop(input, pages, box4)?;
                Ok(bytes_step(PipelineArtifact::Bytes(out)))
            }
            (engine_name::PDF, operation::PDF_RESIZE) => {
                let pages = required_str(&step.params, "pages")?;
                let spec = resize_spec(&step.params)?;
                let out = self.pdf.resize(input, pages, &spec)?;
                Ok(bytes_step(PipelineArtifact::Bytes(out)))
            }
            (engine_name::PDF, operation::PDF_SCALE_CONTENT) => {
                let pages = required_str(&step.params, "pages")?;
                let scale_x = required_f64(&step.params, "scale_x")?;
                let scale_y = optional_f64(&step.params, "scale_y");
                let out =
                    pdf_ops::scale_content(input, pages, scale_x, scale_y, self.pdf.limits())?;
                Ok(bytes_step(PipelineArtifact::Bytes(out)))
            }
            (engine_name::PDF, operation::PDF_SET_VIEW_CROP) => {
                let pages = required_str(&step.params, "pages")?;
                let rect = rect_param(&step.params, "crop rect")?;
                let out = pdf_ops::set_view_crop(input, pages, rect, self.pdf.limits())?;
                Ok(bytes_step(PipelineArtifact::Bytes(out)))
            }
            (engine_name::PDF, operation::PDF_CLEAR_CROP) => {
                let pages = required_str(&step.params, "pages")?;
                let out = pdf_ops::clear_crop(input, pages, self.pdf.limits())?;
                Ok(bytes_step(PipelineArtifact::Bytes(out)))
            }
            (engine_name::PDF, operation::PDF_SET_PAGE_ROTATION) => {
                let pages = required_str(&step.params, "pages")?;
                let degrees = required_u32(&step.params, "degrees")?;
                let degrees = u16::try_from(degrees).map_err(|_| {
                    StepError::Params(format!("degrees {degrees} does not fit in u16"))
                })?;
                let out = pdf_ops::set_page_rotation(input, pages, degrees, self.pdf.limits())?;
                Ok(bytes_step(PipelineArtifact::Bytes(out)))
            }
            (engine_name::PDF, operation::PDF_NORMALIZE_ROTATIONS) => {
                let out = pdf_ops::normalize_rotations(input, self.pdf.limits())?;
                Ok(bytes_step(PipelineArtifact::Bytes(out)))
            }
            (engine_name::PDF, operation::PDF_SET_PAGE_LABELS) => {
                let labels = page_labels_param(&step.params)?;
                let out = pdf_ops::set_page_labels(input, &labels, self.pdf.limits())?;
                Ok(bytes_step(PipelineArtifact::Bytes(out)))
            }
            (engine_name::PDF, operation::PDF_CLEAR_PAGE_LABELS) => {
                let out = pdf_ops::clear_page_labels(input, self.pdf.limits())?;
                Ok(bytes_step(PipelineArtifact::Bytes(out)))
            }
            (engine_name::PDF, operation::PDF_GRAYSCALE) => {
                let (out, report) = pdf_ops::grayscale(
                    input,
                    optional_str(&step.params, "pages"),
                    self.pdf.limits(),
                )?;
                Ok(transform_report_step(
                    PipelineArtifact::Bytes(out),
                    serde_json::to_value(&report).map_err(json_err)?,
                ))
            }
            (engine_name::PDF, operation::PDF_INVERT_COLORS) => {
                let (out, report) = pdf_ops::invert_colors(
                    input,
                    optional_str(&step.params, "pages"),
                    self.pdf.limits(),
                )?;
                Ok(transform_report_step(
                    PipelineArtifact::Bytes(out),
                    serde_json::to_value(&report).map_err(json_err)?,
                ))
            }
            // ---- pdf engine: document-level transforms ----
            (engine_name::PDF, operation::PDF_COMPRESS) => {
                let out = self.pdf.compress(input)?;
                Ok(bytes_step(PipelineArtifact::Bytes(out)))
            }
            (engine_name::PDF, operation::PDF_REPAIR) => {
                let report = self.pdf.repair(input)?;
                Ok(transform_report_step(
                    PipelineArtifact::Bytes(report.bytes.clone()),
                    serde_json::to_value(&report).map_err(json_err)?,
                ))
            }
            (engine_name::PDF, operation::PDF_LINEARIZE) => {
                let report = pdf_ops::linearize(input, self.pdf.limits())?;
                Ok(transform_report_step(
                    PipelineArtifact::Bytes(report.bytes.clone()),
                    serde_json::json!({
                        "linearized": report.linearized,
                        "hint_stream": report.hint_stream,
                        "notes": report.notes,
                    }),
                ))
            }
            (engine_name::PDF, operation::PDF_GARBAGE_COLLECT) => {
                let report = pdf_ops::garbage_collect(input, self.pdf.limits())?;
                Ok(transform_report_step(
                    PipelineArtifact::Bytes(report.bytes.clone()),
                    serde_json::json!({
                        "objects_before": report.objects_before,
                        "objects_after": report.objects_after,
                        "objects_pruned": report.objects_pruned,
                        "bytes_before": report.bytes_before,
                        "bytes_after": report.bytes_after,
                        "techniques": report.techniques,
                    }),
                ))
            }
            (engine_name::PDF, operation::PDF_FLATTEN) => {
                let out = self.pdf.flatten(input)?;
                Ok(bytes_step(PipelineArtifact::Bytes(out)))
            }
            (engine_name::PDF, operation::PDF_SANITIZE) => {
                let out = self.pdf.sanitize(input)?;
                Ok(bytes_step(PipelineArtifact::Bytes(out)))
            }
            (engine_name::PDF, operation::PDF_REMOVE_BLANK_PAGES) => {
                let report = pdf_ops::remove_blank_pages(input, self.pdf.limits())?;
                Ok(transform_report_step(
                    PipelineArtifact::Bytes(report.pdf.clone()),
                    serde_json::json!({
                        "removed": report.removed,
                        "kept": report.kept,
                        "total_before": report.total_before,
                        "total_after": report.total_after,
                    }),
                ))
            }
            (engine_name::PDF, operation::PDF_REMOVE_IMAGES) => {
                let report = pdf_ops::remove_images(
                    input,
                    optional_str(&step.params, "pages"),
                    self.pdf.limits(),
                )?;
                let mut v = serde_json::to_value(&report).map_err(json_err)?;
                v.as_object_mut().map(|m| m.remove("bytes"));
                Ok(transform_report_step(
                    PipelineArtifact::Bytes(report.bytes),
                    v,
                ))
            }
            (engine_name::PDF, operation::PDF_REMOVE_EMBEDDED_FILES) => {
                let out = pdf_ops::remove_embedded_files(input, self.pdf.limits())?;
                Ok(bytes_step(PipelineArtifact::Bytes(out)))
            }
            (engine_name::PDF, operation::PDF_REMOVE_JAVASCRIPT_NAMES) => {
                let out = pdf_ops::remove_javascript_names(input, self.pdf.limits())?;
                Ok(bytes_step(PipelineArtifact::Bytes(out)))
            }
            (engine_name::PDF, operation::PDF_REMOVE_SIGNATURES) => {
                let out = self.pdf.remove_signatures(input)?;
                Ok(bytes_step(PipelineArtifact::Bytes(out)))
            }
            (engine_name::PDF, operation::PDF_REMOVE_ANNOTATIONS) => {
                let filter: pdf_ops::AnnotFilter = spec_from_params(&step.params)?;
                let out = pdf_ops::remove_annotations(input, &filter, self.pdf.limits())?;
                Ok(bytes_step(PipelineArtifact::Bytes(out)))
            }
            (engine_name::PDF, operation::PDF_ADD_COMMENT) => {
                let spec: pdf_ops::CommentSpec = spec_from_params(&step.params)?;
                let out = pdf_ops::add_comment(input, &spec, self.pdf.limits())?;
                Ok(bytes_step(PipelineArtifact::Bytes(out)))
            }
            (engine_name::PDF, operation::PDF_ADD_FREETEXT) => {
                let spec: pdf_ops::FreeTextSpec = spec_from_params(&step.params)?;
                let out = pdf_ops::add_freetext(input, &spec, self.pdf.limits())?;
                Ok(bytes_step(PipelineArtifact::Bytes(out)))
            }
            (engine_name::PDF, operation::PDF_ADD_INK) => {
                let spec: pdf_ops::InkSpec = spec_from_params(&step.params)?;
                let out = pdf_ops::add_ink(input, &spec, self.pdf.limits())?;
                Ok(bytes_step(PipelineArtifact::Bytes(out)))
            }
            (engine_name::PDF, operation::PDF_REMOVE_INK) => {
                let filter: pdf_ops::AnnotFilter = spec_from_params(&step.params)?;
                let out = pdf_ops::remove_ink(input, &filter, self.pdf.limits())?;
                Ok(bytes_step(PipelineArtifact::Bytes(out)))
            }
            (engine_name::PDF, operation::PDF_REMOVE_ARTICLES) => {
                let out = pdf_ops::remove_articles(input, self.pdf.limits())?;
                Ok(bytes_step(PipelineArtifact::Bytes(out)))
            }
            // ---- pdf engine: metadata / forms / outline ----
            (engine_name::PDF, operation::PDF_SET_METADATA) => {
                let patch: pdf_ops::MetadataPatch = spec_from_params(&step.params)?;
                let out = pdf_ops::set_metadata(input, &patch, self.pdf.limits())?;
                Ok(bytes_step(PipelineArtifact::Bytes(out)))
            }
            (engine_name::PDF, operation::PDF_CLEAR_METADATA) => {
                let out = pdf_ops::clear_metadata(input, self.pdf.limits())?;
                Ok(bytes_step(PipelineArtifact::Bytes(out)))
            }
            (engine_name::PDF, operation::PDF_SET_XMP) => {
                let xmp = required_b64(&step.params, "xmp")?;
                let out = pdf_ops::set_xmp(input, &xmp, self.pdf.limits())?;
                Ok(bytes_step(PipelineArtifact::Bytes(out)))
            }
            (engine_name::PDF, operation::PDF_FILL_FORM) => {
                let values = form_values_param(&step.params)?;
                let out = pdf_ops::fill_form(input, &values, self.pdf.limits())?;
                Ok(bytes_step(PipelineArtifact::Bytes(out)))
            }
            (engine_name::PDF, operation::PDF_CLEAR_FORM) => {
                let out = pdf_ops::clear_form(input, self.pdf.limits())?;
                Ok(bytes_step(PipelineArtifact::Bytes(out)))
            }
            (engine_name::PDF, operation::PDF_CREATE_TEXT_FIELD) => {
                let spec: pdf_ops::TextFieldSpec = spec_from_params(&step.params)?;
                let out = pdf_ops::create_text_field(input, &spec, self.pdf.limits())?;
                Ok(bytes_step(PipelineArtifact::Bytes(out)))
            }
            (engine_name::PDF, operation::PDF_DELETE_FORM_FIELD) => {
                let name = required_str(&step.params, "name")?;
                let out = pdf_ops::delete_form_field(input, name, self.pdf.limits())?;
                Ok(bytes_step(PipelineArtifact::Bytes(out)))
            }
            (engine_name::PDF, operation::PDF_UNLOCK_FIELDS) => {
                let names = string_array(&step.params, "names");
                let (out, report) =
                    pdf_ops::unlock_fields(input, names.as_deref(), self.pdf.limits())?;
                Ok(transform_report_step(
                    PipelineArtifact::Bytes(out),
                    serde_json::to_value(&report).map_err(json_err)?,
                ))
            }
            (engine_name::PDF, operation::PDF_LOCK_FIELDS) => {
                let names = string_array(&step.params, "names");
                let (out, report) =
                    pdf_ops::lock_fields(input, names.as_deref(), self.pdf.limits())?;
                Ok(transform_report_step(
                    PipelineArtifact::Bytes(out),
                    serde_json::to_value(&report).map_err(json_err)?,
                ))
            }
            (engine_name::PDF, operation::PDF_SET_OUTLINE) => {
                let items_v = step.params.get("items").ok_or_else(|| {
                    StepError::Params("missing required parameter `items`".to_string())
                })?;
                let items: Vec<pdf_ops::OutlineItemSpec> = serde_json::from_value(items_v.clone())
                    .map_err(|e| StepError::Params(format!("invalid outline items: {e}")))?;
                let out = pdf_ops::set_outline(input, &items, self.pdf.limits())?;
                Ok(bytes_step(PipelineArtifact::Bytes(out)))
            }
            (engine_name::PDF, operation::PDF_REMOVE_OUTLINE) => {
                let out = pdf_ops::remove_outline(input, self.pdf.limits())?;
                Ok(bytes_step(PipelineArtifact::Bytes(out)))
            }
            // ---- pdf engine: security / standards transforms ----
            (engine_name::PDF, operation::PDF_ENCRYPT) => {
                let user_password = required_str(&step.params, "user_password")?;
                let owner_password = optional_str(&step.params, "owner_password");
                let permissions: pdf_ops::PdfPermissions = match step.params.get("permissions") {
                    Some(v) if !v.is_null() => serde_json::from_value(v.clone())
                        .map_err(|e| StepError::Params(format!("invalid permissions: {e}")))?,
                    _ => pdf_ops::PdfPermissions::default(),
                };
                let out = self
                    .pdf
                    .encrypt(input, user_password, owner_password, permissions)?;
                Ok(bytes_step(PipelineArtifact::Bytes(out)))
            }
            (engine_name::PDF, operation::PDF_DECRYPT) => {
                let password = required_str(&step.params, "password")?;
                let out = self.pdf.decrypt(input, password)?;
                Ok(bytes_step(PipelineArtifact::Bytes(out)))
            }
            (engine_name::PDF, operation::PDF_REDACT) => {
                let terms = required_string_array(&step.params, "terms")?;
                let pages = optional_str(&step.params, "pages");
                let out = self.pdf.redact(input, &terms, pages)?;
                Ok(bytes_step(PipelineArtifact::Bytes(out)))
            }
            (engine_name::PDF, operation::PDF_SIGN) => {
                let credentials: pdf_ops::SignCredentials =
                    spec_from_params_key(&step.params, "credentials")?;
                let options: pdf_ops::SignOptions = spec_from_params(&step.params)?;
                let out = self.pdf.sign(input, &credentials, &options)?;
                Ok(bytes_step(PipelineArtifact::Bytes(out)))
            }
            (engine_name::PDF, operation::PDF_PDFA) => {
                let level = required_str(&step.params, "level").and_then(parse_pdfa_level)?;
                let report = pdf_ops::to_pdfa(input, level, self.pdf.limits())?;
                let mut v = serde_json::to_value(&report).map_err(json_err)?;
                v.as_object_mut().map(|m| m.remove("bytes"));
                Ok(transform_report_step(
                    PipelineArtifact::Bytes(report.bytes),
                    v,
                ))
            }
            (engine_name::PDF, operation::PDF_PDFUA) => {
                let level = required_str(&step.params, "level").and_then(parse_pdfua_level)?;
                let report = pdf_ops::to_pdfua(input, level, self.pdf.limits())?;
                let mut v = serde_json::to_value(&report).map_err(json_err)?;
                v.as_object_mut().map(|m| m.remove("bytes"));
                Ok(transform_report_step(
                    PipelineArtifact::Bytes(report.bytes),
                    v,
                ))
            }
            (engine_name::PDF, operation::PDF_PDFX) => {
                let level = required_str(&step.params, "level").and_then(parse_pdfx_level)?;
                let report = pdf_ops::to_pdfx(input, level, self.pdf.limits())?;
                let mut v = serde_json::to_value(&report).map_err(json_err)?;
                v.as_object_mut().map(|m| m.remove("bytes"));
                Ok(transform_report_step(
                    PipelineArtifact::Bytes(report.bytes),
                    v,
                ))
            }
            // ---- pdf engine: output-format transforms ----
            (engine_name::PDF, operation::PDF_CONVERT) => {
                let target_raw = required_str(&step.params, "target")?;
                let target: pdf_ops::ConvertTarget =
                    serde_json::from_value(Value::String(target_raw.to_string())).map_err(|e| {
                        StepError::Params(format!("invalid target `{target_raw}`: {e}"))
                    })?;
                let pages = optional_str(&step.params, "pages");
                let out = self.pdf.convert(input, target, pages)?;
                Ok(bytes_step(PipelineArtifact::Bytes(out)))
            }
            (engine_name::PDF, operation::PDF_RENDER) => {
                let pages = optional_str(&step.params, "pages");
                let dpi = optional_u32(&step.params, "dpi")?;
                let format = match optional_str(&step.params, "format") {
                    Some(s) => parse_render_format(s)?,
                    None => pdf_ops::RenderFormat::Png,
                };
                let out = self.pdf.render(input, pages, dpi, format)?;
                Ok(bytes_step(PipelineArtifact::Bytes(out)))
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
            // The union of operations dispatchable through a pipeline —
            // every `EngineOperation` the pdf engine exposes that a
            // single-stream pipeline can express. Multi-input ops
            // (`merge`, `overlay`, `compare`, `interleave`,
            // `create_portfolio`) take their extra documents as
            // base64 `params`, so they are included.
            operations: vec![
                EngineOperation::Merge,
                EngineOperation::Split,
                EngineOperation::Rotate,
                EngineOperation::Resize,
                EngineOperation::Crop,
                EngineOperation::Reorder,
                EngineOperation::RemovePages,
                EngineOperation::ExtractPages,
                EngineOperation::Compress,
                EngineOperation::Repair,
                EngineOperation::Flatten,
                EngineOperation::Overlay,
                EngineOperation::Watermark,
                EngineOperation::Stamp,
                EngineOperation::DrawText,
                EngineOperation::Redact,
                EngineOperation::Sanitize,
                EngineOperation::Sign,
                EngineOperation::RemoveSignatures,
                EngineOperation::VerifySignature,
                EngineOperation::Validate,
                EngineOperation::Inspect,
                EngineOperation::PageCount,
                EngineOperation::ExtractMetadata,
                EngineOperation::ExtractImages,
                EngineOperation::ExtractText,
                EngineOperation::Render,
                EngineOperation::Compare,
                EngineOperation::Convert,
                EngineOperation::Ocr,
                EngineOperation::Encrypt,
                EngineOperation::Decrypt,
            ],
            input_formats: vec!["pdf".to_string()],
            // Most steps keep the stream a PDF; `convert` emits
            // text/markdown and `render` emits an image.
            output_formats: vec![
                "pdf".to_string(),
                "text".to_string(),
                "markdown".to_string(),
                "png".to_string(),
                "jpeg".to_string(),
            ],
            limits: EngineLimits {
                max_input_bytes: Some(self.limits.max_input_bytes),
                max_pages: self.limits.max_pages,
                max_pixels: None,
                max_outputs: self.limits.max_outputs,
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

/// `pdf.*` operations [`PipelineEngine::dispatch`] handles. Must stay
/// in sync with the match arms — `check_definition` uses it for
/// up-front validation. Ordered roughly as in the `operation` module.
const PDF_OPERATIONS: &[&str] = &[
    operation::PDF_VALIDATE,
    operation::PDF_INSPECT,
    operation::PDF_PAGE_COUNT,
    operation::PDF_EXTRACT_TEXT,
    operation::PDF_EXTRACT_METADATA,
    operation::PDF_EXTRACT_IMAGES,
    operation::PDF_EXTRACT_PAGES,
    operation::PDF_REMOVE_PAGES,
    operation::PDF_REORDER,
    operation::PDF_ROTATE,
    operation::PDF_SPLIT,
    operation::PDF_SPLIT_MODE,
    operation::PDF_SPLIT_AT_OUTLINE,
    operation::PDF_MERGE,
    operation::PDF_INTERLEAVE,
    operation::PDF_OVERLAY,
    operation::PDF_COMPARE,
    operation::PDF_CREATE_PORTFOLIO,
    operation::PDF_WATERMARK,
    operation::PDF_STAMP,
    operation::PDF_DRAW_TEXT,
    operation::PDF_DRAW_SHAPES,
    operation::PDF_TEXT_MARKUP,
    operation::PDF_EDIT_TEXT,
    operation::PDF_HEADERS_FOOTERS,
    operation::PDF_ADD_PAGE_NUMBERS,
    operation::PDF_NUP,
    operation::PDF_BOOKLET,
    operation::PDF_SINGLE_PAGE,
    operation::PDF_POSTER,
    operation::PDF_INSERT_BLANK,
    operation::PDF_DUPLICATE_PAGES,
    operation::PDF_CROP,
    operation::PDF_RESIZE,
    operation::PDF_SCALE_CONTENT,
    operation::PDF_SET_VIEW_CROP,
    operation::PDF_CLEAR_CROP,
    operation::PDF_SET_PAGE_ROTATION,
    operation::PDF_NORMALIZE_ROTATIONS,
    operation::PDF_GET_PAGE_ROTATIONS,
    operation::PDF_SET_PAGE_LABELS,
    operation::PDF_CLEAR_PAGE_LABELS,
    operation::PDF_GET_PAGE_LABELS,
    operation::PDF_GRAYSCALE,
    operation::PDF_INVERT_COLORS,
    operation::PDF_COMPRESS,
    operation::PDF_REPAIR,
    operation::PDF_LINEARIZE,
    operation::PDF_GARBAGE_COLLECT,
    operation::PDF_FLATTEN,
    operation::PDF_SANITIZE,
    operation::PDF_REMOVE_BLANK_PAGES,
    operation::PDF_REMOVE_IMAGES,
    operation::PDF_REMOVE_EMBEDDED_FILES,
    operation::PDF_REMOVE_JAVASCRIPT_NAMES,
    operation::PDF_REMOVE_SIGNATURES,
    operation::PDF_REMOVE_ANNOTATIONS,
    operation::PDF_LIST_ANNOTATIONS,
    operation::PDF_ADD_COMMENT,
    operation::PDF_ADD_FREETEXT,
    operation::PDF_ADD_INK,
    operation::PDF_LIST_INK,
    operation::PDF_REMOVE_INK,
    operation::PDF_LIST_ARTICLES,
    operation::PDF_REMOVE_ARTICLES,
    operation::PDF_SET_METADATA,
    operation::PDF_CLEAR_METADATA,
    operation::PDF_SET_XMP,
    operation::PDF_GET_XMP,
    operation::PDF_ENCRYPT,
    operation::PDF_DECRYPT,
    operation::PDF_REDACT,
    operation::PDF_VERIFY_SIGNATURES,
    operation::PDF_VERIFY_SIGNATURES_CRYPTO,
    operation::PDF_SIGN,
    operation::PDF_LIST_FORM_FIELDS,
    operation::PDF_FILL_FORM,
    operation::PDF_CLEAR_FORM,
    operation::PDF_CREATE_TEXT_FIELD,
    operation::PDF_DELETE_FORM_FIELD,
    operation::PDF_UNLOCK_FIELDS,
    operation::PDF_LOCK_FIELDS,
    operation::PDF_SET_OUTLINE,
    operation::PDF_REMOVE_OUTLINE,
    operation::PDF_LIST_OUTLINE,
    operation::PDF_PDFA,
    operation::PDF_PDFUA,
    operation::PDF_PDFX,
    operation::PDF_CONVERT,
    operation::PDF_RENDER,
    operation::PDF_OCR,
    operation::PDF_OCR_LANGUAGES,
    operation::PDF_DOCUMENT_DIGEST,
];

/// `(engine, operation)` pairs [`PipelineEngine::dispatch`] handles.
/// Must stay in sync with the match arms — `check_definition` uses it
/// for up-front validation.
fn is_supported(engine: &str, operation_name: &str) -> bool {
    match engine {
        engine_name::SECURITY => matches!(
            operation_name,
            operation::VALIDATE | operation::VALIDATE_UPLOAD
        ),
        engine_name::PDF => PDF_OPERATIONS.contains(&operation_name),
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
fn parse_sniffed_format(s: &str) -> Result<SniffedFormat, StepError> {
    match s {
        "pdf" => Ok(SniffedFormat::Pdf),
        "png" => Ok(SniffedFormat::Png),
        "jpeg" | "jpg" => Ok(SniffedFormat::Jpeg),
        "zip" => Ok(SniffedFormat::Zip),
        "gif" => Ok(SniffedFormat::Gif),
        "unknown" => Ok(SniffedFormat::Unknown),
        other => Err(StepError::Params(format!(
            "unknown expected_format `{other}` (expected pdf|png|jpeg|zip|gif|unknown)"
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

/// Outcome for a step that both transforms the artifact and reports
/// (e.g. `repair`, `pdfa`, `grayscale`).
fn transform_report_step(artifact: PipelineArtifact, report: Value) -> StepOutcome {
    let produced_bytes = artifact.len_bytes();
    StepOutcome {
        artifact: Some(artifact),
        report: Some(report),
        produced_bytes,
    }
}

// ---- typed param helpers ----

/// Optional finite/any f64 param.
fn optional_f64(params: &Value, key: &str) -> Option<f64> {
    params.get(key).and_then(Value::as_f64)
}

/// Required f64 param.
fn required_f64(params: &Value, key: &str) -> Result<f64, StepError> {
    optional_f64(params, key)
        .ok_or_else(|| StepError::Params(format!("missing required number parameter `{key}`")))
}

/// Optional u32 param (accepts any JSON unsigned integer in range).
fn optional_u32(params: &Value, key: &str) -> Result<Option<u32>, StepError> {
    match params.get(key) {
        None | Some(Value::Null) => Ok(None),
        Some(v) => {
            let n = v.as_u64().ok_or_else(|| {
                StepError::Params(format!("parameter `{key}` must be an unsigned integer"))
            })?;
            u32::try_from(n).map(Some).map_err(|_| {
                StepError::Params(format!("parameter `{key}` value {n} does not fit in u32"))
            })
        }
    }
}

/// Required u32 param.
fn required_u32(params: &Value, key: &str) -> Result<u32, StepError> {
    optional_u32(params, key)?
        .ok_or_else(|| StepError::Params(format!("missing required integer parameter `{key}`")))
}

/// Optional array-of-strings param; `None` when absent or JSON null.
fn string_array(params: &Value, key: &str) -> Option<Vec<String>> {
    params.get(key).and_then(Value::as_array).map(|arr| {
        arr.iter()
            .filter_map(|v| v.as_str().map(str::to_string))
            .collect()
    })
}

/// Required array-of-strings param.
fn required_string_array(params: &Value, key: &str) -> Result<Vec<String>, StepError> {
    string_array(params, key).ok_or_else(|| {
        StepError::Params(format!("missing required string-array parameter `{key}`"))
    })
}

/// Optional base64-decoded param; `None` when absent or JSON null.
fn optional_b64(params: &Value, key: &str) -> Result<Option<Vec<u8>>, StepError> {
    match params.get(key) {
        None | Some(Value::Null) => Ok(None),
        Some(Value::String(s)) => super::b64_serde::decode(s)
            .map(Some)
            .map_err(|e| StepError::Params(format!("parameter `{key}` is not valid base64: {e}"))),
        Some(_) => Err(StepError::Params(format!(
            "parameter `{key}` must be a base64 string"
        ))),
    }
}

/// Required base64-decoded param.
fn required_b64(params: &Value, key: &str) -> Result<Vec<u8>, StepError> {
    optional_b64(params, key)?
        .ok_or_else(|| StepError::Params(format!("missing required base64 parameter `{key}`")))
}

/// Decode the `other_inputs` param: an array of base64-encoded
/// documents appended after the pipeline's byte stream. Absent/null
/// yields an empty list.
fn other_inputs(params: &Value) -> Result<Vec<Vec<u8>>, StepError> {
    let Some(arr) = params.get("other_inputs").and_then(Value::as_array) else {
        return Ok(Vec::new());
    };
    let mut out = Vec::with_capacity(arr.len());
    for (i, item) in arr.iter().enumerate() {
        let s = item.as_str().ok_or_else(|| {
            StepError::Params(format!("other_inputs[{i}] must be a base64 string"))
        })?;
        out.push(super::b64_serde::decode(s).map_err(|e| {
            StepError::Params(format!("other_inputs[{i}] is not valid base64: {e}"))
        })?);
    }
    Ok(out)
}

/// `other_inputs` that must contain at least one document — used by
/// ops requiring ≥2 total inputs (`interleave`).
fn required_other_inputs(params: &Value) -> Result<Vec<Vec<u8>>, StepError> {
    let others = other_inputs(params)?;
    if others.is_empty() {
        return Err(StepError::Params(
            "this operation requires `other_inputs` with at least one base64 document".to_string(),
        ));
    }
    Ok(others)
}

/// Deserialize `params` itself as a spec type (unknown keys ignored —
/// each spec only reads the fields it declares).
fn spec_from_params<T: DeserializeOwned>(params: &Value) -> Result<T, StepError> {
    serde_json::from_value(params.clone())
        .map_err(|e| StepError::Params(format!("invalid step parameters: {e}")))
}

/// Deserialize the `params[key]` sub-object as a spec type.
fn spec_from_params_key<T: DeserializeOwned>(params: &Value, key: &str) -> Result<T, StepError> {
    let v = params
        .get(key)
        .ok_or_else(|| StepError::Params(format!("missing required parameter `{key}`")))?;
    serde_json::from_value(v.clone())
        .map_err(|e| StepError::Params(format!("invalid `{key}` parameter: {e}")))
}

/// Optional `[u8; 3]` RGB param.
fn optional_u8_array(params: &Value, key: &str) -> Result<Option<[u8; 3]>, StepError> {
    match params.get(key) {
        None | Some(Value::Null) => Ok(None),
        Some(v) => {
            let arr = v.as_array().ok_or_else(|| {
                StepError::Params(format!("parameter `{key}` must be an array of 3 integers"))
            })?;
            if arr.len() != 3 {
                return Err(StepError::Params(format!(
                    "parameter `{key}` must have exactly 3 components"
                )));
            }
            let mut out = [0u8; 3];
            for (i, item) in arr.iter().enumerate() {
                let n = item.as_u64().ok_or_else(|| {
                    StepError::Params(format!("parameter `{key}[{i}]` must be an integer"))
                })?;
                out[i] = u8::try_from(n).map_err(|_| {
                    StepError::Params(format!(
                        "parameter `{key}[{i}]` value {n} does not fit in u8"
                    ))
                })?;
            }
            Ok(Some(out))
        }
    }
}

/// A `[f64; 4]` rectangle param: either a `box`/`rect` array of four
/// numbers or `x0`/`y0`/`x1`/`y1` keys.
fn rect_param(params: &Value, what: &str) -> Result<[f64; 4], StepError> {
    for key in ["box", "rect"] {
        if let Some(arr) = params.get(key).and_then(Value::as_array) {
            if arr.len() == 4 {
                let mut out = [0.0f64; 4];
                for (i, item) in arr.iter().enumerate() {
                    out[i] = item.as_f64().ok_or_else(|| {
                        StepError::Params(format!("{what}[{i}] must be a number"))
                    })?;
                }
                return Ok(out);
            }
        }
    }
    let x0 = required_f64(params, "x0")?;
    let y0 = required_f64(params, "y0")?;
    let x1 = required_f64(params, "x1")?;
    let y1 = required_f64(params, "y1")?;
    let _ = what;
    Ok([x0, y0, x1, y1])
}

/// `labels` param for `set_page_labels`: `[{"page": n, "label": "..."}]`.
fn page_labels_param(params: &Value) -> Result<Vec<(u32, String)>, StepError> {
    let arr = params
        .get("labels")
        .and_then(Value::as_array)
        .ok_or_else(|| {
            StepError::Params(
                "missing required parameter `labels` (array of {page,label})".to_string(),
            )
        })?;
    let mut out = Vec::with_capacity(arr.len());
    for (i, item) in arr.iter().enumerate() {
        let page = item
            .get("page")
            .and_then(Value::as_u64)
            .and_then(|n| u32::try_from(n).ok())
            .ok_or_else(|| {
                StepError::Params(format!("labels[{i}].page must be a positive integer"))
            })?;
        let label = item
            .get("label")
            .and_then(Value::as_str)
            .ok_or_else(|| StepError::Params(format!("labels[{i}].label must be a string")))?
            .to_string();
        out.push((page, label));
    }
    Ok(out)
}

/// `values` param for `fill_form`: an object `{"field": "value"}` or an
/// array of `{"name": ..., "value": ...}` / `[name, value]` pairs.
fn form_values_param(params: &Value) -> Result<Vec<(String, String)>, StepError> {
    let v = params
        .get("values")
        .ok_or_else(|| StepError::Params("missing required parameter `values`".to_string()))?;
    let mut out = Vec::new();
    match v {
        Value::Object(map) => {
            for (k, val) in map {
                let s = val
                    .as_str()
                    .ok_or_else(|| StepError::Params(format!("values.{k} must be a string")))?;
                out.push((k.clone(), s.to_string()));
            }
        }
        Value::Array(arr) => {
            for (i, item) in arr.iter().enumerate() {
                match item {
                    Value::Object(map) => {
                        let name = map.get("name").and_then(Value::as_str).ok_or_else(|| {
                            StepError::Params(format!("values[{i}].name must be a string"))
                        })?;
                        let value = map.get("value").and_then(Value::as_str).ok_or_else(|| {
                            StepError::Params(format!("values[{i}].value must be a string"))
                        })?;
                        out.push((name.to_string(), value.to_string()));
                    }
                    Value::Array(pair) if pair.len() == 2 => {
                        let name = pair[0].as_str().ok_or_else(|| {
                            StepError::Params(format!("values[{i}][0] must be a string"))
                        })?;
                        let value = pair[1].as_str().ok_or_else(|| {
                            StepError::Params(format!("values[{i}][1] must be a string"))
                        })?;
                        out.push((name.to_string(), value.to_string()));
                    }
                    _ => {
                        return Err(StepError::Params(format!(
                            "values[{i}] must be an object or a [name, value] pair"
                        )));
                    }
                }
            }
        }
        _ => {
            return Err(StepError::Params(
                "parameter `values` must be an object or an array".to_string(),
            ));
        }
    }
    Ok(out)
}

// ---- spec builders for ops without serde types ----

/// `watermark` params → [`pdf_ops::WatermarkOptions`].
fn watermark_options(params: &Value) -> Result<pdf_ops::WatermarkOptions, StepError> {
    let mut opts = pdf_ops::WatermarkOptions::default();
    opts.text = required_str(params, "text")?.to_string();
    if let Some(v) = optional_f64(params, "font_size") {
        opts.font_size = v as f32;
    }
    if let Some(v) = optional_f64(params, "opacity") {
        opts.opacity = v as f32;
    }
    if let Some(v) = optional_f64(params, "rotation_degrees") {
        opts.rotation_degrees = v as f32;
    }
    if let Some(s) = optional_str(params, "position") {
        opts.position = parse_mark_position(s)?;
    }
    if let Some(s) = optional_str(params, "pages") {
        opts.pages = Some(s.to_string());
    }
    opts.color = optional_u8_array(params, "color")?;
    Ok(opts)
}

/// `stamp` params → [`pdf_ops::StampOptions`]. Exactly one of `text` /
/// `image_png` (base64) must be present; the op itself rejects neither
/// or both.
fn stamp_options(params: &Value) -> Result<pdf_ops::StampOptions, StepError> {
    let mut opts = pdf_ops::StampOptions::default();
    opts.text = optional_str(params, "text").map(str::to_string);
    opts.image_png = optional_b64(params, "image_png")?;
    if let Some(s) = optional_str(params, "position") {
        opts.position = parse_mark_position(s)?;
    }
    if let Some(v) = optional_f64(params, "scale") {
        opts.scale = v as f32;
    }
    if let Some(v) = optional_f64(params, "opacity") {
        opts.opacity = v as f32;
    }
    if let Some(v) = optional_f64(params, "margin") {
        opts.margin = v as f32;
    }
    if let Some(s) = optional_str(params, "pages") {
        opts.pages = Some(s.to_string());
    }
    Ok(opts)
}

/// `draw_text` params → [`pdf_ops::DrawTextSpec`].
fn draw_text_spec(params: &Value) -> Result<pdf_ops::DrawTextSpec, StepError> {
    let mut spec = pdf_ops::DrawTextSpec::default();
    spec.text = required_str(params, "text")?.to_string();
    spec.x = required_f64(params, "x")? as f32;
    spec.y = required_f64(params, "y")? as f32;
    if let Some(v) = optional_f64(params, "font_size") {
        spec.font_size = v as f32;
    }
    if let Some(arr) = params.get("color").and_then(Value::as_array) {
        if arr.len() == 3 {
            let mut c = [0f32; 3];
            for (i, item) in arr.iter().enumerate() {
                c[i] = item.as_f64().ok_or_else(|| {
                    StepError::Params(format!("color[{i}] must be a number in 0..=1"))
                })? as f32;
            }
            spec.color = Some(c);
        } else {
            return Err(StepError::Params(
                "parameter `color` must be an array of 3 numbers".to_string(),
            ));
        }
    }
    if let Some(v) = optional_f64(params, "opacity") {
        spec.opacity = Some(v as f32);
    }
    if let Some(v) = optional_f64(params, "rotation") {
        spec.rotation = Some(v as f32);
    }
    if let Some(s) = optional_str(params, "pages") {
        spec.pages = Some(s.to_string());
    }
    Ok(spec)
}

/// `draw_shapes` params → [`pdf_ops::DrawSpec`]. Each shape is an
/// object `{"type": "rectangle"|"line"|"ellipse"|"arrow", ...coords,
/// "style": {...}}`; `style` fields default per [`pdf_ops::ShapeStyle`].
fn draw_spec(params: &Value) -> Result<pdf_ops::DrawSpec, StepError> {
    let mut spec = pdf_ops::DrawSpec::default();
    if let Some(s) = optional_str(params, "pages") {
        spec.pages = Some(s.to_string());
    }
    let arr = params
        .get("shapes")
        .and_then(Value::as_array)
        .ok_or_else(|| {
            StepError::Params("missing required parameter `shapes` (array)".to_string())
        })?;
    for (i, item) in arr.iter().enumerate() {
        spec.shapes.push(parse_shape(item, i)?);
    }
    Ok(spec)
}

/// Parse one shape object for [`draw_spec`].
fn parse_shape(item: &Value, index: usize) -> Result<pdf_ops::Shape, StepError> {
    let err = |m: &str| StepError::Params(format!("shapes[{index}]: {m}"));
    let kind = item
        .get("type")
        .and_then(Value::as_str)
        .ok_or_else(|| err("missing `type`"))?;
    let style = match item.get("style") {
        Some(v) if !v.is_null() => parse_shape_style(v, index)?,
        _ => pdf_ops::ShapeStyle::default(),
    };
    let num = |key: &str| -> Result<f32, StepError> {
        item.get(key)
            .and_then(Value::as_f64)
            .map(|v| v as f32)
            .ok_or_else(|| err(&format!("missing or non-numeric `{key}`")))
    };
    match kind {
        "rectangle" => Ok(pdf_ops::Shape::Rectangle {
            x: num("x")?,
            y: num("y")?,
            width: num("width")?,
            height: num("height")?,
            style,
        }),
        "line" => Ok(pdf_ops::Shape::Line {
            x1: num("x1")?,
            y1: num("y1")?,
            x2: num("x2")?,
            y2: num("y2")?,
            style,
        }),
        "ellipse" => Ok(pdf_ops::Shape::Ellipse {
            cx: num("cx")?,
            cy: num("cy")?,
            rx: num("rx")?,
            ry: num("ry")?,
            style,
        }),
        "arrow" => Ok(pdf_ops::Shape::Arrow {
            x1: num("x1")?,
            y1: num("y1")?,
            x2: num("x2")?,
            y2: num("y2")?,
            head_length: match item.get("head_length") {
                Some(v) if !v.is_null() => Some(
                    v.as_f64()
                        .ok_or_else(|| err("`head_length` must be a number"))?
                        as f32,
                ),
                _ => None,
            },
            style,
        }),
        other => Err(err(&format!(
            "unknown shape type `{other}` (expected rectangle|line|ellipse|arrow)"
        ))),
    }
}

/// Parse a `style` object for [`parse_shape`].
fn parse_shape_style(item: &Value, index: usize) -> Result<pdf_ops::ShapeStyle, StepError> {
    let err = |m: &str| StepError::Params(format!("shapes[{index}].style: {m}"));
    let mut style = pdf_ops::ShapeStyle::default();
    let rgb = |key: &str| -> Result<Option<[u8; 3]>, StepError> {
        match item.get(key) {
            Some(v) if !v.is_null() => {
                let arr = v
                    .as_array()
                    .ok_or_else(|| err(&format!("`{key}` must be an array of 3 integers")))?;
                if arr.len() != 3 {
                    return Err(err(&format!("`{key}` must have exactly 3 components")));
                }
                let mut out = [0u8; 3];
                for (i, c) in arr.iter().enumerate() {
                    let n = c
                        .as_u64()
                        .ok_or_else(|| err(&format!("`{key}[{i}]` must be an integer")))?;
                    out[i] = u8::try_from(n)
                        .map_err(|_| err(&format!("`{key}[{i}]` value {n} does not fit in u8")))?;
                }
                Ok(Some(out))
            }
            _ => Ok(None),
        }
    };
    if let Some(c) = rgb("stroke")? {
        style.stroke = c;
    }
    style.fill = rgb("fill")?;
    if let Some(v) = item.get("line_width").and_then(Value::as_f64) {
        style.line_width = v as f32;
    }
    if let Some(v) = item.get("opacity").and_then(Value::as_f64) {
        style.opacity = Some(v as f32);
    }
    Ok(style)
}

/// `headers_footers` params → [`pdf_ops::HeaderFooterSpec`].
fn header_footer_spec(params: &Value) -> Result<pdf_ops::HeaderFooterSpec, StepError> {
    let mut spec = pdf_ops::HeaderFooterSpec {
        header: None,
        footer: None,
        page_numbers: pdf_ops::PageNumberStyle::None,
        font_size: None,
        margin: None,
    };
    if let Some(s) = optional_str(params, "header") {
        spec.header = Some(s.to_string());
    }
    if let Some(s) = optional_str(params, "footer") {
        spec.footer = Some(s.to_string());
    }
    if let Some(s) = optional_str(params, "page_numbers") {
        spec.page_numbers = serde_json::from_value(Value::String(s.to_string()))
            .map_err(|e| {
                StepError::Params(format!(
                    "invalid page_numbers `{s}` (expected none|bottom_center|bottom_right|bottom_left): {e}"
                ))
            })?;
    }
    if let Some(v) = optional_f64(params, "font_size") {
        spec.font_size = Some(v as f32);
    }
    if let Some(v) = optional_f64(params, "margin") {
        spec.margin = Some(v as f32);
    }
    Ok(spec)
}

/// `resize` params → [`pdf_ops::ResizeSpec`]: `scale` for uniform
/// scaling, otherwise `width_pt` + `height_pt`.
fn resize_spec(params: &Value) -> Result<pdf_ops::ResizeSpec, StepError> {
    if let Some(s) = optional_f64(params, "scale") {
        return Ok(pdf_ops::ResizeSpec::Scale(s));
    }
    let width = required_f64(params, "width_pt")?;
    let height = required_f64(params, "height_pt")?;
    Ok(pdf_ops::ResizeSpec::Size {
        width_pt: width,
        height_pt: height,
    })
}

// ---- small enum parsers ----

fn parse_mark_position(s: &str) -> Result<pdf_ops::MarkPosition, StepError> {
    serde_json::from_value(Value::String(s.to_string())).map_err(|_| {
        StepError::Params(format!(
            "unknown position `{s}` (expected center|top_left|top_right|bottom_left|bottom_right|top_center|bottom_center)"
        ))
    })
}

fn parse_compare_mode(s: &str) -> Result<pdf_ops::CompareMode, StepError> {
    serde_json::from_value(Value::String(s.to_string())).map_err(|_| {
        StepError::Params(format!(
            "unknown compare mode `{s}` (expected text|structure|bytes)"
        ))
    })
}

fn parse_merge_mode(s: &str) -> Result<pdf_ops::MergeMode, StepError> {
    match s {
        "sequential" => Ok(pdf_ops::MergeMode::Sequential),
        "interleave" => Ok(pdf_ops::MergeMode::Interleave),
        "reverse" => Ok(pdf_ops::MergeMode::Reverse),
        other => Err(StepError::Params(format!(
            "unknown merge mode `{other}` (expected sequential|interleave|reverse)"
        ))),
    }
}

fn parse_markup_kind(s: &str) -> Result<pdf_ops::MarkupKind, StepError> {
    serde_json::from_value(Value::String(s.to_string())).map_err(|_| {
        StepError::Params(format!(
            "unknown markup kind `{s}` (expected highlight|underline|strike)"
        ))
    })
}

fn parse_render_format(s: &str) -> Result<pdf_ops::RenderFormat, StepError> {
    serde_json::from_value(Value::String(s.to_string()))
        .map_err(|_| StepError::Params(format!("unknown render format `{s}` (expected png|jpeg)")))
}

fn parse_timestamp_algorithm(s: &str) -> Result<pdf_ops::TimestampAlgorithm, StepError> {
    match s {
        "sha256" | "SHA256" | "Sha256" => Ok(pdf_ops::TimestampAlgorithm::Sha256),
        "sha512" | "SHA512" | "Sha512" => Ok(pdf_ops::TimestampAlgorithm::Sha512),
        other => Err(StepError::Params(format!(
            "unknown digest algorithm `{other}` (expected sha256|sha512)"
        ))),
    }
}

fn parse_pdfa_level(s: &str) -> Result<pdf_ops::PdfaLevel, StepError> {
    match s.to_ascii_lowercase().as_str() {
        "1b" | "pdfa1b" | "pdfa-1b" | "pdf/a-1b" => Ok(pdf_ops::PdfaLevel::PdfA1b),
        "2b" | "pdfa2b" | "pdfa-2b" | "pdf/a-2b" => Ok(pdf_ops::PdfaLevel::PdfA2b),
        "3b" | "pdfa3b" | "pdfa-3b" | "pdf/a-3b" => Ok(pdf_ops::PdfaLevel::PdfA3b),
        other => Err(StepError::Params(format!(
            "unknown PDF/A level `{other}` (expected 1b|2b|3b)"
        ))),
    }
}

fn parse_pdfua_level(s: &str) -> Result<pdf_ops::PdfuaLevel, StepError> {
    match s.to_ascii_lowercase().as_str() {
        "1" | "ua1" | "pdfua1" | "pdfua-1" | "pdf/ua-1" => Ok(pdf_ops::PdfuaLevel::PdfUa1),
        other => Err(StepError::Params(format!(
            "unknown PDF/UA level `{other}` (expected 1|ua1)"
        ))),
    }
}

fn parse_pdfx_level(s: &str) -> Result<pdf_ops::PdfxLevel, StepError> {
    match s.to_ascii_lowercase().as_str() {
        "1a" | "pdfx1a" | "pdfx-1a" | "pdf/x-1a" => Ok(pdf_ops::PdfxLevel::Pdfx1a),
        "3" | "pdfx3" | "pdfx-3" | "pdf/x-3" => Ok(pdf_ops::PdfxLevel::Pdfx3),
        other => Err(StepError::Params(format!(
            "unknown PDF/X level `{other}` (expected 1a|3)"
        ))),
    }
}

fn parse_booklet_size(s: &str) -> Result<pdf_ops::BookletSize, StepError> {
    match s {
        "input" | "same_as_input" => Ok(pdf_ops::BookletSize::SameAsInput),
        "a4" => Ok(pdf_ops::BookletSize::A4),
        "letter" => Ok(pdf_ops::BookletSize::Letter),
        other => Err(StepError::Params(format!(
            "unknown booklet sheet_size `{other}` (expected input|a4|letter)"
        ))),
    }
}

fn parse_booklet_mode(s: &str) -> Result<pdf_ops::BookletMode, StepError> {
    match s {
        "left_right" => Ok(pdf_ops::BookletMode::LeftRight),
        "right_left" => Ok(pdf_ops::BookletMode::RightLeft),
        other => Err(StepError::Params(format!(
            "unknown booklet imposition `{other}` (expected left_right|right_left)"
        ))),
    }
}

/// `insert_blank` position: `position` = `end` (default) | `before` |
/// `after` | `at`, with `page` as the anchor/absolute position.
fn parse_insert_pos(params: &Value) -> Result<pdf_ops::InsertPos, StepError> {
    let anchor = || -> Result<u32, StepError> { required_u32(params, "page") };
    match optional_str(params, "position").unwrap_or("end") {
        "end" => Ok(pdf_ops::InsertPos::End),
        "before" => Ok(pdf_ops::InsertPos::Before(anchor()?)),
        "after" => Ok(pdf_ops::InsertPos::After(anchor()?)),
        "at" => Ok(pdf_ops::InsertPos::At(anchor()?)),
        other => Err(StepError::Params(format!(
            "unknown insert position `{other}` (expected end|before|after|at)"
        ))),
    }
}

/// `insert_blank` size: `a4` | `letter` | `match` (absent → `None`,
/// which the op treats as `Match`).
fn parse_page_size(s: &str) -> Result<pdf_ops::PageSize, StepError> {
    match s {
        "a4" => Ok(pdf_ops::PageSize::A4),
        "letter" => Ok(pdf_ops::PageSize::Letter),
        "match" => Ok(pdf_ops::PageSize::Match),
        other => Err(StepError::Params(format!(
            "unknown page size `{other}` (expected a4|letter|match)"
        ))),
    }
}

/// `duplicate_pages` position: `end` (default) | `before` | `after` |
/// `in_place`, with `page` anchoring `before`/`after`.
fn parse_dup_pos(params: &Value) -> Result<pdf_ops::DupPos, StepError> {
    match optional_str(params, "position").unwrap_or("end") {
        "end" => Ok(pdf_ops::DupPos::End),
        "in_place" => Ok(pdf_ops::DupPos::InPlace),
        "before" => Ok(pdf_ops::DupPos::Before(required_u32(params, "page")?)),
        "after" => Ok(pdf_ops::DupPos::After(required_u32(params, "page")?)),
        other => Err(StepError::Params(format!(
            "unknown duplicate position `{other}` (expected end|before|after|in_place)"
        ))),
    }
}

/// `split_mode` params → [`pdf_ops::SplitMode`]. `mode` is one of
/// `count` (`pages_per_file`), `range` (`spec`), `every_nth`
/// (`stride`). `at_bookmarks` exists in the engine but always errors —
/// it is deliberately not accepted here.
fn parse_split_mode<'a>(params: &'a Value) -> Result<pdf_ops::SplitMode<'a>, StepError> {
    match required_str(params, "mode")? {
        "count" => Ok(pdf_ops::SplitMode::ByCount {
            pages_per_file: required_u32(params, "pages_per_file")?,
        }),
        "range" => Ok(pdf_ops::SplitMode::ByRange {
            spec: required_str(params, "spec")?,
        }),
        "every_nth" => Ok(pdf_ops::SplitMode::EveryNth {
            n: required_u32(params, "stride")?,
        }),
        other => Err(StepError::Params(format!(
            "unknown split mode `{other}` (expected count|range|every_nth)"
        ))),
    }
}
