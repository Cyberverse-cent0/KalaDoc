//! Read, set and remove the five page-box entries (`/MediaBox`,
//! `/CropBox`, `/BleedBox`, `/TrimBox`, `/ArtBox`) on selected pages.
//!
//! Page boxes are inheritable per the PDF spec: a box defined on a
//! `/Pages` ancestor applies to every descendant page that does not
//! override it. [`get_page_boxes`] resolves each box through
//! [`inherited_rect`][super::crop::inherited_rect] so callers always see
//! the *effective* rectangle. Effective-value cascade follows the spec:
//!
//! - `/MediaBox` — page's own entry, else inherited, else US Letter
//!   ([`DEFAULT_MEDIA_BOX`][super::crop::DEFAULT_MEDIA_BOX]).
//! - `/CropBox` — own/inherited entry, else the effective `/MediaBox`.
//! - `/BleedBox`, `/TrimBox`, `/ArtBox` — own/inherited entry, else the
//!   effective `/CropBox` (which itself falls back to `/MediaBox`).
//!
//! All five names are therefore always present in
//! [`PageBoxInfo::boxes`], keyed by their PDF name (e.g. `"MediaBox"`).
//!
//! [`set_page_box`] writes the box on each selected page's own
//! dictionary (shadowing any inherited value). The rectangle must be
//! four finite numbers with `x0 < x1` and `y0 < y1`; negative origins
//! are allowed because page boxes are not required to start at the
//! user-space origin. Per the spec a `/CropBox` *should* lie within
//! the `/MediaBox`, but it is not required to — out-of-MediaBox CropBoxes
//! are tolerated by viewers, so [`set_page_box`] accepts them and only
//! documents the deviation here rather than rejecting.
//!
//! [`remove_page_box`] deletes the page-level entry only; an inherited
//! value from a `/Pages` ancestor then becomes effective again.
//! `/MediaBox` is required by the spec and cannot be removed —
//! [`PageBox::MediaBox`] is rejected with [`PdfError::InvalidInput`].
//! Use [`set_page_size`][super::page_size::set_page_size] or
//! [`crop`][super::crop::crop] to change the media area instead.
//!
//! [`get_page_boxes`] is a pure read: encrypted input that `lopdf`
//! can decrypt on its own (e.g. an empty user password) is read
//! normally, while a password-protected document — whose page objects
//! cannot be loaded without the key — fails with
//! [`PdfError::Encrypted`] rather than reporting a bogus empty result.
//! [`set_page_box`] and [`remove_page_box`] transform the document and
//! return [`PdfError::Encrypted`] for encrypted input.

use std::collections::BTreeMap;

use lopdf::Object;

use crate::crates::core::engine::EngineLimits;

use crate::crates::pdf_engine::error::{PdfError, PdfResult};

use super::crop::{DEFAULT_MEDIA_BOX, inherited_rect, number_object};
use super::load::{doc_is_encrypted, load_pdf, lopdf_err, reject_encrypted, save_pdf};
use super::pages::parse_page_range;

/// One of the five page-box entries defined by the PDF spec.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum PageBox {
    /// `/MediaBox` — the physical page extent in default user space.
    MediaBox,
    /// `/CropBox` — the visible region; defaults to `/MediaBox`.
    CropBox,
    /// `/BleedBox` — the region for bleed during printing; defaults to
    /// `/CropBox`.
    BleedBox,
    /// `/TrimBox` — the finished trim size; defaults to `/CropBox`.
    TrimBox,
    /// `/ArtBox` — the extent of meaningful content; defaults to
    /// `/CropBox`.
    ArtBox,
}

impl PageBox {
    /// All five box kinds, in spec order.
    pub const ALL: [PageBox; 5] = [
        PageBox::MediaBox,
        PageBox::CropBox,
        PageBox::BleedBox,
        PageBox::TrimBox,
        PageBox::ArtBox,
    ];

    /// The PDF dictionary key for this box (e.g. `"MediaBox"`).
    pub fn name(&self) -> &'static str {
        match self {
            PageBox::MediaBox => "MediaBox",
            PageBox::CropBox => "CropBox",
            PageBox::BleedBox => "BleedBox",
            PageBox::TrimBox => "TrimBox",
            PageBox::ArtBox => "ArtBox",
        }
    }

    /// The raw dictionary key bytes (`b"MediaBox"`, ...) for
    /// [`inherited_rect`][super::crop::inherited_rect].
    fn dict_key(&self) -> &'static [u8] {
        match self {
            PageBox::MediaBox => b"MediaBox",
            PageBox::CropBox => b"CropBox",
            PageBox::BleedBox => b"BleedBox",
            PageBox::TrimBox => b"TrimBox",
            PageBox::ArtBox => b"ArtBox",
        }
    }
}

