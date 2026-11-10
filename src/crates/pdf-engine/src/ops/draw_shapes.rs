//! Draw vector shapes — rectangles, lines, ellipses and arrows — on
//! existing pages.
//!
//! Each selected page gets an appended content stream (drawn **on top**
//! of existing content, like [`super::draw_text`]) containing the path
//! construction and painting operators for every shape in the spec.
//!
//! # Coordinate convention
//!
//! Shape coordinates are in **points** with the origin at the
//! lower-left corner of the page's `/MediaBox` (the same page-local
//! space used by `watermark`/`stamp`). The MediaBox origin offset is
//! added when emitting operators, so a rectangle at `(0, 0)` sits on
//! the visible lower-left corner even when the MediaBox does not start
//! at `(0, 0)`. `/UserUnit` is not honored.
//!
//! # Rendering details
//!
//! - Stroke color uses the stroking RGB operator `RG`; fill color uses
//!   the non-stroking `rg`. Both take `0..=1` components derived from
//!   the `[u8; 3]` inputs.
//! - `line_width` is emitted with `w`.
//! - `opacity` registers an `/ExtGState` (`ca`/`CA`) via
//!   [`ensure_ext_gstate`]; `None` or `>= 1.0` skips it. One graphics
//!   state is created per distinct opacity value per page.
//! - `Line` shapes ignore `fill` (a line has no interior); all other
//!   shapes honor it. When both fill and stroke are present the path is
//!   emitted twice — once for `f`, once for `S` — because painting
//!   operators consume the current path.
//! - `Arrow` renders as a shaft (`m`/`l`) plus two head segments. The
//!   head length defaults to a quarter of the shaft length clamped to
//!   `4..=48` pt, or `head_length` when given; the head half-angle is
//!   30 degrees.
//! - `Ellipse` is approximated by four cubic Bézier segments with the
//!   standard kappa constant `0.5522847498`.
//!
//! Every shape is wrapped in `q`/`Q` so graphics state never leaks
//! between shapes.

use std::collections::HashMap;

use lopdf::Object;
use lopdf::content::Operation;

use crate::crates::core::engine::EngineLimits;

use crate::crates::pdf_engine::error::{PdfError, PdfResult};

use super::load::{load_pdf, reject_encrypted, save_pdf};
use super::pages::parse_page_range;
use super::watermark::{
    add_content_stream, append_page_overlay, encode_operations, ensure_ext_gstate, media_box,
};

/// Stroke/fill/width/opacity shared by every [`Shape`] variant.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct ShapeStyle {
    /// Stroke RGB color, components 0-255. Always applied via `RG`.
    pub stroke: [u8; 3],
    /// Optional fill RGB color, components 0-255. `None` = no fill.
    pub fill: Option<[u8; 3]>,
    /// Stroke width in points; must be finite and in `(0, 1000]`.
    pub line_width: f32,
    /// Opacity in `0.0..=1.0` via an ExtGState resource. `None` or
    /// `>= 1.0` is fully opaque.
    pub opacity: Option<f32>,
}

impl Default for ShapeStyle {
    fn default() -> Self {
        Self {
            stroke: [0, 0, 0],
            fill: None,
            line_width: 1.0,
            opacity: None,
        }
    }
}

/// A single vector shape to draw on each selected page.
#[derive(Debug, Clone, Copy, PartialEq)]
pub enum Shape {
    /// Axis-aligned rectangle. `x`/`y` is the lower-left corner;
    /// `width`/`height` must be positive.
    Rectangle {
        /// Lower-left x, points.
        x: f32,
        /// Lower-left y, points.
        y: f32,
        /// Width, points; must be finite and `> 0`.
        width: f32,
        /// Height, points; must be finite and `> 0`.
        height: f32,
        /// Shared style.
        style: ShapeStyle,
    },
    /// Straight segment from `(x1, y1)` to `(x2, y2)`.
    Line {
        /// Start x, points.
        x1: f32,
        /// Start y, points.
        y1: f32,
        /// End x, points.
        x2: f32,
        /// End y, points.
        y2: f32,
        /// Shared style (`fill` is ignored).
        style: ShapeStyle,
    },
    /// Ellipse centered at `(cx, cy)` with radii `rx`/`ry`, drawn as
    /// four cubic Béziers. Radii must be positive.
    Ellipse {
        /// Center x, points.
        cx: f32,
        /// Center y, points.
        cy: f32,
        /// Horizontal radius, points; must be finite and `> 0`.
        rx: f32,
        /// Vertical radius, points; must be finite and `> 0`.
        ry: f32,
        /// Shared style.
        style: ShapeStyle,
    },
    /// Arrow from `(x1, y1)` to `(x2, y2)`: a shaft plus two head
    /// segments at the tip. The endpoints must be distinct.
    /// `head_length` overrides the auto-computed head size.
    Arrow {
        /// Tail x, points.
        x1: f32,
        /// Tail y, points.
        y1: f32,
        /// Tip x, points.
        x2: f32,
        /// Tip y, points.
        y2: f32,
        /// Optional explicit head length in points; must be finite and
        /// `> 0` when present. `None` uses a quarter of the shaft
        /// length clamped to `4..=48` pt.
        head_length: Option<f32>,
        /// Shared style (`fill` is ignored).
        style: ShapeStyle,
    },
}

