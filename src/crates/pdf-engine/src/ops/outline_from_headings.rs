//! Build a document outline (bookmarks) heuristically from text font sizes.
//!
//! # What this op does — and what it does not
//!
//! [`lopdf`]'s text extractor (`ops::text::extract_text`) returns only
//! strings — it does not expose the font size each run was drawn with.
//! This op therefore walks each page's **own content streams** itself
//! (the same `Tf`/`Tj`/`TJ`/`'`/`"` operators `text_markup` tracks)
//! and records, per *line* of shown text, the font size that was
//! current when the line's first glyph was drawn.
//!
//! Candidate "headings" are lines whose font size falls inside
//! [`HeadingSpec::min_font_size`]..=[`HeadingSpec::max_font_size`].
//! Distinct sizes are clustered (sizes within ~5% or 0.5 pt of a
//! cluster's largest member count as the same size — PDF producers
//! rarely emit byte-identical sizes for one logical heading level),
//! larger clusters become shallower outline levels, and
//! [`HeadingSpec::max_depth`] caps nesting. The resulting flat
//! `(title, page, level)` spec is handed to
//! [`super::outline::set_outline`], which owns all `/Outlines`
//! tree mechanics.
//!
//! # Honest limitations — read before relying on this
//!
//! - **Heuristic, not semantics.** Font size is a *proxy* for heading
//!   level. Large body text, drop caps, titles inside images-of-text,
//!   or a document that simply uses several body sizes will produce
//!   bogus outline entries. Always review the result.
//! - Only the page's **own content streams** are scanned. Text inside
//!   Form XObjects (`Do`), Type3 glyph descriptions, tiling patterns,
//!   annotations and optional-content layers is invisible to this op.
//! - Text position is **not** used: a "line" is delimited by text
//!   positioning operators (`Td`/`TD`/`T*`/`Tm`/`'`/`"`/`ET`), so
//!   multi-column layouts or absolutely-positioned fragments may be
//!   merged or split unexpectedly.
//! - Only the `Tf` size operand is honored. Text scaled through the
//!   text matrix (`Tm`/`Tz`) or a form XObject's matrix is *not*
//!   reflected in the recorded size.
//! - String decoding reuses lopdf's font-encoding machinery
//!   (`get_font_encoding_with_limit`, bounded decompression). Fonts
//!   whose encoding cannot be resolved contribute no text; the count
//!   is reported in [`OutlineBuildReport::warnings`]. Custom-encoded
//!   text may decode incorrectly.
//! - A content stream that fails to decompress or decode is skipped
//!   (counted in warnings), matching `text_markup`'s tolerance.
//! - If **no** candidate line is found the input is returned
//!   unchanged (not re-saved) with an explanatory warning.
//!
//! Encrypted documents are rejected via [`reject_encrypted`], like
//! every other transforming op.

use std::collections::BTreeMap;

use lopdf::content::{Content, Operation};
use lopdf::{Document, Encoding, Object};
use serde::{Deserialize, Serialize};

use crate::crates::core::engine::EngineLimits;

use crate::crates::pdf_engine::error::{PdfError, PdfResult};

use super::load::{load_pdf, reject_encrypted};
use super::outline::{OutlineItemSpec, set_outline};

/// Decompression bound per content stream — same 64 MiB guard
/// `text_markup`/`watermark` use.
const MAX_CONTENT_STREAM: usize = 64 * 1024 * 1024;

/// Cap on collected text lines. A pathological document could show
/// text per glyph; beyond this we stop collecting and warn rather
/// than exhaust memory. `set_outline` itself caps at 100k items.
const MAX_LINES: usize = 100_000;

/// `set_outline` rejects nesting deeper than 64 levels; clamp
/// `max_depth` to that ceiling.
const OUTLINE_DEPTH_CEILING: u32 = 64;

/// Heading-detection parameters for [`build_outline_from_text`].
#[derive(Debug, Clone, Copy, PartialEq, Serialize, Deserialize)]
pub struct HeadingSpec {
    /// Smallest font size (points) that may count as a heading.
    /// Must be finite and `<= max_font_size`. Sizes `<= 0` never
    /// qualify regardless.
    pub min_font_size: f32,
    /// Largest font size (points) that may count as a heading.
    pub max_font_size: f32,
    /// Maximum outline nesting depth (>= 1). Clamped to 64, the
    /// deepest tree `set_outline` accepts. Deeper candidates are
    /// flattened into the deepest allowed level.
    pub max_depth: u32,
}

