//! Text markup: highlight / underline / strike through drawn directly
//! into page content streams.
//!
//! # What this op actually does
//!
//! For every selected page, each `/Contents` stream is decoded into
//! [`lopdf::content::Content`] operations and walked with a small
//! **text-state machine** that tracks the PDF text matrices (`Tm` /
//! `Tlm`), the current font size (`Tf`), character/word spacing
//! (`Tc`/`Tw`), horizontal scaling (`Tz`), leading (`TL`), text rise
//! (`Ts`), render mode (`Tr`) and the graphics-state matrix (`cm`,
//! `q`/`Q`). Every text-showing operator (`Tj`, `TJ`, `'`, `"`) gets a
//! markup quad — a filled path — appended to the **end of the same
//! content stream**, positioned where the text was actually drawn.
//!
//! Coordinates are computed by transforming the quad's corners through
//! the tracked text matrix and `cm` stack, then through the *inverse*
//! of the content stream's final `cm` state, so the emitted path lands
//! on the text even when the stream's coordinate system was transformed
//! mid-stream. Because the markup is real vector art (not an
//! annotation), it survives printing, rasterization and viewers that
//! ignore markup annotations — but it is also **flattened into the
//! page**: there is no editable highlight object left afterwards.
//!
//! # Geometry — honest approximations
//!
//! - Text *position* is exact where the content stream uses ordinary
//!   `Td`/`TD`/`Tm`/`T*`/`'`/`"` positioning and axis-aligned or
//!   rotated text matrices.
//! - Text *width* is **estimated**, not measured: each byte of a shown
//!   string counts ~0.5 em (ASCII) or ~1.0 em (non-ASCII) of the
//!   current font size — the same heuristic `watermark` uses. Real
//!   glyph advances (proportional fonts, kerning pairs, font
//!   subsetting) are not consulted, so quads may be short or long by
//!   roughly a character width. `TJ` kerning numbers are honored as
//!   `-n/1000 em` adjustments, and `Tc`/`Tw`/`Tz` are applied, but all
//!   of this remains an approximation.
//! - Band positions are font-metric guesses: highlight covers
//!   `-0.30 em .. +0.82 em` around the baseline, underline sits at
//!   `-0.10 em` below it, strike-out at `+0.30 em`, each with a
//!   ~`0.06 em` (min 0.5 pt) stroke converted to a filled rectangle.
//!   Fonts with unusual metrics will look slightly off.
//! - Each `TJ` array produces **one** quad spanning the whole run;
//!   each `Tj`/`'`/`"` produces one quad. Empty or zero-width shows
//!   produce nothing.
//!
//! # Limitations — read before relying on this
//!
//! - Text inside **Form XObjects** (`Do`), Type3 glyph descriptions,
//!   tiling patterns, annotation appearance streams and optional
//!   content layers is **not** marked — only the page's own content
//!   streams are walked.
//! - Text shown with render mode `Tr 3` (invisible) is not marked.
//! - A content stream that fails to decompress or decode is skipped
//!   entirely (its text is left unmarked); there is no per-page report
//!   — inspect the output visually if completeness matters.
//! - Rewritten streams are stored **uncompressed** (`/Filter` and
//!   `/DecodeParms` dropped), matching `redact`/`watermark`.
//! - Markup is appended at the end of each content stream, so it draws
//!   **on top of** existing content. Highlight uses a translucent
//!   `/ExtGState` (alpha 0.4) so the text remains readable; underline
//!   and strike-out are opaque.
//! - `/UserUnit` and `/Rotate` need no special handling: markup is
//!   emitted in the same user space as the text it covers, so page
//!   rotation applies to both identically.

use std::collections::HashSet;

use lopdf::content::{Content, Operation};
use lopdf::{Object, ObjectId};
use serde::{Deserialize, Serialize};

use crate::crates::core::engine::EngineLimits;

use crate::crates::pdf_engine::error::PdfResult;

use super::load::{load_pdf, lopdf_err, reject_encrypted, save_pdf};
use super::watermark::{ensure_ext_gstate, selected_pages};

