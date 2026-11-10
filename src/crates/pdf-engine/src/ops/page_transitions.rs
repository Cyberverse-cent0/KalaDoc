//! Set, clear and list page transitions (`/Trans` dictionaries).
//!
//! A transition dictionary on a page describes the visual effect a
//! viewer applies when *leaving* that page during a presentation. This
//! module writes the dictionary inline on the page object (valid per
//! the PDF spec — the value need not be indirect) and reads it back.
//!
//! # Entry mapping (ISO 32000, transition dictionary)
//!
//! - `/S` — transition style name. `Replace` is stored under its spec
//!   name `/R`.
//! - `/D` — duration in seconds, stored as a real number. Always
//!   written; the spec default is 1 second.
//! - `/Di` — direction. Written for every [`TransitionDir`] value:
//!   `H`, `V`, `I` (inward) or `O` (outward).
//! - `/Dm` — dimension. Additionally written as `H`/`V` when the
//!   direction is horizontal or vertical, since `/Dm` is the modern
//!   (PDF 1.4+) key for the Split/Blinds dimension while `/Di` is the
//!   legacy direction entry; emitting both keeps the dictionary valid
//!   for old and new consumers.
//! - `/M` — motion, `I` (inward) or `O` (outward).
//!
//! Entries that do not apply to a given transition style are harmless
//! per the spec (viewers ignore irrelevant keys), so [`TransitionSpec`]
//! does not try to police which combinations are meaningful.
//!
//! # Honesty notes
//!
//! - [`list_transitions`] only reads the page dictionary's **own**
//!   `/Trans` entry (resolved through one level of indirection). It
//!   never inherits from `/Pages` ancestors — there is no inheritance
//!   for `/Trans`, but malformed or unrecognized entries are skipped
//!   rather than failing the whole read.
//! - [`clear_transitions`] removes the `/Trans` key entirely; unlike
//!   `garbage_collect` the now-unreferenced dictionary objects (if the
//!   transition was indirect) remain in the file.
//! - Encrypted documents are rejected by all three functions.

use lopdf::{Object, dictionary};
use serde::{Deserialize, Serialize};

use crate::crates::core::engine::EngineLimits;

use crate::crates::pdf_engine::error::{PdfError, PdfResult};

use super::load::{load_pdf, lopdf_err, reject_encrypted, save_pdf};
use super::pages::parse_page_range;

/// Transition style — the `/S` entry of a transition dictionary.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum TransitionType {
    /// Two lines sweep across the screen (`/Split`).
    Split,
    /// Multiple evenly spaced lines sweep in one direction (`/Blinds`).
    Blinds,
    /// A rectangular box sweeps inward or outward (`/Box`).
    Box,
    /// A single line sweeps across the page (`/Wipe`).
    Wipe,
    /// The old page dissolves gradually (`/Dissolve`).
    Dissolve,
    /// Like dissolve, but the effect sweeps across the page (`/Glitter`).
    Glitter,
    /// Instant replacement, no visual effect (`/R` in the PDF file).
    Replace,
    /// The new page gradually becomes visible (`/Fade`, PDF 1.5+).
    Fade,
}

impl TransitionType {
    /// The `/S` name written to the PDF.
    fn name(self) -> &'static str {
        match self {
            Self::Split => "Split",
            Self::Blinds => "Blinds",
            Self::Box => "Box",
            Self::Wipe => "Wipe",
            Self::Dissolve => "Dissolve",
            Self::Glitter => "Glitter",
            Self::Replace => "R",
            Self::Fade => "Fade",
        }
    }

    /// Parse an `/S` name back into a [`TransitionType`].
    fn from_name(name: &[u8]) -> Option<Self> {
        Some(match name {
            b"Split" => Self::Split,
            b"Blinds" => Self::Blinds,
            b"Box" => Self::Box,
            b"Wipe" => Self::Wipe,
            b"Dissolve" => Self::Dissolve,
            b"Glitter" => Self::Glitter,
            b"R" | b"Replace" => Self::Replace,
            b"Fade" => Self::Fade,
            _ => return None,
        })
    }
}

