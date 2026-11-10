//! Document comparison across three modes.
//!
//! - [`CompareMode::Text`] — extracts text from both documents and
//!   diffs it page-by-page, line-by-line (a small LCS diff written
//!   here; no external diff dependency). Reports added/removed line
//!   counts plus the first differing lines (capped).
//! - [`CompareMode::Structure`] — compares page counts and a SHA-256
//!   hash of each page's extracted text. Cheaper than `Text` when only
//!   equality matters; still uses the same best-effort text extractor,
//!   so it inherits its limitations (unusual encodings may compare as
//!   equal-empty).
//! - [`CompareMode::Bytes`] — SHA-256 of the raw input buffers plus
//!   sizes. The strictest mode: any byte difference (metadata,
//!   whitespace inside streams, xref layout) counts.
//!
//! **Out of scope**: visual/raster comparison (rendering both
//! documents and diffing pixels) is not implemented — use the `render`
//! op plus an image diff externally if needed. Document-info metadata
//! (`title`/`author`/...) is not part of any mode either.

use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};

use crate::crates::core::engine::EngineLimits;

use crate::crates::pdf_engine::error::PdfResult;

use super::load::{load_pdf, reject_encrypted};
use super::text::{PageText, extract_text};

/// How two documents are compared.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum CompareMode {
    /// Page-by-page text diff with line-level detail.
    Text,
    /// Page count + per-page text-hash comparison.
    Structure,
    /// Raw byte equality via SHA-256 and sizes.
    Bytes,
}

/// Kind of a differing line in [`DiffEntry`].
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum DiffKind {
    /// Line present only in document B.
    Added,
    /// Line present only in document A.
    Removed,
}

/// One differing line, tagged with the page it belongs to.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct DiffEntry {
    /// 1-based page number (of document A when both have the page).
    pub page: u32,
    /// Whether the line was added (in B) or removed (from A).
    pub kind: DiffKind,
    /// The line content (verbatim, not trimmed).
    pub line: String,
}

/// Result of [`compare`].
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct CompareReport {
    /// The mode that produced this report.
    pub mode: CompareMode,
    /// `true` when no differences were found in the chosen mode.
    pub identical: bool,
    /// Input sizes in bytes.
    pub size_a: u64,
    /// Input sizes in bytes.
    pub size_b: u64,
    /// SHA-256 (hex) of input A.
    pub sha256_a: String,
    /// SHA-256 (hex) of input B.
    pub sha256_b: String,
    /// Page counts (0 for [`CompareMode::Bytes`]).
    pub page_count_a: u32,
    /// Page counts (0 for [`CompareMode::Bytes`]).
    pub page_count_b: u32,
    /// Total lines present only in B, across all compared pages.
    pub added_lines: usize,
    /// Total lines present only in A, across all compared pages.
    pub removed_lines: usize,
    /// 1-based page numbers whose text differs (or that exist on only
    /// one side). Empty for `Bytes` mode.
    pub differing_pages: Vec<u32>,
    /// First differing lines, capped at [`MAX_DIFF_ENTRIES`].
    pub diff: Vec<DiffEntry>,
}

/// Maximum number of [`DiffEntry`] rows kept in a report.
const MAX_DIFF_ENTRIES: usize = 200;

/// Maximum LCS matrix size (cells); larger page pairs degrade to a
/// whole-page replace rather than risking quadratic memory.
const MAX_LCS_CELLS: usize = 4_000_000;

