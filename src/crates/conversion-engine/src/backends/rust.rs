//! Pure-Rust conversion backend (lopdf-based).
//!
//! Supported pairs (deliberately small — this backend only advertises
//! what it can do correctly in-process):
//!
//! | From       | To   | Implementation                                       |
//! |------------|------|------------------------------------------------------|
//! | `txt`      | `pdf`| Hand-authored PDF via `lopdf`: A4/Letter pages,      |
//! |            |      | Helvetica/Helvetica-Bold/Courier base-14 fonts,      |
//! |            |      | word-wrapped to margins, paginated.                  |
//! | `markdown` | `pdf`| Same renderer plus minimal structure: `#` headings   |
//! |            |      | get larger bold sizes, list markers indent, fenced   |
//! |            |      | code blocks render in Courier. Not a real Markdown   |
//! |            |      | engine — links/images/html pass through as text.     |
//! | `pdf`      | `txt`| Delegates to [`PdfEngine::extract_text`] (best-effort |
//! |            |      | lopdf extraction; complex PDFs may yield partial     |
//! |            |      | text).                                              |
//! | `pdf`      | `md` | Delegates to [`PdfEngine::convert`] with the         |
//! |            |      | `markdown` target (`## Page n` sections). Same       |
//! |            |      | text-layer-only caveats as `pdf` → `txt`.            |
//! | `png`/`jpg`| `pdf`| Single-page PDF with the image centered and scaled   |
//! |            |      | to fit the page margins. PNG is decoded via the     |
//! |            |      | `image` crate and embedded as raw RGB (FlateDecode  |
//! |            |      | via `lopdf` compression); JPEG bytes are embedded   |
//! |            |      | verbatim with `/DCTDecode`. Alpha channels are      |
//! |            |      | dropped with a warning.                             |
//!
//! Deliberately NOT supported:
//! - `html` → `pdf` and everything else.
//!
//! JPEG embedding trusts the file's own DCT stream: 1-component
//! (grayscale) and 3-component (YCbCr/RGB) images are supported;
//! other component counts are rejected as invalid input so an
//! external backend can take over.
//!
//! Text is encoded as PDF literal strings in WinAnsiEncoding
//! (Windows-1252). Characters outside that repertoire are replaced
//! with `?` and reported in [`ConvertOutput::warnings`].

use lopdf::content::{Content, Operation};
use lopdf::{Document, Object, Stream, StringFormat, dictionary};

use crate::crates::conversion_engine::backend::{BackendAvailability, ConversionBackend};
use crate::crates::conversion_engine::error::{ConvertError, ConvertResult};
use crate::crates::conversion_engine::model::{ConvertOutput, ConvertRequest, Format};
use crate::crates::pdf_engine::PdfEngine;

/// Page margins in points (1 inch).
const MARGIN: f32 = 72.0;
/// Line spacing multiplier applied to each line's font size.
const LINE_SPACING: f32 = 1.2;
/// Body text size for txt/markdown → PDF.
const BODY_SIZE: f32 = 12.0;
/// Courier size inside fenced code blocks.
const CODE_SIZE: f32 = 10.0;
/// Maximum wrap width used for markdown lines, assuming A4. The real
/// page width may be Letter (slightly wider), so this is conservative.
const MD_TEXT_WIDTH: f32 = 595.0 - 2.0 * MARGIN;
const MD_LIST_WIDTH: f32 = MD_TEXT_WIDTH - 18.0;
const MD_CODE_WIDTH: f32 = MD_TEXT_WIDTH - 12.0;
const MD_LIST_INDENT: f32 = 18.0;
const MD_CODE_INDENT: f32 = 12.0;
const MD_HEADING_GAP: f32 = 10.0;
/// Markdown heading sizes by level (H1..H6), Helvetica-Bold.
const HEADING_SIZES: [f32; 6] = [20.0, 17.0, 15.0, 14.0, 13.0, 12.0];

/// Base-14 fonts used by the PDF writer.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum PdfFont {
    /// `/Helvetica` — body text.
    Helvetica,
    /// `/Helvetica-Bold` — markdown headings.
    HelveticaBold,
    /// `/Courier` — fenced code blocks.
    Courier,
}

impl PdfFont {
    /// Resource name in the page `/Resources` font dictionary.
    fn resource_name(self) -> &'static str {
        match self {
            Self::Helvetica => "F1",
            Self::HelveticaBold => "F2",
            Self::Courier => "F3",
        }
    }

    /// `/BaseFont` name for the font dictionary.
    fn base_font_name(self) -> &'static str {
        match self {
            Self::Helvetica => "Helvetica",
            Self::HelveticaBold => "Helvetica-Bold",
            Self::Courier => "Courier",
        }
    }
}

/// One laid-out line of text on a PDF page.
#[derive(Debug, Clone)]
struct RenderLine {
    /// Source text (Unicode); encoded to WinAnsi at emit time.
    text: String,
    font: PdfFont,
    /// Font size in points.
    size: f32,
    /// Extra left indent in points (markdown lists/code).
    indent: f32,
    /// Extra vertical space before this line, in points.
    gap_before: f32,
}

/// Pure-Rust conversion backend. Holds no state; always available.
#[derive(Debug, Default, Clone, Copy)]
pub struct RustBackend;

impl RustBackend {
    /// Create a new backend instance.
    pub fn new() -> Self {
        Self
    }
}

impl ConversionBackend for RustBackend {
    fn name(&self) -> &'static str {
        "rust"
    }

    fn availability(&self) -> BackendAvailability {
        // Pure in-process library — nothing to probe.
        BackendAvailability::Available { version: None }
    }

    fn supports(&self, from: Format, to: Format) -> bool {
        matches!(
            (from, to),
            (Format::Txt, Format::Pdf)
                | (Format::Markdown, Format::Pdf)
                | (Format::Pdf, Format::Txt)
                | (Format::Pdf, Format::Markdown)
                | (Format::Png, Format::Pdf)
                | (Format::Jpeg, Format::Pdf)
        )
    }

    fn convert(&self, req: &ConvertRequest) -> ConvertResult<ConvertOutput> {
        if req.input_format == req.output_format {
            return Err(ConvertError::InvalidInput {
                reason: "input and output formats are identical".to_string(),
            });
        }
        if !self.supports(req.input_format, req.output_format) {
            return Err(ConvertError::UnsupportedConversion {
                from: req.input_format,
                to: req.output_format,
            });
        }
        if req.input.is_empty() {
            return Err(ConvertError::InvalidInput {
                reason: "input is empty".to_string(),
            });
        }

        let mut warnings = option_warnings(req);

        match (req.input_format, req.output_format) {
            (Format::Pdf, Format::Txt) => {
                let bytes = self.pdf_to_txt(&req.input, &mut warnings)?;
                Ok(ConvertOutput {
                    bytes,
                    output_format: Format::Txt,
                    backend: self.name().to_string(),
                    warnings,
                })
            }
            (Format::Pdf, Format::Markdown) => {
                let bytes = self.pdf_to_markdown(&req.input, &mut warnings)?;
                Ok(ConvertOutput {
                    bytes,
                    output_format: Format::Markdown,
                    backend: self.name().to_string(),
                    warnings,
                })
            }
            (Format::Png | Format::Jpeg, Format::Pdf) => {
                let bytes = self.image_to_pdf(req, &mut warnings)?;
                Ok(ConvertOutput {
                    bytes,
                    output_format: Format::Pdf,
                    backend: self.name().to_string(),
                    warnings,
                })
            }
            (Format::Txt | Format::Markdown, Format::Pdf) => {
                let bytes = self.text_to_pdf(req, &mut warnings)?;
                Ok(ConvertOutput {
                    bytes,
                    output_format: Format::Pdf,
                    backend: self.name().to_string(),
                    warnings,
                })
            }
            _ => Err(ConvertError::UnsupportedConversion {
                from: req.input_format,
                to: req.output_format,
            }),
        }
    }
}