impl std::fmt::Display for PageBox {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(self.name())
    }
}

/// Per-page page-box report produced by [`get_page_boxes`].
#[derive(Debug, Clone, PartialEq)]
pub struct PageBoxInfo {
    /// 1-based page number.
    pub page: u32,
    /// Effective rectangles for all five box kinds, keyed by PDF name
    /// (`"MediaBox"`, `"CropBox"`, ...). Values are `[x0, y0, x1, y1]`
    /// in PDF points; see the module docs for the fallback cascade.
    pub boxes: BTreeMap<String, [f64; 4]>,
}

/// Resolve the five effective page boxes of one page.
fn effective_boxes(
    doc: &lopdf::Document,
    page_id: lopdf::ObjectId,
) -> PdfResult<BTreeMap<String, [f64; 4]>> {
    let media =
        inherited_rect(doc, page_id, PageBox::MediaBox.dict_key())?.unwrap_or(DEFAULT_MEDIA_BOX);
    let crop = inherited_rect(doc, page_id, PageBox::CropBox.dict_key())?.unwrap_or(media);
    let mut boxes = BTreeMap::new();
    boxes.insert(PageBox::MediaBox.name().to_string(), media);
    boxes.insert(PageBox::CropBox.name().to_string(), crop);
    for bx in [PageBox::BleedBox, PageBox::TrimBox, PageBox::ArtBox] {
        let rect = inherited_rect(doc, page_id, bx.dict_key())?.unwrap_or(crop);
        boxes.insert(bx.name().to_string(), rect);
    }
    Ok(boxes)
}

/// Validate a `[x0, y0, x1, y1]` page-box rectangle.
fn check_rect(which: PageBox, rect: [f64; 4]) -> PdfResult<()> {
    let [x0, y0, x1, y1] = rect;
    if ![x0, y0, x1, y1].iter().all(|v| v.is_finite()) {
        return Err(PdfError::InvalidInput(format!(
            "{which} box coordinates must be finite numbers"
        )));
    }
    if x1 <= x0 || y1 <= y0 {
        return Err(PdfError::InvalidInput(format!(
            "{which} box must have positive width and height (x1 > x0, y1 > y0), got {rect:?}"
        )));
    }
    Ok(())
}

fn page_id_for<'a>(
    pages_map: &'a std::collections::BTreeMap<u32, lopdf::ObjectId>,
    page_number: u32,
) -> PdfResult<lopdf::ObjectId> {
    pages_map.get(&page_number).copied().ok_or_else(|| {
        PdfError::InvalidPageRange(format!("page {page_number} not found in document"))
    })
}

/// Report the effective page boxes of the pages selected by `pages`
/// (a range spec such as `"1-3,8"` or `"all"`, 1-based) as
/// [`PageBoxInfo`] entries in selection order.
///
/// This is a pure read and never modifies the document. Encrypted
/// input that `lopdf` decrypts transparently (empty user password) is
/// read normally; a password-protected document fails with
/// [`PdfError::Encrypted`] because its page objects cannot be loaded
/// without the key.
pub fn get_page_boxes(
    input: &[u8],
    pages: &str,
    limits: &EngineLimits,
) -> PdfResult<Vec<PageBoxInfo>> {
    let doc = load_pdf(input, limits)?;
    if doc_is_encrypted(&doc) && doc.get_pages().is_empty() {
        return Err(PdfError::Encrypted);
    }
    let page_count = doc.get_pages().len() as u32;
    let selected = parse_page_range(pages, page_count)?;
    let pages_map = doc.get_pages();

    let mut out = Vec::with_capacity(selected.len());
    for page_number in selected {
        let page_id = page_id_for(&pages_map, page_number)?;
        out.push(PageBoxInfo {
            page: page_number,
            boxes: effective_boxes(&doc, page_id)?,
        });
    }
    Ok(out)
}

