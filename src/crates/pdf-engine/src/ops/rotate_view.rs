//! Set absolute page rotation (`/Rotate`) and inspect or normalize it.
//!
//! This complements [`super::rotate`]: `rotate` *accumulates* a delta
//! onto each page's existing `/Rotate`, while [`set_page_rotation`]
//! writes an **absolute** value — the requested orientation replaces
//! whatever was there. Use `set_page_rotation` when the caller knows
//! the desired final orientation (e.g. "make this page landscape");
//! use `rotate` for "turn this page another quarter".
//!
//! `/Rotate` lives on the page dictionary itself; per the PDF spec it
//! is inheritable from ancestor `/Pages` nodes, but this module only
//! ever reads/writes the **page-level** entry — matching `rotate`'s
//! documented convention that a page-local `/Rotate` overrides
//! inheritance for viewers. [`get_page_rotations`] therefore reports
//! `0` for pages that rely on an inherited `/Rotate`, and
//! [`normalize_rotations`] leaves inherited values untouched.

use lopdf::Object;

use crate::crates::core::engine::EngineLimits;

use crate::crates::pdf_engine::error::{PdfError, PdfResult};

use super::load::{load_pdf, lopdf_err, reject_encrypted, save_pdf};
use super::pages::parse_page_range;

/// Set the absolute `/Rotate` of the pages selected by `pages` (a
/// range spec such as `"1-3,8"` or `"all"`, 1-based).
///
/// `degrees` must be a multiple of 90 and is reduced modulo 360, so
/// `450` becomes `90` and `-90` (when expressed as an unsigned wrap,
/// e.g. `270`) is accepted. The stored value is always one of
/// `0, 90, 180, 270`. Unlike [`super::rotate::rotate`], the value is
/// **not** added to the existing `/Rotate` — it replaces it. Setting
/// `0` still writes an explicit `/Rotate 0` entry (a no-op for viewers,
/// but explicit); use [`normalize_rotations`] to remove redundant
/// entries.
///
/// Returns [`PdfError::Encrypted`] for encrypted input.
pub fn set_page_rotation(
    input: &[u8],
    pages: &str,
    degrees: u16,
    limits: &EngineLimits,
) -> PdfResult<Vec<u8>> {
    if degrees % 90 != 0 {
        return Err(PdfError::InvalidInput(format!(
            "rotation must be a multiple of 90 degrees, got {degrees}"
        )));
    }
    let rotation = i64::from(degrees % 360);

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
        doc.get_dictionary_mut(*page_id)
            .map_err(lopdf_err)?
            .set("Rotate", Object::Integer(rotation));
    }

    save_pdf(&mut doc)
}

/// Report the effective `/Rotate` of every page, as
/// `(page_number, degrees)` pairs sorted by page number.
///
/// Only the page dictionary's **own** `/Rotate` entry is read —
/// values inherited from `/Pages` ancestors are not resolved, so a
/// page relying on inheritance reports `0`. Numeric values are
/// normalized into `0..360`; non-numeric or absent entries report `0`.
///
/// Like [`super::info::inspect`], this tolerates encrypted input: a
/// `/Rotate` value is a number, not an encrypted string, so reporting
/// it does not require decryption.
pub fn get_page_rotations(input: &[u8], limits: &EngineLimits) -> PdfResult<Vec<(u32, u16)>> {
    let doc = load_pdf(input, limits)?;
    let page_count = doc.get_pages().len() as u32;
    let pages_map = doc.get_pages();

    let mut out = Vec::with_capacity(page_count as usize);
    for page_number in 1..=page_count {
        let rotation = pages_map
            .get(&page_number)
            .and_then(|id| doc.get_dictionary(*id).ok())
            .and_then(|dict| dict.get(b"Rotate").ok())
            .and_then(rotation_degrees)
            .unwrap_or(0);
        out.push((page_number, rotation));
    }
    Ok(out)
}

/// Normalize every page's own `/Rotate` entry into the canonical
/// `0..360` range and drop entries that carry no information.
///
/// For each page with a numeric `/Rotate`:
///
/// - values are reduced modulo 360 (so `360` → `0`, `-90` → `270`,
///   `450` → `90`);
/// - a normalized value of `0` **removes** the `/Rotate` key entirely
///   (0 is the default; an explicit entry is redundant);
/// - any other normalized value is written back as an integer.
///
/// Pages without a `/Rotate` entry, with a non-numeric one, or whose
/// `/Rotate` is inherited from a `/Pages` node are left untouched —
/// this op cleans up page-level entries only. Returns
/// [`PdfError::Encrypted`] for encrypted input.
pub fn normalize_rotations(input: &[u8], limits: &EngineLimits) -> PdfResult<Vec<u8>> {
    let mut doc = load_pdf(input, limits)?;
    reject_encrypted(&doc)?;

    let page_count = doc.get_pages().len() as u32;
    let pages_map = doc.get_pages();

    for page_number in 1..=page_count {
        let Some(page_id) = pages_map.get(&page_number).copied() else {
            continue;
        };
        let current = doc
            .get_dictionary(page_id)
            .ok()
            .and_then(|dict| dict.get(b"Rotate").ok().cloned())
            .and_then(|obj| numeric_rotation(&obj));
        let Some(current) = current else {
            continue;
        };
        let normalized = current.rem_euclid(360);
        let dict = doc.get_dictionary_mut(page_id).map_err(lopdf_err)?;
        if normalized == 0 {
            let _ = dict.remove(b"Rotate");
        } else if normalized != current {
            dict.set("Rotate", Object::Integer(normalized));
        }
    }

    save_pdf(&mut doc)
}