impl RustBackend {
    /// `pdf` → `txt` via the shared [`PdfEngine`] text extractor.
    fn pdf_to_txt(&self, input: &[u8], warnings: &mut Vec<String>) -> ConvertResult<Vec<u8>> {
        let engine = PdfEngine::new();
        let pages = engine
            .extract_text(input, None)
            .map_err(|e| ConvertError::BackendFailed {
                backend: self.name().to_string(),
                message: format!("PDF text extraction failed: {e}"),
            })?;

        // Each page's text already ends with a newline (every line is
        // emitted as its own text object); joining with "\n" puts a
        // blank line between pages.
        let text = pages
            .iter()
            .map(|p| p.text.as_str())
            .collect::<Vec<_>>()
            .join("\n");
        if text.trim().is_empty() {
            warnings.push(
                "no extractable text found (document may be scanned/images only)".to_string(),
            );
        }
        Ok(text.into_bytes())
    }

    /// `pdf` → `markdown` via the shared [`PdfEngine`] converter.
    /// Output is `## Page n` sections of extracted text — structure
    /// beyond page boundaries is not reconstructed.
    fn pdf_to_markdown(&self, input: &[u8], warnings: &mut Vec<String>) -> ConvertResult<Vec<u8>> {
        let bytes = PdfEngine::new()
            .convert(
                input,
                crate::crates::pdf_engine::ConvertTarget::Markdown,
                None,
            )
            .map_err(|e| ConvertError::BackendFailed {
                backend: self.name().to_string(),
                message: format!("PDF markdown conversion failed: {e}"),
            })?;
        if String::from_utf8_lossy(&bytes).trim().is_empty() {
            warnings.push(
                "no extractable text found (document may be scanned/images only)".to_string(),
            );
        }
        Ok(bytes)
    }

    /// `png`/`jpeg` → `pdf`: one page, image scaled to fit the
    /// margins preserving aspect ratio.
    fn image_to_pdf(
        &self,
        req: &ConvertRequest,
        warnings: &mut Vec<String>,
    ) -> ConvertResult<Vec<u8>> {
        let image = decode_image(req.input_format, &req.input, warnings)?;
        let (page_w, page_h) = page_dimensions(req, warnings);

        // Natural size assumes 96 DPI (0.75 pt per pixel); the image is
        // then scaled — up or down — to fit inside the margins.
        let nat_w = image.width() as f32 * 0.75;
        let nat_h = image.height() as f32 * 0.75;
        let avail_w = page_w - 2.0 * MARGIN;
        let avail_h = page_h - 2.0 * MARGIN;
        if nat_w <= 0.0 || nat_h <= 0.0 || avail_w <= 0.0 || avail_h <= 0.0 {
            return Err(ConvertError::BackendFailed {
                backend: self.name().to_string(),
                message: "image dimensions are degenerate".to_string(),
            });
        }
        let scale = (avail_w / nat_w).min(avail_h / nat_h);
        let draw_w = nat_w * scale;
        let draw_h = nat_h * scale;
        // PDF user space origin is bottom-left; center the image.
        let x = MARGIN + (avail_w - draw_w) / 2.0;
        let y = MARGIN + (avail_h - draw_h) / 2.0;

        let mut doc = Document::with_version("1.5");
        let pages_id = doc.new_object_id();
        let image_id = doc.add_object(image.xobject_stream());
        let resources_id = doc.add_object(dictionary! {
            "XObject" => dictionary! {
                "Im1" => image_id,
            },
        });

        let content = Content {
            operations: vec![
                Operation::new("q", vec![]),
                Operation::new(
                    "cm",
                    vec![
                        draw_w.into(),
                        0.into(),
                        0.into(),
                        draw_h.into(),
                        x.into(),
                        y.into(),
                    ],
                ),
                Operation::new("Do", vec![Object::Name(b"Im1".to_vec())]),
                Operation::new("Q", vec![]),
            ],
        };
        let encoded = content.encode().map_err(|e| ConvertError::BackendFailed {
            backend: self.name().to_string(),
            message: format!("failed to encode page content stream: {e}"),
        })?;
        let content_id = doc.add_object(Stream::new(dictionary! {}, encoded));
        let media_box = vec![
            0.into(),
            0.into(),
            (page_w as i64).into(),
            (page_h as i64).into(),
        ];
        let page_id = doc.add_object(dictionary! {
            "Type" => "Page",
            "Parent" => pages_id,
            "Contents" => content_id,
            "Resources" => resources_id,
            "MediaBox" => media_box.clone(),
        });
        doc.objects.insert(
            pages_id,
            Object::Dictionary(dictionary! {
                "Type" => "Pages",
                "Kids" => vec![Object::Reference(page_id)],
                "Count" => 1i64,
                "MediaBox" => media_box,
            }),
        );
        let catalog_id = doc.add_object(dictionary! {
            "Type" => "Catalog",
            "Pages" => pages_id,
        });
        let info_id = doc.add_object(dictionary! {
            "Title" => Object::string_literal(format!(
                "{} image",
                req.input_format.as_str()
            )),
            "Producer" => Object::string_literal("KalaDoc conversion-engine (rust)"),
            "Creator" => Object::string_literal("KalaDoc"),
        });
        doc.trailer.set("Root", catalog_id);
        doc.trailer.set("Info", info_id);

        // Flate-compress the unfiltered raw-RGB stream (JPEG streams
        // already carry /DCTDecode and are skipped by lopdf).
        doc.compress();

        let mut buf = Vec::new();
        doc.save_to(&mut buf)
            .map_err(|e| ConvertError::BackendFailed {
                backend: "rust".to_string(),
                message: format!("failed to serialize PDF: {e}"),
            })?;
        Ok(buf)
    }