/// Decompression bound per content stream — same 64 MiB guard
/// `watermark` uses when merging page contents.
const MAX_CONTENT_STREAM: usize = 64 * 1024 * 1024;

/// Which kind of markup to draw.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum MarkupKind {
    /// Translucent yellow rectangle behind/over the text line.
    Highlight,
    /// Thin filled bar just below the text baseline.
    Underline,
    /// Thin filled bar through the lower third of the text.
    Strike,
}

impl MarkupKind {
    /// Default RGB color (0-255) when the caller passes `None`.
    fn default_color(self) -> [u8; 3] {
        match self {
            // Material-style yellow, visible on white paper.
            MarkupKind::Highlight => [255, 235, 59],
            // Conventional underline/strike colors.
            MarkupKind::Underline => [0, 0, 0],
            MarkupKind::Strike => [220, 38, 38],
        }
    }
}

// ---------------------------------------------------------------------------
// Affine matrices (PDF [a b c d e f] convention)
// ---------------------------------------------------------------------------

/// A 2-D affine matrix `[a b c d e f]` mapping `(x, y)` to
/// `(a*x + c*y + e, b*x + d*y + f)`.
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
    const IDENTITY: Mat = Mat {
        a: 1.0,
        b: 0.0,
        c: 0.0,
        d: 1.0,
        e: 0.0,
        f: 0.0,
    };

    fn translate(x: f64, y: f64) -> Mat {
        Mat {
            a: 1.0,
            b: 0.0,
            c: 0.0,
            d: 1.0,
            e: x,
            f: y,
        }
    }

    /// `self ∘ inner`: apply `inner` first, then `self`.
    fn mul(self, inner: Mat) -> Mat {
        Mat {
            a: self.a * inner.a + self.c * inner.b,
            b: self.b * inner.a + self.d * inner.b,
            c: self.a * inner.c + self.c * inner.d,
            d: self.b * inner.c + self.d * inner.d,
            e: self.a * inner.e + self.c * inner.f + self.e,
            f: self.b * inner.e + self.d * inner.f + self.f,
        }
    }

    fn apply(&self, x: f64, y: f64) -> (f64, f64) {
        (
            self.a * x + self.c * y + self.e,
            self.b * x + self.d * y + self.f,
        )
    }

    /// Inverse matrix, or `None` for (near-)singular transforms.
    fn inverse(&self) -> Option<Mat> {
        let det = self.a * self.d - self.b * self.c;
        if !det.is_finite() || det.abs() < 1e-12 {
            return None;
        }
        let (a, b, c, d) = (self.d / det, -self.b / det, -self.c / det, self.a / det);
        Some(Mat {
            a,
            b,
            c,
            d,
            e: (c * self.f - d * self.e),
            f: (b * self.e - a * self.f),
        })
    }
}

fn mat_from_operands(operands: &[Object]) -> Option<Mat> {
    if operands.len() < 6 {
        return None;
    }
    let mut v = [0f64; 6];
    for (slot, obj) in v.iter_mut().zip(operands.iter()) {
        *slot = obj.as_float().ok()? as f64;
        if !slot.is_finite() {
            return None;
        }
    }
    Some(Mat {
        a: v[0],
        b: v[1],
        c: v[2],
        d: v[3],
        e: v[4],
        f: v[5],
    })
}

fn num_operand(op: &Operation, index: usize) -> Option<f64> {
    op.operands
        .get(index)
        .and_then(|o| o.as_float().ok())
        .map(|v| v as f64)
        .filter(|v| v.is_finite())
}

// ---------------------------------------------------------------------------
// Text state machine
// ---------------------------------------------------------------------------

