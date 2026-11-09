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

/// Video container families identified from magic bytes.
///
/// Distinct from [`SniffedFormat`] itself because several video
/// containers share one byte-level signature family (EBML covers both
/// Matroska and WebM); the container carries the disambiguated result.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, serde::Serialize, serde::Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum VideoContainer {
    /// ISO Base Media File Format: `....ftyp` box at offset 4 with a
    /// recognized MP4/MOV major brand (`mp4*`, `isom`, `qt  `, …).
    Mp4,
    /// EBML header (`1A 45 DF A3`) with Matroska DocType evidence, or
    /// EBML with no readable DocType — see [`sniff_format`].
    Matroska,
    /// EBML header with WebM DocType evidence.
    Webm,
    /// `RIFF....AVI ` — Audio Video Interleave.
    Avi,
    /// MPEG transport stream: `0x47` sync byte at each 188-byte packet
    /// boundary.
    MpegTs,
    /// MPEG program stream: `00 00 01 BA` pack header.
    MpegPs,
}

/// A document or media format identified from magic bytes.
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
    /// Video container — see [`VideoContainer`] for the detected family.
    Video { container: VideoContainer },
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
            Self::Video { container } => container.sniffed_extension(),
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
            Self::Video { container } => container.label(),
            Self::Unknown => "unknown",
        }
    }

    /// Whether the sniffed format is a video container. Useful for
    /// callers applying video-specific budgets without re-matching.
    pub fn is_video(&self) -> bool {
        matches!(self, Self::Video { .. })
    }
}

impl VideoContainer {
    /// Conventional file extension including the dot.
    pub fn sniffed_extension(&self) -> &'static str {
        match self {
            Self::Mp4 => ".mp4",
            Self::Matroska => ".mkv",
            Self::Webm => ".webm",
            Self::Avi => ".avi",
            Self::MpegTs => ".ts",
            Self::MpegPs => ".mpg",
        }
    }

    /// Human-readable label for logs and error messages.
    pub fn label(&self) -> &'static str {
        match self {
            Self::Mp4 => "MP4/MOV video",
            Self::Matroska => "Matroska video",
            Self::Webm => "WebM video",
            Self::Avi => "AVI video",
            Self::MpegTs => "MPEG-TS video",
            Self::MpegPs => "MPEG-PS video",
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
    } else if let Some(container) = sniff_video_container(bytes) {
        SniffedFormat::Video { container }
    } else {
        SniffedFormat::Unknown
    }
}

/// Identify a video container from magic bytes, or `None`.
///
/// Signatures are checked in an order that avoids false positives:
/// the `ftyp` box and RIFF/AVI patterns are unambiguous at their
/// offsets; EBML is disambiguated by scanning the head for DocType
/// strings; MPEG-PS requires its 4-byte pack header; MPEG-TS is the
/// weakest check (see below).
fn sniff_video_container(bytes: &[u8]) -> Option<VideoContainer> {
    if is_mp4_ftyp(bytes) {
        return Some(VideoContainer::Mp4);
    }
    if bytes.starts_with(&[0x1A, 0x45, 0xDF, 0xA3]) {
        return Some(ebml_container(bytes));
    }
    // RIFF....AVI — WAV and WebP share the RIFF magic, so the form
    // type at offset 8 must be checked too.
    if bytes.len() >= 12 && bytes.starts_with(b"RIFF") && &bytes[8..12] == b"AVI " {
        return Some(VideoContainer::Avi);
    }
    if bytes.starts_with(&[0x00, 0x00, 0x01, 0xBA]) {
        return Some(VideoContainer::MpegPs);
    }
    if is_mpeg_ts(bytes) {
        return Some(VideoContainer::MpegTs);
    }
    None
}

/// ISO-BMFF `ftyp` box: 4-byte size, `ftyp` at offset 4, major brand at
/// 8..12. Only recognized MP4/MOV brand families are accepted — an
/// `ftyp` box with an unknown brand (e.g. `heic`) is reported as
/// unknown rather than guessed at as video.
fn is_mp4_ftyp(bytes: &[u8]) -> bool {
    if bytes.len() < 12 || &bytes[4..8] != b"ftyp" {
        return false;
    }
    let brand = &bytes[8..12];
    brand.starts_with(b"mp4")
        || matches!(
            brand,
            b"isom"
                | b"iso2"
                | b"iso3"
                | b"iso4"
                | b"iso5"
                | b"iso6"
                | b"iso8"
                | b"iso9"
                | b"avc1"
                | b"dash"
                | b"qt  "
                | b"M4V "
                | b"M4A "
        )
}

/// EBML covers both Matroska (.mkv) and WebM; they are the same
/// container family distinguished only by the DocType element inside
/// the header. Sniffing scans the head window for the DocType strings
/// `webm`/`matroska`. When neither is visible (truncated or
/// non-canonical header), the file is still an EBML stream, so it is
/// conservatively reported as [`VideoContainer::Matroska`] — callers
/// must not treat the container label as proof of WebM; the video
/// engine's decoder is the real arbiter. This ambiguity is inherent:
/// security never trusts the filename extension to disambiguate.
fn ebml_container(bytes: &[u8]) -> VideoContainer {
    let window = &bytes[..bytes.len().min(64)];
    let has_matroska = window
        .windows(b"matroska".len())
        .any(|w| w == b"matroska");
    let has_webm = window.windows(b"webm".len()).any(|w| w == b"webm");
    if has_webm && !has_matroska {
        VideoContainer::Webm
    } else {
        // Matroska evidence, or neither string visible: report the
        // broader Matroska family rather than guessing WebM.
        VideoContainer::Matroska
    }
}

