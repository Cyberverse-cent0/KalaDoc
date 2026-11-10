//! Render PDF pages to raster images via `pdftoppm`.
//!
//! Rasterization itself lives in [`super::raster`], the crate's single
//! subprocess boundary: it validates the input through [`load_pdf`]
//! (input size / page-count limits, encryption rejection), probes
//! `pdftoppm` on `PATH` ([`PdfError::MissingTool`] when absent),
//! enforces DPI bounds and per-page/total PNG byte budgets, and
//! returns decoded PNGs with their pixel dimensions.
//!
//! This op wraps that helper with the public contract:
//!
//! - `pages` selects input pages, but **only the first selected page
//!   is returned** — the return type is a single `Vec<u8>` image, so
//!   multi-page rendering is out of scope here (callers needing all
//!   pages can use the `raster` helper's `PageImage` list internally
//!   or split the document first). Only the first page is actually
//!   rasterized; later selected pages are not rendered at all.
//! - `dpi` is validated by [`checked_dpi`] (36–1200, default 150).
//! - `format` chooses the output encoding: `Png` returns the
//!   `pdftoppm` PNG bytes verbatim; `Jpeg` transcodes the rendered
//!   PNG through the `image` crate.
//! - `max_pixels` is enforced against the rendered pixel dimensions.

use std::io::Cursor;

use serde::{Deserialize, Serialize};

use crate::crates::core::engine::EngineLimits;

use crate::crates::pdf_engine::error::{PdfError, PdfResult};

use super::load::load_pdf;
use super::pages::parse_page_range;
use super::raster::{
    RENDER_TOOL, RasterFormat, RasterOptions, RenderedPage, checked_dpi, rasterize,
    rasterize_render,
};
use super::watermark::media_box;

/// Output image encoding for [`render`].
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum RenderFormat {
    /// PNG (lossless, returned as produced by `pdftoppm`).
    Png,
    /// JPEG (lossy; the rendered PNG is transcoded at default quality).
    Jpeg,
}

/// Render the first page selected by `pages` to an image.
///
/// `pages` is an optional 1-based range spec (`"1-3,8"`, `"all"`);
/// `None` selects page 1. Only the **first** selected page is
/// rendered and returned — see the module docs. `dpi` defaults to 150
/// and must be within the shared bounds (36–1200).
///
/// Returns the encoded image bytes (`image/png` or `image/jpeg`
/// depending on `format`). Encrypted documents are rejected. When
/// `pdftoppm` is not installed the error is
/// [`PdfError::MissingTool`]; a non-zero exit is
/// [`PdfError::ExternalTool`].
pub fn render(
    input: &[u8],
    pages: Option<&str>,
    dpi: Option<u32>,
    format: RenderFormat,
    limits: &EngineLimits,
) -> PdfResult<Vec<u8>> {
    let dpi = checked_dpi(dpi)?;

    // Resolve the first selected page up front so `rasterize` only
    // renders that single page instead of the whole selection.
    let doc = load_pdf(input, limits)?;
    let page_count = doc.get_pages().len() as u32;
    drop(doc); // rasterize re-loads for its own validation
    if page_count == 0 {
        return Err(PdfError::InvalidInput(
            "document has no pages to render".to_string(),
        ));
    }
    let first_page = match pages {
        Some(spec) => {
            let selected = parse_page_range(spec, page_count)?;
            *selected.first().ok_or_else(|| {
                PdfError::InvalidPageRange("page range selected no pages".to_string())
            })?
        }
        None => 1,
    };

    let mut images = rasterize(input, Some(&first_page.to_string()), dpi, limits)?;
    let image = images.pop().ok_or_else(|| PdfError::ExternalTool {
        tool: RENDER_TOOL.to_string(),
        detail: "no rendered page returned".to_string(),
    })?;

    if let Some(max_pixels) = limits.max_pixels {
        let pixels = u64::from(image.width_px) * u64::from(image.height_px);
        if pixels > max_pixels {
            return Err(PdfError::LimitExceeded(format!(
                "rendered page {} is {}x{} ({pixels} pixels), exceeding max_pixels {max_pixels}",
                image.page, image.width_px, image.height_px
            )));
        }
    }

    match format {
        RenderFormat::Png => Ok(image.png),
        RenderFormat::Jpeg => png_to_jpeg(&image.png),
    }
}

