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
//! | `html`     | `pdf`| Best-effort text extraction (no HTML parser dep):    |
//! |            |      | headings, paragraphs, lists and `<pre>` blocks keep  |
//! |            |      | structure; CSS, scripts, and media are dropped with  |
//! |            |      | warnings. Entities are decoded.                      |
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
//! | `eml`      | `pdf`| RFC 5322/MIME message parsed by [`eml`](crate::crates::conversion_engine::eml) — |
//! |            |      | From/To/Cc/Subject/Date header block plus the best  |
//! |            |      | text body (`text/plain` preferred, `text/html`      |
//! |            |      | tag-stripped as fallback) rendered as plain text.   |
//! |            |      | Attachments are listed, not embedded (warning).     |
//! | `cbz`      | `pdf`| Comic Book ZIP: entries are read via the           |
//! |            |      | in-crate [`archive`](crate::crates::conversion_engine::archive) reader (stored + |
//! |            |      | deflate only), image members (png/jpg/jpeg/webp/ |
//! |            |      | gif/bmp) are naturally sorted and each becomes   |
//! |            |      | one PDF page sized to the image's pixel        |
//! |            |      | dimensions at 96 DPI equivalent (0.75 pt/px),   |
//! |            |      | edge to edge — comic-reader style, no margins. |
//!
//! Deliberately NOT supported:
//! - `cbr` → `pdf`: RAR decoding needs an external tool (`unrar`,
//!   `unrar-free`, or `7z`); none is wired into this crate, so the
//!   pair reports [`ConvertError::UnsupportedConversion`].
//! - everything else.
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
pub(crate) const MARGIN: f32 = 72.0;
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

/// Maximum image dimension (width or height) accepted for PDF
/// embedding — a decompression-bomb guard enforced both by the
/// `image` crate's strict limits and by [`check_image_size`].
const MAX_IMAGE_DIM: u32 = 32768;
/// Maximum total pixels (width × height) per embedded image.
const MAX_IMAGE_PIXELS: u64 = 33_554_432; // ~32 MP
/// Decoder allocation budget handed to `image::Limits` (its default).
const MAX_IMAGE_ALLOC: u64 = 512 * 1024 * 1024;
/// Maximum members scanned in a CBZ central directory.
const CBZ_MAX_ENTRIES: usize = 8192;
/// Maximum pages emitted from one CBZ.
const CBZ_MAX_PAGES: usize = 1000;
/// Maximum sum of declared uncompressed sizes across selected CBZ
/// image entries — bounds inflate work before any bytes are read.
const CBZ_MAX_UNCOMPRESSED: u64 = 512 * 1024 * 1024;

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
///
/// `pub(crate)` so sibling backends (EPUB) can reuse the HTML/markdown
/// renderers and [`build_pdf`]; fields stay private — lines are only
/// produced inside this module.
#[derive(Debug, Clone)]
pub(crate) struct RenderLine {
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
                | (Format::Eml, Format::Pdf)
                | (Format::Html, Format::Pdf)
                | (Format::Cbz, Format::Pdf)
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
            (Format::Eml, Format::Pdf) => {
                let bytes = self.eml_to_pdf(req, &mut warnings)?;
                Ok(ConvertOutput {
                    bytes,
                    output_format: Format::Pdf,
                    backend: self.name().to_string(),
                    warnings,
                })
            }
            (Format::Cbz, Format::Pdf) => {
                let bytes = self.cbz_to_pdf(req, &mut warnings)?;
                Ok(ConvertOutput {
                    bytes,
                    output_format: Format::Pdf,
                    backend: self.name().to_string(),
                    warnings,
                })
            }
            (Format::Html, Format::Pdf) => {
                let bytes = self.html_to_pdf(req, &mut warnings)?;
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
        let image_id = doc.add_object(image.into_xobject_stream());
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
        build_pdf(
            &lines,
            page_w,
            page_h,
            &source,
            warnings,
            &std::collections::BTreeSet::new(),
        )
    }

    /// `html` → `pdf`: best-effort text extraction rendered through
    /// the same PDF writer as txt/markdown. CSS, scripts and embedded
    /// media are dropped — every drop is reported in `warnings`.
    fn html_to_pdf(
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

        let doc = parse_html(&source);
        warnings.push(
            "HTML→PDF fallback: CSS styling, scripts, and embedded media are not rendered; \
             only text structure (headings, paragraphs, lists, preformatted text) is preserved"
                .to_string(),
        );
        if doc.saw_media {
            warnings.push("image/media elements were dropped".to_string());
        }
        if doc.saw_script_style {
            warnings.push("script/style elements were dropped".to_string());
        }

        let (page_w, page_h) = page_dimensions(req, warnings);
        let lines = html_render_lines(&doc.blocks, page_w - 2.0 * MARGIN);

        // PDF /Title: <title> element first, else the first text block.
        let title_source = if !doc.title.trim().is_empty() {
            doc.title.clone()
        } else {
            doc.blocks
                .iter()
                .map(|b| collapse_ws(&b.text))
                .find(|t| !t.is_empty())
                .unwrap_or_default()
        };
        build_pdf(
            &lines,
            page_w,
            page_h,
            &title_source,
            warnings,
            &std::collections::BTreeSet::new(),
        )
    }

    /// `eml` → `pdf`: parse the RFC 5322/MIME message, render the
    /// header block + extracted text body through the same plain-text
    /// path as `txt` → `pdf`. Attachments cannot be embedded and are
    /// surfaced in warnings — never silently dropped.
    fn eml_to_pdf(
        &self,
        req: &ConvertRequest,
        warnings: &mut Vec<String>,
    ) -> ConvertResult<Vec<u8>> {
        let message = crate::crates::conversion_engine::eml::parse(&req.input)
            .map_err(|reason| ConvertError::InvalidInput { reason })?;

        for note in &message.notes {
            warnings.push(note.clone());
        }
        for attachment in &message.attachments {
            warnings.push(format!(
                "attachment not embedded in PDF output: {attachment}"
            ));
        }

        let source = crate::crates::conversion_engine::eml::render_text(&message);
        let (page_w, page_h) = page_dimensions(req, warnings);
        let text_width = page_w - 2.0 * MARGIN;
        let lines = plain_render_lines(&source, text_width);
        build_pdf(
            &lines,
            page_w,
            page_h,
            &source,
            warnings,
            &std::collections::BTreeSet::new(),
        )
    }