/// Tracked text/graphics state while walking one content stream.
///
/// `tm`/`tlm` are text-space → user-space matrices in the PDF sense:
/// `Td`-family offsets are applied in *text space* (so they follow a
/// rotated text matrix), `Tm` operands set the matrix absolutely, and
/// showing text advances `tm` along its local x-axis. Only the
/// matrices' translations and linear parts as produced by `Tm`/`Td`
/// are honored — there is no separate text-rendering-matrix font
/// scaling beyond what the operands carry.
struct TextMachine {
    tm: Mat,
    tlm: Mat,
    /// Current font size from `Tf` (unscaled text units; 0 until set).
    fs: f64,
    /// Char spacing `Tc`, word spacing `Tw` (text units).
    tc: f64,
    tw: f64,
    /// Horizontal scaling `Tz` in percent (default 100).
    tz: f64,
    /// Leading `TL` (text units).
    tl: f64,
    /// Text rise `Ts` (text units).
    ts: f64,
    /// Render mode `Tr` (3 = invisible).
    tr: i64,
}

impl TextMachine {
    fn new() -> Self {
        Self {
            tm: Mat::IDENTITY,
            tlm: Mat::IDENTITY,
            fs: 0.0,
            tc: 0.0,
            tw: 0.0,
            tz: 100.0,
            tl: 0.0,
            ts: 0.0,
            tr: 0,
        }
    }

    /// `Td`/`TD`/`T*`: move the line matrix by `(tx, ty)` measured in
    /// text space, then copy it to the text matrix.
    fn move_line(&mut self, tx: f64, ty: f64) {
        self.tlm = self.tlm.mul(Mat::translate(tx, ty));
        self.tm = self.tlm;
    }

    /// Advance the text matrix by `w` text units along its x-axis.
    fn advance(&mut self, w: f64) {
        self.tm = self.tm.mul(Mat::translate(w, 0.0));
    }

    /// Estimated advance width (text units) for showing `bytes`,
    /// including `Tc`/`Tw` and an optional precomputed `kern`
    /// adjustment (used for `TJ` numbers, already in text units).
    fn show_width(&self, bytes: &[u8], kern: f64) -> f64 {
        if !self.fs.is_finite() || self.fs <= 0.0 {
            return 0.0;
        }
        let mut units = 0f64;
        let mut spaces = 0f64;
        for &b in bytes {
            units += if b.is_ascii() { 0.5 } else { 1.0 };
            if b == b' ' {
                spaces += 1.0;
            }
        }
        let n = bytes.len() as f64;
        let raw = units * self.fs + self.tc * n + self.tw * spaces + kern;
        let w = raw * (self.tz / 100.0);
        if w.is_finite() && w > 0.0 { w } else { 0.0 }
    }
}

/// Text-space y-bands (fractions of font size, plus `Ts` rise) for each
/// markup kind, returned as `(y_bottom, y_top)`.
fn kind_band(kind: MarkupKind, fs: f64, ts: f64) -> (f64, f64) {
    match kind {
        MarkupKind::Highlight => (ts - 0.30 * fs, ts + 0.82 * fs),
        MarkupKind::Underline => {
            let t = (0.06 * fs).max(0.5);
            let top = ts - 0.10 * fs;
            (top - t, top)
        }
        MarkupKind::Strike => {
            let t = (0.06 * fs).max(0.5);
            let mid = ts + 0.30 * fs;
            (mid - t * 0.5, mid + t * 0.5)
        }
    }
}

/// Horizontal padding (text units) so quads don't clip glyph edges.
fn kind_pad(fs: f64) -> f64 {
    (0.04 * fs).max(0.5)
}

/// Build the four text-space corners of the markup quad for a show of
/// width `w` starting at the current text origin, transform them
/// through `tm` then `ctm`, and push the resulting quad (in
/// ctm-relative "device" space) onto `quads`.
fn push_quad(
    machine: &TextMachine,
    ctm: &Mat,
    kind: MarkupKind,
    w: f64,
    quads: &mut Vec<[[f64; 2]; 4]>,
) {
    // Invisible text gets no visible markup (it still advances `tm`).
    if machine.tr == 3 {
        return;
    }
    let fs = machine.fs;
    if !(fs.is_finite() && fs > 0.0) || !(w.is_finite() && w > 0.0) {
        return;
    }
    let pad = kind_pad(fs);
    let (y0, y1) = kind_band(kind, fs, machine.ts);
    let x0 = -pad;
    let x1 = w + pad;
    let corners = [(x0, y0), (x1, y0), (x1, y1), (x0, y1)];
    let mut quad = [[0f64; 2]; 4];
    for (i, &(x, y)) in corners.iter().enumerate() {
        let (ux, uy) = machine.tm.apply(x, y);
        let (dx, dy) = ctm.apply(ux, uy);
        if !dx.is_finite() || !dy.is_finite() {
            return;
        }
        quad[i] = [dx, dy];
    }
    quads.push(quad);
}

