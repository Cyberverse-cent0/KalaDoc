//! Set an absolute crop box on selected pages.
//!
//! `crop_box` is `[x0, y0, x1, y1]` in PDF points (user space, before
//! any rotation). The box is validated against each target page's
//! effective `/MediaBox` — inherited from the `/Pages` tree when the
//! page does not define one itself — and pages whose MediaBox does not
//! intersect the box are rejected with [`PdfError::InvalidInput`]
//! rather than producing an empty visible area.
//!
//! The result is written as the page's `/CropBox`; `/MediaBox` and the
//! page content are left unchanged, matching how viewers composite
//! crop.

use lopdf::{Document, Object, ObjectId};

use crate::crates::core::engine::EngineLimits;

use crate::crates::pdf_engine::error::{PdfError, PdfResult};

use super::load::{load_pdf, lopdf_err, reject_encrypted, save_pdf};
use super::pages::parse_page_range;

/// Default MediaBox (US Letter) used when a page tree defines none.
pub(crate) const DEFAULT_MEDIA_BOX: [f64; 4] = [0.0, 0.0, 612.0, 792.0];

/// Set `/CropBox` to `crop_box` (`[x0, y0, x1, y1]` in points) on the
/// pages selected by `pages` (a range spec such as `"1-3,8"` or
/// `"all"`, 1-based).
///
/// Fails with [`PdfError::InvalidInput`] when the box is malformed
/// (negative origin, empty or inverted extent) or lies entirely
/// outside a selected page's MediaBox.
pub fn crop(
    input: &[u8],
    pages: &str,
    crop_box: [f32; 4],
    limits: &EngineLimits,
) -> PdfResult<Vec<u8>> {
    let [x0, y0, x1, y1] = crop_box.map(f64::from);
    if ![x0, y0, x1, y1].iter().all(|v| v.is_finite()) {
        return Err(PdfError::InvalidInput(
            "crop box coordinates must be finite numbers".to_string(),
        ));
    }
    if x0 < 0.0 || y0 < 0.0 {
        return Err(PdfError::InvalidInput(
            "crop box origin must be non-negative".to_string(),
        ));
    }
    if x1 <= x0 || y1 <= y0 {
        return Err(PdfError::InvalidInput(
            "crop box must have positive width and height (x1 > x0, y1 > y0)".to_string(),
        ));
    }

    let mut doc = load_pdf(input, limits)?;
    reject_encrypted(&doc)?;

    let page_count = doc.get_pages().len() as u32;
    let selected = parse_page_range(pages, page_count)?;
    let pages_map = doc.get_pages();

    for page_number in selected {
        let page_id = pages_map[&page_number];
        let media = effective_media_box(&doc, page_id)?;
        let intersects = x1 > media[0] && x0 < media[2] && y1 > media[1] && y0 < media[3];
        if !intersects {
            return Err(PdfError::InvalidInput(format!(
                "crop box [{x0}, {y0}, {x1}, {y1}] does not intersect page {page_number} media box [{}, {}, {}, {}]",
                media[0], media[1], media[2], media[3]
            )));
        }
        let crop_objects: Vec<Object> =
            [x0, y0, x1, y1].iter().map(|&v| number_object(v)).collect();
        doc.get_dictionary_mut(page_id)
            .map_err(lopdf_err)?
            .set("CropBox", Object::Array(crop_objects));
    }

    save_pdf(&mut doc)
}

/// The effective `/MediaBox` of a page: the page dictionary's own
/// entry, else inherited from its `/Pages` ancestors, else the
/// default letter size.
fn effective_media_box(doc: &Document, page_id: ObjectId) -> PdfResult<[f64; 4]> {
    Ok(inherited_rect(doc, page_id, b"MediaBox")?.unwrap_or(DEFAULT_MEDIA_BOX))
}

/// The first `[x0, y0, x1, y1]` rectangle found for `key` (`b"MediaBox"`,
/// `b"CropBox"`, ...) on the page dictionary or its `/Pages` ancestors.
///
/// Returns `None` when no ancestor defines the key or every entry found
/// is malformed — a malformed box is skipped rather than fatal, matching
/// how lenient viewers treat broken page trees. Shared by `crop` and
/// `resize`; the `/Parent` walk is depth-capped at 32 so a cyclic page
/// tree cannot loop forever.
pub(crate) fn inherited_rect(
    doc: &Document,
    page_id: ObjectId,
    key: &[u8],
) -> PdfResult<Option<[f64; 4]>> {
    let mut current = Some(page_id);
    for _ in 0..32 {
        let Some(id) = current else { break };
        let dict = doc.get_dictionary(id).map_err(lopdf_err)?;
        if let Ok(box_obj) = dict.get(key)
            && let Ok(rect) = media_box_array(box_obj)
        {
            return Ok(Some(rect));
        }
        current = dict.get(b"Parent").ok().and_then(|p| p.as_reference().ok());
    }
    Ok(None)
}

