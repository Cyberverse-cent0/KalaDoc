//! Freehand ink annotations (`/Subtype /Ink`).
//!
//! [`add_ink`] appends one `/Ink` annotation per selected page; every
//! annotation carries the same [`InkSpec::strokes`] in the page's
//! default user space. Ink annotations render as smooth freehand
//! strokes in conforming viewers without any appearance stream — like
//! `/Text` comments, the viewer draws them itself.
//!
//! # `/InkList` encoding
//!
//! Per PDF 32000 §12.5.6.13, `/InkList` is an array of stroked paths;
//! each path is an array of points, and each point is an array of two
//! numbers `[x y]` in default user space. Callers supply polylines
//! ([`Stroke::points`]); this op emits each polyline **directly** as a
//! point array — it does *not* synthesize Bézier control points. The
//! spec's "bezier-ish" phrasing refers to the fact that viewers may
//! smooth/interpolate the vertex sequence when stroking; a dense
//! polyline is therefore the faithful representation of sampled ink.
//! (An earlier alternative — converting each segment into a cubic
//! quad — was rejected because it changes the drawn geometry and adds
//! no information the viewer cannot recover from the vertices.)
//!
//! # Annotation dictionary
//!
//! Each annotation gets:
//!
//! - `/Type /Annot`, `/Subtype /Ink`
//! - `/InkList` — nested point arrays as described above
//! - `/Rect` — bounding box of all stroke points padded by
//!   `width / 2 + 2` pt so the stroke edge is not clipped
//! - `/BS` — `<< /W <width> /S /S >>` solid border style
//! - `/C` — `[r g b]` in 0.0–1.0, derived from the u8 RGB in the spec
//! - `/F 4` — print flag (ink should appear in print output)
//!
//! No `/AP`, `/M`, `/NM` or `/T` entries are written (same trade-offs
//! as `annotations.rs`).

use lopdf::{Document, Object, ObjectId, dictionary};
use serde::{Deserialize, Serialize};

use crate::crates::core::engine::EngineLimits;

use crate::crates::pdf_engine::error::{PdfError, PdfResult};

use super::annotations::AnnotFilter;
use super::load::{load_pdf, lopdf_err, reject_encrypted, save_pdf};
use super::watermark::{media_box, selected_pages};

/// Maximum number of strokes accepted per [`InkSpec`]. Guards against
/// pathological inputs that would bloat the `/InkList` array.
const MAX_INK_STROKES: usize = 256;

/// Maximum number of points accepted per [`Stroke`].
const MAX_INK_POINTS_PER_STROKE: usize = 10_000;

/// Padding (in points) added around the ink bounding box for `/Rect`,
/// on top of half the stroke width.
const INK_RECT_PADDING: f64 = 2.0;

/// One freehand stroke: an ordered polyline in default user space.
///
/// Coordinates are PDF points with the origin at the bottom-left of
/// the page's `/MediaBox` (y grows upward). At least two points are
/// required; all coordinates must be finite.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Stroke {
    /// Polyline vertices as `[x, y]` pairs, in drawing order.
    pub points: Vec<[f64; 2]>,
}

/// Specification for [`add_ink`].
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct InkSpec {
    /// Strokes to draw; the same set is applied to every selected page.
    pub strokes: Vec<Stroke>,
    /// Optional 1-based page range (`"1-3,8"`, `"all"`). `None` = every page.
    pub pages: Option<String>,
    /// Stroke color as 8-bit `[r, g, b]`; each channel is scaled to
    /// 0.0–1.0 for the annotation `/C` entry.
    pub color: [u8; 3],
    /// Stroke width in points, written to `/BS /W`. Must be positive
    /// and finite.
    pub width: f32,
}

