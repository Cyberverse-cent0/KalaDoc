//! Upload validation pipeline.
//!
//! [`validate_upload`] is the single entry point callers should use before
//! bytes reach storage or a processing engine. It composes the three
//! checks this crate provides — filename sanitization, resource limits,
//! and magic-byte content sniffing — and returns a
//! [`ValidationReport`] describing the *sanitized* identity of the upload.
//!
//! Trust model: the filename, declared size, and declared content type
//! are all attacker-controlled. Only the returned sanitized filename and
//! the sniffed format are trustworthy.

use serde::{Deserialize, Serialize};

use super::error::{SecurityError, SecurityResult};
use super::filename::sanitize_filename;
use super::limits::ResourceLimits;
use super::sniff::{SniffedFormat, sniff_format};

/// Outcome of a successful upload validation.
///
/// All fields describe sanitized/verified data — safe to log, persist,
/// or hand to storage. `sanitized_filename` is a bare filename (never a
/// path); `format` comes from magic bytes, not from any declared type.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct ValidationReport {
    /// The validated bare filename, safe to join onto a storage root.
    pub sanitized_filename: String,
    /// Format detected from the content's magic bytes.
    pub format: SniffedFormat,
    /// Verified upload size in bytes (equals the content length).
    pub size_bytes: u64,
}

/// Validate an upload's filename, size, and content before storage.
///
/// `content` must be the actual upload bytes (or at minimum its head —
/// the sniffer only reads the first few). `declared_size` is the size the
/// caller believes the upload to be; it must equal `content.len()` so a
/// client cannot under-declare size to slip past [`ResourceLimits`].
///
/// Checks, in order:
/// 1. magic-byte sniff (cheap; needed to pick the applicable size cap);
/// 2. `declared_size` against the format's cap —
///    [`ResourceLimits::max_video_upload_bytes`] for sniffed video,
///    [`ResourceLimits::max_upload_bytes`] otherwise;
/// 3. `declared_size == content.len()` ([`SecurityError::SizeMismatch`]);
/// 4. non-empty content ([`SecurityError::EmptyFile`]);
/// 5. filename via [`sanitize_filename`];
/// 6. when `expected` is `Some(fmt)`, the sniffed
///    format must equal `fmt` — passing `None` accepts any sniffed
///    format including [`SniffedFormat::Unknown`] and video containers.
///    An expected `Pdf` upload whose bytes are really a ZIP archive is
///    rejected, closing the extension/MIME-spoofing hole.
///
/// # Errors
/// Returns the first failing [`SecurityError`]. Error messages never
/// contain the raw user-supplied filename.
pub fn validate_upload(
    filename: &str,
    declared_size: u64,
    content: &[u8],
    limits: &ResourceLimits,
    expected: Option<SniffedFormat>,
) -> SecurityResult<ValidationReport> {
    // Sniff first: the detected format selects the size cap, so video
    // uploads are judged against `max_video_upload_bytes` rather than
    // the tighter document budget.
    let format = sniff_format(content);
    limits.check_upload_size(declared_size, &format)?;

    let actual = content.len() as u64;
    if declared_size != actual {
        return Err(SecurityError::SizeMismatch {
            declared: declared_size,
            actual,
        });
    }
    if content.is_empty() {
        return Err(SecurityError::EmptyFile);
    }

    let sanitized_filename = sanitize_filename(filename)?;

    if let Some(expected) = expected {
        if format != expected {
            return Err(SecurityError::ContentTypeMismatch {
                expected,
                found: format,
            });
        }
    }

    Ok(ValidationReport {
        sanitized_filename,
        format,
        size_bytes: declared_size,
    })
}

/// How far into the input the `%PDF-` magic may appear. PDF readers
/// scan for the header rather than requiring offset 0; 1024 bytes is
/// the conventional search window.
const HEADER_SEARCH_WINDOW: usize = 1024;

