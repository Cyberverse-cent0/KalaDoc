//! Content sniffing via magic bytes.
//!
//! File extensions and caller-declared MIME types are attacker-controlled
//! metadata; the only trustworthy signal available before parsing is the
//! leading byte signature. [`sniff_format`] recognizes the formats this
//! application accepts for upload and returns
//! [`SniffedFormat::Unknown`] for anything else — callers decide whether
//! unknown content is acceptable (see
//! [`crate::crates::security_engine::validate_upload`]).
//!
//! Detection is intentionally conservative: a format is only reported
//! when its signature matches at offset 0. Truncated headers, garbage,
//! and polyglot files whose signature does not lead all return `Unknown`.

use std::fmt;

/// A document format identified from magic bytes.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, serde::Serialize, serde::Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum SniffedFormat {
    /// `%PDF-` — PDF document.
    Pdf,
    /// `\x89PNG` — PNG image.
    Png,
    /// `FF D8 FF` — JPEG image.
    Jpeg,
    /// `PK\x03\x04` — ZIP archive (also covers docx/xlsx/jar family).
    Zip,
    /// `GIF8` — GIF image (87a/89a).
    Gif,
    /// No recognized signature.
    Unknown,
}

impl SniffedFormat {
    /// Conventional file extension including the dot, or `""` for
    /// [`SniffedFormat::Unknown`]. Suitable for building storage names.
    pub fn sniffed_extension(&self) -> &'static str {
        match self {
            Self::Pdf => ".pdf",
            Self::Png => ".png",
            Self::Jpeg => ".jpg",
            Self::Zip => ".zip",
            Self::Gif => ".gif",
            Self::Unknown => "",
        }
    }

    /// Human-readable label for logs and error messages.
    pub fn label(&self) -> &'static str {
        match self {
            Self::Pdf => "PDF",
            Self::Png => "PNG",
            Self::Jpeg => "JPEG",
            Self::Zip => "ZIP",
            Self::Gif => "GIF",
            Self::Unknown => "unknown",
        }
    }
}

impl fmt::Display for SniffedFormat {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.label())
    }
}

/// Identify the format of `bytes` from its leading magic bytes.
///
/// Only the first few bytes are inspected; `bytes` may be a head sample
/// of a larger file. Returns [`SniffedFormat::Unknown`] when no known
/// signature matches or the input is too short.
pub fn sniff_format(bytes: &[u8]) -> SniffedFormat {
    if bytes.starts_with(b"%PDF-") {
        SniffedFormat::Pdf
    } else if bytes.starts_with(b"\x89PNG") {
        SniffedFormat::Png
    } else if bytes.starts_with(&[0xFF, 0xD8, 0xFF]) {
        SniffedFormat::Jpeg
    } else if bytes.starts_with(b"PK\x03\x04") {
        SniffedFormat::Zip
    } else if bytes.starts_with(b"GIF8") {
        SniffedFormat::Gif
    } else {
        SniffedFormat::Unknown
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn detects_each_format() {
        assert_eq!(sniff_format(b"%PDF-1.7\n..."), SniffedFormat::Pdf);
        assert_eq!(sniff_format(b"\x89PNG\r\n\x1a\n..."), SniffedFormat::Png);
        assert_eq!(
            sniff_format(&[0xFF, 0xD8, 0xFF, 0xE0, 0x00]),
            SniffedFormat::Jpeg
        );
        assert_eq!(sniff_format(b"PK\x03\x04\x14\x00"), SniffedFormat::Zip);
        assert_eq!(sniff_format(b"GIF89a...."), SniffedFormat::Gif);
        assert_eq!(sniff_format(b"GIF87a...."), SniffedFormat::Gif);
    }

    #[test]
    fn rejects_unknown_and_truncated() {
        assert_eq!(sniff_format(b""), SniffedFormat::Unknown);
        assert_eq!(sniff_format(b"%PDF"), SniffedFormat::Unknown); // missing '-'
        assert_eq!(sniff_format(b"%pdF-"), SniffedFormat::Unknown); // case-sensitive
        assert_eq!(sniff_format(b"\x89PN"), SniffedFormat::Unknown); // truncated sig
        assert_eq!(sniff_format(b"PK\x03"), SniffedFormat::Unknown);
        assert_eq!(sniff_format(b"MZ\x90\x00"), SniffedFormat::Unknown); // PE executable
        assert_eq!(sniff_format(b"#!/bin/sh\necho hi"), SniffedFormat::Unknown);
        assert_eq!(sniff_format(&[0xFF, 0xD8]), SniffedFormat::Unknown); // truncated JPEG
        assert_eq!(
            sniff_format(b"random garbage bytes"),
            SniffedFormat::Unknown
        );
    }

    #[test]
    fn extension_and_label_helpers() {
        assert_eq!(SniffedFormat::Pdf.sniffed_extension(), ".pdf");
        assert_eq!(SniffedFormat::Jpeg.sniffed_extension(), ".jpg");
        assert_eq!(SniffedFormat::Unknown.sniffed_extension(), "");
        assert_eq!(SniffedFormat::Gif.label(), "GIF");
        assert_eq!(SniffedFormat::Unknown.to_string(), "unknown");
        // Serde round-trips through lowercase names.
        assert_eq!(
            serde_json::to_string(&SniffedFormat::Pdf).unwrap(),
            "\"pdf\""
        );
        assert_eq!(
            serde_json::from_str::<SniffedFormat>("\"jpeg\"").unwrap(),
            SniffedFormat::Jpeg
        );
    }
}
