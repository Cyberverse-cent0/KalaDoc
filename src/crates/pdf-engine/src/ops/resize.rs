//! Resize the page boxes (`/MediaBox`, and `/CropBox` when present) of
//! selected pages.
//!
//! This is a **page-box** operation, not a raster resize: the page's
//! coordinate-system box is rewritten, but the content stream is *not*
//! transformed. Existing content keeps its user-space coordinates, so
//! after a resize it may appear smaller/larger relative to the page,
//! gain whitespace, or be clipped by the new box edges. Callers that
//! need the rendered output scaled must use the `render` op instead.
//!
//! Semantics, per selected page:
//!
//! - The effective `/MediaBox` (page's own entry, else inherited from
//!   `/Pages` ancestors, else US Letter) is rescaled about its
//!   **lower-left corner**: the new box is
//!   `[x0, y0, x0 + w·sx, y0 + h·sy]` where `w`/`h` are the old
//!   extents and `sx`/`sy` the per-axis scale factors.
//! - `ResizeSpec::Scale(s)` applies `sx = sy = s` (uniform).
//!   `ResizeSpec::Size { width_pt, height_pt }` sets explicit target
//!   extents, allowing non-uniform scaling of the box.
//! - If the page has an effective `/CropBox` (own or inherited), it is
//!   mapped through the same per-axis transform relative to the old
//!   MediaBox and written onto the page, preserving the relative crop
//!   region. Pages without a CropBox do not gain one.
//! - `/MediaBox` is always written onto the page dictionary itself, so
//!   pages that previously inherited it keep siblings unchanged.
//!
//! Resulting extents must stay positive, finite and at most
//! [`MAX_PAGE_BOX_PT`] (14400 pt ≈ 200 in) per axis — larger requests
//! are rejected as [`PdfError::InvalidInput`].

use lopdf::Object;

use crate::crates::core::engine::EngineLimits;

use crate::crates::pdf_engine::error::{PdfError, PdfResult};

use super::crop::{DEFAULT_MEDIA_BOX, inherited_rect, number_object};
use super::load::{load_pdf, lopdf_err, reject_encrypted, save_pdf};
use super::pages::parse_page_range;

/// Maximum allowed page-box extent in points (14400 pt ≈ 200 inches).
/// Guards against absurd scale factors or explicit sizes.
const MAX_PAGE_BOX_PT: f64 = 14400.0;

/// How to resize each selected page's box.
#[derive(Debug, Clone, Copy, PartialEq)]
pub enum ResizeSpec {
    /// Uniform scale factor applied to the MediaBox extents
    /// (`sx = sy = s`). Must be finite and positive.
    Scale(f64),
    /// Explicit target MediaBox size in points. `width_pt` and
    /// `height_pt` may differ, giving non-uniform box scaling; both
    /// must be finite, positive and at most 14400.
    Size { width_pt: f64, height_pt: f64 },
}

