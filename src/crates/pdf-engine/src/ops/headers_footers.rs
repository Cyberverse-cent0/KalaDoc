//! Headers, footers and page numbers for existing pages.
//!
//! Each selected page gets an appended content stream that paints the
//! header (top-center), footer text (bottom-center) and/or a page
//! number (per [`PageNumberStyle`]) in base-14 Helvetica. The marks are
//! drawn **on top of** existing page content via
//! [`append_page_overlay`] — the same choice `draw_text` and `stamp`
//! make — because headers/footers/page numbers are meant to stay
//! readable even over full-page scans or dense layouts (unlike
//! watermarks, which deliberately go under via [`wrap_page_content`]).
//! They sit inside the page margins, so overlap with body content is
//! rare; when it does happen the header/footer wins.
//!
//! # Templates
//!
//! `header`/`footer` strings support two placeholders, substituted per
//! page before drawing:
//!
//! - `{page}` — the 1-based page number of the page being drawn.
//! - `{total}` — the document's total page count.
//!
//! e.g. `"Page {page} of {total}"`, `"Confidential — {page}"`.
//! Placeholders are plain substring replacements; a literal `{` or `}`
//! cannot be escaped.
//!
//! # Layout
//!
//! All coordinates are absolute PDF user-space units translated by the
//! page's `/MediaBox` origin (`/UserUnit` ignored, as in the other mark
//! ops). `margin` (default 36 pt = 0.5 in) is the distance kept between
//! the page edge and the text block:
//!
//! - **Header**: lines are split on `'\n'`; the first line's baseline
//!   sits `font_size * 0.8` below `MediaBox.top - margin`, and each
//!   following line drops by `line_height = font_size * 1.2`.
//! - **Footer text**: same line splitting; the block's bottom edge is
//!   `MediaBox.bottom + margin`, raised by one `line_height` when a
//!   bottom page number is also drawn so the two never overlap.
//! - **Page number**: baseline `font_size * 0.25` above
//!   `MediaBox.bottom + margin`; horizontal alignment follows the
//!   [`PageNumberStyle`] variant. Only the bare number is drawn (e.g.
//!   `"3"`) — use the footer template for "Page 3 of 12" text.
//!
//! Text width for centering/right-alignment uses
//! [`estimate_text_width`] (the shared Helvetica approximation), so
//! centered marks are centered to within roughly a character width.
//!
//! # Encoding
//!
//! Text is emitted through [`pdf_text_operand`]: Latin-1-range text
//! becomes a literal string under WinAnsiEncoding; anything outside
//! that range falls back to a UTF-16BE hex string, which base-14
//! Helvetica cannot fully render (exotic glyphs may show as `.notdef`
//! boxes in strict viewers). Control characters become `?`.

use lopdf::Object;
use lopdf::content::Operation;
use serde::{Deserialize, Serialize};

use crate::crates::core::engine::EngineLimits;

use crate::crates::pdf_engine::error::{PdfError, PdfResult};

use super::load::{load_pdf, reject_encrypted, save_pdf};
use super::watermark::{
    add_content_stream, append_page_overlay, encode_operations, ensure_font_resource,
    estimate_text_width, media_box, pdf_text_operand, selected_pages,
};

/// Default margin in points (0.5 inch) when
/// [`HeaderFooterSpec::margin`] is `None`.
const DEFAULT_MARGIN: f32 = 36.0;

/// Default font size in points when
/// [`HeaderFooterSpec::font_size`] is `None`.
const DEFAULT_FONT_SIZE: f32 = 10.0;

/// Where the page number is painted. `None` draws no number.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum PageNumberStyle {
    /// No page number.
    None,
    /// Centered horizontally at the bottom margin.
    BottomCenter,
    /// Right-aligned at the bottom margin (text ends `margin` from
    /// the right edge).
    BottomRight,
    /// Left-aligned at the bottom margin (text starts `margin` from
    /// the left edge).
    BottomLeft,
}

/// Options for [`headers_footers`].
#[derive(Debug, Clone)]
pub struct HeaderFooterSpec {
    /// Header text, drawn top-center on every selected page.
    /// `None` or whitespace-only draws nothing. Supports `{page}` and
    /// `{total}` placeholders; `'\n'` splits into multiple lines.
    pub header: Option<String>,
    /// Footer text, drawn bottom-center on every selected page.
    /// `None` or whitespace-only draws nothing. Same placeholders and
    /// line splitting as `header`.
    pub footer: Option<String>,
    /// Page-number placement. [`PageNumberStyle::None`] draws no
    /// number. The bare page number is drawn regardless of `header`/
    /// `footer`.
    pub page_numbers: PageNumberStyle,
    /// Font size in points for all header/footer/number text.
    /// `None` = 10. Must be finite and in `(0, 1000]` when set.
    pub font_size: Option<f32>,
    /// Distance in points between the page edge and the text blocks.
    /// `None` = 36 (0.5 in). Must be finite and `>= 0` when set.
    pub margin: Option<f32>,
}

