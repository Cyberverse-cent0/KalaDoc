//! View-level cropping: `/CropBox` plus a real content-stream clip.
//!
//! [`super::crop::crop`] sets `/CropBox` only — the standard, minimal
//! way to crop. Some viewers and downstream consumers still render
//! content that lies outside the crop box (annotations, optional
//! content, or viewers that ignore `/CropBox` for certain operators),
//! so [`set_view_crop`] additionally prepends a clipping path
//! (`x0 y0 w h re W n`) to each selected page's content stream. The
//! clip intersects the page's clipping path for everything drawn
//! afterwards, so out-of-crop content is genuinely hidden, not just
//! outside the visible window.
//!
//! [`clear_crop`] removes the page-level `/CropBox` entry, reverting
//! the page to its `/MediaBox` (or an inherited `/CropBox` from a
//! `/Pages` ancestor). It does **not** remove clip paths previously
//! injected by [`set_view_crop`] — content-stream operators cannot be
//! reliably excised — so pages cropped with `set_view_crop` stay
//! visually clipped even after `clear_crop`.

use lopdf::Object;

use crate::crates::core::engine::EngineLimits;

use crate::crates::pdf_engine::error::{PdfError, PdfResult};

use super::crop::{DEFAULT_MEDIA_BOX, inherited_rect, number_object};
use super::load::{load_pdf, lopdf_err, reject_encrypted, save_pdf};
use super::pages::parse_page_range;
use super::watermark::{add_content_stream, wrap_page_content};

/// Set `/CropBox` to `rect` (`[x0, y0, x1, y1]` in PDF points) on the
/// pages selected by `pages`, and prepend a `re W n` clipping path to
/// each page's content stream so content outside the box is actually
/// hidden.
///
/// Validation mirrors [`super::crop::crop`]: the box must be finite,
/// have a non-negative origin and positive extent, and intersect each
/// selected page's effective `/MediaBox`. The clip path is emitted in
/// absolute page coordinates (the same space `/CropBox` uses), before
/// any existing content — it is not wrapped in `q`/`Q`, so it applies
/// to the whole page.
///
/// Returns [`PdfError::Encrypted`] for encrypted input.
pub fn set_view_crop(
    input: &[u8],
    pages: &str,
    rect: [f64; 4],
    limits: &EngineLimits,
) -> PdfResult<Vec<u8>> {
    let [x0, y0, x1, y1] = rect;
    if ![x0, y0, x1, y1].iter().all(|v| v.is_finite()) {
        return Err(PdfError::InvalidInput(
            "crop rect coordinates must be finite numbers".to_string(),
        ));
    }
    if x0 < 0.0 || y0 < 0.0 {
        return Err(PdfError::InvalidInput(
            "crop rect origin must be non-negative".to_string(),
        ));
    }
    if x1 <= x0 || y1 <= y0 {
        return Err(PdfError::InvalidInput(
            "crop rect must have positive width and height (x1 > x0, y1 > y0)".to_string(),
        ));
    }

    let mut doc = load_pdf(input, limits)?;
    reject_encrypted(&doc)?;

    let page_count = doc.get_pages().len() as u32;
    let selected = parse_page_range(pages, page_count)?;
    let pages_map = doc.get_pages();

    for page_number in selected {
        let Some(page_id) = pages_map.get(&page_number).copied() else {
            return Err(PdfError::InvalidPageRange(format!(
                "page {page_number} not found in document"
            )));
        };
        let media = inherited_rect(&doc, page_id, b"MediaBox")?.unwrap_or(DEFAULT_MEDIA_BOX);
        let intersects = x1 > media[0] && x0 < media[2] && y1 > media[1] && y0 < media[3];
        if !intersects {
            return Err(PdfError::InvalidInput(format!(
                "crop rect [{x0}, {y0}, {x1}, {y1}] does not intersect page {page_number} media box [{}, {}, {}, {}]",
                media[0], media[1], media[2], media[3]
            )));
        }

        // 1. /CropBox — the standard viewer-level crop.
        let crop_objects: Vec<Object> =
            [x0, y0, x1, y1].iter().map(|&v| number_object(v)).collect();
        doc.get_dictionary_mut(page_id)
            .map_err(lopdf_err)?
            .set("CropBox", Object::Array(crop_objects));

        // 2. Content-stream clip — hides out-of-crop painting for
        //    viewers/consumers that honor the clipping path.
        let width = x1 - x0;
        let height = y1 - y0;
        // Raw bytes keep full f64 precision (Object::Real is f32).
        let clip = format!("{x0} {y0} {width} {height} re W n\n");
        let stream_id = add_content_stream(&mut doc, clip.into_bytes())?;
        // Prepend: the clip must precede existing operators.
        wrap_page_content(&mut doc, page_id, stream_id)?;
    }

    doc.compress();
    save_pdf(&mut doc)
}

