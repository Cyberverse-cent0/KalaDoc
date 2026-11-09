//! Automatic page-orientation detection and normalization.
//!
//! [`detect_orientation`] reports, per page, the geometry-derived
//! orientation of the page and the absolute `/Rotate` value (0/90/180/
//! 270) that would present it as **portrait**. [`normalize_orientation`]
//! rewrites each page's `/Rotate` so the *effective* on-screen
//! orientation matches a requested [`OrientMode`].
//!
//! # What is — and is not — detected
//!
//! This module uses **page geometry only**: the effective `/MediaBox`
//! (inherited from `/Pages` ancestors when the page lacks one, like
//! `crop`) combined with the page-level `/Rotate`. It does **not**
//! implement text-direction or computer-vision detection — there is no
//! way to know whether a landscape page contains sideways text or a
//! legitimate wide table. Every [`PageOrientation`] therefore carries a
//! `confidence` that stays modest and a `reasons` entry stating that
//! text-orientation detection is not implemented.
//!
//! # Conventions
//!
//! - Only the page dictionary's **own** `/Rotate` is read, matching
//!   `rotate`/`rotate_view`: an inherited `/Rotate` from a `/Pages`
//!   ancestor is not resolved, and a page-level value is assumed to
//!   override it for viewers.
//! - `/Rotate` values that are not multiples of 90 are tolerated by
//!   folding them modulo 360; the axis-swap test uses `rot / 90 % 2`,
//!   which is exact for right-angle rotations and an approximation for
//!   anything else.
//! - [`detect_orientation`] is a read-only inspection op: like
//!   `inspect`/`get_page_rotations` it **tolerates encrypted input**,
//!   because `/MediaBox` and `/Rotate` are numbers, not encrypted
//!   strings.
//! - [`normalize_orientation`] modifies the document, so it rejects
//!   encrypted input via [`reject_encrypted`] — except `OrientMode::AsIs`,
//!   which changes nothing and returns the input bytes untouched.
//!
//! # Normalization semantics
//!
//! For `Portrait`/`Landscape`, each page whose effective orientation
//! does not already match is assigned the "closest" canonical rotation
//! (smallest cyclic distance from its current `/Rotate`; ties prefer
//! the order 0, 90, 180, 270 so upright 0 wins over upside-down 180).
//! When both 90 and 270 would work — genuinely ambiguous without looking
//! at content — 90 (clockwise) is chosen. Pages that already display in
//! the requested orientation, and pages with a square MediaBox (no
//! meaningful portrait/landscape distinction), are left untouched.
//! The page **content stream is never modified** — this is a
//! metadata-only fix; viewers rotate the page at display time.

use lopdf::Object;

use serde::{Deserialize, Serialize};

use crate::crates::core::engine::EngineLimits;

use crate::crates::pdf_engine::error::PdfResult;

use super::crop::{DEFAULT_MEDIA_BOX, inherited_rect};
use super::load::{load_pdf, lopdf_err, reject_encrypted, save_pdf};
use super::rotate_view::numeric_rotation;

/// Geometry-derived orientation report for one page.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct PageOrientation {
    /// 1-based page number, matching the CLI/MCP contract.
    pub page: u32,
    /// MediaBox width in points (`x1 - x0`, absolute value).
    pub media_w: f64,
    /// MediaBox height in points (`y1 - y0`, absolute value).
    pub media_h: f64,
    /// The absolute `/Rotate` value (one of 0, 90, 180, 270) the page
    /// should carry to display **portrait** with minimal change from
    /// its current rotation. Equals the current rotation when the page
    /// already displays portrait; see module docs for tie-breaking.
    pub suggested_rotation: u16,
    /// Confidence in `suggested_rotation`, roughly 0.3–0.8. Based on
    /// aspect-ratio strength only — never high, because text-direction
    /// detection is not implemented.
    pub confidence: f64,
    /// Human-readable explanation of the heuristic signals used.
    pub reasons: Vec<String>,
}

/// Requested effective page orientation for [`normalize_orientation`].
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum OrientMode {
    /// Every page should display taller than wide.
    Portrait,
    /// Every page should display wider than tall.
    Landscape,
    /// Leave every page untouched; the input is returned as-is after
    /// validation. Useful for callers that want a uniform "normalize"
    /// entry point without forcing a direction.
    AsIs,
}

