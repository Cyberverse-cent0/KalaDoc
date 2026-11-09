//! Booklet: 2-up-per-side imposition for saddle-stitched printing.
//!
//! Selected source pages are imported as Form XObjects (see [`nup`]
//! for the shared machinery). Pages are grouped in chunks of four;
//! each chunk becomes one physical sheet with **two pages per side**
//! (four page faces per sheet), emitted as two output PDF pages —
//! front side first, then back side:
//!
//! - front side: chunk pages 0 and 1
//! - back side:  chunk pages 2 and 3
//!
//! [`BookletMode::LeftRight`] places the first page of each pair in
//! the left slot and the second in the right slot;
//! [`BookletMode::RightLeft`] mirrors the pair (first page right,
//! second left) for right-to-left reading order. Each page is scaled
//! uniformly to fit its slot and centered; a `re W n` clip keeps
//! overflow inside the slot.
//!
//! # Honest scope — what this is NOT
//!
//! This is a **sequential 2-up-per-side layout, not a true
//! saddle-stitch imposition**. Real booklet imposition reorders pages
//! across sheets so that, after folding and collating, the page
//! sequence reads correctly (the classic "signature" ordering). This
//! op does **not** reorder: chunk *k* always holds selected pages
//! `4k..4k+4` in order. Use it for simple 2-up handouts or as a
//! building block; do not expect folded-correct output for arbitrary
//! documents.
//!
//! Also not done:
//!
//! - No creep / shingling / gutter compensation — both slots are
//!   equal halves of the sheet.
//! - No automatic rotation of source pages; `/Rotate` is ignored and
//!   only `/MediaBox` geometry is honored.
//! - Blank padding: an odd page count is padded with blank slots so
//!   every emitted side exists (needed for duplex printing). A fully
//!   blank *side* is still emitted as a (blank) page.
//! - Annotations, outlines and other page-level structures of the
//!   source are dropped, as with `overlay`/`nup`.
//!
//! [`nup`]: super::nup

use crate::crates::core::engine::EngineLimits;

use crate::crates::pdf_engine::error::{PdfError, PdfResult};

use super::load::{load_pdf, reject_encrypted, save_pdf};
use super::nup::{OutPage, Paint, emit_pages, import_as_forms, selected_pages};

/// Finished sheet size for [`BookletSpec`].
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum BookletSize {
    /// Use the first selected page's `/MediaBox` extent as the sheet
    /// size, in whatever orientation the page has (no rotation applied).
    SameAsInput,
    /// A4 sheet in landscape orientation: 595.28 x 841.89 pt portrait
    /// swapped to 841.89 x 595.28 pt.
    A4,
    /// US Letter sheet in landscape orientation: 792 x 612 pt.
    Letter,
}

/// Horizontal ordering of the two page slots on each side.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum BookletMode {
    /// First page of a pair goes left, second goes right.
    LeftRight,
    /// First page of a pair goes right, second goes left.
    RightLeft,
}

/// Parameters for [`booklet`].
#[derive(Debug, Clone, Copy)]
pub struct BookletSpec {
    /// Finished sheet size.
    pub sheet_size: BookletSize,
    /// Slot ordering for right-to-left vs left-to-right books.
    pub imposition: BookletMode,
}

/// US Letter width in points (for landscape sheet math).
const LETTER_W: f64 = 612.0;
/// US Letter height in points.
const LETTER_H: f64 = 792.0;
/// A4 width in points (ISO 216: 210 mm).
const A4_W: f64 = 595.28;
/// A4 height in points (ISO 216: 297 mm).
const A4_H: f64 = 841.89;

