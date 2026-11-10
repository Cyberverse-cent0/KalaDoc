//! Bates numbering — stamp sequential, zero-padded identifiers on
//! selected pages.
//!
//! Each selected page gets an appended content stream (via the shared
//! watermark/stamp machinery) that draws `prefix` + zero-padded
//! counter as Helvetica text at [`BatesSpec::position`]. The counter
//! starts at [`BatesSpec::start`] and increments by one per *selected*
//! page, in page order — skipped pages consume no number.
//!
//! The number is formatted with exactly `digits` zero-padded digits
//! (minimum 4); if the counter grows past `digits` digits it simply
//! prints wider — it never truncates.
//!
//! `/UserUnit` is ignored (see `overlay` module docs).

use lopdf::Object;
use lopdf::content::Operation;

use crate::crates::core::engine::EngineLimits;

use crate::crates::pdf_engine::error::{PdfError, PdfResult};

use super::load::{load_pdf, reject_encrypted, save_pdf};
use super::pages::parse_page_range;
use super::watermark::{
    MarkPosition, add_content_stream, append_page_overlay, box_origin, encode_operations,
    ensure_ext_gstate, ensure_font_resource, estimate_text_width, media_box, pdf_text_operand,
    position_point,
};

/// Minimum / maximum width of the zero-padded counter portion.
const MIN_DIGITS: u32 = 4;
const MAX_DIGITS: u32 = 20;
/// Maximum length of `prefix` in characters.
const MAX_PREFIX_CHARS: usize = 64;
/// Fixed edge distance used for edge/corner placements (the spec has
/// no margin knob).
const MARGIN: f32 = 36.0;

/// Options for [`add_bates`].
#[derive(Debug, Clone)]
pub struct BatesSpec {
    /// Text prepended to every number (e.g. `"ABC-"`). Must be at most
    /// 64 characters and Latin-1 encodable (the base-14 Helvetica font
    /// cannot render anything else). Default empty.
    pub prefix: String,
    /// First number stamped. Default 1.
    pub start: u64,
    /// Minimum number of zero-padded digits for the counter,
    /// `4..=20`. Default 6.
    pub digits: u32,
    /// Page range spec (1-based, e.g. `"1-3,8"` or `"all"`). `None`
    /// numbers every page.
    pub pages: Option<String>,
    /// Where on the page the mark is anchored. Default `BottomRight`
    /// (the conventional Bates placement).
    pub position: MarkPosition,
    /// Font size in points; must be finite and in `0.1..=1000`.
    /// Default 10.
    pub font_size: f32,
    /// RGB fill color as 8-bit components `0..=255`. Default black.
    pub color: [u8; 3],
    /// Opacity in `0.0..=1.0` via an ExtGState resource. Default 1.0.
    pub opacity: f32,
}

impl Default for BatesSpec {
    fn default() -> Self {
        Self {
            prefix: String::new(),
            start: 1,
            digits: 6,
            pages: None,
            position: MarkPosition::BottomRight,
            font_size: 10.0,
            color: [0, 0, 0],
            opacity: 1.0,
        }
    }
}

/// Format one Bates label: `prefix` + counter zero-padded to `digits`.
fn bates_label(prefix: &str, counter: u64, digits: u32) -> String {
    format!("{prefix}{counter:0width$}", width = digits as usize)
}

/// Build the content-stream operations drawing `label` on one page.
#[allow(clippy::too_many_arguments)]
fn bates_operations(
    label: &str,
    font_size: f32,
    color: [u8; 3],
    position: MarkPosition,
    page: (f32, f32, f32, f32),
    font_name: &str,
    gs_name: Option<&str>,
) -> Vec<Operation> {
    let (x0, y0, x1, y1) = page;
    let page_w = x1 - x0;
    let page_h = y1 - y0;

    let mark_w = estimate_text_width(label, font_size);
    let mark_h = font_size;
    let anchor = position_point(position, page_w, page_h, MARGIN);
    let (bx, by) = box_origin(position, anchor, mark_w, mark_h);
    // Absolute lower-left of the mark box; baseline sits at box bottom.
    let tx = x0 + bx;
    let ty = y0 + by;

    let mut ops = vec![Operation::new("q", vec![])];
    if let Some(gs) = gs_name {
        ops.push(Operation::new(
            "gs",
            vec![Object::Name(gs.as_bytes().to_vec())],
        ));
    }
    ops.push(Operation::new(
        "rg",
        vec![
            Object::Real(f32::from(color[0]) / 255.0),
            Object::Real(f32::from(color[1]) / 255.0),
            Object::Real(f32::from(color[2]) / 255.0),
        ],
    ));
    ops.extend([
        Operation::new("BT", vec![]),
        Operation::new(
            "Tf",
            vec![
                Object::Name(font_name.as_bytes().to_vec()),
                Object::Real(font_size),
            ],
        ),
        Operation::new("Td", vec![Object::Real(tx), Object::Real(ty)]),
        Operation::new("Tj", vec![pdf_text_operand(label)]),
        Operation::new("ET", vec![]),
        Operation::new("Q", vec![]),
    ]);
    ops
}