/// Parse a `[x0, y0, x1, y1]`-shaped array of four numbers.
fn media_box_array(obj: &Object) -> PdfResult<[f64; 4]> {
    let array = obj
        .as_array()
        .map_err(|_| PdfError::InvalidInput("page box is not an array".to_string()))?;
    if array.len() != 4 {
        return Err(PdfError::InvalidInput(format!(
            "page box must have 4 numbers, got {}",
            array.len()
        )));
    }
    let mut out = [0.0; 4];
    for (i, item) in array.iter().enumerate() {
        out[i] = match item {
            Object::Integer(v) => *v as f64,
            Object::Real(v) => f64::from(*v),
            _ => {
                return Err(PdfError::InvalidInput(
                    "page box contains a non-numeric entry".to_string(),
                ));
            }
        };
    }
    Ok(out)
}

/// A PDF number object: `Integer` when the value is whole and fits,
/// `Real` otherwise.
pub(crate) fn number_object(v: f64) -> Object {
    if v.fract() == 0.0 && v >= i64::MIN as f64 && v <= i64::MAX as f64 {
        Object::Integer(v as i64)
    } else {
        Object::Real(v as f32)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::crates::pdf_engine::ops::test_utils::pdf_with_pages;
    use lopdf::Document;

    fn crop_box_of(pdf: &[u8], page: u32) -> Vec<f64> {
        let doc = Document::load_mem(pdf).unwrap();
        let page_id = doc.get_pages()[&page];
        let dict = doc.get_dictionary(page_id).unwrap();
        dict.get(b"CropBox")
            .unwrap()
            .as_array()
            .unwrap()
            .iter()
            .map(|o| match o {
                Object::Integer(v) => *v as f64,
                Object::Real(v) => f64::from(*v),
                _ => panic!("non-numeric CropBox entry"),
            })
            .collect()
    }

    #[test]
    fn sets_crop_box() {
        // test_utils pages have MediaBox [0 0 595 842].
        let pdf = pdf_with_pages(&["one", "two"]);
        let out = crop(
            &pdf,
            "1",
            [10.0, 20.0, 300.0, 400.0],
            &EngineLimits::default(),
        )
        .unwrap();
        let doc = Document::load_mem(&out).unwrap();
        assert_eq!(doc.get_pages().len(), 2);
        assert_eq!(crop_box_of(&out, 1), vec![10.0, 20.0, 300.0, 400.0]);
        // Page 2 is untouched: it has no CropBox.
        let page2 = doc.get_pages()[&2];
        assert!(doc.get_dictionary(page2).unwrap().get(b"CropBox").is_err());
    }

    #[test]
    fn supports_fractional_box() {
        let pdf = pdf_with_pages(&["one"]);
        let out = crop(
            &pdf,
            "all",
            [0.5, 0.25, 100.75, 200.5],
            &EngineLimits::default(),
        )
        .unwrap();
        let got = crop_box_of(&out, 1);
        assert!((got[0] - 0.5).abs() < 1e-4);
        assert!((got[2] - 100.75).abs() < 1e-3);
    }

    #[test]
    fn rejects_out_of_bounds_box() {
        let pdf = pdf_with_pages(&["one"]);
        // Entirely outside MediaBox [0 0 595 842].
        let err = crop(
            &pdf,
            "1",
            [1000.0, 1000.0, 1100.0, 1100.0],
            &EngineLimits::default(),
        )
        .unwrap_err();
        assert!(matches!(err, PdfError::InvalidInput(_)), "got {err:?}");
    }

    #[test]
    fn rejects_malformed_boxes() {
        let pdf = pdf_with_pages(&["one"]);
        for bx in [
            [-1.0, 0.0, 100.0, 100.0], // negative origin
            [0.0, -5.0, 100.0, 100.0],
            [100.0, 0.0, 50.0, 100.0],  // x1 <= x0
            [0.0, 100.0, 100.0, 100.0], // y1 <= y0
            [0.0, 0.0, f32::NAN, 100.0],
        ] {
            assert!(
                crop(&pdf, "1", bx, &EngineLimits::default()).is_err(),
                "box {bx:?} should fail"
            );
        }
        assert!(crop(&pdf, "9", [0.0, 0.0, 10.0, 10.0], &EngineLimits::default()).is_err());
        assert!(
            crop(
                b"junk",
                "1",
                [0.0, 0.0, 10.0, 10.0],
                &EngineLimits::default()
            )
            .is_err()
        );
    }
}