/// Pre-flight validation for PDF bytes before any parser trusts them.
///
/// This is the cheap-but-real gate for callers that already hold a PDF
/// payload (e.g. job handlers). Unlike [`validate_upload`] — which only
/// sniffs magic bytes — this runs `lopdf`'s structural parse and rejects
/// encrypted documents, because a validator that cannot see inside an
/// encrypted container cannot vouch for it and downstream engines would
/// fail anyway without a password.
///
/// Checks, in order:
/// 1. `data.len() <= max_bytes` ([`SecurityError::FileTooLarge`]);
/// 2. non-empty input ([`SecurityError::EmptyFile`]);
/// 3. `%PDF-` magic within the first 1024 bytes
///    ([`SecurityError::NotPdf`]) — PDF readers scan for the header per
///    spec, so leading junk is tolerated but bounded;
/// 4. `lopdf` can parse the document structure
///    ([`SecurityError::Malformed`]);
/// 5. the document is not encrypted ([`SecurityError::Encrypted`]).
///
/// Returns `Ok(())` only when all checks pass. Never panics.
///
/// # Errors
/// See the check list above; every failure is a [`SecurityError`].
pub fn validate_pdf(data: &[u8], max_bytes: u64) -> SecurityResult<()> {
    let size = data.len() as u64;
    if size > max_bytes {
        return Err(SecurityError::FileTooLarge {
            size,
            max: max_bytes,
        });
    }
    if data.is_empty() {
        return Err(SecurityError::EmptyFile);
    }
    if !has_pdf_header(data) {
        return Err(SecurityError::NotPdf);
    }

    let doc = lopdf::Document::load_mem(data).map_err(|e| SecurityError::Malformed {
        reason: e.to_string(),
    })?;

    // `is_encrypted` sees the trailer `/Encrypt` entry; `was_encrypted`
    // also catches documents lopdf auto-decrypted with an empty user
    // password during load — both must be rejected by policy.
    if doc.is_encrypted() || doc.was_encrypted() {
        return Err(SecurityError::Encrypted);
    }

    Ok(())
}

