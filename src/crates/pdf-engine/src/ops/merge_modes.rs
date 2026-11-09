//! Merge several PDFs into one with an explicit page-ordering mode.
//!
//! This is a thin dispatcher over the existing `merge` and `interleave`
//! ops so callers can pick a combination strategy by name instead of
//! choosing between functions:
//!
//! - [`MergeMode::Sequential`] — all pages of input 1, then all pages of
//!   input 2, and so on (identical to [`super::merge::merge`]).
//! - [`MergeMode::Interleave`] — round-robin pages across the inputs:
//!   page 1 of doc A, page 1 of doc B, ..., page 2 of doc A, ...
//!   (identical to [`super::interleave::interleave`]).
//! - [`MergeMode::Reverse`] — inputs are applied in reverse order, so
//!   the *last* input's pages come first in the output and the first
//!   input's pages come last (implemented by reversing the input slice
//!   and running the sequential merge).
//!
//! All limit and encrypted-input semantics are inherited from the
//! delegated op: `max_pages` is enforced against the *combined* page
//! count of every input, `max_input_bytes`/`max_pages` are checked per
//! input during load, and any encrypted input yields
//! [`PdfError::Encrypted`].

use crate::crates::core::engine::EngineLimits;

use crate::crates::pdf_engine::error::PdfResult;

use super::interleave::interleave;
use super::merge::merge;

/// How the pages of multiple input documents are combined.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum MergeMode {
    /// Document order: every page of input 1, then every page of input
    /// 2, etc. Requires at least one input.
    Sequential,
    /// Round-robin: page *r* of every input that still has one, for
    /// *r* = 1, 2, 3, ... Requires at least two inputs.
    Interleave,
    /// Inputs applied in reverse order: the last input contributes the
    /// first pages of the output. Requires at least one input.
    Reverse,
}

/// Merge `inputs` (raw PDF bytes) into a single document using `mode`.
///
/// See the module docs for the per-mode ordering. Returns
/// [`PdfError::Encrypted`] if any input is encrypted and
/// [`PdfError::LimitExceeded`] when the combined page count exceeds
/// `max_pages`.
pub fn merge_mode(inputs: &[&[u8]], mode: MergeMode, limits: &EngineLimits) -> PdfResult<Vec<u8>> {
    match mode {
        MergeMode::Sequential => merge(inputs, limits),
        MergeMode::Interleave => interleave(inputs, limits),
        MergeMode::Reverse => {
            let reversed: Vec<&[u8]> = inputs.iter().rev().copied().collect();
            merge(&reversed, limits)
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::crates::pdf_engine::ops::encrypt::{PdfPermissions, encrypt};
    use crate::crates::pdf_engine::ops::test_utils::{page_count, pdf_with_pages};
    use lopdf::Document;

    fn page_text(pdf: &[u8], n: u32) -> String {
        let doc = Document::load_mem(pdf).unwrap();
        let page_id = doc.get_pages()[&n];
        String::from_utf8_lossy(&doc.get_page_content(page_id)).into_owned()
    }

    #[test]
    fn sequential_keeps_document_order() {
        let a = pdf_with_pages(&["a1", "a2"]);
        let b = pdf_with_pages(&["b1"]);
        let out = merge_mode(&[&a, &b], MergeMode::Sequential, &EngineLimits::default()).unwrap();
        assert_eq!(page_count(&out), 3);
        assert!(page_text(&out, 1).contains("a1"));
        assert!(page_text(&out, 2).contains("a2"));
        assert!(page_text(&out, 3).contains("b1"));
    }

    #[test]
    fn interleave_delegates_to_round_robin() {
        let a = pdf_with_pages(&["a1", "a2", "a3"]);
        let b = pdf_with_pages(&["b1", "b2"]);
        let out = merge_mode(&[&a, &b], MergeMode::Interleave, &EngineLimits::default()).unwrap();
        assert_eq!(page_count(&out), 5);
        for (n, want) in [(1, "a1"), (2, "b1"), (3, "a2"), (4, "b2"), (5, "a3")] {
            assert!(page_text(&out, n).contains(want), "page {n}");
        }
    }

    #[test]
    fn reverse_applies_inputs_last_to_first() {
        let a = pdf_with_pages(&["a1", "a2"]);
        let b = pdf_with_pages(&["b1"]);
        let c = pdf_with_pages(&["c1", "c2", "c3"]);
        let out = merge_mode(&[&a, &b, &c], MergeMode::Reverse, &EngineLimits::default()).unwrap();
        assert_eq!(page_count(&out), 6);
        // c's pages first, then b, then a.
        let texts: Vec<String> = (1..=6).map(|n| page_text(&out, n)).collect();
        assert!(texts[0].contains("c1"));
        assert!(texts[1].contains("c2"));
        assert!(texts[2].contains("c3"));
        assert!(texts[3].contains("b1"));
        assert!(texts[4].contains("a1"));
        assert!(texts[5].contains("a2"));
    }

    #[test]
    fn rejects_bad_and_encrypted_inputs() {
        let a = pdf_with_pages(&["a1"]);
        // Empty input list fails in the delegated merge.
        assert!(merge_mode(&[], MergeMode::Sequential, &EngineLimits::default()).is_err());
        assert!(merge_mode(&[], MergeMode::Reverse, &EngineLimits::default()).is_err());
        // Malformed bytes fail.
        assert!(
            merge_mode(
                &[b"junk".as_slice()],
                MergeMode::Sequential,
                &EngineLimits::default()
            )
            .is_err()
        );
        // Encrypted input is rejected.
        let encrypted = encrypt(
            &a,
            "pw",
            None,
            PdfPermissions::default(),
            &EngineLimits::default(),
        )
        .unwrap();
        for mode in [MergeMode::Sequential, MergeMode::Reverse] {
            let err = merge_mode(&[&encrypted], mode, &EngineLimits::default()).unwrap_err();
            assert!(matches!(
                err,
                crate::crates::pdf_engine::error::PdfError::Encrypted
            ));
        }
    }

    #[test]
    fn enforces_combined_max_pages() {
        let a = pdf_with_pages(&["a1", "a2"]);
        let b = pdf_with_pages(&["b1", "b2"]);
        let limits = EngineLimits {
            max_pages: Some(3),
            ..EngineLimits::default()
        };
        assert!(merge_mode(&[&a, &b], MergeMode::Sequential, &limits).is_err());
        assert!(merge_mode(&[&a, &b], MergeMode::Reverse, &limits).is_err());
        assert!(merge_mode(&[&a, &b], MergeMode::Interleave, &limits).is_err());
    }
}
