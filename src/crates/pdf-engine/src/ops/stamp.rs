//! Stamp a mark — text or a PNG image — on top of selected pages.
//!
//! Unlike [`super::watermark`], stamps are appended **after** the page's
//! existing content so they are never hidden underneath it.
//!
//! # Text stamps
//!
//! Drawn in opaque (or `opacity`-adjusted) black Helvetica at a size
//! derived from the page's smaller dimension and `scale` —
//! `StampOptions` deliberately has no `font_size` field; `scale` is the
//! sizing knob for both text and images.
//!
//! # Image stamps
//!
//! `image_png` is decoded with the `image` crate (PNG, and anything else
//! its enabled decoders recognize), alpha-composited onto white, and
//! embedded as an **uncompressed** `/Image` XObject — `lopdf` does not
//! expose a flate encoder to this crate, so the stream is stored raw and
//! the whole document is recompressed at the end via `Document::compress`
//! (which does have access to flate internally). The image is scaled so
//! its largest dimension fits in roughly a third of the page's smaller
//! dimension times `scale`, then clamped to the page area.
//!
//! `/UserUnit` is ignored (see `overlay` module docs).

use lopdf::content::Operation;
use lopdf::{Object, Stream, dictionary};

use crate::crates::core::engine::EngineLimits;

use crate::crates::pdf_engine::error::{PdfError, PdfResult};

use super::load::{load_pdf, lopdf_err, reject_encrypted, save_pdf};
use super::pages::parse_page_range;
use super::watermark::{
    MarkPosition, add_content_stream, append_page_overlay, box_origin, encode_operations,
    ensure_ext_gstate, ensure_font_resource, ensure_xobject_resource, estimate_text_width,
    media_box, pdf_text_operand, position_point,
};

/// Options for [`stamp`]. Exactly one of `text` / `image_png` must be
/// set.
#[derive(Debug, Clone)]
pub struct StampOptions {
    /// Stamp text. Mutually exclusive with `image_png`; whitespace-only
    /// text counts as absent.
    pub text: Option<String>,
    /// PNG image bytes (decoded via the `image` crate). Mutually
    /// exclusive with `text`.
    pub image_png: Option<Vec<u8>>,
    /// Where on the page the stamp is anchored. Default `Center`.
    pub position: MarkPosition,
    /// Size multiplier in `0.05..=20`. For images it scales the fitted
    /// display size; for text it scales the derived font size.
    /// Default 1.0.
    pub scale: f32,
    /// Opacity in `0.0..=1.0` via an ExtGState resource. Default 1.0
    /// (fully opaque — stamps are meant to be seen).
    pub opacity: f32,
    /// Distance between the stamp's bounding box and the page edge for
    /// edge/corner positions. Default 36.
    pub margin: f32,
    /// Page range spec (1-based, e.g. `"1-3,8"` or `"all"`). `None`
    /// stamps every page.
    pub pages: Option<String>,
}

impl Default for StampOptions {
    fn default() -> Self {
        Self {
            text: None,
            image_png: None,
            position: MarkPosition::Center,
            scale: 1.0,
            opacity: 1.0,
            margin: 36.0,
            pages: None,
        }
    }
}

/// Decode `image_png`, composite it onto white (flattening alpha), and
/// return `(width, height, rgb_bytes)`.
fn decode_stamp_image(png: &[u8], limits: &EngineLimits) -> PdfResult<(u32, u32, Vec<u8>)> {
    let image = image::load_from_memory(png)
        .map_err(|err| PdfError::InvalidInput(format!("image_png could not be decoded: {err}")))?;
    let (w, h) = (image.width(), image.height());
    if let Some(max_pixels) = limits.max_pixels {
        let pixels = u64::from(w) * u64::from(h);
        if pixels > max_pixels {
            return Err(PdfError::LimitExceeded(format!(
                "image dimensions {w}x{h} ({pixels} pixels) exceed max_pixels {max_pixels}"
            )));
        }
    }
    if w == 0 || h == 0 {
        return Err(PdfError::InvalidInput(
            "image_png decoded to a zero-sized image".to_string(),
        ));
    }
    let rgba = image.to_rgba8();
    // Flatten onto white so transparent PNGs don't stamp black boxes.
    let mut rgb = vec![0u8; rgba.len() / 4 * 3];
    for (src, dst) in rgba.chunks_exact(4).zip(rgb.chunks_exact_mut(3)) {
        let a = u32::from(src[3]);
        let inv = 255 - a;
        for c in 0..3 {
            // src[c] * a + 255 * (1 - a), scaled back to 0..=255.
            dst[c] = ((u32::from(src[c]) * a + 255 * inv + 127) / 255) as u8;
        }
    }
    Ok((w, h, rgb))
}