/// Lay out the selected pages as a 2-up-per-side booklet.
///
/// `page_spec` — despite the name, this is the *sheet* spec
/// ([`BookletSpec`]: sheet size + slot ordering); the page selection
/// is fixed to all pages. See the module docs for the exact ordering
/// scheme and its limitations.
pub fn booklet(input: &[u8], page_spec: BookletSpec, limits: &EngineLimits) -> PdfResult<Vec<u8>> {
    let mut doc = load_pdf(input, limits)?;
    reject_encrypted(&doc)?;

    let numbers = selected_pages(&doc, None)?;
    let forms = import_as_forms(&mut doc, &numbers)?;

    // Sheet extent [0, 0, w, h].
    let (sheet_w, sheet_h) = match page_spec.sheet_size {
        BookletSize::SameAsInput => {
            let b = forms[0].bbox;
            let w = (b[2] - b[0]).abs();
            let h = (b[3] - b[1]).abs();
            if !(w.is_finite() && h.is_finite() && w > 0.0 && h > 0.0) {
                return Err(PdfError::InvalidInput(
                    "first page has a degenerate MediaBox; cannot size the sheet".to_string(),
                ));
            }
            (w, h)
        }
        // Landscape: width and height swapped relative to portrait.
        BookletSize::A4 => (A4_H, A4_W),
        BookletSize::Letter => (LETTER_H, LETTER_W),
    };

    // Two equal slots per side, each clipped to its half of the sheet.
    let slot_w = sheet_w / 2.0;
    let left_slot = [0.0, 0.0, slot_w, sheet_h];
    let right_slot = [slot_w, 0.0, slot_w, sheet_h];

    // Pad the page list to a multiple of 4 with blanks.
    let chunk_count = numbers.len().div_ceil(4);
    let mut outs = Vec::with_capacity(chunk_count * 2);
    for chunk in 0..chunk_count {
        // Front side: pages 4k+0, 4k+1. Back side: 4k+2, 4k+3.
        // Missing indices stay blank.
        let mut sides: [Vec<Paint>; 2] = [Vec::new(), Vec::new()];
        for side in 0..2 {
            for slot in 0..2 {
                let idx = chunk * 4 + side * 2 + slot;
                let Some(form) = forms.get(idx) else { continue };
                // RightLeft mirrors the pair within the side.
                let use_left = match page_spec.imposition {
                    BookletMode::LeftRight => slot == 0,
                    BookletMode::RightLeft => slot == 1,
                };
                let rect = if use_left { left_slot } else { right_slot };
                let [sx, sy, sw, sh] = rect;
                let src_w = (form.bbox[2] - form.bbox[0]).abs();
                let src_h = (form.bbox[3] - form.bbox[1]).abs();
                if !(src_w.is_finite() && src_h.is_finite() && src_w > 0.0 && src_h > 0.0) {
                    return Err(PdfError::InvalidInput(format!(
                        "page {} has a degenerate MediaBox",
                        form.number
                    )));
                }
                let s = (sw / src_w).min(sh / src_h);
                let dw = src_w * s;
                let dh = src_h * s;
                let dst_x = sx + (sw - dw) / 2.0;
                let dst_y = sy + (sh - dh) / 2.0;
                sides[side].push(Paint {
                    form_id: form.form_id,
                    matrix: [
                        s,
                        0.0,
                        0.0,
                        s,
                        dst_x - form.bbox[0] * s,
                        dst_y - form.bbox[1] * s,
                    ],
                    clip: Some(rect),
                });
            }
        }
        for side in sides {
            outs.push(OutPage {
                mediabox: [0.0, 0.0, sheet_w, sheet_h],
                cropbox: None,
                paints: side,
            });
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

    fn spec() -> BookletSpec {
        BookletSpec {
            sheet_size: BookletSize::Letter,
            imposition: BookletMode::LeftRight,
        }
    }

    #[test]
    fn four_pages_make_one_sheet() {
        let pdf = pdf_with_pages(&["a", "b", "c", "d"]);
        let out = booklet(&pdf, spec(), &EngineLimits::default()).unwrap();
        assert_eq!(test_page_count(&out), 2); // front + back
        let mb = media_box(&out, 1);
        // Letter landscape: 792 x 612.
        assert!((mb[2] - 792.0).abs() < 0.01);
        assert!((mb[3] - 612.0).abs() < 0.01);
    }

    #[test]
    fn odd_count_pads_to_full_sheets() {
        // 5 pages -> 2 chunks -> 4 output pages (last back side blank).
        let pdf = pdf_with_pages(&["a", "b", "c", "d", "e"]);
        let out = booklet(&pdf, spec(), &EngineLimits::default()).unwrap();
        assert_eq!(test_page_count(&out), 4);
    }

    #[test]
    fn same_as_input_uses_first_page_size() {
        // test_utils pages are 595 x 842 portrait; sheet keeps that.
        let pdf = pdf_with_pages(&["a", "b"]);
        let out = booklet(
            &pdf,
            BookletSpec {
                sheet_size: BookletSize::SameAsInput,
                imposition: BookletMode::RightLeft,
            },
            &EngineLimits::default(),
        )
        .unwrap();
        assert_eq!(test_page_count(&out), 2);
        let mb = media_box(&out, 1);
        assert!((mb[2] - 595.0).abs() < 0.01);
        assert!((mb[3] - 842.0).abs() < 0.01);
    }

    #[test]
    fn enforces_output_page_limit() {
        let pdf = pdf_with_pages(&["a", "b", "c", "d", "e"]);
        let limits = EngineLimits {
            max_pages: Some(2),
            ..EngineLimits::default()
        };
        // 5 pages -> 4 output pages > 2.
        let err = booklet(&pdf, spec(), &limits).unwrap_err();
        assert!(matches!(err, PdfError::LimitExceeded(_)), "got {err:?}");
    }

    #[test]
    fn rejects_bad_input() {
        assert!(booklet(b"junk", spec(), &EngineLimits::default()).is_err());
    }
}