    /// `txt`/`markdown` → `pdf`.
    fn text_to_pdf(
        &self,
        req: &ConvertRequest,
        warnings: &mut Vec<String>,
    ) -> ConvertResult<Vec<u8>> {
        let source = String::from_utf8_lossy(&req.input);
        if source.contains('\u{FFFD}') {
            warnings.push(
                "input was not valid UTF-8; invalid byte sequences were replaced".to_string(),
            );
        }

        let (page_w, page_h) = page_dimensions(req, warnings);
        let text_width = page_w - 2.0 * MARGIN;
        let lines = match req.input_format {
            Format::Markdown => markdown_render_lines(&source),
            _ => plain_render_lines(&source, text_width),
        };
        build_pdf(&lines, page_w, page_h, &source, warnings)
    }
}

/// Warn about options this backend cannot honor for the request.
fn option_warnings(req: &ConvertRequest) -> Vec<String> {
    let mut warnings = Vec::new();

    // dpi/quality only matter for raster outputs; this backend never
    // produces raster output.
    if req.options.dpi.is_some() {
        warnings.push(format!(
            "dpi option is not applicable to {} output and was ignored",
            req.output_format.as_str()
        ));
    }
    if req.options.quality.is_some() {
        warnings.push(format!(
            "quality option is not applicable to {} output and was ignored",
            req.output_format.as_str()
        ));
    }
    if req.output_format != Format::Pdf && req.options.page_size.is_some() {
        warnings.push(format!(
            "page_size option is not applicable to {} output and was ignored",
            req.output_format.as_str()
        ));
    }
    warnings
}

/// Resolve `(width, height)` in points from `options.page_size`.
/// Unknown values warn and fall back to A4.
fn page_dimensions(req: &ConvertRequest, warnings: &mut Vec<String>) -> (f32, f32) {
    match req.options.page_size.as_deref() {
        None => (595.0, 842.0), // A4 default
        Some(raw) => match raw.trim().to_ascii_lowercase().as_str() {
            "a4" => (595.0, 842.0),
            "letter" => (612.0, 792.0),
            other => {
                warnings.push(format!(
                    "unrecognized page_size \"{other}\"; defaulting to A4"
                ));
                (595.0, 842.0)
            }
        },
    }
}

/// Plain-text → render lines: every source line is word-wrapped to the
/// text width. Blank source lines are preserved.
fn plain_render_lines(source: &str, text_width: f32) -> Vec<RenderLine> {
    let mut out = Vec::new();
    for raw in source.lines() {
        if raw.trim().is_empty() {
            out.push(RenderLine {
                text: String::new(),
                font: PdfFont::Helvetica,
                size: BODY_SIZE,
                indent: 0.0,
                gap_before: 0.0,
            });
            continue;
        }
        for piece in wrap_text(raw, PdfFont::Helvetica, BODY_SIZE, text_width) {
            out.push(RenderLine {
                text: piece,
                font: PdfFont::Helvetica,
                size: BODY_SIZE,
                indent: 0.0,
                gap_before: 0.0,
            });
        }
    }
    if out.is_empty() {
        // Empty document still produces one (blank) page.
        out.push(RenderLine {
            text: String::new(),
            font: PdfFont::Helvetica,
            size: BODY_SIZE,
            indent: 0.0,
            gap_before: 0.0,
        });
    }
    out
}

/// Minimal markdown → render lines.
///
/// Recognized: ATX headings (`#`–`######`), fenced code blocks
/// (` ``` `), list markers (`-`, `*`, `+`) which indent. Everything
/// else — inline markup, links, images, tables, HTML — is rendered as
/// literal text. This is intentionally not a Markdown parser.
fn markdown_render_lines(source: &str) -> Vec<RenderLine> {
    let mut out = Vec::new();
    let mut in_code = false;

    for raw in source.lines() {
        let trimmed = raw.trim_end();

        if trimmed.trim_start().starts_with("```") {
            in_code = !in_code;
            continue;
        }

        if in_code {
            let body = trimmed.trim_start();
            if body.is_empty() {
                out.push(RenderLine {
                    text: String::new(),
                    font: PdfFont::Courier,
                    size: CODE_SIZE,
                    indent: MD_CODE_INDENT,
                    gap_before: 0.0,
                });
            } else {
                for piece in wrap_text(body, PdfFont::Courier, CODE_SIZE, MD_CODE_WIDTH) {
                    out.push(RenderLine {
                        text: piece,
                        font: PdfFont::Courier,
                        size: CODE_SIZE,
                        indent: MD_CODE_INDENT,
                        gap_before: 0.0,
                    });
                }
            }
            continue;
        }

        if trimmed.trim().is_empty() {
            out.push(RenderLine {
                text: String::new(),
                font: PdfFont::Helvetica,
                size: BODY_SIZE,
                indent: 0.0,
                gap_before: 0.0,
            });
            continue;
        }

        // ATX heading: 1–6 '#' followed by whitespace or EOL.
        let hashes = trimmed.chars().take_while(|&c| c == '#').count();
        if (1..=6).contains(&hashes) {
            let after = &trimmed[hashes..];
            if after.is_empty() || after.starts_with(char::is_whitespace) {
                let text = after.trim().to_string();
                if !text.is_empty() {
                    out.push(RenderLine {
                        text,
                        font: PdfFont::HelveticaBold,
                        size: HEADING_SIZES[hashes - 1],
                        indent: 0.0,
                        gap_before: MD_HEADING_GAP,
                    });
                }
                continue;
            }
        }

        // List marker: "- ", "* ", "+ " followed by whitespace. The
        // marker stays as literal text; the line is indented.
        let t = trimmed.trim_start();
        let mut chars = t.chars();
        let bullet = match (chars.next(), chars.next()) {
            (Some(m @ ('-' | '*' | '+')), Some(c)) if c.is_whitespace() => {
                Some(format!("{m} {}", t[1..].trim_start()))
            }
            _ => None,
        };
        if let Some(first) = bullet {
            let mut pieces = wrap_text(&first, PdfFont::Helvetica, BODY_SIZE, MD_LIST_WIDTH);
            // Continuation lines of a wrapped bullet get a small extra
            // indent to visually hang under the marker.
            for p in pieces.iter_mut().skip(1) {
                p.insert_str(0, "  ");
            }
            for piece in pieces {
                out.push(RenderLine {
                    text: piece,
                    font: PdfFont::Helvetica,
                    size: BODY_SIZE,
                    indent: MD_LIST_INDENT,
                    gap_before: 0.0,
                });
            }
            continue;
        }

        // Plain paragraph line (inline markup stays literal).
        for piece in wrap_text(trimmed, PdfFont::Helvetica, BODY_SIZE, MD_TEXT_WIDTH) {
            out.push(RenderLine {
                text: piece,
                font: PdfFont::Helvetica,
                size: BODY_SIZE,
                indent: 0.0,
                gap_before: 0.0,
            });
        }
    }

    if out.is_empty() {
        out.push(RenderLine {
            text: String::new(),
            font: PdfFont::Helvetica,
            size: BODY_SIZE,
            indent: 0.0,
            gap_before: 0.0,
        });
    }
    out
}

