//! Region-based redaction: opaque fill rectangles plus best-effort
//! removal of the content underneath.
//!
//! Each [`Redaction`] names a 1-based page and a rectangle
//! `[x0, y0, x1, y1]` in **default user space** — the unrotated page
//! coordinate system whose origin is the lower-left corner of the
//! page's `/MediaBox` (coordinates are absolute; a non-zero MediaBox
//! origin needs no translation). `/UserUnit` and `/Rotate` are honored
//! implicitly because the fill and the removed content live in the
//! same user space.
//!
//! For every page that has at least one region, this op:
//!
//! 1. **Scans** the page's own `/Contents` streams for `Do`
//!    invocations of image XObjects whose unit-square footprint
//!    (under the `cm`/`q`/`Q` graphics state at the `Do`) intersects a
//!    redaction rect, and removes those resource entries using the
//!    same page-private-resources technique as `remove_images`.
//! 2. **Rewrites** each content stream, dropping (a) `Do` operators
//!    that reference removed images and (b) text-showing operators
//!    (`Tj`, `TJ`, `'`, `"`) whose *estimated* text quad intersects a
//!    rect. The text-state machine (`Tm`/`Td`/`Tf`/`Tc`/`Tw`/`Tz`/`TL`/
//!    `Ts`/`Tr`, `cm`, `q`/`Q`) is a local copy of the one in
//!    `text_markup` — its helpers are private to that module.
//! 3. **Filters** the page's `/Annots` array, dropping annotations
//!    whose `/Rect` intersects a redaction rect.
//! 4. **Appends** an overlay stream (via
//!    [`watermark::append_page_overlay`]) painting an opaque black
//!    rectangle over each region, with an optional centered `label`
//!    drawn in white on top.
//!
//! # Honesty notes — read before relying on this
//!
//! - **The black fill is the guarantee; everything else is
//!   best-effort.** Content drawn *inside* Form XObjects (`Do` of
//!   `/Subtype /Form`), Type3 glyph descriptions, tiling patterns,
//!   annotation appearance streams (`/AP`), optional content layers
//!   and inline images (`BI`/`ID`/`EI`) is **not** inspected and may
//!   remain visible or recoverable in the file. This op must not be
//!   relied on for security-sensitive sanitization.
//! - Image overlap uses the image's unit square under the CTM active
//!   at `Do`; a non-identity `/Matrix` on the image XObject is ignored.
//! - Text quads reuse `text_markup`'s approximations: glyph advances
//!   are estimated (~0.5 em ASCII / ~1 em non-ASCII), so borderline
//!   text may be kept or removed incorrectly. Text inside Form
//!   XObjects is not seen at all.
//! - Each content stream is walked with fresh text/graphics state —
//!   state carried across an array of `/Contents` streams is not
//!   tracked (rare in practice; documented approximation).
//! - A content stream shared between a redacted page and a
//!   non-redacted page is rewritten once and affects both.
//! - Streams that fail to decode are left untouched and reported in
//!   `warnings`; content they contain is not redacted.
//! - Removed image objects and dropped text operators leave orphaned
//!   objects in the file (lopdf does not garbage-collect); `sanitize`
//!   or `compress` can shrink the result.
//! - Rewritten content streams are stored **uncompressed**
//!   (`/Filter`/`/DecodeParms` dropped); `doc.compress()` at the end
//!   re-compresses everything.
//! - Encrypted documents are rejected via [`reject_encrypted`].

use std::collections::{BTreeMap, BTreeSet};

use lopdf::content::{Content, Operation};
use lopdf::{Document, Object, ObjectId};
use serde::{Deserialize, Serialize};

use crate::crates::core::engine::EngineLimits;

use crate::crates::pdf_engine::error::{PdfError, PdfResult};

use super::load::{load_pdf, lopdf_err, reject_encrypted, save_pdf};
use super::watermark::{
    append_page_overlay, effective_resources_dict, encode_operations, estimate_text_width,
    pdf_text_operand,
};

/// Decompression bound per existing content stream — same 64 MiB
/// guard `watermark`/`text_markup` use.
const MAX_CONTENT_STREAM: usize = 64 * 1024 * 1024;

/// One redaction region: a 1-based `page` and a rectangle
/// `[x0, y0, x1, y1]` in default user space (see module docs).
#[derive(Debug, Clone, Copy, PartialEq, Serialize, Deserialize)]
pub struct Redaction {
    /// 1-based page number.
    pub page: u32,
    /// Rectangle `[x0, y0, x1, y1]`; `x0 <= x1`, `y0 <= y1`, all finite.
    pub rect: [f64; 4],
}

/// Outcome of [`redact_regions`].
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
pub struct RedactReport {
    /// Number of redaction rectangles applied (fill rects drawn).
    pub regions_applied: usize,
    /// Number of text-showing operators removed for intersecting a
    /// redaction rect.
    pub text_ops_removed: usize,
    /// Number of image XObject resource entries removed (counted per
    /// page, like `remove_images`).
    pub images_removed: usize,
    /// Non-fatal issues: undecodable streams, dropped annotations,
    /// approximations the caller should know about.
    pub warnings: Vec<String>,
}

