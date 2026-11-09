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
            _ => None,
        }
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
