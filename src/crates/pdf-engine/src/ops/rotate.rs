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
