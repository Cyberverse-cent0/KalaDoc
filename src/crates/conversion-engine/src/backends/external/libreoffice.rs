//! LibreOffice backend: office formats → pdf (and docx ↔ odt).

use std::ffi::OsStr;
use std::path::PathBuf;

use super::util;
use crate::crates::conversion_engine::{
    BackendAvailability, ConversionBackend, ConvertError, ConvertOutput, ConvertRequest,
    ConvertResult, Format,
};

/// Input formats LibreOffice converts well headlessly: office
/// documents (Writer/Calc/Impress) plus raster images (Draw), all of
/// which export cleanly to PDF.
const LO_INPUTS: &[Format] = &[
    Format::Docx,
    Format::Odt,
    Format::Xlsx,
    Format::Xls,
    Format::Pptx,
    Format::Ppt,
    Format::Png,
    Format::Jpeg,
];

/// Output formats we advertise. `→ pdf` always; docx↔odt is a
/// straightforward `--convert-to` and is included. PDF *import* is
/// deliberately not advertised (weak fidelity).
fn lo_target(format: Format) -> Option<&'static str> {
    match format {
        Format::Pdf => Some("pdf"),
        Format::Docx => Some("docx"),
        Format::Odt => Some("odt"),
        _ => None,
    }
}

fn output_ext(format: Format) -> &'static str {
    match format {
        Format::Pdf => "pdf",
        Format::Docx => "docx",
        Format::Odt => "odt",
        _ => unreachable!("gated by supports()"),
    }
}

/// LibreOffice backend (`soffice`, falling back to `libreoffice`).
pub struct LibreOfficeBackend {
    availability: BackendAvailability,
    /// Resolved binary path.
    program: PathBuf,
}

impl LibreOfficeBackend {
    /// Probe PATH once; `None` when neither binary exists.
    pub fn detect(path_env: &OsStr) -> Option<Self> {
        let (program, _) = ["soffice", "libreoffice"]
            .iter()
            .find_map(|n| util::find_in_path(n, path_env).map(|p| (p, *n)))?;
        let version = util::probe_version(&program);
        Some(Self {
            availability: BackendAvailability::Available { version },
            program,
        })
    }
}