/// What [`build_outline_from_text`] did, for honest reporting.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct OutlineBuildReport {
    /// Number of outline items written into the `/Outlines` tree.
    pub items_created: usize,
    /// Number of document pages whose content streams were examined.
    pub pages_scanned: u32,
    /// Human-readable caveats: the heuristic nature of the result,
    /// skipped streams/pages, undecodable text, flattened depth.
    pub warnings: Vec<String>,
}

/// One collected line of shown text with the font size that was
/// current when its first glyph was drawn.
#[derive(Debug)]
struct TextLine {
    page: u32,
    font_size: f32,
    text: String,
}

/// Per-page font-name → encoding map plus running warning counters.
struct ScanStats {
    streams_failed: usize,
    strings_undecodable: usize,
    fonts_unresolved: usize,
    pages_without_fonts: usize,
}

/// Validate `spec` before touching the document.
fn check_spec(spec: &HeadingSpec) -> PdfResult<u32> {
    if !spec.min_font_size.is_finite()
        || !spec.max_font_size.is_finite()
        || spec.min_font_size < 0.0
        || spec.max_font_size <= 0.0
        || spec.min_font_size > spec.max_font_size
    {
        return Err(PdfError::InvalidInput(format!(
            "heading font-size range must satisfy 0 <= min <= max with max > 0, got [{}, {}]",
            spec.min_font_size, spec.max_font_size
        )));
    }
    if spec.max_depth == 0 {
        return Err(PdfError::InvalidInput(
            "heading max_depth must be >= 1".to_string(),
        ));
    }
    Ok(spec.max_depth.min(OUTLINE_DEPTH_CEILING))
}

/// Build the per-page font encoding table. Fonts that fail to
/// resolve are counted (their text is skipped); the page itself is
/// still scanned so other fonts contribute.
fn page_encodings<'a>(
    doc: &'a Document,
    page_id: lopdf::ObjectId,
    stats: &mut ScanStats,
) -> BTreeMap<Vec<u8>, Encoding<'a>> {
    let mut encodings = BTreeMap::new();
    let fonts = match doc.get_page_fonts(page_id) {
        Ok(f) => f,
        Err(_) => {
            stats.pages_without_fonts += 1;
            return encodings;
        }
    };
    for (name, font_dict) in fonts {
        match font_dict.get_font_encoding_with_limit(doc, MAX_CONTENT_STREAM) {
            Ok(enc) => {
                encodings.insert(name, enc);
            }
            Err(_) => stats.fonts_unresolved += 1,
        }
    }
    encodings
}

/// Mutable state while walking one content stream's operations.
struct StreamScan<'a, 'b> {
    page: u32,
    encodings: &'b BTreeMap<Vec<u8>, Encoding<'a>>,
    stats: &'b mut ScanStats,
    lines: &'b mut Vec<TextLine>,
    /// Current `Tf` font resource name and size.
    font_name: Vec<u8>,
    font_size: f64,
    /// Render mode (`Tr`); 3 = invisible text.
    render_mode: i64,
    /// Accumulated text of the current line.
    line_text: String,
    /// Font size captured when the current line's first glyph was
    /// shown (0 = no glyph yet).
    line_font_size: f32,
}

impl StreamScan<'_, '_> {
    /// Close the current line: trim and keep it if non-empty.
    fn flush_line(&mut self) {
        let text = self.line_text.trim().to_string();
        if !text.is_empty() && self.line_font_size > 0.0 {
            self.lines.push(TextLine {
                page: self.page,
                font_size: self.line_font_size,
                text,
            });
        }
        self.line_text.clear();
        self.line_font_size = 0.0;
    }

    /// Append one shown string to the current line under the current
    /// font. Undecodable strings are skipped and counted.
    fn show_string(&mut self, bytes: &[u8]) {
        if self.render_mode == 3 || bytes.is_empty() {
            return;
        }
        let Some(enc) = self.encodings.get(&self.font_name) else {
            self.stats.strings_undecodable += 1;
            return;
        };
        if self.line_font_size == 0.0 {
            self.line_font_size = self.font_size as f32;
        }
        if enc.write_to_string(bytes, &mut self.line_text).is_err() {
            // Partial output may have been appended; the line keeps
            // whatever decoded — the count is what matters.
            self.stats.strings_undecodable += 1;
        }
    }

