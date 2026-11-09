//! Split a PDF into multiple files using an explicit split strategy.
//!
//! This is a dispatcher over the existing split/extract ops plus one
//! strategy implemented here:
//!
//! - [`SplitMode::ByCount`] — chunks of `pages_per_file` consecutive
//!   pages (identical to [`super::split::split`]).
//! - [`SplitMode::ByRange`] — one output per comma-separated item of
//!   `spec`; each item is a page-range spec (`"1-3"`, `"8"`, ...)
//!   resolved with [`super::extract::extract_pages`]. E.g.
//!   `"1-2,5-6,9"` produces three files. `max_outputs` is checked
//!   against the item count *before* any output is produced.
//! - [`SplitMode::EveryNth`] — strided split: output *k* receives pages
//!   `k+1, k+1+n, k+1+2n, ...` (1-based page numbers congruent to
//!   `k+1` modulo `n`). Produces `min(n, page_count)` non-empty files;
//!   each output preserves the original relative page order.
//! - [`SplitMode::AtBookmarks`] — split at outline/bookmark
//!   boundaries. Not yet available: this build returns
//!   [`PdfError::UnsupportedOperation`] for this mode.
//!
//! `max_outputs` is enforced for every mode that produces a variable
//! number of files; `max_pages`/`max_input_bytes` are enforced on the
//! input by the shared loader, and encrypted input yields
//! [`PdfError::Encrypted`].

use crate::crates::core::engine::EngineLimits;

use crate::crates::pdf_engine::error::{PdfError, PdfResult};

use super::extract::extract_pages;
use super::load::{check_page_limit, load_pdf, reject_encrypted, save_pdf};
use super::split::split;

/// How a single input document is divided into output files.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SplitMode<'a> {
    /// Consecutive chunks of `pages_per_file` pages each; the last
    /// chunk may be smaller.
    ByCount {
        /// Pages per output file; must be > 0.
        pages_per_file: u32,
    },
    /// One output file per comma-separated range item in `spec`
    /// (e.g. `"1-3,8,10-12"` → three files).
    ByRange {
        /// Comma-separated page-range spec; each item uses the
        /// [`super::pages::parse_page_range`] grammar.
        spec: &'a str,
    },
    /// Strided split with stride `n`: output *k* gets pages
    /// `k+1, k+1+n, k+1+2n, ...`. `n` must be > 0.
    EveryNth {
        /// Stride between pages assigned to the same output file.
        n: u32,
    },
    /// Split at outline/bookmark boundaries at the given outline
    /// depth. Currently unsupported — see module docs.
    AtBookmarks {
        /// Outline depth (1 = top-level bookmarks).
        level: u32,
    },
}

/// Split `input` into multiple PDF documents according to `mode`.
///
/// Returns one `Vec<u8>` per output file. Fails with
/// [`PdfError::InvalidInput`] on degenerate parameters (zero stride,
/// empty document), [`PdfError::InvalidPageRange`] on malformed range
/// specs, [`PdfError::LimitExceeded`] when the number of outputs would
/// exceed `max_outputs`, and [`PdfError::UnsupportedOperation`] for
/// [`SplitMode::AtBookmarks`].
pub fn split_mode(
    input: &[u8],
    mode: SplitMode<'_>,
    limits: &EngineLimits,
) -> PdfResult<Vec<Vec<u8>>> {
    match mode {
        SplitMode::ByCount { pages_per_file } => split(input, pages_per_file, limits),
        SplitMode::ByRange { spec } => split_by_range(input, spec, limits),
        SplitMode::EveryNth { n } => split_every_nth(input, n, limits),
        SplitMode::AtBookmarks { level } => Err(PdfError::UnsupportedOperation(format!(
            "split at outline level {level} is not available in this build"
        ))),
    }
}

