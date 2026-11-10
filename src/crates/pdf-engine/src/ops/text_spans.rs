//! Positioned text-span extraction and literal text search.
//!
//! [`ops::text::extract_text`](super::text::extract_text) returns plain
//! strings with no geometry. This module walks each page's **own**
//! content streams — the same `Tf`/`Tj`/`TJ`/`'`/`"` operators and
//! text-state tracking (`Tm`/`Td`/`TD`/`T*`/`TL`/`BT`, `cm`, `q`/`Q`)
//! that `extract_tables` uses — and emits one [`TextSpan`] per
//! text-showing run, carrying an approximate `(x, y)` position in PDF
//! user space (origin bottom-left, y grows upward), the font resource
//! name, font size and an estimated advance width.
//!
//! # Honesty notes — read before relying on the output
//!
//! - **Positions and widths are estimates.** Glyph advance is ~0.5 em
//!   per character adjusted by `Tz` and `TJ` kerning; `Tc`, `Tw`,
//!   font-specific widths and the CTM's effect on advance are not
//!   modeled. `height` is just the unscaled `Tf` size — a rough
//!   estimate, not a measured glyph box.
//! - Text inside Form XObjects (`Do`), Type3 glyphs, patterns,
//!   annotations and optional-content layers is **not** reached —
//!   only the page's own content streams.
//! - String decoding reuses lopdf's font-encoding machinery
//!   (`get_font_encoding_with_limit`, bounded decompression), same as
//!   `extract_tables`. When a shown string cannot be decoded — no
//!   resolvable encoding for the current font, or a decode error —
//!   the span is still emitted but flagged [`TextSpan::decoded`] =
//!   `false`, and `text` falls back to a lossy UTF-8 rendering of the
//!   raw bytes, which may be mojibake. Check the flag before trusting
//!   `text`.
//! - [`search_text`] performs a **literal substring** search, not a
//!   regex search: the `regex` crate is not a dependency of this
//!   crate, and adding one was out of scope. `pattern` is matched
//!   byte-for-byte, case-sensitively. Per-page "searchable text" is
//!   reconstructed from spans (newline between differing baselines,
//!   space for gaps), so a match can span adjacent spans on a line
//!   but never crosses a line break.
//!
//! Encrypted documents are rejected via [`reject_encrypted`], like
//! every other inspecting op.

use std::collections::BTreeMap;

use lopdf::content::{Content, Operation};
use lopdf::{Document, Encoding, Object};
use serde::{Deserialize, Serialize};

use crate::crates::core::engine::EngineLimits;

use crate::crates::pdf_engine::error::{PdfError, PdfResult};

use super::load::{load_pdf, reject_encrypted};
use super::pages::parse_page_range;

/// Decompression bound per content stream — same 64 MiB guard
/// `extract_tables`/`text_markup` use.
const MAX_CONTENT_STREAM: usize = 64 * 1024 * 1024;

/// Cap on collected spans across the whole document. A pathological
/// PDF could show text per glyph; beyond this we stop rather than
/// exhaust memory.
const MAX_SPANS: usize = 100_000;

/// Cap on matches returned by [`search_text`].
const MAX_MATCHES: usize = 10_000;

/// Estimated advance per character, in ems (~average glyph width).
const CHAR_ADVANCE_EM: f64 = 0.5;
/// Baselines differing by more than this (pt) start a new line when
/// reconstructing page text for [`search_text`].
const LINE_Y_TOLERANCE: f64 = 2.0;
/// Context characters kept on each side of a [`TextMatch`].
const MATCH_CONTEXT_CHARS: usize = 80;

/// One shown run of text with its approximate position and font.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct TextSpan {
    /// 1-based page number, matching the CLI/MCP contract.
    pub page: u32,
    /// Decoded text of the run. When [`decoded`](Self::decoded) is
    /// `false` this is a lossy rendering of the raw bytes and may be
    /// mojibake — do not trust it.
    pub text: String,
    /// Approximate x of the run's left edge in PDF user space.
    pub x: f64,
    /// Approximate baseline y in PDF user space.
    pub y: f64,
    /// Estimated advance width in user units (unscaled by CTM — an
    /// approximation; see module docs).
    pub width: f64,
    /// Estimated height: the unscaled `Tf` font size at show time.
    /// A rough estimate, not a measured glyph box.
    pub height: f64,
    /// Font resource name in effect (e.g. `"F1"`), as written in the
    /// content stream.
    pub font: String,
    /// The font dictionary's `/BaseFont` name when resolvable
    /// (e.g. `"Helvetica"`); `None` for fonts without one.
    pub base_font: Option<String>,
    /// Font size from the last `Tf` operator, in text space units.
    pub font_size: f64,
    /// `true` when `text` was decoded through the font's encoding;
    /// `false` means decoding failed or was unavailable and `text`
    /// is a lossy fallback.
    pub decoded: bool,
}