/// Apply opaque black redaction rectangles to `input`.
///
/// `regions` must be non-empty; every rect must be finite and
/// non-inverted (`x0 <= x1`, `y0 <= y1`). `label`, when `Some`, is
/// drawn centered inside each region in white — pass `None` for an
/// unmarked black box. Page numbers are 1-based and must exist.
/// Returns the rewritten PDF bytes and a [`RedactReport`].
/// Encrypted documents are rejected.
pub fn redact_regions(
    input: &[u8],
    regions: &[Redaction],
    label: Option<&str>,
    limits: &EngineLimits,
) -> PdfResult<(Vec<u8>, RedactReport)> {
    if regions.is_empty() {
        return Err(PdfError::InvalidInput(
            "redact_regions requires at least one region".to_string(),
        ));
    }
    for region in regions {
        validate_region(region)?;
    }

    let mut doc = load_pdf(input, limits)?;
    reject_encrypted(&doc)?;

    // Group rects by page up front so each page is processed once.
    let mut by_page: BTreeMap<u32, Vec<[f64; 4]>> = BTreeMap::new();
    for region in regions {
        by_page.entry(region.page).or_default().push(region.rect);
    }

    let page_count = doc.get_pages().len() as u32;
    for &page_number in by_page.keys() {
        if page_number == 0 || page_number > page_count {
            return Err(PdfError::InvalidPageRange(format!(
                "redaction page {page_number} out of range; document has {page_count} page(s)"
            )));
        }
    }

    let pages_map = doc.get_pages();
    let mut report = RedactReport::default();
    for (page_number, rects) in &by_page {
        let Some(&page_id) = pages_map.get(page_number) else {
            continue; // validated above
        };
        report.regions_applied += rects.len();
        redact_page(&mut doc, page_id, *page_number, rects, label, &mut report)?;
    }

    // Recompress (merged overlay streams and rewritten content) to keep
    // output size sane — same convention as watermark/text_markup.
    doc.compress();
    let bytes = save_pdf(&mut doc)?;
    Ok((bytes, report))
}

/// Validate one region's rect: all coordinates finite, non-inverted.
fn validate_region(region: &Redaction) -> PdfResult<()> {
    let [x0, y0, x1, y1] = region.rect;
    if ![x0, y0, x1, y1].iter().all(|v| v.is_finite()) {
        return Err(PdfError::InvalidInput(format!(
            "redaction rect on page {} must be finite, got {:?}",
            region.page, region.rect
        )));
    }
    if x0 > x1 || y0 > y1 {
        return Err(PdfError::InvalidInput(format!(
            "redaction rect on page {} is inverted: {:?} (expected x0<=x1, y0<=y1)",
            region.page, region.rect
        )));
    }
    Ok(())
}

/// Axis-aligned overlap test on `[x0, y0, x1, y1]` boxes. Touching
/// edges do not count as overlap.
fn rects_overlap(a: [f64; 4], b: [f64; 4]) -> bool {
    a[0] < b[2] && b[0] < a[2] && a[1] < b[3] && b[1] < a[3]
}

/// Whether `bbox` intersects any of `rects`.
fn overlaps_any(bbox: [f64; 4], rects: &[[f64; 4]]) -> bool {
    rects.iter().any(|r| rects_overlap(bbox, *r))
}

/// Run all four redaction passes on one page.
fn redact_page(
    doc: &mut Document,
    page_id: ObjectId,
    page_number: u32,
    rects: &[[f64; 4]],
    label: Option<&str>,
    report: &mut RedactReport,
) -> PdfResult<()> {
    // Pass 1: find image XObject resource names whose painted area
    // overlaps a rect, then remove them page-privately.
    let removed_names = scan_overlapping_images(doc, page_id, rects)?;
    if !removed_names.is_empty() {
        remove_image_resources(doc, page_id, &removed_names)?;
        report.images_removed += removed_names.len();
    }

    // Pass 2: rewrite content streams — drop removed `Do`s and
    // intersecting text-showing ops.
    for stream_id in doc.get_page_contents(page_id) {
        rewrite_content_stream(doc, stream_id, rects, &removed_names, page_number, report)?;
    }

    // Pass 3: drop annotations whose /Rect intersects a region.
    redact_annotations(doc, page_id, page_number, rects, report)?;

    // Pass 4: paint the opaque fills (and optional labels) on top.
    append_redaction_overlay(doc, page_id, rects, label)?;
    Ok(())
}

// ---------------------------------------------------------------------------
// Affine matrix (same convention as text_markup's private Mat)
// ---------------------------------------------------------------------------

/// 2-D affine matrix `[a b c d e f]` mapping `(x, y)` to
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
}