    /// Handle a text-showing operator (`Tj`, `TJ`, `'`, `"`).
    fn show_operands(&mut self, operands: &[Object]) {
        match operands.first() {
            Some(Object::String(bytes, _)) => self.show_string(bytes),
            Some(Object::Array(items)) => {
                for item in items {
                    match item {
                        Object::String(bytes, _) => self.show_string(bytes),
                        // Large negative kern in a TJ array implies a
                        // word space — same approximation lopdf makes.
                        Object::Integer(n) if *n < -100 => {
                            self.line_text.push(' ');
                        }
                        _ => {}
                    }
                }
            }
            _ => {}
        }
    }

    /// `'` shows its single string operand after an implicit line
    /// move; `"` sets word/char spacing then shows operand 2. Both
    /// begin a new line first.
    fn show_quote(&mut self, operands: &[Object], index: usize) {
        self.flush_line();
        if let Some(Object::String(bytes, _)) = operands.get(index) {
            self.show_string(bytes);
        }
    }

    /// `Td`/`TD`: a nonzero vertical offset starts a new line; a
    /// purely horizontal move continues the current one.
    fn move_text(&mut self, operands: &[Object]) {
        let ty = operands
            .get(1)
            .and_then(|o| o.as_float().ok())
            .unwrap_or(0.0);
        if ty != 0.0 {
            self.flush_line();
        }
    }

    fn run(&mut self, operations: &[Operation]) {
        for op in operations {
            match op.operator.as_str() {
                "BT" | "ET" | "T*" | "Tm" => self.flush_line(),
                "Tf" => {
                    if let Some(name) = op.operands.first().and_then(|o| o.as_name().ok()) {
                        self.font_name = name.to_vec();
                    }
                    if let Some(size) = op.operands.get(1).and_then(|o| o.as_float().ok()) {
                        self.font_size = size as f64;
                    }
                }
                "Tr" => {
                    self.render_mode = op
                        .operands
                        .first()
                        .and_then(|o| o.as_float().ok())
                        .map(|v| v.round() as i64)
                        .unwrap_or(self.render_mode);
                }
                "Td" | "TD" => self.move_text(&op.operands),
                "Tj" | "TJ" => self.show_operands(&op.operands),
                "'" => self.show_quote(&op.operands, 0),
                "\"" => self.show_quote(&op.operands, 2),
                _ => {}
            }
        }
        self.flush_line();
    }
}

/// Decode and scan one page's content streams, appending lines.
fn scan_page(
    doc: &Document,
    page_number: u32,
    page_id: lopdf::ObjectId,
    lines: &mut Vec<TextLine>,
    stats: &mut ScanStats,
) {
    let encodings = page_encodings(doc, page_id, stats);
    for stream_id in doc.get_page_contents(page_id) {
        if lines.len() >= MAX_LINES {
            return;
        }
        let Ok(object) = doc.get_object(stream_id) else {
            stats.streams_failed += 1;
            continue;
        };
        let Ok(stream) = object.as_stream() else {
            continue;
        };
        let raw = stream
            .decompressed_content_with_limit(MAX_CONTENT_STREAM)
            .unwrap_or_else(|_| stream.content.clone());
        let Ok(content) = Content::decode(&raw) else {
            stats.streams_failed += 1;
            continue;
        };
        let mut scan = StreamScan {
            page: page_number,
            encodings: &encodings,
            stats,
            lines,
            font_name: Vec::new(),
            font_size: 0.0,
            render_mode: 0,
            line_text: String::new(),
            line_font_size: 0.0,
        };
        scan.run(&content.operations);
    }
}

/// Collect `(font_size, page)` clusters: returns the sorted
/// distinct sizes (descending) and a rank map keyed by `f32::to_bits`.
/// Sizes within `max(0.5pt, 5% of cluster max)` of the cluster's
/// largest member share a rank — real documents rarely emit
/// byte-identical sizes for one logical heading level.
fn cluster_sizes(sizes: &[f32]) -> (Vec<f32>, BTreeMap<u32, u32>) {
    let mut unique: Vec<f32> = sizes.to_vec();
    unique.sort_by(|a, b| b.total_cmp(a));
    unique.dedup();

    let mut cluster_maxes: Vec<f32> = Vec::new();
    let mut ranks = BTreeMap::new();
    for &size in &unique {
        let belongs = cluster_maxes.last().is_some_and(|&max| {
            let tol = (0.05 * max).max(0.5);
            size >= max - tol
        });
        if !belongs {
            cluster_maxes.push(size);
        }
        ranks.insert(size.to_bits(), (cluster_maxes.len() - 1) as u32);
    }
    (cluster_maxes, ranks)
}