/// Transcode PNG bytes to JPEG using the `image` crate.
fn png_to_jpeg(png: &[u8]) -> PdfResult<Vec<u8>> {
    let decoded = image::load_from_memory_with_format(png, image::ImageFormat::Png)
        .map_err(|e| PdfError::Encode(std::io::Error::other(e)))?;
    let mut out = Cursor::new(Vec::new());
    decoded
        .write_to(&mut out, image::ImageFormat::Jpeg)
        .map_err(|e| PdfError::Encode(std::io::Error::other(e)))?;
    Ok(out.into_inner())
}

// ---------------------------------------------------------------------------
// Extended rendering: multi-page, regions, formats, modes
// ---------------------------------------------------------------------------

/// A rectangular region of a page in **PDF points**, origin at the
/// bottom-left of the page's effective `MediaBox` (the same coordinate
/// space as `MediaBox`/`CropBox` entries).
#[derive(Debug, Clone, Copy, PartialEq, serde::Serialize, serde::Deserialize)]
pub struct RenderRegion {
    /// Left edge (x of the lower-left corner).
    pub x: f64,
    /// Bottom edge (y of the lower-left corner).
    pub y: f64,
    /// Width in points; must be finite and positive.
    pub width: f64,
    /// Height in points; must be finite and positive.
    pub height: f64,
}

/// Options for [`render_pages`].
#[derive(Debug, Clone, Default)]
pub struct RenderPageOptions {
    /// Output encoding: PNG, JPEG or TIFF. Default PNG.
    pub format: RasterFormat,
    /// Grayscale output.
    pub grayscale: bool,
    /// Monochrome (1-bit) output; takes precedence over `grayscale`.
    pub monochrome: bool,
    /// Optional region to render, in PDF points. When set, only that
    /// rectangle is rasterized. The region is resolved against the
    /// **first** selected page's `MediaBox`; pages with different
    /// boxes get the same pixel crop (documented approximation).
    pub region: Option<RenderRegion>,
    /// Scale the longer page side to at most this many pixels.
    pub scale_to: Option<u32>,
    /// JPEG quality 1–100; ignored for PNG/TIFF.
    pub jpeg_quality: Option<u8>,
}

/// Render every page selected by `pages` with the given options.
///
/// Unlike [`render`] — which returns only the first selected page as
/// a single buffer — this returns one [`RenderedPage`] per selected
/// page, in ascending page order, each carrying its own encoded
/// `data`/`format`. `dpi`, encryption, limits and `MissingTool`
/// semantics are identical to [`render`].
///
/// `region` coordinates are PDF points; they are converted to the
/// pixel-space crop `pdftoppm -x -y -W -H` expects using the render
/// DPI and the first selected page's `MediaBox`.
pub fn render_pages(
    input: &[u8],
    pages: Option<&str>,
    dpi: Option<u32>,
    options: &RenderPageOptions,
    limits: &EngineLimits,
) -> PdfResult<Vec<RenderedPage>> {
    let dpi = checked_dpi(dpi)?;
    if let Some(q) = options.jpeg_quality
        && !(1..=100).contains(&q)
    {
        return Err(PdfError::InvalidInput(format!(
            "jpeg_quality must be in 1..=100, got {q}"
        )));
    }

    // Resolve the crop once, in output pixels, from the first
    // selected page's MediaBox (see RenderPageOptions::region docs).
    let crop_px = match options.region {
        Some(region) => Some(region_to_px(input, pages, dpi, region, limits)?),
        None => None,
    };

    let opts = RasterOptions {
        format: options.format,
        grayscale: options.grayscale,
        monochrome: options.monochrome,
        crop_px,
        scale_to: options.scale_to,
        jpeg_quality: options.jpeg_quality,
    };
    rasterize_render(input, pages, dpi, &opts, limits)
}