/// Compare `input_a` and `input_b` under `mode`.
///
/// Both inputs are loaded and limit-checked. `Text` and `Structure`
/// modes reject encrypted documents (their strings cannot be read);
/// `Bytes` mode does not need to parse at all and tolerates any input.
pub fn compare(
    input_a: &[u8],
    input_b: &[u8],
    mode: CompareMode,
    limits: &EngineLimits,
) -> PdfResult<CompareReport> {
    let sha256_a = sha256_hex(input_a);
    let sha256_b = sha256_hex(input_b);

    let mut report = CompareReport {
        mode,
        identical: false,
        size_a: input_a.len() as u64,
        size_b: input_b.len() as u64,
        sha256_a,
        sha256_b,
        page_count_a: 0,
        page_count_b: 0,
        added_lines: 0,
        removed_lines: 0,
        differing_pages: Vec::new(),
        diff: Vec::new(),
    };

    match mode {
        CompareMode::Bytes => {
            report.identical = input_a == input_b;
        }
        CompareMode::Structure => {
            let (count_a, hashes_a) = structure_fingerprint(input_a, limits)?;
            let (count_b, hashes_b) = structure_fingerprint(input_b, limits)?;
            report.page_count_a = count_a;
            report.page_count_b = count_b;
            report.identical = count_a == count_b && hashes_a == hashes_b;
            if !report.identical {
                let max = count_a.max(count_b);
                for i in 0..max {
                    let ha = hashes_a.get(i as usize);
                    let hb = hashes_b.get(i as usize);
                    if ha != hb {
                        report.differing_pages.push(i + 1);
                    }
                }
            }
        }
        CompareMode::Text => {
            let pages_a = extract_text(input_a, None, limits)?;
            let pages_b = extract_text(input_b, None, limits)?;
            report.page_count_a = pages_a.len() as u32;
            report.page_count_b = pages_b.len() as u32;
            compare_page_texts(&pages_a, &pages_b, &mut report);
            report.identical = report.differing_pages.is_empty()
                && report.added_lines == 0
                && report.removed_lines == 0;
        }
    }
    Ok(report)
}

fn sha256_hex(bytes: &[u8]) -> String {
    let digest = Sha256::digest(bytes);
    let mut hex = String::with_capacity(64);
    for byte in digest {
        use std::fmt::Write as _;
        let _ = write!(hex, "{byte:02x}");
    }
    hex
}

/// Page count + per-page text hashes for `Structure` mode.
fn structure_fingerprint(input: &[u8], limits: &EngineLimits) -> PdfResult<(u32, Vec<String>)> {
    let doc = load_pdf(input, limits)?;
    reject_encrypted(&doc)?;
    let page_count = doc.get_pages().len() as u32;
    drop(doc);
    let pages = extract_text(input, None, limits)?;
    let hashes = pages
        .iter()
        .map(|p| sha256_hex(p.text.as_bytes()))
        .collect();
    Ok((page_count, hashes))
}

/// Compare two per-page text extractions, filling `differing_pages`,
/// line counts and the capped `diff`.
fn compare_page_texts(a: &[PageText], b: &[PageText], report: &mut CompareReport) {
    let common = a.len().min(b.len());
    for i in 0..common {
        let (ta, tb) = (&a[i].text, &b[i].text);
        if ta == tb {
            continue;
        }
        report.differing_pages.push(a[i].page.max(b[i].page));
        let lines_a = ta.lines().collect::<Vec<_>>();
        let lines_b = tb.lines().collect::<Vec<_>>();
        diff_lines(&lines_a, &lines_b, a[i].page, report);
    }
    // Pages present on only one side: every line is added/removed.
    for extra in &b[common..] {
        report.differing_pages.push(extra.page);
        for line in extra.text.lines() {
            report.added_lines += 1;
            push_diff(
                report,
                DiffEntry {
                    page: extra.page,
                    kind: DiffKind::Added,
                    line: line.to_string(),
                },
            );
        }
    }
    for extra in &a[common..] {
        report.differing_pages.push(extra.page);
        for line in extra.text.lines() {
            report.removed_lines += 1;
            push_diff(
                report,
                DiffEntry {
                    page: extra.page,
                    kind: DiffKind::Removed,
                    line: line.to_string(),
                },
            );
        }
    }
    report.differing_pages.sort_unstable();
    report.differing_pages.dedup();
}

fn push_diff(report: &mut CompareReport, entry: DiffEntry) {
    if report.diff.len() < MAX_DIFF_ENTRIES {
        report.diff.push(entry);
    }
}

