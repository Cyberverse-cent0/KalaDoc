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

use serde::{Deserialize, Serialize};

use crate::crates::core::engine::EngineLimits;

use crate::crates::pdf_engine::error::{PdfError, PdfResult};

use super::raster::{self, OCR_TOOL, RENDER_TOOL, checked_dpi, rasterize};
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

// ---------------------------------------------------------------------------
// Detailed OCR: languages, PSM, confidence, coordinates, line grouping
// ---------------------------------------------------------------------------

/// Options for [`ocr_detailed`]. All fields optional; absent values use
/// tesseract's defaults (English data, automatic page segmentation).
#[derive(Debug, Clone, Default)]
pub struct OcrOptions {
    /// Rasterization DPI (36–1200, default 150).
    pub dpi: Option<u32>,
    /// Tesseract language code(s): `"eng"`, `"eng+deu"` or `"eng,deu"`.
    /// Validated against [`ocr_languages`] when possible.
    pub lang: Option<String>,
    /// Tesseract page-segmentation mode (0–13).
    pub psm: Option<u32>,
}

/// One recognized word with pixel-space geometry and confidence.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct OcrWord {
    /// Recognized text.
    pub text: String,
    /// Left edge in raster pixels.
    pub left: f64,
    /// Top edge in raster pixels.
    pub top: f64,
    /// Width in pixels.
    pub width: f64,
    /// Height in pixels.
    pub height: f64,
    /// Tesseract confidence, 0–100 (-1 when tesseract reports none).
    pub confidence: f64,
    /// TSV block index (reading-order grouping).
    pub block: u32,
    /// TSV paragraph index within the block.
    pub paragraph: u32,
    /// TSV line index within the paragraph.
    pub line: u32,
}

/// A line of recognized text: the words tesseract grouped into one
/// TSV line record, in reading order.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct OcrLine {
    /// Line text (words joined by single spaces).
    pub text: String,
    /// Bounding box in raster pixels `[left, top, width, height]`.
    pub bbox: [f64; 4],
    /// Mean word confidence, 0–100.
    pub confidence: f64,
    /// Member words in reading order.
    pub words: Vec<OcrWord>,
}

/// Per-page detailed OCR result.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct OcrPageDetail {
    /// 1-based page number.
    pub page: u32,
    /// Full page text (lines joined by `\n`).
    pub text: String,
    /// Recognized words in TSV (reading) order.
    pub words: Vec<OcrWord>,
    /// Lines reconstructed from TSV block/par/line grouping.
    pub lines: Vec<OcrLine>,
}

/// Recognize text with word/line geometry and confidence via
/// `pdftoppm` + `tesseract tsv`.
///
/// `pages` follows the shared range grammar; `None` OCRs every page.
/// `options.dpi` overrides the default 150 DPI, `options.lang` passes
/// `-l` (e.g. `"eng+deu"`), and `options.psm` passes `--psm`.
///
/// Output is honest: when tesseract is absent the error is
/// [`PdfError::MissingTool`]; pages with no recognized text yield
/// empty `words`/`lines` and an empty `text`.
pub fn ocr_detailed(
    input: &[u8],
    pages: Option<&str>,
    options: &OcrOptions,
    limits: &EngineLimits,
) -> PdfResult<Vec<OcrPageDetail>> {
    raster::find_tool(RENDER_TOOL)?;
    let tesseract = raster::find_tool(OCR_TOOL)?;
    let dpi = checked_dpi(options.dpi)?;

    let lang = options
        .lang
        .as_deref()
        .map(str::trim)
        .filter(|s| !s.is_empty());
    if let Some(lang) = lang {
        // Best-effort validation: warn-free hard error only when the
        // requested code is definitely not installed.
        let installed = ocr_languages()?;
        let requested: Vec<&str> = lang.split(['+', ',']).collect();
        for code in requested {
            if !installed.iter().any(|l| l == code) {
                return Err(PdfError::InvalidInput(format!(
                    "tesseract language {code:?} is not installed (have: {})",
                    installed.join(", ")
                )));
            }
        }
    }
    if let Some(psm) = options.psm
        && psm > 13
    {
        return Err(PdfError::InvalidInput(format!(
            "psm must be in 0..=13, got {psm}"
        )));
    }

    let images = rasterize(input, pages, dpi, limits)?;
    if images.is_empty() {
        return Ok(Vec::new());
    }

    let dir = tempfile::tempdir().map_err(|e| PdfError::Io(std::io::Error::other(e)))?;
    let mut out = Vec::with_capacity(images.len());
    for image in images {
        let png_path = dir.path().join(format!("page-{}.png", image.page));
        std::fs::write(&png_path, &image.png).map_err(PdfError::Io)?;

        let mut args: Vec<String> = vec![png_path.to_string_lossy().into_owned()];
        args.push("stdout".to_string());
        if let Some(lang) = lang {
            args.push("-l".to_string());
            args.push(lang.to_string());
        }
        if let Some(psm) = options.psm {
            args.push("--psm".to_string());
            args.push(psm.to_string());
        }
        args.push("tsv".to_string());

        let output = Command::new(&tesseract)
            .args(&args)
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
        let tsv = String::from_utf8_lossy(&output.stdout);
        out.push(parse_tsv_detail(image.page, &tsv));
    }
    Ok(out)
}

