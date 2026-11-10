//! Poppler backend: pdf → png/jpeg via `pdftoppm`, pdf → txt via
//! `pdftotext`, pdf → html via `pdftohtml`.
//!
//! Each tool is probed independently at detection time; the backend
//! is registered when at least one is present and only advertises the
//! pairs its tools can serve.

use std::ffi::OsStr;
use std::fs;
use std::path::PathBuf;

use super::util;
use crate::crates::conversion_engine::{
    BackendAvailability, ConversionBackend, ConvertError, ConvertOutput, ConvertRequest,
    ConvertResult, Format,
};

/// Raster DPI used when the request does not specify one (matches
/// pdftoppm's own default).
const DEFAULT_DPI: u32 = 150;
/// Upper bound on requested DPI — beyond this memory/time cost is
/// unreasonable for a single page rasterization.
const MAX_DPI: u32 = 1200;

/// Poppler backend covering whichever of `pdftoppm`, `pdftotext` and
/// `pdftohtml` are installed.
pub struct PopplerBackend {
    availability: BackendAvailability,
    /// `pdftoppm` for pdf → png/jpeg.
    pdftoppm: Option<PathBuf>,
    /// `pdftotext` for pdf → txt.
    pdftotext: Option<PathBuf>,
    /// `pdftohtml` for pdf → html.
    pdftohtml: Option<PathBuf>,
}

impl PopplerBackend {
    /// Probe PATH once per tool; `None` when none of them exist.
    pub fn detect(path_env: &OsStr) -> Option<Self> {
        let pdftoppm = util::find_in_path("pdftoppm", path_env);
        let pdftotext = util::find_in_path("pdftotext", path_env);
        let pdftohtml = util::find_in_path("pdftohtml", path_env);
        if pdftoppm.is_none() && pdftotext.is_none() && pdftohtml.is_none() {
            return None;
        }
        // All poppler tools ship together and share a version line;
        // probe whichever binary was found first.
        let version = [&pdftoppm, &pdftotext, &pdftohtml]
            .into_iter()
            .flatten()
            .find_map(|p| util::probe_version(p));
        Some(Self {
            availability: BackendAvailability::Available { version },
            pdftoppm,
            pdftotext,
            pdftohtml,
        })
    }

    /// Warn about options the text/html paths cannot honor.
    fn text_option_warnings(&self, req: &ConvertRequest, warnings: &mut Vec<String>) {
        if req.options.dpi.is_some() {
            warnings.push("dpi is ignored by the poppler backend".to_string());
        }
        if req.options.quality.is_some() {
            warnings.push("quality is ignored by the poppler backend".to_string());
        }
        if req.options.page_size.is_some() {
            warnings.push("page_size is ignored by the poppler backend".to_string());
        }
        if !req.options.params.is_null() {
            warnings.push("params is ignored by the poppler backend".to_string());
        }
    }

    /// pdf → txt via `pdftotext -enc UTF-8`, text on stdout.
    fn pdf_to_txt(
        &self,
        req: &ConvertRequest,
        warnings: &mut Vec<String>,
    ) -> ConvertResult<Vec<u8>> {
        let program = self.pdftotext.as_ref().expect("supports() gated");
        self.text_option_warnings(req, warnings);

        let tmp = util::TempDir::new("poppler")?;
        let input_path = tmp.file("input.pdf");
        fs::write(&input_path, &req.input)?;

        let args = vec![
            "-enc".to_string(),
            "UTF-8".to_string(),
            "-q".to_string(),
            input_path.to_string_lossy().into_owned(),
            "-".to_string(), // stdout
        ];
        let output = util::run_tool(program, &args, util::COMMAND_TIMEOUT, "poppler")?;
        if !output.status.success() {
            return Err(ConvertError::BackendFailed {
                backend: "poppler".to_string(),
                message: util::failure_message(
                    &output,
                    &format!("pdftotext exited with {}", output.status),
                ),
            });
        }
        if output.stdout.is_empty() {
            warnings.push(
                "no extractable text found (document may be scanned/images only)".to_string(),
            );
        }
        Ok(output.stdout)
    }