/// One literal-substring match found by [`search_text`].
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct TextMatch {
    /// 1-based page number the match was found on.
    pub page: u32,
    /// The matched text (equal to `pattern`).
    pub matched: String,
    /// Character offset of the match start within the page's
    /// reconstructed searchable text.
    pub offset: usize,
    /// Up to [`MATCH_CONTEXT_CHARS`] characters of surrounding page
    /// text on each side of the match.
    pub context: String,
}

// ---- affine matrices ----------------------------------------------------

/// PDF affine matrix `[a b c d e f]` acting on row vectors:
/// `(x, y) -> (a*x + c*y + e, b*x + d*y + f)`.
#[derive(Debug, Clone, Copy)]
struct Mat {
    a: f64,
    b: f64,
    c: f64,
    d: f64,
    e: f64,
    f: f64,
}

impl Mat {
    fn identity() -> Self {
        Self {
            a: 1.0,
            b: 0.0,
            c: 0.0,
            d: 1.0,
            e: 0.0,
            f: 0.0,
        }
    }

    fn translate(tx: f64, ty: f64) -> Self {
        Self {
            a: 1.0,
            b: 0.0,
            c: 0.0,
            d: 1.0,
            e: tx,
            f: ty,
        }
    }

    /// Row-vector product `self * n`: the result applies `self` first,
    /// then `n`.
    fn then(self, n: Mat) -> Mat {
        Mat {
            a: self.a * n.a + self.b * n.c,
            b: self.a * n.b + self.b * n.d,
            c: self.c * n.a + self.d * n.c,
            d: self.c * n.b + self.d * n.d,
            e: self.e * n.a + self.f * n.c + n.e,
            f: self.e * n.b + self.f * n.d + n.f,
        }
    }

    /// `(x, y)` transformed by this matrix.
    fn apply(self, x: f64, y: f64) -> (f64, f64) {
        (
            self.a * x + self.c * y + self.e,
            self.b * x + self.d * y + self.f,
        )
    }
}

// ---- positioned-text scanning --------------------------------------------

/// Per-page font resource name → encoding plus the `/BaseFont` name.
struct PageFonts<'a> {
    encodings: BTreeMap<Vec<u8>, Encoding<'a>>,
    base_fonts: BTreeMap<Vec<u8>, Option<String>>,
}

/// Build the per-page font tables (same approach as
/// `extract_tables::page_encodings`).
fn page_fonts<'a>(doc: &'a Document, page_id: lopdf::ObjectId) -> PageFonts<'a> {
    let mut encodings = BTreeMap::new();
    let mut base_fonts = BTreeMap::new();
    if let Ok(fonts) = doc.get_page_fonts(page_id) {
        for (name, font_dict) in fonts {
            if let Ok(enc) = font_dict.get_font_encoding_with_limit(doc, MAX_CONTENT_STREAM) {
                encodings.insert(name.clone(), enc);
            }
            let base = font_dict
                .get(b"BaseFont")
                .and_then(|o| o.as_name())
                .map(|n| String::from_utf8_lossy(n).into_owned())
                .ok();
            base_fonts.insert(name, base);
        }
    }
    PageFonts {
        encodings,
        base_fonts,
    }
}

/// Mutable text/graphics state while walking one content stream.
struct StreamScan<'a, 'b> {
    fonts: &'b PageFonts<'a>,
    spans: &'b mut Vec<TextSpan>,
    page: u32,
    /// Current `cm`-concatenated CTM (user → device space).
    ctm: Mat,
    /// Saved CTMs for `q`/`Q`.
    ctm_stack: Vec<Mat>,
    /// Text line matrix and text matrix (text space → user space).
    tlm: Mat,
    tm: Mat,
    /// `TL` leading, used by `T*`/`'`/`"`.
    leading: f64,
    /// `Tf` font resource name and size.
    font_name: Vec<u8>,
    font_size: f64,
    /// `Tz` horizontal scaling, percent (default 100).
    hscale: f64,
    cap_hit: bool,
}