/// MPEG transport stream: `0x47` sync byte at offset 0 and, when the
/// buffer reaches the next packet boundary, at offset 188. When the
/// head sample is shorter than 189 bytes the second sync cannot be
/// checked, so a lone `0x47` is accepted — a deliberately weak check
/// documented here; downstream decoding is the real verification.
fn is_mpeg_ts(bytes: &[u8]) -> bool {
    if bytes.is_empty() || bytes[0] != 0x47 {
        return false;
    }
    bytes.len() < 189 || bytes[188] == 0x47
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
    fn detects_video_containers() {
        // MP4/MOV: size-prefixed `ftyp` box at offset 4, brand at 8..12.
        let mp4 = b"\x00\x00\x00\x18ftypmp42\x00\x00\x00\x00mp42isom";
        assert_eq!(
            sniff_format(mp4),
            SniffedFormat::Video {
                container: VideoContainer::Mp4
            }
        );
        let mov = b"\x00\x00\x00\x14ftypqt  \x00\x00\x00\x00qt  ";
        assert_eq!(
            sniff_format(mov),
            SniffedFormat::Video {
                container: VideoContainer::Mp4
            }
        );
        let isom = b"\x00\x00\x00\x20ftypisom\x00\x00\x02\x00isomiso2";
        assert_eq!(
            sniff_format(isom),
            SniffedFormat::Video {
                container: VideoContainer::Mp4
            }
        );

        // Matroska/WebM: EBML magic, DocType string distinguishes them.
        let mkv = b"\x1a\x45\xdf\xa3\x93\x42\x82\x88matroska\x42\x87\x81\x02\x42\xf7\x81\x01";
        assert_eq!(
            sniff_format(mkv),
            SniffedFormat::Video {
                container: VideoContainer::Matroska
            }
        );
        let webm = b"\x1a\x45\xdf\xa3\x9f\x42\x82\x84webm\x42\x87\x81\x04\x42\xf7\x81\x01";
        assert_eq!(
            sniff_format(webm),
            SniffedFormat::Video {
                container: VideoContainer::Webm
            }
        );
        // EBML header with no visible DocType → conservatively Matroska.
        let bare_ebml = b"\x1a\x45\xdf\xa3\x01\x00\x00\x00\x00\x00\x00\x00";
        assert_eq!(
            sniff_format(bare_ebml),
            SniffedFormat::Video {
                container: VideoContainer::Matroska
            }
        );

        // AVI: RIFF wrapper with "AVI " form type at offset 8.
        let avi = b"RIFF\x00\x00\x00\x00AVI LIST\x00\x00\x00\x00hdrl";
        assert_eq!(
            sniff_format(avi),
            SniffedFormat::Video {
                container: VideoContainer::Avi
            }
        );

        // MPEG-PS pack header.
        let ps = b"\x00\x00\x01\xba\x44\x00\x04\x00\x04\x01\x89\xc3\xf8";
        assert_eq!(
            sniff_format(ps),
            SniffedFormat::Video {
                container: VideoContainer::MpegPs
            }
        );

        // MPEG-TS: 0x47 sync at packet boundaries 0 and 188.
        let mut ts = vec![0u8; 376];
        ts[0] = 0x47;
        ts[188] = 0x47;
        assert_eq!(
            sniff_format(&ts),
            SniffedFormat::Video {
                container: VideoContainer::MpegTs
            }
        );
        // Short head (< 189 bytes): lone sync byte accepted, documented.
        assert_eq!(
            sniff_format(&[0x47, 0x40, 0x00, 0x10]),
            SniffedFormat::Video {
                container: VideoContainer::MpegTs
            }
        );
        // Sync at 0 but broken second sync → not TS.
        let mut bad_ts = vec![0u8; 376];
        bad_ts[0] = 0x47;
        bad_ts[188] = 0x00;
        assert_eq!(sniff_format(&bad_ts), SniffedFormat::Unknown);
    }

    #[test]
    fn rejects_non_video_lookalikes() {
        // RIFF is shared with WAV/WebP — only "AVI " form type counts.
        assert_eq!(
            sniff_format(b"RIFF\x00\x00\x00\x00WAVEfmt "),
            SniffedFormat::Unknown
        );
        assert_eq!(
            sniff_format(b"RIFF\x00\x00\x00\x00WEBPVP8 "),
            SniffedFormat::Unknown
        );
        // ftyp box with unrecognized brand is not claimed as video.
        assert_eq!(
            sniff_format(b"\x00\x00\x00\x18ftypheic\x00\x00\x00\x00mif1heic"),
            SniffedFormat::Unknown
        );
        // ftyp magic at wrong offset.
        assert_eq!(
            sniff_format(b"\x00\x00ftypmp42\x00\x00\x00\x00"),
            SniffedFormat::Unknown
        );
        // Truncated signatures.
        assert_eq!(sniff_format(b"\x1a\x45\xdf"), SniffedFormat::Unknown);
        assert_eq!(sniff_format(b"RIFF\x00\x00"), SniffedFormat::Unknown);
        assert_eq!(sniff_format(b"\x00\x00\x01"), SniffedFormat::Unknown);
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
        let webm = SniffedFormat::Video {
            container: VideoContainer::Webm,
        };
        assert_eq!(webm.sniffed_extension(), ".webm");
        assert_eq!(webm.label(), "WebM video");
        assert!(webm.is_video());
        assert!(!SniffedFormat::Pdf.is_video());
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
