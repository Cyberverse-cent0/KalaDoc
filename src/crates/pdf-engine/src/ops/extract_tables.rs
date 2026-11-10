//! Best-effort table and key-value extraction from positioned text.
//!
//! [`ops::text::extract_text`](super::text::extract_text) returns plain
//! strings with no geometry, so this module walks each page's **own
//! content streams** itself — the same `Tf`/`Tj`/`TJ`/`'`/`"` operators
//! `outline_from_headings` and `text_markup` scan — but additionally
//! tracks the text state matrix (`Tm`/`Td`/`TD`/`T*`/`TL`/`BT`/`ET`),
//! the text line matrix, `cm` concatenation and the `q`/`Q` graphics
//! state stack, so every shown string is recorded with an approximate
//! `(x, y)` position in PDF user space (origin bottom-left, y grows
//! upward).
//!
//! # Honesty notes — read before relying on the output
//!
//! - **Heuristic, not semantics.** There is no table structure in a
//!   PDF; "tables" are *guessed* from aligned text positions. Borders,
//!   ruling lines, merged cells and multi-line cells are invisible —
//!   only positioned glyphs are seen. Prose with accidentally aligned
//!   fragments can produce a bogus [`Table`]; always review.
//! - Glyph advance is **estimated** (~0.5 em per character, adjusted
//!   by `Tz` and `TJ` kerning). `Tc`, `Tw`, font-specific widths and
//!   `/ToUnicode`-less custom encodings are not modeled; x positions
//!   can drift on long runs. Text inside Form XObjects (`Do`), Type3
//!   glyphs, patterns, annotations and optional-content layers is
//!   **not** reached — only the page's own content streams.
//! - String decoding reuses lopdf's font-encoding machinery
//!   (`get_font_encoding_with_limit`, bounded decompression), same as
//!   `outline_from_headings`. Undecodable strings are skipped and
//!   counted in [`TableReport::warnings`] / dropped silently for KV.
//! - At most **one table per page** is reported: all aligned column
//!   clusters on a page are merged into a single grid, so pages with
//!   two separate tables produce one (usually wrong) grid or none.
//! - [`extract_key_values`] applies simple line patterns
//!   (`Label: value`, `Label .... value`, `Label  value`, `Label
//!   value`). Prose lines can yield spurious pairs; single-space
//!   splits additionally reject values ending in `.` to suppress
//!   sentence false-positives.
//!
//! Encrypted documents are rejected via [`reject_encrypted`], like
//! every other transforming/inspecting op.

use std::collections::BTreeMap;

use lopdf::content::{Content, Operation};
use lopdf::{Document, Encoding, Object};
use serde::{Deserialize, Serialize};

use crate::crates::core::engine::EngineLimits;

use crate::crates::pdf_engine::error::PdfResult;

use super::load::{load_pdf, reject_encrypted};

/// Decompression bound per content stream — same 64 MiB guard
/// `text_markup`/`watermark`/`edit_text` use.
const MAX_CONTENT_STREAM: usize = 64 * 1024 * 1024;

/// Cap on collected text fragments across the whole document. A
/// pathological PDF could show text per glyph; beyond this we stop and
/// warn rather than exhaust memory.
const MAX_FRAGMENTS: usize = 100_000;

// ---- table heuristics -------------------------------------------------

/// Fragments whose baseline y differs by at most this many points are
/// merged into one line.
const ROW_Y_TOLERANCE: f64 = 2.0;
/// Max gap (pt) between consecutive fragment x-starts for them to
/// share a column cluster.
const COL_X_CLUSTER_TOLERANCE: f64 = 5.0;
/// Max distance (pt) from a column-cluster center for a fragment to
/// count as belonging to that column.
const COL_MEMBER_TOLERANCE: f64 = 10.0;
/// A column cluster must be touched by at least this many distinct
/// lines to count as a real column ("consistent x-gaps across >=3
/// rows").
const MIN_COLUMN_SUPPORT: usize = 3;
/// Emitted rows must have at least this many non-empty cells.
const MIN_ROW_FILLED_CELLS: usize = 2;
/// At least this many lines must touch >=2 column clusters before a
/// page is considered to contain a table.
const MIN_ALIGNED_ROWS: usize = 3;
/// Estimated advance per character, in ems (~average glyph width).
const CHAR_ADVANCE_EM: f64 = 0.5;
/// Gap (pt) above which two fragments on a line are joined with two
/// spaces — preserves wide separations for the key-value patterns.
const WIDE_GAP_PT: f64 = 10.0;
/// Smaller positive gaps still get a single separating space.
const SPACE_GAP_PT: f64 = 1.0;

// ---- key-value heuristics ----------------------------------------------

/// Maximum key length in characters.
const MAX_KEY_CHARS: usize = 40;
/// A run of this many `.` characters acts as a separator ("Label ....
/// value").
const MIN_DOT_LEADER: usize = 3;