/// One row of [`list_ink`] output.
#[derive(Debug, Clone, Serialize)]
pub struct InkInfo {
    /// 1-based page number the annotation sits on.
    pub page: u32,
    /// `/Rect` as `[x0, y0, x1, y1]`, or `None` when absent/malformed.
    pub rect: Option<[f64; 4]>,
    /// Number of stroked paths in `/InkList` (0 when malformed/absent).
    pub stroke_count: usize,
    /// `/C` color as `[r, g, b]` floats in 0.0–1.0, when present.
    pub color: Option<[f64; 3]>,
}

// ---------------------------------------------------------------------------
// Shared helpers (self-contained copies of the annotations.rs patterns —
// those helpers are private to that module).
// ---------------------------------------------------------------------------

/// Resolve the `pages` selection of `doc` to `(page_number, page_id)`
/// pairs in page order.
fn selected_page_ids(doc: &Document, pages: Option<&str>) -> PdfResult<Vec<(u32, ObjectId)>> {
    let wanted = selected_pages(doc, pages)?;
    let pages_map = doc.get_pages();
    Ok(wanted
        .into_iter()
        .filter_map(|n| pages_map.get(&n).map(|id| (n, *id)))
        .collect())
}

/// Read a page's `/Annots` array as cloned [`Object`]s — the array may
/// be direct or a single indirection, and entries may be direct
/// dictionaries or references.
fn annots_entries(doc: &Document, page_id: ObjectId) -> Vec<Object> {
    let Ok(page) = doc.get_dictionary(page_id) else {
        return Vec::new();
    };
    let Ok(annots) = page.get(b"Annots") else {
        return Vec::new();
    };
    let array = match annots {
        Object::Array(items) => Some(items.clone()),
        Object::Reference(id) => doc
            .get_object(*id)
            .ok()
            .and_then(|o| o.as_array().ok())
            .cloned(),
        _ => None,
    };
    array.unwrap_or_default()
}

/// Resolve an `/Annots` entry to a dictionary (direct or referenced).
fn resolve_dict<'a>(doc: &'a Document, entry: &'a Object) -> Option<&'a lopdf::Dictionary> {
    match entry {
        Object::Dictionary(d) => Some(d),
        Object::Reference(id) => doc.get_dictionary(*id).ok(),
        _ => None,
    }
}

/// Parse a `/Rect`-style 4-number array into `[x0, y0, x1, y1]`.
fn rect_from_object(obj: &Object) -> Option<[f64; 4]> {
    let items = obj.as_array().ok()?;
    if items.len() != 4 {
        return None;
    }
    let mut out = [0.0; 4];
    for (i, item) in items.iter().enumerate() {
        out[i] = item.as_float().ok()? as f64;
    }
    Some(out)
}

/// Append `entry` to the page's `/Annots` array, creating it when
/// absent. Any pre-existing array (direct or indirect) is preserved by
/// cloning its entries into a fresh direct array.
fn append_annot(doc: &mut Document, page_id: ObjectId, entry: Object) -> PdfResult<()> {
    let mut entries = annots_entries(doc, page_id);
    entries.push(entry);
    let page = doc.get_dictionary_mut(page_id).map_err(lopdf_err)?;
    page.set("Annots", Object::Array(entries));
    Ok(())
}

// ---------------------------------------------------------------------------
// InkList / geometry
// ---------------------------------------------------------------------------

