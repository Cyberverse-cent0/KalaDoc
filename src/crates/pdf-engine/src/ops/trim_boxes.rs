//! Print-production page boxes: `/TrimBox`, `/BleedBox`, `/MediaBox`, `/CropBox`.
//!
//! [`apply_trim_preset`] resizes the *trim* area of selected pages to a
//! standard print preset (A4, A5, Letter, Legal, A3), centered on each
//! page's effective box (its `/CropBox` when present, else `/MediaBox`,
//! both resolved through the `/Pages` ancestor chain — see
//! [`super::crop::inherited_rect`]). `/BleedBox` is set to the trim box
//! expanded by `bleed` points on every side.
//!
//! `/MediaBox` and `/CropBox` are deliberately left unchanged: most
//! viewers keep showing the full media area, while prepress tools read
//! `/TrimBox` (and `/BleedBox`) to know where the finished piece is cut
//! and how far artwork may extend past the trim. This matches the PDF
//! spec's box-intersection rules — TrimBox must lie inside BleedBox and
//! CropBox.
//!
//! [`get_trim_boxes`] is the read-only counterpart: it reports the four
//! page boxes for inspection without modifying the document.

use lopdf::{Document, Object, ObjectId};

use crate::crates::core::engine::EngineLimits;

use crate::crates::pdf_engine::error::{PdfError, PdfResult};

use super::crop::{DEFAULT_MEDIA_BOX, inherited_rect, number_object};
use super::load::{load_pdf, lopdf_err, reject_encrypted, save_pdf};
use super::pages::parse_page_range;

/// Millimetres to PostScript points (72 pt per inch).
const MM_TO_PT: f64 = 72.0 / 25.4;

/// Default bleed in points: 3 mm ≈ 8.5 pt — the conventional print-shop
/// bleed allowance.
pub const DEFAULT_BLEED_PT: f64 = 3.0 * MM_TO_PT;

/// Maximum allowed bleed in points (36 pt = ½ inch). Larger values are
/// almost certainly unit mistakes (e.g. millimetres passed as points).
pub const MAX_BLEED_PT: f64 = 36.0;

/// Default offset for crop/registration marks relative to the trim box,
/// in points (9 pt ≈ 3 mm). Carried on [`BoxPreset`] for callers that
/// also draw marks (see `super::print_marks`); this module only stores
/// the boxes.
pub const DEFAULT_MARKS_OFFSET_PT: f64 = 9.0;

/// Standard print presets for [`apply_trim_preset`].
///
/// Sizes are the finished (trim) dimensions in portrait orientation,
/// in points. Presets are **not** rotated to match page `/Rotate` —
/// the trim box is written in unrotated user space, consistent with how
/// `/MediaBox` and `/CropBox` are defined.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum TrimPreset {
    /// ISO A4 — 210 × 297 mm.
    A4,
    /// ISO A5 — 148 × 210 mm.
    A5,
    /// US Letter — 8.5 × 11 in (612 × 792 pt).
    Letter,
    /// US Legal — 8.5 × 14 in (612 × 1008 pt).
    Legal,
    /// ISO A3 — 297 × 420 mm.
    A3,
}

impl TrimPreset {
    /// Trim size `[width, height]` in points, portrait.
    pub fn size(self) -> [f64; 2] {
        match self {
            // mm → pt, rounded to 2 decimals (the conventional values
            // used throughout the industry).
            TrimPreset::A4 => [
                (210.0 * MM_TO_PT * 100.0).round() / 100.0,
                (297.0 * MM_TO_PT * 100.0).round() / 100.0,
            ],
            TrimPreset::A5 => [
                (148.0 * MM_TO_PT * 100.0).round() / 100.0,
                (210.0 * MM_TO_PT * 100.0).round() / 100.0,
            ],
            TrimPreset::Letter => [612.0, 792.0],
            TrimPreset::Legal => [612.0, 1008.0],
            TrimPreset::A3 => [
                (297.0 * MM_TO_PT * 100.0).round() / 100.0,
                (420.0 * MM_TO_PT * 100.0).round() / 100.0,
            ],
        }
    }
}

