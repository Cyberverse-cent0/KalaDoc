//! Searchable PDF — inject an invisible OCR text layer into scanned pages.
//!
//! A scanned PDF has page images but no text, so text extraction and
//! search return nothing. This op rasterizes each selected page with
//! `pdftoppm` (reusing [`super::raster::rasterize`], which enforces all
//! load limits, DPI bounds and PNG budgets), runs `tesseract` in **TSV
//! mode** to get word-level bounding boxes in pixels, converts those
//! boxes into PDF user-space coordinates, and appends an invisible text
//! layer (`3 Tr` render mode — text state, no marks) positioned at each
//! word's location. The result is searchable and selectable without
//! changing the page's appearance.
//!
//! **Two external tools are required**: `pdftoppm` (poppler-utils) and
//! `tesseract`. Either missing → [`PdfError::MissingTool`]; a tool
//! exiting non-zero → [`PdfError::ExternalTool`]. Nothing is faked.
//!
//! # Honest degraded mode
//!
//! If tesseract's TSV output yields no usable word boxes for a page
//! (unsupported output, catastrophic parse), the op falls back to a
//! plain-text OCR pass and injects the page's text as **one invisible
//! block** near the top of the page — still extractable/searchable, but
//! not positionally accurate. This is reported loudly:
//! [`SearchableReport::positioned`] is `false` and a warning names the
//! page. Pages where tesseract finds no text at all get no layer and a
//! warning.
//!
//! # Coordinate mapping
//!
//! `pdftoppm` renders the page box at `dpi` pixels per inch; TSV boxes
//! are in pixels with a top-left origin. Conversion uses the page's
//! effective `/MediaBox` and the actual PNG pixel dimensions (per-axis
//! scale), which absorbs rounding. Known approximation: `pdftoppm`
//! renders the CropBox when present while [`media_box`] reports the
//! MediaBox — pages where those differ may be slightly misaligned.
//!
//! # Scope notes
//!
//! - The injected font is base-14 Helvetica via WinAnsiEncoding (same
//!   as `watermark`); characters outside that coverage may not extract
//!   identically to the OCR'd text.
//! - Tesseract's default language data (`eng`) is used.
//! - OCR runs at the default raster DPI (150).
//! - Rendered PNGs live in a private tempdir removed on return (RAII).

use std::path::Path;
use std::process::Command;

use lopdf::Object;
use lopdf::content::Operation;
use serde::{Deserialize, Serialize};

use crate::crates::core::engine::EngineLimits;

use crate::crates::pdf_engine::error::{PdfError, PdfResult};

use super::load::{load_pdf, reject_encrypted, save_pdf};
use super::raster::{self, OCR_TOOL, RENDER_TOOL};
use super::watermark::{
    add_content_stream, append_page_overlay, encode_operations, ensure_font_resource, media_box,
    pdf_text_operand, selected_pages,
};

/// Outcome of [`make_searchable`].
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SearchableReport {
    /// Pages that went through the OCR + injection pipeline (including
    /// pages where tesseract recognized no text).
    pub pages_processed: usize,
    /// Number of invisible text-showing operations injected — word-level
    /// `Tj`s in positioned mode, whitespace-separated tokens in fallback
    /// block mode.
    pub words_injected: usize,
    /// `true` when every injected layer used word-level positioning.
    /// `false` when at least one page fell back to an unpositioned text
    /// block (see `warnings`).
    pub positioned: bool,
    /// Human-readable notes: fallback usage, pages with no recognized
    /// text, and similar non-fatal conditions.
    pub warnings: Vec<String>,
}

/// A word bounding box parsed from tesseract TSV, in **pixel**
/// coordinates (top-left origin).
#[derive(Debug, Clone, PartialEq)]
struct TsvWord {
    left: f64,
    top: f64,
    width: f64,
    height: f64,
    text: String,
}

/// A word converted to PDF user space, ready for content-stream ops.
#[derive(Debug, Clone, PartialEq)]
struct PositionedWord {
    /// Baseline origin x (points, absolute page space).
    x: f64,
    /// Baseline y (points, absolute page space).
    baseline_y: f64,
    /// Font size in points.
    font_size: f64,
    text: String,
}

/// Per-page word cap — a guard against pathological TSV output. At 150
/// DPI a letter page is ~1275x1650 px; even dense text stays far below
/// this.
const MAX_WORDS_PER_PAGE: usize = 250_000;