/// Greedy word wrap. `text` is a single source line (no newlines).
/// Multiple consecutive spaces are preserved.
fn wrap_text(text: &str, font: PdfFont, size: f32, max_width: f32) -> Vec<String> {
    if max_width <= 0.0 || text.trim().is_empty() {
        return vec![text.to_string()];
    }

    let space_w = char_width(font, ' ') * size / 1000.0;
    let mut out: Vec<String> = Vec::new();
    let mut cur = String::new();
    let mut cur_w = 0.0f32;

    for word in text.split(' ') {
        let ww = text_width(font, word, size);
        if ww > max_width {
            // Word longer than the line: flush current, hard-split.
            if !cur.is_empty() {
                out.push(std::mem::take(&mut cur));
                cur_w = 0.0;
            }
            out.extend(hard_split(word, font, size, max_width));
            continue;
        }
        let extra = if cur.is_empty() { ww } else { ww + space_w };
        if !cur.is_empty() && cur_w + extra > max_width {
            out.push(std::mem::take(&mut cur));
            cur = word.to_string();
            cur_w = ww;
        } else {
            if !cur.is_empty() {
                cur.push(' ');
                cur_w += space_w;
            }
            cur.push_str(word);
            cur_w += ww;
        }
    }
    if !cur.is_empty() {
        out.push(cur);
    }
    if out.is_empty() {
        out.push(String::new());
    }
    out
}

/// Split an over-long word at character boundaries.
fn hard_split(word: &str, font: PdfFont, size: f32, max_width: f32) -> Vec<String> {
    let mut out = Vec::new();
    let mut cur = String::new();
    let mut w = 0.0f32;
    for ch in word.chars() {
        let cw = char_width(font, ch) * size / 1000.0;
        if w + cw > max_width && !cur.is_empty() {
            out.push(std::mem::take(&mut cur));
            w = 0.0;
        }
        cur.push(ch);
        w += cw;
    }
    if !cur.is_empty() {
        out.push(cur);
    }
    if out.is_empty() {
        out.push(String::new());
    }
    out
}

/// Approximate advance width of `text` in points.
fn text_width(font: PdfFont, text: &str, size: f32) -> f32 {
    text.chars().map(|c| char_width(font, c)).sum::<f32>() * size / 1000.0
}

/// Glyph advance width in 1/1000 em units.
///
/// Helvetica uses the standard AFM widths for ASCII 32–126; characters
/// outside that range (WinAnsi punctuation, accented Latin) fall back
/// to a typical mid-width. Courier is exactly 600/1000 for everything.
fn char_width(font: PdfFont, c: char) -> f32 {
    match font {
        PdfFont::Courier => 600.0,
        PdfFont::Helvetica | PdfFont::HelveticaBold => {
            // AFM Helvetica widths for ASCII 32..=126 (95 entries).
            const WIDTHS: [f32; 95] = [
                278.0, 278.0, 355.0, 556.0, 556.0, 889.0, 667.0, 191.0, // 32-39
                333.0, 333.0, 389.0, 584.0, 278.0, 333.0, 278.0, 278.0, // 40-47
                556.0, 556.0, 556.0, 556.0, 556.0, 556.0, 556.0, 556.0, // 48-55
                556.0, 556.0, 278.0, 278.0, 584.0, 584.0, 584.0, 556.0, // 56-63
                1015.0, 667.0, 667.0, 722.0, 722.0, 667.0, 611.0, 778.0, // 64-71
                722.0, 278.0, 500.0, 667.0, 556.0, 833.0, 722.0, 778.0, // 72-79
                667.0, 778.0, 722.0, 667.0, 611.0, 722.0, 667.0, 944.0, // 80-87
                667.0, 667.0, 611.0, 278.0, 278.0, 278.0, 469.0, 556.0, // 88-95
                333.0, 556.0, 556.0, 500.0, 556.0, 556.0, 278.0, 556.0, // 96-103
                556.0, 222.0, 222.0, 500.0, 222.0, 833.0, 556.0, 556.0, // 104-111
                556.0, 556.0, 333.0, 500.0, 278.0, 556.0, 500.0, 722.0, // 112-119
                500.0, 500.0, 500.0, 334.0, 260.0, 334.0, 584.0, // 120-126
            ];
            let code = c as u32;
            if (32..=126).contains(&code) {
                WIDTHS[(code - 32) as usize]
            } else {
                // WinAnsi punctuation/accents: mid-width approximation.
                556.0
            }
        }
    }
}

/// Encode `text` as Windows-1252 bytes for a PDF literal string.
/// Returns `(bytes, had_replacement)`.
fn winansi_bytes(text: &str) -> (Vec<u8>, bool) {
    let mut out = Vec::with_capacity(text.len());
    let mut replaced = false;
    for c in text.chars() {
        match winansi_byte(c) {
            Some(byte) => out.push(byte),
            None => {
                replaced = true;
                out.push(b'?');
            }
        }
    }
    (out, replaced)
}

/// Map a char to its Windows-1252 byte, or `None` if unrepresentable.
fn winansi_byte(c: char) -> Option<u8> {
    let cp = c as u32;
    match cp {
        // ASCII printable; control chars and DEL are unrepresentable.
        0x20..=0x7E => Some(cp as u8),
        0xA0..=0xFF => Some(cp as u8), // cp1252 == Latin-1 here
        // Windows-1252 punctuation/letters in the 0x80–0x9F range.
        0x20AC => Some(0x80), // €
        0x201A => Some(0x82), // ‚
        0x0192 => Some(0x83), // ƒ
        0x201E => Some(0x84), // „
        0x2026 => Some(0x85), // …
        0x2020 => Some(0x86), // †
        0x2021 => Some(0x87), // ‡
        0x02C6 => Some(0x88), // ˆ
        0x2030 => Some(0x89), // ‰
        0x0160 => Some(0x8A), // Š
        0x2039 => Some(0x8B), // ‹
        0x0152 => Some(0x8C), // Œ
        0x017D => Some(0x8E), // Ž
        0x2018 => Some(0x91), // '
        0x2019 => Some(0x92), // '
        0x201C => Some(0x93), // "
        0x201D => Some(0x94), // "
        0x2022 => Some(0x95), // •
        0x2013 => Some(0x96), // –
        0x2014 => Some(0x97), // —
        0x02DC => Some(0x98), // ˜
        0x2122 => Some(0x99), // ™
        0x0161 => Some(0x9A), // š
        0x203A => Some(0x9B), // ›
        0x0153 => Some(0x9C), // œ
        0x017E => Some(0x9E), // ž
        0x0178 => Some(0x9F), // Ÿ
        _ => None,
    }
}

