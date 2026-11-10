//! Shared mark/overlay internals plus the text [`watermark`] operation.
//!
//! This module is the shared toolkit for ops that draw marks on existing
//! pages (`watermark`, `stamp`, and the resource helpers consumed by
//! `overlay`):
//!
//! - [`MarkPosition`] / [`position_point`] / [`box_origin`] — where a
//!   mark's bounding box sits on a page.
//! - [`media_box`] / [`resources_object`] / [`effective_resources_dict`] /
//!   [`ensure_named_resource`] / [`set_page_resources`] — page geometry
//!   and `/Resources` manipulation shared with `overlay`.
//! - [`selected_pages`] — page-range selection.
//! - [`wrap_page_content`] / [`append_page_overlay`] — merge a content
//!   stream into a page's `/Contents`.
//! - [`ensure_font_resource`] / [`ensure_ext_gstate`] /
//!   [`ensure_xobject_resource`] — register fonts, alpha and images.
//!
//! # Coordinate convention
//!
//! PDF user space has its origin at the **lower-left** corner of the
//! page's `/MediaBox`. [`position_point`] works in a *page-local* space
//! spanning `[0, w] x [0, h]`; callers translate by the MediaBox origin
//! before emitting `cm`/`Td` operands. `/UserUnit` is **not** honored —
//! all coordinates are emitted in default user units (1/72 inch).
//!
//! # Content merging
//!
//! [`wrap_page_content`] decodes every existing `/Contents` stream of a
//! page (bounded by a 64 MiB decompressed limit), concatenates them with
//! the overlay stream's operators into **one new uncompressed content
//! stream**, and points the page's `/Contents` at it. The old content
//! streams are left in the object graph unreferenced — harmless, and
//! safer than rewriting object references inside other structures.

use lopdf::content::{Content, Operation};
use lopdf::{Dictionary, Document, Object, ObjectId, Stream, StringFormat, dictionary};
use serde::{Deserialize, Serialize};

use crate::crates::core::engine::EngineLimits;

use crate::crates::pdf_engine::error::{PdfError, PdfResult};

use super::load::{load_pdf, lopdf_err, reject_encrypted, save_pdf};
use super::pages::parse_page_range;

// ---------------------------------------------------------------------------
// Mark placement
// ---------------------------------------------------------------------------

/// Where on a page an overlay mark is anchored.
///
/// The variants name the point of the mark's bounding box that is placed
/// `margin` points away from the corresponding page edge(s): corners name
/// box corners, `TopCenter`/`BottomCenter` name the midpoint of the top /
/// bottom edge, and `Center` names the page center (which is also the
/// center of the mark's box).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum MarkPosition {
    /// Centered on the page.
    Center,
    /// Top-left corner of the page (visually).
    TopLeft,
    /// Top-right corner of the page (visually).
    TopRight,
    /// Bottom-left corner of the page.
    BottomLeft,
    /// Bottom-right corner of the page.
    BottomRight,
    /// Horizontally centered, near the top edge.
    TopCenter,
    /// Horizontally centered, near the bottom edge.
    BottomCenter,
}

/// Compute the anchor point of a mark's bounding box, in the page-local
/// `[0, w] x [0, h]` space described in the module docs.
///
/// `margin` is the distance kept between the mark's box and the page
/// edge(s) for the edge/corner variants; it is ignored for `Center`.
pub(crate) fn position_point(
    position: MarkPosition,
    page_width: f32,
    page_height: f32,
    margin: f32,
) -> (f32, f32) {
    let m = margin.max(0.0);
    match position {
        MarkPosition::Center => (page_width * 0.5, page_height * 0.5),
        MarkPosition::TopLeft => (m, page_height - m),
        MarkPosition::TopRight => (page_width - m, page_height - m),
        MarkPosition::BottomLeft => (m, m),
        MarkPosition::BottomRight => (page_width - m, m),
        MarkPosition::TopCenter => (page_width * 0.5, page_height - m),
        MarkPosition::BottomCenter => (page_width * 0.5, m),
    }
}

/// Convert an anchor point (from [`position_point`]) plus the mark's
/// bounding-box size into the box's **lower-left** corner, still in
/// page-local coordinates. This is the origin callers translate to (or
/// rotate about, after adding half the box size).
pub(crate) fn box_origin(
    position: MarkPosition,
    anchor: (f32, f32),
    mark_w: f32,
    mark_h: f32,
) -> (f32, f32) {
    let x = match position {
        MarkPosition::TopLeft | MarkPosition::BottomLeft => anchor.0,
        MarkPosition::TopRight | MarkPosition::BottomRight => anchor.0 - mark_w,
        _ => anchor.0 - mark_w * 0.5,
    };
    let y = match position {
        MarkPosition::BottomLeft | MarkPosition::BottomRight | MarkPosition::BottomCenter => {
            anchor.1
        }
        MarkPosition::TopLeft | MarkPosition::TopRight | MarkPosition::TopCenter => {
            anchor.1 - mark_h
        }
        MarkPosition::Center => anchor.1 - mark_h * 0.5,
    };
    (x, y)
}