impl StreamScan<'_, '_> {
    /// Position of the current text origin in device space.
    fn text_origin(&self) -> (f64, f64) {
        self.tm.then(self.ctm).apply(0.0, 0.0)
    }

    /// Estimated advance (text-space units) for `text` under the
    /// current font: ~0.5 em per char scaled by font size and `Tz`.
    fn estimate_width(&self, text: &str) -> f64 {
        let chars = text.chars().count() as f64;
        chars * CHAR_ADVANCE_EM * self.font_size * (self.hscale / 100.0)
    }

    /// Push one span if under the cap.
    fn push_span(&mut self, text: String, x: f64, y: f64, width: f64, decoded: bool) {
        if text.is_empty() {
            return;
        }
        if self.spans.len() >= MAX_SPANS {
            self.cap_hit = true;
            return;
        }
        self.spans.push(TextSpan {
            page: self.page,
            text,
            x,
            y,
            width,
            height: self.font_size,
            font: String::from_utf8_lossy(&self.font_name).into_owned(),
            base_font: self
                .fonts
                .base_fonts
                .get(&self.font_name)
                .cloned()
                .flatten(),
            font_size: self.font_size,
            decoded,
        });
    }

    /// Decode `bytes` with the current font's encoding. Returns the
    /// decoded text and whether decoding succeeded; with no encoding
    /// the raw bytes are rendered lossily and flagged undecoded.
    fn decode(&self, bytes: &[u8]) -> (String, bool) {
        match self.fonts.encodings.get(&self.font_name) {
            Some(enc) => {
                let mut text = String::new();
                match enc.write_to_string(bytes, &mut text) {
                    Ok(()) => (text, true),
                    Err(_) => (text, false),
                }
            }
            None => (String::from_utf8_lossy(bytes).into_owned(), false),
        }
    }

    /// Advance the text matrix by `width` text-space units along the
    /// text x-axis.
    fn advance_tm(&mut self, width: f64) {
        if width != 0.0 {
            self.tm = Mat::translate(width, 0.0).then(self.tm);
        }
    }

    /// `Tj`: show one string as one span.
    fn op_tj(&mut self, operands: &[Object]) {
        if let Some(Object::String(bytes, _)) = operands.first() {
            if bytes.is_empty() {
                return;
            }
            let (x, y) = self.text_origin();
            let (text, decoded) = self.decode(bytes);
            let width = self.estimate_width(&text);
            self.push_span(text, x, y, width, decoded);
            self.advance_tm(width);
        }
    }

    /// `TJ`: show an array of strings and kerning numbers as ONE
    /// logical run — a single span at the run's start position.
    /// Large negative kerns imply a word space (same approximation
    /// `extract_tables` uses).
    fn op_tj_array(&mut self, operands: &[Object]) {
        let Some(Object::Array(items)) = operands.first() else {
            return;
        };
        let (x, y) = self.text_origin();
        let mut text = String::new();
        let mut decoded = true;
        let mut width = 0.0;
        for item in items {
            match item {
                Object::String(bytes, _) => {
                    if bytes.is_empty() {
                        continue;
                    }
                    let (piece, ok) = self.decode(bytes);
                    decoded &= ok;
                    width += self.estimate_width(&piece);
                    text.push_str(&piece);
                }
                // TJ numbers are 1/1000 em, subtracted from the
                // current position: positive = tighter, negative =
                // looser.
                Object::Integer(n) => {
                    let adj = -(*n as f64) / 1000.0 * self.font_size * (self.hscale / 100.0);
                    width += adj;
                    if *n < -100 && !text.ends_with(' ') {
                        text.push(' ');
                    }
                }
                Object::Real(n) => {
                    let adj = -(*n as f64) / 1000.0 * self.font_size * (self.hscale / 100.0);
                    width += adj;
                    if *n < -100.0 && !text.ends_with(' ') {
                        text.push(' ');
                    }
                }
                _ => {}
            }
        }
        self.push_span(text, x, y, width, decoded);
        self.advance_tm(width);
    }