/// Replace `{page}`/`{total}` placeholders in `template`.
fn render_template(template: &str, page: u32, total: u32) -> String {
    template
        .replace("{page}", &page.to_string())
        .replace("{total}", &total.to_string())
}

/// Emit one `BT / Tf / Td / Tj / ET` text-object sequence drawing
/// `text` with its baseline origin at absolute `(x, y)`.
fn text_line_ops(font_name: &str, font_size: f32, x: f32, y: f32, text: &str) -> Vec<Operation> {
    vec![
        Operation::new("BT", vec![]),
        Operation::new(
            "Tf",
            vec![
                Object::Name(font_name.as_bytes().to_vec()),
                Object::Real(font_size),
            ],
        ),
        Operation::new("Td", vec![Object::Real(x), Object::Real(y)]),
        Operation::new("Tj", vec![pdf_text_operand(text)]),
        Operation::new("ET", vec![]),
    ]
}

/// Horizontally center `text` (estimated width) inside `[left, right]`
/// and return the baseline-origin x coordinate.
fn centered_x(left: f32, right: f32, text: &str, font_size: f32) -> f32 {
    left + (right - left - estimate_text_width(text, font_size)) * 0.5
}

/// Apply headers, footers and/or page numbers to `input`; returns the
/// new PDF bytes.
///
/// `pages` is an optional 1-based range spec (`"1-3,8"`, `"all"`);
/// `None` marks every page. Page numbers and `{page}`/`{total}`
/// placeholders always refer to real page positions, not positions
/// within the selection.
///
/// Returns [`PdfError::InvalidInput`] when the spec asks for nothing
/// (no header, no footer, [`PageNumberStyle::None`]) or carries
/// out-of-range `font_size`/`margin`, [`PdfError::Encrypted`] for
/// encrypted input, and [`PdfError::InvalidPageRange`] for a bad
/// `pages` spec.
pub fn headers_footers(
    input: &[u8],
    spec: &HeaderFooterSpec,
    pages: Option<&str>,
    limits: &EngineLimits,
) -> PdfResult<Vec<u8>> {
    let font_size = match spec.font_size {
        Some(v) => {
            if !v.is_finite() || v <= 0.0 || v > 1000.0 {
                return Err(PdfError::InvalidInput(format!(
                    "headers_footers font_size must be in (0, 1000], got {v}"
                )));
            }
            v
        }
        None => DEFAULT_FONT_SIZE,
    };
    let margin = match spec.margin {
        Some(v) => {
            if !v.is_finite() || v < 0.0 {
                return Err(PdfError::InvalidInput(format!(
                    "headers_footers margin must be finite and >= 0, got {v}"
                )));
            }
            v
        }
        None => DEFAULT_MARGIN,
    };
    let has_header = spec.header.as_deref().is_some_and(|t| !t.trim().is_empty());
    let has_footer = spec.footer.as_deref().is_some_and(|t| !t.trim().is_empty());
    let has_numbers = spec.page_numbers != PageNumberStyle::None;
    if !has_header && !has_footer && !has_numbers {
        return Err(PdfError::InvalidInput(
            "headers_footers spec draws nothing: header and footer are empty \
             and page_numbers is None"
                .to_string(),
        ));
    }

    let mut doc = load_pdf(input, limits)?;
    reject_encrypted(&doc)?;

    let pages_map = doc.get_pages();
    let total = pages_map.len() as u32;
    let selected = selected_pages(&doc, pages)?;

    let line_height = font_size * 1.2;
    // Bottom page numbers reserve one line of vertical space; the
    // footer text block is lifted above it so the two never collide.
    let number_height = if has_numbers { line_height } else { 0.0 };

    for page_number in selected {
        let Some(&page_id) = pages_map.get(&page_number) else {
            return Err(PdfError::InvalidPageRange(format!(
                "page {page_number} not found in document"
            )));
        };
        let [bx0, by0, bx1, by1] = media_box(&doc, page_id);
        let (x0, y0) = (bx0 as f32, by0 as f32);
        let page_w = (bx1 - bx0) as f32;
        let page_h = (by1 - by0) as f32;

        let font_name = ensure_font_resource(&mut doc, page_id)?;
        let mut operations = Vec::new();

        // -- Header: top-center, first baseline font_size*0.8 below the
        //    top margin, subsequent lines drop by line_height.
        if let Some(template) = spec.header.as_deref().filter(|t| !t.trim().is_empty()) {
            let text = render_template(template, page_number, total);
            let mut y = y0 + page_h - margin - font_size * 0.8;
            for line in text.split('\n') {
                let line = line.strip_suffix('\r').unwrap_or(line);
                let x = centered_x(x0, x0 + page_w, line, font_size);
                operations.extend(text_line_ops(&font_name, font_size, x, y, line));
                y -= line_height;
            }
        }

        // -- Footer text: bottom-center, block bottom edge sits `margin`
        //    (+ one line_height when a page number shares the bottom
        //    strip) above the MediaBox bottom.
        if let Some(template) = spec.footer.as_deref().filter(|t| !t.trim().is_empty()) {
            let text = render_template(template, page_number, total);
            let lines: Vec<&str> = text
                .split('\n')
                .map(|l| l.strip_suffix('\r').unwrap_or(l))
                .collect();
            let bottom_edge = y0 + margin + number_height;
            // Last line's baseline sits a quarter-em above the block's
            // bottom edge; earlier lines stack upward by line_height.
            let last_baseline = bottom_edge + font_size * 0.25;
            for (i, line) in lines.iter().enumerate() {
                let y = last_baseline + (lines.len() - 1 - i) as f32 * line_height;
                let x = centered_x(x0, x0 + page_w, line, font_size);
                operations.extend(text_line_ops(&font_name, font_size, x, y, line));
            }
        }

        // -- Page number at the bottom per style.
        if has_numbers {
            let number_text = page_number.to_string();
            let w = estimate_text_width(&number_text, font_size);
            let x = match spec.page_numbers {
                PageNumberStyle::BottomLeft => x0 + margin,
                PageNumberStyle::BottomCenter => x0 + (page_w - w) * 0.5,
                PageNumberStyle::BottomRight => x0 + page_w - margin - w,
                PageNumberStyle::None => unreachable!("guarded by has_numbers"),
            };
            let y = y0 + margin + font_size * 0.25;
            operations.extend(text_line_ops(&font_name, font_size, x, y, &number_text));
        }

        let stream_id = add_content_stream(&mut doc, encode_operations(operations)?)?;
        // Headers/footers/numbers paint on top of page content.
        append_page_overlay(&mut doc, page_id, stream_id)?;
    }

    doc.compress();
    save_pdf(&mut doc)
}

