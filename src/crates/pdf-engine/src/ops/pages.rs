//! 1-based page range spec parsing, shared by `rotate`,
//! `extract_pages` and `extract_text`.
//!
//! Grammar (whitespace around tokens is ignored):
//!
//! ```text
//! spec    := "all" | item ("," item)*
//! item    := number | number "-" number
//! number  := [0-9]+
//! ```
//!
//! Examples: `"1"`, `"1-3"`, `"2,5-7"`, `" 1 - 3 , 8 "`.
//!
//! Pages are 1-based to match the CLI/MCP contract. The spec is fully
//! validated — including bounds against the document's page count —
//! *before* any document work happens, per the project convention of
//! rejecting invalid ranges early. The result is sorted and deduplicated.

use std::collections::BTreeSet;

use crate::crates::pdf_engine::error::{PdfError, PdfResult};

/// Parse a page range spec into a sorted, deduplicated list of 1-based
/// page numbers.
///
/// `page_count` is the document's page count; every selected page must
/// satisfy `1 <= page <= page_count`. `"all"` (case-insensitive) selects
/// every page. Returns [`PdfError::InvalidPageRange`] on any malformed or
/// out-of-bounds input.
pub fn parse_page_range(spec: &str, page_count: u32) -> PdfResult<Vec<u32>> {
    let spec = spec.trim();
    if spec.is_empty() {
        return Err(PdfError::InvalidPageRange(
            "empty page range; expected e.g. \"1-3,8,10-12\"".to_string(),
        ));
    }
    if page_count == 0 {
        return Err(PdfError::InvalidPageRange(
            "document has no pages".to_string(),
        ));
    }
    if spec.eq_ignore_ascii_case("all") {
        return Ok((1..=page_count).collect());
    }

    let mut pages = BTreeSet::new();
    for part in spec.split(',') {
        let part = part.trim();
        if part.is_empty() {
            return Err(PdfError::InvalidPageRange(format!(
                "empty item in page range \"{spec}\""
            )));
        }
        let (start, end) = match part.split_once('-') {
            Some((a, b)) => {
                let start = parse_page_number(a, spec)?;
                let end = parse_page_number(b, spec)?;
                if start > end {
                    return Err(PdfError::InvalidPageRange(format!(
                        "range \"{part}\" starts after it ends"
                    )));
                }
                (start, end)
            }
            None => {
                let n = parse_page_number(part, spec)?;
                (n, n)
            }
        };
        if start == 0 {
            return Err(PdfError::InvalidPageRange(
                "page numbers are 1-based; 0 is not a valid page".to_string(),
            ));
        }
        if start > page_count {
            return Err(PdfError::InvalidPageRange(format!(
                "page {start} is out of range; document has {page_count} page(s)"
            )));
        }
        // `end` may exceed page_count only when start <= page_count; clamp
        // would silently accept typos like "1-999", so reject instead.
        if end > page_count {
            return Err(PdfError::InvalidPageRange(format!(
                "page {end} is out of range; document has {page_count} page(s)"
            )));
        }
        pages.extend(start..=end);
    }
    Ok(pages.into_iter().collect())
}

fn parse_page_number(token: &str, spec: &str) -> PdfResult<u32> {
    let token = token.trim();
    if token.is_empty() || !token.bytes().all(|b| b.is_ascii_digit()) {
        return Err(PdfError::InvalidPageRange(format!(
            "invalid page number \"{token}\" in range \"{spec}\""
        )));
    }
    token
        .parse::<u32>()
        .map_err(|_| PdfError::InvalidPageRange(format!("page number \"{token}\" is too large")))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn single_page() {
        assert_eq!(parse_page_range("1", 5).unwrap(), vec![1]);
        assert_eq!(parse_page_range(" 3 ", 5).unwrap(), vec![3]);
    }

    #[test]
    fn range_and_list() {
        assert_eq!(parse_page_range("1-3", 5).unwrap(), vec![1, 2, 3]);
        assert_eq!(parse_page_range("2,5-7", 9).unwrap(), vec![2, 5, 6, 7]);
        assert_eq!(parse_page_range("1-2,2-3", 5).unwrap(), vec![1, 2, 3]);
        assert_eq!(parse_page_range("all", 3).unwrap(), vec![1, 2, 3]);
        assert_eq!(parse_page_range(" ALL ", 2).unwrap(), vec![1, 2]);
        assert_eq!(parse_page_range("1 - 3 , 8", 10).unwrap(), vec![1, 2, 3, 8]);
    }

    #[test]
    fn rejects_invalid() {
        for spec in ["", " ", "0", "3-1", "a", "1,,2", "1-", "-3", "1.5", "1-999"] {
            assert!(
                parse_page_range(spec, 5).is_err(),
                "spec {spec:?} should fail"
            );
        }
        assert!(parse_page_range("6", 5).is_err());
        assert!(parse_page_range("1-6", 5).is_err());
        assert!(parse_page_range("1", 0).is_err());
    }
}