/// Effective on-screen orientation of a page, after `/Rotate`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum DisplayOrientation {
    Portrait,
    Landscape,
    /// Width equals height; no meaningful distinction.
    Square,
}

/// Inspect `input` and report a [`PageOrientation`] for every page.
///
/// Read-only and tolerant of encrypted documents (numbers are not
/// encrypted). Pages whose effective MediaBox is missing *and* cannot
/// fall back to the default letter size, or whose box is degenerate
/// (non-positive extent), are skipped rather than failing the whole
/// report.
pub fn detect_orientation(input: &[u8], limits: &EngineLimits) -> PdfResult<Vec<PageOrientation>> {
    let doc = load_pdf(input, limits)?;
    let page_count = doc.get_pages().len() as u32;
    let pages_map = doc.get_pages();

    let mut out = Vec::with_capacity(page_count as usize);
    for page_number in 1..=page_count {
        let Some(&page_id) = pages_map.get(&page_number) else {
            continue;
        };
        let media = inherited_rect(&doc, page_id, b"MediaBox")?.unwrap_or(DEFAULT_MEDIA_BOX);
        let w = (media[2] - media[0]).abs();
        let h = (media[3] - media[1]).abs();
        if !(w.is_finite() && h.is_finite() && w > 0.0 && h > 0.0) {
            continue;
        }
        let rotate = page_rotation(&doc, page_id);
        let display = display_orientation(w, h, rotate);
        let suggested = choose_rotation(w, h, rotate, Some(false));

        let mut reasons = Vec::new();
        reasons.push(format!(
            "MediaBox is {:.0}x{:.0} pt; /Rotate is {}",
            w, h, rotate
        ));
        if axes_swapped(rotate) {
            reasons.push(format!(
                "/Rotate {rotate} swaps the media box axes for display"
            ));
        }
        match display {
            DisplayOrientation::Portrait => reasons.push(format!(
                "effective display size {:.0}x{:.0} pt is portrait",
                display_w(w, h, rotate),
                display_h(w, h, rotate)
            )),
            DisplayOrientation::Landscape => reasons.push(format!(
                "effective display size {:.0}x{:.0} pt is landscape",
                display_w(w, h, rotate),
                display_h(w, h, rotate)
            )),
            DisplayOrientation::Square => {
                reasons.push("media box is square; portrait/landscape is ambiguous".to_string())
            }
        }
        if suggested != rotate.rem_euclid(360) as u16 {
            reasons.push(format!(
                "suggested /Rotate {suggested} to present the page portrait"
            ));
        }
        reasons.push(
            "text-direction detection is not implemented; suggestion is based on page geometry only"
                .to_string(),
        );

        out.push(PageOrientation {
            page: page_number,
            media_w: w,
            media_h: h,
            suggested_rotation: suggested,
            confidence: orientation_confidence(w, h, rotate),
            reasons,
        });
    }
    Ok(out)
}

/// Rewrite `/Rotate` on every page so the effective display orientation
/// matches `mode`.
///
/// Metadata-only: page content streams, MediaBox and everything else
/// are untouched — viewers apply `/Rotate` at display time.
/// `OrientMode::AsIs` returns `input` unchanged (still limit-checked
/// by the load). `Portrait`/`Landscape` reject encrypted input and
/// leave pages that already match, or whose MediaBox is square,
/// unchanged. See module docs for rotation tie-breaking.
pub fn normalize_orientation(
    input: &[u8],
    mode: OrientMode,
    limits: &EngineLimits,
) -> PdfResult<Vec<u8>> {
    let mut doc = load_pdf(input, limits)?;
    let want_landscape = match mode {
        OrientMode::AsIs => return Ok(input.to_vec()),
        OrientMode::Portrait => Some(false),
        OrientMode::Landscape => Some(true),
    };
    reject_encrypted(&doc)?;

    let page_count = doc.get_pages().len() as u32;
    let pages_map = doc.get_pages();

    for page_number in 1..=page_count {
        let Some(&page_id) = pages_map.get(&page_number) else {
            continue;
        };
        let media = inherited_rect(&doc, page_id, b"MediaBox")?.unwrap_or(DEFAULT_MEDIA_BOX);
        let w = (media[2] - media[0]).abs();
        let h = (media[3] - media[1]).abs();
        if !(w.is_finite() && h.is_finite() && w > 0.0 && h > 0.0) {
            continue;
        }
        let rotate = page_rotation(&doc, page_id);
        let target = choose_rotation(w, h, rotate, want_landscape);
        let current_norm = rotate.rem_euclid(360);
        if i64::from(target) != current_norm {
            doc.get_dictionary_mut(page_id)
                .map_err(lopdf_err)?
                .set("Rotate", Object::Integer(i64::from(target)));
        }
    }

    save_pdf(&mut doc)
}

