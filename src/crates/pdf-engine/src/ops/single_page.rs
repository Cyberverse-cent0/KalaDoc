//! Single page: arrange selected pages on one giant output page.
//!
//! All selected source pages are imported as Form XObjects (see
//! [`nup`] for the shared machinery) and painted, scaled uniformly to
//! fit, into a grid on a single output page. The grid has
//! `cols` columns (default `ceil(sqrt(n))`) and `ceil(n / cols)`
//! rows; cell size is the maximum source page width × height across
//! the selection, so mixed-size documents still line up.
//!
//! # Documented choices and limitations
//!
//! - `cols`, when given, must be in `1..=1024`; values larger than the
//!   page count leave trailing empty cells. `None` picks
//!   `ceil(sqrt(n))`.
//! - The output page size is `cols * max_page_w` × `rows * max_page_h`
//!   where `max_page_w`/`max_page_h` are the largest selected-page
//!   MediaBox extents. Both dimensions are capped at
//!   [`MAX_OUTPUT_DIM`] points (20000 pt ≈ 7 m); exceeding it fails
//!   with [`PdfError::InvalidInput`] instead of producing an
//!   unusable page. `limits.max_pages` trivially passes (1 page).
//! - Only `/MediaBox` geometry is honored; `/Rotate`, `/CropBox` and
//!   annotations are not carried over.
//! - The output document consists only of the new page (see [`nup`]).
//!
//! [`nup`]: super::nup

use crate::crates::core::engine::EngineLimits;

use crate::crates::pdf_engine::error::{PdfError, PdfResult};

use super::load::{load_pdf, reject_encrypted, save_pdf};
use super::nup::{OutPage, Paint, emit_pages, import_as_forms, selected_pages};

/// Largest allowed `cols` value for [`single_page`].
pub(crate) const SINGLE_PAGE_MAX_COLS: u32 = 1024;

/// Largest allowed output page dimension in points for [`single_page`].
///
/// 20000 pt ≈ 264 inches — far beyond any real media, so hitting it
/// means the grid parameters were absurd.
pub(crate) const MAX_OUTPUT_DIM: f64 = 20_000.0;

/// Arrange the selected pages on a single large page.
///
/// `pages` selects source pages with the shared 1-based range spec;
/// `None` selects all of them. `cols` optionally fixes the grid
/// column count; see the module docs for defaults, sizing and limits.
pub fn single_page(
    input: &[u8],
    pages: Option<&str>,
    cols: Option<u32>,
    limits: &EngineLimits,
) -> PdfResult<Vec<u8>> {
    if let Some(c) = cols
        && !(1..=SINGLE_PAGE_MAX_COLS).contains(&c)
    {
        return Err(PdfError::InvalidInput(format!(
            "cols must be in 1..={SINGLE_PAGE_MAX_COLS}, got {c}"
        )));
    }

    let mut doc = load_pdf(input, limits)?;
    reject_encrypted(&doc)?;

    let numbers = selected_pages(&doc, pages)?;
    let forms = import_as_forms(&mut doc, &numbers)?;

    let n = forms.len();
    let cols = cols.unwrap_or_else(|| (n as f64).sqrt().ceil() as u32);
    let rows = n.div_ceil(cols as usize) as u32;

    // Uniform cell size = largest source page extent.
    let mut cell_w = 0.0f64;
    let mut cell_h = 0.0f64;
    for form in &forms {
        let w = (form.bbox[2] - form.bbox[0]).abs();
        let h = (form.bbox[3] - form.bbox[1]).abs();
        if !(w.is_finite() && h.is_finite() && w > 0.0 && h > 0.0) {
            return Err(PdfError::InvalidInput(format!(
                "page {} has a degenerate MediaBox",
                form.number
            )));
        }
        cell_w = cell_w.max(w);
        cell_h = cell_h.max(h);
    }

    let page_w = f64::from(cols) * cell_w;
    let page_h = f64::from(rows) * cell_h;
    if page_w > MAX_OUTPUT_DIM || page_h > MAX_OUTPUT_DIM {
        return Err(PdfError::InvalidInput(format!(
            "single-page layout would be {page_w:.0} x {page_h:.0} pt, \
             exceeding the {MAX_OUTPUT_DIM:.0} pt per-dimension cap"
        )));
    }

    let mut paints = Vec::with_capacity(n);
    for (i, form) in forms.iter().enumerate() {
        let col = (i % cols as usize) as f64;
        let row = (i / cols as usize) as f64; // 0 = top row
        let src_w = (form.bbox[2] - form.bbox[0]).abs();
        let src_h = (form.bbox[3] - form.bbox[1]).abs();
        let s = (cell_w / src_w).min(cell_h / src_h);
        let dw = src_w * s;
        let dh = src_h * s;
        let dst_x = col * cell_w + (cell_w - dw) / 2.0;
        let dst_y = page_h - (row + 1.0) * cell_h + (cell_h - dh) / 2.0;
        paints.push(Paint {
            form_id: form.form_id,
            matrix: [
                s,
                0.0,
                0.0,
                s,
                dst_x - form.bbox[0] * s,
                dst_y - form.bbox[1] * s,
            ],
            clip: None,
        });
    }

    let outs = vec![OutPage {
        mediabox: [0.0, 0.0, page_w, page_h],
        cropbox: None,
        paints,
    }];
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

    fn media_box(pdf: &[u8]) -> Vec<f64> {
        let doc = Document::load_mem(pdf).unwrap();
        let page_id = doc.get_pages()[&1];
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
    fn combines_pages_onto_one_page() {
        // test_utils pages are 595 x 842; sqrt(4) = 2 cols.
        let pdf = pdf_with_pages(&["a", "b", "c", "d"]);
        let out = single_page(&pdf, None, None, &EngineLimits::default()).unwrap();
        assert_eq!(test_page_count(&out), 1);
        let mb = media_box(&out);
        assert!((mb[2] - 2.0 * 595.0).abs() < 0.01);
        assert!((mb[3] - 2.0 * 842.0).abs() < 0.01);
    }

    #[test]
    fn honors_explicit_cols() {
        let pdf = pdf_with_pages(&["a", "b", "c"]);
        let out = single_page(&pdf, None, Some(3), &EngineLimits::default()).unwrap();
        assert_eq!(test_page_count(&out), 1);
        let mb = media_box(&out);
        // 3 cols x 1 row of 595 x 842 cells.
        assert!((mb[2] - 3.0 * 595.0).abs() < 0.01);
        assert!((mb[3] - 842.0).abs() < 0.01);
    }

    #[test]
    fn rejects_oversized_layout() {
        let pdf = pdf_with_pages(&["a"]);
        let err = single_page(&pdf, None, Some(1024), &EngineLimits::default()).unwrap_err();
        assert!(matches!(err, PdfError::InvalidInput(_)), "got {err:?}");
        assert!(single_page(&pdf, None, Some(0), &EngineLimits::default()).is_err());
        assert!(single_page(&pdf, Some("9"), None, &EngineLimits::default()).is_err());
        assert!(single_page(b"junk", None, None, &EngineLimits::default()).is_err());
    }
}
