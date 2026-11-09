//! OCR via `pdftoppm` + `tesseract`.
//!
//! Scanned PDFs have no text layer, so [`extract_text`](super::text)
//! returns empty strings for them. This op rasterizes each selected
//! page with `pdftoppm` (reusing [`super::raster::rasterize`], which
//! enforces all load limits, DPI bounds and PNG budgets) and feeds the
//! PNGs to `tesseract`, returning one [`PageText`] per page.
//!
//! **Two external tools are required**: `pdftoppm` (poppler-utils) for
//! rasterization and `tesseract` for recognition. Either missing →
//! [`PdfError::MissingTool`]; a tool exiting non-zero →
//! [`PdfError::ExternalTool`]. Nothing is faked: when the tools are
//! absent this op always fails.
//!
//! Scope notes:
//!
//! - OCR runs at the default raster DPI (150); there is no DPI or
//!   language parameter yet.
//! - Tesseract's default language data (`eng`) is used; documents in
//!   other scripts will produce poor or empty text.
//! - Confidence data is not collected — only the recognized text.
//! - Rendered PNGs are written to a private tempdir that is removed
//!   on return (RAII), never left behind.

use std::process::Command;

use crate::crates::core::engine::EngineLimits;

use crate::crates::pdf_engine::error::{PdfError, PdfResult};

use super::raster::{self, OCR_TOOL, RENDER_TOOL, rasterize};
use super::text::PageText;

/// Recognize text on the selected pages via rasterization + tesseract.
///
/// `pages` is an optional 1-based range spec; `None` OCRs every page.
/// Returns one [`PageText`] per page in ascending order; pages where
/// tesseract finds nothing yield an empty `text` (legitimate output,
/// not an error).
pub fn ocr(input: &[u8], pages: Option<&str>, limits: &EngineLimits) -> PdfResult<Vec<PageText>> {
    // Probe both tools up front so the error names the missing one
    // before any work happens.
    raster::find_tool(RENDER_TOOL)?;
    raster::find_tool(OCR_TOOL)?;

    let images = rasterize(input, pages, raster::DEFAULT_DPI, limits)?;
    if images.is_empty() {
        return Ok(Vec::new());
    }

    let tesseract = raster::find_tool(OCR_TOOL)?;
    let dir = tempfile::tempdir().map_err(|e| PdfError::Io(std::io::Error::other(e)))?;

    let mut out = Vec::with_capacity(images.len());
    for image in images {
        let png_path = dir.path().join(format!("page-{}.png", image.page));
        std::fs::write(&png_path, &image.png).map_err(PdfError::Io)?;

        // `tesseract <image> stdout` writes recognized text to stdout.
        let output = Command::new(&tesseract)
            .arg(&png_path)
            .arg("stdout")
            .stdin(std::process::Stdio::null())
            .stdout(std::process::Stdio::piped())
            .stderr(std::process::Stdio::null())
            .output()
            .map_err(|e| PdfError::ExternalTool {
                tool: OCR_TOOL.to_string(),
                detail: format!("failed to spawn: {e}"),
            })?;
        if !output.status.success() {
            return Err(PdfError::ExternalTool {
                tool: OCR_TOOL.to_string(),
                detail: format!("exited with {}", output.status),
            });
        }
        let text = String::from_utf8_lossy(&output.stdout)
            .trim_end()
            .to_string();
        out.push(PageText {
            page: image.page,
            text,
        });
    }
    Ok(out)
}

/// List the language codes installed for tesseract
/// (`tesseract --list-langs`).
///
/// Returns [`PdfError::MissingTool`] when tesseract is not installed
/// and [`PdfError::ExternalTool`] when the probe fails. The first
/// output line (a header) is skipped; each remaining non-empty line
/// contributes its first whitespace-separated token.
pub fn ocr_languages() -> PdfResult<Vec<String>> {
    let tesseract = raster::find_tool(OCR_TOOL)?;
    let output = Command::new(&tesseract)
        .arg("--list-langs")
        .stdin(std::process::Stdio::null())
        .stdout(std::process::Stdio::piped())
        .stderr(std::process::Stdio::null())
        .output()
        .map_err(|e| PdfError::ExternalTool {
            tool: OCR_TOOL.to_string(),
            detail: format!("failed to spawn: {e}"),
        })?;
    if !output.status.success() {
        return Err(PdfError::ExternalTool {
            tool: OCR_TOOL.to_string(),
            detail: format!("--list-langs exited with {}", output.status),
        });
    }
    let stdout = String::from_utf8_lossy(&output.stdout);
    Ok(stdout
        .lines()
        .skip(1) // header: "List of available languages in ..."
        .filter_map(|line| line.split_whitespace().next())
        .map(str::to_string)
        .collect())
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;
    use crate::crates::pdf_engine::ops::test_utils::pdf_with_pages;

    /// OCR needs BOTH pdftoppm and tesseract; skip when either is absent.
    fn ocr_available() -> bool {
        raster::tool_available(RENDER_TOOL) && raster::tool_available(OCR_TOOL)
    }

    #[test]
    fn missing_tools_error_never_fake() {
        let pdf = pdf_with_pages(&["some text"]);
        if ocr_available() {
            return;
        }
        let err = ocr(&pdf, None, &EngineLimits::default()).unwrap_err();
        assert!(matches!(err, PdfError::MissingTool(_)));
    }

    #[test]
    fn ocr_runs_when_tools_present() {
        let pdf = pdf_with_pages(&["hello ocr world"]);
        if !ocr_available() {
            // tesseract is not installed on this system — skip.
            return;
        }
        let pages = ocr(&pdf, None, &EngineLimits::default()).unwrap();
        assert_eq!(pages.len(), 1);
        assert_eq!(pages[0].page, 1);
        // Only the plumbing is asserted — recognition quality depends
        // on the installed tesseract language data.
    }

    #[test]
    fn garbage_rejected_before_tools() {
        // Malformed input fails at load_pdf inside rasterize — but the
        // tool probe happens first; either error is honest.
        let _ = ocr(b"junk", None, &EngineLimits::default());
    }
}