/// Rough advance-width estimate for base-14 Helvetica text, used only to
/// center marks horizontally. ASCII counts as ~0.5 em, everything else
/// (CJK, symbols) as ~1 em. This is an approximation — real centering
/// would need font metrics the engine does not carry.
pub(crate) fn estimate_text_width(text: &str, font_size: f32) -> f32 {
    let units: f32 = text
        .chars()
        .map(|c| if c.is_ascii() { 0.5 } else { 1.0 })
        .sum();
    (units * font_size).max(font_size * 0.5)
}

/// Build the PDF string operand for a `Tj` operation.
///
/// Text that fits in Latin-1 is emitted as a literal string with `\`,
/// `(`, `)` and control characters escaped. Anything containing a
/// character above U+00FF is emitted as a hexadecimal string containing
/// UTF-16BE bytes with a byte-order mark, which PDF viewers decode per
/// the PDF spec's text-string rules.
pub(crate) fn pdf_text_operand(text: &str) -> Object {
    if text.chars().all(|c| (c as u32) <= 0xFF) {
        // Build raw bytes: chars 0x80..=0xFF become single Latin-1
        // bytes (a PDFDocEncoding approximation — pushing the `char`
        // would UTF-8-encode it into two bytes). ()\ and control
        // characters are NOT pre-escaped here: `Content::encode`'s
        // writer already escapes ()\ correctly, and pre-escaping would
        // double-escape. Control chars have no WinAnsi glyph → `?`.
        let mut bytes = Vec::with_capacity(text.len() + 8);
        for c in text.chars() {
            match c {
                c if (c as u32) < 0x20 || c as u32 == 0x7F => bytes.push(b'?'),
                c => bytes.push(c as u8),
            }
        }
        Object::String(bytes, StringFormat::Literal)
    } else {
        let mut bytes = vec![0xFE, 0xFF];
        for unit in text.encode_utf16() {
            bytes.extend_from_slice(&unit.to_be_bytes());
        }
        Object::String(bytes, StringFormat::Hexadecimal)
    }
}

/// Encode a list of content operations into stream bytes.
pub(crate) fn encode_operations(operations: Vec<Operation>) -> PdfResult<Vec<u8>> {
    Content { operations }.encode().map_err(lopdf_err)
}

/// Add a content stream object holding `bytes` (uncompressed) and return
/// its object id. Callers pass the id to [`wrap_page_content`] or
/// [`append_page_overlay`].
pub(crate) fn add_content_stream(doc: &mut Document, bytes: Vec<u8>) -> PdfResult<ObjectId> {
    let stream = Stream::new(Dictionary::new(), bytes);
    doc.try_add_object(Object::Stream(stream))
        .map_err(lopdf_err)
}

// ---------------------------------------------------------------------------
// Page selection / geometry / resources (shared with `overlay`)
// ---------------------------------------------------------------------------

/// Resolve the pages selected by `pages`: `None` selects every page;
/// `Some` is parsed with the shared 1-based [`parse_page_range`] spec.
/// The result is sorted and deduplicated.
pub(crate) fn selected_pages(doc: &Document, pages: Option<&str>) -> PdfResult<Vec<u32>> {
    let page_count = doc.get_pages().len() as u32;
    match pages {
        None => Ok((1..=page_count).collect()),
        Some(spec) => parse_page_range(spec, page_count),
    }
}

/// Maximum `/Parent` hops followed while resolving inherited page keys.
const MAX_ANCESTOR_HOPS: u32 = 32;

/// Find the first `key` entry on the page or its `/Pages` ancestors,
/// returned as a cloned [`Object`].
fn find_inherited(doc: &Document, page_id: ObjectId, key: &[u8]) -> Option<Object> {
    let mut current = Some(page_id);
    for _ in 0..MAX_ANCESTOR_HOPS {
        let id = current?;
        let dict = doc.get_dictionary(id).ok()?;
        if let Ok(value) = dict.get(key) {
            return Some(value.clone());
        }
        current = match dict.get(b"Parent") {
            Ok(Object::Reference(parent)) => Some(*parent),
            _ => None,
        };
    }
    None
}

/// Return the page's effective `/MediaBox` as `[x0, y0, x1, y1]` f64,
/// honoring inheritance from ancestor `/Pages` nodes. Defaults to US
/// Letter `[0, 0, 612, 792]` when absent **or** malformed — callers that
/// need strictness should validate the page dictionary themselves.
///
/// `/UserUnit` is ignored (see module docs).
pub(crate) fn media_box(doc: &Document, page_id: ObjectId) -> [f64; 4] {
    let Some(obj) = find_inherited(doc, page_id, b"MediaBox") else {
        return [0.0, 0.0, 612.0, 792.0];
    };
    // Dereference one level if the box itself is an indirect array.
    let resolved;
    let array_obj = match &obj {
        Object::Reference(id) => match doc.get_object(*id) {
            Ok(o) => {
                resolved = o.clone();
                &resolved
            }
            Err(_) => return [0.0, 0.0, 612.0, 792.0],
        },
        other => other,
    };
    let Ok(array) = array_obj.as_array() else {
        return [0.0, 0.0, 612.0, 792.0];
    };
    if array.len() < 4 {
        return [0.0, 0.0, 612.0, 792.0];
    }
    let mut out = [0.0f64; 4];
    for (slot, item) in out.iter_mut().zip(array.iter()) {
        let num_obj = match item {
            Object::Reference(id) => match doc.get_object(*id) {
                Ok(o) => o,
                Err(_) => return [0.0, 0.0, 612.0, 792.0],
            },
            other => other,
        };
        match num_obj.as_float() {
            Ok(v) => *slot = f64::from(v),
            Err(_) => return [0.0, 0.0, 612.0, 792.0],
        }
    }
    out
}