/// Read a `/Rotate` value as a normalized `0..360` degree count.
/// Accepts integer or real numbers; anything else yields `None`.
fn rotation_degrees(obj: &Object) -> Option<u16> {
    numeric_rotation(obj).map(|v| v.rem_euclid(360) as u16)
}

/// Extract the raw numeric value of a `/Rotate` entry, if it is an
/// integer or a real number. Shared with `auto_orient`.
pub(crate) fn numeric_rotation(obj: &Object) -> Option<i64> {
    if let Ok(v) = obj.as_i64() {
        return Some(v);
    }
    // Real-valued /Rotate is non-conforming but seen in the wild; a
    // fractional degree is meaningless, so truncate toward zero after
    // folding into range below.
    obj.as_float().ok().map(|v| v as i64)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::crates::pdf_engine::ops::encrypt::{PdfPermissions, encrypt};
    use crate::crates::pdf_engine::ops::test_utils::pdf_with_pages;
    use lopdf::Document;

    fn rotate_of(pdf: &[u8], page: u32) -> Option<i64> {
        let doc = Document::load_mem(pdf).unwrap();
        let page_id = doc.get_pages()[&page];
        let dict = doc.get_dictionary(page_id).unwrap();
        dict.get(b"Rotate").ok().and_then(|o| o.as_i64().ok())
    }

    #[test]
    fn sets_absolute_rotation() {
        let pdf = pdf_with_pages(&["a", "b"]);
        // Pre-set page 1 to 90 via set_page_rotation, then "set" 180 —
        // must replace, not accumulate (rotate would give 270).
        let out = set_page_rotation(&pdf, "1", 90, &EngineLimits::default()).unwrap();
        let out = set_page_rotation(&out, "1", 180, &EngineLimits::default()).unwrap();
        assert_eq!(rotate_of(&out, 1), Some(180));
        // Page 2 untouched.
        assert_eq!(rotate_of(&out, 2), None);
    }

    #[test]
    fn normalizes_degree_input() {
        let pdf = pdf_with_pages(&["a"]);
        let out = set_page_rotation(&pdf, "1", 450, &EngineLimits::default()).unwrap();
        assert_eq!(rotate_of(&out, 1), Some(90));
        let out = set_page_rotation(&pdf, "1", 0, &EngineLimits::default()).unwrap();
        assert_eq!(rotate_of(&out, 1), Some(0));
        for bad in [45u16, 100, 359] {
            assert!(
                set_page_rotation(&pdf, "1", bad, &EngineLimits::default()).is_err(),
                "{bad} should be rejected"
            );
        }
        assert!(
            set_page_rotation(&pdf, "9", 90, &EngineLimits::default()).is_err(),
            "bad page range should fail"
        );
    }

    #[test]
    fn reports_rotations_page_level_only() {
        let pdf = pdf_with_pages(&["a", "b", "c"]);
        let out = set_page_rotation(&pdf, "2-3", 270, &EngineLimits::default()).unwrap();
        assert_eq!(
            get_page_rotations(&out, &EngineLimits::default()).unwrap(),
            vec![(1, 0), (2, 270), (3, 270)]
        );
        // Input without any /Rotate reports all zeros.
        assert_eq!(
            get_page_rotations(&pdf, &EngineLimits::default()).unwrap(),
            vec![(1, 0), (2, 0), (3, 0)]
        );
    }

    #[test]
    fn normalize_cleans_redundant_values() {
        // Build a doc with /Rotate 360, -90 and 45 on three pages.
        let mut pdf = pdf_with_pages(&["a", "b", "c"]);
        {
            let mut doc = Document::load_mem(&pdf).unwrap();
            let pages = doc.get_pages();
            for (page, val) in [(1u32, 360i64), (2, -90), (3, 45)] {
                doc.get_dictionary_mut(pages[&page])
                    .unwrap()
                    .set("Rotate", Object::Integer(val));
            }
            let mut buf = Vec::new();
            doc.save_to(&mut buf).unwrap();
            pdf = buf;
        }
        let out = normalize_rotations(&pdf, &EngineLimits::default()).unwrap();
        let doc = Document::load_mem(&out).unwrap();
        let pages = doc.get_pages();
        // 360 → key removed entirely.
        assert!(
            doc.get_dictionary(pages[&1])
                .unwrap()
                .get(b"Rotate")
                .is_err()
        );
        // -90 → 270.
        assert_eq!(rotate_of(&out, 2), Some(270));
        // 45 is not a multiple of 90 but still normalizes mod 360.
        assert_eq!(rotate_of(&out, 3), Some(45));
    }

    #[test]
    fn normalize_leaves_missing_alone() {
        let pdf = pdf_with_pages(&["a"]);
        let out = normalize_rotations(&pdf, &EngineLimits::default()).unwrap();
        assert!(Document::load_mem(&out).is_ok());
        assert_eq!(rotate_of(&out, 1), None);
    }

    #[test]
    fn transforms_reject_encrypted() {
        let pdf = pdf_with_pages(&["a"]);
        let enc = encrypt(
            &pdf,
            "user",
            None,
            PdfPermissions::default(),
            &EngineLimits::default(),
        )
        .unwrap();
        assert!(set_page_rotation(&enc, "1", 90, &EngineLimits::default()).is_err());
        assert!(normalize_rotations(&enc, &EngineLimits::default()).is_err());
        // Reads still work on encrypted input.
        assert!(get_page_rotations(&enc, &EngineLimits::default()).is_ok());
    }
}
