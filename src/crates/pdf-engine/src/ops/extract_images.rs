//! Extract embedded images from a PDF's page resources.
//!
//! Each page's `/Resources` → `/XObject` dictionary is scanned for
//! entries whose dereferenced object is a stream with
//! `/Subtype /Image`. `/Resources` is inherited from ancestor `/Pages`
//! nodes when a page does not define its own.
//!
//! The image's stream bytes are returned **still filtered** — i.e.
//! exactly as stored in the file (`/DCTDecode`, `/FlateDecode`,
//! `/JPXDecode`, ... are *not* decoded; lopdf does not decompress
//! streams at load time). Callers that need pixels must decode the
//! filter themselves; the dictionary parameters needed for that
//! (width, height, color space, bits per component, filter chain) are
//! all reported.
//!
//! Scope and honesty notes:
//!
//! - **Inline images** (`BI`/`ID`/`EI` inside content streams) are not
//!   extracted — they have no object identity and would need content
//!   stream parsing.
//! - Images referenced only inside **Form XObjects** (nested resource
//!   dictionaries) are not found; only page-level `/XObject` entries
//!   are walked.
//! - XObjects shared between pages are reported **once**, on the first
//!   page that references them (deduplicated by resolved object id).
//! - Encrypted documents are rejected — their streams cannot be read
//!   without decrypting, and this engine never decrypts.

use std::collections::HashSet;

use lopdf::{Document, Object, ObjectId};
use serde::{Deserialize, Serialize};

use crate::crates::core::engine::EngineLimits;

use crate::crates::pdf_engine::error::{PdfError, PdfResult};

use super::load::{load_pdf, reject_encrypted};
use super::pages::parse_page_range;

/// Safety cap on the number of images reported per document.
const MAX_IMAGES: usize = 4096;
/// Cap on parent-chain walking when resolving inherited `/Resources`.
const MAX_INHERIT_DEPTH: usize = 64;

/// An image embedded in a PDF page's resources.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct EmbeddedImage {
    /// 1-based page number of the first page referencing this image.
    pub page: u32,
    /// Resource name, e.g. `"Im1"` or `"X4"`.
    pub name: String,
    /// `/Width` in pixels (0 when absent or malformed).
    pub width: u32,
    /// `/Height` in pixels (0 when absent or malformed).
    pub height: u32,
    /// `/ColorSpace` rendered as a short string: the color space name
    /// for name-valued spaces (`"DeviceRGB"`), or the first array
    /// element's name for array-valued ones (`"ICCBased"`,
    /// `"Indexed"`). `None` when missing or not name/array shaped.
    pub color_space: Option<String>,
    /// `/BitsPerComponent` (0 when absent or malformed).
    pub bits_per_component: u32,
    /// `/Filter` rendered as a comma-joined name list in decoding
    /// order (`"DCTDecode"`, `"FlateDecode,ASCII85Decode"`). `None`
    /// when the stream is stored unfiltered or the entry is
    /// unreadable. [`EmbeddedImage::data`] is still in this filtered
    /// form.
    pub filter: Option<String>,
    /// Raw stream bytes, **still filtered** — exactly as stored in
    /// the PDF. Not decoded pixels.
    pub data: Vec<u8>,
}