/// Stamp sequential Bates numbers on the selected pages of `input`;
/// returns the new PDF bytes.
///
/// Validation happens before any document mutation: `digits` must be in
/// `4..=20`, `prefix` at most 64 Latin-1 characters, `font_size` finite
/// in `0.1..=1000`, `opacity` finite in `0.0..=1.0`, and the page range
/// is checked against the actual page count. The counter is `u64`;
/// `start + selected_count - 1` overflowing `u64` is rejected. Returns
/// [`PdfError::Encrypted`] for encrypted input.
pub fn add_bates(input: &[u8], spec: &BatesSpec, limits: &EngineLimits) -> PdfResult<Vec<u8>> {
    if !(MIN_DIGITS..=MAX_DIGITS).contains(&spec.digits) {
        return Err(PdfError::InvalidInput(format!(
            "bates digits must be in [{MIN_DIGITS}, {MAX_DIGITS}], got {}",
            spec.digits
        )));
    }
    if spec.prefix.chars().count() > MAX_PREFIX_CHARS {
        return Err(PdfError::InvalidInput(format!(
            "bates prefix must be at most {MAX_PREFIX_CHARS} characters, got {}",
            spec.prefix.chars().count()
        )));
    }
    if spec
        .prefix
        .chars()
        .any(|c| (c as u32) > 0xFF || (c as u32) < 0x20 || c as u32 == 0x7F)
    {
        return Err(PdfError::InvalidInput(
            "bates prefix must be encodable in the base-14 Helvetica font (Latin-1, no control characters)"
                .to_string(),
        ));
    }
    if !spec.font_size.is_finite() || !(0.1..=1000.0).contains(&spec.font_size) {
        return Err(PdfError::InvalidInput(format!(
            "bates font_size must be in [0.1, 1000], got {}",
            spec.font_size
        )));
    }
    if !spec.opacity.is_finite() || !(0.0..=1.0).contains(&spec.opacity) {
        return Err(PdfError::InvalidInput(format!(
            "bates opacity must be in [0, 1], got {}",
            spec.opacity
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

    // The counter increments once per selected page; check for u64
    // overflow up front so a partial run never happens.
    let last = selected.len() as u64 - 1;
    let _ = spec
        .start
        .checked_add(last)
        .ok_or_else(|| PdfError::InvalidInput("bates counter overflows u64".to_string()))?;

    let mut counter = spec.start;
    let selected_count = selected.len();
    for (index, page_number) in selected.into_iter().enumerate() {
        let Some(&page_id) = pages_map.get(&page_number) else {
            return Err(PdfError::InvalidPageRange(format!(
                "page {page_number} not found in document"
            )));
        };
        let [bx0, by0, bx1, by1] = media_box(&doc, page_id);
        let page = (
            bx0 as f32,
            by0 as f32,
            bx0 as f32 + (bx1 - bx0) as f32,
            by0 as f32 + (by1 - by0) as f32,
        );

        let needs_gs = spec.opacity < 1.0;
        let gs_name = if needs_gs {
            Some(ensure_ext_gstate(&mut doc, page_id, spec.opacity)?)
        } else {
            None
        };
        let font_name = ensure_font_resource(&mut doc, page_id)?;

        let label = bates_label(&spec.prefix, counter, spec.digits);
        // The overflow check above guarantees `counter` itself is valid;
        // only increment when another page will consume the next value,
        // so a `u64::MAX` start on the final page cannot wrap.
        if index + 1 < selected_count {
            counter += 1;
        }

        let operations = bates_operations(
            &label,
            spec.font_size,
            spec.color,
            spec.position,
            page,
            &font_name,
            gs_name.as_deref(),
        );
        let stream_id = add_content_stream(&mut doc, encode_operations(operations)?)?;
        // Bates marks go on top of existing content.
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

    fn spec() -> BatesSpec {
        BatesSpec {
            prefix: "BATES-".to_string(),
            start: 1,
            digits: 4,
            ..Default::default()
        }
    }

    /// Decompressed content of every page, in page order.
    fn page_contents(pdf: &[u8]) -> Vec<String> {
        let doc = Document::load_mem(pdf).unwrap();
        let mut pages: Vec<u32> = doc.get_pages().keys().copied().collect();
        pages.sort_unstable();
        pages
            .into_iter()
            .map(|n| {
                let id = doc.get_pages()[&n];
                String::from_utf8_lossy(&doc.get_page_content(id)).into_owned()
            })
            .collect()
    }

    #[test]
    fn numbers_increment_across_pages() {
        let pdf = pdf_with_pages(&["a", "b", "c"]);
        let out = add_bates(&pdf, &spec(), &EngineLimits::default()).unwrap();
        assert_eq!(page_count(&out), 3);
        let texts = page_contents(&out);
        assert!(texts[0].contains("(BATES-0001)"), "{}", texts[0]);
        assert!(texts[1].contains("(BATES-0002)"), "{}", texts[1]);
        assert!(texts[2].contains("(BATES-0003)"), "{}", texts[2]);
    }

    #[test]
    fn respects_start_digits_and_page_range() {
        let pdf = pdf_with_pages(&["one", "two", "three", "four"]);
        let s = BatesSpec {
            start: 10,
            digits: 6,
            pages: Some("2-3".to_string()),
            ..spec()
        };
        let out = add_bates(&pdf, &s, &EngineLimits::default()).unwrap();
        let texts = page_contents(&out);
        assert!(!texts[0].contains("BATES-"));
        assert!(texts[1].contains("(BATES-000010)"), "{}", texts[1]);
        assert!(texts[2].contains("(BATES-000011)"), "{}", texts[2]);
        assert!(!texts[3].contains("BATES-"));
    }

    #[test]
    fn counter_widens_beyond_digits_without_truncation() {
        let pdf = pdf_with_pages(&["a", "b", "c", "d", "e"]);
        let s = BatesSpec {
            start: 9998,
            digits: 4,
            ..spec()
        };
        let out = add_bates(&pdf, &s, &EngineLimits::default()).unwrap();
        let texts = page_contents(&out);
        assert!(texts[4].contains("(BATES-10002)"), "{}", texts[4]);
    }

    #[test]
    fn color_opacity_and_position_are_emitted() {
        let pdf = pdf_with_pages(&["x"]);
        let s = BatesSpec {
            color: [255, 0, 0],
            opacity: 0.5,
            position: MarkPosition::TopLeft,
            ..spec()
        };
        let out = add_bates(&pdf, &s, &EngineLimits::default()).unwrap();
        let text = &page_contents(&out)[0];
        assert!(text.contains(" rg"), "missing color op: {text}");
        assert!(text.contains(" gs"), "missing gs op: {text}");
        assert!(text.contains("/KF"), "missing font resource: {text}");
    }

    #[test]
    fn rejects_bad_specs() {
        let pdf = pdf_with_pages(&["x"]);
        let limits = EngineLimits::default();
        let cases = [
            BatesSpec {
                digits: 3,
                ..spec()
            },
            BatesSpec {
                digits: 21,
                ..spec()
            },
            BatesSpec {
                prefix: "é".repeat(65),
                ..spec()
            },
            BatesSpec {
                prefix: "bad\u{7f}prefix".to_string(),
                ..spec()
            },
            BatesSpec {
                font_size: 0.0,
                ..spec()
            },
            BatesSpec {
                font_size: f32::NAN,
                ..spec()
            },
            BatesSpec {
                opacity: 1.5,
                ..spec()
            },
            BatesSpec {
                pages: Some("9".to_string()),
                ..spec()
            },
        ];
        for s in cases {
            assert!(
                add_bates(&pdf, &s, &limits).is_err(),
                "spec {s:?} should be rejected"
            );
        }
    }

    #[test]
    fn rejects_garbage_and_counter_overflow() {
        assert!(add_bates(b"junk", &spec(), &EngineLimits::default()).is_err());
        // Two selected pages + u64::MAX start overflows the counter.
        let pdf = pdf_with_pages(&["x", "y"]);
        let s = BatesSpec {
            start: u64::MAX,
            ..spec()
        };
        assert!(add_bates(&pdf, &s, &EngineLimits::default()).is_err());
        // A single page at u64::MAX still fits (start + 0).
        let pdf1 = pdf_with_pages(&["x"]);
        let ok = BatesSpec {
            start: u64::MAX,
            ..spec()
        };
        assert!(add_bates(&pdf1, &ok, &EngineLimits::default()).is_ok());
    }
}