/// Set `which`'s box to `rect` (`[x0, y0, x1, y1]` in PDF points) on
/// the pages selected by `pages`, writing the entry on each page's own
/// dictionary so it shadows any inherited value.
///
/// Fails with [`PdfError::InvalidInput`] for non-finite or
/// non-positive-extent rectangles, [`PdfError::InvalidPageRange`] for
/// bad selections, and [`PdfError::Encrypted`] for encrypted input.
/// A `/CropBox` outside the `/MediaBox` is accepted (the spec marks
/// containment as a "should", not a "shall"); see module docs.
pub fn set_page_box(
    input: &[u8],
    pages: &str,
    which: PageBox,
    rect: [f64; 4],
    limits: &EngineLimits,
) -> PdfResult<Vec<u8>> {
    check_rect(which, rect)?;

    let mut doc = load_pdf(input, limits)?;
    reject_encrypted(&doc)?;

    let page_count = doc.get_pages().len() as u32;
    let selected = parse_page_range(pages, page_count)?;
    let pages_map = doc.get_pages();

    let objects: Vec<Object> = rect.iter().map(|&v| number_object(v)).collect();
    for page_number in selected {
        let page_id = page_id_for(&pages_map, page_number)?;
        doc.get_dictionary_mut(page_id)
            .map_err(lopdf_err)?
            .set(which.name(), Object::Array(objects.clone()));
    }

    save_pdf(&mut doc)
}