/// Extract embedded images from `input`.
///
/// `pages` is an optional 1-based range spec (`"1-3,8"`, `"all"`);
/// `None` scans every page. Encrypted documents are rejected.
///
/// Returns [`PdfError::LimitExceeded`] when the document contains more
/// than [`MAX_IMAGES`] distinct image XObjects, or when an image's
/// `width × height` exceeds `max_pixels`.
pub fn extract_images(
    input: &[u8],
    pages: Option<&str>,
    limits: &EngineLimits,
) -> PdfResult<Vec<EmbeddedImage>> {
    let doc = load_pdf(input, limits)?;
    reject_encrypted(&doc)?;

    let page_count = doc.get_pages().len() as u32;
    let pages_map = doc.get_pages();
    let selected: Vec<u32> = match pages {
        Some(spec) => parse_page_range(spec, page_count)?,
        None => (1..=page_count).collect(),
    };

    let mut out = Vec::new();
    let mut seen: HashSet<ObjectId> = HashSet::new();

    for page_number in selected {
        let Some(page_id) = pages_map.get(&page_number).copied() else {
            continue;
        };
        let Some(resources) = inherited_dict(&doc, page_id, b"Resources") else {
            continue;
        };
        let Ok(xobjects_obj) = resources.get_deref(b"XObject", &doc) else {
            continue;
        };
        let Ok(xobjects) = xobjects_obj.as_dict() else {
            continue;
        };
        for (name, value) in xobjects.iter() {
            if out.len() >= MAX_IMAGES {
                return Err(PdfError::LimitExceeded(format!(
                    "document contains more than {MAX_IMAGES} image XObjects"
                )));
            }
            // Resolve the XObject; only stream objects can be images.
            let Ok((object_id, object)) = doc.dereference(value) else {
                continue;
            };
            let Ok(stream) = object.as_stream() else {
                continue;
            };
            if !is_image_stream(&doc, &stream.dict) {
                continue;
            }
            // Dedupe shared XObjects by resolved object id. Direct
            // (non-reference) streams have no id and are always kept.
            if let Some(id) = object_id
                && !seen.insert(id)
            {
                continue;
            }

            let width = dict_u32(&doc, &stream.dict, b"Width");
            let height = dict_u32(&doc, &stream.dict, b"Height");
            if let Some(max_pixels) = limits.max_pixels {
                let pixels = u64::from(width) * u64::from(height);
                if pixels > max_pixels {
                    return Err(PdfError::LimitExceeded(format!(
                        "image \"{}\" is {width}x{height} ({pixels} pixels), \
                         exceeding max_pixels {max_pixels}",
                        String::from_utf8_lossy(name)
                    )));
                }
            }

            out.push(EmbeddedImage {
                page: page_number,
                name: String::from_utf8_lossy(name).into_owned(),
                width,
                height,
                color_space: color_space_string(&doc, &stream.dict),
                bits_per_component: dict_u32(&doc, &stream.dict, b"BitsPerComponent"),
                filter: filter_string(&doc, &stream.dict),
                // `stream.content` is the raw, still-filtered bytes —
                // lopdf does not decompress at load time.
                data: stream.content.clone(),
            });
        }
    }
    Ok(out)
}

/// Whether a stream dictionary declares `/Subtype /Image`.
fn is_image_stream(doc: &Document, dict: &lopdf::Dictionary) -> bool {
    dict.get(b"Subtype")
        .ok()
        .and_then(|o| doc.dereference(o).ok())
        .and_then(|(_, o)| o.as_name().ok())
        .is_some_and(|name| name == b"Image")
}

/// Resolve an inherited dictionary entry (`/Resources`, ...) starting
/// at `page_id` and climbing `/Parent` links. Returns the first
/// dictionary found, or `None` when no ancestor provides it.
fn inherited_dict<'a>(
    doc: &'a Document,
    page_id: ObjectId,
    key: &[u8],
) -> Option<&'a lopdf::Dictionary> {
    let mut current_id = Some(page_id);
    let mut direct: Option<&'a Object> = None;
    for _ in 0..MAX_INHERIT_DEPTH {
        let dict: &lopdf::Dictionary = match (current_id, direct) {
            (Some(id), _) => doc.get_dictionary(id).ok()?,
            (None, Some(obj)) => obj.as_dict().ok()?,
            (None, None) => return None,
        };
        if let Ok(obj) = dict.get_deref(key, doc)
            && let Ok(found) = obj.as_dict()
        {
            return Some(found);
        }
        let parent = dict.get(b"Parent").ok()?;
        let (parent_id, parent_obj) = doc.dereference(parent).ok()?;
        match parent_id {
            Some(id) => {
                current_id = Some(id);
                direct = None;
            }
            None => {
                current_id = None;
                direct = Some(parent_obj);
            }
        }
    }
    None
}

/// Read an integer-valued dictionary entry as `u32`, dereferencing
/// references and tolerating real-valued numbers. Missing or
/// malformed values yield 0.
fn dict_u32(doc: &Document, dict: &lopdf::Dictionary, key: &[u8]) -> u32 {
    let Ok(obj) = dict.get_deref(key, doc) else {
        return 0;
    };
    if let Ok(v) = obj.as_i64() {
        return v.max(0).min(u32::MAX as i64) as u32;
    }
    obj.as_f32()
        .ok()
        .map(|v| v.max(0.0).min(u32::MAX as f32) as u32)
        .unwrap_or(0)
}

/// Render `/ColorSpace` as a display string.
fn color_space_string(doc: &Document, dict: &lopdf::Dictionary) -> Option<String> {
    let obj = dict.get_deref(b"ColorSpace", doc).ok()?;
    name_or_first_name(doc, obj)
}

/// Render `/Filter` as a comma-joined display string in decoding
/// order — array filters joined with `","`, or the single filter
/// name.
fn filter_string(doc: &Document, dict: &lopdf::Dictionary) -> Option<String> {
    let obj = dict.get_deref(b"Filter", doc).ok()?;
    match obj {
        Object::Array(filters) => {
            let names: Vec<String> = filters
                .iter()
                .filter_map(|f| doc.dereference(f).ok())
                .filter_map(|(_, f)| f.as_name().ok())
                .map(|n| String::from_utf8_lossy(n).into_owned())
                .collect();
            (!names.is_empty()).then(|| names.join(","))
        }
        _ => name_or_first_name(doc, obj),
    }
}

