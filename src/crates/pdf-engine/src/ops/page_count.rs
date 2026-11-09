//! Count the pages in a PDF document.
//!
//! Like [`inspect`](super::info::inspect), this is a structural query, so
//! encrypted documents are *not* rejected — the page count is still
//! meaningful metadata. Input-side [`EngineLimits`] are enforced through
//! [`load_pdf`](super::load::load_pdf) like every other op.

use crate::crates::core::engine::EngineLimits;

use crate::crates::pdf_engine::error::PdfResult;

use super::load::load_pdf;

/// Return the number of pages in `input`.
///
/// Fails with [`PdfError::InvalidPdf`] when the bytes are not a
/// parseable PDF and [`PdfError::LimitExceeded`] when the document
/// exceeds the configured input limits.
pub fn page_count(input: &[u8], limits: &EngineLimits) -> PdfResult<u32> {
    let doc = load_pdf(input, limits)?;
    Ok(doc.get_pages().len() as u32)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::crates::pdf_engine::ops::test_utils::{
        page_count as test_page_count, pdf_with_pages,
    };

    #[test]
    fn counts_pages() {
        let pdf = pdf_with_pages(&["one", "two", "three"]);
        assert_eq!(page_count(&pdf, &EngineLimits::default()).unwrap(), 3);
    }

    #[test]
    fn rejects_malformed_input() {
        assert!(page_count(b"not a pdf", &EngineLimits::default()).is_err());
        assert!(page_count(&[], &EngineLimits::default()).is_err());
    }

    #[test]
    fn enforces_page_limit() {
        let pdf = pdf_with_pages(&["one", "two", "three"]);
        let limits = EngineLimits {
            max_pages: Some(2),
            ..EngineLimits::default()
        };
        assert!(page_count(&pdf, &limits).is_err());
        assert_eq!(test_page_count(&pdf), 3);
    }
}