    /// `Td`/`TD`: translate the line matrix by `(tx, ty)` in text
    /// space, then copy to the text matrix. `TD` also sets leading.
    fn op_move(&mut self, operands: &[Object], set_leading: bool) {
        let tx = operands
            .first()
            .and_then(|o| o.as_float().ok())
            .map(|v| v as f64)
            .unwrap_or(0.0);
        let ty = operands
            .get(1)
            .and_then(|o| o.as_float().ok())
            .map(|v| v as f64)
            .unwrap_or(0.0);
        self.tlm = Mat::translate(tx, ty).then(self.tlm);
        self.tm = self.tlm;
        if set_leading {
            self.leading = -ty;
        }
    }

    /// `T*`: move to the next line using the leading set by `TL`/`TD`.
    fn op_next_line(&mut self) {
        self.tlm = Mat::translate(0.0, -self.leading).then(self.tlm);
        self.tm = self.tlm;
    }

    /// `'`/`"`: implicit `T*` then show the string operand
    /// (operand 0 for `'`, operand 2 for `"`).
    fn op_quote(&mut self, operands: &[Object], index: usize) {
        self.op_next_line();
        self.op_tj(std::slice::from_ref(
            operands.get(index).unwrap_or(&Object::Null),
        ));
    }

    fn run(&mut self, operations: &[Operation]) {
        for op in operations {
            match op.operator.as_str() {
                "q" => self.ctm_stack.push(self.ctm),
                "Q" => {
                    if let Some(saved) = self.ctm_stack.pop() {
                        self.ctm = saved;
                    }
                }
                "cm" => {
                    let v: Vec<f64> = op
                        .operands
                        .iter()
                        .filter_map(|o| o.as_float().ok().map(|v| v as f64))
                        .collect();
                    if v.len() == 6 {
                        let m = Mat {
                            a: v[0],
                            b: v[1],
                            c: v[2],
                            d: v[3],
                            e: v[4],
                            f: v[5],
                        };
                        // `cm` concatenates so the new matrix applies
                        // to coordinates first (innermost transform).
                        self.ctm = m.then(self.ctm);
                    }
                }
                "BT" => {
                    self.tlm = Mat::identity();
                    self.tm = Mat::identity();
                }
                "Tf" => {
                    if let Some(name) = op.operands.first().and_then(|o| o.as_name().ok()) {
                        self.font_name = name.to_vec();
                    }
                    if let Some(size) = op
                        .operands
                        .get(1)
                        .and_then(|o| o.as_float().ok())
                        .map(|v| v as f64)
                    {
                        self.font_size = size;
                    }
                }
                "Tz" => {
                    if let Some(v) = op
                        .operands
                        .first()
                        .and_then(|o| o.as_float().ok())
                        .map(|v| v as f64)
                    {
                        self.hscale = v;
                    }
                }
                "TL" => {
                    self.leading = op
                        .operands
                        .first()
                        .and_then(|o| o.as_float().ok())
                        .map(|v| v as f64)
                        .unwrap_or(self.leading);
                }
                "Td" => self.op_move(&op.operands, false),
                "TD" => self.op_move(&op.operands, true),
                "T*" => self.op_next_line(),
                "Tm" => {
                    let v: Vec<f64> = op
                        .operands
                        .iter()
                        .filter_map(|o| o.as_float().ok().map(|v| v as f64))
                        .collect();
                    if v.len() == 6 {
                        self.tm = Mat {
                            a: v[0],
                            b: v[1],
                            c: v[2],
                            d: v[3],
                            e: v[4],
                            f: v[5],
                        };
                        self.tlm = self.tm;
                    }
                }
                "Tj" => self.op_tj(&op.operands),
                "TJ" => self.op_tj_array(&op.operands),
                "'" => self.op_quote(&op.operands, 0),
                "\"" => self.op_quote(&op.operands, 2),
                _ => {}
            }
            if self.cap_hit {
                return;
            }
        }
    }
}

