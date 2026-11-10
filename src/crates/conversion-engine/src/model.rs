//! Conversion formats and the request/response contract shared by
//! every [`ConversionBackend`](crate::crates::conversion_engine::ConversionBackend).

use serde::{Deserialize, Serialize};

/// A document format the conversion engine can convert to or from.
///
/// Serialized as `snake_case` in JSON output. `parse` accepts the
/// same names plus common aliases (`jpg`, `markdown`/`md`, `text`/`txt`).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Format {
    Pdf,
    Txt,
    Markdown,
    Html,
    Png,
    Jpeg,
    Docx,
    Odt,
    Xlsx,
    Xls,
    Pptx,
    Ppt,
    Epub,
    /// RFC 5322 email message (`.eml`). Input-only: converted to PDF
    /// by rendering headers + extracted text body.
    Eml,
    /// URL *source* rather than a document format: `input` carries
    /// the target URL as a UTF-8 string. Only `url` → `pdf` is
    /// meaningful — the engine fetches the page and delegates to the
    /// `html` → `pdf` path.
    Url,
    /// Comic Book ZIP archive (CBZ) — a ZIP container of page images.
    Cbz,
    /// Comic Book RAR archive (CBR). Recognized so callers get an
    /// explicit "unsupported" error rather than "unknown format";
    /// no in-crate RAR decoder exists.
    Cbr,
}

impl Format {
    /// Canonical lowercase identifier (`"pdf"`, `"markdown"`, ...).
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Pdf => "pdf",
            Self::Txt => "txt",
            Self::Markdown => "markdown",
            Self::Html => "html",
            Self::Png => "png",
            Self::Jpeg => "jpeg",
            Self::Docx => "docx",
            Self::Odt => "odt",
            Self::Xlsx => "xlsx",
            Self::Xls => "xls",
            Self::Pptx => "pptx",
            Self::Ppt => "ppt",
            Self::Epub => "epub",
            Self::Eml => "eml",
            Self::Url => "url",
            Self::Cbz => "cbz",
            Self::Cbr => "cbr",
        }
    }

    /// Parses a format name, accepting aliases. Returns `None` for
    /// anything unrecognized — callers map that to an invalid-input
    /// error rather than guessing.
    pub fn parse(s: &str) -> Option<Self> {
        match s.trim().to_ascii_lowercase().as_str() {
            "pdf" => Some(Self::Pdf),
            "txt" | "text" | "plain" => Some(Self::Txt),
            "md" | "markdown" => Some(Self::Markdown),
            "html" | "htm" => Some(Self::Html),
            "png" => Some(Self::Png),
            "jpg" | "jpeg" | "jpe" => Some(Self::Jpeg),
            "docx" => Some(Self::Docx),
            "odt" | "odf" => Some(Self::Odt),
            "xlsx" => Some(Self::Xlsx),
            "xls" => Some(Self::Xls),
            "pptx" => Some(Self::Pptx),
            "ppt" => Some(Self::Ppt),
            "epub" => Some(Self::Epub),
            "eml" | "email" | "rfc822" | "message/rfc822" => Some(Self::Eml),
            "url" | "uri" => Some(Self::Url),
            "cbz" | "comicbook" | "comicbook+zip" | "application/vnd.comicbook+zip" => {
                Some(Self::Cbz)
            }
            "cbr" | "comicbook-rar" | "application/x-cbr" => Some(Self::Cbr),
            _ => None,
        }
    }

    /// Every [`Format`] variant. Backends and the engine iterate this
    /// for exhaustive capability/health scans so a new variant cannot
    /// be silently missed.
    pub const ALL: [Self; 17] = [
        Self::Pdf,
        Self::Txt,
        Self::Markdown,
        Self::Html,
        Self::Png,
        Self::Jpeg,
        Self::Docx,
        Self::Odt,
        Self::Xlsx,
        Self::Xls,
        Self::Pptx,
        Self::Ppt,
        Self::Epub,
        Self::Eml,
        Self::Url,
        Self::Cbz,
        Self::Cbr,
    ];

    /// Canonical file extension for this format (no dot), or `None`
    /// for pseudo-formats that are not files ([`Format::Url`]).
    pub fn extension(self) -> Option<&'static str> {
        Some(match self {
            Self::Pdf => "pdf",
            Self::Txt => "txt",
            Self::Markdown => "md",
            Self::Html => "html",
            Self::Png => "png",
            Self::Jpeg => "jpg",
            Self::Docx => "docx",
            Self::Odt => "odt",
            Self::Xlsx => "xlsx",
            Self::Xls => "xls",
            Self::Pptx => "pptx",
            Self::Ppt => "ppt",
            Self::Epub => "epub",
            Self::Eml => "eml",
            Self::Cbz => "cbz",
            Self::Cbr => "cbr",
            Self::Url => return None,
        })
    }

    /// IANA media type for this format, or `None` for pseudo-formats
    /// that are not files ([`Format::Url`]).
    pub fn mime_type(self) -> Option<&'static str> {
        Some(match self {
            Self::Pdf => "application/pdf",
            Self::Txt => "text/plain",
            Self::Markdown => "text/markdown",
            Self::Html => "text/html",
            Self::Png => "image/png",
            Self::Jpeg => "image/jpeg",
            Self::Docx => "application/vnd.openxmlformats-officedocument.wordprocessingml.document",
            Self::Odt => "application/vnd.oasis.opendocument.text",
            Self::Xlsx => "application/vnd.openxmlformats-officedocument.spreadsheetml.sheet",
            Self::Xls => "application/vnd.ms-excel",
            Self::Pptx => {
                "application/vnd.openxmlformats-officedocument.presentationml.presentation"
            }
            Self::Ppt => "application/vnd.ms-powerpoint",
            Self::Epub => "application/epub+zip",
            Self::Eml => "message/rfc822",
            Self::Cbz => "application/vnd.comicbook+zip",
            Self::Cbr => "application/x-cbr",
            Self::Url => return None,
        })
    }

    /// Whether this is a raster image format.
    pub fn is_image(self) -> bool {
        matches!(self, Self::Png | Self::Jpeg)
    }
}

