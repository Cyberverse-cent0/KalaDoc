//! Replace the pixel data of a named image XObject on a single page.
//!
//! The named `/XObject` on the page's effective `/Resources` must
//! reference a stream with `/Subtype /Image`; its stream bytes and
//! dictionary are rewritten in place with the decoded replacement
//! image. Because the resource entry is replaced — not the resource
//! name — every `Do` invocation of that name (on this page *and* on any
//! other page sharing the same indirect image object) now paints the
//! new image.
//!
//! # Input format and pixel pipeline
//!
//! `new_image` must be **PNG or JPEG** — anything else is rejected with
//! [`PdfError::InvalidInput`] (the `image` crate's other enabled
//! decoders, e.g. GIF/WebP/BMP, are deliberately not accepted). Pass
//! [`ImageInputFormat::Auto`] to sniff, or `Png`/`Jpeg` to force a
//! decoder.
//!
//! The decoded image is **resampled to exactly the original XObject's
//! `/Width` × `/Height`** (`resize_exact`, triangle filter) — aspect
//! ratio is *not* preserved; a differently-shaped replacement is
//! distorted to fit, matching the existing display rectangle. Alpha is
//! composited onto white (same flattening as `stamp`), then stored as
//! 8-bpc `/DeviceRGB` samples.
//!
//! # Dictionary rewrite
//!
//! The replacement stream dictionary is rebuilt as
//! `Type=XObject, Subtype=Image, Width, Height, ColorSpace=DeviceRGB,
//! BitsPerComponent=8, Interpolate=true`. Consequences, documented
//! loudly:
//!
//! - **`/SMask` and `/Mask` are dropped** — transparency in the
//!   replacement is flattened onto white rather than preserved as a
//!   soft mask.
//! - The original `/ColorSpace` (e.g. `DeviceGray`, `ICCBased`,
//!   `Indexed`) is replaced by `DeviceRGB`.
//! - `/Decode`, `/DecodeParms`, `/Intent`, `/Alternates` and any other
//!   image-specific entries are dropped.
//! - The new stream is FlateDecode-compressed **only when that actually
//!   shrinks it** (`lopdf`'s `Stream::compress` keeps the raw bytes for
//!   tiny images); either way the stream is a valid unfiltered-or-
//!   flate RGB sample stream.
//!
//! # Sharing caveat
//!
//! When the XObject is an indirect object referenced by several pages,
//! replacing it changes the image **everywhere it is used** — there is
//! no per-page copy. Direct (non-indirect) image streams are replaced
//! only for the target page, via a fresh page-private `/Resources`
//! dictionary (shared resource dictionaries are never mutated).
//!
//! Encrypted documents are rejected.

use image::{DynamicImage, ImageFormat, imageops::FilterType};
use lopdf::{Document, Object, Stream, dictionary};

use crate::crates::core::engine::EngineLimits;

use crate::crates::pdf_engine::error::{PdfError, PdfResult};

use super::load::{load_pdf, lopdf_err, reject_encrypted, save_pdf};
use super::watermark::effective_resources_dict;

/// Declared format of the replacement image bytes.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ImageInputFormat {
    /// Sniff the format from the bytes (PNG/JPEG magic); other formats
    /// are rejected.
    Auto,
    /// Force PNG decoding.
    Png,
    /// Force JPEG decoding.
    Jpeg,
}

/// Resolve the declared [`ImageInputFormat`] to an [`ImageFormat`],
/// rejecting anything that is not PNG or JPEG.
fn resolve_format(bytes: &[u8], format: ImageInputFormat) -> PdfResult<ImageFormat> {
    let fmt = match format {
        ImageInputFormat::Png => ImageFormat::Png,
        ImageInputFormat::Jpeg => ImageFormat::Jpeg,
        ImageInputFormat::Auto => image::guess_format(bytes).map_err(|err| {
            PdfError::InvalidInput(format!("replacement image format not recognized: {err}"))
        })?,
    };
    match fmt {
        ImageFormat::Png | ImageFormat::Jpeg => Ok(fmt),
        other => Err(PdfError::InvalidInput(format!(
            "replacement image must be PNG or JPEG, got {other:?}"
        ))),
    }
}

