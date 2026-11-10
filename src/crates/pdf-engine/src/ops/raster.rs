//! Rasterization helper — renders PDF pages to PNG via the external
//! `pdftoppm` binary (poppler-utils).
//!
//! `lopdf` parses PDF structure but cannot render; producing pixels
//! requires an external renderer. This module is the single place that
//! shells out to `pdftoppm`, shared by the `render` and `ocr`
//! operations. When the binary is absent every caller fails with
//! [`PdfError::MissingTool`] — results are never faked.
//!
//! Contract:
//!
//! - `find_tool`/`tool_available` probe `PATH` without side effects and
//!   are what [`PdfEngine::health`](crate::crates::pdf_engine::PdfEngine)
//!   uses to report degraded capability.
//! - [`rasterize`] loads the input through [`load_pdf`] first, so
//!   `max_input_bytes`/`max_pages`/encryption rules apply before any
//!   subprocess runs, then writes the bytes to a private tempdir,
//!   invokes `pdftoppm -png -r <dpi>` and reads back the PNG files.
//! - Output is capped: per-page and total byte budgets are enforced so
//!   a huge page count at high DPI cannot exhaust memory.

use std::path::{Path, PathBuf};
use std::process::Command;

use serde::{Deserialize, Serialize};

use crate::crates::core::engine::EngineLimits;

use crate::crates::pdf_engine::error::{PdfError, PdfResult};

use super::load::{load_pdf, reject_encrypted};
use super::pages::parse_page_range;

/// Binary used for rasterization (poppler-utils).
pub(crate) const RENDER_TOOL: &str = "pdftoppm";

/// Binary used for OCR (tesseract).
pub(crate) const OCR_TOOL: &str = "tesseract";

/// Default rasterization DPI — matches common preview/print defaults.
pub(crate) const DEFAULT_DPI: u32 = 150;

/// Accepted DPI range. Below ~36 output is unreadable; above 1200 the
/// PNG budgets below are hit almost immediately.
const MIN_DPI: u32 = 36;
const MAX_DPI: u32 = 1200;

/// Per-page PNG budget (64 MiB) and total budget (512 MiB).
const MAX_PAGE_PNG_BYTES: u64 = 64 * 1024 * 1024;
const MAX_TOTAL_PNG_BYTES: u64 = 512 * 1024 * 1024;

/// A rendered page image.
#[derive(Debug, Clone)]
pub struct PageImage {
    /// 1-based page number.
    pub page: u32,
    /// Pixel width from the PNG IHDR.
    pub width_px: u32,
    /// Pixel height from the PNG IHDR.
    pub height_px: u32,
    /// PNG-encoded image bytes.
    pub png: Vec<u8>,
}

/// Serializable form of [`PageImage`] for CLI/MCP output — the PNG is
/// base64-encoded rather than serialized as a byte array.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct PageImageReport {
    /// 1-based page number.
    pub page: u32,
    /// Pixel width.
    pub width_px: u32,
    /// Pixel height.
    pub height_px: u32,
    /// PNG bytes, base64-encoded.
    pub png_base64: String,
}

impl From<&PageImage> for PageImageReport {
    fn from(image: &PageImage) -> Self {
        use base64::Engine as _;
        Self {
            page: image.page,
            width_px: image.width_px,
            height_px: image.height_px,
            png_base64: base64::engine::general_purpose::STANDARD.encode(&image.png),
        }
    }
}

/// Output encoding for [`rasterize_render`].
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum RasterFormat {
    /// PNG (lossless).
    Png,
    /// JPEG (lossy; `jpeg_quality` applies).
    Jpeg,
    /// TIFF (lossless; produced directly by `pdftoppm -tiff`).
    Tiff,
}

impl Default for RasterFormat {
    fn default() -> Self {
        Self::Png
    }
}

/// Options for [`rasterize_render`]. All fields are optional except
/// `format`, which defaults to PNG.
#[derive(Debug, Clone)]
pub struct RasterOptions {
    /// Output encoding.
    pub format: RasterFormat,
    /// Grayscale output (`pdftoppm -gray`).
    pub grayscale: bool,
    /// Monochrome output (`pdftoppm -mono`). Takes precedence over
    /// `grayscale` when both are set.
    pub monochrome: bool,
    /// Crop region in **output pixels** (`pdftoppm -x -y -W -H`,
    /// top-left origin). Callers convert PDF points using the render
    /// DPI before calling.
    pub crop_px: Option<[u32; 4]>,
    /// Scale the longer page side to at most this many pixels
    /// (`pdftoppm -scale-to`).
    pub scale_to: Option<u32>,
    /// JPEG quality 1–100 (`pdftoppm -jpegopt quality=N`). Ignored
    /// for non-JPEG formats.
    pub jpeg_quality: Option<u8>,
}