/// Convert a [`RenderRegion`] in PDF points to `pdftoppm`'s
/// top-left-origin pixel crop for the first selected page.
fn region_to_px(
    input: &[u8],
    pages: Option<&str>,
    dpi: u32,
    region: RenderRegion,
    limits: &EngineLimits,
) -> PdfResult<[u32; 4]> {
    let RenderRegion {
        x,
        y,
        width,
        height,
    } = region;
    if !x.is_finite()
        || !y.is_finite()
        || !width.is_finite()
        || !height.is_finite()
        || width <= 0.0
        || height <= 0.0
    {
        return Err(PdfError::InvalidInput(
            "region must have finite positive width/height".to_string(),
        ));
    }
    let doc = load_pdf(input, limits)?;
    let page_count = doc.get_pages().len() as u32;
    if page_count == 0 {
        return Err(PdfError::InvalidInput(
            "document has no pages to render".to_string(),
        ));
    }
    let first_page = match pages {
        Some(spec) => *parse_page_range(spec, page_count)?.first().ok_or_else(|| {
            PdfError::InvalidPageRange("page range selected no pages".to_string())
        })?,
        None => 1,
    };
    let pages_map = doc.get_pages();
    let page_id = pages_map
        .get(&first_page)
        .copied()
        .ok_or_else(|| PdfError::InvalidPageRange(format!("page {first_page} not found")))?;
    let mb = media_box(&doc, page_id);

    // PDF points have a bottom-left origin; pdftoppm crop pixels have
    // a top-left origin. Scale is dpi/72.
    let scale = f64::from(dpi) / 72.0;
    let px_x = ((x - mb[0]) * scale).round().max(0.0) as u32;
    let px_y = ((mb[3] - (y + height)) * scale).round().max(0.0) as u32;
    let px_w = (width * scale).round().max(1.0) as u32;
    let px_h = (height * scale).round().max(1.0) as u32;
    Ok([px_x, px_y, px_w, px_h])
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;
    use crate::crates::pdf_engine::ops::raster::tool_available;
    use crate::crates::pdf_engine::ops::test_utils::pdf_with_pages;

    fn limits() -> EngineLimits {
        EngineLimits::default()
    }

    /// Rasterization needs `pdftoppm`; tests that need pixels skip
    /// cleanly (returning `MissingTool` is also a valid assertion).
    fn render_available() -> bool {
        tool_available(RENDER_TOOL)
    }

    #[test]
    fn renders_or_reports_missing_tool() {
        let pdf = pdf_with_pages(&["hello"]);
        match render(&pdf, None, None, RenderFormat::Png, &limits()) {
            Ok(png) => {
                assert_eq!(&png[..8], b"\x89PNG\r\n\x1a\n");
            }
            Err(PdfError::MissingTool(tool)) => {
                assert_eq!(tool, "pdftoppm");
            }
            Err(other) => panic!("unexpected error: {other}"),
        }
    }

    #[test]
    fn jpeg_format_produces_jpeg() {
        let pdf = pdf_with_pages(&["jpeg me"]);
        if !render_available() {
            return;
        }
        let jpeg = render(&pdf, None, None, RenderFormat::Jpeg, &limits()).unwrap();
        assert_eq!(&jpeg[..3], &[0xFF, 0xD8, 0xFF]);
    }

    #[test]
    fn invalid_dpi_rejected_before_tool_check() {
        let pdf = pdf_with_pages(&["hello"]);
        assert!(matches!(
            render(&pdf, None, Some(5), RenderFormat::Png, &limits()),
            Err(PdfError::InvalidInput(_))
        ));
    }

    #[test]
    fn invalid_page_range_rejected() {
        let pdf = pdf_with_pages(&["hello"]);
        assert!(matches!(
            render(&pdf, Some("9"), None, RenderFormat::Png, &limits()),
            Err(PdfError::InvalidPageRange(_))
        ));
    }

    #[test]
    fn first_selected_page_returned() {
        let pdf = pdf_with_pages(&["p1", "p2", "p3"]);
        if !render_available() {
            return;
        }
        // "2-3" selects pages 2 and 3; only page 2 is rendered.
        let png = render(&pdf, Some("2-3"), Some(36), RenderFormat::Png, &limits()).unwrap();
        assert_eq!(&png[..8], b"\x89PNG\r\n\x1a\n");
    }

    #[test]
    fn garbage_rejected() {
        assert!(render(b"junk", None, None, RenderFormat::Png, &limits()).is_err());
    }
}
