//! Draw text at arbitrary page coordinates.
//!
//! Each selected page gets an appended content stream that paints the
//! text on top of existing content — the Stirling-PDF "add text at
//! arbitrary position" feature. Coordinates are absolute PDF points
//! with the origin at the **bottom-left** of the page's `/MediaBox`
//! (PDF user space); `y` grows upward, matching how viewers display
//! the page.
//!
//! # Rendering details
//!
//! - Font: base-14 Helvetica via [`ensure_font_resource`], WinAnsi
//!   encoded — characters outside that set may render as `.notdef`
//!   (see [`pdf_text_operand`]).
//! - `color` sets the non-stroking RGB color (`rg`); omitted means
//!   black, the PDF default.
//! - `opacity` registers an `/ExtGState` (`ca`/`CA`) and is skipped
//!   when `>= 1.0`.
//! - `rotation` is counterclockwise degrees around the text origin,
//!   applied through the `cm` matrix so the baseline pivots at `(x, y)`.
//!
//! `/UserUnit` is ignored, as in the other mark ops.

use lopdf::Object;
use lopdf::content::Operation;

use crate::crates::core::engine::EngineLimits;

use crate::crates::pdf_engine::error::{PdfError, PdfResult};

use super::load::{load_pdf, reject_encrypted, save_pdf};
use super::overlay::{
    add_content_stream, append_page_overlay, encode_operations, ensure_ext_gstate,
    ensure_font_resource, pdf_text_operand,
};
use super::pages::parse_page_range;

/// Options for [`draw_text`].
#[derive(Debug, Clone)]
pub struct DrawTextSpec {
    /// Text to draw. Whitespace-only text counts as absent and is
    /// rejected.
    pub text: String,
    /// Baseline origin, x coordinate in points (PDF user space).
    pub x: f32,
    /// Baseline origin, y coordinate in points (PDF user space).
    pub y: f32,
    /// Font size in points; must be finite and in `0.1..=1000`.
    pub font_size: f32,
    /// RGB fill color, components in `0.0..=1.0`. `None` = black.
    pub color: Option<[f32; 3]>,
    /// Opacity in `0.0..=1.0` via an ExtGState resource. `None` or
    /// `>= 1.0` is fully opaque.
    pub opacity: Option<f32>,
    /// Counterclockwise rotation in degrees around `(x, y)`.
    /// `None` = 0.
    pub rotation: Option<f32>,
    /// Page range spec (1-based, e.g. `"1-3,8"` or `"all"`). `None`
    /// draws on every page.
    pub pages: Option<String>,
}

impl Default for DrawTextSpec {
    fn default() -> Self {
        Self {
            text: String::new(),
            x: 0.0,
            y: 0.0,
            font_size: 12.0,
            color: None,
            opacity: None,
            rotation: None,
            pages: None,
        }
    }
}

