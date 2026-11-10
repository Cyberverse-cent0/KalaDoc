//! Registry-facing [`ProcessingEngine`] adapter for the security engine.
//!
//! The security engine is not a document transformer — it validates and
//! gates. Exposing it through the engine registry lets the CLI/MCP layers
//! discover its capabilities (`validate` over the sniffable formats) and
//! report health uniformly with the real processing engines.

use std::future::Future;
use std::pin::Pin;

use crate::crates::core::engine::{
    EngineCapabilities, EngineHealth, EngineHealthStatus, EngineKind, EngineLimits,
    EngineOperation, ProcessingEngine,
};

/// Security engine exposed to the [`crate::crates::engine_registry`].
///
/// Stateless: all budgets come from [`crate::crates::security_engine::ResourceLimits`]
/// at the call site, so a single shared instance serves every request.
#[derive(Debug, Default)]
pub struct SecurityEngine;

impl SecurityEngine {
    /// Create the engine. No configuration needed — limits are enforced
    /// per-call by [`crate::crates::security_engine::ResourceLimits`].
    pub fn new() -> Self {
        Self
    }

    /// Validate untrusted bytes as a PDF under the given size cap.
    ///
    /// Equivalent to the free function
    /// [`validate_pdf`](crate::crates::security_engine::validate_pdf);
    /// provided as a method so callers holding a `SecurityEngine` (or
    /// the `ProcessingEngine` trait object) have a uniform entry point.
    pub fn validate(
        &self,
        data: &[u8],
        max_bytes: u64,
    ) -> crate::crates::security_engine::SecurityResult<()> {
        crate::crates::security_engine::validate_pdf(data, max_bytes)
    }
}

