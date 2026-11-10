//! Remove selected pages from a PDF document.
//!
//! Pages are selected with the shared 1-based range spec
//! ([`parse_page_range`]); removal goes through `lopdf`'s
//! `delete_pages`, which also fixes `/Count` entries up the page-tree
//! parent chain. The selection is fully validated before the document
//! is modified, and a document must always keep at least one page —
//! removing every page is rejected with [`PdfError::InvalidInput`]
//! because the result would not be a usable PDF.

use crate::crates::core::engine::EngineLimits;

use crate::crates::pdf_engine::error::{PdfError, PdfResult};

use super::load::{load_pdf, reject_encrypted, save_pdf};
use super::pages::parse_page_range;

/// Remove the pages selected by `pages` (a range spec such as
/// `"1-3,8"` or `"all"`, 1-based) and return the remaining document.
///
/// Fails with [`PdfError::InvalidInput`] when the selection would
/// remove every page, leaving nothing behind.
pub fn remove_pages(input: &[u8], pages: &str, limits: &EngineLimits) -> PdfResult<Vec<u8>> {
    let mut doc = load_pdf(input, limits)?;
    reject_encrypted(&doc)?;

    let page_count = doc.get_pages().len() as u32;
    let selected = parse_page_range(pages, page_count)?;
    if selected.len() as u32 == page_count {
        return Err(PdfError::InvalidInput(
            "cannot remove every page; the document must keep at least one page".to_string(),
        ));
    }

    doc.delete_pages(&selected);

    save_pdf(&mut doc)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::crates::pdf_engine::ops::test_utils::{
        page_count as test_page_count, pdf_with_pages,
    };

    #[test]
    fn removes_single_page() {
        let pdf = pdf_with_pages(&["one", "two", "three"]);
        let out = remove_pages(&pdf, "2", &EngineLimits::default()).unwrap();
        assert_eq!(test_page_count(&out), 2);
    }

    #[test]
    fn removes_range() {
        let pdf = pdf_with_pages(&["one", "two", "three", "four"]);
        let out = remove_pages(&pdf, "1-2", &EngineLimits::default()).unwrap();
        assert_eq!(test_page_count(&out), 2);
    }

    #[test]
    fn rejects_removing_all_pages() {
        let pdf = pdf_with_pages(&["one", "two", "three"]);
        for spec in ["all", "1-3", "3,1,2"] {
            let err = remove_pages(&pdf, spec, &EngineLimits::default()).unwrap_err();
            assert!(
                matches!(err, PdfError::InvalidInput(_)),
                "spec {spec:?} should fail with InvalidInput, got {err:?}"
            );
        }
    }

    #[test]
    fn rejects_bad_range() {
        let pdf = pdf_with_pages(&["one", "two"]);
        assert!(remove_pages(&pdf, "5", &EngineLimits::default()).is_err());
        assert!(remove_pages(&pdf, "", &EngineLimits::default()).is_err());
        assert!(remove_pages(b"junk", "1", &EngineLimits::default()).is_err());
    }
}