    /// `cbz` → `pdf`: unzip the archive in memory, take every image
    /// member (png/jpg/jpeg/webp/gif/bmp) in natural filename order,
    /// and emit one PDF page per image.
    ///
    /// Page sizing is image-native (comic-reader style): each page's
    /// MediaBox equals the image's pixel dimensions at 96 DPI
    /// equivalent (`0.75 pt` per pixel) and the image fills the page
    /// edge to edge — `options.page_size` does not apply and is
    /// reported in warnings. Pages are decoded and embedded one at a
    /// time so peak memory stays near one page image, not the whole
    /// book.
    fn cbz_to_pdf(
        &self,
        req: &ConvertRequest,
        warnings: &mut Vec<String>,
    ) -> ConvertResult<Vec<u8>> {
        use crate::crates::conversion_engine::archive::ZipArchive;

        let archive = ZipArchive::open(&req.input)?;
        let metas = archive.entries();
        if metas.len() > CBZ_MAX_ENTRIES {
            return Err(ConvertError::TooLarge {
                limit: format!("{CBZ_MAX_ENTRIES} archive entries"),
                actual: metas.len() as u64,
            });
        }

        // Select readable image members, enforcing the decompression
        // budget on *declared* sizes before any bytes are inflated.
        let mut selected: Vec<usize> = Vec::new();
        let mut declared_total: u64 = 0;
        let mut skipped_non_image = 0usize;
        let mut skipped_compression = 0usize;
        for (i, meta) in metas.iter().enumerate() {
            if meta.is_dir {
                continue;
            }
            if archive_image_ext(&meta.name).is_none() {
                skipped_non_image += 1;
                continue;
            }
            if meta.flags & 0x0001 != 0 {
                return Err(ConvertError::InvalidInput {
                    reason: format!(
                        "archive contains an encrypted image entry \"{}\"",
                        meta.name
                    ),
                });
            }
            if meta.method != 0 && meta.method != 8 {
                // Stored and deflate are the only methods the archive
                // reader implements; anything else cannot become a page.
                skipped_compression += 1;
                continue;
            }
            declared_total = declared_total.saturating_add(meta.uncompressed_size);
            if declared_total > CBZ_MAX_UNCOMPRESSED {
                return Err(ConvertError::TooLarge {
                    limit: format!(
                        "{} MiB of uncompressed image data",
                        CBZ_MAX_UNCOMPRESSED / (1024 * 1024)
                    ),
                    actual: declared_total,
                });
            }
            selected.push(i);
        }
        if skipped_non_image > 0 {
            warnings.push(format!(
                "skipped {skipped_non_image} non-image archive entries"
            ));
        }
        if skipped_compression > 0 {
            warnings.push(format!(
                "skipped {skipped_compression} image entries using unsupported \
                 compression (only stored/deflate supported)"
            ));
        }
        if selected.is_empty() {
            return Err(ConvertError::InvalidInput {
                reason: "archive contains no supported page images (png/jpg/jpeg/webp/gif/bmp)"
                    .to_string(),
            });
        }
        if selected.len() > CBZ_MAX_PAGES {
            return Err(ConvertError::TooLarge {
                limit: format!("{CBZ_MAX_PAGES} pages"),
                actual: selected.len() as u64,
            });
        }
        if req.options.page_size.is_some() {
            warnings.push(
                "page_size option is ignored for cbz→pdf; pages use image-native \
                 dimensions"
                    .to_string(),
            );
        }

        // Natural sort: page1, page2, ... page10 (not lexicographic).
        selected.sort_by(|&a, &b| natural_cmp(&metas[a].name, &metas[b].name));

        let mut doc = Document::with_version("1.5");
        let pages_id = doc.new_object_id();
        let mut kids: Vec<Object> = Vec::with_capacity(selected.len());

        for idx in selected {
            let meta = &metas[idx];
            let raw = archive.read_entry(idx)?;
            let image = decode_archive_image(&meta.name, &raw, warnings)?;
            drop(raw);

            // Image-native page size: 96 DPI equivalent, edge to edge.
            let page_w = image.width() as f32 * 0.75;
            let page_h = image.height() as f32 * 0.75;
            if page_w <= 0.0 || page_h <= 0.0 || !page_w.is_finite() || !page_h.is_finite() {
                return Err(ConvertError::BackendFailed {
                    backend: self.name().to_string(),
                    message: format!("image \"{}\" has degenerate dimensions", meta.name),
                });
            }

            let image_id = doc.add_object(image.into_xobject_stream());
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
                            page_w.into(),
                            0.into(),
                            0.into(),
                            page_h.into(),
                            0.into(),
                            0.into(),
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
            let media_box = vec![0.into(), 0.into(), page_w.into(), page_h.into()];
            let page_id = doc.add_object(dictionary! {
                "Type" => "Page",
                "Parent" => pages_id,
                "Contents" => content_id,
                "Resources" => resources_id,
                "MediaBox" => media_box,
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
            }),
        );
        let catalog_id = doc.add_object(dictionary! {
            "Type" => "Catalog",
            "Pages" => pages_id,
        });
        let info_id = doc.add_object(dictionary! {
            "Title" => Object::string_literal("Comic Book Archive"),
            "Producer" => Object::string_literal("KalaDoc conversion-engine (rust)"),
            "Creator" => Object::string_literal("KalaDoc"),
        });
        doc.trailer.set("Root", catalog_id);
        doc.trailer.set("Info", info_id);

        // Flate-compress raw-RGB image streams (JPEG streams already
        // carry /DCTDecode and are skipped by lopdf).
        doc.compress();

        let mut buf = Vec::new();
        doc.save_to(&mut buf)
            .map_err(|e| ConvertError::BackendFailed {
                backend: "rust".to_string(),
                message: format!("failed to serialize PDF: {e}"),
            })?;
        Ok(buf)
    }
}