/// Return the page's effective `/Resources` value as a cloned [`Object`]
/// — the entry may be a direct dictionary or a reference, and is looked
/// up through the `/Pages` ancestor chain. `None` when no resources
/// exist anywhere.
pub(crate) fn resources_object(doc: &Document, page_id: ObjectId) -> Option<Object> {
    find_inherited(doc, page_id, b"Resources")
}

/// Return the page's effective `/Resources` as an owned [`Dictionary`],
/// following one level of indirection. `None` when absent or not a
/// dictionary. Nested values (e.g. `/Font` sub-dictionaries) may still
/// be references into `doc`.
pub(crate) fn effective_resources_dict(doc: &Document, page_id: ObjectId) -> Option<Dictionary> {
    let obj = resources_object(doc, page_id)?;
    let resolved = match &obj {
        Object::Reference(id) => doc.get_object(*id).ok()?,
        other => other,
    };
    resolved.as_dict().ok().cloned()
}

/// Write `resources` back onto the page's `/Resources`.
///
/// If the page already has an indirect `/Resources` object it is updated
/// **in place** (preserving any other references to it); otherwise a new
/// indirect dictionary is created and referenced from the page.
pub(crate) fn set_page_resources(
    doc: &mut Document,
    page_id: ObjectId,
    resources: Dictionary,
) -> PdfResult<()> {
    let existing = doc
        .get_dictionary(page_id)
        .map_err(lopdf_err)?
        .get(b"Resources")
        .ok()
        .cloned();
    if let Some(Object::Reference(id)) = existing {
        *dict_at_mut(doc, id)? = resources;
        return Ok(());
    }
    let id = doc
        .try_add_object(Object::Dictionary(resources))
        .map_err(lopdf_err)?;
    doc.get_dictionary_mut(page_id)
        .map_err(lopdf_err)?
        .set("Resources", Object::Reference(id));
    Ok(())
}

/// Borrow the dictionary stored at `id` mutably.
fn dict_at_mut<'a>(doc: &'a mut Document, id: ObjectId) -> PdfResult<&'a mut Dictionary> {
    doc.get_object_mut(id)
        .map_err(lopdf_err)?
        .as_dict_mut()
        .map_err(lopdf_err)
}

/// First free resource name of the form `{prefix}{n}` (`n` starting at
/// 1) in `dict`.
fn fresh_resource_name(dict: &Dictionary, prefix: &str) -> Vec<u8> {
    for n in 1u32.. {
        let name = format!("{prefix}{n}");
        if !dict.has(name.as_bytes()) {
            return name.into_bytes();
        }
    }
    unreachable!()
}

/// Register `value` under a fresh `{prefix}{n}` name inside
/// `resources[category]`, creating the category dictionary when missing,
/// and return the chosen name (without a leading slash).
///
/// `resources` must be an **owned clone** (e.g. from
/// [`effective_resources_dict`]) that is disjoint from `doc` — when the
/// category entry is an indirect reference, the referenced dictionary is
/// mutated in place inside `doc`.
pub(crate) fn ensure_named_resource(
    doc: &mut Document,
    resources: &mut Dictionary,
    category: &[u8],
    prefix: &str,
    value: Object,
) -> PdfResult<Vec<u8>> {
    if resources.get(category).is_err() {
        let mut cat = Dictionary::new();
        let name = fresh_resource_name(&cat, prefix);
        cat.set(name.as_slice(), value);
        resources.set(category, Object::Dictionary(cat));
        return Ok(name);
    }
    match resources.get_mut(category) {
        Ok(Object::Dictionary(cat)) => {
            let name = fresh_resource_name(cat, prefix);
            cat.set(name.as_slice(), value);
            Ok(name)
        }
        Ok(Object::Reference(id)) => {
            let id = *id;
            let cat = dict_at_mut(doc, id)?;
            let name = fresh_resource_name(cat, prefix);
            cat.set(name.as_slice(), value);
            Ok(name)
        }
        Ok(_) => Err(PdfError::InvalidInput(format!(
            "/{} is not a dictionary",
            String::from_utf8_lossy(category)
        ))),
        Err(_) => unreachable!("checked above"),
    }
}

// ---------------------------------------------------------------------------
// Resource registration helpers
// ---------------------------------------------------------------------------