/// Resize the page boxes of the pages selected by `pages` (a range
/// spec such as `"1-3,8"` or `"all"`, 1-based) according to `spec`.
///
/// See the module docs for the exact box semantics: the MediaBox is
/// rescaled about its lower-left corner and a present CropBox is
/// transformed consistently; page content is **not** scaled.
///
/// Fails with [`PdfError::InvalidInput`] for non-positive, non-finite
/// or oversized parameters, and with [`PdfError::InvalidPageRange`]
/// for malformed page selections (via [`parse_page_range`]).
pub fn resize(
    input: &[u8],
    pages: &str,
    spec: &ResizeSpec,
    limits: &EngineLimits,
) -> PdfResult<Vec<u8>> {
    // Validate the spec before touching the document so a bad
    // parameter fails fast regardless of input size.
    match *spec {
        ResizeSpec::Scale(s) => {
            if !s.is_finite() || s <= 0.0 {
                return Err(PdfError::InvalidInput(format!(
                    "scale must be a positive finite number, got {s}"
                )));
            }
        }
        ResizeSpec::Size {
            width_pt,
            height_pt,
        } => {
            for (name, v) in [("width_pt", width_pt), ("height_pt", height_pt)] {
                if !v.is_finite() || v <= 0.0 {
                    return Err(PdfError::InvalidInput(format!(
                        "{name} must be a positive finite number, got {v}"
                    )));
                }
                if v > MAX_PAGE_BOX_PT {
                    return Err(PdfError::InvalidInput(format!(
                        "{name} {v} exceeds maximum page-box extent {MAX_PAGE_BOX_PT} pt"
                    )));
                }
            }
        }
    }

    let mut doc = load_pdf(input, limits)?;
    reject_encrypted(&doc)?;

    let page_count = doc.get_pages().len() as u32;
    let selected = parse_page_range(pages, page_count)?;
    let pages_map = doc.get_pages();

    for page_number in selected {
        let page_id = pages_map[&page_number];

        let media = inherited_rect(&doc, page_id, b"MediaBox")?.unwrap_or(DEFAULT_MEDIA_BOX);
        let w = media[2] - media[0];
        let h = media[3] - media[1];
        if !w.is_finite() || !h.is_finite() || w <= 0.0 || h <= 0.0 {
            return Err(PdfError::InvalidInput(format!(
                "page {page_number} has a malformed /MediaBox {media:?}"
            )));
        }

        // Per-axis scale factors relative to the old MediaBox.
        let (sx, sy) = match *spec {
            ResizeSpec::Scale(s) => (s, s),
            ResizeSpec::Size {
                width_pt,
                height_pt,
            } => (width_pt / w, height_pt / h),
        };
        let new_w = w * sx;
        let new_h = h * sy;
        if !new_w.is_finite()
            || !new_h.is_finite()
            || new_w <= 0.0
            || new_h <= 0.0
            || new_w > MAX_PAGE_BOX_PT
            || new_h > MAX_PAGE_BOX_PT
        {
            return Err(PdfError::InvalidInput(format!(
                "resized page {page_number} box would be {new_w} x {new_h} pt, \
                 exceeding the {MAX_PAGE_BOX_PT} pt limit"
            )));
        }

        // Rescale about the MediaBox's lower-left corner.
        let new_media = [media[0], media[1], media[0] + new_w, media[1] + new_h];

        // An effective CropBox (own or inherited) is mapped through the
        // same transform so the relative crop region is preserved.
        let new_crop = inherited_rect(&doc, page_id, b"CropBox")?.map(|crop| {
            [
                media[0] + (crop[0] - media[0]) * sx,
                media[1] + (crop[1] - media[1]) * sy,
                media[0] + (crop[2] - media[0]) * sx,
                media[1] + (crop[3] - media[1]) * sy,
            ]
        });

        let dict = doc.get_dictionary_mut(page_id).map_err(lopdf_err)?;
        let media_objects: Vec<Object> = new_media.iter().map(|&v| number_object(v)).collect();
        dict.set("MediaBox", Object::Array(media_objects));
        if let Some(crop) = new_crop {
            let crop_objects: Vec<Object> = crop.iter().map(|&v| number_object(v)).collect();
            dict.set("CropBox", Object::Array(crop_objects));
        }
    }

    save_pdf(&mut doc)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::crates::pdf_engine::ops::crop::crop;
    use crate::crates::pdf_engine::ops::test_utils::{
        page_count as test_page_count, pdf_with_pages,
    };
    use lopdf::Document;

    /// Read a `[x0, y0, x1, y1]` entry (`MediaBox`/`CropBox`) off a page.
    fn box_of(pdf: &[u8], page: u32, key: &[u8]) -> Option<Vec<f64>> {
        let doc = Document::load_mem(pdf).unwrap();
        let page_id = doc.get_pages()[&page];
        let dict = doc.get_dictionary(page_id).unwrap();
        dict.get(key).ok()?.as_array().ok().map(|arr| {
            arr.iter()
                .map(|o| match o {
                    Object::Integer(v) => *v as f64,
                    Object::Real(v) => f64::from(*v),
                    _ => panic!("non-numeric box entry"),
                })
                .collect()
        })
    }

    fn approx_eq(got: &[f64], want: &[f64]) {
        assert_eq!(got.len(), want.len());
        for (g, w) in got.iter().zip(want) {
            assert!((g - w).abs() < 1e-3, "got {got:?}, want {want:?}");
        }
    }

    #[test]
    fn uniform_scale_doubles_media_box() {
        // test_utils pages have MediaBox [0 0 595 842].
        let pdf = pdf_with_pages(&["one", "two"]);
        let out = resize(
            &pdf,
            "all",
            &ResizeSpec::Scale(2.0),
            &EngineLimits::default(),
        )
        .unwrap();
        assert_eq!(test_page_count(&out), 2);
        approx_eq(
            &box_of(&out, 1, b"MediaBox").unwrap(),
            &[0.0, 0.0, 1190.0, 1684.0],
        );
        approx_eq(
            &box_of(&out, 2, b"MediaBox").unwrap(),
            &[0.0, 0.0, 1190.0, 1684.0],
        );
    }

    #[test]
    fn explicit_size_is_non_uniform() {
        let pdf = pdf_with_pages(&["one"]);
        let out = resize(
            &pdf,
            "1",
            &ResizeSpec::Size {
                width_pt: 300.0,
                height_pt: 400.0,
            },
            &EngineLimits::default(),
        )
        .unwrap();
        approx_eq(
            &box_of(&out, 1, b"MediaBox").unwrap(),
            &[0.0, 0.0, 300.0, 400.0],
        );
    }

    #[test]
    fn only_selected_pages_resize() {
        let pdf = pdf_with_pages(&["one", "two", "three"]);
        let out = resize(&pdf, "2", &ResizeSpec::Scale(0.5), &EngineLimits::default()).unwrap();
        approx_eq(
            &box_of(&out, 1, b"MediaBox").unwrap(),
            &[0.0, 0.0, 595.0, 842.0],
        );
        approx_eq(
            &box_of(&out, 2, b"MediaBox").unwrap(),
            &[0.0, 0.0, 297.5, 421.0],
        );
        approx_eq(
            &box_of(&out, 3, b"MediaBox").unwrap(),
            &[0.0, 0.0, 595.0, 842.0],
        );
    }

    #[test]
    fn crop_box_is_transformed_consistently() {
        // Crop page 1 to [10 20 300 400], then scale by 2: the CropBox
        // must scale about the MediaBox origin like the MediaBox does.
        let pdf = pdf_with_pages(&["one"]);
        let cropped = crop(
            &pdf,
            "1",
            [10.0, 20.0, 300.0, 400.0],
            &EngineLimits::default(),
        )
        .unwrap();
        let out = resize(
            &cropped,
            "1",
            &ResizeSpec::Scale(2.0),
            &EngineLimits::default(),
        )
        .unwrap();
        approx_eq(
            &box_of(&out, 1, b"MediaBox").unwrap(),
            &[0.0, 0.0, 1190.0, 1684.0],
        );
        approx_eq(
            &box_of(&out, 1, b"CropBox").unwrap(),
            &[20.0, 40.0, 600.0, 800.0],
        );
    }

    #[test]
    fn pages_without_crop_box_gain_none() {
        let pdf = pdf_with_pages(&["one"]);
        let out = resize(&pdf, "1", &ResizeSpec::Scale(2.0), &EngineLimits::default()).unwrap();
        assert!(box_of(&out, 1, b"CropBox").is_none());
    }

    #[test]
    fn rejects_invalid_specs() {
        let pdf = pdf_with_pages(&["one"]);
        for spec in [
            ResizeSpec::Scale(0.0),
            ResizeSpec::Scale(-1.0),
            ResizeSpec::Scale(f64::NAN),
            ResizeSpec::Scale(f64::INFINITY),
            ResizeSpec::Size {
                width_pt: 0.0,
                height_pt: 100.0,
            },
            ResizeSpec::Size {
                width_pt: 100.0,
                height_pt: -5.0,
            },
            ResizeSpec::Size {
                width_pt: f64::NAN,
                height_pt: 100.0,
            },
            // Over the 14400 pt cap.
            ResizeSpec::Size {
                width_pt: 20000.0,
                height_pt: 100.0,
            },
            // Uniform scale that would exceed the cap (595 * 30 > 14400).
            ResizeSpec::Scale(30.0),
        ] {
            assert!(
                resize(&pdf, "1", &spec, &EngineLimits::default()).is_err(),
                "spec {spec:?} should fail"
            );
        }
    }

    #[test]
    fn rejects_bad_input() {
        let pdf = pdf_with_pages(&["one", "two"]);
        assert!(resize(&pdf, "9", &ResizeSpec::Scale(2.0), &EngineLimits::default()).is_err());
        assert!(resize(&pdf, "", &ResizeSpec::Scale(2.0), &EngineLimits::default()).is_err());
        assert!(
            resize(
                b"junk",
                "1",
                &ResizeSpec::Scale(2.0),
                &EngineLimits::default()
            )
            .is_err()
        );
    }

    #[test]
    fn enforces_page_limit() {
        let pdf = pdf_with_pages(&["one", "two", "three"]);
        let limits = EngineLimits {
            max_pages: Some(2),
            ..EngineLimits::default()
        };
        assert!(resize(&pdf, "all", &ResizeSpec::Scale(2.0), &limits).is_err());
    }
}