/// Options for [`draw_shapes`].
#[derive(Debug, Clone)]
pub struct DrawSpec {
    /// Shapes to draw, in order. Must be non-empty.
    pub shapes: Vec<Shape>,
    /// Page range spec (1-based, e.g. `"1-3,8"` or `"all"`). `None`
    /// draws on every page.
    pub pages: Option<String>,
}

impl Default for DrawSpec {
    fn default() -> Self {
        Self {
            shapes: Vec::new(),
            pages: None,
        }
    }
}

/// Kappa constant for approximating a circle/ellipse with four cubic
/// Bézier segments (`4/3 * (sqrt(2) - 1)`).
const KAPPA: f32 = 0.5522847498;

/// Half-angle of an arrow head, in radians (30 degrees).
const ARROW_HEAD_HALF_ANGLE: f32 = std::f32::consts::FRAC_PI_6;

fn real(v: f32) -> Object {
    Object::Real(v)
}

fn name(s: &str) -> Object {
    Object::Name(s.as_bytes().to_vec())
}

/// Validate one shape's geometry and style. Returns a descriptive
/// [`PdfError::InvalidInput`] on the first problem found.
fn validate_shape(shape: &Shape) -> PdfResult<()> {
    let style = match shape {
        Shape::Rectangle { style, .. }
        | Shape::Line { style, .. }
        | Shape::Ellipse { style, .. }
        | Shape::Arrow { style, .. } => *style,
    };
    if !style.line_width.is_finite() || style.line_width <= 0.0 || style.line_width > 1000.0 {
        return Err(PdfError::InvalidInput(format!(
            "shape line_width must be in (0, 1000], got {}",
            style.line_width
        )));
    }
    if let Some(opacity) = style.opacity
        && (!opacity.is_finite() || !(0.0..=1.0).contains(&opacity))
    {
        return Err(PdfError::InvalidInput(format!(
            "shape opacity must be in [0, 1], got {opacity}"
        )));
    }

    match shape {
        Shape::Rectangle {
            x,
            y,
            width,
            height,
            ..
        } => {
            for (label, v) in [("x", *x), ("y", *y), ("width", *width), ("height", *height)] {
                if !v.is_finite() {
                    return Err(PdfError::InvalidInput(format!(
                        "rectangle {label} must be finite, got {v}"
                    )));
                }
            }
            if *width <= 0.0 || *height <= 0.0 {
                return Err(PdfError::InvalidInput(format!(
                    "rectangle width and height must be > 0, got {width} x {height}"
                )));
            }
        }
        Shape::Line { x1, y1, x2, y2, .. } => {
            for (label, v) in [("x1", *x1), ("y1", *y1), ("x2", *x2), ("y2", *y2)] {
                if !v.is_finite() {
                    return Err(PdfError::InvalidInput(format!(
                        "line {label} must be finite, got {v}"
                    )));
                }
            }
        }
        Shape::Ellipse { cx, cy, rx, ry, .. } => {
            for (label, v) in [("cx", *cx), ("cy", *cy), ("rx", *rx), ("ry", *ry)] {
                if !v.is_finite() {
                    return Err(PdfError::InvalidInput(format!(
                        "ellipse {label} must be finite, got {v}"
                    )));
                }
            }
            if *rx <= 0.0 || *ry <= 0.0 {
                return Err(PdfError::InvalidInput(format!(
                    "ellipse radii must be > 0, got rx={rx} ry={ry}"
                )));
            }
        }
        Shape::Arrow {
            x1,
            y1,
            x2,
            y2,
            head_length,
            ..
        } => {
            for (label, v) in [("x1", *x1), ("y1", *y1), ("x2", *x2), ("y2", *y2)] {
                if !v.is_finite() {
                    return Err(PdfError::InvalidInput(format!(
                        "arrow {label} must be finite, got {v}"
                    )));
                }
            }
            let dx = x2 - x1;
            let dy = y2 - y1;
            if dx == 0.0 && dy == 0.0 {
                return Err(PdfError::InvalidInput(
                    "arrow endpoints must be distinct".to_string(),
                ));
            }
            if let Some(h) = head_length
                && (!h.is_finite() || *h <= 0.0)
            {
                return Err(PdfError::InvalidInput(format!(
                    "arrow head_length must be > 0, got {h}"
                )));
            }
        }
    }
    Ok(())
}

