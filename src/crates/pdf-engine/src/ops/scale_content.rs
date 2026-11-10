//! Scale a page's *content* — the honest counterpart to [`resize`][super::resize::resize].
//!
//! [`resize`](super::resize::resize) only rewrites the page boxes
//! (`/MediaBox`, `/CropBox`); the content stream keeps its original
//! user-space coordinates, so painted content does not actually grow or
//! shrink. `scale_content` fixes that by wrapping each selected page's
//! existing `/Contents` in a PDF graphics-state save/restore pair:
//!
//! ```text
//! /Contents [ <opener> <existing streams...> <closer> ]
//! opener : q  sx 0 0 sy tx ty  cm
//! closer : Q
//! ```
//!
//! `/Contents` streams are painted in array order, so the opener runs
//! first: `q` pushes the graphics state, `cm` concatenates the scale
//! matrix onto the CTM, every existing stream then paints through the
//! scaled coordinate system, and the trailing `Q` restores the state.
//! No existing stream is decoded, decompressed or rewritten — the
//! original objects stay in the file untouched and are simply
//! referenced inside the new array (a page whose `/Contents` was a
//! single stream reference becomes a three-element array; an existing
//! array keeps all of its elements between the wrappers).
//!
//! Transform semantics:
//!
//! - Content coordinates are scaled **about the page's effective
//!   `/MediaBox` lower-left corner** `(m0, m1)`, matching how
//!   [`resize`](super::resize::resize) rescales the boxes. The `cm`
//!   matrix is therefore `[sx 0 0 sy tx ty]` with
//!   `tx = m0·(1−sx)`, `ty = m1·(1−sy)` — a bare `sx 0 0 sy 0 0 cm`
//!   would scale about the user-space origin `(0,0)`, which differs
//!   whenever the MediaBox does not start at the origin.
//! - The effective `/MediaBox` is rewritten to
//!   `[m0, m1, m0 + w·sx, m1 + h·sy]` (own entry written onto the page
//!   dictionary even when inherited, like `resize`).
//! - An effective `/CropBox` (own or inherited) is mapped through the
//!   same per-axis transform relative to the MediaBox and written onto
//!   the page; pages without one gain none.
//! - `/Rotate` is a viewer hint applied after the content transform and
//!   needs no change — rotation and scaling compose correctly.
//!
//! Limitations: the wrapper assumes existing content keeps balanced
//! `q`/`Q` pairs (true for all conforming PDFs); a stream with unbalanced
//! graphics-state operators could interact with the outer `Q`, same as
//! any viewer would. Pages with no `/Contents` at all are skipped for
//! wrapping but still get rescaled boxes. `/UserUnit` scaling is applied
//! by viewers on top of this and is ignored here.
//!
//! `scale_y = None` means uniform scaling (`sy = sx`). Both factors
//! must be finite and within `0.01..=100`; violations are
//! [`PdfError::InvalidInput`]. Encrypted input is rejected.

use lopdf::Object;

use crate::crates::core::engine::EngineLimits;

use crate::crates::pdf_engine::error::{PdfError, PdfResult};

use super::crop::{DEFAULT_MEDIA_BOX, inherited_rect, number_object};
use super::load::{load_pdf, lopdf_err, reject_encrypted, save_pdf};
use super::pages::parse_page_range;
use super::watermark::add_content_stream;

/// Inclusive bounds for accepted scale factors.
const MIN_SCALE: f64 = 0.01;
const MAX_SCALE: f64 = 100.0;

/// Format an `f64` as a PDF number literal. Rust's `Display` never uses
/// scientific notation, and whole values are emitted without a
/// fractional part for readability.
fn pdf_num(v: f64) -> String {
    if v.fract() == 0.0 && v >= i64::MIN as f64 && v <= i64::MAX as f64 {
        format!("{}", v as i64)
    } else {
        format!("{v}")
    }
}

/// Validate one scale factor; `name` is used in the error message.
fn check_scale(name: &str, v: f64) -> PdfResult<()> {
    if !v.is_finite() || !(MIN_SCALE..=MAX_SCALE).contains(&v) {
        return Err(PdfError::InvalidInput(format!(
            "{name} must be a finite number in [{MIN_SCALE}, {MAX_SCALE}], got {v}"
        )));
    }
    Ok(())
}

/// The page's existing `/Contents` as a list of objects to keep inside
/// the new array — array elements are preserved individually, any other
/// single value (reference, inline stream) is kept as one element.
/// `None` when the page has no `/Contents` entry at all.
fn existing_contents(page: &lopdf::Dictionary) -> Option<Vec<Object>> {
    match page.get(b"Contents") {
        Ok(Object::Array(items)) => Some(items.clone()),
        Ok(obj) => Some(vec![obj.clone()]),
        Err(_) => None,
    }
}

