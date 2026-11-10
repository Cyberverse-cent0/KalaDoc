//! Pandoc backend: markup/office formats → pdf/txt/markdown/html.

use std::ffi::OsStr;
use std::path::PathBuf;

use super::util;
use crate::crates::conversion_engine::{
    BackendAvailability, ConversionBackend, ConvertError, ConvertOutput, ConvertRequest,
    ConvertResult, Format,
};

/// Pandoc input formats we advertise (everything in our [`Format`]
/// enum pandoc can actually read).
const PANDOC_READABLE: &[Format] = &[
    Format::Txt,
    Format::Markdown,
    Format::Html,
    Format::Docx,
    Format::Odt,
    Format::Epub,
];

/// Pandoc writer names pandoc can always produce without a PDF engine.
fn pandoc_format(format: Format) -> Option<&'static str> {
    match format {
        Format::Txt => Some("plain"),
        Format::Markdown => Some("markdown"),
        Format::Html => Some("html"),
        Format::Docx => Some("docx"),
        Format::Odt => Some("odt"),
        Format::Epub => Some("epub"),
        _ => None,
    }
}

/// Whether any PDF-producing engine pandoc could delegate to exists.
/// pandoc picks its own `--pdf-engine` default; we only need to know
/// *some* engine is installed before advertising `→ Pdf` pairs.
fn pdf_engine_available(path_env: &OsStr) -> bool {
    [
        "pdflatex",
        "xelatex",
        "tectonic",
        "weasyprint",
        "typst",
        "context",
    ]
    .iter()
    .any(|tool| util::find_in_path(tool, path_env).is_some())
}

/// Pandoc CLI backend.
pub struct PandocBackend {
    availability: BackendAvailability,
    /// Path to the pandoc binary (resolved at detection time).
    program: PathBuf,
    /// Whether a LaTeX/typst/weasyprint engine exists for `→ pdf`.
    pdf_engine: bool,
}

impl PandocBackend {
    /// Probe PATH once; `None` when pandoc is not installed.
    pub fn detect(path_env: &OsStr) -> Option<Self> {
        let program = util::find_in_path("pandoc", path_env)?;
        let version = util::probe_version(&program);
        Some(Self {
            availability: BackendAvailability::Available { version },
            program,
            pdf_engine: pdf_engine_available(path_env),
        })
    }

    /// Whether `to` is a pandoc-writable output we advertise.
    fn output_supported(&self, to: Format) -> bool {
        match to {
            Format::Pdf => self.pdf_engine,
            _ => pandoc_format(to).is_some(),
        }
    }
}

