//! Structural validation of a PDF document.
//!
//! `validate` performs sanity checks that do not require rendering:
//! the document must load, have a resolvable catalog/page tree, a
//! non-zero page count, readable page content streams, and a clean
//! cross-reference recovery story. It is intentionally conservative —
//! it reports issues rather than failing on the first anomaly, and
//! encrypted documents are reported (not rejected) since only surface
//! structure is inspected.

use lopdf::xref::XrefRecovery;
use serde::{Deserialize, Serialize};

use crate::crates::core::engine::EngineLimits;

use crate::crates::pdf_engine::error::PdfResult;

use super::load::{doc_is_encrypted, load_pdf};

/// Result of [`validate`].
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ValidationReport {
    /// `true` when no issues were found.
    pub ok: bool,
    /// Human-readable problems found, empty when `ok`.
    pub issues: Vec<String>,
}

/// Validate the structure of `input`.
///
/// Returns `Ok(report)` for any parseable PDF — structural problems are
/// reported in [`ValidationReport::issues`], not as errors. Returns
/// `Err` only when the input cannot be loaded at all or a limit is
/// exceeded.
pub fn validate(input: &[u8], limits: &EngineLimits) -> PdfResult<ValidationReport> {
    let doc = load_pdf(input, limits)?;
    let mut issues = Vec::new();

    if doc_is_encrypted(&doc) {
        issues.push("document is encrypted; only surface structure was inspected".to_string());
    }

    match &doc.xref_recovery {
        Some(XrefRecovery::OffsetCorrected) => issues.push(
            "cross-reference offsets were corrected during load; file may be damaged".to_string(),
        ),
        Some(XrefRecovery::Reconstructed) => issues.push(
            "cross-reference table was reconstructed during load; objects may be missing"
                .to_string(),
        ),
        None => {}
        // XrefRecovery is not marked non_exhaustive today, but stay
        // tolerant of future variants.
        Some(_) => issues.push("cross-reference data required repair during load".to_string()),
    }

    let pages = doc.get_pages();
    if pages.is_empty() {
        issues.push("document has no pages".to_string());
    }

    for page_id in pages.values() {
        match doc.get_dictionary(*page_id) {
            Err(_) => issues.push(format!("page object {page_id:?} is not a dictionary")),
            Ok(dict) => {
                // /Type /Page is required by the spec but frequently
                // omitted in the wild; report it as an issue, not an
                // error.
                if dict.get(b"Type").is_err() {
                    issues.push(format!("page object {page_id:?} has no /Type entry"));
                }
                // Pages without /Contents are legal (blank page); when
                // present, the stream must decode.
                if dict.get(b"Contents").is_ok()
                    && let Err(err) = doc.get_page_content_with_limit(*page_id, 64 * 1024 * 1024)
                {
                    issues.push(format!(
                        "page {page_id:?} content stream could not be read: {err}"
                    ));
                }
            }
        }
    }

    Ok(ValidationReport {
        ok: issues.is_empty(),
        issues,
    })
}