/// Build the PDF document bytes from render lines.
fn build_pdf(
    lines: &[RenderLine],
    page_w: f32,
    page_h: f32,
    source: &str,
    warnings: &mut Vec<String>,
) -> ConvertResult<Vec<u8>> {
    let mut doc = Document::with_version("1.5");

    let pages_id = doc.new_object_id();
    // Font dicts borrow `doc` mutably — build them before the
    // resources dictionary borrows it again.
    let f1 = font_dict(&mut doc, PdfFont::Helvetica);
    let f2 = font_dict(&mut doc, PdfFont::HelveticaBold);
    let f3 = font_dict(&mut doc, PdfFont::Courier);
    let resources_id = doc.add_object(dictionary! {
        "Font" => dictionary! {
            "F1" => f1,
            "F2" => f2,
            "F3" => f3,
        },
    });

    // Lay out lines into pages of content streams. Each line is its own
    // BT/ET block positioned with Tm; the trailing ET makes lopdf's
    // text extractor emit a newline per line, which keeps
    // pdf→txt round-trips faithful.
    let mut page_ops: Vec<Vec<Operation>> = Vec::new();
    let mut current: Vec<Operation> = Vec::new();
    let mut cursor = 0.0f32; // points consumed below the top margin
    let mut replaced_any = false;

    let usable_h = page_h - 2.0 * MARGIN;
    for line in lines {
        let leading = line.size * LINE_SPACING + line.gap_before;
        if cursor + leading > usable_h && !current.is_empty() {
            page_ops.push(std::mem::take(&mut current));
            cursor = 0.0;
        }
        cursor += line.gap_before;
        // Baseline: cursor is distance below the top margin; cap height
        // ~0.7em, so baseline ≈ top + 0.8em.
        let baseline = page_h - MARGIN - cursor - line.size * 0.8;
        let x = MARGIN + line.indent;
        let (bytes, replaced) = winansi_bytes(&line.text);
        replaced_any |= replaced;

        current.push(Operation::new("BT", vec![]));
        current.push(Operation::new(
            "Tf",
            vec![
                Object::Name(line.font.resource_name().as_bytes().to_vec()),
                line.size.into(),
            ],
        ));
        current.push(Operation::new(
            "Tm",
            vec![
                1.into(),
                0.into(),
                0.into(),
                1.into(),
                x.into(),
                baseline.into(),
            ],
        ));
        current.push(Operation::new(
            "Tj",
            vec![Object::String(bytes, StringFormat::Literal)],
        ));
        current.push(Operation::new("ET", vec![]));

        cursor += line.size * LINE_SPACING;
    }
    page_ops.push(current);

    if replaced_any {
        warnings.push(
            "some characters are not representable in PDF WinAnsi encoding and were replaced with '?'"
                .to_string(),
        );
    }

    let media_box = vec![
        0.into(),
        0.into(),
        (page_w as i64).into(),
        (page_h as i64).into(),
    ];

    let mut kids: Vec<Object> = Vec::with_capacity(page_ops.len());
    for ops in page_ops {
        let content = Content { operations: ops };
        let encoded = content.encode().map_err(|e| ConvertError::BackendFailed {
            backend: "rust".to_string(),
            message: format!("failed to encode page content stream: {e}"),
        })?;
        let content_id = doc.add_object(Stream::new(dictionary! {}, encoded));
        let page_id = doc.add_object(dictionary! {
            "Type" => "Page",
            "Parent" => pages_id,
            "Contents" => content_id,
            "Resources" => resources_id,
            "MediaBox" => media_box.clone(),
        });
        kids.push(Object::Reference(page_id));
    }

    let page_count = kids.len() as i64;
    doc.objects.insert(
        pages_id,
        Object::Dictionary(dictionary! {
            "Type" => "Pages",
            "Kids" => kids,
            "Count" => page_count,
            "Resources" => resources_id,
            "MediaBox" => media_box,
        }),
    );

    let catalog_id = doc.add_object(dictionary! {
        "Type" => "Catalog",
        "Pages" => pages_id,
    });

    let info_id = doc.add_object(dictionary! {
        "Title" => Object::string_literal(pdf_title(source)),
        "Producer" => Object::string_literal("KalaDoc conversion-engine (rust)"),
        "Creator" => Object::string_literal("KalaDoc"),
    });
    doc.trailer.set("Root", catalog_id);
    doc.trailer.set("Info", info_id);

    let mut buf = Vec::new();
    doc.save_to(&mut buf)
        .map_err(|e| ConvertError::BackendFailed {
            backend: "rust".to_string(),
            message: format!("failed to serialize PDF: {e}"),
        })?;
    Ok(buf)
}

/// Add a base-14 font dictionary to the document, returning a
/// reference object for embedding in resource dictionaries.
fn font_dict(doc: &mut Document, font: PdfFont) -> Object {
    Object::Reference(doc.add_object(dictionary! {
        "Type" => "Font",
        "Subtype" => "Type1",
        "BaseFont" => font.base_font_name(),
        "Encoding" => "WinAnsiEncoding",
    }))
}

/// Derive a document title: first markdown heading text, else first
/// non-empty line, truncated to 120 chars.
fn pdf_title(source: &str) -> String {
    for line in source.lines() {
        let t = line.trim();
        if t.is_empty() {
            continue;
        }
        let stripped = t.trim_start_matches('#').trim();
        let candidate = if stripped.is_empty() { t } else { stripped };
        let mut title: String = candidate.chars().take(120).collect();
        if candidate.chars().nth(120).is_some() {
            title.push('…');
        }
        return title;
    }
    "Untitled".to_string()
}

/// PNG file signature (8 bytes).
const PNG_SIGNATURE: [u8; 8] = [0x89, 0x50, 0x4E, 0x47, 0x0D, 0x0A, 0x1A, 0x0A];

/// A raster image decoded into a form the PDF writer can embed.
enum ImagePixels {
    /// 8-bit RGB samples, 3 bytes per pixel, top-down rows. Embedded
    /// unfiltered; `lopdf`'s `compress()` applies FlateDecode at save.
    Rgb8 {
        width: u32,
        height: u32,
        data: Vec<u8>,
    },
    /// JPEG file bytes embedded verbatim under `/DCTDecode`.
    Jpeg {
        width: u32,
        height: u32,
        data: Vec<u8>,
        /// PDF `/ColorSpace` name for the component count.
        color_space: &'static str,
    },
}

impl ImagePixels {
    fn width(&self) -> u32 {
        match self {
            Self::Rgb8 { width, .. } | Self::Jpeg { width, .. } => *width,
        }
    }

    fn height(&self) -> u32 {
        match self {
            Self::Rgb8 { height, .. } | Self::Jpeg { height, .. } => *height,
        }
    }

    /// Build the PDF image XObject stream for this image.
    fn xobject_stream(&self) -> Stream {
        let (width, height, color_space, data, filter) = match self {
            Self::Rgb8 {
                width,
                height,
                data,
            } => (*width, *height, "DeviceRGB", data.clone(), None),
            Self::Jpeg {
                width,
                height,
                data,
                color_space,
            } => (
                *width,
                *height,
                *color_space,
                data.clone(),
                Some("DCTDecode"),
            ),
        };
        let mut dict = dictionary! {
            "Type" => "XObject",
            "Subtype" => "Image",
            "Width" => width as i64,
            "Height" => height as i64,
            "ColorSpace" => color_space,
            "BitsPerComponent" => 8i64,
        };
        if let Some(f) = filter {
            dict.set("Filter", Object::Name(f.as_bytes().to_vec()));
        }
        Stream::new(dict, data)
    }
}