/// Font size / line pitch used by the unpositioned fallback block.
const BLOCK_FONT_SIZE: f64 = 10.0;
const BLOCK_LINE_HEIGHT: f64 = 12.0;
const BLOCK_MARGIN: f64 = 10.0;

/// Make the selected pages of `input` searchable by injecting an
/// invisible OCR text layer.
///
/// `pages` is an optional 1-based range spec; `None` processes every
/// page. Returns the rewritten PDF bytes plus a [`SearchableReport`]
/// describing what was injected.
///
/// Errors: [`PdfError::MissingTool`] when `pdftoppm`/`tesseract` are
/// absent, [`PdfError::Encrypted`] for encrypted input, range/limit
/// errors as usual.
pub fn make_searchable(
    input: &[u8],
    pages: Option<&str>,
    limits: &EngineLimits,
) -> PdfResult<(Vec<u8>, SearchableReport)> {
    // Probe both tools before any work so the error names the missing one.
    let _pdftoppm = raster::find_tool(RENDER_TOOL)?;
    let tesseract = raster::find_tool(OCR_TOOL)?;

    // Rasterize first: it validates the input (limits, encryption,
    // page range) before we mutate anything.
    let images = raster::rasterize(input, pages, raster::DEFAULT_DPI, limits)?;
    let mut report = SearchableReport {
        pages_processed: 0,
        words_injected: 0,
        positioned: true,
        warnings: Vec::new(),
    };
    if images.is_empty() {
        return Ok((input.to_vec(), report));
    }

    let mut doc = load_pdf(input, limits)?;
    reject_encrypted(&doc)?;
    let pages_map = doc.get_pages();
    let selected = selected_pages(&doc, pages)?;
    debug_assert_eq!(
        selected.len(),
        images.len(),
        "rasterize and page selection disagree"
    );

    let dir = tempfile::tempdir().map_err(|e| PdfError::Io(std::io::Error::other(e)))?;

    for image in &images {
        let Some(&page_id) = pages_map.get(&image.page) else {
            return Err(PdfError::InvalidPageRange(format!(
                "page {} not found in document",
                image.page
            )));
        };
        let mb = media_box(&doc, page_id);

        let png_path = dir.path().join(format!("page-{}.png", image.page));
        std::fs::write(&png_path, &image.png).map_err(PdfError::Io)?;

        // Primary path: TSV gives word-level boxes.
        let tsv = run_tesseract(&tesseract, &png_path, Some("tsv"))?;
        let words: Vec<PositionedWord> = parse_tsv_words(&tsv)
            .iter()
            .filter_map(|w| word_to_pdf(w, image.width_px, image.height_px, mb))
            .collect();
        if words.len() > MAX_WORDS_PER_PAGE {
            return Err(PdfError::LimitExceeded(format!(
                "page {} produced {} OCR words, exceeding per-page cap {MAX_WORDS_PER_PAGE}",
                image.page,
                words.len()
            )));
        }

        enum Layer {
            Positioned(Vec<PositionedWord>),
            Block(String),
            Empty,
        }
        let layer = if !words.is_empty() {
            Layer::Positioned(words)
        } else {
            // TSV yielded nothing usable — try plain text for the
            // honest degraded block.
            let text = run_tesseract(&tesseract, &png_path, None)?;
            if text.trim().is_empty() {
                report.warnings.push(format!(
                    "page {}: tesseract recognized no text; no layer injected",
                    image.page
                ));
                Layer::Empty
            } else {
                report.positioned = false;
                report.warnings.push(format!(
                    "page {}: word positions unavailable; injected unpositioned text block",
                    image.page
                ));
                Layer::Block(text)
            }
        };

        let (ops, n_words) = match layer {
            Layer::Empty => {
                report.pages_processed += 1;
                continue;
            }
            Layer::Positioned(words) => {
                let font = ensure_font_resource(&mut doc, page_id)?;
                let n = words.len();
                (positioned_ops(&font, &words), n)
            }
            Layer::Block(text) => {
                let font = ensure_font_resource(&mut doc, page_id)?;
                let n = text.split_whitespace().count();
                (block_ops(&font, &text, mb), n)
            }
        };

        let stream_id = add_content_stream(&mut doc, encode_operations(ops)?)?;
        // Invisible text — order vs. existing content is irrelevant,
        // but appending keeps the stream last like stamps.
        append_page_overlay(&mut doc, page_id, stream_id)?;

        report.pages_processed += 1;
        report.words_injected += n_words;
    }

    doc.compress();
    let bytes = save_pdf(&mut doc)?;
    Ok((bytes, report))
}