/// The page dictionary's own `/Rotate`, normalized to `0..360`.
/// Non-numeric or absent entries report `0` — the spec default.
fn page_rotation(doc: &lopdf::Document, page_id: lopdf::ObjectId) -> i64 {
    doc.get_dictionary(page_id)
        .ok()
        .and_then(|dict| dict.get(b"Rotate").ok())
        .and_then(numeric_rotation)
        .unwrap_or(0)
        .rem_euclid(360)
}

/// Whether a normalized `/Rotate` swaps the media box axes on display.
/// Exact for right-angle rotations; approximate otherwise (see module
/// docs).
fn axes_swapped(rotate: i64) -> bool {
    (rotate / 90).rem_euclid(2) == 1
}

fn display_w(w: f64, h: f64, rotate: i64) -> f64 {
    if axes_swapped(rotate) { h } else { w }
}

fn display_h(w: f64, h: f64, rotate: i64) -> f64 {
    if axes_swapped(rotate) { w } else { h }
}

fn display_orientation(w: f64, h: f64, rotate: i64) -> DisplayOrientation {
    let dw = display_w(w, h, rotate);
    let dh = display_h(w, h, rotate);
    if dw > dh {
        DisplayOrientation::Landscape
    } else if dh > dw {
        DisplayOrientation::Portrait
    } else {
        DisplayOrientation::Square
    }
}

/// Pick the canonical `/Rotate` (0/90/180/270) that makes the page
/// display in the wanted orientation (`want_landscape`: `Some(true)` =
/// landscape, `Some(false)` = portrait), changing as little as possible
/// from `current`.
///
/// Candidates are filtered to those producing the wanted display
/// orientation, then the one with the smallest cyclic distance from
/// `current` wins; ties resolve in the order 0, 90, 180, 270 so upright
/// beats upside-down. A square media box has no orientation to fix, so
/// `current` (normalized) is returned unchanged.
fn choose_rotation(w: f64, h: f64, current: i64, want_landscape: Option<bool>) -> u16 {
    let current_norm = current.rem_euclid(360);
    if w == h {
        return current_norm as u16;
    }
    let mut best: Option<(u32, u16)> = None;
    for candidate in [0u16, 90, 180, 270] {
        let disp = display_orientation(w, h, i64::from(candidate));
        let matches = match want_landscape {
            Some(true) => disp == DisplayOrientation::Landscape,
            Some(false) => disp == DisplayOrientation::Portrait,
            None => true,
        };
        if !matches {
            continue;
        }
        let dist = cyclic_distance(current_norm, i64::from(candidate));
        if best.map_or(true, |(d, _)| dist < d) {
            best = Some((dist, candidate));
        }
    }
    // Unreachable in practice — {0,90,180,270} always contains both
    // parities — but never panic on logic errors.
    best.map(|(_, r)| r).unwrap_or(current_norm as u16)
}

/// Smallest clockwise-or-counterclockwise distance between two angles
/// in `0..360`.
fn cyclic_distance(a: i64, b: i64) -> u32 {
    let d = (a - b).abs() % 360;
    d.min(360 - d) as u32
}

