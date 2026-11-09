//! Convert a PDF's text layer into plain text or Markdown.
//!
//! This is a *text-layer-only* conversion: it uses the same best-effort
//! extractor as [`extract_text`](super::text::extract_text). It does NOT
//! perform OCR on scanned/image-only pages, does not preserve layout,
//! tables, fonts, or images, and does not reconstruct document
//! structure beyond page boundaries. Pages without extractable text
//! produce empty sections — never fabricated content.

use serde::{Deserialize, Serialize};

use crate::crates::core::engine::EngineLimits;

use crate::crates::pdf_engine::error::PdfResult;

use super::text::{PageText, extract_text};

/// Output format for [`convert`].
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ConvertTarget {
    /// Plain text: page texts joined by a blank line.
    Text,
    /// Markdown: one `## Page {n}` heading per selected page.
    Markdown,
}

/// Convert `input` to the requested text format, returned as UTF-8 bytes.
///
/// `pages` selects which pages to convert (shared 1-based range spec;
/// `None` converts every page). Encrypted documents are rejected by the
/// underlying extractor.
///
/// `Text` joins the extracted page texts with `"\n\n"`. `Markdown`
/// emits `## Page {n}` per selected page followed by that page's text;
/// every selected page is emitted honestly — a page with no extractable
/// text yields a header with an empty body.
pub fn convert(
    input: &[u8],
    target: ConvertTarget,
    pages: Option<&str>,
    limits: &EngineLimits,
) -> PdfResult<Vec<u8>> {
    let pages_text: Vec<PageText> = extract_text(input, pages, limits)?;

    let mut out = String::new();
    for (i, page) in pages_text.iter().enumerate() {
        if i > 0 {
            out.push_str("\n\n");
        }
        match target {
            ConvertTarget::Text => out.push_str(&page.text),
            ConvertTarget::Markdown => {
                out.push_str(&format!("## Page {}\n\n{}\n\n", page.page, page.text));
            }
        }
    }
    Ok(out.into_bytes())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::crates::pdf_engine::error::PdfError;
    use crate::crates::pdf_engine::ops::test_utils::pdf_with_pages;

    fn limits() -> EngineLimits {
        EngineLimits::default()
    }

    fn to_string(bytes: &[u8]) -> String {
        String::from_utf8(bytes.to_vec()).unwrap()
    }

    #[test]
    fn convert_text_joins_pages() {
        let pdf = pdf_with_pages(&["alpha one", "beta two"]);
        let out = convert(&pdf, ConvertTarget::Text, None, &limits()).unwrap();
        let text = to_string(&out);
        assert!(text.contains("alpha one"), "missing page 1 text: {text:?}");
        assert!(text.contains("beta two"), "missing page 2 text: {text:?}");
        // Pages joined by a blank line.
        assert!(
            text.contains("alpha one\n\nbeta two")
                || text.contains("alpha one\n\n") && text.contains("\n\nbeta two"),
            "pages not separated by blank line: {text:?}"
        );
    }

    #[test]
    fn convert_markdown_has_page_headers() {
        let pdf = pdf_with_pages(&["alpha one", "beta two"]);
        let out = convert(&pdf, ConvertTarget::Markdown, None, &limits()).unwrap();
        let text = to_string(&out);
        assert!(
            text.contains("## Page 1"),
            "missing page 1 header: {text:?}"
        );
        assert!(
            text.contains("## Page 2"),
            "missing page 2 header: {text:?}"
        );
        assert!(text.contains("alpha one"));
        assert!(text.contains("beta two"));
    }

    #[test]
    fn convert_page_range_selects_subset() {
        let pdf = pdf_with_pages(&["first", "second", "third"]);
        let out = convert(&pdf, ConvertTarget::Text, Some("2-3"), &limits()).unwrap();
        let text = to_string(&out);
        assert!(!text.contains("first"), "unexpected page 1 text: {text:?}");
        assert!(text.contains("second"));
        assert!(text.contains("third"));

        let md = convert(&pdf, ConvertTarget::Markdown, Some("2"), &limits()).unwrap();
        let text = to_string(&md);
        assert!(text.contains("## Page 2"));
        assert!(!text.contains("## Page 1"));
        assert!(!text.contains("## Page 3"));
    }

    #[test]
    fn convert_garbage_is_err() {
        assert!(matches!(
            convert(b"not a pdf", ConvertTarget::Text, None, &limits()),
            Err(PdfError::InvalidPdf(_))
        ));
    }

    #[test]
    fn convert_target_serde_roundtrip() {
        assert_eq!(
            serde_json::to_string(&ConvertTarget::Text).unwrap(),
            "\"text\""
        );
        assert_eq!(
            serde_json::to_string(&ConvertTarget::Markdown).unwrap(),
            "\"markdown\""
        );
        let parsed: ConvertTarget = serde_json::from_str("\"markdown\"").unwrap();
        assert_eq!(parsed, ConvertTarget::Markdown);
    }
}
