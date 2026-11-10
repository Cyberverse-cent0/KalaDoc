//! Add crop marks and registration marks for print production.
//!
//! Crop marks are short perpendicular line pairs drawn just outside each
//! corner of the page's effective **trim box** (its `/CropBox`, falling
//! back to `/MediaBox`, both resolved through the `/Pages` ancestor
//! chain). Registration marks are a stroked circle plus a crosshair at
//! the trim box center.
//!
//! The marks are appended **on top of** each selected page's content as
//! a new content stream (the [`super::stamp`] approach), using only
//! path construction and stroking operators (`m`, `l`, `c`, `S`, `w`,
//! `RG`) — no fonts, XObjects or ExtGState resources are needed.
//!
//! Note that viewers clip page content to the `/CropBox`, so marks
//! drawn outside the trim box are typically only visible to prepress
//! tools that render beyond the crop area; that is the intended use.

use lopdf::Object;
use lopdf::content::Operation;

use crate::crates::core::engine::EngineLimits;

use crate::crates::pdf_engine::error::{PdfError, PdfResult};

use super::crop::{DEFAULT_MEDIA_BOX, inherited_rect};
use super::load::{load_pdf, reject_encrypted, save_pdf};
use super::watermark::{add_content_stream, append_page_overlay, encode_operations};

/// Default mark offset from the trim box edge, in points
/// (9 pt ≈ 3 mm — the conventional crop-mark gap).
pub const DEFAULT_PRINT_MARK_OFFSET: f64 = 9.0;

/// Maximum allowed mark offset in points (72 pt = 1 inch).
const MAX_PRINT_MARK_OFFSET: f64 = 72.0;

/// Stroke width for all print marks, in points.
const MARK_LINE_WIDTH: f64 = 0.5;

/// Options for [`add_print_marks`].
#[derive(Debug, Clone)]
pub struct PrintMarksSpec<'a> {
    /// Draw crop-mark line pairs at the four trim-box corners.
    pub crop_marks: bool,
    /// Draw a registration mark (circle + crosshair) at the trim-box
    /// center.
    pub registration: bool,
    /// Distance in points from the trim-box edge to the marks.
    /// Must be finite and in `0.0..=72`. Default
    /// [`DEFAULT_PRINT_MARK_OFFSET`] (9 pt).
    pub offset: f64,
    /// Stroke color as `[r, g, b]` with 0-255 components.
    /// Default `[0, 0, 0]` (black — registration/crop marks are almost
    /// always black regardless of document color).
    pub color: [u8; 3],
    /// Page range spec (e.g. `"1-3,8"` or `"all"`, 1-based).
    /// `None` marks every page.
    pub pages: Option<&'a str>,
}

impl Default for PrintMarksSpec<'_> {
    fn default() -> Self {
        Self {
            crop_marks: true,
            registration: false,
            offset: DEFAULT_PRINT_MARK_OFFSET,
            color: [0, 0, 0],
            pages: None,
        }
    }
}

fn real(v: f64) -> Object {
    Object::Real(v as f32)
}

fn move_to(x: f64, y: f64) -> Operation {
    Operation::new("m", vec![real(x), real(y)])
}

fn line_to(x: f64, y: f64) -> Operation {
    Operation::new("l", vec![real(x), real(y)])
}

fn stroke() -> Operation {
    Operation::new("S", vec![])
}

/// Append the mark geometry for one page to `ops`.
fn page_mark_operations(ops: &mut Vec<Operation>, trim: [f64; 4], spec: &PrintMarksSpec) {
    let [x0, y0, x1, y1] = trim;
    let off = spec.offset;

    if spec.crop_marks {
        // At each corner draw an L-shaped pair of short segments sitting
        // in the quadrant diagonally outside the corner: one segment
        // parallel to each adjoining edge, `offset` away from the box.
        // (sx, sy) point away from the box at that corner.
        for (cx, cy, sx, sy) in [
            (x0, y0, -1.0, -1.0),
            (x1, y0, 1.0, -1.0),
            (x0, y1, -1.0, 1.0),
            (x1, y1, 1.0, 1.0),
        ] {
            // Segment parallel to the vertical edge.
            ops.push(move_to(cx + sx * off, cy));
            ops.push(line_to(cx + sx * off, cy + sy * off));
            ops.push(stroke());
            // Segment parallel to the horizontal edge.
            ops.push(move_to(cx, cy + sy * off));
            ops.push(line_to(cx + sx * off, cy + sy * off));
            ops.push(stroke());
        }
    }

    if spec.registration {
        let cx = (x0 + x1) / 2.0;
        let cy = (y0 + y1) / 2.0;
        let min_dim = (x1 - x0).min(y1 - y0);
        // Radius stays well inside the trim box even for small pages.
        let r = (min_dim * 0.05).clamp(2.0, 30.0).min(min_dim * 0.45);

        // Circle approximated by four cubic Bézier segments
        // (κ ≈ 0.5523 is the standard circle-fit constant).
        let k = r * 0.5522847498;
        ops.push(move_to(cx + r, cy));
        ops.push(Operation::new(
            "c",
            vec![
                real(cx + r),
                real(cy + k),
                real(cx + k),
                real(cy + r),
                real(cx),
                real(cy + r),
            ],
        ));
        ops.push(Operation::new(
            "c",
            vec![
                real(cx - k),
                real(cy + r),
                real(cx - r),
                real(cy + k),
                real(cx - r),
                real(cy),
            ],
        ));
        ops.push(Operation::new(
            "c",
            vec![
                real(cx - r),
                real(cy - k),
                real(cx - k),
                real(cy - r),
                real(cx),
                real(cy - r),
            ],
        ));
        ops.push(Operation::new(
            "c",
            vec![
                real(cx + k),
                real(cy - r),
                real(cx + r),
                real(cy - k),
                real(cx + r),
                real(cy),
            ],
        ));
        ops.push(stroke());

        // Crosshair through the center; arms reach ~1.6× the circle
        // radius so the two marks read as one registration target.
        let arm = r * 1.6;
        ops.push(move_to(cx - arm, cy));
        ops.push(line_to(cx + arm, cy));
        ops.push(move_to(cx, cy - arm));
        ops.push(line_to(cx, cy + arm));
        ops.push(stroke());
    }
}