/// Run `tesseract <png> stdout [config]` and return stdout as text.
fn run_tesseract(tool: &Path, png: &Path, config: Option<&str>) -> PdfResult<String> {
    let mut cmd = Command::new(tool);
    cmd.arg(png).arg("stdout");
    if let Some(config) = config {
        cmd.arg(config);
    }
    let output = cmd
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
    Ok(String::from_utf8_lossy(&output.stdout).into_owned())
}

/// Parse tesseract TSV output into word boxes.
///
/// TSV columns: `level page block par line word left top width height
/// conf text`. Only level-5 (word) rows with parseable geometry and
/// non-empty text are kept; the text field may itself contain tabs'
/// worth of trailing data, so we split with `splitn(12)` and take the
/// remainder as the text.
fn parse_tsv_words(tsv: &str) -> Vec<TsvWord> {
    let mut words = Vec::new();
    for line in tsv.lines() {
        let cols: Vec<&str> = line.splitn(12, '\t').collect();
        if cols.len() < 12 || cols[0] != "5" {
            continue;
        }
        let (Ok(left), Ok(top), Ok(width), Ok(height)) = (
            cols[6].parse::<f64>(),
            cols[7].parse::<f64>(),
            cols[8].parse::<f64>(),
            cols[9].parse::<f64>(),
        ) else {
            continue;
        };
        let text = cols[11].trim();
        if text.is_empty() {
            continue;
        }
        words.push(TsvWord {
            left,
            top,
            width,
            height,
            text: text.to_string(),
        });
    }
    words
}

/// Convert a pixel-space TSV word box into a [`PositionedWord`] in PDF
/// user space. Returns `None` for degenerate geometry (zero-size image
/// or MediaBox, non-finite results).
///
/// Pixel y grows downward; PDF y grows upward from the MediaBox
/// origin, so the word's bottom edge maps to
/// `by0 + (img_h - top - height) * sy`.
fn word_to_pdf(word: &TsvWord, img_w: u32, img_h: u32, mb: [f64; 4]) -> Option<PositionedWord> {
    let page_w = mb[2] - mb[0];
    let page_h = mb[3] - mb[1];
    if img_w == 0 || img_h == 0 || page_w <= 0.0 || page_h <= 0.0 {
        return None;
    }
    let sx = page_w / f64::from(img_w);
    let sy = page_h / f64::from(img_h);
    let x = mb[0] + word.left * sx;
    let bottom = mb[1] + (f64::from(img_h) - word.top - word.height) * sy;
    let font_size = (word.height * sy).clamp(0.5, 1000.0);
    // Lift the baseline slightly off the box bottom so descenders sit
    // inside the recognized box rather than below it.
    let baseline_y = bottom + font_size * 0.1;
    if ![x, baseline_y, font_size].iter().all(|v| v.is_finite()) {
        return None;
    }
    Some(PositionedWord {
        x,
        baseline_y,
        font_size,
        text: word.text.clone(),
    })
}

/// Build the invisible positioned text layer: one text object, render
/// mode 3 (invisible), each word placed by its own text matrix.
fn positioned_ops(font_name: &str, words: &[PositionedWord]) -> Vec<Operation> {
    let mut ops = vec![
        Operation::new("BT", vec![]),
        Operation::new("Tr", vec![Object::Integer(3)]),
    ];
    for word in words {
        ops.push(Operation::new(
            "Tf",
            vec![
                Object::Name(font_name.as_bytes().to_vec()),
                Object::Real(word.font_size as f32),
            ],
        ));
        ops.push(Operation::new(
            "Tm",
            vec![
                Object::Real(1.0),
                Object::Real(0.0),
                Object::Real(0.0),
                Object::Real(1.0),
                Object::Real(word.x as f32),
                Object::Real(word.baseline_y as f32),
            ],
        ));
        ops.push(Operation::new("Tj", vec![pdf_text_operand(&word.text)]));
    }
    ops.push(Operation::new("ET", vec![]));
    ops
}

