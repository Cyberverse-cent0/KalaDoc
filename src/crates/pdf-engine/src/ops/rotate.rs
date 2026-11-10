//! Rotate selected pages by a multiple of 90 degrees.
//!
//! Rotation is expressed through each page dictionary's `/Rotate` entry,
//! accumulated with any `/Rotate` already set directly on that page
//! (values are normalized into `0..360`). `/Rotate` inherited from a
//! parent `/Pages` node is *not* read — the new value is written on the
//! page itself, which overrides inheritance for viewers.

use lopdf::Object;

use crate::crates::core::engine::EngineLimits;

use crate::crates::pdf_engine::error::{PdfError, PdfResult};

use super::load::{load_pdf, lopdf_err, reject_encrypted, save_pdf};
use super::pages::parse_page_range;

/// A supported page rotation. Only right-angle turns are meaningful in
/// PDF; anything else is rejected rather than approximated.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RotateDegrees {
    /// 90 degrees clockwise.
    Degrees90,
    /// 180 degrees.
    Degrees180,
    /// 270 degrees clockwise (90 counter-clockwise).
    Degrees270,
}

impl RotateDegrees {
    /// The rotation in degrees, as written to `/Rotate` deltas.
    pub fn degrees(self) -> u16 {
        match self {
            Self::Degrees90 => 90,
            Self::Degrees180 => 180,
            Self::Degrees270 => 270,
        }
    }
}

impl TryFrom<u16> for RotateDegrees {
    type Error = PdfError;

    fn try_from(value: u16) -> PdfResult<Self> {
        match value {
            90 => Ok(Self::Degrees90),
            180 => Ok(Self::Degrees180),
            270 => Ok(Self::Degrees270),
            other => Err(PdfError::InvalidInput(format!(
                "rotation must be 90, 180 or 270 degrees, got {other}"
            ))),
        }
    }
}

/// Rotate the pages selected by `pages` (a range spec such as
/// `"1-3,8"` or `"all"`, 1-based) by `degrees`.
///
/// The range is fully validated before the document is modified. The
/// output is a new PDF; the input buffer is untouched.
pub fn rotate(
    input: &[u8],
    pages: &str,
    degrees: RotateDegrees,
    limits: &EngineLimits,
) -> PdfResult<Vec<u8>> {
    let mut doc = load_pdf(input, limits)?;
    reject_encrypted(&doc)?;

    let page_count = doc.get_pages().len() as u32;
    let selected = parse_page_range(pages, page_count)?;
    let pages_map = doc.get_pages();

    for page_number in selected {
        let Some(page_id) = pages_map.get(&page_number) else {
            return Err(PdfError::InvalidPageRange(format!(
                "page {page_number} not found in document"
            )));
        };
        let current = doc
            .get_dictionary(*page_id)
            .ok()
            .and_then(|dict| dict.get(b"Rotate").ok())
            .and_then(|obj| obj.as_i64().ok())
            .unwrap_or(0);
        let new_rotation = (current + i64::from(degrees.degrees())).rem_euclid(360);
        doc.get_dictionary_mut(*page_id)
            .map_err(lopdf_err)?
            .set("Rotate", Object::Integer(new_rotation));
    }

    save_pdf(&mut doc)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::crates::pdf_engine::ops::test_utils::{page_count, pdf_with_pages};
    use lopdf::Document;

    /// Read the `/Rotate` value of a 1-based page in a serialized PDF.
    fn rotate_of(pdf: &[u8], page: u32) -> Option<i64> {
        let doc = Document::load_mem(pdf).unwrap();
        let pages = doc.get_pages();
        let dict = doc.get_dictionary(pages[&page]).unwrap();
        dict.get(b"Rotate").ok().and_then(|o| o.as_i64().ok())
    }

    #[test]
    fn try_from_accepts_only_right_angles() {
        assert_eq!(
            RotateDegrees::try_from(90).unwrap(),
            RotateDegrees::Degrees90
        );
        assert_eq!(
            RotateDegrees::try_from(180).unwrap(),
            RotateDegrees::Degrees180
        );
        assert_eq!(
            RotateDegrees::try_from(270).unwrap(),
            RotateDegrees::Degrees270
        );
        for bad in [0u16, 45, 91, 359, 360] {
            let err = RotateDegrees::try_from(bad).unwrap_err();
            assert!(matches!(err, PdfError::InvalidInput(_)), "got {err:?}");
        }
    }

    #[test]
    fn rotates_only_selected_pages() {
        let pdf = pdf_with_pages(&["one", "two", "three"]);
        let out = rotate(
            &pdf,
            "2",
            RotateDegrees::Degrees90,
            &EngineLimits::default(),
        )
        .unwrap();
        assert_eq!(page_count(&out), 3);
        assert_eq!(rotate_of(&out, 1), None);
        assert_eq!(rotate_of(&out, 2), Some(90));
        assert_eq!(rotate_of(&out, 3), None);
    }

    #[test]
    fn all_rotates_every_page() {
        let pdf = pdf_with_pages(&["one", "two"]);
        let out = rotate(
            &pdf,
            "all",
            RotateDegrees::Degrees180,
            &EngineLimits::default(),
        )
        .unwrap();
        assert_eq!(rotate_of(&out, 1), Some(180));
        assert_eq!(rotate_of(&out, 2), Some(180));
    }

    #[test]
    fn rotation_accumulates_and_wraps() {
        let pdf = pdf_with_pages(&["one"]);
        let once = rotate(
            &pdf,
            "1",
            RotateDegrees::Degrees90,
            &EngineLimits::default(),
        )
        .unwrap();
        assert_eq!(rotate_of(&once, 1), Some(90));

        let twice = rotate(
            &once,
            "1",
            RotateDegrees::Degrees90,
            &EngineLimits::default(),
        )
        .unwrap();
        assert_eq!(rotate_of(&twice, 1), Some(180));

        // 180 + 270 = 450 -> normalized to 90.
        let thrice = rotate(
            &twice,
            "1",
            RotateDegrees::Degrees270,
            &EngineLimits::default(),
        )
        .unwrap();
        assert_eq!(rotate_of(&thrice, 1), Some(90));

        // 90 + 270 = 360 -> normalized to 0.
        let back = rotate(
            &thrice,
            "1",
            RotateDegrees::Degrees270,
            &EngineLimits::default(),
        )
        .unwrap();
        assert_eq!(rotate_of(&back, 1), Some(0));
    }

    #[test]
    fn rejects_invalid_range_and_input() {
        let pdf = pdf_with_pages(&["one", "two"]);
        for spec in ["", "0", "3", "2-1", "1,x", "alll"] {
            let err = rotate(
                &pdf,
                spec,
                RotateDegrees::Degrees90,
                &EngineLimits::default(),
            )
            .unwrap_err();
            assert!(
                matches!(err, PdfError::InvalidPageRange(_)),
                "spec {spec:?} got {err:?}"
            );
        }
        assert!(
            rotate(
                b"junk",
                "1",
                RotateDegrees::Degrees90,
                &EngineLimits::default()
            )
            .is_err()
        );
    }
}