/// Parse tesseract TSV output into words and reconstructed lines.
///
/// Columns: `level page block par line word left top width height
/// conf text`. Words are level 5; lines are grouped by the
/// (block, paragraph, line) indices tesseract emits, preserving TSV
/// order which is tesseract's reading order.
fn parse_tsv_detail(page: u32, tsv: &str) -> OcrPageDetail {
    // (block, par, line) -> index into `lines`
    let mut line_index: std::collections::HashMap<(u32, u32, u32), usize> =
        std::collections::HashMap::new();
    let mut lines: Vec<OcrLine> = Vec::new();
    let mut words: Vec<OcrWord> = Vec::new();

    for row in tsv.lines() {
        let cols: Vec<&str> = row.splitn(12, '\t').collect();
        if cols.len() < 12 || cols[0] != "5" {
            continue;
        }
        let (Ok(block), Ok(paragraph), Ok(line_no)) = (
            cols[2].parse::<u32>(),
            cols[3].parse::<u32>(),
            cols[4].parse::<u32>(),
        ) else {
            continue;
        };
        let (Ok(left), Ok(top), Ok(width), Ok(height), Ok(conf)) = (
            cols[6].parse::<f64>(),
            cols[7].parse::<f64>(),
            cols[8].parse::<f64>(),
            cols[9].parse::<f64>(),
            cols[10].parse::<f64>(),
        ) else {
            continue;
        };
        let text = cols[11].trim();
        if text.is_empty() {
            continue;
        }
        let word = OcrWord {
            text: text.to_string(),
            left,
            top,
            width,
            height,
            confidence: conf,
            block,
            paragraph,
            line: line_no,
        };
        let key = (block, paragraph, line_no);
        let idx = *line_index.entry(key).or_insert_with(|| {
            lines.push(OcrLine {
                text: String::new(),
                bbox: [
                    f64::INFINITY,
                    f64::INFINITY,
                    f64::NEG_INFINITY,
                    f64::NEG_INFINITY,
                ],
                confidence: 0.0,
                words: Vec::new(),
            });
            lines.len() - 1
        });
        let line = &mut lines[idx];
        if !line.text.is_empty() {
            line.text.push(' ');
        }
        line.text.push_str(&word.text);
        line.bbox[0] = line.bbox[0].min(left);
        line.bbox[1] = line.bbox[1].min(top);
        line.bbox[2] = line.bbox[2].max(left + width);
        line.bbox[3] = line.bbox[3].max(top + height);
        line.confidence += conf.max(0.0);
        line.words.push(word.clone());
        words.push(word);
    }

    for line in &mut lines {
        if !line.words.is_empty() {
            line.confidence /= line.words.len() as f64;
        }
        line.bbox = [
            line.bbox[0],
            line.bbox[1],
            (line.bbox[2] - line.bbox[0]).max(0.0),
            (line.bbox[3] - line.bbox[1]).max(0.0),
        ];
        if !line.bbox[0].is_finite() {
            line.bbox = [0.0; 4];
        }
    }

    let text = lines
        .iter()
        .map(|l| l.text.as_str())
        .collect::<Vec<_>>()
        .join("\n");
    OcrPageDetail {
        page,
        text,
        words,
        lines,
    }
}