/// Confidence in the portrait suggestion, derived from how strongly
/// the effective aspect ratio departs from square. Deliberately modest:
/// geometry alone cannot confirm text direction.
fn orientation_confidence(w: f64, h: f64, rotate: i64) -> f64 {
    let dw = display_w(w, h, rotate);
    let dh = display_h(w, h, rotate);
    let (long, short) = if dw >= dh { (dw, dh) } else { (dh, dw) };
    if short <= 0.0 {
        return 0.3;
    }
    let aspect = long / short;
    if aspect <= 1.0 {
        0.3
    } else {
        // aspect 1.0 → 0.5, aspect → ∞ → 0.8
        (0.5 + 0.3 * (1.0 - 1.0 / aspect)).clamp(0.0, 1.0)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::crates::pdf_engine::ops::crop::number_object;
    use crate::crates::pdf_engine::ops::encrypt::{PdfPermissions, encrypt};
    use lopdf::{Document, Stream, dictionary};

    /// Build a one-page PDF with an explicit MediaBox and optional
    /// page-level `/Rotate`.
    fn one_page_pdf(media: [f64; 4], rotate: Option<i64>) -> Vec<u8> {
        let mut doc = Document::with_version("1.5");
        let pages_id = doc.new_object_id();
        let content_id = doc.add_object(Stream::new(dictionary! {}, Vec::new()));
        let media_objects: Vec<Object> = media.iter().map(|&v| number_object(v)).collect();
        let mut page = dictionary! {
            "Type" => "Page",
            "Parent" => pages_id,
            "Contents" => content_id,
            "MediaBox" => media_objects,
        };
        if let Some(r) = rotate {
            page.set("Rotate", Object::Integer(r));
        }
        let page_id = doc.add_object(Object::Dictionary(page));
        let pages = dictionary! {
            "Type" => "Pages",
            "Kids" => vec![Object::Reference(page_id)],
            "Count" => 1i64,
            "MediaBox" => media.iter().map(|&v| number_object(v)).collect::<Vec<_>>(),
        };
        doc.objects.insert(pages_id, Object::Dictionary(pages));
        let catalog_id = doc.add_object(dictionary! {
            "Type" => "Catalog",
            "Pages" => pages_id,
        });
        doc.trailer.set("Root", catalog_id);
        let mut buf = Vec::new();
        doc.save_to(&mut buf).unwrap();
        buf
    }

    fn rotate_of(pdf: &[u8]) -> Option<i64> {
        let doc = Document::load_mem(pdf).unwrap();
        let page_id = doc.get_pages()[&1];
        doc.get_dictionary(page_id)
            .unwrap()
            .get(b"Rotate")
            .ok()
            .and_then(|o| o.as_i64().ok())
    }

    const PORTRAIT: [f64; 4] = [0.0, 0.0, 595.0, 842.0];
    const LANDSCAPE: [f64; 4] = [0.0, 0.0, 842.0, 595.0];

    #[test]
    fn detects_portrait_media_without_rotation() {
        let pdf = one_page_pdf(PORTRAIT, None);
        let out = detect_orientation(&pdf, &EngineLimits::default()).unwrap();
        assert_eq!(out.len(), 1);
        let p = &out[0];
        assert_eq!(p.page, 1);
        assert!((p.media_w - 595.0).abs() < 0.01);
        assert!((p.media_h - 842.0).abs() < 0.01);
        assert_eq!(p.suggested_rotation, 0);
        assert!(p.confidence > 0.5 && p.confidence < 0.9);
        assert!(p.reasons.iter().any(|r| r.contains("geometry only")));
    }

    #[test]
    fn detects_landscape_media() {
        let pdf = one_page_pdf(LANDSCAPE, None);
        let out = detect_orientation(&pdf, &EngineLimits::default()).unwrap();
        let p = &out[0];
        assert_eq!(p.suggested_rotation, 90, "landscape should suggest 90");
        assert!(p.reasons.iter().any(|r| r.contains("landscape")));
    }

    #[test]
    fn stray_rotation_on_portrait_media_suggests_removal() {
        // Portrait media box but /Rotate 90 → displays landscape.
        // Minimal fix is back to 0 (preferred over upside-down 180).
        let pdf = one_page_pdf(PORTRAIT, Some(90));
        let out = detect_orientation(&pdf, &EngineLimits::default()).unwrap();
        assert_eq!(out[0].suggested_rotation, 0);
        assert!(out[0].reasons.iter().any(|r| r.contains("swaps")));
    }

    #[test]
    fn upside_down_but_portrait_keeps_rotation() {
        // /Rotate 180 on portrait media still displays portrait (upside
        // down, but geometry cannot detect that) → keep 180.
        let pdf = one_page_pdf(PORTRAIT, Some(180));
        let out = detect_orientation(&pdf, &EngineLimits::default()).unwrap();
        assert_eq!(out[0].suggested_rotation, 180);
    }

    #[test]
    fn non_right_angle_rotation_is_tolerated() {
        let pdf = one_page_pdf(PORTRAIT, Some(45));
        let out = detect_orientation(&pdf, &EngineLimits::default()).unwrap();
        assert_eq!(out.len(), 1);
        // 45/90 = 0 → treated as unrotated: portrait, keep current.
        assert_eq!(out[0].suggested_rotation, 45);
    }

    #[test]
    fn normalize_portrait_on_landscape_sets_rotate() {
        let pdf = one_page_pdf(LANDSCAPE, None);
        let out =
            normalize_orientation(&pdf, OrientMode::Portrait, &EngineLimits::default()).unwrap();
        assert_eq!(rotate_of(&out), Some(90));
        // Roundtrip: detection on the normalized output agrees.
        let detected = detect_orientation(&out, &EngineLimits::default()).unwrap();
        assert_eq!(detected[0].suggested_rotation, 90);
        // Content untouched: still a valid loadable PDF.
        assert!(Document::load_mem(&out).is_ok());
    }

    #[test]
    fn normalize_landscape_on_portrait_sets_rotate() {
        let pdf = one_page_pdf(PORTRAIT, None);
        let out =
            normalize_orientation(&pdf, OrientMode::Landscape, &EngineLimits::default()).unwrap();
        assert_eq!(rotate_of(&out), Some(90));
        let detected = detect_orientation(&out, &EngineLimits::default()).unwrap();
        // Displayed landscape; portrait suggestion flips it back.
        assert_eq!(detected[0].suggested_rotation, 0);
    }

    #[test]
    fn normalize_is_idempotent_and_minimal() {
        // Portrait media + stray /Rotate 90 displays landscape; Portrait
        // mode should restore 0, not pile on another 90.
        let pdf = one_page_pdf(PORTRAIT, Some(90));
        let out =
            normalize_orientation(&pdf, OrientMode::Portrait, &EngineLimits::default()).unwrap();
        assert_eq!(rotate_of(&out), Some(0));
        // Second pass changes nothing.
        let out2 =
            normalize_orientation(&out, OrientMode::Portrait, &EngineLimits::default()).unwrap();
        assert_eq!(rotate_of(&out2), Some(0));
    }

    #[test]
    fn normalize_asis_returns_input() {
        let pdf = one_page_pdf(LANDSCAPE, Some(270));
        let out = normalize_orientation(&pdf, OrientMode::AsIs, &EngineLimits::default()).unwrap();
        assert_eq!(out, pdf);
    }

    #[test]
    fn square_media_is_left_alone() {
        let pdf = one_page_pdf([0.0, 0.0, 500.0, 500.0], Some(90));
        let detected = detect_orientation(&pdf, &EngineLimits::default()).unwrap();
        assert_eq!(detected[0].confidence, 0.3);
        let out =
            normalize_orientation(&pdf, OrientMode::Landscape, &EngineLimits::default()).unwrap();
        assert_eq!(rotate_of(&out), Some(90), "square page keeps its rotation");
    }

    #[test]
    fn encrypted_detect_ok_normalize_rejected() {
        let pdf = one_page_pdf(LANDSCAPE, None);
        let enc = encrypt(
            &pdf,
            "user",
            None,
            PdfPermissions::default(),
            &EngineLimits::default(),
        )
        .unwrap();
        // Read-only detection tolerates encryption (numbers aren't encrypted).
        assert!(detect_orientation(&enc, &EngineLimits::default()).is_ok());
        // Transforming modes reject it; AsIs still returns input.
        assert!(
            normalize_orientation(&enc, OrientMode::Portrait, &EngineLimits::default()).is_err()
        );
        assert_eq!(
            normalize_orientation(&enc, OrientMode::AsIs, &EngineLimits::default()).unwrap(),
            enc
        );
    }

    #[test]
    fn garbage_input_errors_not_panics() {
        assert!(detect_orientation(b"not a pdf", &EngineLimits::default()).is_err());
        assert!(
            normalize_orientation(b"not a pdf", OrientMode::Portrait, &EngineLimits::default())
                .is_err()
        );
    }
}
