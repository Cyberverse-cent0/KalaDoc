//! Set an absolute page size (`/MediaBox` + `/CropBox`) with optional
//! content scaling — the "make this page A4" counterpart to
//! [`resize`][super::resize::resize] and [`scale_content`][super::scale_content::scale_content].
//!
//! Unlike `resize` (which rescales boxes about the old lower-left
//! corner and never touches content) this op **re-anchors the page at
//! the user-space origin**: the new box is always `[0 0 w h]` in
//! points, where `w`/`h` come from [`PageSizeSpec`] converted through
//! its [`SizeUnit`].
//!
//! When `PageSizeSpec::scale_content` is `true`, each selected page's
//! existing `/Contents` is wrapped so the painted content is mapped
//! from the old effective `/MediaBox` onto the new box:
//!
//! ```text
//! q
//! sx 0 0 sy tx ty cm      <- prepended stream (before existing content)
//! …existing content…
//! Q                        <- appended stream (after existing content)
//! ```
//!
//! with `sx = new_w / old_w`, `sy = new_h / old_h`,
//! `tx = -old_x0 · sx`, `ty = -old_y0 · sy` — i.e. content coordinates
//! are scaled about the old MediaBox origin and then translated so the
//! old box lands exactly on `[0 0 new_w new_h]`. The wrap reuses the
//! watermark helpers [`add_content_stream`], [`wrap_page_content`] and
//! [`append_page_overlay`], which decode and re-emit the page's content
//! into a single uncompressed stream.
//!
//! When `scale_content` is `false` only the boxes change — identical in
//! spirit to `resize` but with an origin-anchored target box; existing
//! content keeps its user-space coordinates and may be clipped.
//!
//! `/CropBox` is always written (even when the page had none) so the
//! visible area equals the new MediaBox. `/Rotate` is a viewer hint
//! applied after content and needs no change.
//!
//! Both target dimensions must convert to a finite value in
//! `1.0..=14400.0` points (~200 in); violations are
//! [`PdfError::InvalidInput`]. Encrypted input is rejected for
//! [`set_page_size`]; [`get_page_sizes`] is a pure read and tolerates
//! encrypted documents (box entries are numbers, not encrypted strings),
//! matching `get_page_rotations`.

use lopdf::Object;

use crate::crates::core::engine::EngineLimits;

use crate::crates::pdf_engine::error::{PdfError, PdfResult};

use super::crop::{DEFAULT_MEDIA_BOX, inherited_rect, number_object};
use super::load::{load_pdf, lopdf_err, reject_encrypted, save_pdf};
use super::pages::parse_page_range;
use super::watermark::{add_content_stream, append_page_overlay, wrap_page_content};

/// Maximum allowed page-box extent in points (14400 pt ≈ 200 inches).
const MAX_PAGE_BOX_PT: f64 = 14400.0;
/// Minimum allowed page-box extent in points.
const MIN_PAGE_BOX_PT: f64 = 1.0;

/// Unit of the [`PageSizeSpec`] width/height. All variants convert to
/// PDF points (1/72 inch) via [`SizeUnit::to_points`].
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SizeUnit {
    /// Points — the PDF native unit. 1:1.
    Pt,
    /// Millimetres. `72 / 25.4` pt per mm.
    Mm,
    /// Inches. 72 pt per inch.
    In,
    /// Pixels at 72 dpi — 1 px = 1 pt.
    Px,
}

impl SizeUnit {
    /// Convert a value in this unit to points.
    pub fn to_points(&self, value: f64) -> f64 {
        match self {
            SizeUnit::Pt => value,
            SizeUnit::Mm => value * 72.0 / 25.4,
            SizeUnit::In => value * 72.0,
            SizeUnit::Px => value,
        }
    }
}

/// Target page size for [`set_page_size`]. `width`/`height` are given
/// in `unit`; after conversion both must lie in `1.0..=14400.0` pt.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct PageSizeSpec {
    /// Page width in `unit`.
    pub width: f64,
    /// Page height in `unit`.
    pub height: f64,
    /// Unit `width`/`height` are expressed in.
    pub unit: SizeUnit,
    /// When `true`, wrap each page's `/Contents` in a `cm` transform
    /// mapping the old effective MediaBox onto the new box so painted
    /// content scales with the page. When `false`, only the boxes
    /// change.
    pub scale_content: bool,
}