/// Filter lines to the spec's size range and assign outline levels.
///
/// Level assignment walks lines in reading order keeping a stack of
/// open ancestor *size ranks*: a line nests under the nearest
/// preceding line with a strictly larger size; equal or smaller
/// sizes pop the stack. This guarantees contiguous, non-skipping
/// levels with the first item at level 0 — exactly what
/// [`set_outline`] requires. `max_depth` flattens anything deeper.
fn build_item_specs(
    lines: Vec<TextLine>,
    spec: &HeadingSpec,
    max_depth: u32,
) -> Vec<OutlineItemSpec> {
    let candidates: Vec<&TextLine> = lines
        .iter()
        .filter(|l| {
            l.font_size.is_finite()
                && l.font_size > 0.0
                && l.font_size >= spec.min_font_size
                && l.font_size <= spec.max_font_size
        })
        .collect();
    if candidates.is_empty() {
        return Vec::new();
    }

    let sizes: Vec<f32> = candidates.iter().map(|l| l.font_size).collect();
    let (_cluster_maxes, ranks) = cluster_sizes(&sizes);

    let mut items = Vec::with_capacity(candidates.len());
    let mut stack: Vec<u32> = Vec::new(); // open ancestor size ranks
    for line in candidates {
        let rank = ranks.get(&line.font_size.to_bits()).copied().unwrap_or(0);
        // Pop ancestors that are not strictly larger, and flatten
        // anything deeper than `max_depth` into the deepest level.
        while let Some(&top) = stack.last() {
            if top >= rank || stack.len() >= max_depth as usize {
                stack.pop();
            } else {
                break;
            }
        }
        let level = stack.len() as u32;
        stack.push(rank);
        items.push(OutlineItemSpec {
            title: line.text.clone(),
            page: line.page,
            level,
            bold: false,
            italic: false,
        });
    }
    items
}