fn mat_from_operands(operands: &[Object]) -> Option<Mat> {
    if operands.len() < 6 {
        return None;
    }
    let mut v = [0f64; 6];
    for (slot, obj) in v.iter_mut().zip(operands.iter()) {
        *slot = f64::from(obj.as_float().ok()?);
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
        .map(f64::from)
        .filter(|v| v.is_finite())
}

// ---------------------------------------------------------------------------
// Pass 1: overlapping image XObjects
// ---------------------------------------------------------------------------

/// Decode a stream's content bounded by [`MAX_CONTENT_STREAM`].
/// Streams without `/Filter` fall back to raw bytes; undecodable
/// streams yield `None`.
fn stream_ops(stream: &lopdf::Stream) -> Option<Content> {
    let raw = match stream.decompressed_content_with_limit(MAX_CONTENT_STREAM) {
        Ok(data) => data,
        Err(_) if !stream.dict.has(b"Filter") => stream.content.clone(),
        Err(_) => return None,
    };
    Content::decode(&raw).ok()
}

/// Resolve the page's effective `/Resources` → `/XObject` dictionary
/// (cloned). `None` when absent or not a dictionary.
fn page_xobjects(doc: &Document, page_id: ObjectId) -> Option<lopdf::Dictionary> {
    let resources = effective_resources_dict(doc, page_id)?;
    let xobj = resources.get(b"XObject").ok()?;
    let resolved = match xobj {
        Object::Reference(id) => doc.get_object(*id).ok()?,
        other => other,
    };
    resolved.as_dict().ok().cloned()
}

/// Whether a dereferenced XObject target is an image stream.
fn is_image_target(doc: &Document, target: &Object) -> bool {
    let Ok(stream) = target.as_stream() else {
        return false;
    };
    stream
        .dict
        .get(b"Subtype")
        .ok()
        .and_then(|o| doc.dereference(o).ok())
        .and_then(|(_, o)| o.as_name().ok())
        .is_some_and(|name| name == b"Image")
}

/// Scan the page's content streams for `Do` operators that paint an
/// image XObject overlapping any rect. Returns the set of resource
/// names (no leading slash) to remove.
fn scan_overlapping_images(
    doc: &Document,
    page_id: ObjectId,
    rects: &[[f64; 4]],
) -> PdfResult<BTreeSet<Vec<u8>>> {
    let Some(xobjects) = page_xobjects(doc, page_id) else {
        return Ok(BTreeSet::new());
    };
    let mut removed = BTreeSet::new();
    for stream_id in doc.get_page_contents(page_id) {
        let Ok(object) = doc.get_object(stream_id) else {
            continue;
        };
        let Ok(stream) = object.as_stream() else {
            continue;
        };
        let Some(content) = stream_ops(stream) else {
            continue;
        };
        // Only the graphics state matters here: q/Q/cm.
        let mut ctm = Mat::IDENTITY;
        let mut gstack: Vec<Mat> = Vec::new();
        for op in &content.operations {
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
                "Do" => {
                    let Some(Object::Name(name)) = op.operands.first() else {
                        continue;
                    };
                    let Ok(target_obj) = xobjects.get(name.as_slice()) else {
                        continue;
                    };
                    let Ok((_, target)) = doc.dereference(target_obj) else {
                        continue;
                    };
                    if !is_image_target(doc, target) {
                        continue;
                    }
                    // The image covers the unit square in the space in
                    // effect at Do; map its corners through the CTM to
                    // default user space (initial CTM is identity).
                    let mut bbox: Option<[f64; 4]> = None;
                    for (x, y) in [(0.0, 0.0), (1.0, 0.0), (1.0, 1.0), (0.0, 1.0)] {
                        let (dx, dy) = ctm.apply(x, y);
                        if !dx.is_finite() || !dy.is_finite() {
                            bbox = None;
                            break;
                        }
                        bbox = Some(match bbox {
                            None => [dx, dy, dx, dy],
                            Some([x0, y0, x1, y1]) => {
                                [x0.min(dx), y0.min(dy), x1.max(dx), y1.max(dy)]
                            }
                        });
                    }
                    if let Some(b) = bbox
                        && overlaps_any(b, rects)
                    {
                        removed.insert(name.clone());
                    }
                }
                _ => {}
            }
        }
    }
    Ok(removed)
}

/// Remove `names` from the page's effective `/Resources` → `/XObject`
/// dictionary, writing a fresh page-private resources object — the
/// `remove_images` technique, so shared resource dictionaries on
/// unselected pages are never mutated.
fn remove_image_resources(
    doc: &mut Document,
    page_id: ObjectId,
    names: &BTreeSet<Vec<u8>>,
) -> PdfResult<()> {
    let Some(mut resources) = effective_resources_dict(doc, page_id) else {
        return Ok(());
    };
    let Ok(xobj) = resources.get(b"XObject") else {
        return Ok(());
    };
    let resolved;
    let xobjects = match xobj {
        Object::Reference(id) => {
            resolved = doc.get_object(*id).map_err(lopdf_err)?;
            resolved.as_dict().map_err(lopdf_err)?
        }
        other => other.as_dict().map_err(lopdf_err)?,
    };
    let mut new_xobjects = xobjects.clone();
    for name in names {
        new_xobjects.remove(name);
    }
    resources.set(b"XObject", Object::Dictionary(new_xobjects));

    // Always write a *new* indirect resources dictionary onto the page.
    let resources_id = doc
        .try_add_object(Object::Dictionary(resources))
        .map_err(lopdf_err)?;
    doc.get_dictionary_mut(page_id)
        .map_err(lopdf_err)?
        .set("Resources", Object::Reference(resources_id));
    Ok(())
}

// ---------------------------------------------------------------------------
// Pass 2: content-stream rewrite (text ops + dangling Do removal)
// ---------------------------------------------------------------------------

/// Minimal text state — a local copy of `text_markup`'s private
/// `TextMachine` (its helpers are not `pub(crate)`).
struct TextState {
    tm: Mat,
    tlm: Mat,
    fs: f64,
    tc: f64,
    tw: f64,
    tz: f64,
    tl: f64,
    ts: f64,
}

impl TextState {
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
        }
    }

    fn move_line(&mut self, tx: f64, ty: f64) {
        self.tlm = self.tlm.mul(Mat::translate(tx, ty));
        self.tm = self.tlm;
    }

    fn advance(&mut self, w: f64) {
        self.tm = self.tm.mul(Mat::translate(w, 0.0));
    }

    /// Estimated advance width for a shown byte string, including
    /// `Tc`/`Tw`/`Tz` — same heuristic as `text_markup`.
    fn show_width(&self, bytes: &[u8]) -> f64 {
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
        let raw = units * self.fs + self.tc * n + self.tw * spaces;
        let w = raw * (self.tz / 100.0);
        if w.is_finite() && w > 0.0 { w } else { 0.0 }
    }
}