/// Estimated advance width (text units) for a `TJ` array operand:
/// string bytes contribute glyph advances, numbers contribute kern
/// adjustments of `-n/1000 em` each.
fn tj_measure(machine: &TextMachine, array: &[Object]) -> f64 {
    let mut units = 0f64;
    let mut chars = 0f64;
    let mut spaces = 0f64;
    let mut kern = 0f64;
    for item in array {
        match item {
            Object::String(bytes, _) => {
                for &b in bytes.iter() {
                    units += if b.is_ascii() { 0.5 } else { 1.0 };
                    if b == b' ' {
                        spaces += 1.0;
                    }
                }
                chars += bytes.len() as f64;
            }
            Object::Integer(n) => kern -= *n as f64 * machine.fs / 1000.0,
            Object::Real(n) => kern -= *n as f64 * machine.fs / 1000.0,
            _ => {}
        }
    }
    if !machine.fs.is_finite() || machine.fs <= 0.0 {
        return 0.0;
    }
    let raw = units * machine.fs + machine.tc * chars + machine.tw * spaces + kern;
    let w = raw * (machine.tz / 100.0);
    if w.is_finite() && w > 0.0 { w } else { 0.0 }
}

/// Handle one text-showing operator: measure, emit a quad, advance.
fn handle_show(
    machine: &mut TextMachine,
    ctm: &Mat,
    kind: MarkupKind,
    operands: &[Object],
    quads: &mut Vec<[[f64; 2]; 4]>,
) {
    match operands.first() {
        Some(Object::Array(items)) => {
            // TJ: single quad spanning the whole array run.
            let w = tj_measure(machine, items);
            push_quad(machine, ctm, kind, w, quads);
            machine.advance(w);
        }
        Some(Object::String(bytes, _)) => {
            let w = machine.show_width(bytes, 0.0);
            push_quad(machine, ctm, kind, w, quads);
            machine.advance(w);
        }
        _ => {}
    }
}

/// Find the string operand of a `'` or `"` show (always the last
/// operand) and handle it like `Tj` after the implicit line move.
fn handle_quote_show(
    machine: &mut TextMachine,
    ctm: &Mat,
    kind: MarkupKind,
    operands: &[Object],
    quads: &mut Vec<[[f64; 2]; 4]>,
) {
    if let Some(Object::String(bytes, _)) = operands.last() {
        let w = machine.show_width(bytes, 0.0);
        push_quad(machine, ctm, kind, w, quads);
        machine.advance(w);
    }
}

// ---------------------------------------------------------------------------
// Stream walking / rewriting
// ---------------------------------------------------------------------------