impl ConversionBackend for LibreOfficeBackend {
    fn name(&self) -> &'static str {
        "libreoffice"
    }

    fn availability(&self) -> BackendAvailability {
        self.availability.clone()
    }

    fn supports(&self, from: Format, to: Format) -> bool {
        if !LO_INPUTS.contains(&from) {
            return false;
        }
        match to {
            Format::Pdf => true,
            // Same-format "conversion" is meaningless.
            Format::Docx => from == Format::Odt,
            Format::Odt => from == Format::Docx,
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
        let target = lo_target(req.output_format).expect("supports() gated");

        let mut warnings = Vec::new();
        if req.options.dpi.is_some() {
            warnings.push("dpi is ignored by the libreoffice backend".to_string());
        }
        if req.options.quality.is_some() {
            warnings.push("quality is ignored by the libreoffice backend".to_string());
        }
        if req.options.page_size.is_some() {
            warnings.push("page_size is ignored by the libreoffice backend".to_string());
        }
        if !req.options.params.is_null() {
            warnings.push("params is ignored by the libreoffice backend".to_string());
        }

        let tmp = util::TempDir::new("libreoffice")?;
        let in_ext = util::format_ext(req.input_format);
        // LibreOffice derives the output filename from the input stem.
        let input_path = tmp.file(&format!("input.{in_ext}"));
        let out_dir = tmp.file("out");
        std::fs::create_dir_all(&out_dir)?;
        std::fs::write(&input_path, &req.input)?;

        let args: Vec<String> = [
            "--headless".to_string(),
            "--norestore".to_string(),
            "--nologo".to_string(),
            "--convert-to".to_string(),
            target.to_string(),
            "--outdir".to_string(),
            out_dir.to_string_lossy().into_owned(),
            input_path.to_string_lossy().into_owned(),
        ]
        .to_vec();

        let output = util::run_tool(&self.program, &args, util::COMMAND_TIMEOUT, "libreoffice")?;
        if !output.status.success() {
            return Err(ConvertError::BackendFailed {
                backend: "libreoffice".to_string(),
                message: util::failure_message(
                    &output,
                    &format!("soffice exited with {}", output.status),
                ),
            });
        }

        // LO names the result after the input stem: input.<ext> →
        // input.<target-ext> inside --outdir.
        let produced = out_dir.join(format!("input.{}", output_ext(req.output_format)));
        let bytes = std::fs::read(&produced).map_err(|_| ConvertError::BackendFailed {
            backend: "libreoffice".to_string(),
            message: format!(
                "libreoffice did not produce expected output: {}",
                util::failure_message(&output, "no output file")
            ),
        })?;
        if bytes.is_empty() {
            return Err(ConvertError::BackendFailed {
                backend: "libreoffice".to_string(),
                message: "libreoffice produced an empty output file".to_string(),
            });
        }

        Ok(util::success(
            bytes,
            req.output_format,
            "libreoffice",
            warnings,
        ))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn backend() -> LibreOfficeBackend {
        LibreOfficeBackend {
            availability: BackendAvailability::Available { version: None },
            program: PathBuf::from("/nonexistent/soffice"),
        }
    }

    #[test]
    fn supports_matrix() {
        let b = backend();
        assert!(b.supports(Format::Docx, Format::Pdf));
        assert!(b.supports(Format::Odt, Format::Pdf));
        assert!(b.supports(Format::Docx, Format::Odt));
        assert!(b.supports(Format::Odt, Format::Docx));
        assert!(!b.supports(Format::Docx, Format::Docx));
        assert!(!b.supports(Format::Odt, Format::Odt));
        assert!(!b.supports(Format::Pdf, Format::Docx)); // weak import: not advertised
        assert!(!b.supports(Format::Markdown, Format::Pdf));
        assert!(!b.supports(Format::Txt, Format::Txt));
        // Office spreadsheets/presentations and images → pdf only.
        for &from in &[Format::Xlsx, Format::Xls, Format::Pptx, Format::Ppt] {
            assert!(b.supports(from, Format::Pdf), "{from:?} -> pdf");
            assert!(!b.supports(from, Format::Docx), "{from:?} -> docx");
            assert!(!b.supports(from, Format::Odt), "{from:?} -> odt");
        }
        assert!(b.supports(Format::Png, Format::Pdf));
        assert!(b.supports(Format::Jpeg, Format::Pdf));
        assert!(!b.supports(Format::Png, Format::Jpeg));
        assert!(!b.supports(Format::Epub, Format::Pdf)); // pandoc's job
    }

    #[test]
    fn convert_rejects_bad_input() {
        let b = backend();
        let req = ConvertRequest {
            input_format: Format::Pdf,
            output_format: Format::Docx,
            input: vec![1, 2, 3],
            options: Default::default(),
        };
        assert!(matches!(
            b.convert(&req),
            Err(ConvertError::UnsupportedConversion { .. })
        ));
        let req = ConvertRequest {
            input_format: Format::Docx,
            output_format: Format::Pdf,
            input: Vec::new(),
            options: Default::default(),
        };
        assert!(matches!(
            b.convert(&req),
            Err(ConvertError::InvalidInput { .. })
        ));
    }

    #[test]
    fn detect_finds_soffice_stub() {
        let dir = std::env::temp_dir().join(format!(
            "kaladoc-test-lo-{}-{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .map(|d| d.as_nanos())
                .unwrap_or(0)
        ));
        std::fs::create_dir_all(&dir).unwrap();
        for name in ["soffice", "libreoffice"] {
            let stub = dir.join(name);
            std::fs::write(&stub, "#!/bin/sh\necho 'LibreOffice 24.2'\n").unwrap();
            #[cfg(unix)]
            {
                use std::os::unix::fs::PermissionsExt;
                std::fs::set_permissions(&stub, std::fs::Permissions::from_mode(0o755)).unwrap();
            }
        }
        let path_env = std::ffi::OsString::from(dir.as_os_str());
        let b = LibreOfficeBackend::detect(&path_env).expect("stub should be found");
        assert!(b.supports(Format::Docx, Format::Pdf));
        std::fs::remove_dir_all(&dir).ok();
    }
}