/// A resolved print-box specification: the trim rectangle, the bleed
/// amount, and the mark offset used by downstream marking operations.
///
/// `trim` is stored as `[0, 0, w, h]` — the preset's nominal size at the
/// origin. [`apply_trim_preset`] re-centers it on each page's effective
/// box, so the stored origin is only meaningful as "size".
#[derive(Debug, Clone, Copy)]
pub struct BoxPreset {
    /// Trim box `[x0, y0, x1, y1]` in points (nominal `[0, 0, w, h]`).
    pub trim: [f64; 4],
    /// Bleed beyond the trim box on every side, in points.
    /// Must be finite and in `0.0..=36` ([`MAX_BLEED_PT`]).
    pub bleed: f64,
    /// Crop/registration mark offset from the trim edge, in points.
    /// Must be finite and non-negative.
    pub marks_offset: f64,
}

impl BoxPreset {
    /// Build a [`BoxPreset`] for `preset` with the given bleed and mark
    /// offset. `bleed` defaults to [`DEFAULT_BLEED_PT`] when `None`.
    ///
    /// Fails with [`PdfError::InvalidInput`] on non-finite or out-of-range
    /// `bleed`/`marks_offset`.
    pub fn new(
        preset: TrimPreset,
        bleed: Option<f64>,
        marks_offset: Option<f64>,
    ) -> PdfResult<Self> {
        let bleed = bleed.unwrap_or(DEFAULT_BLEED_PT);
        validate_bleed(bleed)?;
        let marks_offset = marks_offset.unwrap_or(DEFAULT_MARKS_OFFSET_PT);
        if !marks_offset.is_finite() || marks_offset < 0.0 {
            return Err(PdfError::InvalidInput(
                "marks offset must be a finite non-negative number of points".to_string(),
            ));
        }
        let [w, h] = preset.size();
        Ok(Self {
            trim: [0.0, 0.0, w, h],
            bleed,
            marks_offset,
        })
    }

    /// The trim box `[x0, y0, x1, y1]` of preset size centered on `base`
    /// (`[x0, y0, x1, y1]` — typically the page's effective CropBox or
    /// MediaBox).
    pub fn centered_trim(&self, base: [f64; 4]) -> [f64; 4] {
        let [w, h] = [self.trim[2] - self.trim[0], self.trim[3] - self.trim[1]];
        let cx = (base[0] + base[2]) / 2.0;
        let cy = (base[1] + base[3]) / 2.0;
        [cx - w / 2.0, cy - h / 2.0, cx + w / 2.0, cy + h / 2.0]
    }

    /// The bleed box for a given trim box: trim expanded by `bleed` on
    /// all four sides.
    pub fn bleed_box(&self, trim: [f64; 4]) -> [f64; 4] {
        [
            trim[0] - self.bleed,
            trim[1] - self.bleed,
            trim[2] + self.bleed,
            trim[3] + self.bleed,
        ]
    }
}

fn validate_bleed(bleed: f64) -> PdfResult<()> {
    if !bleed.is_finite() || !(0.0..=MAX_BLEED_PT).contains(&bleed) {
        return Err(PdfError::InvalidInput(format!(
            "bleed must be a finite number of points in 0..={MAX_BLEED_PT} (3 mm ≈ {DEFAULT_BLEED_PT:.1} pt is typical), got {bleed}"
        )));
    }
    Ok(())
}

/// The effective box a preset is centered on: the page's `/CropBox` if
/// defined (inherited through `/Pages` ancestors), else its `/MediaBox`,
/// else the default letter size — mirroring how viewers pick the
/// visible area before the preset is applied.
fn effective_base_box(doc: &Document, page_id: ObjectId) -> PdfResult<[f64; 4]> {
    if let Some(rect) = inherited_rect(doc, page_id, b"CropBox")? {
        return Ok(rect);
    }
    if let Some(rect) = inherited_rect(doc, page_id, b"MediaBox")? {
        return Ok(rect);
    }
    Ok(DEFAULT_MEDIA_BOX)
}

