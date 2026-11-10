//! Poster: split each selected page into a grid of tiles, one tile
//! per output page.
//!
//! Every source page is imported as a Form XObject (see [`nup`] for
//! the shared machinery) and each output page paints the *whole* form
//! translated so that one tile region lands at the origin. The tile
//! page's `/MediaBox` and `/CropBox` are both set to the tile rect,
//! and a `re W n` clipping path is emitted before the `Do`, so only
//! the tile region is visible even for viewers that ignore `/CropBox`.
//!
//! # Documented choices and limitations
//!
//! - `tile_cols` and `tile_rows` must each be in `1..=10`.
//! - **Exact split only — no overlap.** Each source page is divided
//!   into `tile_cols * tile_rows` equal rectangles with no gutter or
//!   overlap margin; content crossing a tile boundary is simply cut.
//!   An `overlap` parameter is deliberately not offered.
//! - Tiles are emitted in reading order per source page: row 0 is the
//!   top row, columns fill left-to-right; source pages keep their
//!   selection order.
//! - Tile pages are sized to the tile region of *their own* source
//!   page, so mixed-size documents produce mixed-size output pages.
//! - Only `/MediaBox` geometry is honored; `/Rotate`, `/CropBox` and
//!   annotations of the source are not carried over.
//! - Output pages replace the input page tree entirely (see [`nup`]).
//!
//! [`nup`]: super::nup

use crate::crates::core::engine::EngineLimits;

use crate::crates::pdf_engine::error::{PdfError, PdfResult};

use super::load::{load_pdf, reject_encrypted, save_pdf};
use super::nup::{OutPage, Paint, emit_pages, import_as_forms, selected_pages};

/// Largest allowed value for `tile_cols` / `tile_rows` in [`poster`].
pub(crate) const POSTER_MAX_TILES: u32 = 10;

/// Split each selected page into `tile_cols * tile_rows` tile pages.
///
/// `pages` selects source pages with the shared 1-based range spec;
/// `None` selects all of them. See the module docs for the exact-split
/// rule and limitations.
pub fn poster(
    input: &[u8],
    pages: Option<&str>,
    tile_cols: u32,
    tile_rows: u32,
    limits: &EngineLimits,
) -> PdfResult<Vec<u8>> {
    if !(1..=POSTER_MAX_TILES).contains(&tile_cols) {
        return Err(PdfError::InvalidInput(format!(
            "tile_cols must be in 1..={POSTER_MAX_TILES}, got {tile_cols}"
        )));
    }
    if !(1..=POSTER_MAX_TILES).contains(&tile_rows) {
        return Err(PdfError::InvalidInput(format!(
            "tile_rows must be in 1..={POSTER_MAX_TILES}, got {tile_rows}"
        )));
    }

    let mut doc = load_pdf(input, limits)?;
    reject_encrypted(&doc)?;

    let numbers = selected_pages(&doc, pages)?;
    let forms = import_as_forms(&mut doc, &numbers)?;

    let mut outs = Vec::new();
    for form in &forms {
        let [x0, y0, x1, y1] = form.bbox;
        let page_w = x1 - x0;
        let page_h = y1 - y0;
        if !(page_w.is_finite() && page_h.is_finite() && page_w > 0.0 && page_h > 0.0) {
            return Err(PdfError::InvalidInput(format!(
                "page {} has a degenerate MediaBox",
                form.number
            )));
        }
        let tile_w = page_w / f64::from(tile_cols);
        let tile_h = page_h / f64::from(tile_rows);

        for r in 0..tile_rows {
            for c in 0..tile_cols {
                // Region of the source page this tile covers, in the
                // source's own user-space coordinates. Row 0 is top.
                let rx0 = x0 + f64::from(c) * tile_w;
                let rx1 = rx0 + tile_w;
                let ry1 = y1 - f64::from(r) * tile_h;
                let ry0 = ry1 - tile_h;

                // Sanity: exact split must reproduce the page extent.
                debug_assert!((rx1 - rx0 - tile_w).abs() < 1e-6);
                debug_assert!((ry1 - ry0 - tile_h).abs() < 1e-6);

                outs.push(OutPage {
                    mediabox: [0.0, 0.0, tile_w, tile_h],
                    // CropBox matches MediaBox; the `re W n` clip in the
                    // content stream enforces it for all viewers.
                    cropbox: Some([0.0, 0.0, tile_w, tile_h]),
                    paints: vec![Paint {
                        form_id: form.form_id,
                        // Translate the tile region's min corner to
                        // the output origin; no scaling.
                        matrix: [1.0, 0.0, 0.0, 1.0, -rx0, -ry0],
                        clip: Some([0.0, 0.0, tile_w, tile_h]),
                    }],
                });
            }
        }
    }

    emit_pages(&mut doc, &outs, limits)?;
    save_pdf(&mut doc)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::crates::pdf_engine::ops::test_utils::{
        page_count as test_page_count, pdf_with_pages,
    };
    use lopdf::{Document, Object};

    fn media_box(pdf: &[u8], page: u32) -> Vec<f64> {
        let doc = Document::load_mem(pdf).unwrap();
        let page_id = doc.get_pages()[&page];
        let dict = doc.get_dictionary(page_id).unwrap();
        dict.get(b"MediaBox")
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

    #[test]
    fn splits_each_page_into_tiles() {
        // test_utils pages are 595 x 842.
        let pdf = pdf_with_pages(&["one", "two"]);
        let out = poster(&pdf, None, 2, 2, &EngineLimits::default()).unwrap();
        assert_eq!(test_page_count(&out), 8); // 2 pages x 4 tiles
        let mb = media_box(&out, 1);
        assert!((mb[2] - 297.5).abs() < 0.01);
        assert!((mb[3] - 421.0).abs() < 0.01);
    }

    #[test]
    fn honors_page_selection() {
        let pdf = pdf_with_pages(&["a", "b", "c"]);
        let out = poster(&pdf, Some("2"), 1, 3, &EngineLimits::default()).unwrap();
        assert_eq!(test_page_count(&out), 3);
    }

    #[test]
    fn rejects_bad_tile_counts() {
        let pdf = pdf_with_pages(&["a"]);
        for (c, r) in [(0, 1), (1, 0), (11, 1), (1, 11)] {
            assert!(poster(&pdf, None, c, r, &EngineLimits::default()).is_err());
        }
        assert!(poster(&pdf, Some("9"), 1, 1, &EngineLimits::default()).is_err());
        assert!(poster(b"junk", None, 1, 1, &EngineLimits::default()).is_err());
    }

    #[test]
    fn enforces_output_page_limit() {
        let pdf = pdf_with_pages(&["a"]);
        let limits = EngineLimits {
            max_pages: Some(3),
            ..EngineLimits::default()
        };
        // 2x2 tiles -> 4 output pages > 3.
        let err = poster(&pdf, None, 2, 2, &limits).unwrap_err();
        assert!(matches!(err, PdfError::LimitExceeded(_)), "got {err:?}");
    }
}