/// Validate the spec's strokes and return the flat bounding box
/// `[x0, y0, x1, y1]` over every point of every stroke.
fn validate_strokes(strokes: &[Stroke]) -> PdfResult<[f64; 4]> {
    if strokes.is_empty() {
        return Err(PdfError::InvalidInput(
            "ink spec must contain at least one stroke".to_string(),
        ));
    }
    if strokes.len() > MAX_INK_STROKES {
        return Err(PdfError::LimitExceeded(format!(
            "stroke count {} exceeds the {MAX_INK_STROKES} stroke cap",
            strokes.len()
        )));
    }
    let mut bbox = [
        f64::INFINITY,
        f64::INFINITY,
        f64::NEG_INFINITY,
        f64::NEG_INFINITY,
    ];
    for (i, stroke) in strokes.iter().enumerate() {
        if stroke.points.len() < 2 {
            return Err(PdfError::InvalidInput(format!(
                "stroke {i} must contain at least 2 points, got {}",
                stroke.points.len()
            )));
        }
        if stroke.points.len() > MAX_INK_POINTS_PER_STROKE {
            return Err(PdfError::LimitExceeded(format!(
                "stroke {i} has {} points, exceeding the {MAX_INK_POINTS_PER_STROKE} point cap",
                stroke.points.len()
            )));
        }
        for (j, p) in stroke.points.iter().enumerate() {
            if !p[0].is_finite() || !p[1].is_finite() {
                return Err(PdfError::InvalidInput(format!(
                    "stroke {i} point {j} has non-finite coordinates"
                )));
            }
            bbox[0] = bbox[0].min(p[0]);
            bbox[1] = bbox[1].min(p[1]);
            bbox[2] = bbox[2].max(p[0]);
            bbox[3] = bbox[3].max(p[1]);
        }
    }
    Ok(bbox)
}

/// `/InkList` value: an array of strokes, each an array of `[x y]`
/// point arrays (see module docs for the encoding decision).
fn ink_list_object(strokes: &[Stroke]) -> Object {
    let strokes_obj: Vec<Object> = strokes
        .iter()
        .map(|stroke| {
            Object::Array(
                stroke
                    .points
                    .iter()
                    .map(|p| {
                        Object::Array(vec![Object::Real(p[0] as f32), Object::Real(p[1] as f32)])
                    })
                    .collect(),
            )
        })
        .collect();
    Object::Array(strokes_obj)
}

/// `/Rect` covering `bbox` padded by `pad` points on every side.
fn padded_rect(bbox: [f64; 4], pad: f64) -> [f64; 4] {
    [bbox[0] - pad, bbox[1] - pad, bbox[2] + pad, bbox[3] + pad]
}

/// `/C` array: 8-bit RGB channels scaled to 0.0–1.0 floats.
fn color_object(color: [u8; 3]) -> Object {
    Object::Array(
        color
            .iter()
            .map(|c| Object::Real(f32::from(*c) / 255.0))
            .collect(),
    )
}

/// Count the stroked paths in an `/InkList` object, tolerating a
/// direct array or a single indirection.
fn ink_list_len(doc: &Document, annot: &lopdf::Dictionary) -> usize {
    let Ok(obj) = annot.get(b"InkList") else {
        return 0;
    };
    let resolved;
    let array_obj = match obj {
        Object::Reference(id) => match doc.get_object(*id) {
            Ok(o) => {
                resolved = o.clone();
                &resolved
            }
            Err(_) => return 0,
        },
        other => other,
    };
    array_obj.as_array().map(|a| a.len()).unwrap_or(0)
}

// ---------------------------------------------------------------------------
// Operations
// ---------------------------------------------------------------------------