/// Register a base-14 Helvetica font on the page's effective
/// `/Resources` and return the resource name (without the leading
/// slash) to use in `Tf` operands.
///
/// The name is chosen to not collide with existing entries — a page
/// that already uses `F1` for Courier gets a fresh `KF1` rather than
/// having its font clobbered. One new font object is created per call.
pub(crate) fn ensure_font_resource(doc: &mut Document, page_id: ObjectId) -> PdfResult<String> {
    let font_id = doc
        .try_add_object(dictionary! {
            "Type" => "Font",
            "Subtype" => "Type1",
            "BaseFont" => "Helvetica",
            "Encoding" => "WinAnsiEncoding",
        })
        .map_err(lopdf_err)?;
    let mut resources = effective_resources_dict(doc, page_id).unwrap_or_default();
    let name = ensure_named_resource(
        doc,
        &mut resources,
        b"Font",
        "KF",
        Object::Reference(font_id),
    )?;
    set_page_resources(doc, page_id, resources)?;
    Ok(String::from_utf8_lossy(&name).into_owned())
}

/// Register an `/ExtGState` graphics-state resource on the page carrying
/// the given alpha values, and return its resource name (without the
/// leading slash). Both `/ca` (nonstroking — fills and text) and `/CA`
/// (stroking) are set to `opacity`, clamped to `0.0..=1.0`. Emit
/// `/NAME gs` in the content stream to activate it.
pub(crate) fn ensure_ext_gstate(
    doc: &mut Document,
    page_id: ObjectId,
    opacity: f32,
) -> PdfResult<String> {
    let opacity = opacity.clamp(0.0, 1.0);
    let gs_id = doc
        .try_add_object(dictionary! {
            "Type" => "ExtGState",
            "ca" => Object::Real(opacity),
            "CA" => Object::Real(opacity),
        })
        .map_err(lopdf_err)?;
    let mut resources = effective_resources_dict(doc, page_id).unwrap_or_default();
    let name = ensure_named_resource(
        doc,
        &mut resources,
        b"ExtGState",
        "GS",
        Object::Reference(gs_id),
    )?;
    set_page_resources(doc, page_id, resources)?;
    Ok(String::from_utf8_lossy(&name).into_owned())
}

/// Register an image XObject on the page's `/Resources` and return the
/// resource name (without the leading slash) to use with `Do`.
pub(crate) fn ensure_xobject_resource(
    doc: &mut Document,
    page_id: ObjectId,
    image_stream_id: ObjectId,
) -> PdfResult<String> {
    let mut resources = effective_resources_dict(doc, page_id).unwrap_or_default();
    let name = ensure_named_resource(
        doc,
        &mut resources,
        b"XObject",
        "Im",
        Object::Reference(image_stream_id),
    )?;
    set_page_resources(doc, page_id, resources)?;
    Ok(String::from_utf8_lossy(&name).into_owned())
}

// ---------------------------------------------------------------------------
// Content merging
// ---------------------------------------------------------------------------

/// Maximum decompressed size accepted from a single existing content
/// stream when merging page contents — a decompression-bomb guard.
const MAX_CONTENT_STREAM: usize = 64 * 1024 * 1024;

/// Decode a stream's content, bounded by [`MAX_CONTENT_STREAM`].
/// Streams without a `/Filter` fall back to their raw bytes; streams
/// whose filter fails to decode are an error (their raw bytes are still
/// compressed and would corrupt the merged content stream).
fn stream_content(stream: &Stream) -> PdfResult<Vec<u8>> {
    match stream.decompressed_content_with_limit(MAX_CONTENT_STREAM) {
        Ok(bytes) => Ok(bytes),
        Err(err) => {
            if stream.dict.has(b"Filter") {
                Err(lopdf_err(err))
            } else {
                Ok(stream.content.clone())
            }
        }
    }
}

/// Recursively collect the decoded bytes of every stream reachable from
/// a `/Contents` value (single stream, array of streams, or reference to
/// either — including references to arrays).
fn collect_content_object(
    doc: &Document,
    object: &Object,
    out: &mut Vec<u8>,
    depth: u32,
) -> PdfResult<()> {
    if depth > 16 {
        return Err(PdfError::InvalidInput(
            "circular /Contents reference chain".to_string(),
        ));
    }
    match object {
        Object::Stream(stream) => out.extend(stream_content(stream)?),
        Object::Array(items) => {
            for item in items {
                collect_content_object(doc, item, out, depth + 1)?;
            }
        }
        Object::Reference(id) => {
            let resolved = doc.get_object(*id).map_err(lopdf_err)?;
            collect_content_object(doc, resolved, out, depth + 1)?;
        }
        // Null, numbers, dictionaries, etc. contribute no operators.
        _ => {}
    }
    Ok(())
}

/// Decode all of a page's existing `/Contents` into raw operator bytes.
fn page_content_bytes(doc: &Document, page_id: ObjectId) -> PdfResult<Vec<u8>> {
    let page = doc.get_dictionary(page_id).map_err(lopdf_err)?;
    let contents = match page.get(b"Contents") {
        Ok(object) => object.clone(),
        Err(_) => return Ok(Vec::new()),
    };
    let mut out = Vec::new();
    collect_content_object(doc, &contents, &mut out, 0)?;
    Ok(out)
}

