//! Built-in pipeline definitions shipped with the engine.
//!
//! These are ordinary [`PipelineDefinition`] values — inspect them,
//! serialize them, or use them as templates for custom definitions.
//! [`PipelineEngine::run_named`](crate::crates::pipeline_engine::PipelineEngine::run_named)
//! resolves names against this list.

use super::model::{PipelineDefinition, PipelineStep, engine_name, operation};

/// All built-in pipeline definitions.
pub fn builtin_definitions() -> Vec<PipelineDefinition> {
    vec![pdf_intake(), pdf_extract_text()]
}

/// Find a built-in definition by name, returning an owned copy.
pub(crate) fn find_builtin(name: &str) -> Option<PipelineDefinition> {
    builtin_definitions().into_iter().find(|def| def.name == name)
}

/// `pdf.intake` — the standard ingestion gate: security validation,
/// PDF structural validation, metadata inspection, and text
/// extraction. The artifact returned is the original PDF bytes; the
/// reports of the last three steps describe it.
pub fn pdf_intake() -> PipelineDefinition {
    PipelineDefinition {
        name: "pdf.intake".to_string(),
        description: Some(
            "Validate an uploaded PDF, inspect its metadata, and extract its text".to_string(),
        ),
        steps: vec![
            PipelineStep::new(
                "security-gate",
                engine_name::SECURITY,
                operation::VALIDATE,
            ),
            PipelineStep::new("pdf-validate", engine_name::PDF, operation::PDF_VALIDATE),
            PipelineStep::new("pdf-inspect", engine_name::PDF, operation::PDF_INSPECT),
            PipelineStep::new(
                "pdf-extract-text",
                engine_name::PDF,
                operation::PDF_EXTRACT_TEXT,
            ),
        ],
    }
}

/// `pdf.extract-text` — lighter-weight variant of [`pdf_intake`] that
/// skips the security gate (callers that already validated uploads).
pub fn pdf_extract_text() -> PipelineDefinition {
    PipelineDefinition {
        name: "pdf.extract-text".to_string(),
        description: Some("Validate a PDF and extract its per-page text".to_string()),
        steps: vec![
            PipelineStep::new("pdf-validate", engine_name::PDF, operation::PDF_VALIDATE),
            PipelineStep::new(
                "pdf-extract-text",
                engine_name::PDF,
                operation::PDF_EXTRACT_TEXT,
            ),
        ],
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn builtins_have_unique_names_and_valid_shape() {
        let defs = builtin_definitions();
        assert_eq!(defs.len(), 2);
        for def in &defs {
            assert!(!def.steps.is_empty());
        }
        assert_eq!(find_builtin("pdf.intake").unwrap().name, "pdf.intake");
        assert!(find_builtin("nope").is_none());
    }
}