/// Append `/Subtype /Ink` freehand annotations — one per selected
/// page, each containing all of `spec.strokes` — to `input` and return
/// the rewritten PDF.
///
/// See module docs for the `/InkList` point-array encoding and the
/// annotation entries written.
pub fn add_ink(input: &[u8], spec: &InkSpec, limits: &EngineLimits) -> PdfResult<Vec<u8>> {
    if !spec.width.is_finite() || spec.width <= 0.0 {
        return Err(PdfError::InvalidInput(
            "ink width must be a positive finite number".to_string(),
        ));
    }
    let bbox = validate_strokes(&spec.strokes)?;

    let mut doc = load_pdf(input, limits)?;
    reject_encrypted(&doc)?;

    let pad = f64::from(spec.width) / 2.0 + INK_RECT_PADDING;
    let rect = padded_rect(bbox, pad);
    let ink_list = ink_list_object(&spec.strokes);
    let color = color_object(spec.color);

    for (_page_no, page_id) in selected_page_ids(&doc, spec.pages.as_deref())? {
        // `media_box` is consulted only to keep parity with sibling ops;
        // ink coordinates are absolute user space and need no page
        // geometry — the binding is intentionally unused.
        let _ = media_box(&doc, page_id);
        let dict = dictionary! {
            "Type" => "Annot",
            "Subtype" => Object::Name(b"Ink".to_vec()),
            "Rect" => Object::Array(
                rect.iter().map(|v| Object::Real(*v as f32)).collect::<Vec<_>>()
            ),
            "InkList" => ink_list.clone(),
            "BS" => dictionary! {
                "W" => Object::Real(spec.width),
                "S" => Object::Name(b"S".to_vec()),
            },
            "C" => color.clone(),
            "F" => Object::Integer(4),
        };
        let annot_id = doc
            .try_add_object(Object::Dictionary(dict))
            .map_err(lopdf_err)?;
        append_annot(&mut doc, page_id, Object::Reference(annot_id))?;
    }

    save_pdf(&mut doc)
}

/// Walk every page's `/Annots` and return one [`InkInfo`] per
/// `/Subtype /Ink` annotation found, in page order then array order.
///
/// Read-only: encrypted documents still list fine.
pub fn list_ink(input: &[u8], limits: &EngineLimits) -> PdfResult<Vec<InkInfo>> {
    let doc = load_pdf(input, limits)?;

    let pages_map = doc.get_pages();
    let mut out = Vec::new();
    for (page_no, page_id) in &pages_map {
        for entry in annots_entries(&doc, *page_id) {
            let Some(annot) = resolve_dict(&doc, &entry) else {
                continue;
            };
            let is_ink = annot
                .get(b"Subtype")
                .ok()
                .and_then(|o| o.as_name().ok())
                .is_some_and(|n| n.eq_ignore_ascii_case(b"ink"));
            if !is_ink {
                continue;
            }
            let rect = annot.get(b"Rect").ok().and_then(rect_from_object);
            let stroke_count = ink_list_len(&doc, annot);
            let color = annot
                .get(b"C")
                .ok()
                .and_then(|o| o.as_array().ok())
                .and_then(|a| {
                    if a.len() != 3 {
                        return None;
                    }
                    let mut c = [0.0f64; 3];
                    for (i, item) in a.iter().enumerate() {
                        c[i] = item.as_float().ok()? as f64;
                    }
                    Some(c)
                });
            out.push(InkInfo {
                page: *page_no,
                rect,
                stroke_count,
                color,
            });
        }
    }
    Ok(out)
}

/// Remove `/Subtype /Ink` annotations from `input` and return the
/// rewritten PDF.
///
/// `filter.pages` selects the pages (`None` = all); `filter.all` and
/// `filter.subtypes` are accepted for interface parity but ignored —
/// `remove_ink` always removes *only* ink annotations. Removal is
/// delegated to [`super::annotations::remove_annotations`] with the
/// subtype pinned to `Ink`.
pub fn remove_ink(input: &[u8], filter: &AnnotFilter, limits: &EngineLimits) -> PdfResult<Vec<u8>> {
    let ink_only = AnnotFilter {
        pages: filter.pages.clone(),
        subtypes: Some(vec!["Ink".to_string()]),
        all: false,
    };
    super::annotations::remove_annotations(input, &ink_only, limits)
}

#[cfg(test)]
mod tests {
    use lopdf::Document;

    use super::*;
    use crate::crates::pdf_engine::ops::test_utils::pdf_with_pages;

    fn limits() -> EngineLimits {
        EngineLimits::default()
    }

    fn spec() -> InkSpec {
        InkSpec {
            strokes: vec![
                Stroke {
                    points: vec![[50.0, 50.0], [100.0, 120.0], [200.0, 80.0]],
                },
                Stroke {
                    points: vec![[10.0, 700.0], [590.0, 700.0]],
                },
            ],
            pages: None,
            color: [255, 0, 0],
            width: 2.5,
        }
    }