/// Remove the page-level `/CropBox` from the pages selected by
/// `pages`, reverting each to its `/MediaBox` (or to an inherited
/// `/CropBox` from a `/Pages` ancestor, if one exists).
///
/// Only the dictionary entry is removed; content streams are not
/// touched, so clip paths injected by [`set_view_crop`] remain in
/// effect. Returns [`PdfError::Encrypted`] for encrypted input.
pub fn clear_crop(input: &[u8], pages: &str, limits: &EngineLimits) -> PdfResult<Vec<u8>> {
    let mut doc = load_pdf(input, limits)?;
    reject_encrypted(&doc)?;

    let page_count = doc.get_pages().len() as u32;
    let selected = parse_page_range(pages, page_count)?;
    let pages_map = doc.get_pages();

    for page_number in selected {
        let Some(page_id) = pages_map.get(&page_number).copied() else {
            return Err(PdfError::InvalidPageRange(format!(
                "page {page_number} not found in document"
            )));
        };
        let dict = doc.get_dictionary_mut(page_id).map_err(lopdf_err)?;
        // Removing a missing key is a no-op (lopdf returns Err).
        let _ = dict.remove(b"CropBox");
    }

    save_pdf(&mut doc)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::crates::pdf_engine::ops::encrypt::{PdfPermissions, encrypt};
    use crate::crates::pdf_engine::ops::test_utils::{page_count, pdf_with_pages};
    use lopdf::Document;

    fn crop_box_of(pdf: &[u8], page: u32) -> Option<Vec<f64>> {
        let doc = Document::load_mem(pdf).unwrap();
        let page_id = doc.get_pages()[&page];
        let dict = doc.get_dictionary(page_id).unwrap();
        dict.get(b"CropBox").ok().map(|o| {
            o.as_array()
                .unwrap()
                .iter()
                .map(|v| match v {
                    Object::Integer(i) => *i as f64,
                    Object::Real(r) => f64::from(*r),
                    _ => panic!("non-numeric CropBox entry"),
                })
                .collect()
        })
    }

    fn content_text(pdf: &[u8], page: u32) -> String {
        let doc = Document::load_mem(pdf).unwrap();
        let page_id = doc.get_pages()[&page];
        let bytes = doc.get_page_content(page_id);
        String::from_utf8_lossy(&bytes).into_owned()
    }

    #[test]
    fn sets_crop_box_and_clip() {
        let pdf = pdf_with_pages(&["body text"]);
        let out = set_view_crop(
            &pdf,
            "1",
            [10.0, 20.0, 300.0, 400.0],
            &EngineLimits::default(),
        )
        .unwrap();
        assert_eq!(page_count(&out), 1);
        assert_eq!(crop_box_of(&out, 1), Some(vec![10.0, 20.0, 300.0, 400.0]));
        let text = content_text(&out, 1);
        // Clip precedes the original content.
        let clip_pos = text.find("re W n").expect("clip path present");
        let orig_pos = text.find("(body text)").expect("original content present");
        assert!(clip_pos < orig_pos, "clip must precede page content");
        assert!(
            text.contains("10 20 290 380"),
            "clip rect in content: {text}"
        );
    }

    #[test]
    fn respects_page_range() {
        let pdf = pdf_with_pages(&["one", "two", "three"]);
        let out = set_view_crop(
            &pdf,
            "2",
            [0.0, 0.0, 100.0, 100.0],
            &EngineLimits::default(),
        )
        .unwrap();
        assert_eq!(crop_box_of(&out, 1), None);
        assert_eq!(crop_box_of(&out, 2), Some(vec![0.0, 0.0, 100.0, 100.0]));
        assert_eq!(crop_box_of(&out, 3), None);
        // Page 1's content must not have gained a clip.
        assert!(!content_text(&out, 1).contains("re W n"));
    }

    #[test]
    fn rejects_bad_rects() {
        let pdf = pdf_with_pages(&["x"]);
        for rect in [
            [f64::NAN, 0.0, 10.0, 10.0],
            [-1.0, 0.0, 10.0, 10.0],
            [0.0, 0.0, 0.0, 10.0],            // zero width
            [0.0, 50.0, 10.0, 10.0],          // y1 <= y0
            [1000.0, 1000.0, 1100.0, 1100.0], // outside MediaBox
        ] {
            assert!(
                set_view_crop(&pdf, "1", rect, &EngineLimits::default()).is_err(),
                "rect {rect:?} should fail"
            );
        }
        assert!(
            set_view_crop(&pdf, "9", [0.0, 0.0, 10.0, 10.0], &EngineLimits::default()).is_err()
        );
        assert!(
            set_view_crop(
                b"junk",
                "1",
                [0.0, 0.0, 10.0, 10.0],
                &EngineLimits::default()
            )
            .is_err()
        );
    }

    #[test]
    fn clear_crop_removes_box_only() {
        let pdf = pdf_with_pages(&["keep me"]);
        let cropped = set_view_crop(
            &pdf,
            "1",
            [0.0, 0.0, 200.0, 200.0],
            &EngineLimits::default(),
        )
        .unwrap();
        let cleared = clear_crop(&cropped, "1", &EngineLimits::default()).unwrap();
        assert_eq!(page_count(&cleared), 1);
        assert_eq!(crop_box_of(&cleared, 1), None);
        // The injected clip remains — documented limitation.
        assert!(content_text(&cleared, 1).contains("re W n"));
        // Original content survives.
        assert!(content_text(&cleared, 1).contains("(keep me)"));
        // Clearing a page with no CropBox is a harmless no-op.
        let plain = pdf_with_pages(&["plain"]);
        let out = clear_crop(&plain, "all", &EngineLimits::default()).unwrap();
        assert_eq!(page_count(&out), 1);
    }

    #[test]
    fn transforms_reject_encrypted() {
        let pdf = pdf_with_pages(&["a"]);
        let enc = encrypt(
            &pdf,
            "user",
            None,
            PdfPermissions::default(),
            &EngineLimits::default(),
        )
        .unwrap();
        assert!(
            set_view_crop(&enc, "1", [0.0, 0.0, 10.0, 10.0], &EngineLimits::default()).is_err()
        );
        assert!(clear_crop(&enc, "1", &EngineLimits::default()).is_err());
    }
}