/// Decode `new_image` and resample it to `(width, height)`, returning
/// flattened RGB bytes (alpha composited onto white).
fn decode_and_resample(
    new_image: &[u8],
    format: ImageInputFormat,
    width: u32,
    height: u32,
    limits: &EngineLimits,
) -> PdfResult<Vec<u8>> {
    let fmt = resolve_format(new_image, format)?;
    let image = image::load_from_memory_with_format(new_image, fmt).map_err(|err| {
        PdfError::InvalidInput(format!("replacement image could not be decoded: {err}"))
    })?;

    let (w, h) = (image.width(), image.height());
    if let Some(max_pixels) = limits.max_pixels {
        let pixels = u64::from(w) * u64::from(h);
        if pixels > max_pixels {
            return Err(PdfError::LimitExceeded(format!(
                "replacement image dimensions {w}x{h} ({pixels} pixels) \
                 exceed max_pixels {max_pixels}"
            )));
        }
    }
    if w == 0 || h == 0 {
        return Err(PdfError::InvalidInput(
            "replacement image decoded to a zero-sized image".to_string(),
        ));
    }
    if width == 0 || height == 0 {
        return Err(PdfError::InvalidInput(
            "target image XObject has zero width or height".to_string(),
        ));
    }

    // Resample to the original pixel dimensions exactly (aspect ratio is
    // not preserved — see module docs), then composite alpha onto
    // white and emit RGB triplets.
    let resized: DynamicImage = image.resize_exact(width, height, FilterType::Triangle);
    let rgba = resized.to_rgba8();
    let mut rgb = vec![0u8; rgba.len() / 4 * 3];
    for (src, dst) in rgba.chunks_exact(4).zip(rgb.chunks_exact_mut(3)) {
        let a = u32::from(src[3]);
        let inv = 255 - a;
        for c in 0..3 {
            dst[c] = ((u32::from(src[c]) * a + 255 * inv + 127) / 255) as u8;
        }
    }
    Ok(rgb)
}

/// Whether a name is usable as an XObject resource name for lookup.
/// Resource names cannot contain whitespace or PDF delimiter bytes.
fn valid_xobject_name(name: &str) -> bool {
    !name.is_empty()
        && name
            .bytes()
            .all(|b| !b.is_ascii_whitespace() && !b"()<>[]{}/%".contains(&b))
}

/// Integer-valued dictionary entry as `u32`, tolerating references and
/// real values. Missing/malformed yields 0.
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

/// Whether a stream dictionary declares `/Subtype /Image`.
fn is_image_stream(doc: &Document, dict: &lopdf::Dictionary) -> bool {
    dict.get(b"Subtype")
        .ok()
        .and_then(|o| doc.dereference(o).ok())
        .and_then(|(_, o)| o.as_name().ok())
        .is_some_and(|name| name == b"Image")
}