/// Estimated advance for a `TJ` array: string bytes plus `-n/1000 em`
/// kern adjustments.
fn tj_width(machine: &TextState, array: &[Object]) -> f64 {
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

/// Estimated text quad for a show of width `w` starting at the current
/// text origin, mapped through `tm` then `ctm` into default user
/// space. Returns `[x0, y0, x1, y1]` or `None` for degenerate /
/// non-finite geometry. The y-band is the same approximation
/// `text_markup` uses for highlights.
fn show_bbox(machine: &TextState, ctm: &Mat, w: f64) -> Option<[f64; 4]> {
    if !(w.is_finite() && w > 0.0) || !(machine.fs.is_finite() && machine.fs > 0.0) {
        return None;
    }
    let pad = (0.05 * machine.fs).max(0.5);
    let y0 = machine.ts - 0.35 * machine.fs;
    let y1 = machine.ts + 0.85 * machine.fs;
    let mut bbox: Option<[f64; 4]> = None;
    for (x, y) in [(-pad, y0), (w + pad, y0), (w + pad, y1), (-pad, y1)] {
        let (ux, uy) = machine.tm.apply(x, y);
        let (dx, dy) = ctm.apply(ux, uy);
        if !dx.is_finite() || !dy.is_finite() {
            return None;
        }
        bbox = Some(match bbox {
            None => [dx, dy, dx, dy],
            Some([x0, y0, x1, y1]) => [x0.min(dx), y0.min(dy), x1.max(dx), y1.max(dy)],
        });
    }
    bbox
}

/// Handle a text-showing operator: returns `(width, intersects)`.
/// The caller advances the text matrix by `width` whether or not the
/// op is kept.
fn measure_show(
    machine: &TextState,
    ctm: &Mat,
    operands: &[Object],
    rects: &[[f64; 4]],
) -> (f64, bool) {
    let w = match operands.first() {
        Some(Object::Array(items)) => tj_width(machine, items),
        Some(Object::String(bytes, _)) => machine.show_width(bytes),
        _ => 0.0,
    };
    let intersects = show_bbox(machine, ctm, w).is_some_and(|b| overlaps_any(b, rects));
    (w, intersects)
}

/// Handle `'` / `"` shows: string is the last operand; `"` also sets
/// word/char spacing from operands 0/1 (caller applies that first).
fn measure_quote_show(
    machine: &TextState,
    ctm: &Mat,
    operands: &[Object],
    rects: &[[f64; 4]],
) -> (f64, bool) {
    match operands.last() {
        Some(Object::String(bytes, _)) => {
            let w = machine.show_width(bytes);
            let intersects = show_bbox(machine, ctm, w).is_some_and(|b| overlaps_any(b, rects));
            (w, intersects)
        }
        _ => (0.0, false),
    }
}

/// Decode, filter and rewrite one content stream in place: drop `Do`
/// ops naming removed images and text-showing ops whose estimated quad
/// intersects a redaction rect. Streams that fail to decode are left
/// untouched and reported via `warnings`.
#[allow(clippy::too_many_arguments)]
fn rewrite_content_stream(
    doc: &mut Document,
    stream_id: ObjectId,
    rects: &[[f64; 4]],
    removed_names: &BTreeSet<Vec<u8>>,
    page_number: u32,
    report: &mut RedactReport,
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
        Err(_) if !stream.dict.has(b"Filter") => stream.content.clone(),
        Err(_) => {
            report.warnings.push(format!(
                "page {page_number}: a content stream could not be decompressed; \
                 redaction may be incomplete"
            ));
            return Ok(());
        }
    };
    let Ok(content) = Content::decode(&raw) else {
        report.warnings.push(format!(
            "page {page_number}: a content stream could not be decoded; \
             redaction may be incomplete"
        ));
        return Ok(());
    };

    let mut machine = TextState::new();
    let mut ctm = Mat::IDENTITY;
    let mut gstack: Vec<Mat> = Vec::new();
    let mut kept: Vec<Operation> = Vec::with_capacity(content.operations.len());
    // Removed ops of any kind (decides whether the stream is rewritten);
    // text-showing removals are counted separately for the report.
    let mut removed_here = 0usize;
    let mut text_removed = 0usize;

    for op in content.operations {
        let operator = op.operator.as_str();
        match operator {
            "q" => {
                gstack.push(ctm);
                kept.push(op);
            }
            "Q" => {
                if let Some(m) = gstack.pop() {
                    ctm = m;
                }
                kept.push(op);
            }
            "cm" => {
                if let Some(m) = mat_from_operands(&op.operands) {
                    ctm = ctm.mul(m);
                }
                kept.push(op);
            }
            "BT" => {
                machine.tm = Mat::IDENTITY;
                machine.tlm = Mat::IDENTITY;
                kept.push(op);
            }
            "Tf" => {
                if let Some(size) = num_operand(&op, 1) {
                    machine.fs = size;
                }
                kept.push(op);
            }
            "Tc" => {
                if let Some(v) = num_operand(&op, 0) {
                    machine.tc = v;
                }
                kept.push(op);
            }
            "Tw" => {
                if let Some(v) = num_operand(&op, 0) {
                    machine.tw = v;
                }
                kept.push(op);
            }
            "Tz" => {
                if let Some(v) = num_operand(&op, 0) {
                    machine.tz = v;
                }
                kept.push(op);
            }
            "TL" => {
                if let Some(v) = num_operand(&op, 0) {
                    machine.tl = v;
                }
                kept.push(op);
            }
            "Ts" => {
                if let Some(v) = num_operand(&op, 0) {
                    machine.ts = v;
                }
                kept.push(op);
            }
            "Td" | "TD" => {
                if let (Some(tx), Some(ty)) = (num_operand(&op, 0), num_operand(&op, 1)) {
                    if operator == "TD" {
                        machine.tl = -ty;
                    }
                    machine.move_line(tx, ty);
                }
                kept.push(op);
            }
            "Tm" => {
                if let Some(m) = mat_from_operands(&op.operands) {
                    machine.tm = m;
                    machine.tlm = m;
                }
                kept.push(op);
            }
            "T*" => {
                machine.move_line(0.0, -machine.tl);
                kept.push(op);
            }
            "Tj" | "TJ" => {
                let (w, intersects) = measure_show(&machine, &ctm, &op.operands, rects);
                machine.advance(w);
                if intersects {
                    removed_here += 1;
                    text_removed += 1;
                } else {
                    kept.push(op);
                }
            }
            "'" => {
                machine.move_line(0.0, -machine.tl);
                let (w, intersects) = measure_quote_show(&machine, &ctm, &op.operands, rects);
                machine.advance(w);
                if intersects {
                    removed_here += 1;
                    text_removed += 1;
                } else {
                    kept.push(op);
                }
            }
            "\"" => {
                // aw ac " — set word/char spacing, then behave like '.
                if let Some(aw) = num_operand(&op, 0) {
                    machine.tw = aw;
                }
                if let Some(ac) = num_operand(&op, 1) {
                    machine.tc = ac;
                }
                machine.move_line(0.0, -machine.tl);
                let (w, intersects) = measure_quote_show(&machine, &ctm, &op.operands, rects);
                machine.advance(w);
                if intersects {
                    removed_here += 1;
                    text_removed += 1;
                } else {
                    kept.push(op);
                }
            }
            "Do" => {
                let is_removed = matches!(
                    op.operands.first(),
                    Some(Object::Name(name)) if removed_names.contains(name)
                );
                if is_removed {
                    removed_here += 1;
                } else {
                    kept.push(op);
                }
            }
            _ => kept.push(op),
        }
    }

    if removed_here == 0 {
        return Ok(()); // nothing changed — leave the stream untouched
    }
    report.text_ops_removed += text_removed;

    let encoded = Content { operations: kept }.encode().map_err(lopdf_err)?;
    stream.content = encoded;
    // New bytes are uncompressed: drop filter entries so readers do
    // not try to decode them. /Length is rewritten on save.
    stream.dict.remove(b"Filter");
    stream.dict.remove(b"DecodeParms");
    Ok(())
}