impl Default for RasterOptions {
    fn default() -> Self {
        Self {
            format: RasterFormat::Png,
            grayscale: false,
            monochrome: false,
            crop_px: None,
            scale_to: None,
            jpeg_quality: None,
        }
    }
}

/// A rendered page in an explicit encoding — the multi-format
/// counterpart of [`PageImage`].
#[derive(Debug, Clone)]
pub struct RenderedPage {
    /// 1-based page number.
    pub page: u32,
    /// Pixel width (decoded from the output when the format allows).
    pub width_px: u32,
    /// Pixel height.
    pub height_px: u32,
    /// Encoding of `data`.
    pub format: RasterFormat,
    /// Encoded image bytes.
    pub data: Vec<u8>,
}

/// Locate `tool` on `PATH`. Returns the full path of the first
/// executable match, or [`PdfError::MissingTool`].
///
/// The search is a plain directory scan — no shell, no hashing — so it
/// is cheap enough to call from `health()` probes.
pub(crate) fn find_tool(tool: &str) -> PdfResult<PathBuf> {
    let path_var = std::env::var_os("PATH").unwrap_or_default();
    for dir in std::env::split_paths(&path_var) {
        let candidate = dir.join(tool);
        if is_executable(&candidate) {
            return Ok(candidate);
        }
    }
    Err(PdfError::MissingTool(tool.to_string()))
}

/// Whether `tool` resolves on `PATH`. Never fails — absence is `false`.
pub(crate) fn tool_available(tool: &str) -> bool {
    find_tool(tool).is_ok()
}

#[cfg(unix)]
fn is_executable(path: &Path) -> bool {
    use std::os::unix::fs::PermissionsExt;
    path.is_file()
        && std::fs::metadata(path)
            .map(|m| m.permissions().mode() & 0o111 != 0)
            .unwrap_or(false)
}

#[cfg(not(unix))]
fn is_executable(path: &Path) -> bool {
    path.is_file()
}

/// Validate a caller-supplied DPI, returning it or a default.
pub(crate) fn checked_dpi(dpi: Option<u32>) -> PdfResult<u32> {
    match dpi {
        None => Ok(DEFAULT_DPI),
        Some(dpi) if (MIN_DPI..=MAX_DPI).contains(&dpi) => Ok(dpi),
        Some(dpi) => Err(PdfError::InvalidInput(format!(
            "dpi must be between {MIN_DPI} and {MAX_DPI}, got {dpi}"
        ))),
    }
}

/// Rasterize selected pages of `input` to PNG via `pdftoppm`.
///
/// `pages` follows the shared 1-based range grammar (`"1-3,8"`,
/// `"all"`); `None` renders every page. Encrypted documents are
/// rejected — a renderer cannot read them without a password, and this
/// engine does not decrypt implicitly.
///
/// The `pdftoppm` `-f`/`-l` window covers `min..=max` of the selection
/// (the tool only accepts contiguous ranges); non-contiguous selections
/// are rendered then filtered back to the requested set.
pub(crate) fn rasterize(
    input: &[u8],
    pages: Option<&str>,
    dpi: u32,
    limits: &EngineLimits,
) -> PdfResult<Vec<PageImage>> {
    let rendered = rasterize_render(input, pages, dpi, &RasterOptions::default(), limits)?;
    Ok(rendered
        .into_iter()
        .map(|r| PageImage {
            page: r.page,
            width_px: r.width_px,
            height_px: r.height_px,
            png: r.data,
        })
        .collect())
}