/// Walk one decoded content stream, collecting markup quads in
/// ctm-relative space. Returns the quads plus the stream's final `cm`
/// state (needed to express them in end-of-stream user space).
fn collect_quads(operations: &[Operation], kind: MarkupKind) -> (Vec<[[f64; 2]; 4]>, Mat) {
    let mut machine = TextMachine::new();
    let mut ctm = Mat::IDENTITY;
    let mut gstack: Vec<Mat> = Vec::new();
    let mut quads = Vec::new();

    for op in operations {
        match op.operator.as_str() {
            "q" => gstack.push(ctm),
            "Q" => {
                if let Some(m) = gstack.pop() {
                    ctm = m;
                }
            }
            "cm" => {
                if let Some(m) = mat_from_operands(&op.operands) {
                    ctm = ctm.mul(m);
                }
            }
            "BT" => {
                machine.tm = Mat::IDENTITY;
                machine.tlm = Mat::IDENTITY;
            }
            "Tf" => {
                if let Some(size) = num_operand(op, 1) {
                    machine.fs = size;
                }
            }
            "Tc" => {
                if let Some(v) = num_operand(op, 0) {
                    machine.tc = v;
                }
            }
            "Tw" => {
                if let Some(v) = num_operand(op, 0) {
                    machine.tw = v;
                }
            }
            "Tz" => {
                if let Some(v) = num_operand(op, 0) {
                    machine.tz = v;
                }
            }
            "TL" => {
                if let Some(v) = num_operand(op, 0) {
                    machine.tl = v;
                }
            }
            "Ts" => {
                if let Some(v) = num_operand(op, 0) {
                    machine.ts = v;
                }
            }
            "Tr" => {
                if let Some(v) = num_operand(op, 0) {
                    machine.tr = v.round() as i64;
                }
            }
            "Td" => {
                if let (Some(tx), Some(ty)) = (num_operand(op, 0), num_operand(op, 1)) {
                    machine.move_line(tx, ty);
                }
            }
            "TD" => {
                if let (Some(tx), Some(ty)) = (num_operand(op, 0), num_operand(op, 1)) {
                    machine.tl = -ty;
                    machine.move_line(tx, ty);
                }
            }
            "Tm" => {
                if let Some(m) = mat_from_operands(&op.operands) {
                    machine.tm = m;
                    machine.tlm = m;
                }
            }
            "T*" => machine.move_line(0.0, -machine.tl),
            "Tj" | "TJ" => handle_show(&mut machine, &ctm, kind, &op.operands, &mut quads),
            "'" => {
                machine.move_line(0.0, -machine.tl);
                handle_quote_show(&mut machine, &ctm, kind, &op.operands, &mut quads);
            }
            "\"" => {
                // aw ac " — set word/char spacing, then behave like '.
                if let Some(aw) = num_operand(op, 0) {
                    machine.tw = aw;
                }
                if let Some(ac) = num_operand(op, 1) {
                    machine.tc = ac;
                }
                machine.move_line(0.0, -machine.tl);
                handle_quote_show(&mut machine, &ctm, kind, &op.operands, &mut quads);
            }
            _ => {}
        }
    }

    (quads, ctm)
}

/// Emit the path-painting operators for one quad (already in
/// end-of-stream user space) wrapped in `q`/`Q`.
fn quad_operations(
    quad: &[[f64; 2]; 4],
    rgb: (f64, f64, f64),
    gs_name: Option<&str>,
) -> Vec<Operation> {
    let mut ops = vec![Operation::new("q", vec![])];
    if let Some(name) = gs_name {
        ops.push(Operation::new(
            "gs",
            vec![Object::Name(name.as_bytes().to_vec())],
        ));
    }
    ops.push(Operation::new(
        "rg",
        vec![
            Object::Real(rgb.0 as f32),
            Object::Real(rgb.1 as f32),
            Object::Real(rgb.2 as f32),
        ],
    ));
    for (i, [x, y]) in quad.iter().enumerate() {
        let op = if i == 0 { "m" } else { "l" };
        ops.push(Operation::new(
            op,
            vec![Object::Real(*x as f32), Object::Real(*y as f32)],
        ));
    }
    ops.push(Operation::new("h", vec![]));
    ops.push(Operation::new("f", vec![]));
    ops.push(Operation::new("Q", vec![]));
    ops
}