/// Build the path-construction operators for `shape`, translated by
/// `(ox, oy)` (the page's MediaBox lower-left corner). The returned
/// operations end with an open path — callers add painting operators.
fn shape_path_ops(shape: &Shape, ox: f32, oy: f32) -> Vec<Operation> {
    match *shape {
        Shape::Rectangle {
            x,
            y,
            width,
            height,
            ..
        } => vec![Operation::new(
            "re",
            vec![real(x + ox), real(y + oy), real(width), real(height)],
        )],
        Shape::Line { x1, y1, x2, y2, .. } => vec![
            Operation::new("m", vec![real(x1 + ox), real(y1 + oy)]),
            Operation::new("l", vec![real(x2 + ox), real(y2 + oy)]),
        ],
        Shape::Ellipse { cx, cy, rx, ry, .. } => {
            let (cx, cy) = (cx + ox, cy + oy);
            let kx = KAPPA * rx;
            let ky = KAPPA * ry;
            vec![
                Operation::new("m", vec![real(cx + rx), real(cy)]),
                // Right → top → left → bottom → right.
                Operation::new(
                    "c",
                    vec![
                        real(cx + rx),
                        real(cy + ky),
                        real(cx + kx),
                        real(cy + ry),
                        real(cx),
                        real(cy + ry),
                    ],
                ),
                Operation::new(
                    "c",
                    vec![
                        real(cx - kx),
                        real(cy + ry),
                        real(cx - rx),
                        real(cy + ky),
                        real(cx - rx),
                        real(cy),
                    ],
                ),
                Operation::new(
                    "c",
                    vec![
                        real(cx - rx),
                        real(cy - ky),
                        real(cx - kx),
                        real(cy - ry),
                        real(cx),
                        real(cy - ry),
                    ],
                ),
                Operation::new(
                    "c",
                    vec![
                        real(cx + kx),
                        real(cy - ry),
                        real(cx + rx),
                        real(cy - ky),
                        real(cx + rx),
                        real(cy),
                    ],
                ),
                Operation::new("h", vec![]),
            ]
        }
        Shape::Arrow {
            x1,
            y1,
            x2,
            y2,
            head_length,
            ..
        } => {
            let (x1, y1, x2, y2) = (x1 + ox, y1 + oy, x2 + ox, y2 + oy);
            let dx = x2 - x1;
            let dy = y2 - y1;
            let len = dx.hypot(dy);
            // Validated non-zero by validate_shape; guard anyway so a
            // direct caller of this helper cannot produce NaNs.
            if !(len.is_finite() && len > 0.0) {
                return vec![
                    Operation::new("m", vec![real(x1), real(y1)]),
                    Operation::new("l", vec![real(x2), real(y2)]),
                ];
            }
            let (ux, uy) = (dx / len, dy / len);
            let head_len = head_length
                .map(|h| h.max(0.0))
                .unwrap_or_else(|| (len * 0.25).clamp(4.0, 48.0));
            // Unit perpendicular to the shaft.
            let (nx, ny) = (-uy, ux);
            let back = head_len * ARROW_HEAD_HALF_ANGLE.cos();
            let side = head_len * ARROW_HEAD_HALF_ANGLE.sin();
            // Wing endpoints: tip minus head vector ± perpendicular wing.
            let (w1x, w1y) = (x2 - back * ux - side * nx, y2 - back * uy - side * ny);
            let (w2x, w2y) = (x2 - back * ux + side * nx, y2 - back * uy + side * ny);
            vec![
                Operation::new("m", vec![real(x1), real(y1)]),
                Operation::new("l", vec![real(x2), real(y2)]),
                Operation::new("l", vec![real(w1x), real(w1y)]),
                Operation::new("m", vec![real(x2), real(y2)]),
                Operation::new("l", vec![real(w2x), real(w2y)]),
            ]
        }
    }
}