/// Extract a name object as a string; for arrays, the first element's
/// name (covers `[ /ICCBased ref ]`, `[ /Indexed ... ]`, etc.).
fn name_or_first_name(doc: &Document, obj: &Object) -> Option<String> {
    let name = match obj {
        Object::Name(name) => name,
        Object::Array(items) => {
            let (_, first) = doc.dereference(items.first()?).ok()?;
            first.as_name().ok()?
        }
        _ => return None,
    };
    Some(String::from_utf8_lossy(name).into_owned())
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;
    use crate::crates::pdf_engine::ops::test_utils::pdf_with_pages;
    use lopdf::{Stream, dictionary};

    fn limits() -> EngineLimits {
        EngineLimits::default()
    }

    #[test]
    fn text_only_document_has_no_images() {
        let pdf = pdf_with_pages(&["hello", "world"]);
        let images = extract_images(&pdf, None, &limits()).unwrap();
        assert!(images.is_empty());
    }

    #[test]
    fn extracts_image_xobject() {
        let pdf = pdf_with_image();
        let images = extract_images(&pdf, None, &limits()).unwrap();
        assert_eq!(images.len(), 1);
        let img = &images[0];
        assert_eq!(img.page, 1);
        assert_eq!(img.name, "Im1");
        assert_eq!(img.width, 2);
        assert_eq!(img.height, 3);
        assert_eq!(img.bits_per_component, 8);
        assert_eq!(img.color_space.as_deref(), Some("DeviceGray"));
        assert_eq!(img.filter, None); // stored unfiltered
        assert_eq!(img.data, vec![0u8, 255, 128, 64, 32, 16]);
    }

    #[test]
    fn page_range_filters_results() {
        let pdf = pdf_with_pages(&["a", "b"]);
        // Valid range on an imageless doc still succeeds and is empty.
        let images = extract_images(&pdf, Some("2"), &limits()).unwrap();
        assert!(images.is_empty());
        assert!(extract_images(&pdf, Some("9"), &limits()).is_err());
    }

    #[test]
    fn max_pixels_enforced() {
        let pdf = pdf_with_image(); // 2x3 = 6 pixels
        let tight = EngineLimits {
            max_pixels: Some(5),
            ..EngineLimits::default()
        };
        assert!(matches!(
            extract_images(&pdf, None, &tight),
            Err(PdfError::LimitExceeded(_))
        ));
    }

    #[test]
    fn garbage_rejected() {
        assert!(extract_images(b"junk", None, &limits()).is_err());
    }

    /// Build a one-page PDF whose /Resources carries an /XObject image.
    fn pdf_with_image() -> Vec<u8> {
        let mut doc = Document::with_version("1.5");
        let pages_id = doc.new_object_id();
        let font_id = doc.add_object(dictionary! {
            "Type" => "Font", "Subtype" => "Type1", "BaseFont" => "Courier",
        });
        let image_id = doc.add_object(Stream::new(
            dictionary! {
                "Type" => "XObject",
                "Subtype" => "Image",
                "Width" => 2i64,
                "Height" => 3i64,
                "BitsPerComponent" => 8i64,
                "ColorSpace" => "DeviceGray",
            },
            vec![0u8, 255, 128, 64, 32, 16],
        ));
        let resources_id = doc.add_object(dictionary! {
            "Font" => dictionary! { "F1" => font_id },
            "XObject" => dictionary! { "Im1" => image_id },
        });
        let content = lopdf::content::Content {
            operations: vec![lopdf::content::Operation::new("q", vec![])],
        };
        let content_id = doc.add_object(Stream::new(dictionary! {}, content.encode().unwrap()));
        let page_id = doc.add_object(dictionary! {
            "Type" => "Page", "Parent" => pages_id,
            "Contents" => content_id, "Resources" => resources_id,
            "MediaBox" => vec![0.into(), 0.into(), 595.into(), 842.into()],
        });
        doc.objects.insert(
            pages_id,
            Object::Dictionary(dictionary! {
                "Type" => "Pages",
                "Kids" => vec![Object::Reference(page_id)],
                "Count" => 1i64,
                "Resources" => resources_id,
                "MediaBox" => vec![0.into(), 0.into(), 595.into(), 842.into()],
            }),
        );
        let catalog_id = doc.add_object(dictionary! {
            "Type" => "Catalog", "Pages" => pages_id,
        });
        doc.trailer.set("Root", catalog_id);
        let mut buf = Vec::new();
        doc.save_to(&mut buf).unwrap();
        buf
    }
}