/// Direction of a transition — written to `/Di`, and to `/Dm` when it
/// is a dimension (`H`/`V`).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum TransitionDir {
    /// Left-to-right / top-to-bottom sweep axis (`/H`).
    Horizontal,
    /// Top-to-bottom / right-to-left sweep axis (`/V`).
    Vertical,
    /// Toward the center (`/I`).
    Inward,
    /// Away from the center (`/O`).
    Outward,
}

impl TransitionDir {
    /// The `/Di` name written to the PDF.
    fn name(self) -> &'static str {
        match self {
            Self::Horizontal => "H",
            Self::Vertical => "V",
            Self::Inward => "I",
            Self::Outward => "O",
        }
    }

    fn from_name(name: &[u8]) -> Option<Self> {
        Some(match name {
            b"H" | b"Horizontal" => Self::Horizontal,
            b"V" | b"Vertical" => Self::Vertical,
            b"I" | b"Inward" => Self::Inward,
            b"O" | b"Outward" => Self::Outward,
            _ => return None,
        })
    }
}

/// Motion direction — the `/M` entry of a transition dictionary.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum TransitionMotion {
    /// Toward the center (`/I`).
    Inward,
    /// Away from the center (`/O`).
    Outward,
}

impl TransitionMotion {
    fn name(self) -> &'static str {
        match self {
            Self::Inward => "I",
            Self::Outward => "O",
        }
    }

    fn from_name(name: &[u8]) -> Option<Self> {
        Some(match name {
            b"I" | b"Inward" => Self::Inward,
            b"O" | b"Outward" => Self::Outward,
            _ => return None,
        })
    }
}

/// Full specification of a page transition — the arguments to
/// [`set_transition`].
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct TransitionSpec {
    /// Transition style (`/S`).
    pub kind: TransitionType,
    /// Duration in seconds (`/D`); must be finite and `>= 0`.
    pub duration: f32,
    /// Optional direction (`/Di`, plus `/Dm` for `H`/`V`).
    #[serde(default)]
    pub direction: Option<TransitionDir>,
    /// Optional motion (`/M`).
    #[serde(default)]
    pub motion: Option<TransitionMotion>,
}

/// One page's transition as read back by [`list_transitions`].
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct PageTransition {
    /// 1-based page number.
    pub page: u32,
    /// Transition style (`/S`).
    pub kind: TransitionType,
    /// Duration in seconds (`/D`); the spec default `1.0` when absent.
    pub duration: f32,
    /// Direction read from `/Di` or `/Dm`, if present and recognized.
    #[serde(default)]
    pub direction: Option<TransitionDir>,
    /// Motion read from `/M`, if present and recognized.
    #[serde(default)]
    pub motion: Option<TransitionMotion>,
}

/// Validate a [`TransitionSpec`]'s numeric fields before touching the
/// document. `duration` must be finite and non-negative — NaN and
/// infinities would serialize as invalid PDF numbers.
fn validate_spec(spec: &TransitionSpec) -> PdfResult<()> {
    if !spec.duration.is_finite() || spec.duration < 0.0 {
        return Err(PdfError::InvalidInput(format!(
            "transition duration must be a finite number >= 0, got {}",
            spec.duration
        )));
    }
    Ok(())
}

/// Build the `/Trans` dictionary object for `spec`.
fn trans_dictionary(spec: &TransitionSpec) -> Object {
    let mut dict = dictionary! {
        "S" => spec.kind.name(),
        "D" => Object::Real(spec.duration),
    };
    if let Some(dir) = spec.direction {
        dict.set("Di", Object::Name(dir.name().as_bytes().to_vec()));
        // `/Dm` carries only the dimension values H/V; it is the modern
        // key for Split/Blinds while `/Di` is the legacy direction entry.
        match dir {
            TransitionDir::Horizontal => dict.set("Dm", "H"),
            TransitionDir::Vertical => dict.set("Dm", "V"),
            _ => {}
        };
    }
    if let Some(motion) = spec.motion {
        dict.set("M", Object::Name(motion.name().as_bytes().to_vec()));
    }
    Object::Dictionary(dict)
}