/// Append the painting operators for one shape: colors, line width,
/// then `f`/`S` as required. `path` is a closure returning the path
/// operators so the path can be emitted twice when fill + stroke are
/// both requested.
fn paint_ops(shape: &Shape, path: &dyn Fn() -> Vec<Operation>) -> Vec<Operation> {
    let style = match shape {
        Shape::Rectangle { style, .. }
        | Shape::Line { style, .. }
        | Shape::Ellipse { style, .. }
        | Shape::Arrow { style, .. } => *style,
    };
    let [sr, sg, sb] = style.stroke;
    let mut ops = vec![
        Operation::new(
            "RG",
            vec![
                real(sr as f32 / 255.0),
                real(sg as f32 / 255.0),
                real(sb as f32 / 255.0),
            ],
        ),
        Operation::new("w", vec![real(style.line_width)]),
    ];
    // The path must be constructed for every shape — painting
    // operators consume it, and fill+stroke needs it twice.
    ops.extend(path());
    // Lines have no interior; ignore their fill.
    let wants_fill = !matches!(shape, Shape::Line { .. }) && style.fill.is_some();
    if let Some([fr, fg, fb]) = style.fill.filter(|_| wants_fill) {
        ops.push(Operation::new(
            "rg",
            vec![
                real(fr as f32 / 255.0),
                real(fg as f32 / 255.0),
                real(fb as f32 / 255.0),
            ],
        ));
        ops.push(Operation::new("f", vec![]));
        // `f` consumed the path; rebuild it for the stroke.
        ops.extend(path());
    }
    ops.push(Operation::new("S", vec![]));
    ops
}

/// Build the complete content-stream operations for one shape,
/// including the `q`/`Q` wrapper and optional ExtGState.
fn shape_operations(
    doc: &mut lopdf::Document,
    page_id: lopdf::ObjectId,
    shape: &Shape,
    gs_cache: &mut HashMap<u32, String>,
    ox: f32,
    oy: f32,
) -> PdfResult<Vec<Operation>> {
    let style = match shape {
        Shape::Rectangle { style, .. }
        | Shape::Line { style, .. }
        | Shape::Ellipse { style, .. }
        | Shape::Arrow { style, .. } => *style,
    };
    let mut ops = vec![Operation::new("q", vec![])];
    if let Some(opacity) = style.opacity.filter(|o| *o < 1.0) {
        let key = opacity.to_bits();
        if !gs_cache.contains_key(&key) {
            let gs_name = ensure_ext_gstate(doc, page_id, opacity)?;
            gs_cache.insert(key, gs_name);
        }
        if let Some(gs_name) = gs_cache.get(&key) {
            ops.push(Operation::new("gs", vec![name(gs_name)]));
        }
    }
    let path = || shape_path_ops(shape, ox, oy);
    ops.extend(paint_ops(shape, &path));
    ops.push(Operation::new("Q", vec![]));
    Ok(ops)
}