/// Per-page size report produced by [`get_page_sizes`].
#[derive(Debug, Clone, PartialEq)]
pub struct PageSizeInfo {
    /// 1-based page number.
    pub page_number: u32,
    /// Effective `/MediaBox` width in points.
    pub width_pt: f64,
    /// Effective `/MediaBox` height in points.
    pub height_pt: f64,
    /// Effective `/MediaBox` as `[x0, y0, x1, y1]` in points
    /// (page's own entry, else inherited, else US Letter).
    pub media_box: [f64; 4],
    /// Whether the page has an effective `/CropBox` (own or inherited).
    pub has_crop_box: bool,
}

/// Format an `f64` as a PDF number literal (no scientific notation;
/// whole values without a fractional part).
fn pdf_num(v: f64) -> String {
    if v.fract() == 0.0 && v >= i64::MIN as f64 && v <= i64::MAX as f64 {
        format!("{}", v as i64)
    } else {
        format!("{v}")
    }
}

/// Validate that a point-space dimension is usable.
fn check_dim(name: &str, v: f64) -> PdfResult<()> {
    if !v.is_finite() || !(MIN_PAGE_BOX_PT..=MAX_PAGE_BOX_PT).contains(&v) {
        return Err(PdfError::InvalidInput(format!(
            "{name} must be a finite number in [{MIN_PAGE_BOX_PT}, {MAX_PAGE_BOX_PT}] pt, got {v}"
        )));
    }
    Ok(())
}

/// Set the page size of the pages selected by `pages` (a range spec
/// such as `"1-3,8"` or `"all"`, 1-based) to `spec`'s target size.
///
/// `/MediaBox` and `/CropBox` are both written as `[0 0 w h]` in
/// points. With `spec.scale_content`, existing page content is wrapped
/// in a `q … cm` / `Q` pair mapping the old box onto the new one so the
/// painted output fills the page; without it the content keeps its
/// original coordinates.
///
/// Fails with [`PdfError::InvalidInput`] for out-of-range dimensions or
/// malformed page boxes, [`PdfError::InvalidPageRange`] for bad page
/// selections, and [`PdfError::Encrypted`] for encrypted input.
pub fn set_page_size(
    input: &[u8],
    pages: &str,
    spec: &PageSizeSpec,
    limits: &EngineLimits,
) -> PdfResult<Vec<u8>> {
    // Validate before touching the document so a bad spec fails fast.
    let width_pt = spec.unit.to_points(spec.width);
    let height_pt = spec.unit.to_points(spec.height);
    check_dim("width", width_pt)?;
    check_dim("height", height_pt)?;

    let mut doc = load_pdf(input, limits)?;
    reject_encrypted(&doc)?;

    let page_count = doc.get_pages().len() as u32;
    let selected = parse_page_range(pages, page_count)?;
    let pages_map = doc.get_pages();

    for page_number in selected {
        let page_id = pages_map[&page_number];

        let media = inherited_rect(&doc, page_id, b"MediaBox")?.unwrap_or(DEFAULT_MEDIA_BOX);
        let old_w = media[2] - media[0];
        let old_h = media[3] - media[1];
        if !old_w.is_finite() || !old_h.is_finite() || old_w <= 0.0 || old_h <= 0.0 {
            return Err(PdfError::InvalidInput(format!(
                "page {page_number} has a malformed /MediaBox {media:?}"
            )));
        }

        // New box is always anchored at the user-space origin.
        let new_media = [0.0, 0.0, width_pt, height_pt];
        let new_box: Vec<Object> = new_media.iter().map(|&v| number_object(v)).collect();

        if spec.scale_content {
            // Map old MediaBox [m0,m1,m2,m3] onto [0,0,w,h]:
            //   sx = w/old_w, sy = h/old_h
            //   x' = sx·x - sx·m0  →  cm = [sx 0 0 sy (-sx·m0) (-sy·m1)]
            let sx = width_pt / old_w;
            let sy = height_pt / old_h;
            if !sx.is_finite() || !sy.is_finite() || sx <= 0.0 || sy <= 0.0 {
                return Err(PdfError::InvalidInput(format!(
                    "page {page_number} content scale would be non-positive ({sx}, {sy})"
                )));
            }
            let tx = -media[0] * sx;
            let ty = -media[1] * sy;
            let opener = format!(
                "q\n{} 0 0 {} {} {} cm\n",
                pdf_num(sx),
                pdf_num(sy),
                pdf_num(tx),
                pdf_num(ty)
            );
            let opener_id = add_content_stream(&mut doc, opener.into_bytes())?;
            let closer_id = add_content_stream(&mut doc, b"Q\n".to_vec())?;
            wrap_page_content(&mut doc, page_id, opener_id)?;
            append_page_overlay(&mut doc, page_id, closer_id)?;
        }

        let dict = doc.get_dictionary_mut(page_id).map_err(lopdf_err)?;
        dict.set("MediaBox", Object::Array(new_box.clone()));
        dict.set("CropBox", Object::Array(new_box));
    }

    save_pdf(&mut doc)
}