/// Add crop and/or registration marks to the pages selected by
/// `spec.pages` and return the new PDF bytes.
///
/// The trim box used for placement is each page's effective `/CropBox`
/// (inherited through the `/Pages` tree), falling back to the effective
/// `/MediaBox` and finally to US Letter. Marks are stroked in
/// `spec.color` at [`MARK_LINE_WIDTH`] and appended after the page's
/// existing content.
///
/// Fails with [`PdfError::InvalidInput`] when neither mark kind is
/// enabled, `offset` is outside `0.0..=72`, or a target page has a
/// malformed trim box; with [`PdfError::InvalidPageRange`] for a bad
/// page spec; and with [`PdfError::Encrypted`] for encrypted input.
pub fn add_print_marks(
    input: &[u8],
    spec: &PrintMarksSpec,
    limits: &EngineLimits,
) -> PdfResult<Vec<u8>> {
    if !spec.crop_marks && !spec.registration {
        return Err(PdfError::InvalidInput(
            "print marks require at least one of crop_marks or registration".to_string(),
        ));
    }
    if !spec.offset.is_finite() || !(0.0..=MAX_PRINT_MARK_OFFSET).contains(&spec.offset) {
        return Err(PdfError::InvalidInput(format!(
            "print mark offset must be finite and in [0, {MAX_PRINT_MARK_OFFSET}] pt, got {}",
            spec.offset
        )));
    }

    let mut doc = load_pdf(input, limits)?;
    reject_encrypted(&doc)?;

    let pages_map = doc.get_pages();
    let page_count = pages_map.len() as u32;
    let selected: Vec<u32> = match spec.pages {
        None => (1..=page_count).collect(),
        Some(page_spec) => super::pages::parse_page_range(page_spec, page_count)?,
    };

    for page_number in selected {
        let Some(&page_id) = pages_map.get(&page_number) else {
            return Err(PdfError::InvalidPageRange(format!(
                "page {page_number} not found in document"
            )));
        };

        // Effective trim box: CropBox, else MediaBox, else US Letter.
        let trim = match inherited_rect(&doc, page_id, b"CropBox")? {
            Some(rect) => Some(rect),
            None => inherited_rect(&doc, page_id, b"MediaBox")?,
        }
        .unwrap_or(DEFAULT_MEDIA_BOX);
        let [x0, y0, x1, y1] = trim;
        if ![x0, y0, x1, y1].iter().all(|v| v.is_finite()) || x1 <= x0 || y1 <= y0 {
            return Err(PdfError::InvalidInput(format!(
                "page {page_number} has a malformed trim box {trim:?}"
            )));
        }

        let mut ops = vec![
            Operation::new("q", vec![]),
            Operation::new("w", vec![real(MARK_LINE_WIDTH)]),
            Operation::new(
                "RG",
                spec.color
                    .iter()
                    .map(|&c| real(f64::from(c) / 255.0))
                    .collect(),
            ),
        ];
        page_mark_operations(&mut ops, trim, spec);
        ops.push(Operation::new("Q", vec![]));

        let stream_id = add_content_stream(&mut doc, encode_operations(ops)?)?;
        // Print marks belong on top of whatever the page already draws.
        append_page_overlay(&mut doc, page_id, stream_id)?;
    }

    doc.compress();
    save_pdf(&mut doc)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::crates::pdf_engine::ops::test_utils::{page_count, pdf_with_pages};
    use lopdf::content::Content;
    use lopdf::{Document, Object};

    /// All decoded content-stream operator names on `page`, in order.
    fn page_ops(pdf: &[u8], page: u32) -> Vec<String> {
        let doc = Document::load_mem(pdf).unwrap();
        let page_id = doc.get_pages()[&page];
        let dict = doc.get_dictionary(page_id).unwrap();
        let contents = dict.get(b"Contents").unwrap();
        let ids: Vec<lopdf::ObjectId> = match contents {
            Object::Reference(id) => vec![*id],
            Object::Array(arr) => arr.iter().map(|o| o.as_reference().unwrap()).collect(),
            _ => panic!("unexpected Contents"),
        };
        let mut names = Vec::new();
        for id in ids {
            let stream = doc.get_object(id).unwrap().as_stream().unwrap();
            let bytes = stream.decompressed_content().unwrap();
            let content = Content::decode(&bytes).unwrap();
            names.extend(content.operations.iter().map(|op| op.operator.clone()));
        }
        names
    }

    #[test]
    fn crop_marks_emit_path_ops() {
        let pdf = pdf_with_pages(&["one", "two"]);
        let out =
            add_print_marks(&pdf, &PrintMarksSpec::default(), &EngineLimits::default()).unwrap();
        assert_eq!(page_count(&out), 2);
        let ops = page_ops(&out, 1);
        // 8 crop-mark segments → at least 8 `m`, 8 `l`, 8 `S`.
        for op in ["m", "l", "S", "w", "RG", "q", "Q"] {
            assert!(ops.iter().any(|n| n == op), "missing op {op} in {ops:?}");
        }
        assert!(
            ops.iter().filter(|n| *n == "S").count() >= 8,
            "expected >=8 stroked paths, got {ops:?}"
        );
        // Page 2 also marked (default pages = all).
        assert!(page_ops(&out, 2).iter().any(|n| n == "S"));
    }

    #[test]
    fn registration_emits_curves_and_cross() {
        let pdf = pdf_with_pages(&["one"]);
        let spec = PrintMarksSpec {
            crop_marks: false,
            registration: true,
            ..Default::default()
        };
        let out = add_print_marks(&pdf, &spec, &EngineLimits::default()).unwrap();
        let ops = page_ops(&out, 1);
        assert!(ops.iter().any(|n| n == "c"), "circle curves: {ops:?}");
        assert!(ops.iter().any(|n| n == "S"));
    }

    #[test]
    fn honors_page_range() {
        let pdf = pdf_with_pages(&["one", "two", "three"]);
        let spec = PrintMarksSpec {
            pages: Some("2"),
            ..Default::default()
        };
        let out = add_print_marks(&pdf, &spec, &EngineLimits::default()).unwrap();
        assert!(page_ops(&out, 2).iter().any(|n| n == "S"));
        // Pages 1 and 3 keep their original content only (no `S`).
        for p in [1u32, 3] {
            assert!(!page_ops(&out, p).iter().any(|n| n == "S"), "page {p}");
        }
    }

    #[test]
    fn rejects_bad_specs() {
        let pdf = pdf_with_pages(&["one"]);
        // Neither mark kind enabled.
        let none = PrintMarksSpec {
            crop_marks: false,
            registration: false,
            ..Default::default()
        };
        assert!(add_print_marks(&pdf, &none, &EngineLimits::default()).is_err());
        // Offset out of range / non-finite.
        for off in [-1.0, 72.5, f64::NAN, f64::INFINITY] {
            let spec = PrintMarksSpec {
                offset: off,
                ..Default::default()
            };
            assert!(
                add_print_marks(&pdf, &spec, &EngineLimits::default()).is_err(),
                "offset {off} should fail"
            );
        }
        // Boundary offset is accepted.
        let ok = PrintMarksSpec {
            offset: 72.0,
            ..Default::default()
        };
        assert!(add_print_marks(&pdf, &ok, &EngineLimits::default()).is_ok());
        // Bad page range / garbage input.
        let bad_pages = PrintMarksSpec {
            pages: Some("9"),
            ..Default::default()
        };
        assert!(add_print_marks(&pdf, &bad_pages, &EngineLimits::default()).is_err());
        assert!(
            add_print_marks(
                b"junk",
                &PrintMarksSpec::default(),
                &EngineLimits::default()
            )
            .is_err()
        );
    }

    #[test]
    fn rejects_encrypted_input() {
        use crate::crates::pdf_engine::ops::encrypt::{PdfPermissions, encrypt};
        let pdf = pdf_with_pages(&["one"]);
        let enc = encrypt(
            &pdf,
            "user",
            None,
            PdfPermissions::default(),
            &EngineLimits::default(),
        )
        .unwrap();
        assert!(matches!(
            add_print_marks(&enc, &PrintMarksSpec::default(), &EngineLimits::default()),
            Err(PdfError::Encrypted)
        ));
    }
}