    /// pdf → html via `pdftohtml -s -stdout -dataurls` (single
    /// self-contained XHTML document, images inlined as data URLs).
    fn pdf_to_html(
        &self,
        req: &ConvertRequest,
        warnings: &mut Vec<String>,
    ) -> ConvertResult<Vec<u8>> {
        let program = self.pdftohtml.as_ref().expect("supports() gated");
        self.text_option_warnings(req, warnings);

        let tmp = util::TempDir::new("poppler")?;
        let input_path = tmp.file("input.pdf");
        fs::write(&input_path, &req.input)?;

        let args = vec![
            "-q".to_string(),
            "-s".to_string(), // single document covering all pages
            "-stdout".to_string(),
            "-dataurls".to_string(), // inline images as data: URLs
            input_path.to_string_lossy().into_owned(),
        ];
        let output = util::run_tool(program, &args, util::COMMAND_TIMEOUT, "poppler")?;
        if !output.status.success() {
            return Err(ConvertError::BackendFailed {
                backend: "poppler".to_string(),
                message: util::failure_message(
                    &output,
                    &format!("pdftohtml exited with {}", output.status),
                ),
            });
        }
        // pdftohtml derives the document <title> from the input path,
        // which would leak our temp-dir location; rewrite it to a
        // neutral name. Only the exact temp path is replaced.
        let bytes = scrub_temp_path(output.stdout, &input_path);
        if bytes.is_empty() {
            return Err(ConvertError::BackendFailed {
                backend: "poppler".to_string(),
                message: "pdftohtml produced empty output".to_string(),
            });
        }
        Ok(bytes)
    }
}

impl ConversionBackend for PopplerBackend {
    fn name(&self) -> &'static str {
        "poppler"
    }

    fn availability(&self) -> BackendAvailability {
        self.availability.clone()
    }

    fn supports(&self, from: Format, to: Format) -> bool {
        if from != Format::Pdf {
            return false;
        }
        match to {
            Format::Png | Format::Jpeg => self.pdftoppm.is_some(),
            Format::Txt => self.pdftotext.is_some(),
            Format::Html => self.pdftohtml.is_some(),
            _ => false,
        }
    }

    fn convert(&self, req: &ConvertRequest) -> ConvertResult<ConvertOutput> {
        util::check_input(&req.input)?;
        if !self.supports(req.input_format, req.output_format) {
            return Err(ConvertError::UnsupportedConversion {
                from: req.input_format,
                to: req.output_format,
            });
        }

        let mut warnings = Vec::new();
        match req.output_format {
            Format::Txt => {
                let bytes = self.pdf_to_txt(req, &mut warnings)?;
                return Ok(util::success(bytes, Format::Txt, "poppler", warnings));
            }
            Format::Html => {
                let bytes = self.pdf_to_html(req, &mut warnings)?;
                return Ok(util::success(bytes, Format::Html, "poppler", warnings));
            }
            _ => {}
        }

        let program = self.pdftoppm.as_ref().expect("supports() gated");

        // Multi-page PDFs: our contract returns a single buffer, so we
        // rasterize page 1 only and say so.
        match crate::crates::pdf_engine::PdfEngine::new().inspect(&req.input) {
            Ok(info) if info.page_count > 1 => warnings.push(format!(
                "PDF has {} pages; only the first page was converted",
                info.page_count
            )),
            Ok(_) => {}
            Err(_) => warnings
                .push("could not determine PDF page count; converted first page only".to_string()),
        }

        let dpi = match req.options.dpi {
            Some(0) | None => DEFAULT_DPI,
            Some(d) if d > MAX_DPI => {
                warnings.push(format!("dpi {d} clamped to {MAX_DPI}"));
                MAX_DPI
            }
            Some(d) => d,
        };

        let raster_flag = match req.output_format {
            Format::Png => "-png",
            Format::Jpeg => {
                match req.options.quality {
                    Some(q) if (1..=100).contains(&q) => {
                        // handled below via -jpegopt
                    }
                    Some(q) => {
                        warnings.push(format!("jpeg quality {q} out of range 1-100; ignored"))
                    }
                    None => {}
                }
                "-jpeg"
            }
            _ => unreachable!("supports() gated"),
        };

        if req.output_format == Format::Png && req.options.quality.is_some() {
            warnings.push("quality is ignored for PNG output".to_string());
        }
        if req.options.page_size.is_some() {
            warnings.push("page_size is ignored by the poppler backend".to_string());
        }
        if !req.options.params.is_null() {
            warnings.push("params is ignored by the poppler backend".to_string());
        }

        let tmp = util::TempDir::new("poppler")?;
        let input_path = tmp.file("input.pdf");
        let out_prefix = tmp.file("page");
        fs::write(&input_path, &req.input)?;

        let mut args = vec![
            raster_flag.to_string(),
            "-r".to_string(),
            dpi.to_string(),
            "-f".to_string(),
            "1".to_string(),
            "-l".to_string(),
            "1".to_string(),
        ];
        if req.output_format == Format::Jpeg {
            if let Some(q) = req.options.quality {
                if (1..=100).contains(&q) {
                    args.push("-jpegopt".to_string());
                    args.push(format!("quality={q}"));
                }
            }
        }
        args.push(input_path.to_string_lossy().into_owned());
        args.push(out_prefix.to_string_lossy().into_owned());

        let output = util::run_tool(program, &args, util::COMMAND_TIMEOUT, "poppler")?;
        if !output.status.success() {
            return Err(ConvertError::BackendFailed {
                backend: "poppler".to_string(),
                message: util::failure_message(
                    &output,
                    &format!("pdftoppm exited with {}", output.status),
                ),
            });
        }

        // pdftoppm names output `<prefix>-<page>.<ext>` (we forced a
        // single page, so exactly one file should exist).
        let bytes = read_raster_output(tmp.path(), "page")?;
        if bytes.is_empty() {
            return Err(ConvertError::BackendFailed {
                backend: "poppler".to_string(),
                message: "pdftoppm produced an empty output file".to_string(),
            });
        }

        Ok(util::success(bytes, req.output_format, "poppler", warnings))
    }
}