/// A page's four boxes as read from the document, for [`get_trim_boxes`].
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct PageBoxInfo {
    /// 1-based page number.
    pub page: u32,
    /// `/MediaBox` `[x0, y0, x1, y1]` in points, if defined.
    pub media: Option<[f64; 4]>,
    /// `/CropBox` `[x0, y0, x1, y1]` in points, if defined.
    pub crop: Option<[f64; 4]>,
    /// `/TrimBox` `[x0, y0, x1, y1]` in points, if defined.
    pub trim: Option<[f64; 4]>,
    /// `/BleedBox` `[x0, y0, x1, y1]` in points, if defined.
    pub bleed: Option<[f64; 4]>,
}

/// Set `/TrimBox` to `preset`'s size centered on each selected page's
/// effective box, and `/BleedBox` to the trim box expanded by `bleed`
/// points on every side. `/MediaBox` and `/CropBox` are unchanged.
///
/// `pages` is a range spec such as `"1-3,8"` or `"all"` (1-based).
/// `bleed` is in points and must be finite and in `0..=36`
/// ([`MAX_BLEED_PT`]); pass [`DEFAULT_BLEED_PT`] for the conventional
/// 3 mm bleed.
///
/// Encrypted documents are rejected with [`PdfError::Encrypted`], per
/// the transforming-op convention — this engine never decrypts.
pub fn apply_trim_preset(
    input: &[u8],
    pages: &str,
    preset: TrimPreset,
    bleed: f64,
    limits: &EngineLimits,
) -> PdfResult<Vec<u8>> {
    validate_bleed(bleed)?;
    let spec = BoxPreset::new(preset, Some(bleed), None)?;

    let mut doc = load_pdf(input, limits)?;
    reject_encrypted(&doc)?;

    let page_count = doc.get_pages().len() as u32;
    let selected = parse_page_range(pages, page_count)?;
    let pages_map = doc.get_pages();

    for page_number in selected {
        let page_id = pages_map[&page_number];
        let base = effective_base_box(&doc, page_id)?;
        let trim = spec.centered_trim(base);
        let bleed_rect = spec.bleed_box(trim);

        // TrimBox and BleedBox must be well-formed rectangles.
        for (name, rect) in [("TrimBox", trim), ("BleedBox", bleed_rect)] {
            if rect[2] <= rect[0] || rect[3] <= rect[1] {
                return Err(PdfError::InvalidInput(format!(
                    "computed {name} for page {page_number} is empty or inverted: {rect:?}"
                )));
            }
        }

        let dict = doc.get_dictionary_mut(page_id).map_err(lopdf_err)?;
        dict.set(
            "TrimBox",
            Object::Array(
                trim.iter()
                    .map(|&v| number_object(v))
                    .collect::<Vec<Object>>(),
            ),
        );
        dict.set(
            "BleedBox",
            Object::Array(
                bleed_rect
                    .iter()
                    .map(|&v| number_object(v))
                    .collect::<Vec<Object>>(),
            ),
        );
    }

    save_pdf(&mut doc)
}

/// Read the `/MediaBox`, `/CropBox`, `/TrimBox` and `/BleedBox` of every
/// page (or the pages selected by `pages`, e.g. `"1-3,8"` / `"all"`;
/// `None` selects all). Each box is resolved through the `/Pages`
/// ancestor chain; a box that is absent or malformed on every ancestor
/// is reported as `None`.
///
/// Read-only — the document is never modified. Encrypted documents are
/// rejected with [`PdfError::Encrypted`] for consistency with the other
/// box operations.
pub fn get_trim_boxes(input: &[u8], limits: &EngineLimits) -> PdfResult<Vec<PageBoxInfo>> {
    let doc = load_pdf(input, limits)?;
    reject_encrypted(&doc)?;

    let pages_map = doc.get_pages();
    let mut out = Vec::with_capacity(pages_map.len());
    for (&page_number, &page_id) in pages_map.iter() {
        out.push(PageBoxInfo {
            page: page_number,
            media: inherited_rect(&doc, page_id, b"MediaBox")?,
            crop: inherited_rect(&doc, page_id, b"CropBox")?,
            trim: inherited_rect(&doc, page_id, b"TrimBox")?,
            bleed: inherited_rect(&doc, page_id, b"BleedBox")?,
        });
    }
    out.sort_by_key(|info| info.page);
    Ok(out)
}