impl ConversionBackend for PandocBackend {
    fn name(&self) -> &'static str {
        "pandoc"
    }

    fn availability(&self) -> BackendAvailability {
        self.availability.clone()
    }

    fn supports(&self, from: Format, to: Format) -> bool {
        PANDOC_READABLE.contains(&from) && self.output_supported(to)
    }

    fn convert(&self, req: &ConvertRequest) -> ConvertResult<ConvertOutput> {
        util::check_input(&req.input)?;
        let from = pandoc_format(req.input_format).ok_or(ConvertError::UnsupportedConversion {
            from: req.input_format,
            to: req.output_format,
        })?;
        if !self.supports(req.input_format, req.output_format) {
            return Err(ConvertError::UnsupportedConversion {
                from: req.input_format,
                to: req.output_format,
            });
        }
        // `→ pdf` goes through pandoc's default pdf-engine; everything
        // else is a plain writer target.
        let to = if req.output_format == Format::Pdf {
            "pdf"
        } else {
            pandoc_format(req.output_format).expect("supports() gated")
        };

        let mut warnings = Vec::new();
        collect_option_warnings(&req.options, &mut warnings);

        let tmp = util::TempDir::new("pandoc")?;
        let input_path = tmp.file(&format!("input.{}", util::format_ext(req.input_format)));
        let output_path = tmp.file(&format!("output.{}", util::format_ext(req.output_format)));
        std::fs::write(&input_path, &req.input)?;

        let mut args = vec![
            "--quiet".to_string(),
            "-f".to_string(),
            from.to_string(),
            "-t".to_string(),
            to.to_string(),
            "--output".to_string(),
            output_path.to_string_lossy().into_owned(),
            input_path.to_string_lossy().into_owned(),
        ];

        // `params` as a flat string→string object maps cleanly onto
        // pandoc `-V key=value` variables; anything else is reported.
        match params_as_variables(&req.options.params) {
            Ok(vars) => {
                for (k, v) in vars {
                    args.push("-V".to_string());
                    args.push(format!("{k}={v}"));
                }
            }
            Err(why) => warnings.push(why),
        }

        // pandoc's EPUB writer refuses to run without a title in the
        // metadata; supply one (params.title wins, else a placeholder).
        if req.output_format == Format::Epub {
            let title = req
                .options
                .params
                .get("title")
                .and_then(|v| v.as_str())
                .filter(|s| !s.trim().is_empty())
                .unwrap_or("Untitled document");
            args.push("--metadata".to_string());
            args.push(format!("title={title}"));
        }

        let output = util::run_tool(&self.program, &args, util::COMMAND_TIMEOUT, "pandoc")?;
        if !output.status.success() {
            return Err(ConvertError::BackendFailed {
                backend: "pandoc".to_string(),
                message: util::failure_message(
                    &output,
                    &format!("pandoc exited with {}", output.status),
                ),
            });
        }

        let bytes = std::fs::read(&output_path).map_err(|e| ConvertError::BackendFailed {
            backend: "pandoc".to_string(),
            message: format!("could not read pandoc output: {e}"),
        })?;
        if bytes.is_empty() {
            return Err(ConvertError::BackendFailed {
                backend: "pandoc".to_string(),
                message: "pandoc produced an empty output file".to_string(),
            });
        }

        Ok(util::success(bytes, req.output_format, "pandoc", warnings))
    }
}

/// Options that pandoc cannot honor become warnings — never silently
/// dropped. `params` string maps are handled separately (mapped to
/// `-V`), so they are excluded here.
fn collect_option_warnings(
    options: &crate::crates::conversion_engine::ConvertOptions,
    warnings: &mut Vec<String>,
) {
    if options.dpi.is_some() {
        warnings.push("dpi is ignored by the pandoc backend".to_string());
    }
    if options.quality.is_some() {
        warnings.push("quality is ignored by the pandoc backend".to_string());
    }
    if options.page_size.is_some() {
        warnings.push("page_size is not mapped onto pandoc variables; ignored".to_string());
    }
    if !options.params.is_null() && params_as_variables(&options.params).is_err() {
        warnings.push("params could not be mapped to pandoc variables; ignored".to_string());
    }
}