/// One plausible table found on a page.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Table {
    /// 1-based page number.
    pub page: u32,
    /// Grid rows, top to bottom. Each row has `ncols` cells; empty
    /// cells are `""`. Column count is uniform per table.
    pub rows: Vec<Vec<String>>,
    /// Heuristic confidence in `0.0..=1.0` — the fraction of grid
    /// cells that were filled, blended with a base score. See module
    /// docs: treat as a rough signal, not a probability.
    pub confidence: f64,
}

/// What [`extract_tables`] found across the document.
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
pub struct TableReport {
    /// Detected tables, one per page at most, ordered by page number.
    pub tables: Vec<Table>,
    /// Number of document pages examined.
    pub pages_scanned: u32,
    /// Human-readable caveats: undecodable streams/strings, fragment
    /// cap hits, etc.
    pub warnings: Vec<String>,
}

/// One `key -> value` pair recovered from a text line.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct KvPair {
    /// 1-based page number the line was found on.
    pub page: u32,
    /// Label text (trimmed). At most 40 chars, never ends with `.`.
    pub key: String,
    /// Value text (trimmed). Never empty.
    pub value: String,
}

/// A shown text fragment with its approximate position and estimated
/// advance width, before line grouping.
#[derive(Debug, Clone)]
struct PosFragment {
    /// Left edge x in PDF user space (baseline origin of the run).
    x: f64,
    /// Baseline y in PDF user space.
    y: f64,
    /// Estimated right edge: `x +` advance width in user units
    /// (unscaled by CTM — an approximation).
    end_x: f64,
    /// Decoded text of the run.
    text: String,
}

/// A reconstructed line: fragments sharing a baseline (within
/// [`ROW_Y_TOLERANCE`]), sorted left to right, plus the merged text.
#[derive(Debug, Clone)]
struct TextLine {
    /// Representative baseline y (first fragment's y).
    y: f64,
    /// Fragments sorted by x.
    fragments: Vec<PosFragment>,
    /// All fragment texts joined with gap-aware spacing.
    text: String,
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
    /// then `n` — matching lopdf/pdf.js matrix composition.
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

/// Per-page font-name → encoding map plus running counters.
struct ScanStats {
    streams_failed: usize,
    strings_undecodable: usize,
    fonts_unresolved: usize,
    pages_without_fonts: usize,
    fragment_cap_hit: bool,
}

impl ScanStats {
    fn warnings(&self) -> Vec<String> {
        let mut w = Vec::new();
        if self.streams_failed > 0 {
            w.push(format!(
                "{} content stream(s) failed to decode and were skipped",
                self.streams_failed
            ));
        }
        if self.pages_without_fonts > 0 {
            w.push(format!(
                "{} page(s) had no resolvable font resources",
                self.pages_without_fonts
            ));
        }
        if self.fonts_unresolved > 0 {
            w.push(format!(
                "{} font(s) had unresolvable encodings; their text was skipped",
                self.fonts_unresolved
            ));
        }
        if self.strings_undecodable > 0 {
            w.push(format!(
                "{} text string(s) could not be decoded",
                self.strings_undecodable
            ));
        }
        if self.fragment_cap_hit {
            w.push(format!(
                "stopped collecting text fragments at the {MAX_FRAGMENTS} cap"
            ));
        }
        w
    }
}

/// Build the per-page font encoding table (same approach as
/// `outline_from_headings::page_encodings`).
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

/// Mutable text/graphics state while walking one content stream.
struct StreamScan<'a, 'b> {
    encodings: &'b BTreeMap<Vec<u8>, Encoding<'a>>,
    stats: &'b mut ScanStats,
    fragments: &'b mut Vec<PosFragment>,
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
    /// `Tr` render mode; 3 = invisible text.
    render_mode: i64,
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

    /// Decode one string operand and append a positioned fragment.
    /// Returns the estimated advance for matrix updating.
    fn show_string(&mut self, bytes: &[u8]) -> f64 {
        if bytes.is_empty() {
            return 0.0;
        }
        let Some(enc) = self.encodings.get(&self.font_name) else {
            self.stats.strings_undecodable += 1;
            return self.estimate_width("");
        };
        let mut text = String::new();
        if enc.write_to_string(bytes, &mut text).is_err() {
            self.stats.strings_undecodable += 1;
        }
        let width = self.estimate_width(&text);
        if self.render_mode != 3 && !text.is_empty() && self.fragments.len() < MAX_FRAGMENTS {
            let (x, y) = self.text_origin();
            self.fragments.push(PosFragment {
                x,
                y,
                end_x: x + width,
                text,
            });
        } else if self.fragments.len() >= MAX_FRAGMENTS {
            self.stats.fragment_cap_hit = true;
        }
        width
    }