/// Rasterize selected pages with explicit output options.
///
/// Same contract as [`rasterize`] plus [`RasterOptions`]: output
/// encoding (PNG/JPEG/TIFF), grayscale/monochrome modes, pixel-space
/// crop, `-scale-to` sizing and JPEG quality — all passed straight to
/// `pdftoppm`. Encrypted documents are rejected; missing `pdftoppm` is
/// [`PdfError::MissingTool`].
pub(crate) fn rasterize_render(
    input: &[u8],
    pages: Option<&str>,
    dpi: u32,
    opts: &RasterOptions,
    limits: &EngineLimits,
) -> PdfResult<Vec<RenderedPage>> {
    let doc = load_pdf(input, limits)?;
    reject_encrypted(&doc)?;
    let page_count = doc.get_pages().len() as u32;
    drop(doc); // rasterization re-parses via the tool; keep load only for validation/limits
    let selected = match pages {
        Some(spec) => parse_page_range(spec, page_count)?,
        None => (1..=page_count).collect(),
    };
    if selected.is_empty() {
        return Ok(Vec::new());
    }
    let first = *selected.first().unwrap();
    let last = *selected.last().unwrap();

    let tool = find_tool(RENDER_TOOL)?;

    let dir = tempfile::tempdir().map_err(|e| PdfError::Io(std::io::Error::other(e)))?;
    let input_path = dir.path().join("input.pdf");
    std::fs::write(&input_path, input).map_err(PdfError::Io)?;
    let out_prefix = dir.path().join("page");

    let mut cmd = Command::new(&tool);
    cmd.arg(match opts.format {
        RasterFormat::Png => "-png",
        RasterFormat::Jpeg => "-jpeg",
        RasterFormat::Tiff => "-tiff",
    })
    .arg("-r")
    .arg(dpi.to_string())
    .arg("-f")
    .arg(first.to_string())
    .arg("-l")
    .arg(last.to_string());
    if opts.monochrome {
        cmd.arg("-mono");
    } else if opts.grayscale {
        cmd.arg("-gray");
    }
    if let Some(n) = opts.scale_to {
        cmd.arg("-scale-to").arg(n.to_string());
    }
    if let Some([x, y, w, h]) = opts.crop_px {
        cmd.arg("-x")
            .arg(x.to_string())
            .arg("-y")
            .arg(y.to_string())
            .arg("-W")
            .arg(w.to_string())
            .arg("-H")
            .arg(h.to_string());
    }
    if opts.format == RasterFormat::Jpeg
        && let Some(q) = opts.jpeg_quality
    {
        cmd.arg("-jpegopt").arg(format!("quality={q}"));
    }
    let status = cmd
        .arg(&input_path)
        .arg(&out_prefix)
        .stdin(std::process::Stdio::null())
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::piped())
        .status()
        .map_err(|e| PdfError::ExternalTool {
            tool: RENDER_TOOL.to_string(),
            detail: format!("failed to spawn: {e}"),
        })?;
    if !status.success() {
        return Err(PdfError::ExternalTool {
            tool: RENDER_TOOL.to_string(),
            detail: format!("exited with {status}"),
        });
    }

    // pdftoppm names outputs `<prefix>-<page>.<ext>`; the extension
    // depends on the format flag (.png/.jpg/.tif or .tiff).
    let exts: &[&str] = match opts.format {
        RasterFormat::Png => &["png"],
        RasterFormat::Jpeg => &["jpg", "jpeg"],
        RasterFormat::Tiff => &["tif", "tiff"],
    };
    let wanted: std::collections::BTreeSet<u32> = selected.iter().copied().collect();
    let mut out = Vec::with_capacity(wanted.len());
    let mut total_bytes: u64 = 0;
    let mut entries: Vec<_> = std::fs::read_dir(dir.path())
        .map_err(PdfError::Io)?
        .filter_map(|e| e.ok())
        .map(|e| e.path())
        .filter(|p| {
            p.extension()
                .and_then(|x| x.to_str())
                .is_some_and(|x| exts.contains(&x.to_ascii_lowercase().as_str()))
        })
        .collect();
    entries.sort();

    for path in entries {
        let Some(page) = png_page_number(&path) else {
            continue;
        };
        if !wanted.contains(&page) {
            continue;
        }
        let meta = std::fs::metadata(&path).map_err(PdfError::Io)?;
        if meta.len() > MAX_PAGE_PNG_BYTES {
            return Err(PdfError::LimitExceeded(format!(
                "rendered page {page} is {} bytes, exceeding per-page budget {MAX_PAGE_PNG_BYTES}",
                meta.len()
            )));
        }
        total_bytes += meta.len();
        if total_bytes > MAX_TOTAL_PNG_BYTES {
            return Err(PdfError::LimitExceeded(format!(
                "rendered output exceeds total budget {MAX_TOTAL_PNG_BYTES} bytes"
            )));
        }
        let data = std::fs::read(&path).map_err(PdfError::Io)?;
        let (width_px, height_px) =
            rendered_dimensions(opts.format, &data).ok_or_else(|| PdfError::ExternalTool {
                tool: RENDER_TOOL.to_string(),
                detail: format!("page {page} output could not be decoded for dimensions"),
            })?;
        out.push(RenderedPage {
            page,
            width_px,
            height_px,
            format: opts.format,
            data,
        });
    }

    if out.len() != wanted.len() {
        let missing: Vec<u32> = wanted
            .iter()
            .filter(|p| !out.iter().any(|i| &i.page == *p))
            .copied()
            .collect();
        return Err(PdfError::ExternalTool {
            tool: RENDER_TOOL.to_string(),
            detail: format!("no output produced for page(s) {missing:?}"),
        });
    }
    Ok(out)
}

