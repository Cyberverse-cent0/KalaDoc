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

    let encrypted = doc_is_encrypted(&doc);
    if encrypted {
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
    // A password-protected document loads with its trailer but no
    // objects, so an empty page map there means "unreadable", not
    // "zero pages" — do not report a page count we cannot know.
    // (`was_encrypted` is set when lopdf decrypted the document itself,
    // e.g. an empty user password; then the page tree is authoritative.)
    let page_tree_unavailable = encrypted && !doc.was_encrypted();
    if pages.is_empty() && !page_tree_unavailable {
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

#[cfg(test)]
mod tests {
    use super::*;
    use crate::crates::pdf_engine::ops::encrypt::{PdfPermissions, encrypt};
    use crate::crates::pdf_engine::ops::test_utils::pdf_with_pages;
    use lopdf::{Document, Object, dictionary};

    #[test]
    fn well_formed_document_reports_ok() {
        let pdf = pdf_with_pages(&["clean", "pages"]);
        let report = validate(&pdf, &EngineLimits::default()).unwrap();
        assert!(report.ok, "issues: {:?}", report.issues);
        assert!(report.issues.is_empty());
    }

    #[test]
    fn reports_document_without_pages() {
        let mut doc = Document::with_version("1.4");
        let pages_id = doc.new_object_id();
        doc.objects.insert(
            pages_id,
            Object::Dictionary(dictionary! {
                "Type" => "Pages",
                "Kids" => Vec::<Object>::new(),
                "Count" => 0,
            }),
        );
        let catalog_id = doc.add_object(dictionary! {
            "Type" => "Catalog",
            "Pages" => pages_id,
        });
        doc.trailer.set("Root", catalog_id);
        let mut pdf = Vec::new();
        doc.save_to(&mut pdf).unwrap();

        let report = validate(&pdf, &EngineLimits::default()).unwrap();
        assert!(!report.ok);
        assert!(
            report.issues.iter().any(|i| i.contains("no pages")),
            "issues: {:?}",
            report.issues
        );
    }

    #[test]
    fn encrypted_document_is_reported_not_rejected() {
        let plain = pdf_with_pages(&["secret"]);
        let encrypted = encrypt(
            &plain,
            "pw",
            None,
            PdfPermissions::default(),
            &EngineLimits::default(),
        )
        .unwrap();
        let report = validate(&encrypted, &EngineLimits::default()).unwrap();
        assert!(!report.ok);
        // The encrypted issue is reported; lopdf cannot load the page
        // tree without the password, so no "no pages" issue appears.
        assert!(
            report.issues.iter().any(|i| i.contains("encrypted")),
            "issues: {:?}",
            report.issues
        );
        assert!(
            !report.issues.iter().any(|i| i.contains("no pages")),
            "issues: {:?}",
            report.issues
        );
    }

    #[test]
    fn damaged_xref_reports_issues_or_load_error() {
        // Corrupt the xref table: lopdf should either recover (reporting
        // the repair) or reject the file outright — never panic.
        let mut pdf = pdf_with_pages(&["fragile"]);
        // Overwrite a chunk inside the file body with zeros; objects and
        // xref entries near the tail become inconsistent.
        let mid = pdf.len() / 2;
        let end = (mid + 64).min(pdf.len());
        for b in &mut pdf[mid..end] {
            *b = b' ';
        }
        match validate(&pdf, &EngineLimits::default()) {
            Ok(report) => {
                assert!(!report.ok, "corrupted file reported ok");
                assert!(!report.issues.is_empty());
            }
            Err(err) => {
                // Load-level rejection is also acceptable behavior.
                assert!(
                    matches!(
                        err,
                        crate::crates::pdf_engine::error::PdfError::InvalidPdf(_)
                            | crate::crates::pdf_engine::error::PdfError::InvalidInput(_)
                    ),
                    "unexpected error: {err:?}"
                );
            }
        }
    }

    #[test]
    fn rejects_unloadable_input() {
        assert!(validate(b"junk", &EngineLimits::default()).is_err());
    }
}