/// Draw `spec.text` on the selected pages of `input`; returns a new PDF.
///
/// Validates the spec before loading the document: non-empty text,
/// finite coordinates, `font_size` in `0.1..=1000`, color components in
/// `0.0..=1.0`, `opacity` in `0.0..=1.0`, finite `rotation`. The page
/// range is validated against the actual page count. Returns
/// [`PdfError::Encrypted`] for encrypted input.
pub fn draw_text(input: &[u8], spec: &DrawTextSpec, limits: &EngineLimits) -> PdfResult<Vec<u8>> {
    if spec.text.trim().is_empty() {
        return Err(PdfError::InvalidInput(
            "draw_text requires non-empty text".to_string(),
        ));
    }
    if !spec.x.is_finite() || !spec.y.is_finite() {
        return Err(PdfError::InvalidInput(format!(
            "draw_text coordinates must be finite, got ({}, {})",
            spec.x, spec.y
        )));
    }
    if !spec.font_size.is_finite() || !(0.1..=1000.0).contains(&spec.font_size) {
        return Err(PdfError::InvalidInput(format!(
            "draw_text font_size must be in [0.1, 1000], got {}",
            spec.font_size
        )));
    }
    if let Some([r, g, b]) = spec.color
        && [r, g, b]
            .iter()
            .any(|c| !c.is_finite() || !(0.0..=1.0).contains(c))
    {
        return Err(PdfError::InvalidInput(format!(
            "draw_text color components must be in [0, 1], got {r},{g},{b}"
        )));
    }
    if let Some(opacity) = spec.opacity
        && (!opacity.is_finite() || !(0.0..=1.0).contains(&opacity))
    {
        return Err(PdfError::InvalidInput(format!(
            "draw_text opacity must be in [0, 1], got {opacity}"
        )));
    }
    if let Some(rotation) = spec.rotation
        && !rotation.is_finite()
    {
        return Err(PdfError::InvalidInput(format!(
            "draw_text rotation must be finite, got {rotation}"
        )));
    }

    let mut doc = load_pdf(input, limits)?;
    reject_encrypted(&doc)?;

    let pages_map = doc.get_pages();
    let page_count = pages_map.len() as u32;
    let selected: Vec<u32> = match &spec.pages {
        None => (1..=page_count).collect(),
        Some(range) => parse_page_range(range, page_count)?,
    };

    let rotation = spec.rotation.unwrap_or(0.0);
    let (sin, cos) = rotation.to_radians().sin_cos();

    for page_number in selected {
        let Some(&page_id) = pages_map.get(&page_number) else {
            return Err(PdfError::InvalidPageRange(format!(
                "page {page_number} not found in document"
            )));
        };

        let mut operations = vec![Operation::new("q", vec![])];
        if let Some(opacity) = spec.opacity.filter(|o| *o < 1.0) {
            let gs_name = ensure_ext_gstate(&mut doc, page_id, opacity)?;
            operations.push(Operation::new(
                "gs",
                vec![Object::Name(gs_name.into_bytes())],
            ));
        }
        if let Some([r, g, b]) = spec.color {
            operations.push(Operation::new(
                "rg",
                vec![Object::Real(r), Object::Real(g), Object::Real(b)],
            ));
        }
        // Rotate about the text origin, then translate to (x, y): the
        // baseline pivot stays at the requested point.
        operations.push(Operation::new(
            "cm",
            vec![
                Object::Real(cos),
                Object::Real(sin),
                Object::Real(-sin),
                Object::Real(cos),
                Object::Real(spec.x),
                Object::Real(spec.y),
            ],
        ));
        let font_name = ensure_font_resource(&mut doc, page_id)?;
        operations.extend([
            Operation::new("BT", vec![]),
            Operation::new(
                "Tf",
                vec![
                    Object::Name(font_name.into_bytes()),
                    Object::Real(spec.font_size),
                ],
            ),
            Operation::new("Tj", vec![pdf_text_operand(&spec.text)]),
            Operation::new("ET", vec![]),
            Operation::new("Q", vec![]),
        ]);

        let stream_id = add_content_stream(&mut doc, encode_operations(operations)?)?;
        // Drawn text goes on top of existing content.
        append_page_overlay(&mut doc, page_id, stream_id)?;
    }

    doc.compress();
    save_pdf(&mut doc)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::crates::pdf_engine::ops::test_utils::{page_count, pdf_with_pages};
    use lopdf::Document;

    fn spec() -> DrawTextSpec {
        DrawTextSpec {
            text: "ZZZMARKER".to_string(),
            x: 72.0,
            y: 700.0,
            font_size: 14.0,
            ..Default::default()
        }
    }

    fn page_texts(pdf: &[u8]) -> Vec<String> {
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
    fn draws_on_all_pages_by_default() {
        let pdf = pdf_with_pages(&["alpha", "beta"]);
        let out = draw_text(&pdf, &spec(), &EngineLimits::default()).unwrap();
        assert_eq!(page_count(&out), 2);
        let texts = page_texts(&out);
        assert!(texts.iter().all(|t| t.contains("(ZZZMARKER)")));
        assert!(texts.iter().all(|t| t.contains("/KF")));
    }

    #[test]
    fn page_range_selects_subset() {
        let pdf = pdf_with_pages(&["one", "two", "three"]);
        let s = DrawTextSpec {
            pages: Some("2".to_string()),
            ..spec()
        };
        let out = draw_text(&pdf, &s, &EngineLimits::default()).unwrap();
        let texts = page_texts(&out);
        assert!(!texts[0].contains("ZZZMARKER"));
        assert!(texts[1].contains("ZZZMARKER"));
        assert!(!texts[2].contains("ZZZMARKER"));
    }

    #[test]
    fn color_opacity_and_rotation_are_emitted() {
        let pdf = pdf_with_pages(&["x"]);
        let s = DrawTextSpec {
            color: Some([1.0, 0.0, 0.0]),
            opacity: Some(0.5),
            rotation: Some(45.0),
            ..spec()
        };
        let out = draw_text(&pdf, &s, &EngineLimits::default()).unwrap();
        let text = &page_texts(&out)[0];
        assert!(text.contains(" rg"), "missing color op: {text}");
        assert!(text.contains(" gs"), "missing gs op: {text}");
        assert!(text.contains(" cm"), "missing cm op: {text}");
        lopdf::Document::load_mem(&out).unwrap();
    }

    #[test]
    fn rejects_bad_specs() {
        let pdf = pdf_with_pages(&["x"]);
        let limits = EngineLimits::default();
        let cases = [
            DrawTextSpec {
                text: "   ".to_string(),
                ..spec()
            },
            DrawTextSpec {
                x: f32::NAN,
                ..spec()
            },
            DrawTextSpec {
                font_size: 0.0,
                ..spec()
            },
            DrawTextSpec {
                font_size: 1001.0,
                ..spec()
            },
            DrawTextSpec {
                color: Some([1.5, 0.0, 0.0]),
                ..spec()
            },
            DrawTextSpec {
                opacity: Some(1.5),
                ..spec()
            },
            DrawTextSpec {
                rotation: Some(f32::INFINITY),
                ..spec()
            },
        ];
        for s in cases {
            assert!(
                draw_text(&pdf, &s, &limits).is_err(),
                "spec {s:?} should be rejected"
            );
        }
    }

    #[test]
    fn rejects_bad_page_range_and_garbage_input() {
        let pdf = pdf_with_pages(&["x"]);
        let s = DrawTextSpec {
            pages: Some("9".to_string()),
            ..spec()
        };
        assert!(draw_text(&pdf, &s, &EngineLimits::default()).is_err());
        assert!(draw_text(b"junk", &spec(), &EngineLimits::default()).is_err());
    }
}