    fn ink_dicts(doc: &Document, page: u32) -> Vec<lopdf::Dictionary> {
        let page_id = doc.get_pages()[&page];
        let page_dict = doc.get_dictionary(page_id).unwrap();
        let Ok(annots) = page_dict.get(b"Annots") else {
            return Vec::new();
        };
        let items = match annots {
            Object::Array(items) => items.clone(),
            Object::Reference(id) => doc.get_object(*id).unwrap().as_array().unwrap().clone(),
            _ => panic!("Annots is not an array"),
        };
        items
            .iter()
            .filter_map(|entry| match entry {
                Object::Reference(id) => doc.get_dictionary(*id).ok().cloned(),
                Object::Dictionary(d) => Some(d.clone()),
                _ => None,
            })
            .filter(|d| {
                d.get(b"Subtype")
                    .ok()
                    .and_then(|o| o.as_name().ok())
                    .is_some_and(|n| n == b"Ink")
            })
            .collect()
    }

    #[test]
    fn add_ink_writes_ink_annot_per_page() {
        let input = pdf_with_pages(&["one", "two", "three"]);
        let mut s = spec();
        s.pages = Some("1-2".to_string());
        let out = add_ink(&input, &s, &limits()).unwrap();
        let doc = Document::load_mem(&out).unwrap();

        for page in [1u32, 2] {
            let annots = ink_dicts(&doc, page);
            assert_eq!(annots.len(), 1, "page {page}");
            let a = &annots[0];
            // InkList: 2 strokes; first has 3 point arrays of 2 numbers.
            let ink_list = a.get(b"InkList").unwrap().as_array().unwrap();
            assert_eq!(ink_list.len(), 2);
            let stroke0 = ink_list[0].as_array().unwrap();
            assert_eq!(stroke0.len(), 3);
            let p0 = stroke0[0].as_array().unwrap();
            assert_eq!(p0.len(), 2);
            assert_eq!(p0[0].as_float().unwrap(), 50.0);
            // Border style, color, print flag.
            let bs = a.get(b"BS").unwrap();
            let bs = doc
                .get_object(bs.as_reference().unwrap_or(ObjectId::from((0, 0))))
                .ok();
            let _ = bs; // BS may be direct or indirect; check via dictionary! form below.
            let bs_dict = match a.get(b"BS").unwrap() {
                Object::Dictionary(d) => d.clone(),
                Object::Reference(id) => doc.get_dictionary(*id).unwrap().clone(),
                _ => panic!("BS is not a dict"),
            };
            assert_eq!(bs_dict.get(b"W").unwrap().as_float().unwrap(), 2.5);
            assert_eq!(bs_dict.get(b"S").unwrap().as_name().unwrap(), b"S");
            let c = a.get(b"C").unwrap().as_array().unwrap();
            assert_eq!(c.len(), 3);
            assert!((c[0].as_float().unwrap() - 1.0).abs() < 1e-6);
            assert_eq!(a.get(b"F").unwrap().as_i64().unwrap(), 4);
            // Rect covers all points padded by width/2 + 2.
            let rect = a.get(b"Rect").unwrap().as_array().unwrap();
            assert_eq!(rect.len(), 4);
            assert!(rect[0].as_float().unwrap() < 10.0);
            assert!(rect[2].as_float().unwrap() > 590.0);
        }
        // Page 3 untouched.
        assert!(ink_dicts(&doc, 3).is_empty());
    }

