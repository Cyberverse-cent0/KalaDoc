//! Pipeline model — serde-serializable definitions, steps, and outputs.

use serde::{Deserialize, Serialize};

/// Operation names a [`PipelineStep`] may request, as plain strings so
/// JSON/YAML definitions stay readable. Dispatch is validated against
/// the target engine's real capabilities at run time — an unknown
/// `(engine, operation)` pair fails before any work happens.
///
/// Supported pairs today:
///
/// | engine | operation | params | effect |
/// |---|---|---|---|
/// | `security` | `validate` | `max_bytes` (opt) | gate: PDF structure/encryption check, bytes unchanged |
/// | `security` | `validate_upload` | `filename` (req), `expected_format` (opt) | filename + size + magic-byte gate, bytes unchanged |
/// | `pdf` | `validate` | — | structural report, bytes unchanged |
/// | `pdf` | `inspect` | — | metadata report, bytes unchanged |
/// | `pdf` | `extract_text` | `pages` (opt range spec) | per-page text report, bytes unchanged |
/// | `pdf` | `extract_pages` | `pages` (req range spec) | bytes ← selected pages |
/// | `pdf` | `rotate` | `pages` (req), `degrees` (req: 90/180/270) | bytes ← rotated pages |
/// | `pdf` | `split` | `pages_per_file` (req) | terminal multi-output step |
///
/// `merge` is intentionally not supported: it consumes multiple input
/// documents, which a single-stream pipeline cannot express.
pub mod operation {
    /// `security.validate` — deep PDF gate (`validate_pdf`).
    pub const VALIDATE: &str = "validate";
    /// `security.validate_upload` — filename/size/sniff gate.
    pub const VALIDATE_UPLOAD: &str = "validate_upload";
    /// `pdf.validate` — structural `ValidationReport`.
    pub const PDF_VALIDATE: &str = "validate";
    /// `pdf.inspect` — `PdfInfo` metadata report.
    pub const PDF_INSPECT: &str = "inspect";
    /// `pdf.extract_text` — per-page `PageText` report.
    pub const PDF_EXTRACT_TEXT: &str = "extract_text";
    /// `pdf.extract_pages` — byte transform to a page subset.
    pub const PDF_EXTRACT_PAGES: &str = "extract_pages";
    /// `pdf.rotate` — byte transform rotating selected pages.
    pub const PDF_ROTATE: &str = "rotate";
    /// `pdf.split` — terminal step producing multiple PDFs.
    pub const PDF_SPLIT: &str = "split";
}

/// Engine names recognized by the dispatcher.
pub mod engine_name {
    /// The PDF engine (`PdfEngine`).
    pub const PDF: &str = "pdf";
    /// The security engine (`SecurityEngine`).
    pub const SECURITY: &str = "security";
}

/// One step in a [`PipelineDefinition`]: target engine, operation, and
/// a JSON object of operation-specific parameters.
///
/// `params` is deliberately untyped (`serde_json::Value`) — each
/// operation interprets only the keys it knows and ignores the rest,
/// so definitions stay forward-compatible. Unknown *operations* are
/// rejected; unknown *params* are not (documented per operation in
/// [`operation`]).
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct PipelineStep {
    /// Human-readable step name used in errors and reports. Must be
    /// unique within a definition for readable output (not enforced).
    pub name: String,
    /// Target engine: `"pdf"` or `"security"` (see [`engine_name`]).
    pub engine: String,
    /// Operation name (see [`operation`]).
    pub operation: String,
    /// Operation parameters as a JSON object. Missing keys use the
    /// operation's defaults; a non-object value is a definition error.
    #[serde(default)]
    pub params: serde_json::Value,
}

impl PipelineStep {
    /// Convenience constructor for a parameterless step.
    pub fn new(
        name: impl Into<String>,
        engine: impl Into<String>,
        operation: impl Into<String>,
    ) -> Self {
        Self {
            name: name.into(),
            engine: engine.into(),
            operation: operation.into(),
            params: serde_json::Value::Object(serde_json::Map::new()),
        }
    }

    /// Sets `params` (typically `serde_json::json!({...})`).
    pub fn with_params(mut self, params: serde_json::Value) -> Self {
        self.params = params;
        self
    }
}

/// A named, ordered sequence of [`PipelineStep`]s.
///
/// Definitions are pure data — construct them in code, deserialize
/// them from JSON/YAML config, or use
/// [`crate::crates::pipeline_engine::builtin_definitions`]. Validation
/// happens at run time ([`PipelineEngine::run_pipeline`](crate::crates::pipeline_engine::PipelineEngine::run_pipeline))
/// so a bad definition is reported with step context, not at parse
/// time.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct PipelineDefinition {
    /// Stable identifier, e.g. `"pdf.intake"`. Used by
    /// [`PipelineEngine::run_named`](crate::crates::pipeline_engine::PipelineEngine::run_named)
    /// for built-ins and as the `pipeline` field of [`PipelineOutput`].
    pub name: String,
    /// Optional human description shown in listings.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub description: Option<String>,
    /// Ordered steps. Must be non-empty and within
    /// [`PipelineLimits::max_steps`](crate::crates::pipeline_engine::PipelineLimits).
    pub steps: Vec<PipelineStep>,
}