// ---------------------------------------------------------------------------
// Pass 3: annotations
// ---------------------------------------------------------------------------

/// Dereference an object one level (handles `/Rect` arrays stored
/// indirectly, though rare).
fn deref<'a>(doc: &'a Document, obj: &'a Object) -> Option<&'a Object> {
    match obj {
        Object::Reference(id) => doc.get_object(*id).ok(),
        other => Some(other),
    }
}

/// Extract a `/Rect` `[x0, y0, x1, y1]` (f64) from an annotation
/// dictionary object. Returns `None` when absent or malformed — such
/// annotations are kept (fail-safe: don't delete what we can't locate).
fn annot_rect(doc: &Document, annot_obj: &Object) -> Option<[f64; 4]> {
    let dict_obj = deref(doc, annot_obj)?;
    let dict = dict_obj.as_dict().ok()?;
    let rect_obj = dict.get(b"Rect").ok()?;
    let array = deref(doc, rect_obj)?.as_array().ok()?;
    if array.len() < 4 {
        return None;
    }
    let mut out = [0f64; 4];
    for (slot, item) in out.iter_mut().zip(array.iter()) {
        let num = deref(doc, item)?.as_float().ok()?;
        *slot = f64::from(num);
        if !slot.is_finite() {
            return None;
        }
    }
    Some(out)
}

/// Drop `/Annots` entries whose `/Rect` intersects a redaction rect.
fn redact_annotations(
    doc: &mut Document,
    page_id: ObjectId,
    page_number: u32,
    rects: &[[f64; 4]],
    report: &mut RedactReport,
) -> PdfResult<()> {
    // Collect the annots array (resolving one level of indirection).
    let annots_value = {
        let page = doc.get_dictionary(page_id).map_err(lopdf_err)?;
        match page.get(b"Annots") {
            Ok(v) => v.clone(),
            Err(_) => return Ok(()),
        }
    };
    let annots_array = match deref(doc, &annots_value) {
        Some(Object::Array(items)) => items.clone(),
        _ => return Ok(()),
    };
    if annots_array.is_empty() {
        return Ok(());
    }

    let mut kept: Vec<Object> = Vec::with_capacity(annots_array.len());
    let mut dropped = 0usize;
    for annot in annots_array {
        match annot_rect(doc, &annot) {
            Some(r) if overlaps_any(r, rects) => dropped += 1,
            _ => kept.push(annot),
        }
    }
    if dropped == 0 {
        return Ok(());
    }
    report.warnings.push(format!(
        "page {page_number}: removed {dropped} annotation(s) overlapping a redaction region"
    ));

    let page = doc.get_dictionary_mut(page_id).map_err(lopdf_err)?;
    if kept.is_empty() {
        page.remove(b"Annots");
    } else {
        page.set("Annots", Object::Array(kept));
    }
    Ok(())
}

