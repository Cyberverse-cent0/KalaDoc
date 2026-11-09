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
use super::raster::{RENDER_TOOL, checked_dpi, rasterize};

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