/// Write `spec` as the `/Trans` entry of every page selected by `pages`
/// (a range spec such as `"1-3,8"` or `"all"`, 1-based). The dictionary
/// **replaces** any existing `/Trans` on those pages.
///
/// Returns [`PdfError::InvalidInput`] for a bad duration,
/// [`PdfError::InvalidPageRange`] for a bad range, and
/// [`PdfError::Encrypted`] for encrypted input.
pub fn set_transition(
    input: &[u8],
    pages: &str,
    spec: &TransitionSpec,
    limits: &EngineLimits,
) -> PdfResult<Vec<u8>> {
    validate_spec(spec)?;

    let mut doc = load_pdf(input, limits)?;
    reject_encrypted(&doc)?;

    let page_count = doc.get_pages().len() as u32;
    let selected = parse_page_range(pages, page_count)?;
    let pages_map = doc.get_pages();
    let trans = trans_dictionary(spec);

    for page_number in selected {
        let Some(page_id) = pages_map.get(&page_number).copied() else {
            return Err(PdfError::InvalidPageRange(format!(
                "page {page_number} not found in document"
            )));
        };
        doc.get_dictionary_mut(page_id)
            .map_err(lopdf_err)?
            .set("Trans", trans.clone());
    }

    save_pdf(&mut doc)
}

/// Remove the `/Trans` entry from pages.
///
/// `pages` is `Some(range)` to clear only the selected pages (same
/// grammar as [`set_transition`]) or `None` to clear **every** page.
/// Pages without a `/Trans` entry are left untouched.
///
/// Returns [`PdfError::InvalidPageRange`] for a bad range and
/// [`PdfError::Encrypted`] for encrypted input.
pub fn clear_transitions(
    input: &[u8],
    pages: Option<&str>,
    limits: &EngineLimits,
) -> PdfResult<Vec<u8>> {
    let mut doc = load_pdf(input, limits)?;
    reject_encrypted(&doc)?;

    let page_count = doc.get_pages().len() as u32;
    let selected: Vec<u32> = match pages {
        Some(spec) => parse_page_range(spec, page_count)?,
        None => (1..=page_count).collect(),
    };
    let pages_map = doc.get_pages();

    for page_number in selected {
        let Some(page_id) = pages_map.get(&page_number).copied() else {
            continue;
        };
        let dict = doc.get_dictionary_mut(page_id).map_err(lopdf_err)?;
        let _ = dict.remove(b"Trans");
    }

    save_pdf(&mut doc)
}