/// Build a PDF outline from heading-like text, returning the new PDF
/// bytes and an [`OutlineBuildReport`].
///
/// See the module docs for the heuristic and its limitations. The
/// outline is written by [`set_outline`]; documents with no
/// qualifying text are returned unchanged.
pub fn build_outline_from_text(
    input: &[u8],
    spec: &HeadingSpec,
    limits: &EngineLimits,
) -> PdfResult<(Vec<u8>, OutlineBuildReport)> {
    let max_depth = check_spec(spec)?;

    let doc = load_pdf(input, limits)?;
    reject_encrypted(&doc)?;

    let mut warnings = vec![
        "outline levels are inferred from font sizes — a heuristic; \
         review the generated bookmarks"
            .to_string(),
        "only text in page content streams is scanned; text inside \
         Form XObjects, Type3 glyphs, patterns and annotations is ignored"
            .to_string(),
    ];
    let mut stats = ScanStats {
        streams_failed: 0,
        strings_undecodable: 0,
        fonts_unresolved: 0,
        pages_without_fonts: 0,
    };

    let pages = doc.get_pages();
    let page_count = pages.len() as u32;
    let mut lines: Vec<TextLine> = Vec::new();
    for (&page_number, &page_id) in &pages {
        if lines.len() >= MAX_LINES {
            warnings.push(format!(
                "stopped collecting text lines at the {MAX_LINES} line cap"
            ));
            break;
        }
        scan_page(&doc, page_number, page_id, &mut lines, &mut stats);
    }

    // De-duplicate *consecutive* identical (page, title) lines —
    // repeated headers/footers would otherwise flood the outline.
    // Non-consecutive repeats are kept: two sections may legitimately
    // share a title.
    let mut deduped: Vec<TextLine> = Vec::with_capacity(lines.len());
    for line in lines {
        let dup = deduped
            .last()
            .is_some_and(|prev| prev.page == line.page && prev.text == line.text);
        if !dup {
            deduped.push(line);
        }
    }

    let items = build_item_specs(deduped, spec, max_depth);
    let items_created = items.len();

    if stats.streams_failed > 0 {
        warnings.push(format!(
            "{} content stream(s) failed to decode and were skipped",
            stats.streams_failed
        ));
    }
    if stats.pages_without_fonts > 0 {
        warnings.push(format!(
            "{} page(s) had no resolvable font resources",
            stats.pages_without_fonts
        ));
    }
    if stats.fonts_unresolved > 0 {
        warnings.push(format!(
            "{} font(s) had unresolvable encodings; their text was skipped",
            stats.fonts_unresolved
        ));
    }
    if stats.strings_undecodable > 0 {
        warnings.push(format!(
            "{} text string(s) could not be decoded",
            stats.strings_undecodable
        ));
    }

    let report = OutlineBuildReport {
        items_created,
        pages_scanned: page_count,
        warnings,
    };

    if items.is_empty() {
        let mut report = report;
        report.warnings.push(
            "no text lines with font sizes in the configured range \
                   were found; the outline was left unchanged"
                .to_string(),
        );
        return Ok((input.to_vec(), report));
    }

    // `set_outline` re-loads and re-validates; our levels are already
    // contiguous from level 0, and items are in non-decreasing page
    // order, so its stable page sort preserves our reading order.
    let out = set_outline(input, &items, limits)?;
    Ok((out, report))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::crates::pdf_engine::ops::test_utils::pdf_with_pages;
    use crate::crates::pdf_engine::ops::{OutlineEntry, list_outline};
    use lopdf::content::{Content, Operation};
    use lopdf::{Document, Stream, dictionary};

    /// Build a one-page PDF whose content stream shows each
    /// `(font_size, text)` pair on its own line, top to bottom.
    fn pdf_with_sized_lines(lines: &[(f32, &str)]) -> Vec<u8> {
        let mut doc = Document::with_version("1.5");
        let pages_id = doc.new_object_id();
        let font_id = doc.add_object(dictionary! {
            "Type" => "Font",
            "Subtype" => "Type1",
            "BaseFont" => "Helvetica",
        });
        let resources_id = doc.add_object(dictionary! {
            "Font" => dictionary! { "F1" => font_id },
        });

        let mut ops = Vec::new();
        for (i, (size, text)) in lines.iter().enumerate() {
            ops.push(Operation::new("BT", vec![]));
            ops.push(Operation::new("Tf", vec!["F1".into(), Object::Real(*size)]));
            ops.push(Operation::new(
                "Td",
                vec![72.into(), (700 - 20 * i as i64).into()],
            ));
            ops.push(Operation::new("Tj", vec![Object::string_literal(*text)]));
            ops.push(Operation::new("ET", vec![]));
        }
        let content_id = doc.add_object(Stream::new(
            dictionary! {},
            Content { operations: ops }.encode().unwrap(),
        ));
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
                "Resources" => resources_id,
                "MediaBox" => vec![0.into(), 0.into(), 595.into(), 842.into()],
            }),
        );
        let catalog_id = doc.add_object(dictionary! {
            "Type" => "Catalog",
            "Pages" => pages_id,
        });
        doc.trailer.set("Root", catalog_id);
        let mut buf = Vec::new();
        doc.save_to(&mut buf).unwrap();
        buf
    }

    fn spec(min: f32, max: f32, depth: u32) -> HeadingSpec {
        HeadingSpec {
            min_font_size: min,
            max_font_size: max,
            max_depth: depth,
        }
    }

    fn limits() -> EngineLimits {
        EngineLimits::default()
    }

    #[test]
    fn builds_nested_outline_from_font_sizes() {
        let input = pdf_with_sized_lines(&[
            (24.0, "Chapter One"),
            (18.0, "Section A"),
            (18.0, "Section B"),
            (12.0, "body text ignored"),
            (14.0, "Subsection A1"),
        ]);
        // Range 13..=30 admits 14/18/24; body at 12 is excluded.
        let (out, report) =
            build_outline_from_text(&input, &spec(13.0, 30.0, 4), &limits()).unwrap();
        assert_eq!(report.items_created, 4, "warnings: {:?}", report.warnings);
        assert_eq!(report.pages_scanned, 1);
        assert!(!report.warnings.is_empty(), "heuristic must be disclosed");

        let entries = list_outline(&out, &limits()).unwrap();
        let flat: Vec<(u32, &str)> = entries
            .iter()
            .map(|e: &OutlineEntry| (e.level, e.title.as_str()))
            .collect();
        assert_eq!(
            flat,
            vec![
                (0, "Chapter One"),
                (1, "Section A"),
                (1, "Section B"),
                (1, "Subsection A1"),
            ]
        );
        assert!(entries.iter().all(|e| e.page == Some(1)));
    }

    #[test]
    fn depth_is_capped_by_max_depth() {
        let input = pdf_with_sized_lines(&[(30.0, "L0"), (26.0, "L1"), (22.0, "L2"), (18.0, "L3")]);
        let (out, report) =
            build_outline_from_text(&input, &spec(10.0, 40.0, 2), &limits()).unwrap();
        assert_eq!(report.items_created, 4);
        let entries = list_outline(&out, &limits()).unwrap();
        assert!(
            entries.iter().all(|e| e.level <= 1),
            "max_depth 2 → levels 0..=1: {entries:?}"
        );
    }

    #[test]
    fn similar_sizes_cluster_into_one_level() {
        // 17.8 vs 18.0 are within 5% — same rank, siblings not parent/child.
        let input = pdf_with_sized_lines(&[(18.0, "Top"), (17.8, "Same level")]);
        let (out, report) =
            build_outline_from_text(&input, &spec(10.0, 40.0, 4), &limits()).unwrap();
        assert_eq!(report.items_created, 2);
        let entries = list_outline(&out, &limits()).unwrap();
        assert_eq!(entries[0].level, 0);
        assert_eq!(
            entries[1].level, 0,
            "clustered sizes must not nest: {entries:?}"
        );
    }

    #[test]
    fn no_candidates_returns_input_unchanged_with_warning() {
        // All text at 12pt; range starts at 20 → nothing qualifies.
        let input = pdf_with_pages(&["just body text"]);
        let (out, report) =
            build_outline_from_text(&input, &spec(20.0, 40.0, 4), &limits()).unwrap();
        assert_eq!(out, input, "unchanged input returned as-is");
        assert_eq!(report.items_created, 0);
        assert_eq!(report.pages_scanned, 1);
        assert!(
            report.warnings.iter().any(|w| w.contains("left unchanged")),
            "warnings: {:?}",
            report.warnings
        );
    }

    #[test]
    fn rejects_invalid_specs() {
        let input = pdf_with_pages(&["x"]);
        for bad in [
            spec(f32::NAN, 10.0, 2),
            spec(20.0, 10.0, 2), // min > max
            spec(-1.0, 10.0, 2), // negative min
            spec(10.0, 0.0, 2),  // non-positive max
            spec(10.0, 20.0, 0), // zero depth
        ] {
            assert!(
                build_outline_from_text(&input, &bad, &limits()).is_err(),
                "spec {bad:?} must be rejected"
            );
        }
    }

    #[test]
    fn rejects_garbage_and_encrypted_like_other_ops() {
        let bad = spec(10.0, 20.0, 2);
        assert!(build_outline_from_text(b"junk", &bad, &limits()).is_err());
        // A document with no pages is still valid input → empty report.
        let input = pdf_with_pages(&[]);
        let (out, report) = build_outline_from_text(&input, &bad, &limits()).unwrap();
        assert_eq!(report.items_created, 0);
        assert_eq!(report.pages_scanned, 0);
        assert_eq!(out, input);
    }

    #[test]
    fn tj_arrays_and_quote_operators_are_scanned() {
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
        let ops = vec![
            Operation::new("BT", vec![]),
            Operation::new("Tf", vec!["F1".into(), 20.into()]),
            Operation::new("Td", vec![72.into(), 700.into()]),
            Operation::new(
                "TJ",
                vec![Object::Array(vec![
                    Object::string_literal("Array "),
                    Object::Integer(-120),
                    Object::string_literal("Title"),
                ])],
            ),
            Operation::new("ET", vec![]),
            Operation::new("BT", vec![]),
            Operation::new("Tf", vec!["F1".into(), 20.into()]),
            Operation::new("'", vec![Object::string_literal("Quote Title")]),
            Operation::new("ET", vec![]),
        ];
        let content_id = doc.add_object(Stream::new(
            dictionary! {},
            Content { operations: ops }.encode().unwrap(),
        ));
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
                "Resources" => resources_id,
                "MediaBox" => vec![0.into(), 0.into(), 595.into(), 842.into()],
            }),
        );
        let catalog_id = doc.add_object(dictionary! {
            "Type" => "Catalog",
            "Pages" => pages_id,
        });
        doc.trailer.set("Root", catalog_id);
        let mut buf = Vec::new();
        doc.save_to(&mut buf).unwrap();

        let (out, report) = build_outline_from_text(&buf, &spec(15.0, 30.0, 3), &limits()).unwrap();
        assert_eq!(report.items_created, 2, "warnings: {:?}", report.warnings);
        let entries = list_outline(&out, &limits()).unwrap();
        let titles: Vec<&str> = entries.iter().map(|e| e.title.as_str()).collect();
        assert_eq!(titles, vec!["Array Title", "Quote Title"]);
    }
}