/// Merge `overlay_stream_id`'s operators with the page's existing
/// content into one new uncompressed stream and point `/Contents` at it.
/// `overlay_first` controls draw order: `true` draws the overlay
/// *under* existing content (watermarks), `false` draws it *on top*
/// (stamps).
fn merge_page_content(
    doc: &mut Document,
    page_id: ObjectId,
    overlay_stream_id: ObjectId,
    overlay_first: bool,
) -> PdfResult<()> {
    let overlay_bytes = {
        let object = doc.get_object(overlay_stream_id).map_err(lopdf_err)?;
        let stream = object.as_stream().map_err(|_| {
            PdfError::InvalidInput("overlay id does not reference a stream".to_string())
        })?;
        stream_content(stream)?
    };
    let existing = page_content_bytes(doc, page_id)?;

    let mut combined = Vec::with_capacity(overlay_bytes.len() + existing.len() + 2);
    if overlay_first {
        combined.extend_from_slice(&overlay_bytes);
        combined.push(b'\n');
        combined.extend_from_slice(&existing);
    } else {
        combined.extend_from_slice(&existing);
        if !existing.is_empty() {
            combined.push(b'\n');
        }
        combined.extend_from_slice(&overlay_bytes);
    }
    combined.push(b'\n');

    let new_id = add_content_stream(doc, combined)?;
    doc.get_dictionary_mut(page_id)
        .map_err(lopdf_err)?
        .set("Contents", Object::Reference(new_id));
    Ok(())
}

/// Prepend the overlay stream's operators to the page's content so the
/// mark is drawn **under** existing page content (the watermark case).
pub(crate) fn wrap_page_content(
    doc: &mut Document,
    page_id: ObjectId,
    overlay_stream_id: ObjectId,
) -> PdfResult<()> {
    merge_page_content(doc, page_id, overlay_stream_id, true)
}

/// Append the overlay stream's operators after the page's content so the
/// mark is drawn **on top** of existing content (the stamp case).
pub(crate) fn append_page_overlay(
    doc: &mut Document,
    page_id: ObjectId,
    overlay_stream_id: ObjectId,
) -> PdfResult<()> {
    merge_page_content(doc, page_id, overlay_stream_id, false)
}

// ---------------------------------------------------------------------------
// watermark operation
// ---------------------------------------------------------------------------

/// Options for [`watermark`].
#[derive(Debug, Clone)]
pub struct WatermarkOptions {
    /// Watermark text. Must be non-empty after trimming; the trimmed
    /// text is what gets drawn.
    pub text: String,
    /// Font size in points. Must be in `(0, 1000]`. Default 48.
    pub font_size: f32,
    /// Opacity in `0.0..=1.0` applied via an ExtGState resource.
    /// Default 0.3.
    pub opacity: f32,
    /// Counter-clockwise rotation in degrees around the mark's center.
    /// Default -45 (the classic diagonal watermark).
    pub rotation_degrees: f32,
    /// Where on the page the watermark is anchored. Default `Center`.
    pub position: MarkPosition,
    /// Page range spec (e.g. `"1-3,8"` or `"all"`, 1-based). `None`
    /// watermarks every page.
    pub pages: Option<String>,
    /// RGB color, each component 0-255. `None` uses light gray
    /// `[200, 200, 200]` — the conventional watermark tint.
    pub color: Option<[u8; 3]>,
}

impl Default for WatermarkOptions {
    fn default() -> Self {
        Self {
            text: String::new(),
            font_size: 48.0,
            opacity: 0.3,
            rotation_degrees: -45.0,
            position: MarkPosition::Center,
            pages: None,
            color: None,
        }
    }
}

/// Distance kept between the watermark's bounding box and the page edge
/// for edge/corner positions. Tied to the font size so large watermarks
/// don't kiss the page edge.
fn watermark_margin(font_size: f32) -> f32 {
    font_size.max(1.0)
}