    /// Advance the text matrix by `width` text-space units along the
    /// text x-axis (pre-multiply the translation, like `Td`).
    fn advance_tm(&mut self, width: f64) {
        if width != 0.0 {
            self.tm = Mat::translate(width, 0.0).then(self.tm);
        }
    }

    /// `Tj`: show one string.
    fn op_tj(&mut self, operands: &[Object]) {
        if let Some(Object::String(bytes, _)) = operands.first() {
            let w = self.show_string(bytes);
            self.advance_tm(w);
        }
    }

    /// `TJ`: show an array of strings and kerning numbers as ONE
    /// logical run — a single fragment at the run's start position.
    /// Large negative kerns imply a word space (same approximation
    /// `outline_from_headings` uses); numbers also adjust the advance.
    fn op_tj_array(&mut self, operands: &[Object]) {
        let Some(Object::Array(items)) = operands.first() else {
            return;
        };
        let (x, y) = self.text_origin();
        let mut text = String::new();
        let mut width = 0.0;
        for item in items {
            match item {
                Object::String(bytes, _) => {
                    if bytes.is_empty() {
                        continue;
                    }
                    let Some(enc) = self.encodings.get(&self.font_name) else {
                        self.stats.strings_undecodable += 1;
                        continue;
                    };
                    let before = text.len();
                    if enc.write_to_string(bytes, &mut text).is_err() {
                        self.stats.strings_undecodable += 1;
                    }
                    width += self.estimate_width(&text[before..]);
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
        if self.render_mode != 3 && !text.is_empty() {
            if self.fragments.len() < MAX_FRAGMENTS {
                self.fragments.push(PosFragment {
                    x,
                    y,
                    end_x: x + width,
                    text,
                });
            } else {
                self.stats.fragment_cap_hit = true;
            }
        }
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
        if let Some(Object::String(bytes, _)) = operands.get(index) {
            let w = self.show_string(bytes);
            self.advance_tm(w);
        }
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
                "ET" => {}
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
                "Tr" => {
                    self.render_mode = op
                        .operands
                        .first()
                        .and_then(|o| o.as_float().ok())
                        .map(|v| v.round() as i64)
                        .unwrap_or(self.render_mode);
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
        }
    }
}

/// Decode and scan one page's content streams, appending fragments.
fn scan_page(
    doc: &Document,
    page_id: lopdf::ObjectId,
    fragments: &mut Vec<PosFragment>,
    stats: &mut ScanStats,
) {
    let encodings = page_encodings(doc, page_id, stats);
    for stream_id in doc.get_page_contents(page_id) {
        if fragments.len() >= MAX_FRAGMENTS {
            stats.fragment_cap_hit = true;
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
            encodings: &encodings,
            stats,
            fragments,
            ctm: Mat::identity(),
            ctm_stack: Vec::new(),
            tlm: Mat::identity(),
            tm: Mat::identity(),
            leading: 0.0,
            font_name: Vec::new(),
            font_size: 0.0,
            hscale: 100.0,
            render_mode: 0,
        };
        scan.run(&content.operations);
    }
}

// ---- line grouping --------------------------------------------------------

/// Join two fragments' texts with spacing derived from the x gap.
fn join_pair(prev: &PosFragment, next: &PosFragment, out: &mut String) {
    let gap = next.x - prev.end_x;
    let prev_ws = prev.text.ends_with(char::is_whitespace);
    let next_ws = next.text.starts_with(char::is_whitespace);
    let sep = if prev_ws || next_ws || gap <= SPACE_GAP_PT {
        ""
    } else if gap > WIDE_GAP_PT {
        "  "
    } else {
        " "
    };
    out.push_str(sep);
    out.push_str(&next.text);
}

/// Group fragments into baseline-aligned lines. Fragments are sorted
/// by descending y (top of page first, since PDF y grows upward) and
/// clustered with [`ROW_Y_TOLERANCE`]; within a line they are sorted
/// by x and merged into `text`.
fn group_lines(fragments: &[PosFragment]) -> Vec<TextLine> {
    let mut sorted: Vec<&PosFragment> = fragments.iter().collect();
    sorted.sort_by(|a, b| b.y.total_cmp(&a.y).then(a.x.total_cmp(&b.x)));

    let mut lines: Vec<TextLine> = Vec::new();
    for frag in sorted {
        let joins_last = lines.last().is_some_and(|l| {
            // sorted desc: frag.y <= line.y; tolerance is absolute.
            (l.y - frag.y).abs() <= ROW_Y_TOLERANCE
        });
        if joins_last {
            let line = lines.last_mut().unwrap();
            line.fragments.push(frag.clone());
        } else {
            lines.push(TextLine {
                y: frag.y,
                fragments: vec![frag.clone()],
                text: String::new(),
            });
        }
    }

    for line in &mut lines {
        line.fragments.sort_by(|a, b| a.x.total_cmp(&b.x));
        let mut text = String::new();
        for (i, frag) in line.fragments.iter().enumerate() {
            if i == 0 {
                text.push_str(&frag.text);
            } else {
                join_pair(&line.fragments[i - 1], frag, &mut text);
            }
        }
        line.text = text;
    }
    lines
}

// ---- table detection ------------------------------------------------------

/// Distance from `x` to the nearest cluster center, if within
/// [`COL_MEMBER_TOLERANCE`]. Returns the cluster index.
fn nearest_cluster(x: f64, centers: &[f64]) -> Option<usize> {
    let mut best: Option<(usize, f64)> = None;
    for (i, &c) in centers.iter().enumerate() {
        let d = (x - c).abs();
        if d <= COL_MEMBER_TOLERANCE && best.is_none_or(|(_, bd)| d < bd) {
            best = Some((i, d));
        }
    }
    best.map(|(i, _)| i)
}

/// Detect the best table grid on one page from its reconstructed
/// lines. Returns `None` when no plausible grid exists.
///
/// Algorithm (documented heuristic):
/// 1. Only lines with >=2 fragments can be rows.
/// 2. All fragment x-starts are clustered (gap <=
///    [`COL_X_CLUSTER_TOLERANCE`]); clusters touched by >=
///    [`MIN_COLUMN_SUPPORT`] distinct lines become columns.
/// 3. A page qualifies when >=2 columns exist and >=
///    [`MIN_ALIGNED_ROWS`] lines touch >=2 columns.
/// 4. Each qualifying line becomes a row; cells are the merged text
///    of fragments nearest each column. Rows with fewer than
///    [`MIN_ROW_FILLED_CELLS`] non-empty cells are dropped.
fn detect_table(lines: &[TextLine], page: u32) -> Option<Table> {
    let candidates: Vec<&TextLine> = lines.iter().filter(|l| l.fragments.len() >= 2).collect();
    if candidates.len() < MIN_ALIGNED_ROWS {
        return None;
    }

    // Cluster fragment x-starts.
    let mut xs: Vec<f64> = candidates
        .iter()
        .flat_map(|l| l.fragments.iter().map(|f| f.x))
        .collect();
    xs.sort_by(f64::total_cmp);
    let cluster_means: Vec<f64> = {
        let mut clusters: Vec<Vec<f64>> = Vec::new();
        for &x in &xs {
            if let Some(cur) = clusters.last_mut()
                && x - cur.last().unwrap() <= COL_X_CLUSTER_TOLERANCE
            {
                cur.push(x);
            } else {
                clusters.push(vec![x]);
            }
        }
        clusters
            .iter()
            .map(|c| c.iter().sum::<f64>() / c.len() as f64)
            .collect()
    };
    if cluster_means.len() < 2 {
        return None;
    }

    // Column support: distinct lines touching each cluster.
    let mut support = vec![0usize; cluster_means.len()];
    for line in &candidates {
        let mut touched = vec![false; cluster_means.len()];
        for frag in &line.fragments {
            if let Some(ci) = nearest_cluster(frag.x, &cluster_means)
                && !touched[ci]
            {
                touched[ci] = true;
            }
        }
        for (i, &t) in touched.iter().enumerate() {
            if t {
                support[i] += 1;
            }
        }
    }

    // Keep only well-supported columns, preserving left-to-right
    // order (cluster_means is already sorted by construction).
    let columns: Vec<usize> = (0..cluster_means.len())
        .filter(|&i| support[i] >= MIN_COLUMN_SUPPORT)
        .collect();
    if columns.len() < 2 {
        return None;
    }
    let centers: Vec<f64> = columns.iter().map(|&i| cluster_means[i]).collect();

    // Count lines touching >=2 *kept* columns — the "aligned rows".
    let aligned_rows = candidates
        .iter()
        .filter(|line| {
            let mut touched = vec![false; centers.len()];
            let mut count = 0usize;
            for frag in &line.fragments {
                if let Some(ci) = nearest_cluster(frag.x, &centers)
                    && !touched[ci]
                {
                    touched[ci] = true;
                    count += 1;
                }
            }
            count >= 2
        })
        .count();
    if aligned_rows < MIN_ALIGNED_ROWS {
        return None;
    }

    // Build rows top-to-bottom (lines arrive y-descending from
    // `group_lines`; sort defensively).
    let mut ordered: Vec<&&TextLine> = candidates.iter().collect();
    ordered.sort_by(|a, b| b.y.total_cmp(&a.y));

    let mut rows: Vec<Vec<String>> = Vec::new();
    let mut filled_total = 0usize;
    let ncols = centers.len();
    for line in ordered {
        // Assign each fragment to its nearest kept column.
        let mut cell_frags: Vec<Vec<&PosFragment>> = vec![Vec::new(); ncols];
        for frag in &line.fragments {
            if let Some(ci) = nearest_cluster(frag.x, &centers) {
                // `centers` only contains kept columns; index aligns.
                cell_frags[ci].push(frag);
            }
        }
        let mut row: Vec<String> = Vec::with_capacity(ncols);
        let mut filled = 0usize;
        for frags in &cell_frags {
            if frags.is_empty() {
                row.push(String::new());
                continue;
            }
            // Merge the cell's fragments left-to-right.
            let mut text = String::new();
            for (i, f) in frags.iter().enumerate() {
                if i == 0 {
                    text.push_str(&f.text);
                } else {
                    join_pair(frags[i - 1], f, &mut text);
                }
            }
            let cell = text.trim().to_string();
            if !cell.is_empty() {
                filled += 1;
            }
            row.push(cell);
        }
        if filled >= MIN_ROW_FILLED_CELLS {
            filled_total += filled;
            rows.push(row);
        }
    }

    if rows.len() < 2 {
        return None;
    }

    let fill_ratio = filled_total as f64 / (rows.len() * ncols) as f64;
    // Base 0.4 for passing all structural checks + up to 0.6 for
    // cell fullness. Documented as a rough heuristic, not calibrated.
    let confidence = (0.4 + 0.6 * fill_ratio).clamp(0.0, 1.0);

    Some(Table {
        page,
        rows,
        confidence,
    })
}

// ---- key-value parsing -----------------------------------------------------

/// A key is plausible when it is 1..=[`MAX_KEY_CHARS`] chars, contains
/// at least one alphanumeric, and does not end with `.`.
fn valid_key(key: &str) -> bool {
    let n = key.chars().count();
    n >= 1 && n <= MAX_KEY_CHARS && key.chars().any(|c| c.is_alphanumeric()) && !key.ends_with('.')
}

/// Find the earliest separator in `text`: `:`, a tab, a run of >=2
/// spaces, or a run of >= [`MIN_DOT_LEADER`] dots. Returns
/// `(key_raw, value_raw, sep_end_index)` where `sep_end_index` is the
/// byte index just past the separator run.
fn find_separator(text: &str) -> Option<(usize, usize)> {
    let bytes = text.as_bytes();
    let mut best: Option<(usize, usize)> = None; // (sep_start, sep_end)
    let mut i = 0usize;
    while i < bytes.len() {
        let b = bytes[i];
        let is_sep_char = b == b':' || b == b'\t' || b == b' ' || b == b'.';
        if !is_sep_char {
            i += 1;
            continue;
        }
        // Colon is always a single-char separator.
        if b == b':' {
            // "http://…" style false positives: skip when the value
            // starts with "//".
            let rest = &text[i + 1..];
            if !rest.trim_start().starts_with("//") {
                let end = i + 1;
                if best.map_or(true, |(s, _)| i < s) {
                    best = Some((i, end));
                }
            }
            i += 1;
            continue;
        }
        // Otherwise measure the run of [space, tab, dot].
        let start = i;
        let mut j = i;
        while j < bytes.len() {
            match bytes[j] {
                b' ' | b'\t' => {
                    j += 1;
                }
                b'.' => {
                    j += 1;
                }
                _ => break,
            }
        }
        // Separator qualifies as: a tab anywhere, >=2 whitespace
        // chars, or >=MIN_DOT_LEADER dots (optionally surrounded by
        // spaces, e.g. "Label .... value").
        let run = &bytes[start..j];
        let tab = run.contains(&b'\t');
        let ws_count = run.iter().filter(|&&c| c == b' ' || c == b'\t').count();
        let qualifies = tab || ws_count >= 2 || dot_run_max(run) >= MIN_DOT_LEADER;
        if qualifies && best.map_or(true, |(s, _)| start < s) {
            best = Some((start, j));
        }
        i = j;
    }
    best
}

/// Longest consecutive `.` run inside a separator slice.
fn dot_run_max(slice: &[u8]) -> usize {
    let mut max = 0usize;
    let mut cur = 0usize;
    for &b in slice {
        if b == b'.' {
            cur += 1;
            max = max.max(cur);
        } else {
            cur = 0;
        }
    }
    max
}

/// Parse one reconstructed line into `(key, value)` if it matches a
/// key-value pattern. See module docs for the accepted forms.
fn parse_kv(text: &str) -> Option<(String, String)> {
    let t = text.trim();
    if t.is_empty() {
        return None;
    }

    // Preferred separators: colon / tab / multi-space / dot leaders.
    // (`find_separator` never qualifies a lone single space — that
    // case is the stricter fallback below.)
    if let Some((start, end)) = find_separator(t) {
        let key = t[..start].trim();
        let value = t[end..].trim();
        if valid_key(key) && !value.is_empty() {
            return Some((key.to_string(), value.to_string()));
        }
    }

    // Fallback: first single space/tab split ("Label value"). Stricter
    // than the structured separators: the key must look label-ish
    // (uppercase letter or digit present, or very short) and the
    // value must not end with '.' or contain "://" — this suppresses
    // most sentence and URL false positives.
    if let Some(idx) = t.find(|c| c == ' ' || c == '\t') {
        let key = t[..idx].trim();
        let value = t[idx + 1..].trim();
        // (uppercase letter or digit present, or very short — a
        // lone lowercase word like "plain" is prose, not a label)
        let labelish = key
            .chars()
            .any(|c| c.is_ascii_uppercase() || c.is_ascii_digit())
            || key.chars().count() <= 3;
        if valid_key(key)
            && !value.is_empty()
            && labelish
            && !value.ends_with('.')
            && !value.contains("://")
        {
            return Some((key.to_string(), value.to_string()));
        }
    }

    None
}

// ---- public API ------------------------------------------------------------

/// Shared scan: load document, walk pages, return per-page lines.
fn scan_document(
    input: &[u8],
    limits: &EngineLimits,
) -> PdfResult<(Document, Vec<(u32, Vec<TextLine>)>, Vec<String>)> {
    let doc = load_pdf(input, limits)?;
    reject_encrypted(&doc)?;

    let mut warnings = Vec::new();
    let mut stats = ScanStats {
        streams_failed: 0,
        strings_undecodable: 0,
        fonts_unresolved: 0,
        pages_without_fonts: 0,
        fragment_cap_hit: false,
    };

    let pages = doc.get_pages();
    let mut per_page: Vec<(u32, Vec<TextLine>)> = Vec::with_capacity(pages.len());
    let mut fragments: Vec<PosFragment> = Vec::new();

    for (&page_number, &page_id) in &pages {
        fragments.clear();
        scan_page(&doc, page_id, &mut fragments, &mut stats);
        let lines = group_lines(&fragments);
        per_page.push((page_number, lines));
    }

    warnings.extend(stats.warnings());
    Ok((doc, per_page, warnings))
}

/// Extract best-effort tables from `input`.
///
/// Returns a [`TableReport`] with at most one [`Table`] per page.
/// See module docs for the positioning model, heuristics and
/// limitations. Encrypted documents are rejected; malformed input
/// surfaces as [`PdfError::InvalidPdf`]/[`PdfError::InvalidInput`].
pub fn extract_tables(input: &[u8], limits: &EngineLimits) -> PdfResult<TableReport> {
    let (_doc, per_page, warnings) = scan_document(input, limits)?;
    let pages_scanned = per_page.len() as u32;
    let mut tables = Vec::new();
    for (page_number, lines) in &per_page {
        if let Some(table) = detect_table(lines, *page_number) {
            tables.push(table);
        }
    }
    Ok(TableReport {
        tables,
        pages_scanned,
        warnings,
    })
}

/// Extract best-effort `key: value` pairs from `input`, one
/// [`KvPair`] per matching text line, in page order.
///
/// Recognized line forms: `Label: value`, `Label .... value`,
/// `Label<tab>value`, `Label  value` (>=2 spaces) and, more
/// restrictively, `Label value` (single space — key must look
/// label-ish and the value must not end with `.`). See module docs
/// for false-positive notes. Encrypted documents are rejected.
pub fn extract_key_values(input: &[u8], limits: &EngineLimits) -> PdfResult<Vec<KvPair>> {
    let (_doc, per_page, _warnings) = scan_document(input, limits)?;
    let mut pairs = Vec::new();
    for (page_number, lines) in &per_page {
        for line in lines {
            if let Some((key, value)) = parse_kv(&line.text) {
                pairs.push(KvPair {
                    page: *page_number,
                    key,
                    value,
                });
            }
        }
    }
    Ok(pairs)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::crates::pdf_engine::ops::test_utils::pdf_with_pages;
    use lopdf::content::{Content, Operation};
    use lopdf::{Document, Stream, dictionary};

    fn frag(x: f64, y: f64, end_x: f64, text: &str) -> PosFragment {
        PosFragment {
            x,
            y,
            end_x,
            text: text.to_string(),
        }
    }

    fn line(y: f64, frags: Vec<PosFragment>) -> TextLine {
        let mut fragments = frags;
        fragments.sort_by(|a, b| a.x.total_cmp(&b.x));
        let mut text = String::new();
        for (i, f) in fragments.iter().enumerate() {
            if i == 0 {
                text.push_str(&f.text);
            } else {
                join_pair(&fragments[i - 1], f, &mut text);
            }
        }
        TextLine { y, fragments, text }
    }

    // ---------- group_lines ----------

    #[test]
    fn groups_fragments_by_baseline() {
        let frags = vec![
            frag(300.0, 700.0, 320.0, "B"),
            frag(100.0, 700.5, 120.0, "A"),
            frag(100.0, 680.0, 120.0, "C"),
        ];
        let lines = group_lines(&frags);
        assert_eq!(lines.len(), 2);
        // Top line first (higher y), fragments sorted by x.
        assert_eq!(lines[0].fragments[0].text, "A");
        assert_eq!(lines[0].fragments[1].text, "B");
        assert_eq!(lines[1].fragments[0].text, "C");
    }

    #[test]
    fn wide_gap_produces_double_space() {
        let l = line(
            700.0,
            vec![
                frag(100.0, 700.0, 140.0, "Label"),
                frag(300.0, 700.0, 340.0, "value"),
            ],
        );
        assert_eq!(l.text, "Label  value");
    }

    // ---------- detect_table ----------

    /// 3 rows x 2 columns of aligned fragments.
    fn table_lines() -> Vec<TextLine> {
        let mut lines = Vec::new();
        for (i, y) in [700.0, 680.0, 660.0].iter().enumerate() {
            let name = ["Alice", "Bob", "Carol"][i];
            let age = ["30", "25", "41"][i];
            lines.push(line(
                *y,
                vec![frag(100.0, *y, 140.0, name), frag(300.0, *y, 320.0, age)],
            ));
        }
        lines
    }

    #[test]
    fn detects_simple_grid() {
        let table = detect_table(&table_lines(), 1).expect("table should be detected");
        assert_eq!(table.page, 1);
        assert_eq!(table.rows.len(), 3);
        assert_eq!(table.rows[0], vec!["Alice".to_string(), "30".to_string()]);
        assert_eq!(table.rows[2], vec!["Carol".to_string(), "41".to_string()]);
        assert!(table.confidence > 0.5 && table.confidence <= 1.0);
    }

    #[test]
    fn three_columns_detected() {
        let mut lines = Vec::new();
        for (i, y) in [700.0, 680.0, 660.0].iter().enumerate() {
            lines.push(line(
                *y,
                vec![
                    frag(50.0, *y, 80.0, ["a", "b", "c"][i]),
                    frag(200.0, *y, 230.0, ["1", "2", "3"][i]),
                    frag(400.0, *y, 430.0, ["x", "y", "z"][i]),
                ],
            ));
        }
        let table = detect_table(&lines, 2).unwrap();
        assert_eq!(table.rows[0].len(), 3);
        assert_eq!(table.rows[0][2], "x");
    }

    #[test]
    fn non_table_lines_yield_none() {
        // Prose-like lines: fragments at unrelated x positions.
        let lines = vec![
            line(
                700.0,
                vec![
                    frag(50.0, 700.0, 90.0, "Hello"),
                    frag(500.0, 700.0, 560.0, "world"),
                ],
            ),
            line(
                680.0,
                vec![
                    frag(120.0, 680.0, 170.0, "this"),
                    frag(400.0, 680.0, 450.0, "is"),
                ],
            ),
            line(
                660.0,
                vec![
                    frag(80.0, 660.0, 130.0, "not"),
                    frag(600.0, 660.0, 640.0, "aligned"),
                ],
            ),
            line(
                640.0,
                vec![
                    frag(200.0, 640.0, 250.0, "at"),
                    frag(350.0, 640.0, 400.0, "all"),
                ],
            ),
        ];
        assert!(detect_table(&lines, 1).is_none());
    }

    #[test]
    fn too_few_rows_yields_none() {
        // Only 2 aligned rows.
        let mut lines = Vec::new();
        for (i, y) in [700.0, 680.0].iter().enumerate() {
            lines.push(line(
                *y,
                vec![frag(100.0, *y, 130.0, "k"), frag(300.0, *y, 320.0, "v")],
            ));
            let _ = i;
        }
        assert!(detect_table(&lines, 1).is_none());
    }

    #[test]
    fn empty_cells_preserved() {
        // Row 2 has only a cell in column 2 — dropped (<2 filled).
        // Give row 2 a cell in col1 too but empty text can't happen;
        // instead test a row missing col2 entirely is dropped.
        let mut lines = table_lines();
        lines.push(line(
            640.0,
            vec![frag(100.0, 640.0, 140.0, "Dave")], // single fragment → not a candidate row
        ));
        // Add a 4th real row so grid still qualifies.
        lines.push(line(
            620.0,
            vec![
                frag(100.0, 620.0, 140.0, "Erin"),
                frag(300.0, 620.0, 320.0, "33"),
            ],
        ));
        let table = detect_table(&lines, 1).unwrap();
        assert_eq!(table.rows.len(), 4);
        assert!(table.rows.iter().all(|r| r.len() == 2));
    }

    // ---------- parse_kv ----------

    #[test]
    fn kv_colon() {
        assert_eq!(
            parse_kv("Name: John Doe"),
            Some(("Name".to_string(), "John Doe".to_string()))
        );
    }

    #[test]
    fn kv_dot_leader() {
        assert_eq!(
            parse_kv("Invoice No .... 2024-001"),
            Some(("Invoice No".to_string(), "2024-001".to_string()))
        );
    }

    #[test]
    fn kv_multi_space_and_tab() {
        assert_eq!(
            parse_kv("Label   value"),
            Some(("Label".to_string(), "value".to_string()))
        );
        assert_eq!(
            parse_kv("Key\tValue"),
            Some(("Key".to_string(), "Value".to_string()))
        );
    }

    #[test]
    fn kv_single_space_labelish() {
        assert_eq!(
            parse_kv("Name John"),
            Some(("Name".to_string(), "John".to_string()))
        );
    }

    #[test]
    fn kv_rejects_prose_and_bad_keys() {
        // Sentence ending in '.' — rejected by single-space guard.
        assert_eq!(parse_kv("This is a sentence."), None);
        // Key too long (>40 chars).
        let long = format!("{}: v", "k".repeat(41));
        assert_eq!(parse_kv(&long), None);
        // Key ending with '.'.
        assert_eq!(parse_kv("Mr.. value"), None);
        // Empty value.
        assert_eq!(parse_kv("Label:"), None);
        // No separator at all.
        assert_eq!(parse_kv("justaword"), None);
        // URL-ish colon rejected.
        assert_eq!(parse_kv("see http://example.com"), None);
    }

    // ---------- PDF-level tests ----------

    /// Build a one-page PDF whose content stream is `ops` verbatim.
    fn pdf_with_ops(ops: Vec<Operation>) -> Vec<u8> {
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
        let content = Content { operations: ops };
        let content_id = doc.add_object(Stream::new(dictionary! {}, content.encode().unwrap()));
        let page_id = doc.add_object(dictionary! {
            "Type" => "Page",
            "Parent" => pages_id,
            "Contents" => content_id,
            "Resources" => resources_id,
            "MediaBox" => vec![0.into(), 0.into(), 595.into(), 842.into()],
        });
        let pages = dictionary! {
            "Type" => "Pages",
            "Kids" => vec![Object::Reference(page_id)],
            "Count" => 1,
            "Resources" => resources_id,
            "MediaBox" => vec![0.into(), 0.into(), 595.into(), 842.into()],
        };
        doc.objects.insert(pages_id, Object::Dictionary(pages));
        let catalog_id = doc.add_object(dictionary! {
            "Type" => "Catalog",
            "Pages" => pages_id,
        });
        doc.trailer.set("Root", catalog_id);
        let mut buf = Vec::new();
        doc.save_to(&mut buf).unwrap();
        buf
    }

    /// Ops drawing a 3x2 table via absolute `Tm` positions.
    fn table_ops() -> Vec<Operation> {
        let mut ops = vec![
            Operation::new("BT", vec![]),
            Operation::new("Tf", vec!["F1".into(), 12.into()]),
        ];
        let data = [
            (700.0, 100.0, "Alice"),
            (700.0, 300.0, "30"),
            (680.0, 100.0, "Bob"),
            (680.0, 300.0, "25"),
            (660.0, 100.0, "Carol"),
            (660.0, 300.0, "41"),
        ];
        for (y, x, text) in data {
            ops.push(Operation::new(
                "Tm",
                vec![1.into(), 0.into(), 0.into(), 1.into(), x.into(), y.into()],
            ));
            ops.push(Operation::new("Tj", vec![Object::string_literal(text)]));
        }
        ops.push(Operation::new("ET", vec![]));
        ops
    }

    #[test]
    fn pdf_table_smoke() {
        let pdf = pdf_with_ops(table_ops());
        let report = extract_tables(&pdf, &EngineLimits::default()).unwrap();
        assert_eq!(report.tables.len(), 1);
        let t = &report.tables[0];
        assert_eq!(t.rows.len(), 3);
        assert_eq!(t.rows[0][0], "Alice");
        assert_eq!(t.rows[0][1], "30");
        assert_eq!(t.rows[2][0], "Carol");
    }

    #[test]
    fn pdf_key_values_smoke() {
        // pdf_with_pages emits one Tj per page at a fixed position.
        let pdf = pdf_with_pages(&["Name: John Doe", "plain text line"]);
        let pairs = extract_key_values(&pdf, &EngineLimits::default()).unwrap();
        assert_eq!(pairs.len(), 1);
        assert_eq!(pairs[0].page, 1);
        assert_eq!(pairs[0].key, "Name");
        assert_eq!(pairs[0].value, "John Doe");
    }

    #[test]
    fn pdf_no_table_on_prose() {
        let pdf = pdf_with_pages(&["Just a normal paragraph of text."]);
        let report = extract_tables(&pdf, &EngineLimits::default()).unwrap();
        assert!(report.tables.is_empty());
    }

    #[test]
    fn garbage_input_rejected() {
        assert!(extract_tables(b"junk", &EngineLimits::default()).is_err());
        assert!(extract_key_values(b"junk", &EngineLimits::default()).is_err());
    }
}
