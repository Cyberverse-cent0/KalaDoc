//! Security engine — upload validation, sanitization, and resource-limit
//! enforcement.
//!
//! This crate is the layer *above* the storage backend's path
//! confinement: it validates user-supplied filenames and metadata before
//! bytes ever reach storage, sniffs actual content type from magic bytes
//! instead of trusting extensions or declared MIME types, and enforces
//! the file size/page/output budgets required by the CLI/MCP
//! specification ("set explicit file size, page count, execution time,
//! and output limits"; "malicious filenames cannot escape storage
//! roots").
//!
//! Entry points:
//!
//! - [`validate_upload`] — the full pipeline for new uploads: filename
//!   sanitization + size limit + magic-byte sniff + optional expected
//!   format, returning a [`ValidationReport`].
//! - [`validate_pdf`] — deeper gate for callers that already hold PDF
//!   bytes (e.g. job handlers): size cap, `%PDF-` header within the
//!   spec's leading-junk window, `lopdf` structural parse, and
//!   encrypted-document rejection.
//! - [`sanitize_filename`], [`sniff_format`], [`ResourceLimits`] —
//!   lower-level pieces for callers needing finer control.
//! - [`SecurityEngine`] — [`crate::crates::core::engine::ProcessingEngine`]
//!   adapter so the engine registry can discover the `validate`
//!   capability uniformly.
//!
//! Design choices, documented deliberately:
//!
//! - **Bytes only.** File extensions, MIME types, and caller-supplied
//!   metadata are never trusted — validation inspects the byte stream.
//! - **Reject, don't mangle.** Dangerous filenames are refused rather
//!   than silently rewritten (see the `filename` module documentation).
//! - **Encrypted PDFs are rejected.** A validator that cannot see inside
//!   an encrypted container cannot vouch for it; downstream engines
//!   would fail anyway without a password. Rejection is a distinct
//!   [`SecurityError::Encrypted`] so callers can report it accurately.
//! - **Header may be preceded by junk.** The `%PDF-` magic is searched
//!   within the first 1024 bytes, per the PDF spec allowance for
//!   leading garbage.

mod engine;
mod error;
mod filename;
mod limits;
mod sniff;
mod validate;

pub use engine::SecurityEngine;
pub use error::{SecurityError, SecurityResult};
pub use filename::sanitize_filename;
pub use limits::ResourceLimits;
pub use sniff::{SniffedFormat, sniff_format};
pub use validate::{ValidationReport, validate_pdf, validate_upload};

#[cfg(test)]
mod tests {
    use super::*;
    use crate::crates::core::engine::{
        EngineHealthStatus, EngineKind, EngineOperation, ProcessingEngine,
    };

    /// Minimal but structurally valid one-page PDF.
    const MINIMAL_PDF: &[u8] = b"%PDF-1.4\n\
1 0 obj<</Type/Catalog/Pages 2 0 R>>endobj\n\
2 0 obj<</Type/Pages/Kids[3 0 R]/Count 1>>endobj\n\
3 0 obj<</Type/Page/Parent 2 0 R/MediaBox[0 0 612 792]>>endobj\n\
trailer<</Root 1 0 R/ID[<0123456789abcdef0123456789abcdef><0123456789abcdef0123456789abcdef>]>>\n\
%%EOF";

    const MAX: u64 = 10 * 1024 * 1024;

    #[test]
    fn validate_pdf_accepts_minimal_pdf() {
        assert!(validate_pdf(MINIMAL_PDF, MAX).is_ok());
    }

    #[test]
    fn validate_pdf_rejects_empty() {
        assert!(matches!(
            validate_pdf(b"", MAX),
            Err(SecurityError::EmptyFile)
        ));
    }

    #[test]
    fn validate_pdf_rejects_too_large() {
        let big = vec![b'%'; 100];
        let err = validate_pdf(&big, 50).unwrap_err();
        assert!(matches!(
            err,
            SecurityError::FileTooLarge { size: 100, max: 50 }
        ));
    }

    #[test]
    fn validate_pdf_rejects_non_pdf_bytes() {
        for bad in [
            &b"#!/bin/sh\necho hi"[..],
            b"%PDFX-1.4 not quite the magic",
            b"plain text with no magic anywhere in it",
        ] {
            assert!(matches!(validate_pdf(bad, MAX), Err(SecurityError::NotPdf)));
        }
    }

    #[test]
    fn pdf_magic_may_follow_leading_junk_within_window() {
        let mut data = vec![0u8; 100];
        data.extend_from_slice(MINIMAL_PDF);
        assert!(validate_pdf(&data, MAX).is_ok());
    }

    #[test]
    fn pdf_magic_beyond_window_is_rejected() {
        let mut data = vec![0u8; 1025];
        data.extend_from_slice(MINIMAL_PDF);
        assert!(matches!(
            validate_pdf(&data, MAX),
            Err(SecurityError::NotPdf)
        ));
    }

    #[test]
    fn validate_pdf_rejects_malformed_structure() {
        let fake = b"%PDF-1.4\nthis has a header but no objects or xref";
        assert!(matches!(
            validate_pdf(fake, MAX),
            Err(SecurityError::Malformed { .. })
        ));
    }

    #[test]
    fn validate_pdf_rejects_encrypted_pdf() {
        // Build a real encrypted PDF: load the minimal document, encrypt
        // it with lopdf (V1/RC4-40, empty user password so lopdf's loader
        // can authenticate and report `is_encrypted()`), then save.
        let mut doc = lopdf::Document::load_mem(MINIMAL_PDF).unwrap();
        let state = lopdf::EncryptionState::try_from(lopdf::EncryptionVersion::V1 {
            document: &doc,
            owner_password: "owner",
            user_password: "",
            permissions: lopdf::Permissions::all(),
        })
        .unwrap();
        doc.encrypt(&state).unwrap();
        let mut encrypted_pdf = Vec::new();
        doc.save_to(&mut encrypted_pdf).unwrap();

        // Sanity: lopdf itself sees the encryption. `load_mem`
        // auto-decrypts empty-password documents, so `is_encrypted()`
        // is false post-load; `was_encrypted()` records the original
        // state — the same pair `validate_pdf` checks.
        let reloaded = lopdf::Document::load_mem(&encrypted_pdf).unwrap();
        assert!(reloaded.was_encrypted());

        assert!(matches!(
            validate_pdf(&encrypted_pdf, MAX),
            Err(SecurityError::Encrypted)
        ));
    }

    #[test]
    fn engine_metadata_and_capabilities() {
        let engine = SecurityEngine::new();
        assert_eq!(engine.name(), "security");
        assert_eq!(engine.version(), env!("CARGO_PKG_VERSION"));
        assert_eq!(engine.kind(), EngineKind::Security);

        let caps = engine.capabilities();
        assert_eq!(caps.operations, [EngineOperation::Validate]);
        assert!(caps.input_formats.contains(&"pdf".to_string()));
        assert!(caps.output_formats.is_empty());
    }

    #[tokio::test]
    async fn engine_health_is_healthy() {
        let health = SecurityEngine::new().health().await;
        assert_eq!(health.status, EngineHealthStatus::Healthy);
        assert!(health.message.is_none());
    }
}