/// Build the content-stream operations for a text stamp on one page.
#[allow(clippy::too_many_arguments)]
fn text_stamp_operations(
    text: &str,
    font_size: f32,
    margin: f32,
    position: MarkPosition,
    page: (f32, f32, f32, f32),
    font_name: &str,
    gs_name: Option<&str>,
) -> Vec<Operation> {
    let (x0, y0, x1, y1) = page;
    let page_w = x1 - x0;
    let page_h = y1 - y0;

    let mark_w = estimate_text_width(text, font_size);
    let mark_h = font_size;
    let anchor = position_point(position, page_w, page_h, margin);
    let (bx, by) = box_origin(position, anchor, mark_w, mark_h);
    // Absolute lower-left of the mark box; baseline sits at box bottom.
    let tx = x0 + bx;
    let ty = y0 + by;

    let mut ops = vec![Operation::new("q", vec![])];
    if let Some(gs) = gs_name {
        ops.push(Operation::new(
            "gs",
            vec![Object::Name(gs.as_bytes().to_vec())],
        ));
    }
    ops.extend([
        Operation::new("BT", vec![]),
        Operation::new(
            "Tf",
            vec![
                Object::Name(font_name.as_bytes().to_vec()),
                Object::Real(font_size),
            ],
        ),
        Operation::new("Td", vec![Object::Real(tx), Object::Real(ty)]),
        Operation::new("Tj", vec![pdf_text_operand(text)]),
        Operation::new("ET", vec![]),
        Operation::new("Q", vec![]),
    ]);
    ops
}