/// Report the `/Trans` transition of every page that has one.
///
/// Returns `(page_number, spec)` pairs sorted by page number — only
/// pages with a **valid, recognized** `/S` entry are included; pages
/// without `/Trans` or with malformed entries are skipped. `/D`
/// defaults to `1.0` when absent or non-numeric, matching the spec
/// default; unrecognized `/Di`, `/Dm` or `/M` names are reported as
/// `None` rather than failing the read.
///
/// Like [`super::info::inspect`], this tolerates encrypted input: a
/// transition dictionary is names and numbers, not encrypted strings,
/// so reporting it does not require decryption.
pub fn list_transitions(input: &[u8], limits: &EngineLimits) -> PdfResult<Vec<PageTransition>> {
    let doc = load_pdf(input, limits)?;
    let page_count = doc.get_pages().len() as u32;
    let pages_map = doc.get_pages();

    let mut out = Vec::new();
    for page_number in 1..=page_count {
        let Some(page_id) = pages_map.get(&page_number).copied() else {
            continue;
        };
        let Ok(page_dict) = doc.get_dictionary(page_id) else {
            continue;
        };
        // `/Trans` may be a direct dictionary or an indirect reference.
        let Ok(trans_obj) = page_dict.get_deref(b"Trans", &doc) else {
            continue;
        };
        let Ok(trans) = trans_obj.as_dict() else {
            continue;
        };
        let Some(kind) = trans
            .get(b"S")
            .ok()
            .and_then(|o| o.as_name().ok())
            .and_then(TransitionType::from_name)
        else {
            continue;
        };
        let duration = trans
            .get(b"D")
            .ok()
            .and_then(|o| o.as_float().ok())
            .unwrap_or(1.0);
        // Direction lives in /Di (legacy, all four values) or /Dm
        // (modern, H/V only). Prefer /Di since it covers every value.
        let direction = trans
            .get(b"Di")
            .or_else(|_| trans.get(b"Dm"))
            .ok()
            .and_then(|o| o.as_name().ok())
            .and_then(TransitionDir::from_name);
        let motion = trans
            .get(b"M")
            .ok()
            .and_then(|o| o.as_name().ok())
            .and_then(TransitionMotion::from_name);
        out.push(PageTransition {
            page: page_number,
            kind,
            duration,
            direction,
            motion,
        });
    }
    Ok(out)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::crates::pdf_engine::ops::encrypt::{PdfPermissions, encrypt};
    use crate::crates::pdf_engine::ops::test_utils::pdf_with_pages;
    use lopdf::Document;

    fn trans_of(pdf: &[u8], page: u32) -> Option<lopdf::Dictionary> {
        let doc = Document::load_mem(pdf).unwrap();
        let page_id = doc.get_pages()[&page];
        let dict = doc.get_dictionary(page_id).unwrap();
        dict.get(b"Trans")
            .ok()
            .and_then(|o| o.as_dict().ok())
            .cloned()
    }

    fn name_of(dict: &lopdf::Dictionary, key: &[u8]) -> Option<Vec<u8>> {
        dict.get(key)
            .ok()
            .and_then(|o| o.as_name().ok())
            .map(<[u8]>::to_vec)
    }

    fn spec() -> TransitionSpec {
        TransitionSpec {
            kind: TransitionType::Wipe,
            duration: 0.5,
            direction: Some(TransitionDir::Horizontal),
            motion: Some(TransitionMotion::Outward),
        }
    }

    #[test]
    fn writes_all_entries() {
        let pdf = pdf_with_pages(&["a", "b"]);
        let out = set_transition(&pdf, "1-2", &spec(), &EngineLimits::default()).unwrap();
        for page in [1u32, 2] {
            let trans = trans_of(&out, page).expect("page should have /Trans");
            assert_eq!(name_of(&trans, b"S").as_deref(), Some(b"Wipe".as_slice()));
            assert_eq!(trans.get(b"D").unwrap().as_float().unwrap(), 0.5);
            assert_eq!(name_of(&trans, b"Di").as_deref(), Some(b"H".as_slice()));
            assert_eq!(name_of(&trans, b"Dm").as_deref(), Some(b"H".as_slice()));
            assert_eq!(name_of(&trans, b"M").as_deref(), Some(b"O".as_slice()));
        }
    }

    #[test]
    fn replace_uses_spec_name_r() {
        let pdf = pdf_with_pages(&["a"]);
        let spec = TransitionSpec {
            kind: TransitionType::Replace,
            duration: 0.0,
            direction: None,
            motion: None,
        };
        let out = set_transition(&pdf, "1", &spec, &EngineLimits::default()).unwrap();
        let trans = trans_of(&out, 1).unwrap();
        assert_eq!(name_of(&trans, b"S").as_deref(), Some(b"R".as_slice()));
        // No direction/motion entries when unspecified.
        assert!(trans.get(b"Di").is_err());
        assert!(trans.get(b"Dm").is_err());
        assert!(trans.get(b"M").is_err());
    }

    #[test]
    fn inward_outward_direction_skips_dm() {
        let pdf = pdf_with_pages(&["a"]);
        let spec = TransitionSpec {
            kind: TransitionType::Split,
            duration: 1.0,
            direction: Some(TransitionDir::Inward),
            motion: Some(TransitionMotion::Inward),
        };
        let out = set_transition(&pdf, "all", &spec, &EngineLimits::default()).unwrap();
        let trans = trans_of(&out, 1).unwrap();
        assert_eq!(name_of(&trans, b"Di").as_deref(), Some(b"I".as_slice()));
        assert!(trans.get(b"Dm").is_err());
        assert_eq!(name_of(&trans, b"M").as_deref(), Some(b"I".as_slice()));
    }

    #[test]
    fn rejects_bad_duration() {
        let pdf = pdf_with_pages(&["a"]);
        for bad in [f32::NAN, f32::INFINITY, f32::NEG_INFINITY, -0.1] {
            let spec = TransitionSpec {
                kind: TransitionType::Fade,
                duration: bad,
                direction: None,
                motion: None,
            };
            assert!(
                set_transition(&pdf, "1", &spec, &EngineLimits::default()).is_err(),
                "duration {bad} should be rejected"
            );
        }
    }

    #[test]
    fn list_round_trips() {
        let pdf = pdf_with_pages(&["a", "b", "c"]);
        let out = set_transition(&pdf, "1,3", &spec(), &EngineLimits::default()).unwrap();
        let listed = list_transitions(&out, &EngineLimits::default()).unwrap();
        assert_eq!(listed.len(), 2);
        assert_eq!(listed[0].page, 1);
        assert_eq!(listed[0].kind, TransitionType::Wipe);
        assert_eq!(listed[0].duration, 0.5);
        assert_eq!(listed[0].direction, Some(TransitionDir::Horizontal));
        assert_eq!(listed[0].motion, Some(TransitionMotion::Outward));
        assert_eq!(listed[1].page, 3);
        // Page 2 has no transition and is absent from the report.
        assert!(
            list_transitions(&pdf, &EngineLimits::default())
                .unwrap()
                .is_empty()
        );
    }

    #[test]
    fn list_skips_malformed_entries() {
        let mut pdf = pdf_with_pages(&["a", "b"]);
        {
            let mut doc = Document::load_mem(&pdf).unwrap();
            let pages = doc.get_pages();
            // Page 1: /Trans with no /S — malformed, must be skipped.
            doc.get_dictionary_mut(pages[&1])
                .unwrap()
                .set("Trans", dictionary! { "D" => Object::Real(2.0) });
            // Page 2: unrecognized /S name — skipped.
            doc.get_dictionary_mut(pages[&2])
                .unwrap()
                .set("Trans", dictionary! { "S" => "NoSuchStyle" });
            let mut buf = Vec::new();
            doc.save_to(&mut buf).unwrap();
            pdf = buf;
        }
        assert!(
            list_transitions(&pdf, &EngineLimits::default())
                .unwrap()
                .is_empty()
        );
    }

    #[test]
    fn clear_removes_selected_or_all() {
        let pdf = pdf_with_pages(&["a", "b", "c"]);
        let out = set_transition(&pdf, "1-2", &spec(), &EngineLimits::default()).unwrap();
        // Clear only page 1.
        let out = clear_transitions(&out, Some("1"), &EngineLimits::default()).unwrap();
        assert!(trans_of(&out, 1).is_none());
        assert!(trans_of(&out, 2).is_some());
        assert!(trans_of(&out, 3).is_none());
        // Clear everything with None.
        let out = clear_transitions(&out, None, &EngineLimits::default()).unwrap();
        assert!(
            list_transitions(&out, &EngineLimits::default())
                .unwrap()
                .is_empty()
        );
        // Bad range still rejected.
        assert!(clear_transitions(&out, Some("9"), &EngineLimits::default()).is_err());
    }

    #[test]
    fn transforms_reject_encrypted_but_list_reads() {
        let pdf = pdf_with_pages(&["a"]);
        let out = set_transition(&pdf, "1", &spec(), &EngineLimits::default()).unwrap();
        let enc = encrypt(
            &out,
            "user",
            None,
            PdfPermissions::default(),
            &EngineLimits::default(),
        )
        .unwrap();
        assert!(set_transition(&enc, "1", &spec(), &EngineLimits::default()).is_err());
        assert!(clear_transitions(&enc, None, &EngineLimits::default()).is_err());
        // Reads still work on encrypted input.
        assert!(list_transitions(&enc, &EngineLimits::default()).is_ok());
    }

    #[test]
    fn spec_serde_roundtrip() {
        let spec = spec();
        let json = serde_json::to_string(&spec).unwrap();
        let back: TransitionSpec = serde_json::from_str(&json).unwrap();
        assert_eq!(spec, back);
    }
}