/// One output per comma-separated item of `spec`.
fn split_by_range(input: &[u8], spec: &str, limits: &EngineLimits) -> PdfResult<Vec<Vec<u8>>> {
    // `parse_page_range` would treat the whole spec as one selection;
    // here each comma item is its own output file. Reject empty items
    // up front so a spec like "1-2,,4" fails before any output work.
    let items: Vec<&str> = spec.split(',').map(str::trim).collect();
    if items.iter().any(|item| item.is_empty()) {
        return Err(PdfError::InvalidPageRange(format!(
            "empty item in range spec \"{spec}\""
        )));
    }
    if let Some(max) = limits.max_outputs
        && items.len() > max as usize
    {
        return Err(PdfError::LimitExceeded(format!(
            "range spec would produce {} files, exceeding max_outputs {max}",
            items.len()
        )));
    }

    let mut outputs = Vec::with_capacity(items.len());
    for item in items {
        outputs.push(extract_pages(input, item, limits)?);
    }
    Ok(outputs)
}

/// Strided split: output *k* receives pages `k+1, k+1+n, ...`.
fn split_every_nth(input: &[u8], n: u32, limits: &EngineLimits) -> PdfResult<Vec<Vec<u8>>> {
    if n == 0 {
        return Err(PdfError::InvalidInput(
            "stride n must be greater than 0".to_string(),
        ));
    }

    let doc = load_pdf(input, limits)?;
    reject_encrypted(&doc)?;

    let page_count = doc.get_pages().len() as u32;
    if page_count == 0 {
        return Err(PdfError::InvalidInput(
            "document has no pages to split".to_string(),
        ));
    }
    check_page_limit(page_count as usize, limits)?;

    // Outputs k = 0..min(n, page_count)-1; strides beyond the page count
    // would produce empty files, so they are skipped entirely.
    let output_count = (n as usize).min(page_count as usize);
    if let Some(max) = limits.max_outputs
        && output_count > max as usize
    {
        return Err(PdfError::LimitExceeded(format!(
            "strided split would produce {output_count} files, exceeding max_outputs {max}"
        )));
    }

    let mut outputs = Vec::with_capacity(output_count);
    for k in 0..output_count {
        let mut chunk = doc.clone();
        // Keep pages p where (p - 1) % n == k; delete everything else.
        let keep_mod = k as u64 % n as u64;
        let stride = n as u64;
        let delete: Vec<u32> = (1..=page_count)
            .filter(|p| ((*p as u64 - 1) % stride) != keep_mod)
            .collect();
        chunk.delete_pages(&delete);
        outputs.push(save_pdf(&mut chunk)?);
    }
    Ok(outputs)
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
    fn by_count_delegates_to_split() {
        let input = pdf_with_pages(&["p1", "p2", "p3", "p4", "p5"]);
        let outputs = split_mode(
            &input,
            SplitMode::ByCount { pages_per_file: 2 },
            &EngineLimits::default(),
        )
        .unwrap();
        assert_eq!(outputs.len(), 3);
        assert_eq!(page_count(&outputs[0]), 2);
        assert_eq!(page_count(&outputs[1]), 2);
        assert_eq!(page_count(&outputs[2]), 1);
        // Pages stay consecutive: file 1 = p1,p2; file 3 = p5.
        assert!(page_text(&outputs[0], 1).contains("p1"));
        assert!(page_text(&outputs[0], 2).contains("p2"));
        assert!(page_text(&outputs[2], 1).contains("p5"));
    }

    #[test]
    fn every_nth_produces_strided_membership() {
        let input = pdf_with_pages(&["p1", "p2", "p3", "p4", "p5"]);
        let outputs = split_mode(
            &input,
            SplitMode::EveryNth { n: 2 },
            &EngineLimits::default(),
        )
        .unwrap();
        // n = 2 → two files: {1,3,5} and {2,4}.
        assert_eq!(outputs.len(), 2);
        assert_eq!(page_count(&outputs[0]), 3);
        assert_eq!(page_count(&outputs[1]), 2);
        assert!(page_text(&outputs[0], 1).contains("p1"));
        assert!(page_text(&outputs[0], 2).contains("p3"));
        assert!(page_text(&outputs[0], 3).contains("p5"));
        assert!(page_text(&outputs[1], 1).contains("p2"));
        assert!(page_text(&outputs[1], 2).contains("p4"));
    }

    #[test]
    fn every_nth_with_stride_larger_than_pages() {
        let input = pdf_with_pages(&["p1", "p2"]);
        let outputs = split_mode(
            &input,
            SplitMode::EveryNth { n: 5 },
            &EngineLimits::default(),
        )
        .unwrap();
        // Only page_count non-empty outputs are produced.
        assert_eq!(outputs.len(), 2);
        assert_eq!(page_count(&outputs[0]), 1);
        assert_eq!(page_count(&outputs[1]), 1);
        assert!(page_text(&outputs[0], 1).contains("p1"));
        assert!(page_text(&outputs[1], 1).contains("p2"));
    }

    #[test]
    fn by_range_produces_one_file_per_item() {
        let input = pdf_with_pages(&["p1", "p2", "p3", "p4", "p5"]);
        let outputs = split_mode(
            &input,
            SplitMode::ByRange { spec: "1-2,4" },
            &EngineLimits::default(),
        )
        .unwrap();
        assert_eq!(outputs.len(), 2);
        assert_eq!(page_count(&outputs[0]), 2);
        assert_eq!(page_count(&outputs[1]), 1);
        assert!(page_text(&outputs[0], 1).contains("p1"));
        assert!(page_text(&outputs[0], 2).contains("p2"));
        assert!(page_text(&outputs[1], 1).contains("p4"));
    }

    #[test]
    fn rejects_invalid_parameters() {
        let input = pdf_with_pages(&["p1", "p2"]);
        assert!(
            split_mode(
                &input,
                SplitMode::EveryNth { n: 0 },
                &EngineLimits::default()
            )
            .is_err()
        );
        assert!(
            split_mode(
                &input,
                SplitMode::ByCount { pages_per_file: 0 },
                &EngineLimits::default()
            )
            .is_err()
        );
        // Empty range item and out-of-bounds page both fail.
        assert!(
            split_mode(
                &input,
                SplitMode::ByRange { spec: "1,,2" },
                &EngineLimits::default()
            )
            .is_err()
        );
        assert!(
            split_mode(
                &input,
                SplitMode::ByRange { spec: "1-9" },
                &EngineLimits::default()
            )
            .is_err()
        );
        // Bookmark split is explicitly unsupported.
        let err = split_mode(
            &input,
            SplitMode::AtBookmarks { level: 1 },
            &EngineLimits::default(),
        )
        .unwrap_err();
        assert!(matches!(err, PdfError::UnsupportedOperation(_)));
    }

    #[test]
    fn enforces_max_outputs() {
        let input = pdf_with_pages(&["p1", "p2", "p3", "p4", "p5"]);
        let limits = EngineLimits {
            max_outputs: Some(2),
            ..EngineLimits::default()
        };
        // EveryNth n=3 → 3 outputs > 2.
        assert!(split_mode(&input, SplitMode::EveryNth { n: 3 }, &limits).is_err());
        // ByRange with 3 items > 2.
        assert!(split_mode(&input, SplitMode::ByRange { spec: "1,2,3" }, &limits).is_err());
        // ByCount 5 pages / 1 per file → 5 outputs > 2 (delegated check).
        assert!(split_mode(&input, SplitMode::ByCount { pages_per_file: 1 }, &limits).is_err());
    }

    #[test]
    fn rejects_encrypted_input() {
        let input = pdf_with_pages(&["p1", "p2", "p3"]);
        let encrypted = encrypt(
            &input,
            "pw",
            None,
            PdfPermissions::default(),
            &EngineLimits::default(),
        )
        .unwrap();
        for mode in [
            SplitMode::EveryNth { n: 2 },
            SplitMode::ByRange { spec: "1-2" },
            SplitMode::ByCount { pages_per_file: 1 },
        ] {
            let err = split_mode(&encrypted, mode, &EngineLimits::default()).unwrap_err();
            assert!(matches!(err, PdfError::Encrypted), "mode {mode:?}");
        }
    }
}
