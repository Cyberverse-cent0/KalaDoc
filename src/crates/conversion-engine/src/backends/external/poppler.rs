//! Poppler backend: pdf → png/jpeg via `pdftoppm`.

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

/// Poppler `pdftoppm` backend.
pub struct PopplerBackend {
    availability: BackendAvailability,
    program: PathBuf,
}

impl PopplerBackend {
    /// Probe PATH once; `None` when `pdftoppm` is absent.
    pub fn detect(path_env: &OsStr) -> Option<Self> {
        let program = util::find_in_path("pdftoppm", path_env)?;
        let version = util::probe_version(&program);
        Some(Self {
            availability: BackendAvailability::Available { version },
            program,
        })
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
        from == Format::Pdf && matches!(to, Format::Png | Format::Jpeg)
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

        let output = util::run_tool(&self.program, &args, util::COMMAND_TIMEOUT, "poppler")?;
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
            program: PathBuf::from("/nonexistent/pdftoppm"),
        }
    }

    #[test]
    fn supports_only_pdf_to_images() {
        let b = backend();
        assert!(b.supports(Format::Pdf, Format::Png));
        assert!(b.supports(Format::Pdf, Format::Jpeg));
        assert!(!b.supports(Format::Pdf, Format::Pdf));
        assert!(!b.supports(Format::Png, Format::Pdf));
        assert!(!b.supports(Format::Markdown, Format::Png));
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
            "kaladoc-test-pp-{}-{:?}",
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
            "kaladoc-test-ppd-{}-{:?}",
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