/// The data a pipeline carries between steps and returns to the caller.
///
/// Most steps transform [`PipelineArtifact::Bytes`]; `pdf.split` is the
/// only operation producing [`PipelineArtifact::Multiple`], which must
/// be the terminal step (a later step needing a single byte stream
/// fails fast).
///
/// Serialized manually (rather than derived) so byte payloads encode
/// as base64 strings via [`super::b64_serde`] — serde's `with` helper
/// cannot annotate the `Vec<u8>` inside `Vec<Vec<u8>>`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum PipelineArtifact {
    /// A single byte stream — the common case.
    Bytes(Vec<u8>),
    /// Multiple byte streams (e.g. `pdf.split` output).
    Multiple(Vec<Vec<u8>>),
}

impl PipelineArtifact {
    /// Total bytes held by this artifact, for cumulative limit checks.
    pub fn len_bytes(&self) -> u64 {
        match self {
            Self::Bytes(b) => b.len() as u64,
            Self::Multiple(v) => v.iter().map(|b| b.len() as u64).sum(),
        }
    }

    /// Borrow the single byte stream, or `None` for multi-output
    /// artifacts. Steps that consume bytes use this and fail with a
    /// step-context error when it returns `None`.
    pub fn as_bytes(&self) -> Option<&[u8]> {
        match self {
            Self::Bytes(b) => Some(b),
            Self::Multiple(_) => None,
        }
    }
}

impl Serialize for PipelineArtifact {
    fn serialize<S: serde::Serializer>(&self, serializer: S) -> Result<S::Ok, S::Error> {
        use serde::ser::SerializeMap;
        match self {
            Self::Bytes(b) => {
                let mut map = serializer.serialize_map(Some(2))?;
                map.serialize_entry("kind", "bytes")?;
                map.serialize_entry("data", &super::b64_serde::encode(b))?;
                map.end()
            }
            Self::Multiple(v) => {
                let encoded: Vec<String> =
                    v.iter().map(|b| super::b64_serde::encode(b)).collect();
                let mut map = serializer.serialize_map(Some(2))?;
                map.serialize_entry("kind", "multiple")?;
                map.serialize_entry("data", &encoded)?;
                map.end()
            }
        }
    }
}

impl<'de> Deserialize<'de> for PipelineArtifact {
    fn deserialize<D: serde::Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
        use serde::de::{MapAccess, Visitor};
        use std::fmt;

        struct ArtifactVisitor;

        impl<'de> Visitor<'de> for ArtifactVisitor {
            type Value = PipelineArtifact;

            fn expecting(&self, f: &mut fmt::Formatter) -> fmt::Result {
                f.write_str("a pipeline artifact object with `kind` and `data` fields")
            }

            fn visit_map<A: MapAccess<'de>>(self, mut map: A) -> Result<Self::Value, A::Error> {
                let mut kind: Option<String> = None;
                let mut data: Option<serde_json::Value> = None;
                while let Some(key) = map.next_key::<String>()? {
                    match key.as_str() {
                        "kind" => kind = Some(map.next_value()?),
                        "data" => data = Some(map.next_value()?),
                        _ => {
                            let _: serde::de::IgnoredAny = map.next_value()?;
                        }
                    }
                }
                let kind = kind.ok_or_else(|| serde::de::Error::missing_field("kind"))?;
                let data = data.ok_or_else(|| serde::de::Error::missing_field("data"))?;
                match kind.as_str() {
                    "bytes" => {
                        let s = data.as_str().ok_or_else(|| {
                            serde::de::Error::custom(
                                "bytes artifact `data` must be a base64 string",
                            )
                        })?;
                        super::b64_serde::decode(s)
                            .map(PipelineArtifact::Bytes)
                            .map_err(serde::de::Error::custom)
                    }
                    "multiple" => {
                        let list = data.as_array().ok_or_else(|| {
                            serde::de::Error::custom(
                                "multiple artifact `data` must be an array of base64 strings",
                            )
                        })?;
                        let mut out = Vec::with_capacity(list.len());
                        for item in list {
                            let s = item.as_str().ok_or_else(|| {
                                serde::de::Error::custom(
                                    "multiple artifact entries must be base64 strings",
                                )
                            })?;
                            out.push(
                                super::b64_serde::decode(s).map_err(serde::de::Error::custom)?,
                            );
                        }
                        Ok(PipelineArtifact::Multiple(out))
                    }
                    other => Err(serde::de::Error::custom(format!(
                        "unknown pipeline artifact kind `{other}`"
                    ))),
                }
            }
        }

        deserializer.deserialize_map(ArtifactVisitor)
    }
}

/// Per-step record in [`PipelineOutput::steps`]: which step ran and
/// what it produced (for report steps) or a short summary.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct StepReport {
    /// 0-based position in the definition.
    pub index: usize,
    /// Step name from the definition.
    pub name: String,
    /// Engine that ran the step.
    pub engine: String,
    /// Operation that ran.
    pub operation: String,
    /// Structured output for report steps (`validate`, `inspect`,
    /// `extract_text`, `validate_upload`), serialized to JSON. `None`
    /// for byte transforms and `split`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub output: Option<serde_json::Value>,
}

/// Result of a successful
/// [`PipelineEngine::run_pipeline`](crate::crates::pipeline_engine::PipelineEngine::run_pipeline).
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct PipelineOutput {
    /// Name of the definition that ran.
    pub pipeline: String,
    /// Final artifact — bytes for most pipelines, multiple documents
    /// when the last step was `pdf.split`.
    pub artifact: PipelineArtifact,
    /// One [`StepReport`] per executed step, in order.
    pub steps: Vec<StepReport>,
}