/// LCS-based line diff. Emits [`DiffKind::Removed`] for lines only in
/// `a` and [`DiffKind::Added`] for lines only in `b`. Page pairs whose
/// line counts would make the DP table exceed [`MAX_LCS_CELLS`] degrade
/// to remove-all-then-add-all.
fn diff_lines(a: &[&str], b: &[&str], page: u32, report: &mut CompareReport) {
    if a.len().saturating_mul(b.len()) > MAX_LCS_CELLS {
        for line in a {
            report.removed_lines += 1;
            push_diff(
                report,
                DiffEntry {
                    page,
                    kind: DiffKind::Removed,
                    line: (*line).to_string(),
                },
            );
        }
        for line in b {
            report.added_lines += 1;
            push_diff(
                report,
                DiffEntry {
                    page,
                    kind: DiffKind::Added,
                    line: (*line).to_string(),
                },
            );
        }
        return;
    }

    // dp[i][j] = LCS length of a[..i], b[..j].
    let n = a.len();
    let m = b.len();
    let width = m + 1;
    let mut dp = vec![0u32; (n + 1) * width];
    for i in (0..n).rev() {
        for j in (0..m).rev() {
            dp[i * width + j] = if a[i] == b[j] {
                dp[(i + 1) * width + j + 1] + 1
            } else {
                dp[(i + 1) * width + j].max(dp[i * width + j + 1])
            };
        }
    }

    // Walk the table emitting ops in order.
    let (mut i, mut j) = (0usize, 0usize);
    while i < n && j < m {
        if a[i] == b[j] {
            i += 1;
            j += 1;
        } else if dp[(i + 1) * width + j] >= dp[i * width + j + 1] {
            report.removed_lines += 1;
            push_diff(
                report,
                DiffEntry {
                    page,
                    kind: DiffKind::Removed,
                    line: a[i].to_string(),
                },
            );
            i += 1;
        } else {
            report.added_lines += 1;
            push_diff(
                report,
                DiffEntry {
                    page,
                    kind: DiffKind::Added,
                    line: b[j].to_string(),
                },
            );
            j += 1;
        }
    }
    while i < n {
        report.removed_lines += 1;
        push_diff(
            report,
            DiffEntry {
                page,
                kind: DiffKind::Removed,
                line: a[i].to_string(),
            },
        );
        i += 1;
    }
    while j < m {
        report.added_lines += 1;
        push_diff(
            report,
            DiffEntry {
                page,
                kind: DiffKind::Added,
                line: b[j].to_string(),
            },
        );
        j += 1;
    }
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;
    use crate::crates::pdf_engine::ops::test_utils::{pdf_with_pages, pdf_with_pages_and_title};

    fn limits() -> EngineLimits {
        EngineLimits::default()
    }

    #[test]
    fn bytes_mode() {
        let a = pdf_with_pages(&["same"]);
        let b = a.clone();
        let report = compare(&a, &b, CompareMode::Bytes, &limits()).unwrap();
        assert!(report.identical);
        assert_eq!(report.sha256_a, report.sha256_b);
        assert_eq!(report.size_a, report.size_b);

        // Same text, different title → bytes differ even though the
        // text layer is identical.
        let c = pdf_with_pages_and_title(&["same"], "Different Title");
        let report = compare(&a, &c, CompareMode::Bytes, &limits()).unwrap();
        assert!(!report.identical);
        assert_ne!(report.sha256_a, report.sha256_b);
    }

    #[test]
    fn structure_mode_detects_text_change() {
        let a = pdf_with_pages(&["alpha", "beta"]);
        let b = pdf_with_pages(&["alpha", "gamma"]);
        let report = compare(&a, &b, CompareMode::Structure, &limits()).unwrap();
        assert!(!report.identical);
        assert_eq!(report.page_count_a, 2);
        assert_eq!(report.page_count_b, 2);
        assert_eq!(report.differing_pages, vec![2]);

        let same = compare(&a, &a, CompareMode::Structure, &limits()).unwrap();
        assert!(same.identical);
    }

    #[test]
    fn structure_mode_detects_page_count_change() {
        let a = pdf_with_pages(&["one"]);
        let b = pdf_with_pages(&["one", "two"]);
        let report = compare(&a, &b, CompareMode::Structure, &limits()).unwrap();
        assert!(!report.identical);
        assert_eq!(report.differing_pages, vec![2]);
    }

    #[test]
    fn text_mode_line_diff() {
        // Two single-line pages differing in one word — each page's
        // text is a single line, so the diff is one remove + one add.
        let a = pdf_with_pages(&["the quick brown fox"]);
        let b = pdf_with_pages(&["the quick red fox"]);
        let report = compare(&a, &b, CompareMode::Text, &limits()).unwrap();
        assert!(!report.identical);
        assert_eq!(report.differing_pages, vec![1]);
        assert_eq!(report.removed_lines, 1);
        assert_eq!(report.added_lines, 1);
        assert!(
            report
                .diff
                .iter()
                .any(|d| d.kind == DiffKind::Removed && d.line.contains("brown"))
        );
        assert!(
            report
                .diff
                .iter()
                .any(|d| d.kind == DiffKind::Added && d.line.contains("red"))
        );
    }

    #[test]
    fn text_mode_identical() {
        let a = pdf_with_pages(&["identical text"]);
        let report = compare(&a, &a, CompareMode::Text, &limits()).unwrap();
        assert!(report.identical);
        assert_eq!(report.added_lines, 0);
        assert_eq!(report.removed_lines, 0);
        assert!(report.diff.is_empty());
    }

    #[test]
    fn text_mode_page_count_mismatch() {
        let a = pdf_with_pages(&["only page"]);
        let b = pdf_with_pages(&["only page", "extra page"]);
        let report = compare(&a, &b, CompareMode::Text, &limits()).unwrap();
        assert!(!report.identical);
        assert_eq!(report.page_count_a, 1);
        assert_eq!(report.page_count_b, 2);
        assert!(report.added_lines > 0);
        assert!(report.differing_pages.contains(&2));
    }

    #[test]
    fn metadata_difference_does_not_affect_text_mode() {
        // Text mode ignores document-info metadata — same text layer
        // compares identical even with different titles.
        let a = pdf_with_pages_and_title(&["same"], "Title A");
        let b = pdf_with_pages_and_title(&["same"], "Title B");
        let report = compare(&a, &b, CompareMode::Text, &limits()).unwrap();
        assert!(report.identical);
    }

    #[test]
    fn garbage_inputs() {
        // Bytes mode tolerates non-PDF input (it never parses).
        let report = compare(b"aaa", b"bbb", CompareMode::Bytes, &limits()).unwrap();
        assert!(!report.identical);
        // Text/Structure modes must fail on unparseable input.
        assert!(compare(b"aaa", b"bbb", CompareMode::Text, &limits()).is_err());
        assert!(compare(b"aaa", b"bbb", CompareMode::Structure, &limits()).is_err());
    }

    #[test]
    fn diff_capped() {
        // Many differing lines still produce a bounded diff vec while
        // the counts stay exact.
        let a_lines: Vec<String> = (0..300).map(|i| format!("a{i}")).collect();
        let b_lines: Vec<String> = (0..300).map(|i| format!("b{i}")).collect();
        let mut report = CompareReport {
            mode: CompareMode::Text,
            identical: false,
            size_a: 0,
            size_b: 0,
            sha256_a: String::new(),
            sha256_b: String::new(),
            page_count_a: 1,
            page_count_b: 1,
            added_lines: 0,
            removed_lines: 0,
            differing_pages: Vec::new(),
            diff: Vec::new(),
        };
        compare_page_texts(
            &[PageText {
                page: 1,
                text: a_lines.join("\n"),
            }],
            &[PageText {
                page: 1,
                text: b_lines.join("\n"),
            }],
            &mut report,
        );
        assert!(report.diff.len() <= MAX_DIFF_ENTRIES);
        assert_eq!(report.added_lines + report.removed_lines, 600);
    }
}