/// Replace the image XObject `xobject_name` on 1-based `page` of
/// `input` with the decoded `new_image`, returning the new PDF bytes.
///
/// Errors:
/// - [`PdfError::InvalidPageRange`] — `page` is outside `1..=page_count`.
/// - [`PdfError::InvalidInput`] — bad name, undecodable/non-PNG-JPEG
///   image, missing XObject name on the page, or the named XObject is
///   not an image stream.
/// - [`PdfError::LimitExceeded`] — `max_pixels` exceeded by the
///   replacement image.
pub fn replace_image(
    input: &[u8],
    page: u32,
    xobject_name: &str,
    new_image: &[u8],
    format: ImageInputFormat,
    limits: &EngineLimits,
) -> PdfResult<Vec<u8>> {
    if !valid_xobject_name(xobject_name) {
        return Err(PdfError::InvalidInput(format!(
            "invalid XObject resource name {xobject_name:?}"
        )));
    }

    let mut doc = load_pdf(input, limits)?;
    reject_encrypted(&doc)?;

    let page_count = doc.get_pages().len() as u32;
    if page == 0 || page > page_count {
        return Err(PdfError::InvalidPageRange(format!(
            "page {page} is out of range; document has {page_count} page(s)"
        )));
    }
    let pages_map = doc.get_pages();
    let page_id = pages_map
        .get(&page)
        .copied()
        .ok_or_else(|| PdfError::InvalidPageRange(format!("page {page} not found")))?;

    // Resolve the page's effective /Resources → /XObject entry.
    let resources = effective_resources_dict(&doc, page_id).ok_or_else(|| {
        PdfError::InvalidInput(format!(
            "page {page} has no /Resources dictionary"
        ))
    })?;
    let xobjects_obj = resources.get(b"XObject").map_err(|_| {
        PdfError::InvalidInput(format!(
            "page {page} has no /XObject resources; \
             cannot find image {xobject_name:?}"
        ))
    })?;
    let (_, xobjects_resolved) = doc
        .dereference(xobjects_obj)
        .map_err(lopdf_err)?;
    let xobjects = xobjects_resolved.as_dict().map_err(|_| {
        PdfError::InvalidInput("/XObject resource entry is not a dictionary".to_string())
    })?;

    let name_bytes = xobject_name.as_bytes();
    let value = xobjects.get(name_bytes).map_err(|_| {
        PdfError::InvalidInput(format!(
            "page {page} has no XObject named {xobject_name:?}"
        ))
    })?;
    let (target_id, target) = doc
        .dereference(value)
        .map_err(lopdf_err)?;
    let original_stream = target.as_stream().map_err(|_| {
        PdfError::InvalidInput(format!(
            "XObject {xobject_name:?} on page {page} is not a stream"
        ))
    })?;
    if !is_image_stream(&doc, &original_stream.dict) {
        return Err(PdfError::InvalidInput(format!(
            "XObject {xobject_name:?} on page {page} is not an image \
             (/Subtype is not /Image)"
        )));
    }

    let width = dict_u32(&doc, &original_stream.dict, b"Width");
    let height = dict_u32(&doc, &original_stream.dict, b"Height");
    if width == 0 || height == 0 {
        return Err(PdfError::InvalidInput(format!(
            "image XObject {xobject_name:?} has unusable dimensions {width}x{height}"
        )));
    }

    // Decode + resample before mutating the document.
    let rgb = decode_and_resample(new_image, format, width, height, limits)?;

    let mut stream = Stream::new(
        dictionary! {
            "Type" => "XObject",
            "Subtype" => "Image",
            "Width" => width as i64,
            "Height" => height as i64,
            "ColorSpace" => "DeviceRGB",
            "BitsPerComponent" => 8,
            "Interpolate" => true,
        },
        rgb,
    );
    // Best-effort FlateDecode; lopdf keeps raw bytes when compression
    // would not shrink the stream (tiny images).
    let _ = stream.compress();

    match target_id {
        // Indirect image object: replace it in place. Every page
        // referencing this object sees the new image (see module docs).
        Some(id) => {
            doc.objects.insert(id, Object::Stream(stream));
        }
        // Direct stream embedded in the resource dictionary: rebuild a
        // page-private /Resources with the new stream inline.
        None => {
            let mut new_xobjects = xobjects.clone();
            new_xobjects.set(name_bytes, Object::Stream(stream));
            let mut new_resources = resources.clone();
            new_resources.set(b"XObject", Object::Dictionary(new_xobjects));
            let resources_id = doc
                .try_add_object(Object::Dictionary(new_resources))
                .map_err(lopdf_err)?;
            doc.get_dictionary_mut(page_id)
                .map_err(lopdf_err)?
                .set("Resources", Object::Reference(resources_id));
        }
    }

    save_pdf(&mut doc)
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;
    use crate::crates::pdf_engine::ops::extract_images;
    use crate::crates::pdf_engine::ops::test_utils::pdf_with_pages;
    use lopdf::content::Operation;
    use lopdf::{Document, Stream as LopStream};
    use std::io::Cursor;

    fn limits() -> EngineLimits {
        EngineLimits::default()
    }

    /// A 4x4 opaque red PNG.
    fn red_png() -> Vec<u8> {
        let img = image::RgbaImage::from_pixel(4, 4, image::Rgba([255, 0, 0, 255]));
        let mut buf = Cursor::new(Vec::new());
        DynamicImage::ImageRgba8(img)
            .write_to(&mut buf, ImageFormat::Png)
            .unwrap();
        buf.into_inner()
    }

    /// A 6x2 blue JPEG.
    fn blue_jpeg() -> Vec<u8> {
        let img = image::RgbImage::from_pixel(6, 2, image::Rgb([0, 0, 255]));
        let mut buf = Cursor::new(Vec::new());
        DynamicImage::ImageRgb8(img)
            .write_to(&mut buf, ImageFormat::Jpeg)
            .unwrap();
        buf.into_inner()
    }

    /// Build a one-page PDF whose /Resources carries a 2x2 DeviceRGB
    /// image XObject `/Im1` invoked via `Do`.
    fn pdf_with_rgb_image() -> Vec<u8> {
        let mut doc = Document::with_version("1.5");
        let pages_id = doc.new_object_id();
        let font_id = doc.add_object(dictionary! {
            "Type" => "Font", "Subtype" => "Type1", "BaseFont" => "Courier",
        });
        // 2x2 RGB: red, green, blue, white.
        let pixels = vec![
            255, 0, 0, 0, 255, 0, 0, 0, 255, 255, 255, 255,
        ];
        let image_id = doc.add_object(LopStream::new(
            dictionary! {
                "Type" => "XObject",
                "Subtype" => "Image",
                "Width" => 2i64,
                "Height" => 2i64,
                "BitsPerComponent" => 8i64,
                "ColorSpace" => "DeviceRGB",
            },
            pixels,
        ));
        let resources_id = doc.add_object(dictionary! {
            "Font" => dictionary! { "F1" => font_id },
            "XObject" => dictionary! { "Im1" => image_id },
        });
        let content = lopdf::content::Content {
            operations: vec![
                Operation::new("q", vec![]),
                Operation::new("Do", vec![Object::Name(b"Im1".to_vec())]),
                Operation::new("Q", vec![]),
            ],
        };
        let content_id = doc.add_object(LopStream::new(dictionary! {}, content.encode().unwrap()));
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

    #[test]
    fn replaces_image_with_png_resampled_to_original_dims() {
        let pdf = pdf_with_rgb_image();
        // Original is 2x2; the 4x4 PNG must be resampled down to 2x2.
        let out = replace_image(
            &pdf,
            1,
            "Im1",
            &red_png(),
            ImageInputFormat::Auto,
            &limits(),
        )
        .unwrap();

        let images = extract_images(&out, None, &limits()).unwrap();
        assert_eq!(images.len(), 1);
        let img = &images[0];
        assert_eq!(img.name, "Im1");
        assert_eq!(img.width, 2);
        assert_eq!(img.height, 2);
        assert_eq!(img.bits_per_component, 8);
        assert_eq!(img.color_space.as_deref(), Some("DeviceRGB"));
        // 2x2 RGB = 12 bytes raw; tiny streams stay unfiltered, larger
        // ones become FlateDecode — either is acceptable.
        if img.filter.is_none() {
            assert_eq!(img.data.len(), 12);
        }

        // The Do operator is untouched — same resource name still paints.
        let doc = Document::load_mem(&out).unwrap();
        let pages = doc.get_pages();
        let page = pages.get(&1).copied().unwrap();
        let stream_id = doc.get_page_contents(page)[0];
        let obj = doc.get_object(stream_id).unwrap();
        let stream = obj.as_stream().unwrap();
        let raw = stream.decompressed_content().unwrap_or_else(|_| stream.content.clone());
        let content = lopdf::content::Content::decode(&raw).unwrap();
        assert!(content.operations.iter().any(|op| {
            op.operator == "Do"
                && matches!(op.operands.first(), Some(Object::Name(n)) if n == b"Im1")
        }));
    }

    #[test]
    fn replaces_image_with_jpeg() {
        let pdf = pdf_with_rgb_image();
        let out = replace_image(
            &pdf,
            1,
            "Im1",
            &blue_jpeg(),
            ImageInputFormat::Jpeg,
            &limits(),
        )
        .unwrap();
        let images = extract_images(&out, None, &limits()).unwrap();
        assert_eq!(images.len(), 1);
        assert_eq!(images[0].width, 2);
        assert_eq!(images[0].height, 2);
        assert_eq!(images[0].color_space.as_deref(), Some("DeviceRGB"));
    }

    #[test]
    fn rejects_missing_name_and_non_image() {
        let pdf = pdf_with_rgb_image();
        // Name not present on the page.
        assert!(matches!(
            replace_image(&pdf, 1, "Nope", &red_png(), ImageInputFormat::Auto, &limits()),
            Err(PdfError::InvalidInput(_))
        ));
        // A font resource name is not an XObject at all — still a clean
        // "not found" because only /XObject entries are consulted.
        assert!(matches!(
            replace_image(&pdf, 1, "F1", &red_png(), ImageInputFormat::Auto, &limits()),
            Err(PdfError::InvalidInput(_))
        ));
        // Page out of range.
        assert!(matches!(
            replace_image(&pdf, 7, "Im1", &red_png(), ImageInputFormat::Auto, &limits()),
            Err(PdfError::InvalidPageRange(_))
        ));
    }

    #[test]
    fn rejects_non_image_xobject_and_bad_image() {
        // Build a page whose XObject is a Form XObject, not an image.
        let mut doc = Document::with_version("1.5");
        let pages_id = doc.new_object_id();
        let form_id = doc.add_object(LopStream::new(
            dictionary! {
                "Type" => "XObject",
                "Subtype" => "Form",
                "BBox" => vec![0.into(), 0.into(), 10.into(), 10.into()],
            },
            b"q Q".to_vec(),
        ));
        let resources_id = doc.add_object(dictionary! {
            "XObject" => dictionary! { "Fm1" => form_id },
        });
        let page_id = doc.add_object(dictionary! {
            "Type" => "Page", "Parent" => pages_id,
            "Resources" => resources_id,
            "MediaBox" => vec![0.into(), 0.into(), 595.into(), 842.into()],
        });
        doc.objects.insert(
            pages_id,
            Object::Dictionary(dictionary! {
                "Type" => "Pages",
                "Kids" => vec![Object::Reference(page_id)],
                "Count" => 1i64,
            }),
        );
        let catalog_id = doc.add_object(dictionary! {
            "Type" => "Catalog", "Pages" => pages_id,
        });
        doc.trailer.set("Root", catalog_id);
        let mut buf = Vec::new();
        doc.save_to(&mut buf).unwrap();

        assert!(matches!(
            replace_image(&buf, 1, "Fm1", &red_png(), ImageInputFormat::Auto, &limits()),
            Err(PdfError::InvalidInput(_))
        ));

        // Undecodable replacement image on the image fixture.
        let pdf = pdf_with_rgb_image();
        assert!(matches!(
            replace_image(&pdf, 1, "Im1", b"not an image", ImageInputFormat::Auto, &limits()),
            Err(PdfError::InvalidInput(_))
        ));
        // A GIF would decode via the image crate but is not accepted.
        let gif = {
            let img = image::RgbaImage::from_pixel(2, 2, image::Rgba([1, 2, 3, 255]));
            let mut b = Cursor::new(Vec::new());
            DynamicImage::ImageRgba8(img)
                .write_to(&mut b, ImageFormat::Gif)
                .unwrap();
            b.into_inner()
        };
        assert!(matches!(
            replace_image(&pdf, 1, "Im1", &gif, ImageInputFormat::Auto, &limits()),
            Err(PdfError::InvalidInput(_))
        ));
    }

    #[test]
    fn rejects_bad_names_and_garbage() {
        let pdf = pdf_with_rgb_image();
        for bad in ["", "has space", "a/b", "x(y"] {
            assert!(matches!(
                replace_image(&pdf, 1, bad, &red_png(), ImageInputFormat::Auto, &limits()),
                Err(PdfError::InvalidInput(_))
            ));
        }
        assert!(matches!(
            replace_image(b"junk", 1, "Im1", &red_png(), ImageInputFormat::Auto, &limits()),
            Err(_)
        ));
        // A text-only document has no XObject resources at all.
        let text_pdf = pdf_with_pages(&["hello"]);
        assert!(matches!(
            replace_image(&text_pdf, 1, "Im1", &red_png(), ImageInputFormat::Auto, &limits()),
            Err(PdfError::InvalidInput(_))
        ));
    }

    #[test]
    fn max_pixels_enforced_on_replacement() {
        let pdf = pdf_with_rgb_image();
        let tight = EngineLimits {
            max_pixels: Some(4),
            ..EngineLimits::default()
        };
        // red_png is 4x4 = 16 pixels > 4.
        assert!(matches!(
            replace_image(&pdf, 1, "Im1", &red_png(), ImageInputFormat::Auto, &tight),
            Err(PdfError::LimitExceeded(_))
        ));
    }
}