/// True when `%PDF-` appears within the first [`HEADER_SEARCH_WINDOW`]
/// bytes. A fixed offset-0 check would reject spec-legal PDFs with
/// leading junk.
fn has_pdf_header(data: &[u8]) -> bool {
    let window = &data[..data.len().min(HEADER_SEARCH_WINDOW)];
    window.windows(5).any(|w| w == b"%PDF-")
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::crates::security_engine::VideoContainer;

    const PDF: &[u8] = b"%PDF-1.7\n1 0 obj\n<<>>\nendobj\n%%EOF";
    const PNG: &[u8] = b"\x89PNG\r\n\x1a\n....";
    const ZIP: &[u8] = b"PK\x03\x04\x14\x00....";
    const MP4: &[u8] = b"\x00\x00\x00\x18ftypmp42\x00\x00\x00\x00mp42isom....";
    const MKV: &[u8] =
        b"\x1a\x45\xdf\xa3\x93\x42\x82\x88matroska\x42\x87\x81\x02....";

    fn limits() -> ResourceLimits {
        ResourceLimits::default()
    }

    #[test]
    fn happy_path_pdf() {
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
    fn rejects_oversized_upload() {
        let mut l = limits();
        l.max_upload_bytes = 10;
        let err = validate_upload("a.pdf", 11, PDF, &l, None).unwrap_err();
        assert!(matches!(
            err,
            SecurityError::FileTooLarge { size: 11, max: 10 }
        ));
        // Exactly at max passes the size gate (fails later only if content mismatches).
        let report = validate_upload("a.pdf", PDF.len() as u64, PDF, &limits(), None).unwrap();
        assert_eq!(report.format, SniffedFormat::Pdf);
    }

    #[test]
    fn rejects_size_mismatch() {
        // Declared smaller than actual — the classic size-lie attack.
        let err = validate_upload("a.pdf", 4, PDF, &limits(), None).unwrap_err();
        assert!(matches!(
            err,
            SecurityError::SizeMismatch { declared: 4, actual } if actual == PDF.len() as u64
        ));
        // Declared larger than actual.
        let err = validate_upload("a.pdf", PDF.len() as u64 + 1, PDF, &limits(), None).unwrap_err();
        assert!(matches!(err, SecurityError::SizeMismatch { .. }));
    }

    #[test]
    fn rejects_empty_file() {
        let err = validate_upload("empty.pdf", 0, b"", &limits(), None).unwrap_err();
        assert!(matches!(err, SecurityError::EmptyFile));
    }

    #[test]
    fn rejects_bad_filename_before_sniffing() {
        let err = validate_upload("../../etc/passwd", PDF.len() as u64, PDF, &limits(), None)
            .unwrap_err();
        assert!(matches!(err, SecurityError::InvalidFilename { .. }));
    }

    #[test]
    fn rejects_content_type_mismatch() {
        // .pdf extension but real bytes are a ZIP — classic spoof.
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
        // Unknown content against an expected type also mismatches.
        let err = validate_upload(
            "fake.pdf",
            b"not a pdf".len() as u64,
            b"not a pdf",
            &limits(),
            Some(SniffedFormat::Pdf),
        )
        .unwrap_err();
        assert!(matches!(
            err,
            SecurityError::ContentTypeMismatch {
                found: SniffedFormat::Unknown,
                ..
            }
        ));
    }

    #[test]
    fn unknown_format_accepted_without_expectation() {
        let data = b"\x00\x01\x02binary-ish";
        let report = validate_upload("blob.bin", data.len() as u64, data, &limits(), None).unwrap();
        assert_eq!(report.format, SniffedFormat::Unknown);
        assert_eq!(report.sanitized_filename, "blob.bin");
    }

    #[test]
    fn matching_non_pdf_formats() {
        let report = validate_upload(
            "img.png",
            PNG.len() as u64,
            PNG,
            &limits(),
            Some(SniffedFormat::Png),
        )
        .unwrap();
        assert_eq!(report.format, SniffedFormat::Png);
        assert_eq!(report.sanitized_filename, "img.png");
    }

    #[test]
    fn accepts_video_uploads() {
        for (name, data, container) in [
            ("clip.mp4", &MP4[..], VideoContainer::Mp4),
            ("movie.mkv", &MKV[..], VideoContainer::Matroska),
        ] {
            let report = validate_upload(
                name,
                data.len() as u64,
                data,
                &limits(),
                Some(SniffedFormat::Video { container }),
            )
            .unwrap();
            assert_eq!(report.format, SniffedFormat::Video { container });
            assert_eq!(report.sanitized_filename, name);
        }
        // No expectation: video is accepted like any other sniffed format.
        let report = validate_upload("clip.mp4", MP4.len() as u64, MP4, &limits(), None).unwrap();
        assert!(report.format.is_video());
    }

    #[test]
    fn video_uses_larger_size_cap() {
        let mut l = limits();
        l.max_upload_bytes = MP4.len() as u64 - 1; // doc cap rejects it…
        l.max_video_upload_bytes = 1 << 20; // …video cap accepts it.
        let report = validate_upload("clip.mp4", MP4.len() as u64, MP4, &l, None).unwrap();
        assert!(report.format.is_video());
        // Same bytes under a non-video expectation still hit the doc cap
        // only via `expected` — but a non-video file of that size fails.
        let err = validate_upload("doc.pdf", MP4.len() as u64, PDF, &l, None).unwrap_err();
        assert!(matches!(err, SecurityError::FileTooLarge { .. }));
    }

    #[test]
    fn rejects_video_expected_mismatch() {
        // Caller expected a document; bytes are really MP4.
        let err = validate_upload(
            "innocent.pdf",
            MP4.len() as u64,
            MP4,
            &limits(),
            Some(SniffedFormat::Pdf),
        )
        .unwrap_err();
        assert!(matches!(
            err,
            SecurityError::ContentTypeMismatch {
                expected: SniffedFormat::Pdf,
                found: SniffedFormat::Video {
                    container: VideoContainer::Mp4
                }
            }
        ));
    }

    #[test]
    fn report_is_serde_serializable() {
        let report = validate_upload(
            "a.pdf",
            PDF.len() as u64,
            PDF,
            &limits(),
            Some(SniffedFormat::Pdf),
        )
        .unwrap();
        let json = serde_json::to_string(&report).unwrap();
        assert!(json.contains("\"format\":\"pdf\""));
        assert!(json.contains("\"sanitized_filename\":\"a.pdf\""));
        let back: ValidationReport = serde_json::from_str(&json).unwrap();
        assert_eq!(back, report);
    }
}