/// Decode and scan one page's content streams, appending spans.
fn scan_page(
    doc: &Document,
    page_number: u32,
    page_id: lopdf::ObjectId,
    spans: &mut Vec<TextSpan>,
) {
    let fonts = page_fonts(doc, page_id);
    for stream_id in doc.get_page_contents(page_id) {
        if spans.len() >= MAX_SPANS {
            return;
        }
        let Ok(object) = doc.get_object(stream_id) else {
            continue;
        };
        let Ok(stream) = object.as_stream() else {
            continue;
        };
        let raw = stream
            .decompressed_content_with_limit(MAX_CONTENT_STREAM)
            .unwrap_or_else(|_| stream.content.clone());
        let Ok(content) = Content::decode(&raw) else {
            continue;
        };
        let mut scan = StreamScan {
            fonts: &fonts,
            spans,
            page: page_number,
            ctm: Mat::identity(),
            ctm_stack: Vec::new(),
            tlm: Mat::identity(),
            tm: Mat::identity(),
            leading: 0.0,
            font_name: Vec::new(),
            font_size: 0.0,
            hscale: 100.0,
            cap_hit: false,
        };
        scan.run(&content.operations);
        if scan.cap_hit {
            return;
        }
    }
}

/// Extract positioned text spans from `input`, one [`TextSpan`] per
/// text-showing run, for the pages selected by `pages` (`None` = all).
///
/// Pages are returned in ascending order; spans within a page follow
/// content-stream order. Encrypted documents are rejected.
pub fn extract_spans(
    input: &[u8],
    pages: Option<&str>,
    limits: &EngineLimits,
) -> PdfResult<Vec<TextSpan>> {
    let doc = load_pdf(input, limits)?;
    reject_encrypted(&doc)?;

    let pages_map = doc.get_pages();
    let page_count = pages_map.len() as u32;
    let selected = match pages {
        Some(spec) => parse_page_range(spec, page_count)?,
        None => (1..=page_count).collect(),
    };

    let mut spans = Vec::new();
    for page_number in selected {
        let page_id = pages_map[&page_number];
        scan_page(&doc, page_number, page_id, &mut spans);
        if spans.len() >= MAX_SPANS {
            break;
        }
    }
    Ok(spans)
}

// ---- literal search ------------------------------------------------------

/// Reconstruct one page's searchable text from its spans, in
/// content-stream order: a newline starts a new baseline, a space is
/// inserted for a positive x gap on the same baseline. Heuristic —
/// see module docs.
fn build_page_text(spans: &[TextSpan]) -> String {
    let mut out = String::new();
    let mut prev_y: Option<f64> = None;
    let mut prev_end_x: Option<f64> = None;
    for span in spans {
        if span.text.is_empty() {
            continue;
        }
        let same_line = prev_y.is_some_and(|y| (span.y - y).abs() <= LINE_Y_TOLERANCE);
        if same_line {
            if let Some(end_x) = prev_end_x
                && span.x - end_x > 1.0
            {
                out.push(' ');
            }
        } else if !out.is_empty() {
            out.push('\n');
        }
        out.push_str(&span.text);
        prev_y = Some(span.y);
        prev_end_x = Some(span.x + span.width);
    }
    out
}

/// Extract up to `MATCH_CONTEXT_CHARS` characters on each side of the
/// match starting at byte `match_start` (which lies on a char
/// boundary because `pattern` matched there).
fn match_context(page_text: &str, match_start: usize, match_len: usize) -> (usize, String) {
    let before: Vec<(usize, char)> = page_text[..match_start].char_indices().collect::<Vec<_>>();
    let start_char = before.len().saturating_sub(MATCH_CONTEXT_CHARS);
    let ctx_start_byte = before.get(start_char).map(|(i, _)| *i).unwrap_or(0);

    let after_start = match_start + match_len;
    let mut end_byte = page_text.len();
    let mut taken = 0usize;
    for (i, _) in page_text[after_start..].char_indices() {
        if taken >= MATCH_CONTEXT_CHARS {
            end_byte = after_start + i;
            break;
        }
        taken += 1;
    }

    // `before.len()` is the number of chars before the match, i.e. the
    // match's char offset within the page text.
    (
        before.len(),
        page_text[ctx_start_byte..end_byte].to_string(),
    )
}