/// Convenience wrapper: add only a [`PageNumberStyle`] page number to
/// every page of `input` (no header/footer text). Equivalent to
/// [`headers_footers`] with `header: None`, `footer: None`.
///
/// [`PageNumberStyle::None`] is rejected by the underlying call — use
/// [`headers_footers`] with a real spec if you need a no-op.
pub fn add_page_numbers(
    input: &[u8],
    style: PageNumberStyle,
    limits: &EngineLimits,
) -> PdfResult<Vec<u8>> {
    headers_footers(
        input,
        &HeaderFooterSpec {
            header: None,
            footer: None,
            page_numbers: style,
            font_size: None,
            margin: None,
        },
        None,
        limits,
    )
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::crates::pdf_engine::ops::test_utils::{page_count, pdf_with_pages};
    use lopdf::Document as LopdfDocument;

    /// Decompressed content-stream text of each page, in page order.
    fn page_streams(pdf: &[u8]) -> Vec<String> {
        let doc = LopdfDocument::load_mem(pdf).unwrap();
        let mut pages: Vec<u32> = doc.get_pages().keys().copied().collect();
        pages.sort_unstable();
        pages
            .into_iter()
            .map(|n| {
                let id = doc.get_pages()[&n];
                let raw = doc.get_page_content(id);
                String::from_utf8_lossy(&raw).into_owned()
            })
            .collect()
    }

    fn spec() -> HeaderFooterSpec {
        HeaderFooterSpec {
            header: Some("Confidential {page} of {total}".to_string()),
            footer: Some("Page {page}".to_string()),
            page_numbers: PageNumberStyle::BottomCenter,
            font_size: None,
            margin: None,
        }
    }

    #[test]
    fn draws_header_footer_and_numbers_on_all_pages() {
        let pdf = pdf_with_pages(&["alpha", "beta", "gamma"]);
        let out = headers_footers(&pdf, &spec(), None, &EngineLimits::default()).unwrap();
        assert_eq!(page_count(&out), 3);
        let streams = page_streams(&out);
        assert_eq!(streams.len(), 3);
        for (i, stream) in streams.iter().enumerate() {
            let n = (i + 1).to_string();
            assert!(
                stream.contains(&format!("Confidential {n} of 3")),
                "page {} missing header: {stream}",
                i + 1
            );
            assert!(
                stream.contains(&format!("(Page {n})")),
                "page {} missing footer: {stream}",
                i + 1
            );
            // Bare page number drawn by BottomCenter style.
            assert!(
                stream.contains(&format!("({n}) Tj")),
                "page {} missing number operand: {stream}",
                i + 1
            );
            assert!(stream.contains("/KF"), "page {} missing font", i + 1);
        }
        // Page 2 must carry its own numbers, not page 1's.
        assert!(!streams[1].contains("Confidential 1 of 3"));
        assert!(!streams[1].contains("(Page 1)"));
    }

    #[test]
    fn page_numbers_only_via_convenience() {
        let pdf = pdf_with_pages(&["one", "two"]);
        let out =
            add_page_numbers(&pdf, PageNumberStyle::BottomRight, &EngineLimits::default()).unwrap();
        let streams = page_streams(&out);
        assert!(streams[0].contains("(1) Tj"));
        assert!(streams[1].contains("(2) Tj"));
        assert!(!streams[0].contains("(2) Tj"));
    }

    #[test]
    fn number_styles_align_differently() {
        let pdf = pdf_with_pages(&["x"]);
        for style in [
            PageNumberStyle::BottomLeft,
            PageNumberStyle::BottomCenter,
            PageNumberStyle::BottomRight,
        ] {
            let out = add_page_numbers(&pdf, style, &EngineLimits::default()).unwrap();
            assert_eq!(page_count(&out), 1);
            assert!(page_streams(&out)[0].contains("(1) Tj"));
        }
    }

    #[test]
    fn multiline_header_and_custom_size_margin() {
        let pdf = pdf_with_pages(&["x"]);
        let s = HeaderFooterSpec {
            header: Some("line one\nline two".to_string()),
            footer: None,
            page_numbers: PageNumberStyle::None,
            font_size: Some(14.0),
            margin: Some(72.0),
        };
        let out = headers_footers(&pdf, &s, None, &EngineLimits::default()).unwrap();
        let stream = &page_streams(&out)[0];
        assert!(stream.contains("(line one)"));
        assert!(stream.contains("(line two)"));
        assert!(stream.contains(" 14.0 "), "font size operand: {stream}");
    }

    #[test]
    fn respects_page_range() {
        let pdf = pdf_with_pages(&["one", "two", "three"]);
        let out = headers_footers(&pdf, &spec(), Some("2"), &EngineLimits::default()).unwrap();
        let streams = page_streams(&out);
        assert!(!streams[0].contains("Confidential"));
        assert!(streams[1].contains("Confidential 2 of 3"));
        assert!(!streams[2].contains("Confidential"));
    }

    #[test]
    fn extract_text_sees_the_marks() {
        let pdf = pdf_with_pages(&["body"]);
        let out = headers_footers(&pdf, &spec(), None, &EngineLimits::default()).unwrap();
        let pages = super::super::text::extract_text(&out, None, &EngineLimits::default()).unwrap();
        assert_eq!(pages.len(), 1);
        assert!(pages[0].text.contains("Confidential 1 of 1"));
        assert!(pages[0].text.contains("Page 1"));
    }

    #[test]
    fn rejects_bad_specs() {
        let pdf = pdf_with_pages(&["x"]);
        let limits = EngineLimits::default();
        let empty = HeaderFooterSpec {
            header: None,
            footer: Some("   ".to_string()),
            page_numbers: PageNumberStyle::None,
            font_size: None,
            margin: None,
        };
        assert!(headers_footers(&pdf, &empty, None, &limits).is_err());
        for (font_size, margin) in [
            (Some(0.0), None),
            (Some(-1.0), None),
            (Some(f32::NAN), None),
            (Some(1001.0), None),
            (None, Some(-1.0)),
            (None, Some(f32::INFINITY)),
        ] {
            let s = HeaderFooterSpec {
                font_size,
                margin,
                ..spec()
            };
            assert!(
                headers_footers(&pdf, &s, None, &limits).is_err(),
                "font_size={font_size:?} margin={margin:?} should fail"
            );
        }
        assert!(add_page_numbers(&pdf, PageNumberStyle::None, &limits).is_err());
    }

    #[test]
    fn rejects_bad_input() {
        let pdf = pdf_with_pages(&["x"]);
        let limits = EngineLimits::default();
        assert!(headers_footers(b"junk", &spec(), None, &limits).is_err());
        let s = HeaderFooterSpec {
            page_numbers: PageNumberStyle::BottomCenter,
            ..spec()
        };
        assert!(headers_footers(&pdf, &s, Some("9"), &limits).is_err());
    }

    #[test]
    fn non_latin1_text_does_not_panic() {
        let pdf = pdf_with_pages(&["x"]);
        for text in ["héllo wörld", "テキスト", "tab\there"] {
            let s = HeaderFooterSpec {
                header: Some(text.to_string()),
                footer: None,
                page_numbers: PageNumberStyle::None,
                font_size: None,
                margin: None,
            };
            let out = headers_footers(&pdf, &s, None, &EngineLimits::default()).unwrap();
            assert_eq!(page_count(&out), 1);
        }
    }
}