/// Walk, measure and append markup ops to one content stream in place.
/// Streams that fail to decode are skipped silently (documented
/// limitation — there is no report channel in this op's signature).
fn markup_stream(
    doc: &mut lopdf::Document,
    stream_id: ObjectId,
    kind: MarkupKind,
    rgb: (f64, f64, f64),
    gs_name: Option<&str>,
) -> PdfResult<()> {
    let object = match doc.get_object_mut(stream_id) {
        Ok(o) => o,
        Err(_) => return Ok(()),
    };
    let stream = match object.as_stream_mut() {
        Ok(s) => s,
        Err(_) => return Ok(()),
    };

    let raw = match stream.decompressed_content_with_limit(MAX_CONTENT_STREAM) {
        Ok(data) => data,
        Err(_) => stream.content.clone(),
    };
    let Ok(content) = Content::decode(&raw) else {
        return Ok(());
    };

    let (quads, final_ctm) = collect_quads(&content.operations, kind);
    if quads.is_empty() {
        return Ok(());
    }

    // Express each quad (recorded under the CTM active at show time)
    // in the user space the appended ops will execute under — the
    // stream's final CTM.
    let inv = final_ctm.inverse().unwrap_or(Mat::IDENTITY);
    let mut all_ops = content.operations;
    for quad in &quads {
        let mut user_quad = *quad;
        for point in &mut user_quad {
            let (x, y) = inv.apply(point[0], point[1]);
            *point = [x, y];
        }
        all_ops.extend(quad_operations(&user_quad, rgb, gs_name));
    }

    let encoded = Content {
        operations: all_ops,
    }
    .encode()
    .map_err(lopdf_err)?;
    stream.content = encoded;
    // New bytes are uncompressed: drop filter entries so readers do
    // not try to decode them. /Length is rewritten on save.
    stream.dict.remove(b"Filter");
    stream.dict.remove(b"DecodeParms");
    Ok(())
}