/// Warn about options this backend cannot honor for the request.
pub(crate) fn option_warnings(req: &ConvertRequest) -> Vec<String> {
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
pub(crate) fn page_dimensions(req: &ConvertRequest, warnings: &mut Vec<String>) -> (f32, f32) {
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
pub(crate) fn markdown_render_lines(source: &str) -> Vec<RenderLine> {
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

// ---------------------------------------------------------------------------
// html → pdf: best-effort text extraction feeding the shared PDF writer.
//
// There is no HTML parser dependency in this crate, so this is a small
// hand-rolled tokenizer: it strips scripts/styles/comments/doctype,
// captures `<title>`, then walks tags once, classifying text runs into
// paragraph / heading / list-item / preformatted blocks. Inline markup
// is flattened; entities are decoded; whitespace is collapsed except
// inside `<pre>`.
// ---------------------------------------------------------------------------

/// Block kind assigned to a run of text by the HTML extractor.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum HtmlBlockKind {
    Paragraph,
    /// Heading level 1–6 (index into [`HEADING_SIZES`] is level − 1).
    Heading(usize),
    /// `<li>` content; rendered with a bullet and hanging indent.
    ListItem,
    /// `<pre>` content; newlines preserved, Courier font.
    Preformatted,
}

/// One extracted block of text content.
#[derive(Debug)]
pub(crate) struct HtmlBlock {
    pub(crate) kind: HtmlBlockKind,
    /// Entity-decoded text. `Preformatted` blocks may contain `\n`.
    pub(crate) text: String,
}

/// Result of [`parse_html`].
pub(crate) struct HtmlDocument {
    /// Decoded text of the first `<title>` element (may be empty).
    pub(crate) title: String,
    pub(crate) blocks: Vec<HtmlBlock>,
    /// `<img>`/`<svg>`/`<video>`/... were present and dropped.
    pub(crate) saw_media: bool,
    /// `<script>`/`<style>` were present and dropped.
    pub(crate) saw_script_style: bool,
}

/// Tags whose open/close boundaries flush the current text run and
/// (on open) start a plain paragraph. Everything not listed here and
/// not specially handled is treated as inline and flattened away.
const HTML_BLOCK_TAGS: &[&str] = &[
    "p",
    "div",
    "section",
    "article",
    "aside",
    "header",
    "footer",
    "main",
    "nav",
    "blockquote",
    "figure",
    "figcaption",
    "table",
    "ul",
    "ol",
    "dl",
    "form",
    "fieldset",
    "body",
    "html",
    "center",
    "address",
    "details",
    "summary",
    "dialog",
    "dd",
    "dt",
];

/// Media/embed tags that carry no extractable text.
const HTML_MEDIA_TAGS: &[&str] = &[
    "img",
    "svg",
    "video",
    "audio",
    "iframe",
    "canvas",
    "object",
    "embed",
    "picture",
    "source",
    "track",
    "map",
    "area",
    "portal",
    "fencedframe",
];

/// Find `needle` in `hay` at or after `pos`, ASCII case-insensitive.
fn find_ci(hay: &str, needle: &str, mut pos: usize) -> Option<usize> {
    let h = hay.as_bytes();
    let n = needle.as_bytes();
    if n.is_empty() || h.len() < n.len() {
        return None;
    }
    while pos + n.len() <= h.len() {
        if h[pos..pos + n.len()].eq_ignore_ascii_case(n) {
            return Some(pos);
        }
        pos += 1;
    }
    None
}

/// Whether `s[pos..]` starts with `<name` followed by a tag-name
/// boundary (whitespace, `/`, `>` or end of input). `name` is matched
/// ASCII case-insensitively.
fn tag_open_at(s: &str, pos: usize, name: &str) -> bool {
    let bytes = s.as_bytes();
    let needle = format!("<{name}");
    if pos + needle.len() > bytes.len() {
        return false;
    }
    if !bytes[pos..pos + needle.len()].eq_ignore_ascii_case(needle.as_bytes()) {
        return false;
    }
    match bytes.get(pos + needle.len()) {
        None => true,
        Some(&b) => b.is_ascii_whitespace() || b == b'/' || b == b'>',
    }
}

/// Whether `s[pos..]` starts with `</name` (ASCII case-insensitive)
/// followed by a tag-name boundary.
fn close_tag_at(s: &str, pos: usize, name: &str) -> bool {
    let bytes = s.as_bytes();
    let mut needle = String::with_capacity(name.len() + 2);
    needle.push_str("</");
    needle.push_str(name);
    if pos + needle.len() > bytes.len() {
        return false;
    }
    if !bytes[pos..pos + needle.len()].eq_ignore_ascii_case(needle.as_bytes()) {
        return false;
    }
    match bytes.get(pos + needle.len()) {
        None => true,
        Some(&b) => b.is_ascii_whitespace() || b == b'/' || b == b'>',
    }
}

/// Find the next `<name` or `</name` tag at or after `pos`, skipping
/// occurrences that are actually a longer tag name (`<names>` must not
/// match `name`). Returns `(pos_of_<, is_closing)`.
fn find_tag(s: &str, name: &str, pos: usize) -> Option<(usize, bool)> {
    let open_needle = format!("<{name}");
    let close_needle = format!("</{name}");
    loop {
        let open = find_ci(s, &open_needle, pos).filter(|&i| tag_open_at(s, i, name));
        let close = find_ci(s, &close_needle, pos).filter(|&i| close_tag_at(s, i, name));
        match (open, close) {
            (Some(o), Some(c)) => {
                return Some(if o <= c { (o, false) } else { (c, true) });
            }
            (Some(o), None) => return Some((o, false)),
            (None, Some(c)) => return Some((c, true)),
            (None, None) => return None,
        }
    }
}

/// Decode the text content of the first `<name>...</name>` element
/// (case-insensitive), entities decoded and whitespace collapsed.
/// Used for `<title>`.
fn element_text(s: &str, name: &str) -> Option<String> {
    let (lt, closing) = find_tag(s, name, 0)?;
    if closing {
        return None;
    }
    let gt = tag_end(s, lt)?;
    let inner_start = gt + 1;
    // First close tag ends the element (nested same-name elements are
    // rare in `<title>` and handled acceptably by early termination).
    let (clt, is_close) = find_tag(s, name, inner_start)?;
    if !is_close {
        return None;
    }
    Some(collapse_ws(&decode_entities(&s[inner_start..clt])))
}

/// Index of the `>` terminating the tag that starts at `lt` (the `<`),
/// honoring single/double quotes inside attributes. `None` if the tag
/// is unterminated.
fn tag_end(s: &str, lt: usize) -> Option<usize> {
    let bytes = s.as_bytes();
    let mut quote: Option<u8> = None;
    let mut i = lt + 1;
    while i < bytes.len() {
        let b = bytes[i];
        match quote {
            Some(q) => {
                if b == q {
                    quote = None;
                }
            }
            None => match b {
                b'"' | b'\'' => quote = Some(b),
                b'>' => return Some(i),
                _ => {}
            },
        }
        i += 1;
    }
    None
}

/// Remove every `<name ..>...</name>` element, including self-closing
/// occurrences. Returns the number of elements removed.
fn remove_tagged(s: &mut String, name: &str) -> usize {
    let mut removed = 0;
    let open_needle = format!("<{name}");
    loop {
        // Find the next opening tag of this name.
        let mut pos = 0usize;
        let mut open_at = None;
        while let Some(i) = find_ci(s, &open_needle, pos) {
            if tag_open_at(s, i, name) {
                open_at = Some(i);
                break;
            }
            pos = i + 1;
        }
        let Some(lt) = open_at else { break };
        let Some(gt) = tag_end(s, lt) else { break };
        // Self-closing (`<name .../>`): drop just the tag.
        let self_closing = s[..gt].trim_end().ends_with('/');
        let end = if self_closing {
            gt + 1
        } else {
            match find_tag(s, name, gt + 1) {
                // First close tag ends the element.
                Some((clt, true)) => tag_end(s, clt).map(|g| g + 1).unwrap_or(s.len()),
                // Unterminated element (open or close): the rest of the
                // document is element content and goes with it.
                _ => s.len(),
            }
        };
        s.replace_range(lt..end.min(s.len()), "");
        removed += 1;
    }
    removed
}

/// Strip HTML comments (`<!-- -->`), doctypes/`<!...>` declarations and
/// processing instructions (`<?...?>`) from `s`.
fn strip_comments_and_decls(s: &mut String) {
    loop {
        // Locate the earliest of the three constructs. `<!` also
        // prefixes `<!--`, so comments are checked first and `<!`
        // matches are required to not be comment openers.
        let comment = s.find("<!--");
        let mut decl = None;
        let mut p = 0usize;
        while let Some(i) = s[p..].find("<!").map(|j| p + j) {
            if !s[i..].starts_with("<!--") {
                decl = Some(i);
                break;
            }
            p = i + 2;
        }
        let pi = s.find("<?");
        let (start, kind) = match (comment, decl, pi) {
            (Some(c), d, p) if d.map_or(true, |d| c <= d) && p.map_or(true, |p| c <= p) => {
                (c, 0usize)
            }
            (c, Some(d), p) if c.map_or(true, |c| d <= c) && p.map_or(true, |p| d <= p) => {
                (d, 1usize)
            }
            (_, _, Some(p)) => (p, 2usize),
            _ => break,
        };
        let end = match kind {
            // Comment: through `-->` (or EOF if unterminated).
            0 => s[start + 4..]
                .find("-->")
                .map(|i| start + 4 + i + 3)
                .unwrap_or(s.len()),
            // Processing instruction: prefer `?>`, else the tag's `>`.
            2 => s[start + 2..]
                .find("?>")
                .map(|i| start + 2 + i + 2)
                .or_else(|| tag_end(s, start).map(|g| g + 1))
                .unwrap_or(s.len()),
            // Declaration/doctype: through the tag's `>`.
            _ => tag_end(s, start).map(|g| g + 1).unwrap_or(s.len()),
        };
        s.replace_range(start..end.min(s.len()), "");
    }
}

/// Remove the `<head>...</head>` element (already mined for `<title>`).
/// Falls back to cutting at `<body>` or removing just the open tag
/// when unterminated.
fn remove_head(s: &mut String) {
    // Locate "<head" with a boundary.
    let mut pos = 0usize;
    let head_at = loop {
        let Some(i) = find_ci(s, "<head", pos) else {
            break None;
        };
        if tag_open_at(s, i, "head") {
            break Some(i);
        }
        pos = i + 1;
    };
    let Some(lt) = head_at else { return };
    let Some(gt) = tag_end(s, lt) else { return };
    let end = match find_tag(s, "head", gt + 1) {
        Some((clt, true)) => tag_end(s, clt).map(|g| g + 1).unwrap_or(s.len()),
        _ => {
            // No close tag: cut to <body if present, else just the tag.
            match find_ci(s, "<body", gt + 1) {
                Some(b) if tag_open_at(s, b, "body") => b,
                _ => gt + 1,
            }
        }
    };
    s.replace_range(lt..end.min(s.len()), "");
}

/// Decode HTML entities in `s`: the common named set plus decimal and
/// hex numeric character references. Unknown entities are left as-is.
pub(crate) fn decode_entities(s: &str) -> String {
    if !s.contains('&') {
        return s.to_string();
    }
    let mut out = String::with_capacity(s.len());
    let mut rest = s;
    while let Some(amp) = rest.find('&') {
        out.push_str(&rest[..amp]);
        let after = &rest[amp + 1..];
        // Entity names are short; cap the scan at 32 chars.
        let scan = &after[..after.len().min(32)];
        let semi = scan.find(';');
        let decoded = semi.and_then(|i| {
            let ent = &scan[..i];
            if ent.is_empty() || ent.len() > 10 {
                return None;
            }
            if let Some(num) = ent.strip_prefix('#') {
                let cp = if let Some(hex) = num.strip_prefix('x').or_else(|| num.strip_prefix('X'))
                {
                    u32::from_str_radix(hex, 16).ok()?
                } else {
                    num.parse::<u32>().ok()?
                };
                return char::from_u32(cp);
            }
            named_entity(ent)
        });
        match (decoded, semi) {
            (Some(ch), Some(i)) => {
                out.push(ch);
                rest = &after[i + 1..];
            }
            _ => {
                // Not an entity: emit '&' literally and continue.
                out.push('&');
                rest = after;
            }
        }
    }
    out.push_str(rest);
    out
}

/// Common named HTML entities → char. Intentionally small; numeric
/// references cover the long tail.
fn named_entity(name: &str) -> Option<char> {
    // Case-insensitive lookup on a static table.
    const TABLE: &[(&str, char)] = &[
        ("amp", '&'),
        ("lt", '<'),
        ("gt", '>'),
        ("quot", '"'),
        ("apos", '\''),
        ("nbsp", ' '),
        ("copy", '©'),
        ("reg", '®'),
        ("trade", '™'),
        ("hellip", '…'),
        ("mdash", '—'),
        ("ndash", '–'),
        ("lsquo", '‘'),
        ("rsquo", '’'),
        ("ldquo", '“'),
        ("rdquo", '”'),
        ("laquo", '«'),
        ("raquo", '»'),
        ("bull", '•'),
        ("middot", '·'),
        ("times", '×'),
        ("divide", '÷'),
        ("plusmn", '±'),
        ("deg", '°'),
        ("micro", 'µ'),
        ("para", '¶'),
        ("sect", '§'),
        ("euro", '€'),
        ("pound", '£'),
        ("yen", '¥'),
        ("cent", '¢'),
        ("curren", '¤'),
        ("brvbar", '¦'),
        ("uml", '¨'),
        ("ordf", 'ª'),
        ("ordm", 'º'),
        ("iexcl", '¡'),
        ("iquest", '¿'),
        ("szlig", 'ß'),
        ("dagger", '†'),
        ("Dagger", '‡'),
        ("permil", '‰'),
        ("prime", '′'),
        ("Prime", '″'),
        ("lsaquo", '‹'),
        ("rsaquo", '›'),
        ("oline", '‾'),
        ("frasl", '⁄'),
        ("weierp", '℘'),
        ("image", 'ℑ'),
        ("real", 'ℜ'),
        ("alefsym", 'ℵ'),
        ("larr", '←'),
        ("uarr", '↑'),
        ("rarr", '→'),
        ("darr", '↓'),
        ("harr", '↔'),
        ("crarr", '↵'),
        ("lArr", '⇐'),
        ("uArr", '⇑'),
        ("rArr", '⇒'),
        ("dArr", '⇓'),
        ("hArr", '⇔'),
        ("forall", '∀'),
        ("part", '∂'),
        ("exist", '∃'),
        ("empty", '∅'),
        ("nabla", '∇'),
        ("isin", '∈'),
        ("notin", '∉'),
        ("ni", '∋'),
        ("prod", '∏'),
        ("sum", '∑'),
        ("minus", '−'),
        ("lowast", '∗'),
        ("radic", '√'),
        ("prop", '∝'),
        ("infin", '∞'),
        ("ang", '∠'),
        ("and", '∧'),
        ("or", '∨'),
        ("cap", '∩'),
        ("cup", '∪'),
        ("int", '∫'),
        ("there4", '∴'),
        ("sim", '∼'),
        ("cong", '≅'),
        ("asymp", '≈'),
        ("ne", '≠'),
        ("equiv", '≡'),
        ("le", '≤'),
        ("ge", '≥'),
        ("sub", '⊂'),
        ("sup", '⊃'),
        ("nsub", '⊄'),
        ("sube", '⊆'),
        ("supe", '⊇'),
        ("oplus", '⊕'),
        ("otimes", '⊗'),
        ("perp", '⊥'),
        ("sdot", '⋅'),
        ("lceil", '⌈'),
        ("rceil", '⌉'),
        ("lfloor", '⌊'),
        ("rfloor", '⌋'),
        ("lang", '〈'),
        ("rang", '〉'),
        ("loz", '◊'),
        ("spades", '♠'),
        ("clubs", '♣'),
        ("hearts", '♥'),
        ("diams", '♦'),
        ("Alpha", 'Α'),
        ("Beta", 'Β'),
        ("Gamma", 'Γ'),
        ("Delta", 'Δ'),
        ("Epsilon", 'Ε'),
        ("Zeta", 'Ζ'),
        ("Eta", 'Η'),
        ("Theta", 'Θ'),
        ("Iota", 'Ι'),
        ("Kappa", 'Κ'),
        ("Lambda", 'Λ'),
        ("Mu", 'Μ'),
        ("Nu", 'Ν'),
        ("Xi", 'Ξ'),
        ("Omicron", 'Ο'),
        ("Pi", 'Π'),
        ("Rho", 'Ρ'),
        ("Sigma", 'Σ'),
        ("Tau", 'Τ'),
        ("Upsilon", 'Υ'),
        ("Phi", 'Φ'),
        ("Chi", 'Χ'),
        ("Psi", 'Ψ'),
        ("Omega", 'Ω'),
        ("alpha", 'α'),
        ("beta", 'β'),
        ("gamma", 'γ'),
        ("delta", 'δ'),
        ("epsilon", 'ε'),
        ("zeta", 'ζ'),
        ("eta", 'η'),
        ("theta", 'θ'),
        ("iota", 'ι'),
        ("kappa", 'κ'),
        ("lambda", 'λ'),
        ("mu", 'μ'),
        ("nu", 'ν'),
        ("xi", 'ξ'),
        ("omicron", 'ο'),
        ("pi", 'π'),
        ("rho", 'ρ'),
        ("sigmaf", 'ς'),
        ("sigma", 'σ'),
        ("tau", 'τ'),
        ("upsilon", 'υ'),
        ("phi", 'φ'),
        ("chi", 'χ'),
        ("psi", 'ψ'),
        ("omega", 'ω'),
        ("thetasym", 'ϑ'),
        ("upsih", 'ϒ'),
        ("piv", 'ϖ'),
        ("ensp", ' '),
        ("emsp", ' '),
        ("thinsp", ' '),
        ("zwnj", '\u{200C}'),
        ("zwj", '\u{200D}'),
        ("lrm", '\u{200E}'),
        ("rlm", '\u{200F}'),
    ];
    TABLE
        .iter()
        .find(|(n, _)| n.eq_ignore_ascii_case(name))
        .map(|(_, c)| *c)
}

/// Collapse all whitespace runs to single spaces and trim.
pub(crate) fn collapse_ws(s: &str) -> String {
    s.split_whitespace().collect::<Vec<_>>().join(" ")
}

/// Parse HTML source into title + text blocks. Best-effort: malformed
/// markup degrades to flattened text rather than failing.
pub(crate) fn parse_html(source: &str) -> HtmlDocument {
    let mut work = source.to_string();

    let title = element_text(&work, "title").unwrap_or_default();
    let scripts = remove_tagged(&mut work, "script");
    let styles = remove_tagged(&mut work, "style");
    let mut saw_script_style = scripts > 0 || styles > 0;
    strip_comments_and_decls(&mut work);
    remove_head(&mut work);

    let mut blocks: Vec<HtmlBlock> = Vec::new();
    let mut cur = String::new();
    let mut kind = HtmlBlockKind::Paragraph;
    let mut saw_media = false;

    // Flush helper state lives in locals; `flush` is a closure over
    // them via a small fn-like macro pattern — implemented inline below.
    macro_rules! flush {
        () => {{
            let text = match kind {
                HtmlBlockKind::Preformatted => {
                    // Preserve interior newlines; drop leading/trailing
                    // blank lines only.
                    cur.trim_matches('\n').to_string()
                }
                _ => collapse_ws(&cur),
            };
            if !text.trim().is_empty() {
                blocks.push(HtmlBlock { kind, text });
            }
            cur.clear();
            kind = HtmlBlockKind::Paragraph;
        }};
    }

    let mut pos = 0usize;
    while pos < work.len() {
        let Some(lt_rel) = work[pos..].find('<') else {
            push_html_text(&mut cur, &work[pos..], kind);
            break;
        };
        let lt = pos + lt_rel;
        push_html_text(&mut cur, &work[pos..lt], kind);

        let Some(gt) = tag_end(&work, lt) else {
            // Unterminated '<': treat the rest as literal text.
            push_html_text(&mut cur, &work[lt..], kind);
            break;
        };
        let inner = work[lt + 1..gt].trim();
        let (closing, body) = match inner.strip_prefix('/') {
            Some(rest) => (true, rest.trim_start()),
            None => (false, inner),
        };
        // Tag name: ASCII alphanumeric (covers h1..h6).
        let name_len = body
            .find(|c: char| !(c.is_ascii_alphanumeric()))
            .unwrap_or(body.len());
        let name = &body[..name_len];

        if name.is_empty() || name.starts_with(['!', '?']) {
            // Declaration/PI that survived stripping — ignore.
        } else if HTML_MEDIA_TAGS.contains(&name.to_ascii_lowercase().as_str()) {
            saw_media = true;
        } else if name.eq_ignore_ascii_case("script") || name.eq_ignore_ascii_case("style") {
            // Stripped earlier; a stray occurrence is ignored. If it
            // had a separate close tag later, its text was already cut.
            saw_script_style = true;
        } else if name.eq_ignore_ascii_case("br") {
            cur.push('\n');
        } else if name.eq_ignore_ascii_case("hr") {
            flush!();
        } else if let Some(level) = heading_level(name) {
            flush!();
            if !closing {
                kind = HtmlBlockKind::Heading(level);
            }
        } else if name.eq_ignore_ascii_case("li") {
            flush!();
            if !closing {
                kind = HtmlBlockKind::ListItem;
            }
        } else if name.eq_ignore_ascii_case("pre") {
            flush!();
            if closing {
                // kind already reset by flush; nothing else to do.
            } else {
                kind = HtmlBlockKind::Preformatted;
            }
        } else if name.eq_ignore_ascii_case("td") || name.eq_ignore_ascii_case("th") {
            // Cell boundary: separate cells with a space (or newline
            // inside <pre>, where it acts as a line break anyway).
            if !closing {
                cur.push(if kind == HtmlBlockKind::Preformatted {
                    '\n'
                } else {
                    ' '
                });
            }
        } else if HTML_BLOCK_TAGS.contains(&name.to_ascii_lowercase().as_str()) {
            // Generic block boundary: flush; subsequent text is a
            // paragraph (kind was reset by flush!()).
            flush!();
        }
        // Everything else (inline tags, unknown tags) is flattened:
        // the tag itself contributes nothing, its text was already
        // appended by the scan.

        pos = gt + 1;
    }
    flush!();
    // Read `kind` so the macro's trailing reset is not dead code.
    let _ = kind;

    HtmlDocument {
        title,
        blocks,
        saw_media,
        saw_script_style,
    }
}

/// `h1`..`h6` → heading level 1–6, else `None`.
fn heading_level(name: &str) -> Option<usize> {
    let b = name.as_bytes();
    if b.len() == 2 && b[0].eq_ignore_ascii_case(&b'h') && b[1].is_ascii_digit() {
        let level = (b[1] - b'0') as usize;
        if (1..=6).contains(&level) {
            return Some(level);
        }
    }
    None
}

/// Append decoded text to the current run. Outside `<pre>` all
/// whitespace collapses to single spaces; inside it is preserved.
fn push_html_text(cur: &mut String, raw: &str, kind: HtmlBlockKind) {
    if raw.is_empty() {
        return;
    }
    let decoded = decode_entities(raw);
    if kind == HtmlBlockKind::Preformatted {
        cur.push_str(&decoded);
        return;
    }
    for ch in decoded.chars() {
        if ch.is_whitespace() {
            if !cur.is_empty() && !cur.ends_with(' ') {
                cur.push(' ');
            }
        } else {
            cur.push(ch);
        }
    }
}

/// Convert extracted HTML blocks into render lines for [`build_pdf`].
pub(crate) fn html_render_lines(blocks: &[HtmlBlock], text_width: f32) -> Vec<RenderLine> {
    let mut out = Vec::new();
    for block in blocks {
        // Preformatted text keeps its newlines; everything else was
        // already whitespace-collapsed at extraction, collapsed again
        // here defensively.
        let collapsed;
        let text: &str = if block.kind == HtmlBlockKind::Preformatted {
            block.text.as_str()
        } else {
            collapsed = collapse_ws(&block.text);
            collapsed.as_str()
        };
        match block.kind {
            HtmlBlockKind::Heading(level) => {
                if text.is_empty() {
                    continue;
                }
                let size = HEADING_SIZES[level.saturating_sub(1).min(5)];
                let pieces = wrap_text(&text, PdfFont::HelveticaBold, size, text_width);
                for (i, piece) in pieces.into_iter().enumerate() {
                    out.push(RenderLine {
                        text: piece,
                        font: PdfFont::HelveticaBold,
                        size,
                        indent: 0.0,
                        gap_before: if i == 0 { MD_HEADING_GAP } else { 0.0 },
                    });
                }
            }
            HtmlBlockKind::ListItem => {
                let text = format!("• {text}");
                let pieces = wrap_text(&text, PdfFont::Helvetica, BODY_SIZE, MD_LIST_WIDTH);
                for (i, mut piece) in pieces.into_iter().enumerate() {
                    // Hanging indent: continuation lines align under the
                    // bullet text, not under the bullet.
                    if i > 0 {
                        piece.insert_str(0, "  ");
                    }
                    out.push(RenderLine {
                        text: piece,
                        font: PdfFont::Helvetica,
                        size: BODY_SIZE,
                        indent: MD_LIST_INDENT,
                        gap_before: 0.0,
                    });
                }
            }
            HtmlBlockKind::Paragraph => {
                if text.is_empty() {
                    continue;
                }
                for piece in wrap_text(&text, PdfFont::Helvetica, BODY_SIZE, text_width) {
                    out.push(RenderLine {
                        text: piece,
                        font: PdfFont::Helvetica,
                        size: BODY_SIZE,
                        indent: 0.0,
                        gap_before: 0.0,
                    });
                }
            }
            HtmlBlockKind::Preformatted => {
                let mut any = false;
                for raw_line in block.text.split('\n') {
                    any = true;
                    let line = raw_line.trim_end_matches('\r');
                    if line.trim().is_empty() {
                        out.push(RenderLine {
                            text: String::new(),
                            font: PdfFont::Courier,
                            size: CODE_SIZE,
                            indent: MD_CODE_INDENT,
                            gap_before: 0.0,
                        });
                        continue;
                    }
                    for piece in wrap_text(line, PdfFont::Courier, CODE_SIZE, MD_CODE_WIDTH) {
                        out.push(RenderLine {
                            text: piece,
                            font: PdfFont::Courier,
                            size: CODE_SIZE,
                            indent: MD_CODE_INDENT,
                            gap_before: 0.0,
                        });
                    }
                }
                if !any {
                    out.push(RenderLine {
                        text: String::new(),
                        font: PdfFont::Courier,
                        size: CODE_SIZE,
                        indent: MD_CODE_INDENT,
                        gap_before: 0.0,
                    });
                }
            }
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
///
/// `page_breaks` holds line indices at which a new page must start
/// (used by the EPUB backend to start each spine document on a fresh
/// page). A break at an index whose page is still empty is a no-op.
pub(crate) fn build_pdf(
    lines: &[RenderLine],
    page_w: f32,
    page_h: f32,
    source: &str,
    warnings: &mut Vec<String>,
    page_breaks: &std::collections::BTreeSet<usize>,
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
    for (line_index, line) in lines.iter().enumerate() {
        if page_breaks.contains(&line_index) && !current.is_empty() {
            page_ops.push(std::mem::take(&mut current));
            cursor = 0.0;
        }
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

    /// Build the PDF image XObject stream for this image, consuming
    /// `self` so multi-megabyte sample buffers are moved, not cloned.
    fn into_xobject_stream(self) -> Stream {
        let (width, height, color_space, data, filter) = match self {
            Self::Rgb8 {
                width,
                height,
                data,
            } => (width, height, "DeviceRGB", data, None),
            Self::Jpeg {
                width,
                height,
                data,
                color_space,
            } => (width, height, color_space, data, Some("DCTDecode")),
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
        Format::Png => decode_png_bytes(input, warnings),
        Format::Jpeg => decode_jpeg_bytes(input),
        _ => Err(ConvertError::UnsupportedConversion {
            from: input_format,
            to: Format::Pdf,
        }),
    }
}

/// Decode PNG bytes into [`ImagePixels::Rgb8`], enforcing the
/// signature check, decoder resource limits, and pixel caps.
fn decode_png_bytes(input: &[u8], warnings: &mut Vec<String>) -> ConvertResult<ImagePixels> {
    if !input.starts_with(&PNG_SIGNATURE) {
        return Err(ConvertError::InvalidInput {
            reason: "input does not carry a PNG signature".to_string(),
        });
    }
    let img = decode_limited(input)?;
    if img.color().has_alpha() {
        warnings.push("PNG alpha channel is not preserved in PDF output".to_string());
    }
    let rgb = img.to_rgb8();
    let (width, height) = rgb.dimensions();
    check_image_size(width, height)?;
    Ok(ImagePixels::Rgb8 {
        width,
        height,
        data: rgb.into_raw(),
    })
}

/// Embed JPEG bytes verbatim under `/DCTDecode` after validating the
/// header (component count and pixel caps).
fn decode_jpeg_bytes(input: &[u8]) -> ConvertResult<ImagePixels> {
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
    check_image_size(width, height)?;
    Ok(ImagePixels::Jpeg {
        width,
        height,
        data: input.to_vec(),
        color_space,
    })
}

/// Decode any raster format the `image` crate supports (webp, gif,
/// bmp, ...) into [`ImagePixels::Rgb8`] with resource limits.
fn decode_generic_image_bytes(
    input: &[u8],
    warnings: &mut Vec<String>,
) -> ConvertResult<ImagePixels> {
    let img = decode_limited(input)?;
    if img.color().has_alpha() {
        warnings.push("image alpha channel is not preserved in PDF output".to_string());
    }
    let rgb = img.to_rgb8();
    let (width, height) = rgb.dimensions();
    check_image_size(width, height)?;
    Ok(ImagePixels::Rgb8 {
        width,
        height,
        data: rgb.into_raw(),
    })
}

/// Decode image bytes pulled from a CBZ member, dispatched on the
/// member's extension. JPEG keeps its verbatim `/DCTDecode` path;
/// everything else goes through the `image` crate decoder.
fn decode_archive_image(
    name: &str,
    data: &[u8],
    warnings: &mut Vec<String>,
) -> ConvertResult<ImagePixels> {
    match archive_image_ext(name) {
        Some("jpg") | Some("jpeg") => decode_jpeg_bytes(data),
        Some("png") => decode_png_bytes(data, warnings),
        Some(_) => decode_generic_image_bytes(data, warnings),
        None => Err(ConvertError::InvalidInput {
            reason: format!("archive member \"{name}\" is not a supported image"),
        }),
    }
}

/// Decode `input` with the `image` crate under strict dimension and
/// allocation limits — guards against decompression bombs where a
/// tiny file declares enormous pixel dimensions.
fn decode_limited(input: &[u8]) -> ConvertResult<image::DynamicImage> {
    let mut reader = image::ImageReader::new(std::io::Cursor::new(input))
        .with_guessed_format()
        .map_err(|e| ConvertError::InvalidInput {
            reason: format!("could not determine image format: {e}"),
        })?;
    // `image::Limits` is `#[non_exhaustive]`, so build it from `Default`
    // and set the fields individually.
    let mut limits = image::Limits::default();
    limits.max_image_width = Some(MAX_IMAGE_DIM);
    limits.max_image_height = Some(MAX_IMAGE_DIM);
    limits.max_alloc = Some(MAX_IMAGE_ALLOC);
    reader.limits(limits);
    reader.decode().map_err(|e| ConvertError::InvalidInput {
        reason: format!("could not decode image: {e}"),
    })
}

/// Reject degenerate or over-large pixel dimensions.
fn check_image_size(width: u32, height: u32) -> ConvertResult<()> {
    if width == 0 || height == 0 {
        return Err(ConvertError::InvalidInput {
            reason: "image has zero width or height".to_string(),
        });
    }
    let pixels = width as u64 * height as u64;
    if width > MAX_IMAGE_DIM || height > MAX_IMAGE_DIM || pixels > MAX_IMAGE_PIXELS {
        return Err(ConvertError::InvalidInput {
            reason: format!(
                "image dimensions {width}x{height} exceed the {}x{MAX_IMAGE_DIM} / \
                 {MAX_IMAGE_PIXELS}-pixel embedding limit",
                MAX_IMAGE_DIM
            ),
        });
    }
    Ok(())
}

/// Lowercased extension of an archive member name when it is a
/// supported comic page image (`png`, `jpg`/`jpeg`, `webp`, `gif`,
/// `bmp`). Returns `None` for directories and non-image members.
fn archive_image_ext(name: &str) -> Option<&'static str> {
    let base = name.rsplit(['/', '\\']).next().unwrap_or(name);
    let ext = base.rsplit_once('.')?.1;
    if ext.is_empty() || ext.len() > 5 {
        return None;
    }
    match ext.to_ascii_lowercase().as_str() {
        "png" => Some("png"),
        "jpg" | "jpeg" => Some("jpg"),
        "webp" => Some("webp"),
        "gif" => Some("gif"),
        "bmp" => Some("bmp"),
        _ => None,
    }
}

/// Numeric-aware ("natural") ordering for archive member names:
/// `page1 < page2 < page10`, case-insensitive, with a plain
/// byte-order tie-break for determinism.
fn natural_cmp(a: &str, b: &str) -> std::cmp::Ordering {
    use std::cmp::Ordering;
    let mut ac = a.chars().peekable();
    let mut bc = b.chars().peekable();
    loop {
        match (ac.peek().copied(), bc.peek().copied()) {
            (None, None) => break,
            (None, Some(_)) => return Ordering::Less,
            (Some(_), None) => return Ordering::Greater,
            (Some(x), Some(y)) => {
                if x.is_ascii_digit() && y.is_ascii_digit() {
                    let na = take_digits(&mut ac);
                    let nb = take_digits(&mut bc);
                    // Compare digit runs by numeric value: strip
                    // leading zeros, longer stripped run wins, then
                    // lexicographic on the stripped digits.
                    let sa = na.trim_start_matches('0');
                    let sb = nb.trim_start_matches('0');
                    let ord = sa
                        .len()
                        .cmp(&sb.len())
                        .then_with(|| sa.cmp(sb))
                        .then_with(|| na.len().cmp(&nb.len()));
                    if ord != Ordering::Equal {
                        return ord;
                    }
                } else {
                    match x.to_ascii_lowercase().cmp(&y.to_ascii_lowercase()) {
                        Ordering::Equal => {
                            ac.next();
                            bc.next();
                        }
                        ord => return ord,
                    }
                }
            }
        }
    }
    // Numeric runs compared equal but representations differed
    // ("page01" vs "page1") — fall back to byte order.
    a.cmp(b)
}

/// Consume and return the run of ASCII digits at the front of `it`.
fn take_digits(it: &mut std::iter::Peekable<std::str::Chars>) -> String {
    let mut s = String::new();
    while let Some(&c) = it.peek() {
        if !c.is_ascii_digit() {
            break;
        }
        s.push(c);
        it.next();
    }
    s
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
        assert!(b.supports(Format::Html, Format::Pdf));
        // Everything else is false.
        assert!(!b.supports(Format::Png, Format::Png));
        assert!(!b.supports(Format::Jpeg, Format::Markdown));
        assert!(!b.supports(Format::Html, Format::Txt));
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
            .convert(&req(Format::Html, Format::Txt, b"<p>x</p>"))
            .unwrap_err();
        assert!(matches!(err, ConvertError::UnsupportedConversion { .. }));
        let err = b
            .convert(&req(Format::Docx, Format::Pdf, b"PK\x03\x04"))
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
    fn html_to_pdf_produces_valid_pdf() {
        let b = RustBackend::new();
        let html = br#"<!DOCTYPE html>
<html><head><title>Test &amp; Title</title>
<style>body { color: red; }</style>
<script>alert("x")</script></head>
<body>
<h1>Hello <b>World</b></h1>
<p>First paragraph with <a href="https://example.com">a link</a> &amp; entities &#65;.</p>
<ul><li>one</li><li>two</li></ul>
<pre>line1
  line2</pre>
<img src="pic.png">
</body></html>"#;
        let out = b.convert(&req(Format::Html, Format::Pdf, html)).unwrap();
        assert_eq!(out.output_format, Format::Pdf);
        assert_eq!(out.backend, "rust");
        assert!(out.bytes.starts_with(b"%PDF"));
        Document::load_mem(&out.bytes).unwrap();
        // Styling/scripts/media drops are reported.
        assert!(out.warnings.iter().any(|w| w.contains("styling")));
        assert!(out.warnings.iter().any(|w| w.contains("script")));
        assert!(out.warnings.iter().any(|w| w.contains("media")));

        let doc = Document::load_mem(&out.bytes).unwrap();
        let content = content_stream_text(&doc);
        // Extracted text is WinAnsi-encoded inside literal strings;
        // check the raw stream for readable ASCII fragments.
        assert!(content.contains("Hello"), "heading missing: {content}");
        assert!(content.contains("World"));
        assert!(content.contains("First paragraph"));
        assert!(content.contains("a link"));
        assert!(content.contains("entities A."));
        assert!(content.contains("one"));
        assert!(content.contains("line1"));
        // Dropped content must not appear.
        assert!(!content.contains("alert"));
        assert!(!content.contains("color: red"));
        assert!(!content.contains("pic.png"));
        // Heading uses the larger bold font size (H1 = 20pt).
        assert!(content.contains(" 20 Tf"), "H1 size missing: {content}");
        // List item rendered with a bullet.
        assert!(content.contains("\u{2022} one") || content.contains("one"));
    }

    #[test]
    fn html_entities_and_title() {
        let doc = parse_html(
            "<html><head><title>Fish &amp; Chips &lt;3</title></head><body><p>caf&eacute; &#x2014; ok</p></body></html>",
        );
        assert_eq!(doc.title, "Fish & Chips <3");
        assert_eq!(doc.blocks.len(), 1);
        assert!(
            doc.blocks[0].text.contains("café — ok"),
            "got: {:?}",
            doc.blocks[0].text
        );
        assert!(!doc.saw_media && !doc.saw_script_style);
    }

    #[test]
    fn html_script_unclosed_does_not_leak() {
        let doc = parse_html("<p>before</p><script>var x = 1 < 2;</script><p>after</p>");
        let all: String = doc
            .blocks
            .iter()
            .map(|b| b.text.as_str())
            .collect::<Vec<_>>()
            .join(" ");
        assert!(all.contains("before") && all.contains("after"));
        assert!(!all.contains("var x"));
        assert!(doc.saw_script_style);
    }

    #[test]
    fn html_empty_and_garbage_degrade_to_pdf() {
        let b = RustBackend::new();
        // Pure text with no tags still yields a PDF.
        let out = b
            .convert(&req(Format::Html, Format::Pdf, b"just text, no markup"))
            .unwrap();
        assert!(out.bytes.starts_with(b"%PDF"));
        // Bare '<' and stray tags do not panic.
        let out = b
            .convert(&req(Format::Html, Format::Pdf, b"a < b and <p>unclosed"))
            .unwrap();
        assert!(out.bytes.starts_with(b"%PDF"));
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

    /// Small multipart EML fixture: alternative text/plain +
    /// text/html plus a mixed-in PDF attachment.
    const EML_FIXTURE: &[u8] = b"From: Alice <alice@example.com>\r\n\
        To: Bob <bob@example.com>\r\n\
        Cc: Carol <carol@example.com>\r\n\
        Subject: Quarterly report\r\n\
        Date: Mon, 01 Jan 2024 10:00:00 +0000\r\n\
        MIME-Version: 1.0\r\n\
        Content-Type: multipart/mixed; boundary=\"OUTER\"\r\n\
        \r\n\
        --OUTER\r\n\
        Content-Type: multipart/alternative; boundary=\"INNER\"\r\n\
        \r\n\
        --INNER\r\n\
        Content-Type: text/plain; charset=utf-8\r\n\
        \r\n\
        Hello Bob,\r\n\
        the plain-text body.\r\n\
        --INNER\r\n\
        Content-Type: text/html; charset=utf-8\r\n\
        \r\n\
        <p>Hello Bob,</p><p>the <b>html</b> body.</p>\r\n\
        --INNER--\r\n\
        --OUTER\r\n\
        Content-Type: application/pdf\r\n\
        Content-Disposition: attachment; filename=\"report.pdf\"\r\n\
        Content-Transfer-Encoding: base64\r\n\
        \r\n\
        JVBERi0xLjQK\r\n\
        --OUTER--\r\n";

    #[test]
    fn eml_to_pdf_produces_valid_pdf() {
        let b = RustBackend::new();
        assert!(b.supports(Format::Eml, Format::Pdf));
        let out = b
            .convert(&req(Format::Eml, Format::Pdf, EML_FIXTURE))
            .unwrap();
        assert!(out.bytes.starts_with(b"%PDF"), "not a PDF");
        assert_eq!(out.output_format, Format::Pdf);
        assert_eq!(out.backend, "rust");
        // Attachment must be surfaced, never silently dropped.
        assert!(
            out.warnings.iter().any(|w| w.contains("report.pdf")),
            "warnings: {:?}",
            out.warnings
        );
        // Header block and body text land in the content stream.
        let doc = Document::load_mem(&out.bytes).unwrap();
        let content = content_stream_text(&doc);
        assert!(content.contains("Subject"), "{content}");
        assert!(content.contains("Quarterly report"), "{content}");
        assert!(content.contains("plain-text body"), "{content}");
    }

    #[test]
    fn eml_to_pdf_prefers_text_plain_over_html() {
        let b = RustBackend::new();
        let out = b
            .convert(&req(Format::Eml, Format::Pdf, EML_FIXTURE))
            .unwrap();
        let doc = Document::load_mem(&out.bytes).unwrap();
        let content = content_stream_text(&doc);
        assert!(content.contains("plain-text body"));
        // The html alternative must not be rendered when plain exists.
        assert!(!content.contains("html"), "{content}");
    }

    #[test]
    fn eml_to_pdf_html_only_body_is_stripped() {
        let b = RustBackend::new();
        let eml = b"From: a@b.c\r\n\
                   Content-Type: text/html\r\n\
                   \r\n\
                   <p>Hello <b>there</b> &amp; welcome</p>\r\n";
        let out = b.convert(&req(Format::Eml, Format::Pdf, eml)).unwrap();
        assert!(
            out.warnings.iter().any(|w| w.contains("text/html")),
            "warnings: {:?}",
            out.warnings
        );
        let doc = Document::load_mem(&out.bytes).unwrap();
        let content = content_stream_text(&doc);
        assert!(content.contains("Hello there & welcome"), "{content}");
    }

    #[test]
    fn eml_to_pdf_garbage_is_invalid_input() {
        let b = RustBackend::new();
        let err = b
            .convert(&req(
                Format::Eml,
                Format::Pdf,
                b"definitely not a mime message",
            ))
            .unwrap_err();
        assert!(matches!(err, ConvertError::InvalidInput { .. }));
    }

    #[test]
    fn eml_format_parse_aliases() {
        assert_eq!(Format::parse("eml"), Some(Format::Eml));
        assert_eq!(Format::parse("email"), Some(Format::Eml));
        assert_eq!(Format::parse("message/rfc822"), Some(Format::Eml));
        assert_eq!(Format::Eml.as_str(), "eml");
    }

    // --- CBZ → PDF ----------------------------------------------------

    /// PNG of `w`×`h` solid red, encoded via the `image` crate.
    fn png_of(w: u32, h: u32) -> Vec<u8> {
        let img = image::RgbImage::from_pixel(w, h, image::Rgb([200, 30, 30]));
        let mut buf = Vec::new();
        image::DynamicImage::ImageRgb8(img)
            .write_to(&mut std::io::Cursor::new(&mut buf), image::ImageFormat::Png)
            .unwrap();
        buf
    }

    /// Wrap `data` in a raw deflate stream made of stored blocks —
    /// legal method-8 member data that exercises the inflate path.
    fn deflate_stored(data: &[u8]) -> Vec<u8> {
        let mut out = Vec::new();
        let mut chunks = data.chunks(65535).peekable();
        while let Some(chunk) = chunks.next() {
            out.push(if chunks.peek().is_none() { 0x01 } else { 0x00 });
            let len = chunk.len() as u16;
            out.extend_from_slice(&len.to_le_bytes());
            out.extend_from_slice(&(!len).to_le_bytes());
            out.extend_from_slice(chunk);
        }
        if data.is_empty() {
            out.push(0x01);
            out.extend_from_slice(&0u16.to_le_bytes());
            out.extend_from_slice(&0xFFFFu16.to_le_bytes());
        }
        out
    }

    /// Build a minimal multi-entry ZIP. `members` is
    /// `(name, data, deflate)`; `flags_or` is OR-ed into the
    /// general-purpose flags of every member.
    fn test_zip(members: &[(&str, &[u8], bool)], flags_or: u16) -> Vec<u8> {
        let mut out = Vec::new();
        let mut centrals: Vec<(usize, u32, u32, u32, u32)> = Vec::new(); // name_off,name_len,crc,csize,usize_,lho
        for &(name, data, deflate) in members {
            let name_b = name.as_bytes();
            let (payload, method, csize, usize_) = if deflate {
                let d = deflate_stored(data);
                (d.clone(), 8u16, d.len() as u32, data.len() as u32)
            } else {
                (data.to_vec(), 0u16, data.len() as u32, data.len() as u32)
            };
            let crc = crate::crates::conversion_engine::archive::crc32(data);
            let lho = out.len() as u32;
            out.extend_from_slice(&0x0403_4b50u32.to_le_bytes());
            out.extend_from_slice(&20u16.to_le_bytes());
            out.extend_from_slice(&(flags_or | 0u16).to_le_bytes());
            out.extend_from_slice(&method.to_le_bytes());
            out.extend_from_slice(&0u16.to_le_bytes());
            out.extend_from_slice(&0u16.to_le_bytes());
            out.extend_from_slice(&crc.to_le_bytes());
            out.extend_from_slice(&csize.to_le_bytes());
            out.extend_from_slice(&usize_.to_le_bytes());
            out.extend_from_slice(&(name_b.len() as u16).to_le_bytes());
            out.extend_from_slice(&0u16.to_le_bytes());
            out.extend_from_slice(name_b);
            out.extend_from_slice(&payload);
            centrals.push((
                out.len() - payload.len() - name_b.len() - 30,
                name_b.len() as u32,
                crc,
                csize,
                usize_,
            ));
            // stash local header offset alongside: recompute below
            centrals.last_mut().unwrap().0 = lho as usize;
        }
        let cd_offset = out.len() as u32;
        for (i, &(name, _, _)) in members.iter().enumerate() {
            let (lho, name_len, crc, csize, usize_) = centrals[i];
            let name_b = name.as_bytes();
            let method = if members[i].2 { 8u16 } else { 0u16 };
            out.extend_from_slice(&0x0201_4b50u32.to_le_bytes());
            out.extend_from_slice(&20u16.to_le_bytes());
            out.extend_from_slice(&20u16.to_le_bytes());
            out.extend_from_slice(&flags_or.to_le_bytes());
            out.extend_from_slice(&method.to_le_bytes());
            out.extend_from_slice(&0u16.to_le_bytes());
            out.extend_from_slice(&0u16.to_le_bytes());
            out.extend_from_slice(&crc.to_le_bytes());
            out.extend_from_slice(&csize.to_le_bytes());
            out.extend_from_slice(&usize_.to_le_bytes());
            out.extend_from_slice(&(name_len as u16).to_le_bytes());
            out.extend_from_slice(&0u16.to_le_bytes());
            out.extend_from_slice(&0u16.to_le_bytes());
            out.extend_from_slice(&0u16.to_le_bytes());
            out.extend_from_slice(&0u16.to_le_bytes());
            out.extend_from_slice(&0u32.to_le_bytes());
            out.extend_from_slice(&(lho as u32).to_le_bytes());
            out.extend_from_slice(name_b);
        }
        let cd_size = out.len() as u32 - cd_offset;
        out.extend_from_slice(&0x0605_4b50u32.to_le_bytes());
        out.extend_from_slice(&0u16.to_le_bytes());
        out.extend_from_slice(&0u16.to_le_bytes());
        out.extend_from_slice(&(members.len() as u16).to_le_bytes());
        out.extend_from_slice(&(members.len() as u16).to_le_bytes());
        out.extend_from_slice(&cd_size.to_le_bytes());
        out.extend_from_slice(&cd_offset.to_le_bytes());
        out.extend_from_slice(&0u16.to_le_bytes());
        out
    }

    /// Page MediaBox widths (in order) from a generated CBZ PDF.
    fn page_widths(doc: &Document) -> Vec<f32> {
        let mut widths: Vec<(u32, f32)> = doc
            .get_pages()
            .iter()
            .map(|(num, id)| {
                let page = doc.get_dictionary(*id).unwrap();
                let mb = page.get(b"MediaBox").unwrap().as_array().unwrap();
                let w = mb[2].as_f32().unwrap();
                (*num, w)
            })
            .collect();
        widths.sort_by_key(|(n, _)| *n);
        widths.into_iter().map(|(_, w)| w).collect()
    }

    #[test]
    fn cbz_to_pdf_two_png_pages() {
        let b = RustBackend::new();
        let p1 = png_of(2, 2);
        let p2 = png_of(4, 4);
        let zip = test_zip(&[("page1.png", &p1, false), ("page2.png", &p2, true)], 0);
        let out = b.convert(&req(Format::Cbz, Format::Pdf, &zip)).unwrap();
        assert_eq!(out.output_format, Format::Pdf);
        assert!(out.bytes.starts_with(b"%PDF"));
        let doc = Document::load_mem(&out.bytes).unwrap();
        assert_eq!(doc.get_pages().len(), 2);
        // Image-native sizing: 2px→1.5pt, 4px→3.0pt wide.
        assert_eq!(page_widths(&doc), vec![1.5, 3.0]);
    }

    #[test]
    fn cbz_pages_sorted_naturally() {
        let b = RustBackend::new();
        let small = png_of(2, 2); // 1.5 pt
        let mid = png_of(4, 4); // 3.0 pt
        let big = png_of(8, 8); // 6.0 pt
        // Deliberately out of order in the archive.
        let zip = test_zip(
            &[
                ("page10.png", &big, false),
                ("page1.png", &small, false),
                ("page2.png", &mid, false),
            ],
            0,
        );
        let out = b.convert(&req(Format::Cbz, Format::Pdf, &zip)).unwrap();
        let doc = Document::load_mem(&out.bytes).unwrap();
        assert_eq!(doc.get_pages().len(), 3);
        assert_eq!(page_widths(&doc), vec![1.5, 3.0, 6.0]);
    }

    #[test]
    fn cbz_skips_non_image_entries_with_warning() {
        let b = RustBackend::new();
        let p = png_of(2, 2);
        let zip = test_zip(
            &[
                ("ComicInfo.xml", b"<ComicInfo/>", false),
                ("page1.png", &p, false),
                ("thumbs.db", b"junk", false),
            ],
            0,
        );
        let out = b.convert(&req(Format::Cbz, Format::Pdf, &zip)).unwrap();
        assert!(
            out.warnings.iter().any(|w| w.contains("non-image")),
            "warnings: {:?}",
            out.warnings
        );
        let doc = Document::load_mem(&out.bytes).unwrap();
        assert_eq!(doc.get_pages().len(), 1);
    }

    #[test]
    fn cbz_encrypted_entry_is_invalid_input() {
        let b = RustBackend::new();
        let p = png_of(2, 2);
        let zip = test_zip(&[("page1.png", &p, false)], 0x0001);
        let err = b.convert(&req(Format::Cbz, Format::Pdf, &zip)).unwrap_err();
        assert!(matches!(err, ConvertError::InvalidInput { .. }));
    }

    #[test]
    fn cbz_without_images_is_invalid_input() {
        let b = RustBackend::new();
        let zip = test_zip(&[("readme.txt", b"no pages here", false)], 0);
        let err = b.convert(&req(Format::Cbz, Format::Pdf, &zip)).unwrap_err();
        assert!(matches!(err, ConvertError::InvalidInput { .. }));
    }

    #[test]
    fn cbz_garbage_is_invalid_input() {
        let b = RustBackend::new();
        let err = b
            .convert(&req(Format::Cbz, Format::Pdf, b"not a zip at all"))
            .unwrap_err();
        assert!(matches!(err, ConvertError::InvalidInput { .. }));
    }

    #[test]
    fn cbr_is_explicitly_unsupported() {
        let b = RustBackend::new();
        assert!(!b.supports(Format::Cbr, Format::Pdf));
        let err = b
            .convert(&req(Format::Cbr, Format::Pdf, b"rar!"))
            .unwrap_err();
        assert!(matches!(err, ConvertError::UnsupportedConversion { .. }));
    }

    #[test]
    fn cbz_format_parse_aliases() {
        assert_eq!(Format::parse("cbz"), Some(Format::Cbz));
        assert_eq!(
            Format::parse("application/vnd.comicbook+zip"),
            Some(Format::Cbz)
        );
        assert_eq!(Format::parse("cbr"), Some(Format::Cbr));
        assert_eq!(Format::Cbz.as_str(), "cbz");
        assert_eq!(Format::Cbr.as_str(), "cbr");
    }

    #[test]
    fn natural_cmp_orders_pages() {
        let names = ["page10.png", "page2.png", "page1.png", "Page1b.png"];
        let mut sorted = names;
        sorted.sort_by(|a, b| natural_cmp(a, b));
        assert_eq!(
            sorted,
            ["page1.png", "Page1b.png", "page2.png", "page10.png"]
        );
    }
}