/// Variant of [`get_trim_boxes`] honoring a page range spec.
pub fn get_trim_boxes_for_pages(
    input: &[u8],
    pages: &str,
    limits: &EngineLimits,
) -> PdfResult<Vec<PageBoxInfo>> {
    let doc = load_pdf(input, limits)?;
    reject_encrypted(&doc)?;
    let page_count = doc.get_pages().len() as u32;
    let selected: std::collections::BTreeSet<u32> =
        parse_page_range(pages, page_count)?.into_iter().collect();
    let pages_map = doc.get_pages();
    let mut out = Vec::with_capacity(selected.len());
    for &page_number in &selected {
        let page_id = pages_map[&page_number];
        out.push(PageBoxInfo {
            page: page_number,
            media: inherited_rect(&doc, page_id, b"MediaBox")?,
            crop: inherited_rect(&doc, page_id, b"CropBox")?,
            trim: inherited_rect(&doc, page_id, b"TrimBox")?,
            bleed: inherited_rect(&doc, page_id, b"BleedBox")?,
        });
    }
    Ok(out)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::crates::pdf_engine::ops::test_utils::pdf_with_pages;
    use lopdf::Document;

    fn box_of(pdf: &[u8], page: u32, key: &[u8]) -> Option<Vec<f64>> {
        let doc = Document::load_mem(pdf).unwrap();
        let page_id = doc.get_pages()[&page];
        let dict = doc.get_dictionary(page_id).unwrap();
        dict.get(key).ok().map(|o| {
            o.as_array()
                .unwrap()
                .iter()
                .map(|n| match n {
                    Object::Integer(v) => *v as f64,
                    Object::Real(v) => f64::from(*v),
                    _ => panic!("non-numeric box entry"),
                })
                .collect()
        })
    }

    #[test]
    fn a4_preset_centers_trim_on_media_box() {
        // test_utils pages have MediaBox [0 0 595 842]; A4 is
        // 595.28 x 841.89 pt — essentially the same size, centered.
        let pdf = pdf_with_pages(&["one", "two"]);
        let out = apply_trim_preset(
            &pdf,
            "all",
            TrimPreset::A4,
            DEFAULT_BLEED_PT,
            &EngineLimits::default(),
        )
        .unwrap();
        let trim = box_of(&out, 1, b"TrimBox").unwrap();
        let [w, h] = TrimPreset::A4.size();
        assert!((trim[2] - trim[0] - w).abs() < 0.01, "trim width {trim:?}");
        assert!((trim[3] - trim[1] - h).abs() < 0.01, "trim height {trim:?}");
        // Centered on [0,0,595,842]: center ≈ (297.5, 421).
        let cx = (trim[0] + trim[2]) / 2.0;
        let cy = (trim[1] + trim[3]) / 2.0;
        assert!((cx - 297.5).abs() < 0.01);
        assert!((cy - 421.0).abs() < 0.01);
        // BleedBox = trim + 8.5pt each side.
        let bleed = box_of(&out, 1, b"BleedBox").unwrap();
        assert!((bleed[0] - (trim[0] - DEFAULT_BLEED_PT)).abs() < 0.01);
        assert!((bleed[2] - (trim[2] + DEFAULT_BLEED_PT)).abs() < 0.01);
        assert!((bleed[3] - (trim[3] + DEFAULT_BLEED_PT)).abs() < 0.01);
        // MediaBox and CropBox untouched.
        assert!(box_of(&out, 1, b"MediaBox").is_some());
        assert!(box_of(&out, 1, b"CropBox").is_none());
        // Page 2 also got boxes.
        assert!(box_of(&out, 2, b"TrimBox").is_some());
    }

    #[test]
    fn smaller_preset_centers_on_crop_box() {
        // Give page 1 a CropBox [100 100 400 700]; A5 (419.5x595.3) is
        // larger than the crop in width — still centered on it.
        let mut pdf = pdf_with_pages(&["one"]);
        {
            let mut doc = Document::load_mem(&pdf).unwrap();
            let page_id = doc.get_pages()[&1];
            doc.get_dictionary_mut(page_id).unwrap().set(
                "CropBox",
                Object::Array(
                    [100.0, 100.0, 400.0, 700.0]
                        .iter()
                        .map(|&v| number_object(v))
                        .collect(),
                ),
            );
            let mut buf = Vec::new();
            doc.save_to(&mut buf).unwrap();
            pdf = buf;
        }
        let out =
            apply_trim_preset(&pdf, "1", TrimPreset::A5, 0.0, &EngineLimits::default()).unwrap();
        let trim = box_of(&out, 1, b"TrimBox").unwrap();
        let [w, h] = TrimPreset::A5.size();
        assert!((trim[2] - trim[0] - w).abs() < 0.01);
        assert!((trim[3] - trim[1] - h).abs() < 0.01);
        // Center of CropBox is (250, 400).
        let cx = (trim[0] + trim[2]) / 2.0;
        let cy = (trim[1] + trim[3]) / 2.0;
        assert!((cx - 250.0).abs() < 0.01, "cx {cx}");
        assert!((cy - 400.0).abs() < 0.01, "cy {cy}");
        // bleed=0 → BleedBox == TrimBox.
        let bleed = box_of(&out, 1, b"BleedBox").unwrap();
        assert_eq!(bleed, trim);
    }

    #[test]
    fn rejects_bad_bleed_and_bad_pages() {
        let pdf = pdf_with_pages(&["one"]);
        for bleed in [-1.0, 37.0, f64::NAN, f64::INFINITY] {
            assert!(
                apply_trim_preset(&pdf, "1", TrimPreset::A4, bleed, &EngineLimits::default())
                    .is_err(),
                "bleed {bleed} should fail"
            );
        }
        assert!(
            apply_trim_preset(
                &pdf,
                "9",
                TrimPreset::A4,
                DEFAULT_BLEED_PT,
                &EngineLimits::default()
            )
            .is_err()
        );
        assert!(
            apply_trim_preset(
                b"junk",
                "1",
                TrimPreset::A4,
                DEFAULT_BLEED_PT,
                &EngineLimits::default()
            )
            .is_err()
        );
    }

    #[test]
    fn rejects_encrypted() {
        // Encrypt via lopdf is complex; simulate by injecting an
        // /Encrypt entry into the trailer — doc_is_encrypted checks
        // trailer.has(b"Encrypt").
        let pdf = pdf_with_pages(&["one"]);
        let mut doc = Document::load_mem(&pdf).unwrap();
        doc.trailer.set("Encrypt", Object::Null);
        let mut buf = Vec::new();
        doc.save_to(&mut buf).unwrap();
        let err = apply_trim_preset(
            &buf,
            "1",
            TrimPreset::A4,
            DEFAULT_BLEED_PT,
            &EngineLimits::default(),
        )
        .unwrap_err();
        assert!(matches!(err, PdfError::Encrypted), "got {err:?}");
    }

    #[test]
    fn get_trim_boxes_reports_boxes() {
        let pdf = pdf_with_pages(&["one", "two"]);
        let out = apply_trim_preset(
            &pdf,
            "1",
            TrimPreset::Letter,
            DEFAULT_BLEED_PT,
            &EngineLimits::default(),
        )
        .unwrap();
        let infos = get_trim_boxes(&out, &EngineLimits::default()).unwrap();
        assert_eq!(infos.len(), 2);
        assert_eq!(infos[0].page, 1);
        assert!(infos[0].trim.is_some());
        assert!(infos[0].bleed.is_some());
        assert!(infos[0].media.is_some());
        // Page 2 has no TrimBox.
        assert!(infos[1].trim.is_none());
        assert!(infos[1].media.is_some());
    }

    #[test]
    fn box_preset_geometry() {
        let spec = BoxPreset::new(TrimPreset::Letter, None, None).unwrap();
        assert_eq!(spec.bleed, DEFAULT_BLEED_PT);
        let trim = spec.centered_trim([0.0, 0.0, 100.0, 100.0]);
        assert_eq!(trim[2] - trim[0], 612.0);
        assert_eq!(trim[3] - trim[1], 792.0);
        let bleed = spec.bleed_box(trim);
        assert!((bleed[0] - trim[0] + spec.bleed).abs() < 1e-9);
        assert!(BoxPreset::new(TrimPreset::A4, Some(99.0), None).is_err());
    }
}