// ---------------------------------------------------------------------------
// Pass 4: opaque fill overlay (+ optional label)
// ---------------------------------------------------------------------------

/// Build and append the overlay stream painting a black rectangle over
/// each rect, with `label` centered in white when provided.
fn append_redaction_overlay(
    doc: &mut Document,
    page_id: ObjectId,
    rects: &[[f64; 4]],
    label: Option<&str>,
) -> PdfResult<()> {
    let label = label.map(str::trim).filter(|s| !s.is_empty());
    let font_name = if label.is_some() {
        Some(super::watermark::ensure_font_resource(doc, page_id)?)
    } else {
        None
    };

    let mut operations: Vec<Operation> = Vec::new();
    for rect in rects {
        let [x0, y0, x1, y1] = *rect;
        let (w, h) = (x1 - x0, y1 - y0);
        operations.push(Operation::new("q", vec![]));
        operations.push(Operation::new(
            "rg",
            vec![Object::Real(0.0), Object::Real(0.0), Object::Real(0.0)],
        ));
        operations.push(Operation::new(
            "re",
            vec![
                Object::Real(x0 as f32),
                Object::Real(y0 as f32),
                Object::Real(w as f32),
                Object::Real(h as f32),
            ],
        ));
        operations.push(Operation::new("f", vec![]));
        operations.push(Operation::new("Q", vec![]));

        if let (Some(text), Some(font)) = (label, font_name.as_deref()) {
            // Center the label in the rect; size scales with the
            // smaller dimension and is clamped to a readable range.
            let size = (w.min(h) * 0.30).clamp(6.0, 48.0) as f32;
            let est_w = estimate_text_width(text, size);
            let tx = (x0 + w * 0.5 - f64::from(est_w) * 0.5) as f32;
            let ty = (y0 + h * 0.5 - f64::from(size) * 0.35) as f32;
            operations.push(Operation::new("q", vec![]));
            operations.push(Operation::new(
                "rg",
                vec![Object::Real(1.0), Object::Real(1.0), Object::Real(1.0)],
            ));
            operations.push(Operation::new("BT", vec![]));
            operations.push(Operation::new(
                "Tf",
                vec![Object::Name(font.as_bytes().to_vec()), Object::Real(size)],
            ));
            operations.push(Operation::new(
                "Tm",
                vec![
                    Object::Real(1.0),
                    Object::Real(0.0),
                    Object::Real(0.0),
                    Object::Real(1.0),
                    Object::Real(tx),
                    Object::Real(ty),
                ],
            ));
            operations.push(Operation::new("Tj", vec![pdf_text_operand(text)]));
            operations.push(Operation::new("ET", vec![]));
            operations.push(Operation::new("Q", vec![]));
        }
    }

    let stream_id = super::watermark::add_content_stream(doc, encode_operations(operations)?)?;
    // Drawn on top of existing content — the fill must cover it.
    append_page_overlay(doc, page_id, stream_id)
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;
    use crate::crates::pdf_engine::ops::encrypt::{PdfPermissions, encrypt};
    use crate::crates::pdf_engine::ops::extract_images;
    use crate::crates::pdf_engine::ops::extract_text;
    use crate::crates::pdf_engine::ops::test_utils::{page_count, pdf_with_pages};
    use lopdf::content::Operation;
    use lopdf::{Document as LopdfDocument, Stream, dictionary};

    fn limits() -> EngineLimits {
        EngineLimits::default()
    }

    /// test_utils places text at (100, 700) with 12pt Courier.
    fn text_rect() -> [f64; 4] {
        [90.0, 690.0, 300.0, 720.0]
    }

    fn far_rect() -> [f64; 4] {
        [400.0, 100.0, 500.0, 200.0]
    }

    #[test]
    fn removes_text_under_region() {
        let input = pdf_with_pages(&["Hello World", "Nothing to hide"]);
        let regions = [
            Redaction {
                page: 1,
                rect: text_rect(),
            },
            Redaction {
                page: 2,
                rect: far_rect(),
            },
        ];
        let (out, report) = redact_regions(&input, &regions, None, &limits()).unwrap();
        assert_eq!(report.regions_applied, 2);
        assert_eq!(report.text_ops_removed, 1);
        assert_eq!(page_count(&out), 2);
        let texts = extract_text(&out, None, &limits()).unwrap();
        assert!(!texts[0].text.contains("Hello World"));
        assert!(texts[1].text.contains("Nothing to hide"));
    }

    #[test]
    fn keeps_text_outside_region() {
        let input = pdf_with_pages(&["Hello World"]);
        let regions = [Redaction {
            page: 1,
            rect: far_rect(),
        }];
        let (out, report) = redact_regions(&input, &regions, None, &limits()).unwrap();
        assert_eq!(report.text_ops_removed, 0);
        let texts = extract_text(&out, None, &limits()).unwrap();
        assert!(texts[0].text.contains("Hello World"));
    }

    #[test]
    fn appends_black_fill_and_label() {
        let input = pdf_with_pages(&["secret"]);
        // A region away from the text keeps the original ops in the
        // stream so the overlay's position relative to them is
        // observable (covered text would be removed outright).
        let regions = [Redaction {
            page: 1,
            rect: far_rect(),
        }];
        let (out, _report) = redact_regions(&input, &regions, Some("REDACTED"), &limits()).unwrap();
        let doc = LopdfDocument::load_mem(&out).unwrap();
        let page_id = doc.get_pages()[&1];
        let content = String::from_utf8_lossy(&doc.get_page_content(page_id)).into_owned();
        // The overlay `re`/`f` fill and the label Tj must be present.
        assert!(content.contains(" re"), "fill rect present: {content}");
        assert!(content.contains("(REDACTED)"), "label present: {content}");
        // Fill comes after the original text ops (drawn on top).
        let text_pos = content.find("(secret)").unwrap();
        let re_pos = content.find(" re").unwrap();
        assert!(re_pos > text_pos, "overlay must be appended last");
    }

    #[test]
    fn honors_cm_transformed_text_position() {
        // Text drawn under a translate-only CTM: user-space position
        // shifts by the cm translation.
        let ops = vec![
            Operation::new("q", vec![]),
            Operation::new(
                "cm",
                vec![
                    Object::Real(1.0),
                    Object::Real(0.0),
                    Object::Real(0.0),
                    Object::Real(1.0),
                    Object::Real(200.0),
                    Object::Real(0.0),
                ],
            ),
            Operation::new("BT", vec![]),
            Operation::new("Tf", vec!["F1".into(), 12.into()]),
            Operation::new("Td", vec![100.into(), 700.into()]),
            Operation::new("Tj", vec![Object::string_literal("moved")]),
            Operation::new("ET", vec![]),
            Operation::new("Q", vec![]),
        ];
        let mut doc = LopdfDocument::with_version("1.5");
        let pages_id = doc.new_object_id();
        let font_id = doc.add_object(dictionary! {
            "Type" => "Font", "Subtype" => "Type1", "BaseFont" => "Courier",
        });
        let resources_id = doc.add_object(dictionary! {
            "Font" => dictionary! { "F1" => font_id },
        });
        let content = Content { operations: ops };
        let content_id = doc.add_object(Stream::new(dictionary! {}, content.encode().unwrap()));
        let page_id = doc.add_object(dictionary! {
            "Type" => "Page", "Parent" => pages_id,
            "Contents" => content_id, "Resources" => resources_id,
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
            "Type" => "Catalog", "Pages" => pages_id,
        });
        doc.trailer.set("Root", catalog_id);
        let mut buf = Vec::new();
        doc.save_to(&mut buf).unwrap();

        // Text lands at user-space x≈300 (100 + 200 translate). A rect
        // over the *unshifted* position must not match; one over the
        // shifted position must.
        let regions = [Redaction {
            page: 1,
            rect: [90.0, 690.0, 200.0, 720.0],
        }];
        let (_out, report) = redact_regions(&buf, &regions, None, &limits()).unwrap();
        assert_eq!(report.text_ops_removed, 0);

        let regions = [Redaction {
            page: 1,
            rect: [290.0, 690.0, 450.0, 720.0],
        }];
        let (_out, report) = redact_regions(&buf, &regions, None, &limits()).unwrap();
        assert_eq!(report.text_ops_removed, 1);
    }

    /// Build a one-page PDF with an image XObject `/Im1` painted at
    /// user-space rect roughly [50,700]-[150,800] via `cm`+`Do`, plus
    /// an overlapping `/Annots` entry.
    fn pdf_with_image_and_annot() -> Vec<u8> {
        let mut doc = LopdfDocument::with_version("1.5");
        let pages_id = doc.new_object_id();
        let font_id = doc.add_object(dictionary! {
            "Type" => "Font", "Subtype" => "Type1", "BaseFont" => "Courier",
        });
        let image_id = doc.add_object(Stream::new(
            dictionary! {
                "Type" => "XObject", "Subtype" => "Image",
                "Width" => 2i64, "Height" => 2i64,
                "BitsPerComponent" => 8i64, "ColorSpace" => "DeviceGray",
            },
            vec![0u8, 255, 128, 64],
        ));
        let resources_id = doc.add_object(dictionary! {
            "Font" => dictionary! { "F1" => font_id },
            "XObject" => dictionary! { "Im1" => image_id },
        });
        // Image occupies unit square scaled by cm to [50,700]-[150,800].
        let ops = vec![
            Operation::new("q", vec![]),
            Operation::new(
                "cm",
                vec![
                    Object::Real(100.0),
                    Object::Real(0.0),
                    Object::Real(0.0),
                    Object::Real(100.0),
                    Object::Real(50.0),
                    Object::Real(700.0),
                ],
            ),
            Operation::new("Do", vec![Object::Name(b"Im1".to_vec())]),
            Operation::new("Q", vec![]),
            Operation::new("BT", vec![]),
            Operation::new("Tf", vec!["F1".into(), 12.into()]),
            Operation::new("Td", vec![100.into(), 600.into()]),
            Operation::new("Tj", vec![Object::string_literal("caption")]),
            Operation::new("ET", vec![]),
        ];
        let content = Content { operations: ops };
        let content_id = doc.add_object(Stream::new(dictionary! {}, content.encode().unwrap()));
        let annot_id = doc.add_object(dictionary! {
            "Type" => "Annot", "Subtype" => "Text",
            "Rect" => vec![60.into(), 710.into(), 90.into(), 740.into()],
            "Contents" => Object::string_literal("note"),
        });
        let page_id = doc.add_object(dictionary! {
            "Type" => "Page", "Parent" => pages_id,
            "Contents" => content_id, "Resources" => resources_id,
            "MediaBox" => vec![0.into(), 0.into(), 595.into(), 842.into()],
            "Annots" => vec![Object::Reference(annot_id)],
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
            "Type" => "Catalog", "Pages" => pages_id,
        });
        doc.trailer.set("Root", catalog_id);
        let mut buf = Vec::new();
        doc.save_to(&mut buf).unwrap();
        buf
    }

    #[test]
    fn removes_overlapping_image_and_annot() {
        let input = pdf_with_image_and_annot();
        assert_eq!(extract_images(&input, None, &limits()).unwrap().len(), 1);

        // Region covers the image footprint [50,700]-[150,800] and the
        // annotation rect, but not the caption text at y≈600.
        let regions = [Redaction {
            page: 1,
            rect: [40.0, 690.0, 160.0, 810.0],
        }];
        let (out, report) = redact_regions(&input, &regions, None, &limits()).unwrap();
        assert_eq!(report.images_removed, 1);
        assert_eq!(report.text_ops_removed, 0);
        assert_eq!(report.regions_applied, 1);
        assert!(
            report.warnings.iter().any(|w| w.contains("annotation")),
            "annotation removal reported: {:?}",
            report.warnings
        );

        // Image no longer reachable through the page.
        assert!(extract_images(&out, None, &limits()).unwrap().is_empty());

        // Caption text survives (outside the region).
        let texts = extract_text(&out, None, &limits()).unwrap();
        assert!(texts[0].text.contains("caption"));

        // No dangling Do for /Im1 remains in the page content.
        let doc = LopdfDocument::load_mem(&out).unwrap();
        let page_id = doc.get_pages()[&1];
        let mut saw_do = false;
        for stream_id in doc.get_page_contents(page_id) {
            let obj = doc.get_object(stream_id).unwrap();
            let stream = obj.as_stream().unwrap();
            let raw = stream
                .decompressed_content()
                .unwrap_or_else(|_| stream.content.clone());
            if let Ok(content) = Content::decode(&raw) {
                saw_do |= content.operations.iter().any(|op| {
                    op.operator == "Do"
                        && matches!(op.operands.first(), Some(Object::Name(n)) if n == b"Im1")
                });
            }
        }
        assert!(!saw_do, "page still invokes /Im1 via Do");

        // Annots array removed entirely (only entry was dropped).
        let page = doc.get_dictionary(page_id).unwrap();
        assert!(page.get(b"Annots").is_err());
    }

    #[test]
    fn image_outside_region_is_kept() {
        let input = pdf_with_image_and_annot();
        let regions = [Redaction {
            page: 1,
            rect: far_rect(),
        }];
        let (out, report) = redact_regions(&input, &regions, None, &limits()).unwrap();
        assert_eq!(report.images_removed, 0);
        assert_eq!(extract_images(&out, None, &limits()).unwrap().len(), 1);
    }

    #[test]
    fn validates_regions() {
        let input = pdf_with_pages(&["x"]);
        // Empty region list.
        assert!(redact_regions(&input, &[], None, &limits()).is_err());
        // Non-finite coordinates.
        for bad in [
            [f64::NAN, 0.0, 1.0, 1.0],
            [0.0, f64::INFINITY, 1.0, 1.0],
            [0.0, 0.0, f64::NEG_INFINITY, 1.0],
        ] {
            let regions = [Redaction { page: 1, rect: bad }];
            assert!(
                redact_regions(&input, &regions, None, &limits()).is_err(),
                "rect {bad:?} should be rejected"
            );
        }
        // Inverted rects.
        for bad in [[5.0, 0.0, 1.0, 1.0], [0.0, 9.0, 1.0, 2.0]] {
            let regions = [Redaction { page: 1, rect: bad }];
            assert!(redact_regions(&input, &regions, None, &limits()).is_err());
        }
        // Page out of range / page 0.
        for page in [0u32, 2, 99] {
            let regions = [Redaction {
                page,
                rect: [0.0, 0.0, 10.0, 10.0],
            }];
            assert!(redact_regions(&input, &regions, None, &limits()).is_err());
        }
    }

    #[test]
    fn rejects_garbage_and_encrypted() {
        assert!(
            redact_regions(
                b"junk",
                &[Redaction {
                    page: 1,
                    rect: [0.0, 0.0, 1.0, 1.0],
                }],
                None,
                &limits()
            )
            .is_err()
        );

        let input = pdf_with_pages(&["secret"]);
        let encrypted =
            encrypt(&input, "user", None, PdfPermissions::default(), &limits()).unwrap();
        let regions = [Redaction {
            page: 1,
            rect: text_rect(),
        }];
        let err = redact_regions(&encrypted, &regions, None, &limits()).unwrap_err();
        assert!(matches!(err, PdfError::Encrypted));
    }

    #[test]
    fn multiple_regions_same_page_all_apply() {
        let input = pdf_with_pages(&["aaa", "bbb"]);
        // Two disjoint regions on page 1.
        let regions = [
            Redaction {
                page: 1,
                rect: [90.0, 690.0, 200.0, 720.0],
            },
            Redaction {
                page: 1,
                rect: [400.0, 100.0, 500.0, 200.0],
            },
        ];
        let (out, report) = redact_regions(&input, &regions, None, &limits()).unwrap();
        assert_eq!(report.regions_applied, 2);
        // Only the first text op intersects; second region covers nothing.
        assert_eq!(report.text_ops_removed, 1);
        let doc = LopdfDocument::load_mem(&out).unwrap();
        let content =
            String::from_utf8_lossy(&doc.get_page_content(doc.get_pages()[&1])).into_owned();
        // Two `re` fills in one overlay stream.
        assert_eq!(content.matches(" re").count(), 2, "{content}");
    }
}