/// Scale the content and page boxes of the pages selected by `pages`
/// (a range spec such as `"1-3,8"` or `"all"`, 1-based).
///
/// See the module docs for the exact transform: content is wrapped in
/// `q … cm` / `Q` streams and the boxes are rescaled about the MediaBox
/// lower-left corner so the visible area matches the painted content.
///
/// Fails with [`PdfError::InvalidInput`] for out-of-range scale factors
/// or malformed page boxes, and [`PdfError::InvalidPageRange`] via
/// [`parse_page_range`] for bad selections.
pub fn scale_content(
    input: &[u8],
    pages: &str,
    scale_x: f64,
    scale_y: Option<f64>,
    limits: &EngineLimits,
) -> PdfResult<Vec<u8>> {
    let sy = scale_y.unwrap_or(scale_x);
    check_scale("scale_x", scale_x)?;
    check_scale("scale_y", sy)?;

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
        let (sx, sy) = (scale_x, sy);
        let new_w = w * sx;
        let new_h = h * sy;
        if !new_w.is_finite() || !new_h.is_finite() || new_w <= 0.0 || new_h <= 0.0 {
            return Err(PdfError::InvalidInput(format!(
                "scaled page {page_number} box would be {new_w} x {new_h} pt"
            )));
        }

        // Same box math as `resize`: rescale about the MediaBox's
        // lower-left corner.
        let new_media = [media[0], media[1], media[0] + new_w, media[1] + new_h];
        let new_crop = inherited_rect(&doc, page_id, b"CropBox")?.map(|crop| {
            [
                media[0] + (crop[0] - media[0]) * sx,
                media[1] + (crop[1] - media[1]) * sy,
                media[0] + (crop[2] - media[0]) * sx,
                media[1] + (crop[3] - media[1]) * sy,
            ]
        });

        // Wrap existing content: `q sx 0 0 sy tx ty cm` … `Q`, where the
        // translation anchors the scale at the MediaBox origin.
        let existing = {
            let page = doc.get_dictionary(page_id).map_err(lopdf_err)?;
            existing_contents(page)
        };
        if let Some(mut contents) = existing {
            let tx = media[0] * (1.0 - sx);
            let ty = media[1] * (1.0 - sy);
            let opener = format!(
                "q\n{} 0 0 {} {} {} cm\n",
                pdf_num(sx),
                pdf_num(sy),
                pdf_num(tx),
                pdf_num(ty)
            );
            let opener_id = add_content_stream(&mut doc, opener.into_bytes())?;
            let closer_id = add_content_stream(&mut doc, b"Q\n".to_vec())?;

            let mut array = Vec::with_capacity(contents.len() + 2);
            array.push(Object::Reference(opener_id));
            array.append(&mut contents);
            array.push(Object::Reference(closer_id));

            doc.get_dictionary_mut(page_id)
                .map_err(lopdf_err)?
                .set("Contents", Object::Array(array));
        }

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
    use crate::crates::pdf_engine::ops::test_utils::{
        page_count as test_page_count, pdf_with_pages,
    };
    use lopdf::{Document, Object};

    /// `[x0, y0, x1, y1]` of a page's MediaBox.
    fn media_box_of(pdf: &[u8], page: u32) -> Vec<f64> {
        let doc = Document::load_mem(pdf).unwrap();
        let page_id = doc.get_pages()[&page];
        doc.get_dictionary(page_id)
            .unwrap()
            .get(b"MediaBox")
            .unwrap()
            .as_array()
            .unwrap()
            .iter()
            .map(|o| match o {
                Object::Integer(v) => *v as f64,
                Object::Real(v) => f64::from(*v),
                _ => panic!("non-numeric MediaBox entry"),
            })
            .collect()
    }

    fn approx_eq(got: &[f64], want: &[f64]) {
        assert_eq!(got.len(), want.len());
        for (g, w) in got.iter().zip(want) {
            assert!((g - w).abs() < 1e-3, "got {got:?}, want {want:?}");
        }
    }

    /// Decoded bytes of the stream at `id`.
    fn stream_bytes(doc: &Document, id: lopdf::ObjectId) -> Vec<u8> {
        let obj = doc.get_object(id).unwrap();
        let stream = obj.as_stream().unwrap();
        stream.decompressed_content().unwrap()
    }

    /// The `/Contents` array of a page as a list of object ids.
    fn contents_ids(pdf: &[u8], page: u32) -> Vec<lopdf::ObjectId> {
        let doc = Document::load_mem(pdf).unwrap();
        let page_id = doc.get_pages()[&page];
        let contents = doc
            .get_dictionary(page_id)
            .unwrap()
            .get(b"Contents")
            .unwrap()
            .as_array()
            .unwrap()
            .clone();
        contents.iter().map(|o| o.as_reference().unwrap()).collect()
    }

    #[test]
    fn wraps_content_and_scales_media_box() {
        // test_utils MediaBox is [0 0 595 842], single stream contents.
        let pdf = pdf_with_pages(&["hello"]);
        let out = scale_content(&pdf, "1", 2.0, None, &EngineLimits::default()).unwrap();
        assert_eq!(test_page_count(&out), 1);
        approx_eq(&media_box_of(&out, 1), &[0.0, 0.0, 1190.0, 1684.0]);

        let ids = contents_ids(&out, 1);
        assert_eq!(ids.len(), 3, "opener + original + closer");

        let doc = Document::load_mem(&out).unwrap();
        let opener = String::from_utf8(stream_bytes(&doc, ids[0])).unwrap();
        assert!(opener.contains("q\n"), "opener starts with q: {opener:?}");
        assert!(
            opener.contains("2 0 0 2 0 0 cm"),
            "uniform cm matrix: {opener:?}"
        );
        let closer = stream_bytes(&doc, ids[2]);
        assert_eq!(closer, b"Q\n".to_vec());
        // Middle stream is the untouched original content.
        let middle = stream_bytes(&doc, ids[1]);
        assert!(middle.windows(4).any(|w| w == b"BT\n".as_slice()) || middle.starts_with(b"BT"));
    }

    #[test]
    fn non_uniform_scale_and_box_origin_translation() {
        let pdf = pdf_with_pages(&["x"]);
        let out = scale_content(&pdf, "all", 0.5, Some(2.0), &EngineLimits::default()).unwrap();
        approx_eq(&media_box_of(&out, 1), &[0.0, 0.0, 297.5, 1684.0]);

        let ids = contents_ids(&out, 1);
        let doc = Document::load_mem(&out).unwrap();
        let opener = String::from_utf8(stream_bytes(&doc, ids[0])).unwrap();
        assert!(
            opener.contains("0.5 0 0 2 0 0 cm"),
            "non-uniform cm: {opener:?}"
        );
    }

    #[test]
    fn only_selected_pages_are_scaled() {
        let pdf = pdf_with_pages(&["one", "two"]);
        let out = scale_content(&pdf, "2", 2.0, None, &EngineLimits::default()).unwrap();
        approx_eq(&media_box_of(&out, 1), &[0.0, 0.0, 595.0, 842.0]);
        approx_eq(&media_box_of(&out, 2), &[0.0, 0.0, 1190.0, 1684.0]);
        // Page 1 keeps its single-stream Contents (no array wrapper).
        let doc = Document::load_mem(&out).unwrap();
        let p1 = doc.get_pages()[&1];
        assert!(
            doc.get_dictionary(p1)
                .unwrap()
                .get(b"Contents")
                .unwrap()
                .as_reference()
                .is_ok()
        );
    }

    #[test]
    fn rejects_invalid_scales() {
        let pdf = pdf_with_pages(&["one"]);
        for (sx, sy) in [
            (0.0, None),
            (-1.0, None),
            (f64::NAN, None),
            (f64::INFINITY, None),
            (0.005, None), // below 0.01
            (101.0, None), // above 100
            (1.0, Some(0.0)),
            (1.0, Some(f64::NAN)),
        ] {
            assert!(
                scale_content(&pdf, "1", sx, sy, &EngineLimits::default()).is_err(),
                "sx={sx} sy={sy:?} should fail"
            );
        }
    }

    #[test]
    fn rejects_bad_input_and_encrypted() {
        let pdf = pdf_with_pages(&["one", "two"]);
        assert!(scale_content(&pdf, "9", 2.0, None, &EngineLimits::default()).is_err());
        assert!(scale_content(&pdf, "", 2.0, None, &EngineLimits::default()).is_err());
        assert!(scale_content(b"junk", "1", 2.0, None, &EngineLimits::default()).is_err());

        let encrypted = crate::crates::pdf_engine::ops::encrypt::encrypt(
            &pdf,
            "pw",
            None,
            crate::crates::pdf_engine::ops::encrypt::PdfPermissions::default(),
            &EngineLimits::default(),
        )
        .unwrap();
        assert!(matches!(
            scale_content(&encrypted, "1", 2.0, None, &EngineLimits::default()),
            Err(PdfError::Encrypted)
        ));
    }

    #[test]
    fn enforces_page_limit() {
        let pdf = pdf_with_pages(&["one", "two", "three"]);
        let limits = EngineLimits {
            max_pages: Some(2),
            ..EngineLimits::default()
        };
        assert!(scale_content(&pdf, "all", 2.0, None, &limits).is_err());
    }
}