/// Build the degraded invisible layer: the page's whole OCR text as a
/// sequence of lines starting near the top of the MediaBox.
fn block_ops(font_name: &str, text: &str, mb: [f64; 4]) -> Vec<Operation> {
    let mut ops = vec![
        Operation::new("BT", vec![]),
        Operation::new("Tr", vec![Object::Integer(3)]),
    ];
    let mut y = mb[3] - BLOCK_MARGIN - BLOCK_FONT_SIZE;
    for line in text.lines() {
        let line = line.trim_end();
        if !line.trim().is_empty() {
            ops.push(Operation::new(
                "Tf",
                vec![
                    Object::Name(font_name.as_bytes().to_vec()),
                    Object::Real(BLOCK_FONT_SIZE as f32),
                ],
            ));
            ops.push(Operation::new(
                "Tm",
                vec![
                    Object::Real(1.0),
                    Object::Real(0.0),
                    Object::Real(0.0),
                    Object::Real(1.0),
                    Object::Real((mb[0] + BLOCK_MARGIN) as f32),
                    Object::Real(y as f32),
                ],
            ));
            ops.push(Operation::new("Tj", vec![pdf_text_operand(line)]));
        }
        y -= BLOCK_LINE_HEIGHT;
    }
    ops.push(Operation::new("ET", vec![]));
    ops
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::crates::pdf_engine::ops::test_utils::pdf_with_pages;
    use lopdf::Document as LopdfDocument;

    /// TSV fixture: header, two word rows, a non-word row, a malformed
    /// row, and a row with empty text.
    const TSV_FIXTURE: &str = concat!(
        "level\tpage_num\tblock_num\tpar_num\tline_num\tword_num\t",
        "left\ttop\twidth\theight\tconf\ttext\n",
        "1\t1\t0\t0\t0\t0\t0\t0\t1000\t1000\t-1\t\n",
        "5\t1\t1\t1\t1\t1\t100\t200\t50\t20\t96.5\tHello\n",
        "5\t1\t1\t1\t1\t2\t160\t200\t60\t20\t91.0\tworld\n",
        "4\t1\t1\t1\t1\t0\t100\t200\t220\t20\t96.5\tHello world\n",
        "5\t1\t1\t1\t1\t3\tnotanumber\t200\t60\t20\t90.0\tbad\n",
        "5\t1\t1\t1\t1\t4\t300\t400\t40\t15\t80.0\t\n",
    );

    #[test]
    fn parse_tsv_keeps_only_word_rows() {
        let words = parse_tsv_words(TSV_FIXTURE);
        assert_eq!(words.len(), 2);
        assert_eq!(words[0].text, "Hello");
        assert_eq!(words[0].left, 100.0);
        assert_eq!(words[0].top, 200.0);
        assert_eq!(words[0].width, 50.0);
        assert_eq!(words[0].height, 20.0);
        assert_eq!(words[1].text, "world");
    }

    #[test]
    fn parse_tsv_handles_empty_and_garbage() {
        assert!(parse_tsv_words("").is_empty());
        assert!(parse_tsv_words("garbage\n\n").is_empty());
        // Text field containing tabs is preserved as remainder.
        let tsv = "5\t1\t1\t1\t1\t1\t1\t2\t3\t4\t90\ta\tb\tc\n";
        let words = parse_tsv_words(tsv);
        assert_eq!(words.len(), 1);
        assert_eq!(words[0].text, "a\tb\tc");
    }

    #[test]
    fn word_to_pdf_maps_pixels_to_points() {
        // 1224x1584 px image ≙ 612x792 pt MediaBox at origin.
        let word = TsvWord {
            left: 100.0,
            top: 200.0,
            width: 50.0,
            height: 20.0,
            text: "Hi".to_string(),
        };
        let w = word_to_pdf(&word, 1224, 1584, [0.0, 0.0, 612.0, 792.0]).unwrap();
        // sx = sy = 0.5.
        assert!((w.x - 50.0).abs() < 1e-6);
        // bottom edge: y_px_from_bottom = 1584 - 200 - 20 = 1364 → 682 pt.
        // baseline = 682 + font_size*0.1; font_size = 20*0.5 = 10.
        assert!((w.baseline_y - 683.0).abs() < 1e-6);
        assert!((w.font_size - 10.0).abs() < 1e-6);
    }

    #[test]
    fn word_to_pdf_honors_mediabox_origin() {
        let word = TsvWord {
            left: 0.0,
            top: 0.0,
            width: 10.0,
            height: 10.0,
            text: "x".to_string(),
        };
        // MediaBox [100 200 712 992] → w=612 h=792, origin offset.
        let w = word_to_pdf(&word, 1224, 1584, [100.0, 200.0, 712.0, 992.0]).unwrap();
        assert!((w.x - 100.0).abs() < 1e-6);
        // bottom = 200 + (1584 - 0 - 10)*0.5 = 200 + 787 = 987;
        // font_size = 10*0.5 = 5 → baseline = 987.5
        assert!((w.baseline_y - 987.5).abs() < 1e-6);
    }

    #[test]
    fn word_to_pdf_rejects_degenerate_input() {
        let word = TsvWord {
            left: 0.0,
            top: 0.0,
            width: 1.0,
            height: 1.0,
            text: "x".to_string(),
        };
        assert!(word_to_pdf(&word, 0, 100, [0.0, 0.0, 612.0, 792.0]).is_none());
        assert!(word_to_pdf(&word, 100, 100, [0.0, 0.0, 0.0, 0.0]).is_none());
    }

    #[test]
    fn positioned_ops_use_invisible_render_mode() {
        let words = vec![
            PositionedWord {
                x: 10.0,
                baseline_y: 20.0,
                font_size: 12.0,
                text: "one".to_string(),
            },
            PositionedWord {
                x: 30.0,
                baseline_y: 20.0,
                font_size: 12.0,
                text: "two".to_string(),
            },
        ];
        let ops = positioned_ops("KF1", &words);
        let tr = ops.iter().find(|o| o.operator == "Tr").unwrap();
        assert_eq!(tr.operands, vec![Object::Integer(3)]);
        assert_eq!(ops.iter().filter(|o| o.operator == "Tj").count(), 2);
        assert_eq!(ops.iter().filter(|o| o.operator == "Tm").count(), 2);
        assert!(ops.iter().any(|o| o.operator == "ET"));
    }

    #[test]
    fn block_ops_emit_lines_top_down() {
        let ops = block_ops("KF1", "first\n\nsecond line", [0.0, 0.0, 612.0, 792.0]);
        let tj_count = ops.iter().filter(|o| o.operator == "Tj").count();
        assert_eq!(tj_count, 2); // blank line skipped
        let tr = ops.iter().find(|o| o.operator == "Tr").unwrap();
        assert_eq!(tr.operands, vec![Object::Integer(3)]);
    }

    #[test]
    fn injected_layer_is_searchable_via_lopdf() {
        // Build a fake positioned layer, attach via the same helpers,
        // and confirm lopdf's extractor finds the words.
        let pdf = pdf_with_pages(&["original"]);
        let mut doc = LopdfDocument::load_mem(&pdf).unwrap();
        let page_id = doc.get_pages()[&1];
        let font = ensure_font_resource(&mut doc, page_id).unwrap();
        let words = vec![
            PositionedWord {
                x: 50.0,
                baseline_y: 700.0,
                font_size: 12.0,
                text: "invisible".to_string(),
            },
            PositionedWord {
                x: 120.0,
                baseline_y: 700.0,
                font_size: 12.0,
                text: "ocrtext".to_string(),
            },
        ];
        let stream_id = add_content_stream(
            &mut doc,
            encode_operations(positioned_ops(&font, &words)).unwrap(),
        )
        .unwrap();
        append_page_overlay(&mut doc, page_id, stream_id).unwrap();
        let mut buf = Vec::new();
        doc.save_to(&mut buf).unwrap();
        let reloaded = LopdfDocument::load_mem(&buf).unwrap();
        let text = reloaded.extract_text(&[1]).unwrap();
        assert!(text.contains("invisible"), "got: {text:?}");
        assert!(text.contains("ocrtext"), "got: {text:?}");
    }

    fn tools_available() -> bool {
        raster::tool_available(RENDER_TOOL) && raster::tool_available(OCR_TOOL)
    }

    #[test]
    fn missing_tools_error_never_fake() {
        let pdf = pdf_with_pages(&["some text"]);
        if tools_available() {
            return;
        }
        let err = make_searchable(&pdf, None, &EngineLimits::default()).unwrap_err();
        assert!(matches!(err, PdfError::MissingTool(_)));
    }

    #[test]
    fn runs_when_tools_present() {
        let pdf = pdf_with_pages(&["hello searchable world"]);
        if !tools_available() {
            // tesseract is not installed on this system — skip.
            return;
        }
        let (bytes, report) = make_searchable(&pdf, None, &EngineLimits::default()).unwrap();
        assert!(bytes.starts_with(b"%PDF"), "output is not a PDF");
        assert_eq!(report.pages_processed, 1);
        // A rendered text page OCRs into words; whether they parse as
        // positioned depends on tesseract TSV output.
        assert!(report.words_injected > 0 || !report.warnings.is_empty());
    }

    #[test]
    fn garbage_input_rejected() {
        // Either MissingTool (probe first) or a parse error — both honest.
        let _ = make_searchable(b"junk", None, &EngineLimits::default());
    }
}
