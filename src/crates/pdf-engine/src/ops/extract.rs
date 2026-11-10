//! Extract a subset of pages into a new PDF.
//!
//! Pages are selected with the shared 1-based range spec
//! ([`parse_page_range`]); the spec is validated against the document's
//! page count before any page is removed.

use std::collections::BTreeSet;

use crate::crates::core::engine::EngineLimits;

use crate::crates::pdf_engine::error::PdfResult;

use super::load::{load_pdf, reject_encrypted, save_pdf};
use super::pages::parse_page_range;

/// Write a new PDF containing only the pages selected by `pages`
/// (e.g. `"1-3,8,10-12"` or `"all"`), preserving their original order.
pub fn extract_pages(input: &[u8], pages: &str, limits: &EngineLimits) -> PdfResult<Vec<u8>> {
    let mut doc = load_pdf(input, limits)?;
    reject_encrypted(&doc)?;

    let page_count = doc.get_pages().len() as u32;
    let keep: BTreeSet<u32> = parse_page_range(pages, page_count)?.into_iter().collect();

    let delete: Vec<u32> = (1..=page_count).filter(|p| !keep.contains(p)).collect();
    doc.delete_pages(&delete);

    save_pdf(&mut doc)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::crates::pdf_engine::error::PdfError;
    use crate::crates::pdf_engine::ops::encrypt::{PdfPermissions, encrypt};
    use crate::crates::pdf_engine::ops::test_utils::{page_count, pdf_with_pages};

    #[test]
    fn extracts_single_and_multi_page_ranges() {
        let pdf = pdf_with_pages(&["p1", "p2", "p3", "p4"]);

        let out = extract_pages(&pdf, "2", &EngineLimits::default()).unwrap();
        assert_eq!(page_count(&out), 1);

        let out = extract_pages(&pdf, "1,3-4", &EngineLimits::default()).unwrap();
        assert_eq!(page_count(&out), 3);

        let out = extract_pages(&pdf, "all", &EngineLimits::default()).unwrap();
        assert_eq!(page_count(&out), 4);
    }

    #[test]
    fn rejects_invalid_ranges() {
        let pdf = pdf_with_pages(&["p1", "p2"]);
        for spec in ["", "0", "3", "2-1", "1,x", "1-2-3"] {
            let err = extract_pages(&pdf, spec, &EngineLimits::default()).unwrap_err();
            assert!(
                matches!(err, PdfError::InvalidPageRange(_)),
                "spec {spec:?} got {err:?}"
            );
        }
    }

    #[test]
    fn rejects_encrypted_and_corrupt_input() {
        let plain = pdf_with_pages(&["x"]);
        let encrypted = encrypt(
            &plain,
            "pw",
            None,
            PdfPermissions::default(),
            &EngineLimits::default(),
        )
        .unwrap();
        let err = extract_pages(&encrypted, "1", &EngineLimits::default()).unwrap_err();
        assert!(matches!(err, PdfError::Encrypted), "got {err:?}");

        assert!(extract_pages(b"junk", "1", &EngineLimits::default()).is_err());
    }

    #[test]
    fn enforces_input_limits() {
        let pdf = pdf_with_pages(&["p1", "p2"]);
        let limits = EngineLimits {
            max_pages: Some(1),
            ..EngineLimits::default()
        };
        let err = extract_pages(&pdf, "1", &limits).unwrap_err();
        assert!(matches!(err, PdfError::LimitExceeded(_)), "got {err:?}");
    }
}