/// Decode `input` according to `input_format` into [`ImagePixels`].
///
/// Validation failures return [`ConvertError::InvalidInput`] so the
/// engine's fallback chain can try an external backend; decode
/// failures of structurally-valid images are `BackendFailed`.
fn decode_image(
    input_format: Format,
    input: &[u8],
    warnings: &mut Vec<String>,
) -> ConvertResult<ImagePixels> {
    match input_format {
        Format::Png => {
            if !input.starts_with(&PNG_SIGNATURE) {
                return Err(ConvertError::InvalidInput {
                    reason: "input does not carry a PNG signature".to_string(),
                });
            }
            let img = image::load_from_memory(input).map_err(|e| ConvertError::InvalidInput {
                reason: format!("could not decode PNG: {e}"),
            })?;
            if img.color().has_alpha() {
                warnings.push("PNG alpha channel is not preserved in PDF output".to_string());
            }
            let rgb = img.to_rgb8();
            let (width, height) = rgb.dimensions();
            Ok(ImagePixels::Rgb8 {
                width,
                height,
                data: rgb.into_raw(),
            })
        }
        Format::Jpeg => {
            let (width, height, components) =
                jpeg_info(input).ok_or_else(|| ConvertError::InvalidInput {
                    reason: "could not parse JPEG header (missing SOF marker)".to_string(),
                })?;
            let color_space = match components {
                1 => "DeviceGray",
                3 => "DeviceRGB",
                other => {
                    return Err(ConvertError::InvalidInput {
                        reason: format!(
                            "unsupported JPEG component count {other} (only grayscale and RGB/YCC are embeddable)"
                        ),
                    });
                }
            };
            Ok(ImagePixels::Jpeg {
                width,
                height,
                data: input.to_vec(),
                color_space,
            })
        }
        _ => Err(ConvertError::UnsupportedConversion {
            from: input_format,
            to: Format::Pdf,
        }),
    }
}