/// Apply a text watermark to `input` and return the new PDF bytes.
///
/// The watermark is drawn **under** the page's existing content
/// (prepended to the content stream), rotated around the center of its
/// bounding box via a `cm` matrix, and given real transparency through
/// an `/ExtGState` `/ca`+`/CA` resource.
///
/// Validation (empty text, out-of-range `font_size`/`opacity`) happens
/// before the document is mutated; the page range is validated against
/// the actual page count. Returns [`PdfError::Encrypted`] for encrypted
/// input.
///
/// # Approximations
///
/// - Text metrics are estimated (`estimate_text_width`, box height =
///   `font_size`), so "centered" marks are centered to within roughly a
///   character width, not typographically exact.
/// - The font is base-14 Helvetica via WinAnsiEncoding. Characters above
///   U+00FF are emitted as UTF-16BE hex strings, but base-14 fonts only
///   cover WinAnsi glyphs — exotic characters may render as `.notdef`
///   boxes in strict viewers.
/// - `/UserUnit` is ignored (see module docs).
pub fn watermark(
    input: &[u8],
    opts: &WatermarkOptions,
    limits: &EngineLimits,
) -> PdfResult<Vec<u8>> {
    let text = opts.text.trim();
    if text.is_empty() {
        return Err(PdfError::InvalidInput(
            "watermark text must not be empty".to_string(),
        ));
    }
    if !opts.font_size.is_finite() || opts.font_size <= 0.0 || opts.font_size > 1000.0 {
        return Err(PdfError::InvalidInput(format!(
            "watermark font_size must be in (0, 1000], got {}",
            opts.font_size
        )));
    }
    if !opts.opacity.is_finite() || !(0.0..=1.0).contains(&opts.opacity) {
        return Err(PdfError::InvalidInput(format!(
            "watermark opacity must be in [0, 1], got {}",
            opts.opacity
        )));
    }
    if !opts.rotation_degrees.is_finite() {
        return Err(PdfError::InvalidInput(
            "watermark rotation_degrees must be finite".to_string(),
        ));
    }

    let mut doc = load_pdf(input, limits)?;
    reject_encrypted(&doc)?;

    let pages_map = doc.get_pages();
    let selected = selected_pages(&doc, opts.pages.as_deref())?;

    let color = opts.color.unwrap_or([200, 200, 200]);
    let (r, g, b) = (
        f32::from(color[0]) / 255.0,
        f32::from(color[1]) / 255.0,
        f32::from(color[2]) / 255.0,
    );
    // Estimated mark box: advance-width estimate wide, font-size tall.
    let mark_w = estimate_text_width(text, opts.font_size);
    let mark_h = opts.font_size;
    let margin = watermark_margin(opts.font_size);

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

        let anchor = position_point(opts.position, page_w, page_h, margin);
        let (bx, by) = box_origin(opts.position, anchor, mark_w, mark_h);
        // Absolute center of the mark box — the rotation pivot.
        let cx = x0 + bx + mark_w * 0.5;
        let cy = y0 + by + mark_h * 0.5;

        let font_name = ensure_font_resource(&mut doc, page_id)?;
        let gs_name = ensure_ext_gstate(&mut doc, page_id, opts.opacity)?;

        let (sin, cos) = opts.rotation_degrees.to_radians().sin_cos();
        let operations = vec![
            Operation::new("q", vec![]),
            Operation::new(
                "rg",
                vec![Object::Real(r), Object::Real(g), Object::Real(b)],
            ),
            Operation::new("gs", vec![Object::Name(gs_name.clone().into_bytes())]),
            // Rotate about the mark center, then translate there.
            Operation::new(
                "cm",
                vec![
                    Object::Real(cos),
                    Object::Real(sin),
                    Object::Real(-sin),
                    Object::Real(cos),
                    Object::Real(cx),
                    Object::Real(cy),
                ],
            ),
            Operation::new("BT", vec![]),
            Operation::new(
                "Tf",
                vec![
                    Object::Name(font_name.clone().into_bytes()),
                    Object::Real(opts.font_size),
                ],
            ),
            // Draw the text so its estimated box is centered on the pivot.
            Operation::new(
                "Td",
                vec![Object::Real(-mark_w * 0.5), Object::Real(-mark_h * 0.5)],
            ),
            Operation::new("Tj", vec![pdf_text_operand(text)]),
            Operation::new("ET", vec![]),
            Operation::new("Q", vec![]),
        ];

        let stream_id = add_content_stream(&mut doc, encode_operations(operations)?)?;
        // Watermarks go under existing content.
        wrap_page_content(&mut doc, page_id, stream_id)?;
    }

    // Recompress everything (including the merged content streams) to
    // keep output size sane.
    doc.compress();
    save_pdf(&mut doc)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::crates::pdf_engine::ops::test_utils::{page_count, pdf_with_pages};
    use lopdf::Document as LopdfDocument;

    fn opts() -> WatermarkOptions {
        WatermarkOptions {
            text: "DRAFT".to_string(),
            ..Default::default()
        }
    }

    #[test]
    fn position_point_places_anchors() {
        // 600x800 page, margin 10.
        assert_eq!(
            position_point(MarkPosition::Center, 600.0, 800.0, 10.0),
            (300.0, 400.0)
        );
        assert_eq!(
            position_point(MarkPosition::TopLeft, 600.0, 800.0, 10.0),
            (10.0, 790.0)
        );
        assert_eq!(
            position_point(MarkPosition::BottomRight, 600.0, 800.0, 10.0),
            (590.0, 10.0)
        );
        assert_eq!(
            position_point(MarkPosition::TopCenter, 600.0, 800.0, 10.0),
            (300.0, 790.0)
        );
        assert_eq!(
            position_point(MarkPosition::BottomCenter, 600.0, 800.0, 10.0),
            (300.0, 10.0)
        );
    }

    #[test]
    fn box_origin_converts_anchor_to_lower_left() {
        // Anchor = top-left corner of a 100x50 box near the page top-left.
        let anchor = position_point(MarkPosition::TopLeft, 600.0, 800.0, 10.0);
        let (x, y) = box_origin(MarkPosition::TopLeft, anchor, 100.0, 50.0);
        assert_eq!((x, y), (10.0, 740.0));
        // Center anchor → box centered on page center.
        let anchor = position_point(MarkPosition::Center, 600.0, 800.0, 0.0);
        let (x, y) = box_origin(MarkPosition::Center, anchor, 100.0, 50.0);
        assert_eq!((x, y), (250.0, 375.0));
        // Bottom-right anchor is the box's bottom-right corner.
        let anchor = position_point(MarkPosition::BottomRight, 600.0, 800.0, 10.0);
        let (x, y) = box_origin(MarkPosition::BottomRight, anchor, 100.0, 50.0);
        assert_eq!((x, y), (490.0, 10.0));
    }

    #[test]
    fn text_operand_escapes_and_switches_to_utf16() {
        match pdf_text_operand("a (b) \\ c") {
            Object::String(bytes, StringFormat::Literal) => {
                // Raw bytes — the content-stream writer escapes ()\.
                assert_eq!(bytes, b"a (b) \\ c");
            }
            other => panic!("expected literal string, got {other:?}"),
        }
        match pdf_text_operand("héllo") {
            Object::String(bytes, StringFormat::Literal) => {
                // All chars <= U+00FF stay literal; é is raw byte 0xE9.
                assert!(bytes.contains(&0xE9));
            }
            other => panic!("expected literal string, got {other:?}"),
        }
        match pdf_text_operand("テキスト") {
            Object::String(bytes, StringFormat::Hexadecimal) => {
                assert_eq!(&bytes[..2], &[0xFE, 0xFF]);
                // 4 BMP chars → 4 UTF-16 units → 8 bytes + 2 BOM bytes.
                assert_eq!(bytes.len(), 2 + 4 * 2);
            }
            other => panic!("expected hex string, got {other:?}"),
        }
    }

    #[test]
    fn media_box_reads_mediabox_and_defaults() {
        let pdf = pdf_with_pages(&["x"]);
        let doc = LopdfDocument::load_mem(&pdf).unwrap();
        let page_id = doc.get_pages()[&1];
        // test_utils uses MediaBox [0 0 595 842].
        assert_eq!(media_box(&doc, page_id), [0.0, 0.0, 595.0, 842.0]);

        // A page with no MediaBox anywhere defaults to US Letter.
        let mut bare = Document::with_version("1.5");
        let pages_id = bare.new_object_id();
        let content_id = bare.add_object(Stream::new(Dictionary::new(), b"q\nQ\n".to_vec()));
        let page_id = bare.add_object(dictionary! {
            "Type" => "Page",
            "Parent" => pages_id,
            "Contents" => content_id,
        });
        bare.objects.insert(
            pages_id,
            Object::Dictionary(dictionary! {
                "Type" => "Pages",
                "Kids" => vec![Object::Reference(page_id)],
                "Count" => 1,
            }),
        );
        let catalog_id = bare.add_object(dictionary! {
            "Type" => "Catalog",
            "Pages" => pages_id,
        });
        bare.trailer.set("Root", catalog_id);
        let mut buf = Vec::new();
        bare.save_to(&mut buf).unwrap();
        let doc = LopdfDocument::load_mem(&buf).unwrap();
        let pages = doc.get_pages();
        assert_eq!(media_box(&doc, pages[&1]), [0.0, 0.0, 612.0, 792.0]);
    }

    #[test]
    fn font_resource_is_registered_without_clobbering() {
        let pdf = pdf_with_pages(&["x"]);
        let mut doc = LopdfDocument::load_mem(&pdf).unwrap();
        let page_id = doc.get_pages()[&1];
        // test_utils already defines F1 → Courier; we must not clobber it.
        let name1 = ensure_font_resource(&mut doc, page_id).unwrap();
        assert_ne!(name1, "F1");
        let name2 = ensure_font_resource(&mut doc, page_id).unwrap();
        assert_ne!(name1, name2);

        let resources = effective_resources_dict(&doc, page_id).unwrap();
        let font_obj = resources.get(b"Font").unwrap().clone();
        let font_cat = match &font_obj {
            Object::Dictionary(d) => d.clone(),
            Object::Reference(id) => doc
                .get_object(*id)
                .and_then(|o| o.as_dict().cloned())
                .unwrap(),
            other => panic!("Font should be a dictionary, got {other:?}"),
        };
        assert!(font_cat.has(name1.as_bytes()));
        assert!(font_cat.has(name2.as_bytes()));
        // Original F1 → Courier must still be there.
        assert!(font_cat.has(b"F1"));
    }

    #[test]
    fn ext_gstate_is_registered() {
        let pdf = pdf_with_pages(&["x"]);
        let mut doc = LopdfDocument::load_mem(&pdf).unwrap();
        let page_id = doc.get_pages()[&1];
        let name = ensure_ext_gstate(&mut doc, page_id, 0.4).unwrap();
        assert!(name.starts_with("GS"));
        let resources = effective_resources_dict(&doc, page_id).unwrap();
        assert!(resources.get(b"ExtGState").is_ok());
    }

    #[test]
    fn selected_pages_covers_all_and_ranges() {
        let pdf = pdf_with_pages(&["a", "b", "c"]);
        let doc = LopdfDocument::load_mem(&pdf).unwrap();
        assert_eq!(selected_pages(&doc, None).unwrap(), vec![1, 2, 3]);
        assert_eq!(selected_pages(&doc, Some("2")).unwrap(), vec![2]);
        assert_eq!(selected_pages(&doc, Some("all")).unwrap(), vec![1, 2, 3]);
        assert!(selected_pages(&doc, Some("9")).is_err());
    }

    #[test]
    fn wrap_and_append_merge_contents() {
        let pdf = pdf_with_pages(&["hello"]);
        let mut doc = LopdfDocument::load_mem(&pdf).unwrap();
        let page_id = doc.get_pages()[&1];
        // Original content contains "(hello) Tj".
        let overlay = add_content_stream(&mut doc, b"q\n1 0 0 rg\nQ\n".to_vec()).unwrap();
        wrap_page_content(&mut doc, page_id, overlay).unwrap();
        let content = doc.get_page_content(page_id);
        let text = String::from_utf8_lossy(&content);
        // Overlay operators come first (underneath), original after.
        let overlay_pos = text.find("1 0 0 rg").expect("overlay ops present");
        let orig_pos = text.find("(hello)").expect("original content present");
        assert!(
            overlay_pos < orig_pos,
            "overlay should precede page content"
        );

        // Appending puts the stamp on top.
        let overlay2 = add_content_stream(&mut doc, b"0 0 1 rg\n".to_vec()).unwrap();
        append_page_overlay(&mut doc, page_id, overlay2).unwrap();
        let content = doc.get_page_content(page_id);
        let text = String::from_utf8_lossy(&content);
        assert!(text.find("0 0 1 rg").unwrap() > text.find("(hello)").unwrap());
    }

    #[test]
    fn watermarks_all_pages_and_stays_valid() {
        let pdf = pdf_with_pages(&["first page", "second page", "third page"]);
        let out = watermark(&pdf, &opts(), &EngineLimits::default()).unwrap();
        assert_eq!(page_count(&out), 3);
        // Output must re-parse cleanly.
        let doc = LopdfDocument::load_mem(&out).unwrap();
        assert_eq!(doc.get_pages().len(), 3);
    }

    #[test]
    fn respects_page_range() {
        let pdf = pdf_with_pages(&["one", "two", "three"]);
        let o = WatermarkOptions {
            pages: Some("2".to_string()),
            ..opts()
        };
        let out = watermark(&pdf, &o, &EngineLimits::default()).unwrap();
        assert_eq!(page_count(&out), 3);
    }

    #[test]
    fn rejects_bad_page_range() {
        let pdf = pdf_with_pages(&["one"]);
        let o = WatermarkOptions {
            pages: Some("9".to_string()),
            ..opts()
        };
        assert!(watermark(&pdf, &o, &EngineLimits::default()).is_err());
    }

    #[test]
    fn rejects_bad_options() {
        let pdf = pdf_with_pages(&["x"]);
        for bad in [
            WatermarkOptions {
                text: "   ".to_string(),
                ..opts()
            },
            WatermarkOptions {
                font_size: 0.0,
                ..opts()
            },
            WatermarkOptions {
                font_size: 1001.0,
                ..opts()
            },
            WatermarkOptions {
                font_size: f32::NAN,
                ..opts()
            },
            WatermarkOptions {
                opacity: -0.1,
                ..opts()
            },
            WatermarkOptions {
                opacity: 1.5,
                ..opts()
            },
            WatermarkOptions {
                rotation_degrees: f32::INFINITY,
                ..opts()
            },
        ] {
            assert!(
                watermark(&pdf, &bad, &EngineLimits::default()).is_err(),
                "options {bad:?} should be rejected"
            );
        }
    }

    #[test]
    fn escapes_and_encodes_text() {
        let pdf = pdf_with_pages(&["body"]);
        // Parens, backslash, control char, and non-Latin-1 (forces UTF-16BE hex).
        for text in [
            "plain".to_string(),
            "a (b) \\ c".to_string(),
            "line\nbreak".to_string(),
            "héllo wörld".to_string(),
            "テキスト".to_string(),
        ] {
            let o = WatermarkOptions { text, ..opts() };
            let out = watermark(&pdf, &o, &EngineLimits::default()).unwrap();
            assert_eq!(page_count(&out), 1);
        }
    }

    #[test]
    fn all_positions_and_rotations_do_not_panic() {
        let pdf = pdf_with_pages(&["p"]);
        for position in [
            MarkPosition::Center,
            MarkPosition::TopLeft,
            MarkPosition::TopRight,
            MarkPosition::BottomLeft,
            MarkPosition::BottomRight,
            MarkPosition::TopCenter,
            MarkPosition::BottomCenter,
        ] {
            for rotation in [0.0, 45.0, -45.0, 90.0, 720.0] {
                let o = WatermarkOptions {
                    position,
                    rotation_degrees: rotation,
                    opacity: 0.0,
                    ..opts()
                };
                let out = watermark(&pdf, &o, &EngineLimits::default()).unwrap();
                assert_eq!(page_count(&out), 1);
            }
        }
    }

    #[test]
    fn custom_color_and_opacity() {
        let pdf = pdf_with_pages(&["x"]);
        let o = WatermarkOptions {
            color: Some([255, 0, 0]),
            opacity: 1.0,
            font_size: 72.0,
            ..opts()
        };
        let out = watermark(&pdf, &o, &EngineLimits::default()).unwrap();
        assert_eq!(page_count(&out), 1);
    }

    #[test]
    fn rejects_garbage_input() {
        assert!(watermark(b"not a pdf", &opts(), &EngineLimits::default()).is_err());
    }
}