/// pdftohtml uses the input path (minus extension) as the document
/// `<title>`; since our input lives in a temp dir, rewrite occurrences
/// of that path to a neutral name so temp locations don't leak into
/// the returned HTML.
fn scrub_temp_path(html: Vec<u8>, input_path: &std::path::Path) -> Vec<u8> {
    let text = String::from_utf8_lossy(&html);
    let path_str = input_path.to_string_lossy();
    // pdftohtml strips the extension for the title.
    let stem: &str = path_str.strip_suffix(".pdf").unwrap_or(path_str.as_ref());
    if stem.is_empty() || !text.contains(stem) {
        return html;
    }
    text.replace(stem, "document").into_bytes()
}

/// Find and read the single raster file pdftoppm produced under
/// `dir` for prefix `stem` (`<stem>-1.png`, `<stem>-1.jpg`, ...).
fn read_raster_output(dir: &std::path::Path, stem: &str) -> ConvertResult<Vec<u8>> {
    let prefix = format!("{stem}-1.");
    let mut candidates: Vec<PathBuf> = fs::read_dir(dir)
        .map_err(ConvertError::Io)?
        .filter_map(|e| e.ok())
        .map(|e| e.path())
        .filter(|p| {
            p.file_name()
                .and_then(|n| n.to_str())
                .map(|n| n.starts_with(&prefix))
                .unwrap_or(false)
        })
        .collect();
    candidates.sort();
    let path = candidates
        .into_iter()
        .next()
        .ok_or_else(|| ConvertError::BackendFailed {
            backend: "poppler".to_string(),
            message: "pdftoppm produced no raster output file".to_string(),
        })?;
    Ok(fs::read(&path)?)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::crates::conversion_engine::ConvertOptions;

    fn backend() -> PopplerBackend {
        PopplerBackend {
            availability: BackendAvailability::Available { version: None },
            pdftoppm: Some(PathBuf::from("/nonexistent/pdftoppm")),
            pdftotext: Some(PathBuf::from("/nonexistent/pdftotext")),
            pdftohtml: Some(PathBuf::from("/nonexistent/pdftohtml")),
        }
    }

    #[test]
    fn supports_pdf_to_images_text_html() {
        let b = backend();
        assert!(b.supports(Format::Pdf, Format::Png));
        assert!(b.supports(Format::Pdf, Format::Jpeg));
        assert!(b.supports(Format::Pdf, Format::Txt));
        assert!(b.supports(Format::Pdf, Format::Html));
        assert!(!b.supports(Format::Pdf, Format::Pdf));
        assert!(!b.supports(Format::Pdf, Format::Markdown));
        assert!(!b.supports(Format::Png, Format::Pdf));
        assert!(!b.supports(Format::Markdown, Format::Png));
        assert!(!b.supports(Format::Html, Format::Txt));
    }

    #[test]
    fn supports_requires_each_tool() {
        // Only pdftotext present → only pdf→txt advertised.
        let b = PopplerBackend {
            availability: BackendAvailability::Available { version: None },
            pdftoppm: None,
            pdftotext: Some(PathBuf::from("/nonexistent/pdftotext")),
            pdftohtml: None,
        };
        assert!(b.supports(Format::Pdf, Format::Txt));
        assert!(!b.supports(Format::Pdf, Format::Png));
        assert!(!b.supports(Format::Pdf, Format::Html));
        assert!(!b.supports(Format::Pdf, Format::Markdown));
    }

    #[test]
    fn scrub_temp_path_rewrites_title() {
        let html =
            br#"<html><head><title>/tmp/kaladoc-ext-poppler-1-2/input</title></head></html>"#;
        let out = scrub_temp_path(
            html.to_vec(),
            std::path::Path::new("/tmp/kaladoc-ext-poppler-1-2/input.pdf"),
        );
        let text = String::from_utf8(out).unwrap();
        assert!(text.contains("<title>document</title>"), "got: {text}");
        assert!(!text.contains("kaladoc-ext"));
        // Non-matching input is returned untouched.
        let untouched = scrub_temp_path(b"abc".to_vec(), std::path::Path::new("/x/y.pdf"));
        assert_eq!(untouched, b"abc");
    }

    #[test]
    fn convert_rejects_bad_input() {
        let b = backend();
        let req = ConvertRequest {
            input_format: Format::Markdown,
            output_format: Format::Png,
            input: vec![1],
            options: ConvertOptions::default(),
        };
        assert!(matches!(
            b.convert(&req),
            Err(ConvertError::UnsupportedConversion { .. })
        ));
        let req = ConvertRequest {
            input_format: Format::Pdf,
            output_format: Format::Png,
            input: Vec::new(),
            options: ConvertOptions::default(),
        };
        assert!(matches!(
            b.convert(&req),
            Err(ConvertError::InvalidInput { .. })
        ));
    }

    #[test]
    fn read_raster_output_picks_page_one() {
        let dir = std::env::temp_dir().join(format!(
            "kaladoc-test-pp-{}-{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .map(|d| d.as_nanos())
                .unwrap_or(0)
        ));
        fs::create_dir_all(&dir).unwrap();
        fs::write(dir.join("page-1.png"), b"pngbytes").unwrap();
        fs::write(dir.join("page-2.png"), b"other").unwrap();
        let out: ConvertResult<Vec<u8>> = read_raster_output(&dir, "page");
        assert_eq!(out.unwrap(), b"pngbytes");
        fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn detect_finds_pdftoppm_stub() {
        let dir = std::env::temp_dir().join(format!(
            "kaladoc-test-ppd-{}-{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .map(|d| d.as_nanos())
                .unwrap_or(0)
        ));
        fs::create_dir_all(&dir).unwrap();
        let stub = dir.join("pdftoppm");
        fs::write(&stub, "#!/bin/sh\necho 'pdftoppm version 24.02'\n").unwrap();
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            fs::set_permissions(&stub, fs::Permissions::from_mode(0o755)).unwrap();
        }
        let path_env = std::ffi::OsString::from(dir.as_os_str());
        let b = PopplerBackend::detect(&path_env).expect("stub should be found");
        assert!(b.supports(Format::Pdf, Format::Jpeg));
        fs::remove_dir_all(&dir).ok();
    }
}
