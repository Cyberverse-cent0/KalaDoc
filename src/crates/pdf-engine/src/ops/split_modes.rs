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
use super::remove_blank_pages::is_blank_page;
use super::split::split;
use super::split_bookmarks::split_at_outline;
use super::text::extract_text;

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
    /// Split at outline/bookmark boundaries. `level` is 1-based:
    /// `1` splits at top-level bookmarks, `2` at their children, and
    /// so on (mapped onto [`split_at_outline`]'s 0-based levels).
    AtBookmarks {
        /// Outline depth (1 = top-level bookmarks).
        level: u32,
    },
    /// Greedy split by serialized size: pages accumulate into a chunk
    /// until adding the next page would exceed `max_bytes`. A single
    /// page larger than `max_bytes` fails with
    /// [`PdfError::LimitExceeded`]. Sizes are measured on the actual
    /// serialized chunk, so shared resources may make later chunks
    /// smaller than the sum of their pages suggests.
    ByFileSize {
        /// Maximum serialized bytes per output file; must be > 0.
        max_bytes: u64,
    },
    /// Split *at* provably blank pages: each blank page terminates the
    /// current chunk and is kept as its last page. Leading blank pages
    /// fold into the first chunk. Blank detection is the conservative
    /// structural check from [`super::remove_blank_pages`] — pages that
    /// merely *look* empty are not separators.
    AtBlankPages,
    /// Split *before* each page whose extracted text contains `query`
    /// (literal, case-sensitive substring). Pages before the first
    /// match fold into the first chunk. Text extraction is best-effort:
    /// scanned pages or fonts without ToUnicode maps yield no match.
    OnTextMatch {
        /// Literal substring to match against each page's text.
        query: &'a str,
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
        SplitMode::AtBookmarks { level } => {
            // `level` is 1-based here; `split_at_outline` is 0-based.
            split_at_outline(input, level.saturating_sub(1), limits)
        }
        SplitMode::ByFileSize { max_bytes } => split_by_size(input, max_bytes, limits),
        SplitMode::AtBlankPages => split_at_blank_pages(input, limits),
        SplitMode::OnTextMatch { query } => split_on_text_match(input, query, limits),
    }
}

/// Serialize `doc` restricted to the 1-based `pages` selection.
fn chunk_pdf(doc: &lopdf::Document, pages: &[u32], limits: &EngineLimits) -> PdfResult<Vec<u8>> {
    let page_count = doc.get_pages().len() as u32;
    let keep: std::collections::BTreeSet<u32> = pages.iter().copied().collect();
    let delete: Vec<u32> = (1..=page_count).filter(|p| !keep.contains(p)).collect();
    let mut chunk = doc.clone();
    chunk.delete_pages(&delete);
    save_pdf(&mut chunk)
}

/// Enforce `max_outputs` then serialize each page-number chunk.
fn chunks_to_pdfs(
    doc: &lopdf::Document,
    chunks: Vec<Vec<u32>>,
    limits: &EngineLimits,
) -> PdfResult<Vec<Vec<u8>>> {
    if let Some(max) = limits.max_outputs
        && chunks.len() > max as usize
    {
        return Err(PdfError::LimitExceeded(format!(
            "split would produce {} files, exceeding max_outputs {max}",
            chunks.len()
        )));
    }
    chunks
        .iter()
        .map(|pages| chunk_pdf(doc, pages, limits))
        .collect()
}

/// Greedy serialized-size split.
fn split_by_size(input: &[u8], max_bytes: u64, limits: &EngineLimits) -> PdfResult<Vec<Vec<u8>>> {
    if max_bytes == 0 {
        return Err(PdfError::InvalidInput(
            "max_bytes must be greater than 0".to_string(),
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

    let mut outputs = Vec::new();
    let mut chunk_start = 1u32;
    let mut chunk_end = 0u32; // last page confirmed to fit
    for page in 1..=page_count {
        let candidate: Vec<u32> = (chunk_start..=page).collect();
        let size = chunk_pdf(&doc, &candidate, limits)?.len() as u64;
        if size <= max_bytes {
            chunk_end = page;
            continue;
        }
        if chunk_end == 0 || chunk_end < chunk_start {
            // Even a single page exceeds the budget.
            return Err(PdfError::LimitExceeded(format!(
                "page {page} alone serializes to {size} bytes, exceeding max_bytes {max_bytes}"
            )));
        }
        outputs.push(chunk_pdf(
            &doc,
            &(chunk_start..=chunk_end).collect::<Vec<_>>(),
            limits,
        )?);
        chunk_start = page;
        chunk_end = 0;
        // Re-test the single page at the new chunk start.
        let size = chunk_pdf(&doc, &[page], limits)?.len() as u64;
        if size > max_bytes {
            return Err(PdfError::LimitExceeded(format!(
                "page {page} alone serializes to {size} bytes, exceeding max_bytes {max_bytes}"
            )));
        }
        chunk_end = page;
    }
    if chunk_end >= chunk_start {
        outputs.push(chunk_pdf(
            &doc,
            &(chunk_start..=chunk_end).collect::<Vec<_>>(),
            limits,
        )?);
    }
    if let Some(max) = limits.max_outputs
        && outputs.len() > max as usize
    {
        return Err(PdfError::LimitExceeded(format!(
            "size-based split produced {} files, exceeding max_outputs {max}",
            outputs.len()
        )));
    }
    Ok(outputs)
}

/// Split after every provably blank page.
fn split_at_blank_pages(input: &[u8], limits: &EngineLimits) -> PdfResult<Vec<Vec<u8>>> {
    let doc = load_pdf(input, limits)?;
    reject_encrypted(&doc)?;
    let pages_map = doc.get_pages();
    let page_count = pages_map.len() as u32;
    if page_count == 0 {
        return Err(PdfError::InvalidInput(
            "document has no pages to split".to_string(),
        ));
    }

    let mut chunks: Vec<Vec<u32>> = Vec::new();
    let mut current: Vec<u32> = Vec::new();
    for (num, page_id) in &pages_map {
        current.push(*num);
        if is_blank_page(&doc, *page_id) {
            chunks.push(std::mem::take(&mut current));
        }
    }
    if !current.is_empty() {
        chunks.push(current);
    }
    if chunks.is_empty() {
        chunks.push((1..=page_count).collect());
    }
    chunks_to_pdfs(&doc, chunks, limits)
}

/// Split before each page whose text contains `query`.
fn split_on_text_match(
    input: &[u8],
    query: &str,
    limits: &EngineLimits,
) -> PdfResult<Vec<Vec<u8>>> {
    if query.is_empty() {
        return Err(PdfError::InvalidInput(
            "text-match split requires a non-empty query".to_string(),
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

    let texts = extract_text(input, None, limits)?;
    let mut chunks: Vec<Vec<u32>> = Vec::new();
    let mut current: Vec<u32> = Vec::new();
    for pt in texts {
        if pt.text.contains(query) && !current.is_empty() {
            chunks.push(std::mem::take(&mut current));
        }
        current.push(pt.page);
    }
    if !current.is_empty() {
        chunks.push(current);
    }
    if chunks.is_empty() {
        chunks.push((1..=page_count).collect());
    }
    chunks_to_pdfs(&doc, chunks, limits)
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