/// Decode pixel dimensions from rendered output. PNG reads the IHDR
/// directly; JPEG/TIFF go through the `image` crate.
fn rendered_dimensions(format: RasterFormat, data: &[u8]) -> Option<(u32, u32)> {
    match format {
        RasterFormat::Png => png_dimensions(data),
        RasterFormat::Jpeg => image::load_from_memory_with_format(data, image::ImageFormat::Jpeg)
            .ok()
            .map(|img| (img.width(), img.height())),
        RasterFormat::Tiff => image::load_from_memory_with_format(data, image::ImageFormat::Tiff)
            .ok()
            .map(|img| (img.width(), img.height())),
    }
}

/// Extract the page number from a `page-<n>.png` filename.
fn png_page_number(path: &Path) -> Option<u32> {
    let stem = path.file_stem()?.to_str()?;
    let digits = stem.rsplit('-').next()?;
    if digits.is_empty() || !digits.bytes().all(|b| b.is_ascii_digit()) {
        return None;
    }
    digits.parse().ok()
}

/// Read width/height from a PNG's IHDR chunk (bytes 16..24, big-endian).
fn png_dimensions(png: &[u8]) -> Option<(u32, u32)> {
    const PNG_SIG: &[u8; 8] = b"\x89PNG\r\n\x1a\n";
    if png.len() < 24 || &png[..8] != PNG_SIG {
        return None;
    }
    // IHDR must be the first chunk: length(4) "IHDR"(4) width(4) height(4)
    if &png[12..16] != b"IHDR" {
        return None;
    }
    let width = u32::from_be_bytes(png[16..20].try_into().ok()?);
    let height = u32::from_be_bytes(png[20..24].try_into().ok()?);
    Some((width, height))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn png_dimensions_reads_ihdr() {
        // Minimal PNG signature + IHDR length/type + 1x1 dims.
        let mut png = Vec::new();
        png.extend_from_slice(b"\x89PNG\r\n\x1a\n");
        png.extend_from_slice(&13u32.to_be_bytes());
        png.extend_from_slice(b"IHDR");
        png.extend_from_slice(&640u32.to_be_bytes());
        png.extend_from_slice(&480u32.to_be_bytes());
        png.extend_from_slice(&[8, 6, 0, 0, 0]);
        png.extend_from_slice(&[0, 0, 0, 0]);
        assert_eq!(png_dimensions(&png), Some((640, 480)));
        assert_eq!(png_dimensions(b"not a png"), None);
    }

    #[test]
    fn png_page_number_parses_pdftoppm_names() {
        assert_eq!(png_page_number(Path::new("/tmp/x/page-12.png")), Some(12));
        assert_eq!(png_page_number(Path::new("page-1.png")), Some(1));
        assert_eq!(png_page_number(Path::new("page.png")), None);
        assert_eq!(png_page_number(Path::new("page-x.png")), None);
    }

    #[test]
    fn checked_dpi_bounds() {
        assert_eq!(checked_dpi(None).unwrap(), DEFAULT_DPI);
        assert_eq!(checked_dpi(Some(300)).unwrap(), 300);
        assert!(checked_dpi(Some(10)).is_err());
        assert!(checked_dpi(Some(5000)).is_err());
    }
}