impl ProcessingEngine for SecurityEngine {
    fn name(&self) -> &'static str {
        "security"
    }

    fn version(&self) -> &'static str {
        env!("CARGO_PKG_VERSION")
    }

    fn kind(&self) -> EngineKind {
        EngineKind::Security
    }

    fn capabilities(&self) -> EngineCapabilities {
        let limits = crate::crates::security_engine::ResourceLimits::default();
        EngineCapabilities {
            // `validate` is the only operation: this engine checks and
            // gates uploads, it does not transform documents.
            operations: vec![EngineOperation::Validate],
            input_formats: vec![
                "pdf".to_string(),
                "png".to_string(),
                "jpeg".to_string(),
                "zip".to_string(),
                "gif".to_string(),
            ],
            output_formats: Vec::new(),
            limits: EngineLimits {
                max_input_bytes: Some(limits.max_upload_bytes),
                max_pages: limits.max_pages,
                max_pixels: limits.max_pixels,
                max_outputs: Some(limits.max_outputs),
            },
        }
    }

    fn health<'a>(&'a self) -> Pin<Box<dyn Future<Output = EngineHealth> + Send + 'a>> {
        Box::pin(async move {
            // Pure in-process checks — healthy whenever the crate is
            // compiled in, which it always is.
            EngineHealth {
                status: EngineHealthStatus::Healthy,
                message: None,
            }
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn advertises_security_capabilities() {
        let e = SecurityEngine::new();
        assert_eq!(e.name(), "security");
        assert_eq!(e.kind(), EngineKind::Security);
        let caps = e.capabilities();
        assert_eq!(caps.operations, vec![EngineOperation::Validate]);
        assert!(caps.input_formats.contains(&"pdf".to_string()));
        assert!(caps.limits.max_input_bytes.is_some());
    }

    // -- Full validation pipeline --------------------------------------
    //
    // `validate_upload` composes the filename, size, and magic-byte
    // stages this crate provides. These tests exercise the whole
    // pipeline — including the order in which rejection stages fire —
    // plus the `SecurityEngine::validate` method and the advertised
    // limits.

    use crate::crates::security_engine::{
        ResourceLimits, SecurityError, SniffedFormat, sniff_format, validate_upload,
    };

    const PDF: &[u8] = b"%PDF-1.7\n1 0 obj\n<<>>\nendobj\n%%EOF";
    const PNG: &[u8] = b"\x89PNG\r\n\x1a\n....";
    const ZIP: &[u8] = b"PK\x03\x04\x14\x00....";
    const MAX: u64 = 10 * 1024 * 1024;

    fn limits() -> ResourceLimits {
        ResourceLimits::default()
    }

    #[test]
    fn engine_validate_method_runs_the_pdf_gate() {
        let e = SecurityEngine::new();
        assert!(e.validate(PDF, MAX).is_ok());

        let big = vec![b'%'; 100];
        assert!(matches!(
            e.validate(&big, 50),
            Err(SecurityError::FileTooLarge { size: 100, max: 50 })
        ));
        assert!(matches!(
            e.validate(b"not a pdf", MAX),
            Err(SecurityError::NotPdf)
        ));
        assert!(matches!(
            e.validate(b"", MAX),
            Err(SecurityError::EmptyFile)
        ));
    }

    #[test]
    fn pipeline_accepts_a_valid_upload() {
        let report = validate_upload(
            "report.pdf",
            PDF.len() as u64,
            PDF,
            &limits(),
            Some(SniffedFormat::Pdf),
        )
        .unwrap();
        assert_eq!(report.sanitized_filename, "report.pdf");
        assert_eq!(report.format, SniffedFormat::Pdf);
        assert_eq!(report.size_bytes, PDF.len() as u64);
    }

    #[test]
    fn pipeline_rejects_oversize_before_anything_else() {
        let mut l = limits();
        l.max_upload_bytes = 8;
        // A hostile filename and wrong content can't get past the
        // size gate — it runs first.
        let err = validate_upload("../evil", 9, ZIP, &l, Some(SniffedFormat::Pdf)).unwrap_err();
        assert!(matches!(
            err,
            SecurityError::FileTooLarge { size: 9, max: 8 }
        ));
    }

    #[test]
    fn pipeline_rejects_size_mismatch_before_filename_and_sniff() {
        // Declared size lies in both directions; the filename is also
        // hostile — the mismatch still wins.
        for declared in [4u64, PDF.len() as u64 + 1] {
            let err = validate_upload(
                "../../etc/passwd",
                declared,
                PDF,
                &limits(),
                Some(SniffedFormat::Pdf),
            )
            .unwrap_err();
            assert!(matches!(err, SecurityError::SizeMismatch { .. }));
        }
    }

    #[test]
    fn pipeline_rejects_empty_file_before_filename() {
        let err = validate_upload("../evil", 0, b"", &limits(), None).unwrap_err();
        assert!(matches!(err, SecurityError::EmptyFile));
    }

    #[test]
    fn pipeline_rejects_bad_filename_before_sniffing() {
        // The bytes would also fail the expected-format check, but the
        // filename stage fires first.
        let err = validate_upload(
            "CON",
            ZIP.len() as u64,
            ZIP,
            &limits(),
            Some(SniffedFormat::Pdf),
        )
        .unwrap_err();
        assert!(matches!(err, SecurityError::InvalidFilename { .. }));
    }

    #[test]
    fn pipeline_rejects_sniff_mismatch_last() {
        // Clean filename, honest size — only the magic-byte check can
        // still reject the upload.
        let err = validate_upload(
            "innocent.pdf",
            ZIP.len() as u64,
            ZIP,
            &limits(),
            Some(SniffedFormat::Pdf),
        )
        .unwrap_err();
        assert!(matches!(
            err,
            SecurityError::ContentTypeMismatch {
                expected: SniffedFormat::Pdf,
                found: SniffedFormat::Zip
            }
        ));

        // The same bytes are fine when the expectation matches —
        // extensions never gate acceptance, content does.
        let report = validate_upload(
            "archive.zip",
            ZIP.len() as u64,
            ZIP,
            &limits(),
            Some(SniffedFormat::Zip),
        )
        .unwrap();
        assert_eq!(report.format, SniffedFormat::Zip);

        // PNG content against a PDF expectation also mismatches.
        let err = validate_upload(
            "photo.pdf",
            PNG.len() as u64,
            PNG,
            &limits(),
            Some(SniffedFormat::Pdf),
        )
        .unwrap_err();
        assert!(matches!(
            err,
            SecurityError::ContentTypeMismatch {
                expected: SniffedFormat::Pdf,
                found: SniffedFormat::Png
            }
        ));
    }

    #[test]
    fn pipeline_limit_breaches_map_to_their_error_variants() {
        // Beyond the upload-size gate, each budget has its own variant.
        let mut l = limits();
        l.max_pages = Some(2);
        assert!(matches!(
            l.check_pages(3),
            Err(SecurityError::TooManyPages { pages: 3, max: 2 })
        ));
        l.max_pixels = Some(100);
        assert!(matches!(
            l.check_pixels(101),
            Err(SecurityError::TooManyPixels {
                pixels: 101,
                max: 100
            })
        ));
        l.max_outputs = 4;
        assert!(matches!(
            l.check_outputs(5),
            Err(SecurityError::TooManyOutputs { count: 5, max: 4 })
        ));
        l.max_operation_bytes = 10;
        assert!(matches!(
            l.check_operation_size(11),
            Err(SecurityError::OperationTooLarge { size: 11, max: 10 })
        ));
        // Boundaries pass.
        l.max_pages = Some(2);
        assert!(l.check_pages(2).is_ok());
        l.max_outputs = 4;
        assert!(l.check_outputs(4).is_ok());
    }

    #[test]
    fn capabilities_limits_mirror_resource_limits() {
        let caps = SecurityEngine::new().capabilities();
        let rl = ResourceLimits::default();
        assert_eq!(caps.limits.max_input_bytes, Some(rl.max_upload_bytes));
        assert_eq!(caps.limits.max_pages, rl.max_pages);
        assert_eq!(caps.limits.max_pixels, rl.max_pixels);
        assert_eq!(caps.limits.max_outputs, Some(rl.max_outputs));

        // Every advertised input format must be one the sniffer can
        // actually detect — capabilities stay honest.
        let cases: &[(&str, &[u8], SniffedFormat)] = &[
            ("pdf", PDF, SniffedFormat::Pdf),
            ("png", PNG, SniffedFormat::Png),
            ("jpeg", &[0xFF, 0xD8, 0xFF, 0xE0], SniffedFormat::Jpeg),
            ("zip", ZIP, SniffedFormat::Zip),
            ("gif", b"GIF89a....", SniffedFormat::Gif),
        ];
        for (name, bytes, expected) in cases {
            assert!(
                caps.input_formats.contains(&name.to_string()),
                "advertised format {name} missing sample check"
            );
            assert_eq!(sniff_format(bytes), *expected);
        }
    }
}