/// Draw `kind` markup over the text of `input`, returning the new PDF.
///
/// `pages` is an optional 1-based range spec (`"1-3,8"`, `"all"`);
/// `None` marks every page. `color` is an optional RGB triple (0-255);
/// `None` uses the per-kind default documented on [`MarkupKind`].
/// Encrypted documents are rejected.
///
/// See the module docs for the geometry model and limitations — in
/// particular, width is estimated, not measured, and only the page's
/// own content streams are processed.
pub fn text_markup(
    input: &[u8],
    kind: MarkupKind,
    pages: Option<&str>,
    color: Option<[u8; 3]>,
    limits: &EngineLimits,
) -> PdfResult<Vec<u8>> {
    let mut doc = load_pdf(input, limits)?;
    reject_encrypted(&doc)?;

    let selected = selected_pages(&doc, pages)?;
    let pages_map = doc.get_pages();

    let rgb_u8 = color.unwrap_or_else(|| kind.default_color());
    let rgb = (
        f64::from(rgb_u8[0]) / 255.0,
        f64::from(rgb_u8[1]) / 255.0,
        f64::from(rgb_u8[2]) / 255.0,
    );

    // A stream shared by several pages must be marked exactly once —
    // the markup is baked into the stream itself.
    let mut done_streams: HashSet<ObjectId> = HashSet::new();

    for page_number in selected {
        let Some(&page_id) = pages_map.get(&page_number) else {
            continue;
        };
        // Highlight needs a real alpha channel; register one ExtGState
        // per page (only when the kind requires it).
        let gs_name = if kind == MarkupKind::Highlight {
            Some(ensure_ext_gstate(&mut doc, page_id, 0.4)?)
        } else {
            None
        };
        for stream_id in doc.get_page_contents(page_id) {
            if done_streams.insert(stream_id) {
                markup_stream(&mut doc, stream_id, kind, rgb, gs_name.as_deref())?;
            }
        }
    }

    doc.compress();
    save_pdf(&mut doc)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::crates::pdf_engine::ops::test_utils::{page_count, pdf_with_pages};
    use lopdf::content::Operation;
    use lopdf::{Document, Stream, dictionary};

    /// Build a one-page PDF whose content stream is `operations`.
    fn pdf_with_ops(operations: Vec<Operation>) -> Vec<u8> {
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
        let content = Content { operations };
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

    fn page_content(pdf: &[u8]) -> String {
        let doc = Document::load_mem(pdf).unwrap();
        let page_id = doc.get_pages()[&1];
        String::from_utf8_lossy(&doc.get_page_content(page_id)).into_owned()
    }

    #[test]
    fn highlight_adds_alpha_and_quads() {
        let input = pdf_with_pages(&["Hello world"]);
        let out = text_markup(
            &input,
            MarkupKind::Highlight,
            None,
            None,
            &EngineLimits::default(),
        )
        .unwrap();
        assert_eq!(page_count(&out), 1);
        let content = page_content(&out);
        // Original text still present, markup appended after it.
        let text_pos = content.find("(Hello world)").expect("text present");
        let mark_pos = content.find(" rg").expect("markup color op present");
        assert!(
            mark_pos > text_pos,
            "markup should be appended after content"
        );
        assert!(content.contains(" m\n"), "path move present: {content}");
        assert!(content.contains(" h\n"), "path close present");
        assert!(content.contains(" f\n"), "path fill present");
        assert!(content.contains(" gs"), "extgstate alpha applied");

        // The page must reference an ExtGState resource.
        let doc = Document::load_mem(&out).unwrap();
        let page_id = doc.get_pages()[&1];
        let resources = doc
            .get_dictionary(page_id)
            .unwrap()
            .get(b"Resources")
            .unwrap()
            .clone();
        let dict = match resources {
            Object::Dictionary(d) => d,
            Object::Reference(id) => doc.get_object(id).unwrap().as_dict().unwrap().clone(),
            _ => panic!("resources should be a dictionary"),
        };
        assert!(dict.get(b"ExtGState").is_ok(), "ExtGState registered");
    }

    #[test]
    fn underline_and_strike_use_color_no_alpha() {
        let input = pdf_with_pages(&["under me", "strike me"]);
        for (kind, color) in [
            (MarkupKind::Underline, Some([1u8, 2, 3])),
            (MarkupKind::Strike, None),
        ] {
            let out = text_markup(&input, kind, None, color, &EngineLimits::default()).unwrap();
            assert_eq!(page_count(&out), 1);
            let content = page_content(&out);
            assert!(content.contains(" rg"));
            assert!(content.contains(" f\n"));
            // No ExtGState needed for opaque marks.
            assert!(!content.contains(" gs"), "no alpha for {kind:?}");
            if color.is_some() {
                // Custom color still produces a valid rg op.
                assert!(content.contains(" rg"), "{kind:?}: {content}");
            }
        }
    }

    #[test]
    fn respects_page_range() {
        let input = pdf_with_pages(&["one", "two", "three"]);
        let out = text_markup(
            &input,
            MarkupKind::Underline,
            Some("2"),
            None,
            &EngineLimits::default(),
        )
        .unwrap();
        assert_eq!(page_count(&out), 3);
        let doc = Document::load_mem(&out).unwrap();
        let pages = doc.get_pages();
        let c1 = String::from_utf8_lossy(&doc.get_page_content(pages[&1])).into_owned();
        let c2 = String::from_utf8_lossy(&doc.get_page_content(pages[&2])).into_owned();
        let c3 = String::from_utf8_lossy(&doc.get_page_content(pages[&3])).into_owned();
        assert!(!c1.contains(" f\n"), "page 1 untouched");
        assert!(c2.contains(" f\n"), "page 2 marked");
        assert!(!c3.contains(" f\n"), "page 3 untouched");
    }

    #[test]
    fn handles_tj_tjarray_quotes_and_invisible_text() {
        let ops = vec![
            Operation::new("BT", vec![]),
            Operation::new("Tf", vec!["F1".into(), 12.into()]),
            Operation::new("Td", vec![100.into(), 700.into()]),
            Operation::new("Tj", vec![Object::string_literal("Hello")]),
            Operation::new(
                "TJ",
                vec![Object::Array(vec![
                    Object::string_literal("Big "),
                    Object::Integer(-20),
                    Object::string_literal("Text"),
                ])],
            ),
            Operation::new("'", vec![Object::string_literal("Next line")]),
            Operation::new("ET", vec![]),
            Operation::new("BT", vec![]),
            Operation::new("Tf", vec!["F1".into(), 12.into()]),
            Operation::new("Td", vec![100.into(), 600.into()]),
            Operation::new("Tr", vec![3.into()]),
            Operation::new("Tj", vec![Object::string_literal("invisible")]),
            Operation::new("ET", vec![]),
        ];
        let input = pdf_with_ops(ops);
        let out = text_markup(
            &input,
            MarkupKind::Highlight,
            None,
            None,
            &EngineLimits::default(),
        )
        .unwrap();
        let content = page_content(&out);
        // Three visible shows → three quads; invisible text → none.
        let fills = content.matches(" f\n").count();
        assert_eq!(fills, 3, "expected 3 markup quads: {content}");
    }

    #[test]
    fn honors_cm_transforms() {
        // Text drawn inside a 2x scaled, translated CTM.
        let ops = vec![
            Operation::new("q", vec![]),
            Operation::new(
                "cm",
                vec![
                    2.into(),
                    0.into(),
                    0.into(),
                    2.into(),
                    50.into(),
                    100.into(),
                ],
            ),
            Operation::new("BT", vec![]),
            Operation::new("Tf", vec!["F1".into(), 10.into()]),
            Operation::new("Td", vec![100.into(), 700.into()]),
            Operation::new("Tj", vec![Object::string_literal("scaled")]),
            Operation::new("ET", vec![]),
            Operation::new("Q", vec![]),
        ];
        let input = pdf_with_ops(ops);
        let out = text_markup(
            &input,
            MarkupKind::Underline,
            None,
            None,
            &EngineLimits::default(),
        )
        .unwrap();
        let content = page_content(&out);
        assert!(content.contains(" f\n"), "markup emitted: {content}");
        // Emitted coords live in end-of-stream (untransformed) space:
        // text x=100 under 2x+50 → device x=250; quad should start near
        // 250 minus a small pad. Check a coordinate in that ballpark.
        assert!(
            content.contains("24") || content.contains("25"),
            "expected coords near 250: {content}"
        );
    }

    #[test]
    fn empty_and_missing_pages_do_not_panic() {
        // PDF with a page that has no content stream at all.
        let mut doc = Document::with_version("1.5");
        let pages_id = doc.new_object_id();
        let page_id = doc.add_object(dictionary! {
            "Type" => "Page",
            "Parent" => pages_id,
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

        let out = text_markup(
            &buf,
            MarkupKind::Strike,
            None,
            Some([9, 9, 9]),
            &EngineLimits::default(),
        )
        .unwrap();
        assert_eq!(page_count(&out), 1);
    }

    #[test]
    fn mat_mul_and_inverse_roundtrip() {
        let t = Mat::translate(10.0, 20.0);
        let s = Mat {
            a: 2.0,
            b: 0.0,
            c: 0.0,
            d: 3.0,
            e: 0.0,
            f: 0.0,
        };
        // s ∘ t : translate then scale.
        let m = s.mul(t);
        let (x, y) = m.apply(1.0, 1.0);
        assert!((x - 22.0).abs() < 1e-9, "x={x}");
        assert!((y - 63.0).abs() < 1e-9, "y={y}");
        let inv = m.inverse().unwrap();
        let (rx, ry) = inv.apply(x, y);
        assert!((rx - 1.0).abs() < 1e-9);
        assert!((ry - 1.0).abs() < 1e-9);
        // Singular matrix has no inverse.
        let bad = Mat {
            a: 0.0,
            b: 0.0,
            c: 0.0,
            d: 0.0,
            e: 0.0,
            f: 0.0,
        };
        assert!(bad.inverse().is_none());
    }

    #[test]
    fn kind_bands_are_ordered() {
        for kind in [
            MarkupKind::Highlight,
            MarkupKind::Underline,
            MarkupKind::Strike,
        ] {
            let (y0, y1) = kind_band(kind, 12.0, 0.0);
            assert!(y1 > y0, "{kind:?} band inverted");
        }
        // Underline below baseline, strike above, highlight spans both.
        let (hl0, hl1) = kind_band(MarkupKind::Highlight, 12.0, 0.0);
        let (ul0, ul1) = kind_band(MarkupKind::Underline, 12.0, 0.0);
        let (st0, st1) = kind_band(MarkupKind::Strike, 12.0, 0.0);
        assert!(ul1 < 0.0 && ul0 < ul1);
        assert!(st0 > 0.0);
        assert!(hl0 < ul0 && hl1 > st1);
    }
}