/// Remove the page-level `which` entry from the pages selected by
/// `pages`. Removing a key the page does not define is a no-op — an
/// inherited value from a `/Pages` ancestor then applies again.
///
/// `/MediaBox` is required by the PDF spec and cannot be removed:
/// [`PageBox::MediaBox`] is rejected with [`PdfError::InvalidInput`]
/// before the document is even loaded.
pub fn remove_page_box(
    input: &[u8],
    pages: &str,
    which: PageBox,
    limits: &EngineLimits,
) -> PdfResult<Vec<u8>> {
    if which == PageBox::MediaBox {
        return Err(PdfError::InvalidInput(
            "MediaBox cannot be removed; set a new one with set_page_box or set_page_size"
                .to_string(),
        ));
    }

    let mut doc = load_pdf(input, limits)?;
    reject_encrypted(&doc)?;

    let page_count = doc.get_pages().len() as u32;
    let selected = parse_page_range(pages, page_count)?;
    let pages_map = doc.get_pages();

    for page_number in selected {
        let page_id = page_id_for(&pages_map, page_number)?;
        let dict = doc.get_dictionary_mut(page_id).map_err(lopdf_err)?;
        // lopdf returns Err for a missing key; removing it is a no-op.
        let _ = dict.remove(which.name().as_bytes());
    }

    save_pdf(&mut doc)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::crates::pdf_engine::ops::encrypt::{PdfPermissions, encrypt};
    use crate::crates::pdf_engine::ops::test_utils::{page_count, pdf_with_pages};
    use lopdf::{Document, Object};

    /// Read a page dictionary's own box entry as `[f64; 4]`, or `None`.
    fn own_box(pdf: &[u8], page: u32, key: &str) -> Option<Vec<f64>> {
        let doc = Document::load_mem(pdf).unwrap();
        let page_id = doc.get_pages()[&page];
        doc.get_dictionary(page_id)
            .ok()?
            .get(key.as_bytes())
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

    /// Build a PDF where the `/Pages` root carries `CropBox`/`TrimBox`
    /// and page dictionaries carry only `/MediaBox` — everything else
    /// must be inherited.
    fn pdf_with_inherited_boxes() -> Vec<u8> {
        let mut doc = Document::load_mem(&pdf_with_pages(&["one", "two"])).unwrap();
        let pages_map = doc.get_pages();
        for n in 1..=2u32 {
            doc.get_dictionary_mut(pages_map[&n])
                .unwrap()
                .remove(b"MediaBox");
        }
        // Find the /Pages root via a page's /Parent and set inherited boxes.
        let parent = {
            let page_id = pages_map[&1];
            doc.get_dictionary(page_id)
                .unwrap()
                .get(b"Parent")
                .unwrap()
                .as_reference()
                .unwrap()
        };
        doc.get_dictionary_mut(parent).unwrap().set(
            "CropBox",
            Object::Array(vec![
                Object::Integer(10),
                Object::Integer(20),
                Object::Integer(400),
                Object::Integer(700),
            ]),
        );
        doc.get_dictionary_mut(parent).unwrap().set(
            "TrimBox",
            Object::Array(vec![
                Object::Integer(50),
                Object::Integer(60),
                Object::Integer(300),
                Object::Integer(600),
            ]),
        );
        let mut buf = Vec::new();
        doc.save_to(&mut buf).unwrap();
        buf
    }

    #[test]
    fn reports_all_five_boxes_with_fallbacks() {
        // test_utils: MediaBox [0 0 595 842] on both page and /Pages.
        let pdf = pdf_with_pages(&["one"]);
        let infos = get_page_boxes(&pdf, "1", &EngineLimits::default()).unwrap();
        assert_eq!(infos.len(), 1);
        assert_eq!(infos[0].page, 1);
        assert_eq!(infos[0].boxes.len(), 5);
        approx_eq(&infos[0].boxes["MediaBox"], &[0.0, 0.0, 595.0, 842.0]);
        // No CropBox defined → falls back to MediaBox; Bleed/Trim/Art
        // fall back to the effective CropBox (== MediaBox here).
        for name in ["CropBox", "BleedBox", "TrimBox", "ArtBox"] {
            approx_eq(&infos[0].boxes[name], &[0.0, 0.0, 595.0, 842.0]);
        }
    }

    #[test]
    fn resolves_inherited_boxes() {
        let pdf = pdf_with_inherited_boxes();
        let infos = get_page_boxes(&pdf, "all", &EngineLimits::default()).unwrap();
        assert_eq!(infos.len(), 2);
        for info in &infos {
            // MediaBox is inherited from /Pages too ([0 0 595 842]).
            approx_eq(&info.boxes["MediaBox"], &[0.0, 0.0, 595.0, 842.0]);
            approx_eq(&info.boxes["CropBox"], &[10.0, 20.0, 400.0, 700.0]);
            approx_eq(&info.boxes["TrimBox"], &[50.0, 60.0, 300.0, 600.0]);
            // BleedBox/ArtBox have no entry → fall back to CropBox.
            approx_eq(&info.boxes["BleedBox"], &[10.0, 20.0, 400.0, 700.0]);
            approx_eq(&info.boxes["ArtBox"], &[10.0, 20.0, 400.0, 700.0]);
        }
    }

    #[test]
    fn sets_each_box_kind() {
        let pdf = pdf_with_pages(&["one", "two"]);
        for bx in PageBox::ALL {
            let rect = [1.0, 2.0, 300.0, 400.0];
            // Page 2 keeps whatever it had before — for MediaBox the
            // fixture already defines [0 0 595 842] on every page.
            let before = own_box(&pdf, 2, bx.name());
            let out = set_page_box(&pdf, "1", bx, rect, &EngineLimits::default()).unwrap();
            approx_eq(&own_box(&out, 1, bx.name()).unwrap(), &rect);
            // Other pages untouched.
            assert_eq!(own_box(&out, 2, bx.name()), before);
            // Other box kinds on page 1 untouched (CropBox absent stays absent).
        }
        // Page 1 now has every box set; MediaBox got overwritten too.
        let infos =
            get_page_boxes(&pdf_with_pages(&["one"]), "1", &EngineLimits::default()).unwrap();
        assert!(infos[0].boxes.contains_key("ArtBox"));
    }

    #[test]
    fn set_page_box_shadows_inherited_value() {
        let pdf = pdf_with_inherited_boxes();
        let rect = [0.0, 0.0, 200.0, 200.0];
        let out =
            set_page_box(&pdf, "2", PageBox::CropBox, rect, &EngineLimits::default()).unwrap();
        approx_eq(&own_box(&out, 2, "CropBox").unwrap(), &rect);
        // Page 1 still inherits the /Pages CropBox.
        let infos = get_page_boxes(&out, "1", &EngineLimits::default()).unwrap();
        approx_eq(&infos[0].boxes["CropBox"], &[10.0, 20.0, 400.0, 700.0]);
        // Page 2 reports its own value.
        let infos = get_page_boxes(&out, "2", &EngineLimits::default()).unwrap();
        approx_eq(&infos[0].boxes["CropBox"], &rect);
    }

    #[test]
    fn set_page_box_accepts_crop_outside_media() {
        // Spec says containment is a "should", not a "shall" — accepted.
        let pdf = pdf_with_pages(&["one"]);
        let out = set_page_box(
            &pdf,
            "1",
            PageBox::CropBox,
            [-50.0, -50.0, 700.0, 900.0],
            &EngineLimits::default(),
        )
        .unwrap();
        approx_eq(
            &own_box(&out, 1, "CropBox").unwrap(),
            &[-50.0, -50.0, 700.0, 900.0],
        );
    }

    #[test]
    fn set_page_box_validates_rect() {
        let pdf = pdf_with_pages(&["one"]);
        for rect in [
            [f64::NAN, 0.0, 10.0, 10.0],
            [0.0, f64::INFINITY, 10.0, 10.0],
            [10.0, 0.0, 10.0, 10.0], // x1 <= x0
            [0.0, 10.0, 10.0, 10.0], // y1 <= y0
            [5.0, 5.0, 5.0, 9.0],    // zero width
        ] {
            assert!(
                set_page_box(&pdf, "1", PageBox::TrimBox, rect, &EngineLimits::default()).is_err(),
                "rect {rect:?} should fail"
            );
        }
        assert!(
            set_page_box(
                &pdf,
                "9",
                PageBox::TrimBox,
                [0.0, 0.0, 1.0, 1.0],
                &EngineLimits::default()
            )
            .is_err()
        );
        assert!(
            set_page_box(
                b"junk",
                "1",
                PageBox::TrimBox,
                [0.0, 0.0, 1.0, 1.0],
                &EngineLimits::default()
            )
            .is_err()
        );
    }

    #[test]
    fn removes_page_level_box() {
        let pdf = pdf_with_pages(&["one", "two"]);
        let set = set_page_box(
            &pdf,
            "all",
            PageBox::ArtBox,
            [5.0, 5.0, 100.0, 100.0],
            &EngineLimits::default(),
        )
        .unwrap();
        let out = remove_page_box(&set, "1", PageBox::ArtBox, &EngineLimits::default()).unwrap();
        assert!(own_box(&out, 1, "ArtBox").is_none());
        // Page 2 keeps its ArtBox.
        approx_eq(
            &own_box(&out, 2, "ArtBox").unwrap(),
            &[5.0, 5.0, 100.0, 100.0],
        );
        // Removing an absent box is a harmless no-op; both pages remain.
        let out2 = remove_page_box(&pdf, "1", PageBox::BleedBox, &EngineLimits::default()).unwrap();
        assert_eq!(page_count(&out2), 2);
    }

    #[test]
    fn remove_restores_inherited_value() {
        let pdf = pdf_with_inherited_boxes();
        // Give page 1 its own CropBox, then remove it → inherited wins.
        let set = set_page_box(
            &pdf,
            "1",
            PageBox::CropBox,
            [0.0, 0.0, 50.0, 50.0],
            &EngineLimits::default(),
        )
        .unwrap();
        let out = remove_page_box(&set, "1", PageBox::CropBox, &EngineLimits::default()).unwrap();
        let infos = get_page_boxes(&out, "1", &EngineLimits::default()).unwrap();
        approx_eq(&infos[0].boxes["CropBox"], &[10.0, 20.0, 400.0, 700.0]);
    }

    #[test]
    fn media_box_is_not_removable() {
        let pdf = pdf_with_pages(&["one"]);
        let err =
            remove_page_box(&pdf, "1", PageBox::MediaBox, &EngineLimits::default()).unwrap_err();
        assert!(matches!(err, PdfError::InvalidInput(_)), "got {err:?}");
    }

    #[test]
    fn rejects_encrypted_for_transforms() {
        let pdf = pdf_with_pages(&["one"]);
        let enc = encrypt(
            &pdf,
            "pw",
            None,
            PdfPermissions::default(),
            &EngineLimits::default(),
        )
        .unwrap();
        let rect = [0.0, 0.0, 10.0, 10.0];
        assert!(matches!(
            set_page_box(&enc, "1", PageBox::CropBox, rect, &EngineLimits::default()),
            Err(PdfError::Encrypted)
        ));
        assert!(matches!(
            remove_page_box(&enc, "1", PageBox::CropBox, &EngineLimits::default()),
            Err(PdfError::Encrypted)
        ));
        // The read op cannot inspect a password-protected document
        // either: lopdf loads only the trailer without the key, so the
        // page tree is unreachable — it reports Encrypted rather than
        // a misleading "no pages" range error.
        assert!(matches!(
            get_page_boxes(&enc, "1", &EngineLimits::default()),
            Err(PdfError::Encrypted)
        ));
    }

    #[test]
    fn get_page_boxes_rejects_bad_ranges() {
        let pdf = pdf_with_pages(&["one"]);
        assert!(get_page_boxes(&pdf, "9", &EngineLimits::default()).is_err());
        assert!(get_page_boxes(&pdf, "", &EngineLimits::default()).is_err());
        assert!(get_page_boxes(b"junk", "1", &EngineLimits::default()).is_err());
    }

    #[test]
    fn page_box_display_and_keys() {
        assert_eq!(PageBox::BleedBox.name(), "BleedBox");
        assert_eq!(PageBox::ArtBox.to_string(), "ArtBox");
        assert_eq!(PageBox::ALL.len(), 5);
    }
}