/// Per-conversion options. Fields are backend-interpreted; unknown
/// needs belong in [`Self::params`] so definitions stay forward
/// compatible.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct ConvertOptions {
    /// Rasterization DPI for PDF→image conversions.
    #[serde(default)]
    pub dpi: Option<u32>,
    /// JPEG quality 1–100 for JPEG outputs.
    #[serde(default)]
    pub quality: Option<u8>,
    /// Page size hint for PDF-producing conversions (`"a4"`,
    /// `"letter"`, ...). Backends that cannot honor it must say so in
    /// [`ConvertOutput::warnings`], never silently ignore it.
    #[serde(default)]
    pub page_size: Option<String>,
    /// Escape hatch for backend-specific parameters.
    #[serde(default)]
    pub params: serde_json::Value,
}

/// One conversion request: input bytes plus declared formats.
#[derive(Debug, Clone)]
pub struct ConvertRequest {
    /// Format of `input` — must match its actual content; backends
    /// should verify rather than trust it where cheap.
    pub input_format: Format,
    /// Desired output format.
    pub output_format: Format,
    /// The document bytes.
    pub input: Vec<u8>,
    /// Conversion options.
    pub options: ConvertOptions,
}

/// Successful conversion result.
#[derive(Debug, Clone)]
pub struct ConvertOutput {
    /// Converted document bytes.
    pub bytes: Vec<u8>,
    /// Format of `bytes` (normally `request.output_format`).
    pub output_format: Format,
    /// Name of the backend that performed the conversion
    /// ([`ConversionBackend::name`]).
    pub backend: String,
    /// Non-fatal caveats (e.g. "page_size ignored", "first frame only").
    /// Must never contain secrets or absolute server paths.
    pub warnings: Vec<String>,
}