    #[test]
    fn list_and_remove_ink() {
        let input = pdf_with_pages(&["a", "b"]);
        let mut pdf = add_ink(&input, &spec(), &limits()).unwrap();
        // Add a non-ink annotation too; remove_ink must leave it alone.
        let comment = crate::crates::pdf_engine::ops::annotations::CommentSpec {
            text: "keep me".to_string(),
            pages: None,
            rect: None,
            author: None,
            icon: None,
        };
        pdf = crate::crates::pdf_engine::ops::annotations::add_comment(&pdf, &comment, &limits())
            .unwrap();

        let infos = list_ink(&pdf, &limits()).unwrap();
        assert_eq!(infos.len(), 2);
        assert_eq!(infos[0].page, 1);
        assert_eq!(infos[0].stroke_count, 2);
        assert_eq!(infos[0].rect.is_some(), true);

        // Remove ink only on page 1.
        let filter = AnnotFilter {
            pages: Some("1".to_string()),
            subtypes: None,
            all: false,
        };
        let out = remove_ink(&pdf, &filter, &limits()).unwrap();
        let infos = list_ink(&out, &limits()).unwrap();
        assert_eq!(infos.len(), 1);
        assert_eq!(infos[0].page, 2);

        // list_annotations reports every annotation: the two Text
        // comments survive on both pages, and the page-2 ink (outside
        // the page-1 removal scope) is still present.
        let annots =
            crate::crates::pdf_engine::ops::annotations::list_annotations(&out, &limits()).unwrap();
        assert_eq!(annots.iter().filter(|a| a.subtype == "Text").count(), 2);
        assert_eq!(annots.iter().filter(|a| a.subtype == "Ink").count(), 1);

        // all=true is ignored by remove_ink: still only ink removed,
        // and there is none left — comments remain.
        let filter = AnnotFilter {
            pages: None,
            subtypes: None,
            all: true,
        };
        let out = remove_ink(&out, &filter, &limits()).unwrap();
        assert!(list_ink(&out, &limits()).unwrap().is_empty());
        assert_eq!(
            crate::crates::pdf_engine::ops::annotations::list_annotations(&out, &limits())
                .unwrap()
                .len(),
            2
        );
    }

    #[test]
    fn add_ink_validates_inputs() {
        let input = pdf_with_pages(&["a"]);

        // Empty strokes.
        let mut s = spec();
        s.strokes = vec![];
        assert!(matches!(
            add_ink(&input, &s, &limits()),
            Err(PdfError::InvalidInput(_))
        ));

        // Single-point stroke.
        let mut s = spec();
        s.strokes = vec![Stroke {
            points: vec![[1.0, 2.0]],
        }];
        assert!(matches!(
            add_ink(&input, &s, &limits()),
            Err(PdfError::InvalidInput(_))
        ));

        // Non-finite coordinate.
        let mut s = spec();
        s.strokes = vec![Stroke {
            points: vec![[f64::NAN, 0.0], [1.0, 1.0]],
        }];
        assert!(matches!(
            add_ink(&input, &s, &limits()),
            Err(PdfError::InvalidInput(_))
        ));

        // Bad width.
        for w in [0.0, -1.0, f32::NAN, f32::INFINITY] {
            let mut s = spec();
            s.width = w;
            assert!(matches!(
                add_ink(&input, &s, &limits()),
                Err(PdfError::InvalidInput(_))
            ));
        }

        // Stroke cap.
        let mut s = spec();
        s.strokes = (0..=MAX_INK_STROKES)
            .map(|_| Stroke {
                points: vec![[0.0, 0.0], [1.0, 1.0]],
            })
            .collect();
        assert!(matches!(
            add_ink(&input, &s, &limits()),
            Err(PdfError::LimitExceeded(_))
        ));

        // Bad page range.
        let mut s = spec();
        s.pages = Some("9".to_string());
        assert!(matches!(
            add_ink(&input, &s, &limits()),
            Err(PdfError::InvalidPageRange(_))
        ));

        // Garbage input.
        assert!(matches!(
            add_ink(b"not a pdf", &spec(), &limits()),
            Err(PdfError::InvalidPdf(_))
        ));
    }
}