/// Interpret `options.params` as pandoc `-V` variables. Accepts a
/// flat JSON object of string values (capped); `null` yields none.
fn params_as_variables(params: &serde_json::Value) -> Result<Vec<(String, String)>, String> {
    if params.is_null() {
        return Ok(Vec::new());
    }
    let obj = params
        .as_object()
        .ok_or_else(|| "params is not a JSON object".to_string())?;
    if obj.len() > 32 {
        return Err("params has more than 32 entries".to_string());
    }
    let mut vars = Vec::with_capacity(obj.len());
    for (k, v) in obj {
        let v = v
            .as_str()
            .ok_or_else(|| format!("param '{k}' is not a string"))?;
        vars.push((k.clone(), v.to_string()));
    }
    Ok(vars)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::crates::conversion_engine::BackendAvailability;

    fn backend(pdf_engine: bool) -> PandocBackend {
        PandocBackend {
            availability: BackendAvailability::Available { version: None },
            program: PathBuf::from("/nonexistent/pandoc"),
            pdf_engine,
        }
    }

    /// Text-ish outputs pandoc always writes (no pdf engine needed).
    const TEXT_OUTPUTS: &[Format] = &[Format::Txt, Format::Markdown, Format::Html];

    #[test]
    fn supports_matrix_without_pdf_engine() {
        let b = backend(false);
        for &from in PANDOC_READABLE {
            for &to in TEXT_OUTPUTS {
                assert!(b.supports(from, to), "{from:?} -> {to:?}");
            }
            assert!(!b.supports(from, Format::Pdf), "{from:?} -> pdf");
            assert!(!b.supports(from, Format::Png));
            assert!(!b.supports(from, Format::Jpeg));
            // pandoc also writes docx/odt/epub without a pdf engine.
            assert!(b.supports(from, Format::Docx), "{from:?} -> docx");
            assert!(b.supports(from, Format::Odt), "{from:?} -> odt");
            assert!(b.supports(from, Format::Epub), "{from:?} -> epub");
        }
        // pandoc cannot *read* pdf/images as input at all.
        assert!(!b.supports(Format::Pdf, Format::Txt));
        assert!(!b.supports(Format::Png, Format::Markdown));
        assert!(!b.supports(Format::Jpeg, Format::Html));
        // pandoc reads and writes office formats and epub too.
        assert!(b.supports(Format::Docx, Format::Odt));
        assert!(b.supports(Format::Odt, Format::Docx));
        assert!(b.supports(Format::Epub, Format::Html));
        assert!(b.supports(Format::Markdown, Format::Epub));
    }

    #[test]
    fn supports_matrix_with_pdf_engine() {
        let b = backend(true);
        for &from in PANDOC_READABLE {
            for &to in TEXT_OUTPUTS {
                assert!(b.supports(from, to), "{from:?} -> {to:?}");
            }
            assert!(b.supports(from, Format::Pdf), "{from:?} -> pdf");
        }
        assert!(!b.supports(Format::Pdf, Format::Pdf));
        assert!(!b.supports(Format::Png, Format::Png));
        assert!(!b.supports(Format::Pdf, Format::Png));
    }

    #[test]
    fn convert_rejects_unsupported_and_empty() {
        let b = backend(true);
        let mut req = ConvertRequest {
            input_format: Format::Pdf,
            output_format: Format::Txt,
            input: b"%PDF".to_vec(),
            options: Default::default(),
        };
        assert!(matches!(
            b.convert(&req),
            Err(ConvertError::UnsupportedConversion { .. })
        ));
        req.input_format = Format::Markdown;
        req.input = Vec::new();
        assert!(matches!(
            b.convert(&req),
            Err(ConvertError::InvalidInput { .. })
        ));
    }

    #[test]
    fn params_mapping() {
        assert_eq!(
            params_as_variables(&serde_json::json!({"geometry": "a4paper"})).unwrap(),
            vec![("geometry".to_string(), "a4paper".to_string())]
        );
        assert!(params_as_variables(&serde_json::json!([1, 2])).is_err());
        assert!(params_as_variables(&serde_json::json!({"a": 1})).is_err());
        assert!(
            params_as_variables(&serde_json::Value::Null)
                .unwrap()
                .is_empty()
        );
    }

    // Hermetic detection test: fake PATH containing only stub tools.
    // (Also exercised from external.rs against the whole detect vec.)
    #[test]
    fn detect_finds_pandoc_on_fake_path() {
        let dir = std::env::temp_dir().join(format!(
            "kaladoc-test-pd-{}-{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .map(|d| d.as_nanos())
                .unwrap_or(0)
        ));
        std::fs::create_dir_all(&dir).unwrap();
        let stub = dir.join("pandoc");
        std::fs::write(&stub, "#!/bin/sh\necho 'pandoc 3.1.13'\n").unwrap();
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            std::fs::set_permissions(&stub, std::fs::Permissions::from_mode(0o755)).unwrap();
        }
        let path_env = std::ffi::OsString::from(dir.as_os_str());
        let b = PandocBackend::detect(&path_env).expect("pandoc stub should be found");
        assert_eq!(b.name(), "pandoc");
        assert!(b.supports(Format::Markdown, Format::Html));
        // No pdf engine in the fake PATH → no →pdf pairs.
        assert!(!b.supports(Format::Markdown, Format::Pdf));
        std::fs::remove_dir_all(&dir).ok();
    }
}