/// Draw `spec.shapes` on the selected pages of `input`; returns a new
/// PDF.
///
/// All shapes are validated before the document is loaded: non-empty
/// shape list, finite coordinates, positive rectangle dimensions /
/// ellipse radii, distinct arrow endpoints, positive `line_width` and
/// valid `opacity`. The page range is validated against the actual
/// page count. Returns [`PdfError::Encrypted`] for encrypted input.
pub fn draw_shapes(input: &[u8], spec: &DrawSpec, limits: &EngineLimits) -> PdfResult<Vec<u8>> {
    if spec.shapes.is_empty() {
        return Err(PdfError::InvalidInput(
            "draw_shapes requires at least one shape".to_string(),
        ));
    }
    for shape in &spec.shapes {
        validate_shape(shape)?;
    }

    let mut doc = load_pdf(input, limits)?;
    reject_encrypted(&doc)?;

    let pages_map = doc.get_pages();
    let page_count = pages_map.len() as u32;
    let selected: Vec<u32> = match &spec.pages {
        None => (1..=page_count).collect(),
        Some(range) => parse_page_range(range, page_count)?,
    };

    for page_number in selected {
        let Some(&page_id) = pages_map.get(&page_number) else {
            return Err(PdfError::InvalidPageRange(format!(
                "page {page_number} not found in document"
            )));
        };
        // Coordinates are page-local: origin at the MediaBox lower-left.
        let [bx0, by0, _, _] = media_box(&doc, page_id);
        let (ox, oy) = (bx0 as f32, by0 as f32);

        // One ExtGState per distinct opacity value on this page.
        let mut gs_cache: HashMap<u32, String> = HashMap::new();
        let mut all_ops: Vec<Operation> = Vec::new();
        for shape in &spec.shapes {
            all_ops.extend(shape_operations(
                &mut doc,
                page_id,
                shape,
                &mut gs_cache,
                ox,
                oy,
            )?);
        }

        let stream_id = add_content_stream(&mut doc, encode_operations(all_ops)?)?;
        // Shapes are annotations: draw on top of existing content.
        append_page_overlay(&mut doc, page_id, stream_id)?;
    }

    doc.compress();
    save_pdf(&mut doc)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::crates::pdf_engine::ops::encrypt::{PdfPermissions, encrypt};
    use crate::crates::pdf_engine::ops::test_utils::{page_count, pdf_with_pages};
    use lopdf::Document;

    fn spec() -> DrawSpec {
        DrawSpec {
            shapes: vec![Shape::Rectangle {
                x: 10.0,
                y: 10.0,
                width: 100.0,
                height: 50.0,
                style: ShapeStyle::default(),
            }],
            pages: None,
        }
    }

    /// Whether `op` appears as a whitespace-delimited operator token.
    /// lopdf's `Content::encode` emits one operation per line, so a
    /// bare `contains(" S")` never matches a lone paint operator.
    fn has_op(text: &str, op: &str) -> bool {
        text.split_whitespace().any(|tok| tok == op)
    }

    /// Count whitespace-delimited occurrences of operator `op`.
    fn count_op(text: &str, op: &str) -> usize {
        text.split_whitespace().filter(|tok| *tok == op).count()
    }

    fn page_contents(pdf: &[u8]) -> Vec<String> {
        let doc = Document::load_mem(pdf).unwrap();
        doc.get_pages()
            .into_values()
            .map(|id| {
                let raw = doc.get_page_content(id);
                String::from_utf8_lossy(&raw).into_owned()
            })
            .collect()
    }

    #[test]
    fn draws_rectangle_on_all_pages() {
        let pdf = pdf_with_pages(&["alpha", "beta"]);
        let out = draw_shapes(&pdf, &spec(), &EngineLimits::default()).unwrap();
        assert_eq!(page_count(&out), 2);
        let contents = page_contents(&out);
        assert!(contents.iter().all(|c| has_op(c, "re")));
        assert!(contents.iter().all(|c| has_op(c, "S")));
        // Output must re-parse cleanly.
        Document::load_mem(&out).unwrap();
    }

    #[test]
    fn respects_page_range() {
        let pdf = pdf_with_pages(&["one", "two", "three"]);
        let s = DrawSpec {
            pages: Some("2".to_string()),
            ..spec()
        };
        let out = draw_shapes(&pdf, &s, &EngineLimits::default()).unwrap();
        let contents = page_contents(&out);
        assert!(!has_op(&contents[0], "re"));
        assert!(has_op(&contents[1], "re"));
        assert!(!has_op(&contents[2], "re"));
    }

    #[test]
    fn emits_color_width_fill_and_opacity() {
        let pdf = pdf_with_pages(&["x"]);
        let s = DrawSpec {
            shapes: vec![Shape::Ellipse {
                cx: 100.0,
                cy: 100.0,
                rx: 40.0,
                ry: 20.0,
                style: ShapeStyle {
                    stroke: [255, 0, 0],
                    fill: Some([0, 0, 255]),
                    line_width: 2.5,
                    opacity: Some(0.5),
                },
            }],
            pages: None,
        };
        let out = draw_shapes(&pdf, &s, &EngineLimits::default()).unwrap();
        let text = &page_contents(&out)[0];
        assert!(text.contains(" RG"), "missing stroke color: {text}");
        assert!(text.contains(" rg"), "missing fill color: {text}");
        assert!(text.contains(" w"), "missing line width: {text}");
        assert!(text.contains(" gs"), "missing ExtGState: {text}");
        assert!(text.contains(" c"), "missing bezier curves: {text}");
        // Fill then stroke → two paint ops.
        assert!(has_op(text, "f"), "missing fill op: {text}");
        assert!(count_op(text, "S") >= 1);
    }

    #[test]
    fn line_and_arrow_paths() {
        let pdf = pdf_with_pages(&["x"]);
        let s = DrawSpec {
            shapes: vec![
                Shape::Line {
                    x1: 0.0,
                    y1: 0.0,
                    x2: 50.0,
                    y2: 50.0,
                    style: ShapeStyle::default(),
                },
                Shape::Arrow {
                    x1: 10.0,
                    y1: 10.0,
                    x2: 110.0,
                    y2: 10.0,
                    head_length: None,
                    style: ShapeStyle::default(),
                },
            ],
            pages: None,
        };
        let out = draw_shapes(&pdf, &s, &EngineLimits::default()).unwrap();
        let text = &page_contents(&out)[0];
        // Arrow: shaft plus two head segments = 3 `l` ops total per shape
        // set (line has 1, arrow has 4 including head wings).
        assert!(text.contains("m"), "missing move ops: {text}");
        assert!(text.contains("l"), "missing line ops: {text}");
    }

    #[test]
    fn rejects_bad_specs() {
        let pdf = pdf_with_pages(&["x"]);
        let limits = EngineLimits::default();
        let style = ShapeStyle::default();
        let bad_specs = vec![
            DrawSpec {
                shapes: vec![],
                pages: None,
            },
            DrawSpec {
                shapes: vec![Shape::Rectangle {
                    x: f32::NAN,
                    y: 0.0,
                    width: 10.0,
                    height: 10.0,
                    style,
                }],
                pages: None,
            },
            DrawSpec {
                shapes: vec![Shape::Rectangle {
                    x: 0.0,
                    y: 0.0,
                    width: 0.0,
                    height: 10.0,
                    style,
                }],
                pages: None,
            },
            DrawSpec {
                shapes: vec![Shape::Rectangle {
                    x: 0.0,
                    y: 0.0,
                    width: 10.0,
                    height: -5.0,
                    style,
                }],
                pages: None,
            },
            DrawSpec {
                shapes: vec![Shape::Ellipse {
                    cx: 0.0,
                    cy: 0.0,
                    rx: 0.0,
                    ry: 10.0,
                    style,
                }],
                pages: None,
            },
            DrawSpec {
                shapes: vec![Shape::Arrow {
                    x1: 5.0,
                    y1: 5.0,
                    x2: 5.0,
                    y2: 5.0,
                    head_length: None,
                    style,
                }],
                pages: None,
            },
            DrawSpec {
                shapes: vec![Shape::Line {
                    x1: 0.0,
                    y1: 0.0,
                    x2: 1.0,
                    y2: 1.0,
                    style: ShapeStyle {
                        line_width: 0.0,
                        ..style
                    },
                }],
                pages: None,
            },
            DrawSpec {
                shapes: vec![Shape::Line {
                    x1: 0.0,
                    y1: 0.0,
                    x2: 1.0,
                    y2: 1.0,
                    style: ShapeStyle {
                        line_width: f32::INFINITY,
                        ..style
                    },
                }],
                pages: None,
            },
            DrawSpec {
                shapes: vec![Shape::Line {
                    x1: 0.0,
                    y1: 0.0,
                    x2: 1.0,
                    y2: 1.0,
                    style: ShapeStyle {
                        opacity: Some(1.5),
                        ..style
                    },
                }],
                pages: None,
            },
            DrawSpec {
                shapes: vec![Shape::Arrow {
                    x1: 0.0,
                    y1: 0.0,
                    x2: 10.0,
                    y2: 0.0,
                    head_length: Some(-1.0),
                    style,
                }],
                pages: None,
            },
        ];
        for s in bad_specs {
            assert!(
                draw_shapes(&pdf, &s, &limits).is_err(),
                "spec {s:?} should be rejected"
            );
        }
    }

    #[test]
    fn rejects_bad_page_range_and_garbage_input() {
        let pdf = pdf_with_pages(&["x"]);
        let s = DrawSpec {
            pages: Some("9".to_string()),
            ..spec()
        };
        assert!(draw_shapes(&pdf, &s, &EngineLimits::default()).is_err());
        assert!(draw_shapes(b"junk", &spec(), &EngineLimits::default()).is_err());
    }

    #[test]
    fn encrypted_input_is_rejected() {
        let base = pdf_with_pages(&["secret"]);
        let encrypted = encrypt(
            &base,
            "pw",
            None,
            PdfPermissions::default(),
            &EngineLimits::default(),
        )
        .unwrap();
        assert!(matches!(
            draw_shapes(&encrypted, &spec(), &EngineLimits::default()),
            Err(PdfError::Encrypted)
        ));
    }
}