/// Apply a stamp to `input` and return the new PDF bytes.
///
/// Exactly one of `opts.text` / `opts.image_png` must be `Some` —
/// neither or both is [`PdfError::InvalidInput`]. `scale` must be finite
/// and in `0.05..=20`; `opacity` in `0.0..=1.0`; `margin` finite and
/// non-negative. The page range is validated against the actual page
/// count. Returns [`PdfError::Encrypted`] for encrypted input.
pub fn stamp(input: &[u8], opts: &StampOptions, limits: &EngineLimits) -> PdfResult<Vec<u8>> {
    let text = opts
        .text
        .as_deref()
        .map(str::trim)
        .filter(|t| !t.is_empty());
    let has_text = text.is_some();
    let has_image = opts.image_png.is_some();
    match (has_text, has_image) {
        (true, false) | (false, true) => {}
        _ => {
            return Err(PdfError::InvalidInput(
                "stamp requires exactly one of text or image_png".to_string(),
            ));
        }
    }
    if !opts.scale.is_finite() || !(0.05..=20.0).contains(&opts.scale) {
        return Err(PdfError::InvalidInput(format!(
            "stamp scale must be in [0.05, 20], got {}",
            opts.scale
        )));
    }
    if !opts.opacity.is_finite() || !(0.0..=1.0).contains(&opts.opacity) {
        return Err(PdfError::InvalidInput(format!(
            "stamp opacity must be in [0, 1], got {}",
            opts.opacity
        )));
    }
    if !opts.margin.is_finite() || opts.margin < 0.0 {
        return Err(PdfError::InvalidInput(format!(
            "stamp margin must be a non-negative finite number, got {}",
            opts.margin
        )));
    }

    // Decode the image once, up front — a bad PNG fails before any
    // document mutation.
    let image = match &opts.image_png {
        Some(png) => Some(decode_stamp_image(png, limits)?),
        None => None,
    };

    let mut doc = load_pdf(input, limits)?;
    reject_encrypted(&doc)?;

    let pages_map = doc.get_pages();
    let page_count = pages_map.len() as u32;
    let selected: Vec<u32> = match &opts.pages {
        None => (1..=page_count).collect(),
        Some(spec) => parse_page_range(spec, page_count)?,
    };

    // The image XObject is embedded once and shared by every stamped
    // page — its resource entry is added per page below.
    let image_stream_id = match &image {
        Some((w, h, rgb)) => {
            let stream = Stream::new(
                dictionary! {
                    "Type" => "XObject",
                    "Subtype" => "Image",
                    "Width" => *w as i64,
                    "Height" => *h as i64,
                    "ColorSpace" => "DeviceRGB",
                    "BitsPerComponent" => 8,
                    "Interpolate" => true,
                },
                rgb.clone(),
            );
            Some(
                doc.try_add_object(Object::Stream(stream))
                    .map_err(lopdf_err)?,
            )
        }
        None => None,
    };

    for page_number in selected {
        let Some(&page_id) = pages_map.get(&page_number) else {
            return Err(PdfError::InvalidPageRange(format!(
                "page {page_number} not found in document"
            )));
        };
        let [bx0, by0, bx1, by1] = media_box(&doc, page_id);
        let (x0, y0) = (bx0 as f32, by0 as f32);
        let page_w = (bx1 - bx0) as f32;
        let page_h = (by1 - by0) as f32;

        let needs_gs = opts.opacity < 1.0;
        let gs_name = if needs_gs {
            Some(ensure_ext_gstate(&mut doc, page_id, opts.opacity)?)
        } else {
            None
        };

        let operations = if let Some(text) = text {
            // Font size derived from the page's smaller dimension so the
            // stamp stays proportional across page sizes; `scale`
            // multiplies it. Clamped to keep degenerate pages sane.
            let font_size = (page_w.min(page_h) * 0.05 * opts.scale).clamp(4.0, 300.0);
            let font_name = ensure_font_resource(&mut doc, page_id)?;
            text_stamp_operations(
                text,
                font_size,
                opts.margin,
                opts.position,
                (x0, y0, x0 + page_w, y0 + page_h),
                &font_name,
                gs_name.as_deref(),
            )
        } else {
            let (img_w, img_h, _) = image
                .as_ref()
                .ok_or_else(|| PdfError::InvalidInput("missing decoded image".to_string()))?;
            let image_name = ensure_xobject_resource(
                &mut doc,
                page_id,
                image_stream_id
                    .ok_or_else(|| PdfError::InvalidInput("missing image stream".to_string()))?,
            )?;

            // Fit the image so its largest dimension is ~1/3 of the
            // page's smaller dimension, times `scale`; then clamp to the
            // usable page area preserving aspect ratio.
            let fit = (page_w.min(page_h) / 3.0) * opts.scale;
            let max_px = (*img_w).max(*img_h) as f32;
            let mut dw = *img_w as f32 * (fit / max_px);
            let mut dh = *img_h as f32 * (fit / max_px);
            let max_w = (page_w - 2.0 * opts.margin).max(1.0);
            let max_h = (page_h - 2.0 * opts.margin).max(1.0);
            if dw > max_w || dh > max_h {
                let shrink = (max_w / dw).min(max_h / dh);
                dw *= shrink;
                dh *= shrink;
            }

            let anchor = position_point(opts.position, page_w, page_h, opts.margin);
            let (bx, by) = box_origin(opts.position, anchor, dw, dh);
            let x = x0 + bx;
            let y = y0 + by;

            let mut ops = vec![Operation::new("q", vec![])];
            if let Some(gs) = &gs_name {
                ops.push(Operation::new(
                    "gs",
                    vec![Object::Name(gs.as_bytes().to_vec())],
                ));
            }
            ops.extend([
                // PDF images paint the unit square; scale it to the
                // computed display size at the box origin.
                Operation::new(
                    "cm",
                    vec![
                        Object::Real(dw),
                        Object::Real(0.0),
                        Object::Real(0.0),
                        Object::Real(dh),
                        Object::Real(x),
                        Object::Real(y),
                    ],
                ),
                Operation::new("Do", vec![Object::Name(image_name.as_bytes().to_vec())]),
                Operation::new("Q", vec![]),
            ]);
            ops
        };

        let stream_id = add_content_stream(&mut doc, encode_operations(operations)?)?;
        // Stamps go on top of existing content.
        append_page_overlay(&mut doc, page_id, stream_id)?;
    }

    doc.compress();
    save_pdf(&mut doc)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::crates::core::engine::EngineLimits;
    use crate::crates::pdf_engine::ops::test_utils::{page_count, pdf_with_pages};
    use image::{DynamicImage, ImageFormat, RgbaImage};
    use std::io::Cursor;

    fn text_opts() -> StampOptions {
        StampOptions {
            text: Some("APPROVED".to_string()),
            ..Default::default()
        }
    }

    /// A 4x4 red PNG encoded in-test via the `image` crate.
    fn tiny_png() -> Vec<u8> {
        let mut img = RgbaImage::new(4, 4);
        for p in img.pixels_mut() {
            *p = image::Rgba([255, 0, 0, 255]);
        }
        let mut buf = Cursor::new(Vec::new());
        DynamicImage::ImageRgba8(img)
            .write_to(&mut buf, ImageFormat::Png)
            .unwrap();
        buf.into_inner()
    }

    fn image_opts() -> StampOptions {
        StampOptions {
            image_png: Some(tiny_png()),
            ..Default::default()
        }
    }

    #[test]
    fn text_stamp_produces_valid_pdf() {
        let pdf = pdf_with_pages(&["alpha", "beta"]);
        let out = stamp(&pdf, &text_opts(), &EngineLimits::default()).unwrap();
        assert_eq!(page_count(&out), 2);
        lopdf::Document::load_mem(&out).unwrap();
    }

    #[test]
    fn image_stamp_produces_valid_pdf() {
        let pdf = pdf_with_pages(&["alpha", "beta", "gamma"]);
        let out = stamp(&pdf, &image_opts(), &EngineLimits::default()).unwrap();
        assert_eq!(page_count(&out), 3);
        lopdf::Document::load_mem(&out).unwrap();
    }

    #[test]
    fn image_stamp_with_transparency_and_page_range() {
        // 2x2 semi-transparent PNG.
        let mut img = RgbaImage::new(2, 2);
        for p in img.pixels_mut() {
            *p = image::Rgba([0, 255, 0, 128]);
        }
        let mut buf = Cursor::new(Vec::new());
        DynamicImage::ImageRgba8(img)
            .write_to(&mut buf, ImageFormat::Png)
            .unwrap();
        let pdf = pdf_with_pages(&["one", "two", "three"]);
        let o = StampOptions {
            image_png: Some(buf.into_inner()),
            pages: Some("1,3".to_string()),
            opacity: 0.5,
            position: MarkPosition::TopRight,
            scale: 0.5,
            ..Default::default()
        };
        let out = stamp(&pdf, &o, &EngineLimits::default()).unwrap();
        assert_eq!(page_count(&out), 3);
    }

    #[test]
    fn rejects_missing_or_double_payload() {
        let pdf = pdf_with_pages(&["x"]);
        assert!(stamp(&pdf, &StampOptions::default(), &EngineLimits::default()).is_err());
        let both = StampOptions {
            text: Some("t".to_string()),
            image_png: Some(tiny_png()),
            ..Default::default()
        };
        assert!(stamp(&pdf, &both, &EngineLimits::default()).is_err());
        // Whitespace-only text does not count as a payload.
        let ws = StampOptions {
            text: Some("   ".to_string()),
            ..Default::default()
        };
        assert!(stamp(&pdf, &ws, &EngineLimits::default()).is_err());
    }

    #[test]
    fn rejects_bad_numeric_options() {
        let pdf = pdf_with_pages(&["x"]);
        for bad in [
            StampOptions {
                scale: 0.0,
                ..text_opts()
            },
            StampOptions {
                scale: 21.0,
                ..text_opts()
            },
            StampOptions {
                scale: f32::NAN,
                ..text_opts()
            },
            StampOptions {
                opacity: -0.5,
                ..text_opts()
            },
            StampOptions {
                opacity: 2.0,
                ..text_opts()
            },
            StampOptions {
                margin: -1.0,
                ..text_opts()
            },
            StampOptions {
                margin: f32::INFINITY,
                ..text_opts()
            },
        ] {
            assert!(
                stamp(&pdf, &bad, &EngineLimits::default()).is_err(),
                "options {bad:?} should be rejected"
            );
        }
    }

    #[test]
    fn rejects_undecodable_image() {
        let pdf = pdf_with_pages(&["x"]);
        let o = StampOptions {
            image_png: Some(b"definitely not a png".to_vec()),
            ..Default::default()
        };
        assert!(stamp(&pdf, &o, &EngineLimits::default()).is_err());
    }

    #[test]
    fn image_respects_max_pixels() {
        let pdf = pdf_with_pages(&["x"]);
        let limits = EngineLimits {
            max_pixels: Some(4),
            ..Default::default()
        };
        // tiny_png is 4x4 = 16 pixels > 4.
        assert!(stamp(&pdf, &image_opts(), &limits).is_err());
    }

    #[test]
    fn all_positions_do_not_panic_for_both_kinds() {
        let pdf = pdf_with_pages(&["p"]);
        for position in [
            MarkPosition::Center,
            MarkPosition::TopLeft,
            MarkPosition::TopRight,
            MarkPosition::BottomLeft,
            MarkPosition::BottomRight,
            MarkPosition::TopCenter,
            MarkPosition::BottomCenter,
        ] {
            for scale in [0.1, 1.0, 5.0] {
                let t = StampOptions {
                    position,
                    scale,
                    ..text_opts()
                };
                assert_eq!(
                    page_count(&stamp(&pdf, &t, &EngineLimits::default()).unwrap()),
                    1
                );
                let i = StampOptions {
                    position,
                    scale,
                    ..image_opts()
                };
                assert_eq!(
                    page_count(&stamp(&pdf, &i, &EngineLimits::default()).unwrap()),
                    1
                );
            }
        }
    }

    #[test]
    fn rejects_garbage_input() {
        assert!(stamp(b"junk", &text_opts(), &EngineLimits::default()).is_err());
    }
}