/// Search the extracted text of the selected pages for `pattern`.
///
/// `pattern` is a **literal substring** — matched byte-for-byte and
/// case-sensitively. The `regex` crate is not a dependency of this
/// crate, so no regex syntax is supported; an empty pattern is
/// rejected with [`PdfError::InvalidInput`]. At most
/// [`MAX_MATCHES`] matches are returned; searching stops once the cap
/// is hit. Encrypted documents are rejected.
pub fn search_text(
    input: &[u8],
    pattern: &str,
    pages: Option<&str>,
    limits: &EngineLimits,
) -> PdfResult<Vec<TextMatch>> {
    if pattern.is_empty() {
        return Err(PdfError::InvalidInput(
            "search pattern must not be empty".to_string(),
        ));
    }

    let doc = load_pdf(input, limits)?;
    reject_encrypted(&doc)?;

    let pages_map = doc.get_pages();
    let page_count = pages_map.len() as u32;
    let selected = match pages {
        Some(spec) => parse_page_range(spec, page_count)?,
        None => (1..=page_count).collect(),
    };

    let mut matches = Vec::new();
    'pages: for page_number in selected {
        let page_id = pages_map[&page_number];
        let mut spans = Vec::new();
        scan_page(&doc, page_number, page_id, &mut spans);
        let page_text = build_page_text(&spans);
        for (byte_start, _) in page_text.match_indices(pattern) {
            let (offset, context) = match_context(&page_text, byte_start, pattern.len());
            matches.push(TextMatch {
                page: page_number,
                matched: pattern.to_string(),
                offset,
                context,
            });
            if matches.len() >= MAX_MATCHES {
                break 'pages;
            }
        }
    }
    Ok(matches)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::crates::pdf_engine::error::PdfError;
    use crate::crates::pdf_engine::ops::encrypt::{PdfPermissions, encrypt};
    use crate::crates::pdf_engine::ops::test_utils::pdf_with_pages;
    use lopdf::Document;
    use lopdf::content::{Content, Operation};
    use lopdf::{Stream, dictionary};

    #[test]
    fn extracts_spans_with_positions_and_font() {
        let pdf = pdf_with_pages(&["Hello World"]);
        let spans = extract_spans(&pdf, None, &EngineLimits::default()).unwrap();
        assert_eq!(spans.len(), 1);
        let span = &spans[0];
        assert_eq!(span.page, 1);
        assert_eq!(span.text, "Hello World");
        assert_eq!(span.font, "F1");
        assert_eq!(span.base_font.as_deref(), Some("Courier"));
        assert_eq!(span.font_size, 12.0);
        assert!(span.decoded);
        // pdf_with_pages places the text at Td (100, 700).
        assert!((span.x - 100.0).abs() < 0.01, "x={}", span.x);
        assert!((span.y - 700.0).abs() < 0.01, "y={}", span.y);
        assert!(span.width > 0.0);
        assert_eq!(span.height, 12.0);
    }

    #[test]
    fn honors_page_subset() {
        let pdf = pdf_with_pages(&["first page", "second page", "third page"]);
        let spans = extract_spans(&pdf, Some("2,3"), &EngineLimits::default()).unwrap();
        assert_eq!(spans.len(), 2);
        assert_eq!(spans[0].page, 2);
        assert_eq!(spans[1].page, 3);
        assert!(spans[0].text.contains("second"));
    }

    #[test]
    fn search_finds_literal_matches_with_context() {
        let pdf = pdf_with_pages(&["the quick brown fox", "lazy dog sleeps"]);
        let matches = search_text(&pdf, "brown", None, &EngineLimits::default()).unwrap();
        assert_eq!(matches.len(), 1);
        assert_eq!(matches[0].page, 1);
        assert_eq!(matches[0].matched, "brown");
        assert!(
            matches[0].context.contains("quick"),
            "{:?}",
            matches[0].context
        );
        assert!(
            matches[0].context.contains("fox"),
            "{:?}",
            matches[0].context
        );

        // Literal only: regex metacharacters are not special.
        let matches = search_text(&pdf, "q.ck", None, &EngineLimits::default()).unwrap();
        assert!(matches.is_empty());

        // Case-sensitive.
        assert!(
            search_text(&pdf, "BROWN", None, &EngineLimits::default())
                .unwrap()
                .is_empty()
        );
    }

    #[test]
    fn search_honors_page_subset_and_rejects_empty_pattern() {
        let pdf = pdf_with_pages(&["alpha beta", "alpha gamma"]);
        let matches = search_text(&pdf, "alpha", Some("2"), &EngineLimits::default()).unwrap();
        assert_eq!(matches.len(), 1);
        assert_eq!(matches[0].page, 2);

        let err = search_text(&pdf, "", None, &EngineLimits::default()).unwrap_err();
        assert!(matches!(err, PdfError::InvalidInput(_)), "got {err:?}");
    }

    #[test]
    fn rejects_invalid_range_and_bad_input() {
        let pdf = pdf_with_pages(&["x"]);
        for spec in ["", "0", "2"] {
            assert!(extract_spans(&pdf, Some(spec), &EngineLimits::default()).is_err());
            assert!(search_text(&pdf, "x", Some(spec), &EngineLimits::default()).is_err());
        }
        assert!(extract_spans(b"junk", None, &EngineLimits::default()).is_err());
        assert!(search_text(b"junk", "x", None, &EngineLimits::default()).is_err());
    }

    #[test]
    fn enforces_limits_and_rejects_encrypted() {
        let pdf = pdf_with_pages(&["p1", "p2"]);
        let limits = EngineLimits {
            max_pages: Some(1),
            ..EngineLimits::default()
        };
        let err = extract_spans(&pdf, None, &limits).unwrap_err();
        assert!(matches!(err, PdfError::LimitExceeded(_)), "got {err:?}");
        let err = search_text(&pdf, "p", None, &limits).unwrap_err();
        assert!(matches!(err, PdfError::LimitExceeded(_)), "got {err:?}");

        let encrypted = encrypt(
            &pdf,
            "pw",
            None,
            PdfPermissions::default(),
            &EngineLimits::default(),
        )
        .unwrap();
        let err = extract_spans(&encrypted, None, &EngineLimits::default()).unwrap_err();
        assert!(matches!(err, PdfError::Encrypted), "got {err:?}");
        let err = search_text(&encrypted, "p", None, &EngineLimits::default()).unwrap_err();
        assert!(matches!(err, PdfError::Encrypted), "got {err:?}");
    }

    #[test]
    fn tj_array_and_quote_ops_produce_spans() {
        // Build a one-page PDF exercising TJ, ', and " operators.
        let mut doc = Document::with_version("1.5");
        let pages_id = doc.new_object_id();
        let font_id = doc.add_object(dictionary! {
            "Type" => "Font",
            "Subtype" => "Type1",
            "BaseFont" => "Courier",
        });
        let resources_id = doc.add_object(dictionary! {
            "Font" => dictionary! { "F1" => font_id },
        });
        let content = Content {
            operations: vec![
                Operation::new("BT", vec![]),
                Operation::new("Tf", vec!["F1".into(), 12.into()]),
                Operation::new("Td", vec![100.into(), 700.into()]),
                Operation::new("TL", vec![14.into()]),
                Operation::new(
                    "TJ",
                    vec![Object::Array(vec![
                        Object::string_literal("ker-"),
                        Object::Integer(-250),
                        Object::string_literal("ned"),
                    ])],
                ),
                Operation::new("'", vec![Object::string_literal("next line")]),
                Operation::new(
                    "\"",
                    vec![0.into(), 0.into(), Object::string_literal("quoted")],
                ),
                Operation::new("ET", vec![]),
            ],
        };
        let content_id = doc.add_object(Stream::new(dictionary! {}, content.encode().unwrap()));
        let page_id = doc.add_object(dictionary! {
            "Type" => "Page",
            "Parent" => pages_id,
            "Contents" => content_id,
            "Resources" => resources_id,
            "MediaBox" => vec![0.into(), 0.into(), 595.into(), 842.into()],
        });
        doc.objects.insert(
            pages_id,
            Object::Dictionary(dictionary! {
                "Type" => "Pages",
                "Kids" => vec![Object::Reference(page_id)],
                "Count" => 1,
            }),
        );
        let catalog_id = doc.add_object(dictionary! {
            "Type" => "Catalog",
            "Pages" => pages_id,
        });
        doc.trailer.set("Root", catalog_id);
        let mut buf = Vec::new();
        doc.save_to(&mut buf).unwrap();

        let spans = extract_spans(&buf, None, &EngineLimits::default()).unwrap();
        assert_eq!(spans.len(), 3, "spans: {spans:?}");
        assert!(spans[0].text.contains("ker"), "{:?}", spans[0].text);
        assert!(spans[0].text.contains("ned"), "{:?}", spans[0].text);
        assert_eq!(spans[1].text, "next line");
        assert_eq!(spans[2].text, "quoted");
        // ' and " moved to a new line, so y should differ from the first.
        assert_ne!(spans[0].y, spans[1].y);
    }
}