/// Parse a JPEG header for `(width, height, component_count)` by
/// scanning markers for a Start-Of-Frame segment. Returns `None` for
/// non-JPEG or truncated input — no pixel data is decoded.
fn jpeg_info(bytes: &[u8]) -> Option<(u32, u32, u8)> {
    // SOI marker.
    if bytes.len() < 4 || bytes[0] != 0xFF || bytes[1] != 0xD8 {
        return None;
    }
    let mut pos = 2usize;
    while pos + 1 < bytes.len() {
        if bytes[pos] != 0xFF {
            pos += 1;
            continue;
        }
        // Skip fill bytes (0xFF padding is legal before a marker).
        let mut m = pos + 1;
        while m < bytes.len() && bytes[m] == 0xFF {
            m += 1;
        }
        if m >= bytes.len() {
            return None;
        }
        let marker = bytes[m];
        let seg = m + 1; // first byte after the marker code
        match marker {
            // Standalone markers carry no length field.
            0x01 | 0xD0..=0xD9 => pos = seg,
            // SOF markers, excluding DHT (C4), JPG (C8) and DAC (CC).
            0xC0..=0xCF if !matches!(marker, 0xC4 | 0xC8 | 0xCC) => {
                if seg + 8 > bytes.len() {
                    return None;
                }
                let height = u16::from_be_bytes([bytes[seg + 3], bytes[seg + 4]]) as u32;
                let width = u16::from_be_bytes([bytes[seg + 5], bytes[seg + 6]]) as u32;
                let components = bytes[seg + 7];
                return Some((width, height, components));
            }
            _ => {
                if seg + 2 > bytes.len() {
                    return None;
                }
                let len = u16::from_be_bytes([bytes[seg], bytes[seg + 1]]) as usize;
                if len < 2 {
                    return None;
                }
                pos = seg + len;
            }
        }
    }
    None
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::crates::conversion_engine::model::ConvertOptions;

    fn req(from: Format, to: Format, input: &[u8]) -> ConvertRequest {
        ConvertRequest {
            input_format: from,
            output_format: to,
            input: input.to_vec(),
            options: ConvertOptions::default(),
        }
    }

    fn req_with_options(
        from: Format,
        to: Format,
        input: &[u8],
        options: ConvertOptions,
    ) -> ConvertRequest {
        ConvertRequest {
            input_format: from,
            output_format: to,
            input: input.to_vec(),
            options,
        }
    }

    /// Extract all text-showing font sizes (the operand before `Tf`)
    /// from every content stream in `doc`.
    fn content_stream_text(doc: &Document) -> String {
        let mut out = String::new();
        for obj in doc.objects.values() {
            if let Object::Stream(stream) = obj {
                if let Ok(data) = stream.decompressed_content() {
                    out.push_str(&String::from_utf8_lossy(&data));
                    out.push('\n');
                }
            }
        }
        out
    }

    #[test]
    fn name_and_availability() {
        let b = RustBackend::new();
        assert_eq!(b.name(), "rust");
        assert_eq!(
            b.availability(),
            BackendAvailability::Available { version: None }
        );
    }

    #[test]
    fn supports_exact_pairs() {
        let b = RustBackend::new();
        assert!(b.supports(Format::Txt, Format::Pdf));
        assert!(b.supports(Format::Markdown, Format::Pdf));
        assert!(b.supports(Format::Pdf, Format::Txt));
        assert!(b.supports(Format::Pdf, Format::Markdown));
        assert!(b.supports(Format::Png, Format::Pdf));
        assert!(b.supports(Format::Jpeg, Format::Pdf));
        // Everything else is false.
        assert!(!b.supports(Format::Png, Format::Png));
        assert!(!b.supports(Format::Jpeg, Format::Markdown));
        assert!(!b.supports(Format::Html, Format::Pdf));
        assert!(!b.supports(Format::Txt, Format::Markdown));
        assert!(!b.supports(Format::Docx, Format::Pdf));
        assert!(!b.supports(Format::Pdf, Format::Pdf));
        assert!(!b.supports(Format::Xlsx, Format::Pdf));
    }

    #[test]
    fn txt_to_pdf_produces_valid_pdf() {
        let b = RustBackend::new();
        let out = b
            .convert(&req(
                Format::Txt,
                Format::Pdf,
                b"Hello world\nSecond line of text",
            ))
            .unwrap();
        assert_eq!(out.output_format, Format::Pdf);
        assert_eq!(out.backend, "rust");
        assert!(out.bytes.starts_with(b"%PDF"));
        // Must parse.
        Document::load_mem(&out.bytes).unwrap();
    }

    #[test]
    fn roundtrip_txt_pdf_txt() {
        let b = RustBackend::new();
        let input = b"The quick brown fox\njumps over the lazy dog\nPack my box with five dozen liquor jugs";
        let pdf = b.convert(&req(Format::Txt, Format::Pdf, input)).unwrap();
        let txt = b
            .convert(&ConvertRequest {
                input_format: Format::Pdf,
                output_format: Format::Txt,
                input: pdf.bytes.clone(),
                options: ConvertOptions::default(),
            })
            .unwrap();
        let text = String::from_utf8(txt.bytes).unwrap();
        assert!(text.contains("The quick brown fox"), "got: {text:?}");
        assert!(text.contains("jumps over the lazy dog"));
        assert!(text.contains("Pack my box"));
    }

    #[test]
    fn markdown_headings_use_larger_font() {
        let b = RustBackend::new();
        let out = b
            .convert(&req(
                Format::Markdown,
                Format::Pdf,
                b"# Big Title\n\nbody text here\n\n## Sub heading\n\n- item one\n- item two\n\n```\ncode line\n```\n",
            ))
            .unwrap();
        let doc = Document::load_mem(&out.bytes).unwrap();
        let content = content_stream_text(&doc);
        // Tf is emitted as "/F1 20 Tf" style; collect sizes.
        let mut sizes: Vec<f32> = Vec::new();
        for tok in content.split(" Tf") {
            // token ends with "/Fx <size>"
            let tail = tok.split_whitespace().next_back().unwrap_or("");
            if let Ok(v) = tail.parse::<f32>() {
                sizes.push(v);
            }
        }
        assert!(sizes.contains(&20.0), "H1 size missing: {sizes:?}");
        assert!(sizes.contains(&17.0), "H2 size missing: {sizes:?}");
        assert!(sizes.contains(&12.0), "body size missing: {sizes:?}");
        assert!(sizes.contains(&10.0), "code size missing: {sizes:?}");
    }

    #[test]
    fn page_size_letter_honored() {
        let b = RustBackend::new();
        let mut options = ConvertOptions::default();
        options.page_size = Some("LETTER".to_string());
        let out = b
            .convert(&req_with_options(
                Format::Txt,
                Format::Pdf,
                b"some text",
                options,
            ))
            .unwrap();
        let doc = Document::load_mem(&out.bytes).unwrap();
        let mut found_letter = false;
        for obj in doc.objects.values() {
            if let Object::Dictionary(d) = obj {
                if d.get(b"Type").and_then(|o| o.as_name()).ok() == Some(b"Page".as_slice()) {
                    if let Ok(box_arr) = d.get(b"MediaBox").and_then(|o| o.as_array()) {
                        let w = box_arr
                            .first()
                            .and_then(|_| box_arr.get(2))
                            .and_then(|o| o.as_i64().ok())
                            .unwrap_or(0);
                        if w == 612 {
                            found_letter = true;
                        }
                    }
                }
            }
        }
        assert!(found_letter, "letter MediaBox not found");
    }

    #[test]
    fn unknown_page_size_warns_and_defaults_a4() {
        let b = RustBackend::new();
        let mut options = ConvertOptions::default();
        options.page_size = Some("tabloid".to_string());
        let out = b
            .convert(&req_with_options(
                Format::Txt,
                Format::Pdf,
                b"text",
                options,
            ))
            .unwrap();
        assert!(
            out.warnings
                .iter()
                .any(|w| w.contains("page_size") || w.contains("tabloid"))
        );
        let doc = Document::load_mem(&out.bytes).unwrap();
        let mut found_a4 = false;
        for obj in doc.objects.values() {
            if let Object::Dictionary(d) = obj {
                if d.get(b"Type").and_then(|o| o.as_name()).ok() == Some(b"Page".as_slice()) {
                    if let Ok(box_arr) = d.get(b"MediaBox").and_then(|o| o.as_array()) {
                        if box_arr.get(2).and_then(|o| o.as_i64().ok()).unwrap_or(0) == 595 {
                            found_a4 = true;
                        }
                    }
                }
            }
        }
        assert!(found_a4);
    }

    #[test]
    fn dpi_and_quality_warn_when_set() {
        let b = RustBackend::new();
        let mut options = ConvertOptions::default();
        options.dpi = Some(300);
        options.quality = Some(90);
        let out = b
            .convert(&req_with_options(
                Format::Txt,
                Format::Pdf,
                b"text",
                options,
            ))
            .unwrap();
        assert!(out.warnings.iter().any(|w| w.contains("dpi")));
        assert!(out.warnings.iter().any(|w| w.contains("quality")));
    }

    #[test]
    fn same_format_rejected() {
        let b = RustBackend::new();
        let err = b.convert(&req(Format::Txt, Format::Txt, b"x")).unwrap_err();
        assert!(matches!(err, ConvertError::InvalidInput { .. }));
    }

    #[test]
    fn unsupported_pair_rejected() {
        let b = RustBackend::new();
        let err = b
            .convert(&req(Format::Html, Format::Pdf, b"<p>x</p>"))
            .unwrap_err();
        assert!(matches!(err, ConvertError::UnsupportedConversion { .. }));
    }

    #[test]
    fn garbage_pdf_input_errors_not_panics() {
        let b = RustBackend::new();
        let err = b
            .convert(&req(
                Format::Pdf,
                Format::Txt,
                b"not a pdf at all \x00\x01\x02",
            ))
            .unwrap_err();
        assert!(matches!(err, ConvertError::BackendFailed { .. }));
    }

    #[test]
    fn empty_input_rejected() {
        let b = RustBackend::new();
        let err = b.convert(&req(Format::Txt, Format::Pdf, b"")).unwrap_err();
        assert!(matches!(err, ConvertError::InvalidInput { .. }));
    }

    #[test]
    fn long_text_paginates() {
        let b = RustBackend::new();
        let mut input = String::new();
        for i in 0..500 {
            input.push_str(&format!("line number {i} with some text to wrap around\n"));
        }
        let out = b
            .convert(&req(Format::Txt, Format::Pdf, input.as_bytes()))
            .unwrap();
        let doc = Document::load_mem(&out.bytes).unwrap();
        assert!(doc.get_pages().len() > 1, "expected multiple pages");
    }

    #[test]
    fn non_winansi_chars_replaced_with_warning() {
        let b = RustBackend::new();
        // U+00E9 and U+2014 are WinAnsi-representable; U+4F60 is not.
        let out = b
            .convert(&req(
                Format::Txt,
                Format::Pdf,
                "café — \u{4F60}\u{597D} test".as_bytes(),
            ))
            .unwrap();
        assert!(
            out.warnings
                .iter()
                .any(|w| w.contains("WinAnsi") || w.contains("replaced"))
        );
    }

    #[test]
    fn wrap_text_breaks_long_lines() {
        let long = "word ".repeat(200);
        let pieces = wrap_text(long.trim_end(), PdfFont::Helvetica, 12.0, 200.0);
        assert!(pieces.len() > 1);
        for p in &pieces {
            assert!(text_width(PdfFont::Helvetica, p, 12.0) <= 200.0 + 60.0);
        }
    }

    /// Minimal valid JPEG: SOI + SOF0 (8x4, 3 components) + EOI.
    /// No scan data — `jpeg_info` only reads headers.
    fn fake_jpeg(components: u8) -> Vec<u8> {
        let mut v = vec![0xFF, 0xD8]; // SOI
        // SOF0 segment: length 8+3*Nf... use length covering header only.
        v.extend_from_slice(&[0xFF, 0xC0, 0x00, 0x11, 0x08]); // marker, len=17, precision 8
        v.extend_from_slice(&[0x00, 0x04, 0x00, 0x08]); // height=4, width=8
        v.push(components);
        for _ in 0..components {
            v.extend_from_slice(&[0x01, 0x11, 0x00]); // id, sampling, qtable
        }
        v.extend_from_slice(&[0xFF, 0xD9]); // EOI
        v
    }

    /// Tiny valid PNG (2x2, RGBA red) encoded via the `image` crate.
    fn tiny_png() -> Vec<u8> {
        let img = image::RgbaImage::from_pixel(2, 2, image::Rgba([255, 0, 0, 255]));
        let mut buf = Vec::new();
        image::DynamicImage::ImageRgba8(img)
            .write_to(&mut std::io::Cursor::new(&mut buf), image::ImageFormat::Png)
            .unwrap();
        buf
    }

    /// First image XObject stream in `doc`, if any.
    fn first_image_stream(doc: &Document) -> Option<&lopdf::Stream> {
        doc.objects.values().find_map(|obj| match obj {
            Object::Stream(s)
                if s.dict.get(b"Subtype").and_then(|o| o.as_name()).ok()
                    == Some(b"Image".as_slice()) =>
            {
                Some(s)
            }
            _ => None,
        })
    }

    /// Dictionary integer lookup helper for assertions.
    fn dict_int(dict: &lopdf::Dictionary, key: &[u8]) -> Option<i64> {
        dict.get(key).ok().and_then(|o| o.as_i64().ok())
    }

    /// Dictionary name lookup helper for assertions.
    fn dict_name<'a>(dict: &'a lopdf::Dictionary, key: &[u8]) -> Option<&'a [u8]> {
        dict.get(key).ok().and_then(|o| o.as_name().ok())
    }

    #[test]
    fn png_to_pdf_produces_valid_pdf_with_image_xobject() {
        let b = RustBackend::new();
        let png = tiny_png();
        let out = b.convert(&req(Format::Png, Format::Pdf, &png)).unwrap();
        assert_eq!(out.output_format, Format::Pdf);
        assert!(out.bytes.starts_with(b"%PDF"));
        assert!(
            out.warnings.iter().any(|w| w.contains("alpha")),
            "expected alpha warning: {:?}",
            out.warnings
        );
        let doc = Document::load_mem(&out.bytes).unwrap();
        let img = first_image_stream(&doc).expect("no image XObject in generated PDF");
        assert_eq!(dict_int(&img.dict, b"Width"), Some(2));
        assert_eq!(dict_int(&img.dict, b"Height"), Some(2));
        // The content stream should paint the image.
        assert!(content_stream_text(&doc).contains("/Im1"));
    }

    #[test]
    fn jpeg_to_pdf_embeds_dctdecode_stream() {
        let b = RustBackend::new();
        let jpeg = fake_jpeg(3);
        let out = b.convert(&req(Format::Jpeg, Format::Pdf, &jpeg)).unwrap();
        assert!(out.bytes.starts_with(b"%PDF"));
        let doc = Document::load_mem(&out.bytes).unwrap();
        let img = first_image_stream(&doc).expect("no image XObject in generated PDF");
        assert_eq!(
            dict_name(&img.dict, b"Filter"),
            Some(b"DCTDecode".as_slice())
        );
        assert_eq!(
            dict_name(&img.dict, b"ColorSpace"),
            Some(b"DeviceRGB".as_slice())
        );
        assert_eq!(dict_int(&img.dict, b"Width"), Some(8));
    }

    #[test]
    fn jpeg_grayscale_maps_to_device_gray() {
        let b = RustBackend::new();
        let out = b
            .convert(&req(Format::Jpeg, Format::Pdf, &fake_jpeg(1)))
            .unwrap();
        let doc = Document::load_mem(&out.bytes).unwrap();
        let img = first_image_stream(&doc).expect("no image XObject in generated PDF");
        assert_eq!(
            dict_name(&img.dict, b"ColorSpace"),
            Some(b"DeviceGray".as_slice())
        );
    }

    #[test]
    fn jpeg_cmyk_rejected_as_invalid_input() {
        let b = RustBackend::new();
        let err = b
            .convert(&req(Format::Jpeg, Format::Pdf, &fake_jpeg(4)))
            .unwrap_err();
        assert!(matches!(err, ConvertError::InvalidInput { .. }));
    }

    #[test]
    fn corrupt_png_rejected_as_invalid_input() {
        let b = RustBackend::new();
        // Valid signature but garbage body.
        let mut bad = PNG_SIGNATURE.to_vec();
        bad.extend_from_slice(b"not really a png");
        let err = b.convert(&req(Format::Png, Format::Pdf, &bad)).unwrap_err();
        assert!(matches!(err, ConvertError::InvalidInput { .. }));
        // Missing signature entirely.
        let err = b
            .convert(&req(Format::Png, Format::Pdf, b"plain text"))
            .unwrap_err();
        assert!(matches!(err, ConvertError::InvalidInput { .. }));
    }

    #[test]
    fn jpeg_info_parses_sof() {
        assert_eq!(jpeg_info(&fake_jpeg(3)), Some((8, 4, 3)));
        assert_eq!(jpeg_info(&fake_jpeg(1)), Some((8, 4, 1)));
        assert_eq!(jpeg_info(b"not a jpeg"), None);
        assert_eq!(jpeg_info(&[0xFF, 0xD8]), None);
    }

    #[test]
    fn pdf_to_markdown_produces_page_sections() {
        let b = RustBackend::new();
        let pdf = b
            .convert(&req(Format::Txt, Format::Pdf, b"hello markdown world"))
            .unwrap();
        let out = b
            .convert(&ConvertRequest {
                input_format: Format::Pdf,
                output_format: Format::Markdown,
                input: pdf.bytes.clone(),
                options: ConvertOptions::default(),
            })
            .unwrap();
        let text = String::from_utf8(out.bytes).unwrap();
        assert!(text.contains("## Page 1"), "got: {text:?}");
        assert!(text.contains("hello markdown world"));
    }

    #[test]
    fn markdown_code_block_uses_courier() {
        let b = RustBackend::new();
        let out = b
            .convert(&req(
                Format::Markdown,
                Format::Pdf,
                b"```\nfn main() {}\n```\n",
            ))
            .unwrap();
        let doc = Document::load_mem(&out.bytes).unwrap();
        let content = content_stream_text(&doc);
        assert!(content.contains("/F3"), "Courier font not used: {content}");
    }
}