/// Report the effective page size of every page as [`PageSizeInfo`]
/// entries sorted by page number.
///
/// Sizes are the effective `/MediaBox` extents in points (inherited
/// values resolved); `media_box` carries the raw rectangle and
/// `has_crop_box` reports an effective `/CropBox`. Like
/// [`get_page_rotations`][super::rotate_view::get_page_rotations] this
/// tolerates encrypted input.
pub fn get_page_sizes(input: &[u8], limits: &EngineLimits) -> PdfResult<Vec<PageSizeInfo>> {
    let doc = load_pdf(input, limits)?;
    let page_count = doc.get_pages().len() as u32;
    let pages_map = doc.get_pages();

    let mut out = Vec::with_capacity(page_count as usize);
    for page_number in 1..=page_count {
        let page_id = pages_map[&page_number];
        let media = inherited_rect(&doc, page_id, b"MediaBox")?.unwrap_or(DEFAULT_MEDIA_BOX);
        let crop = inherited_rect(&doc, page_id, b"CropBox")?;
        out.push(PageSizeInfo {
            page_number,
            width_pt: media[2] - media[0],
            height_pt: media[3] - media[1],
            media_box: media,
            has_crop_box: crop.is_some(),
        });
    }
    Ok(out)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::crates::pdf_engine::ops::encrypt::{PdfPermissions, encrypt};
    use crate::crates::pdf_engine::ops::test_utils::{
        page_count as test_page_count, pdf_with_pages,
    };
    use lopdf::{Document, Object};

    fn box_of(pdf: &[u8], page: u32, key: &[u8]) -> Option<Vec<f64>> {
        let doc = Document::load_mem(pdf).unwrap();
        let page_id = doc.get_pages()[&page];
        doc.get_dictionary(page_id)
            .ok()?
            .get(key)
            .ok()?
            .as_array()
            .ok()
            .map(|arr| {
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

    /// Decoded bytes of the stream at `id`.
    fn stream_bytes(doc: &Document, id: lopdf::ObjectId) -> Vec<u8> {
        doc.get_object(id)
            .unwrap()
            .as_stream()
            .unwrap()
            .decompressed_content()
            .unwrap()
    }

    #[test]
    fn sets_media_and_crop_box_without_scaling() {
        // test_utils MediaBox is [0 0 595 842]; 210mm x 297mm ≈ A4.
        let pdf = pdf_with_pages(&["one", "two"]);
        let spec = PageSizeSpec {
            width: 210.0,
            height: 297.0,
            unit: SizeUnit::Mm,
            scale_content: false,
        };
        let out = set_page_size(&pdf, "all", &spec, &EngineLimits::default()).unwrap();
        assert_eq!(test_page_count(&out), 2);
        let want_w = 210.0 * 72.0 / 25.4;
        let want_h = 297.0 * 72.0 / 25.4;
        for page in 1..=2 {
            approx_eq(
                &box_of(&out, page, b"MediaBox").unwrap(),
                &[0.0, 0.0, want_w, want_h],
            );
            approx_eq(
                &box_of(&out, page, b"CropBox").unwrap(),
                &[0.0, 0.0, want_w, want_h],
            );
        }
        // Contents untouched: still a single stream reference.
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
    fn unit_conversions() {
        assert_eq!(SizeUnit::Pt.to_points(72.0), 72.0);
        assert!((SizeUnit::Mm.to_points(25.4) - 72.0).abs() < 1e-9);
        assert_eq!(SizeUnit::In.to_points(1.0), 72.0);
        assert_eq!(SizeUnit::Px.to_points(144.0), 144.0);

        let pdf = pdf_with_pages(&["x"]);
        // 8.5 x 11 in = 612 x 792 pt — same as the test fixture box.
        let spec = PageSizeSpec {
            width: 8.5,
            height: 11.0,
            unit: SizeUnit::In,
            scale_content: true,
        };
        let out = set_page_size(&pdf, "1", &spec, &EngineLimits::default()).unwrap();
        approx_eq(
            &box_of(&out, 1, b"MediaBox").unwrap(),
            &[0.0, 0.0, 612.0, 792.0],
        );
    }

    #[test]
    fn scale_content_wraps_stream_with_cm() {
        // 595x842 → 1190x1684 (2x). cm should be `q 2 0 0 2 0 0 cm` … `Q`.
        let pdf = pdf_with_pages(&["hello"]);
        let spec = PageSizeSpec {
            width: 1190.0,
            height: 1684.0,
            unit: SizeUnit::Pt,
            scale_content: true,
        };
        let out = set_page_size(&pdf, "1", &spec, &EngineLimits::default()).unwrap();
        approx_eq(
            &box_of(&out, 1, b"MediaBox").unwrap(),
            &[0.0, 0.0, 1190.0, 1684.0],
        );

        let doc = Document::load_mem(&out).unwrap();
        let page_id = doc.get_pages()[&1];
        // merge_page_content collapses /Contents to a single stream ref.
        let contents = doc
            .get_dictionary(page_id)
            .unwrap()
            .get(b"Contents")
            .unwrap()
            .as_reference()
            .unwrap();
        let bytes = stream_bytes(&doc, contents);
        let text = String::from_utf8_lossy(&bytes);
        assert!(text.contains("q\n2 0 0 2 0 0 cm\n"), "opener: {text:?}");
        assert!(text.trim_end().ends_with('Q'), "closer at end: {text:?}");
        // Original text operator survives inside the wrapper.
        assert!(text.contains("Tj"), "content preserved: {text:?}");
    }

    #[test]
    fn scale_content_maps_nonzero_origin_box() {
        // Build a doc whose MediaBox starts at (50, 100): content at the
        // old origin must land at the new origin, so tx = -50*sx.
        let mut pdf = pdf_with_pages(&["x"]);
        {
            let mut doc = Document::load_mem(&pdf).unwrap();
            let pages = doc.get_pages();
            doc.get_dictionary_mut(pages[&1]).unwrap().set(
                "MediaBox",
                Object::Array(vec![
                    Object::Integer(50),
                    Object::Integer(100),
                    Object::Integer(645),
                    Object::Integer(942),
                ]),
            );
            let mut buf = Vec::new();
            doc.save_to(&mut buf).unwrap();
            pdf = buf;
        }
        let spec = PageSizeSpec {
            width: 595.0,
            height: 842.0,
            unit: SizeUnit::Pt,
            scale_content: true,
        };
        let out = set_page_size(&pdf, "1", &spec, &EngineLimits::default()).unwrap();
        approx_eq(
            &box_of(&out, 1, b"MediaBox").unwrap(),
            &[0.0, 0.0, 595.0, 842.0],
        );
        let doc = Document::load_mem(&out).unwrap();
        let page_id = doc.get_pages()[&1];
        let contents = doc
            .get_dictionary(page_id)
            .unwrap()
            .get(b"Contents")
            .unwrap()
            .as_reference()
            .unwrap();
        let bytes = stream_bytes(&doc, contents);
        let text = String::from_utf8_lossy(&bytes);
        // sx = 595/595 = 1, sy = 842/842 = 1, tx = -50, ty = -100.
        assert!(text.contains("1 0 0 1 -50 -100 cm"), "cm: {text:?}");
    }

    #[test]
    fn only_selected_pages_change() {
        let pdf = pdf_with_pages(&["one", "two"]);
        let spec = PageSizeSpec {
            width: 300.0,
            height: 400.0,
            unit: SizeUnit::Pt,
            scale_content: false,
        };
        let out = set_page_size(&pdf, "2", &spec, &EngineLimits::default()).unwrap();
        approx_eq(
            &box_of(&out, 1, b"MediaBox").unwrap(),
            &[0.0, 0.0, 595.0, 842.0],
        );
        approx_eq(
            &box_of(&out, 2, b"MediaBox").unwrap(),
            &[0.0, 0.0, 300.0, 400.0],
        );
    }

    #[test]
    fn rejects_invalid_dimensions() {
        let pdf = pdf_with_pages(&["one"]);
        for spec in [
            PageSizeSpec {
                width: 0.0,
                height: 100.0,
                unit: SizeUnit::Pt,
                scale_content: false,
            },
            PageSizeSpec {
                width: -5.0,
                height: 100.0,
                unit: SizeUnit::Pt,
                scale_content: false,
            },
            PageSizeSpec {
                width: f64::NAN,
                height: 100.0,
                unit: SizeUnit::Pt,
                scale_content: false,
            },
            // 300 in > 14400 pt cap.
            PageSizeSpec {
                width: 300.0,
                height: 100.0,
                unit: SizeUnit::In,
                scale_content: false,
            },
            // Sub-point size after conversion (0.005 mm ≈ 0.014 pt < 1).
            PageSizeSpec {
                width: 0.005,
                height: 100.0,
                unit: SizeUnit::Mm,
                scale_content: false,
            },
        ] {
            assert!(
                set_page_size(&pdf, "1", &spec, &EngineLimits::default()).is_err(),
                "spec {spec:?} should fail"
            );
        }
    }

    #[test]
    fn rejects_bad_input_and_encrypted() {
        let pdf = pdf_with_pages(&["one", "two"]);
        let spec = PageSizeSpec {
            width: 100.0,
            height: 100.0,
            unit: SizeUnit::Pt,
            scale_content: true,
        };
        assert!(set_page_size(&pdf, "9", &spec, &EngineLimits::default()).is_err());
        assert!(set_page_size(&pdf, "", &spec, &EngineLimits::default()).is_err());
        assert!(set_page_size(b"junk", "1", &spec, &EngineLimits::default()).is_err());

        let enc = encrypt(
            &pdf,
            "pw",
            None,
            PdfPermissions::default(),
            &EngineLimits::default(),
        )
        .unwrap();
        assert!(matches!(
            set_page_size(&enc, "1", &spec, &EngineLimits::default()),
            Err(PdfError::Encrypted)
        ));
        // The read op tolerates encrypted input.
        assert!(get_page_sizes(&enc, &EngineLimits::default()).is_ok());
    }

    #[test]
    fn get_page_sizes_reports_all_pages() {
        let pdf = pdf_with_pages(&["a", "b"]);
        let infos = get_page_sizes(&pdf, &EngineLimits::default()).unwrap();
        assert_eq!(infos.len(), 2);
        assert_eq!(infos[0].page_number, 1);
        assert_eq!(infos[1].page_number, 2);
        approx_eq(&[infos[0].width_pt, infos[0].height_pt], &[595.0, 842.0]);
        assert_eq!(infos[0].media_box, [0.0, 0.0, 595.0, 842.0]);
        assert!(!infos[0].has_crop_box);

        // After set_page_size the report reflects the new box.
        let spec = PageSizeSpec {
            width: 100.0,
            height: 200.0,
            unit: SizeUnit::Pt,
            scale_content: false,
        };
        let out = set_page_size(&pdf, "1", &spec, &EngineLimits::default()).unwrap();
        let infos = get_page_sizes(&out, &EngineLimits::default()).unwrap();
        approx_eq(&[infos[0].width_pt, infos[0].height_pt], &[100.0, 200.0]);
        assert!(infos[0].has_crop_box);
        approx_eq(&[infos[1].width_pt, infos[1].height_pt], &[595.0, 842.0]);
        assert!(!infos[1].has_crop_box);
    }

    #[test]
    fn enforces_page_limit() {
        let pdf = pdf_with_pages(&["one", "two", "three"]);
        let limits = EngineLimits {
            max_pages: Some(2),
            ..EngineLimits::default()
        };
        let spec = PageSizeSpec {
            width: 100.0,
            height: 100.0,
            unit: SizeUnit::Pt,
            scale_content: false,
        };
        assert!(set_page_size(&pdf, "all", &spec, &limits).is_err());
        assert!(get_page_sizes(&pdf, &limits).is_err());
    }
}
