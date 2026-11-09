//! PDF processing engine — merge, split, rotate, extract, inspect, and
//! the wider spec surface: page manipulation, marks, security,
//! extraction, rendering, OCR and conversion.
//!
//! This crate implements a [`ProcessingEngine`](crate::crates::core::engine::ProcessingEngine):
//! [`PdfEngine`], backed by the in-process, pure-Rust `lopdf` library
//! plus optional external tools (`pdftoppm` for rendering, `tesseract`
//! for OCR). Core operations work on in-memory byte buffers and return
//! `Vec<u8>` (or structured reports); they never shell out.
//!
//! # Operations
//!
//! | Engine operation | Function | Notes |
//! |---|---|---|
//! | `merge` | [`merge`] | Concatenates PDFs in input order; outlines/signatures not carried over |
//! | `split` | [`split`] | Fixed pages-per-file chunks |
//! | `rotate` | [`rotate`] | 90/180/270°, 1-based page ranges |
//! | `resize` | [`resize`] | Page-box scaling per [`ResizeSpec`]; content not scaled |
//! | `crop` | [`crop`] | Absolute `/CropBox` on selected pages |
//! | `reorder` | [`reorder`] | Full-page permutation spec `"3,1,2"` |
//! | `remove_pages` | [`remove_pages`] | Delete a range spec's pages |
//! | `extract_pages` | [`extract_pages`] | 1-based range spec, e.g. `"1-3,8,10-12"` |
//! | `page_count` | [`page_count`] | Structural count; tolerates encrypted input |
//! | `compress` | [`compress`] | Object-stream/xref-stream recompression |
//! | `repair` | [`repair`] | Normalized rewrite + [`RepairReport`] notes |
//! | `flatten` | [`flatten`] | Best-effort annotation appearance flattening |
//! | `overlay` | [`overlay`] | Paint one PDF's pages onto another's |
//! | `watermark` | [`watermark`] | Text watermark, [`WatermarkOptions`] |
//! | `stamp` | [`stamp`] | Text or PNG-image stamp, [`StampOptions`] |
//! | `redact` | [`redact`] | Best-effort text removal (see op docs) |
//! | `sanitize` | [`sanitize`] | Strip active-content vectors |
//! | `encrypt` | [`encrypt`] | AES-256, user+owner passwords, [`PdfPermissions`] |
//! | `decrypt` | [`decrypt`] | Password-based decryption |
//! | `verify_signature` | [`verify_signatures`] | Structural report only |
//! | `verify_signature` | [`verify_signatures_crypto`] | CMS digest + optional chain check via `openssl` |
//! | `sign` | [`sign`] | Detached CMS signature via `openssl` — [`SignCredentials`], [`SignOptions`] |
//! | `remove_signatures` | [`remove_signatures`] | Strip signature fields/widgets/`/Perms` refs |
//! | `validate` | [`validate`] | [`ValidationReport`] structural checks |
//! | `inspect` | [`inspect`] | [`PdfInfo`] metadata report |
//! | `extract_metadata` | [`extract_metadata`] | Extended [`PdfMetadata`] incl. XMP |
//! | `extract_images` | [`extract_images`] | Embedded image XObjects |
//! | `extract_text` | [`extract_text`] | Best-effort per-page text |
//! | `render` | [`render`] | First selected page → PNG/JPEG via `pdftoppm` |
//! | `compare` | [`compare`] | Text/structure/bytes diff, [`CompareReport`] |
//! | `convert` | [`convert`] | Text layer → text or Markdown bytes |
//! | `ocr` | [`ocr`] | Rasterize + `tesseract` → per-page text |
//! | `ocr_languages` | [`ocr_languages`] | Installed tesseract language codes |
//!
//! `sign` and `verify_signatures_crypto` shell out to `openssl` for the
//! CMS/PKCS#7 cryptography (`lopdf` has none); a missing binary produces
//! [`PdfError::MissingTool`]. Signing is a whole-document, full-rewrite
//! operation — it refuses already-signed input and does not support
//! RFC 3161 timestamping. See the `sign` module docs for details.
//!
//! # Contracts
//!
//! - **Page numbers are 1-based** everywhere, matching the CLI/MCP spec.
//! - **Page ranges** use one shared grammar parsed by
//!   [`parse_page_range`]: `"1-3,8,10-12"`, `"all"`; validated fully
//!   (including bounds) before any document work happens.
//! - **Encryption**: transforming ops reject encrypted input
//!   ([`PdfError::Encrypted`]); `inspect`/`validate`/`page_count`/
//!   `extract_metadata` tolerate it; `decrypt` accepts a password.
//! - **External tools**: `render`, `ocr`, `ocr_languages` need
//!   `pdftoppm`/`tesseract` on `PATH`, `sign`/`verify_signatures_crypto`
//!   need `openssl` — missing tools produce
//!   [`PdfError::MissingTool`], never fake output. `PdfEngine::health`
//!   reports `Degraded` naming absent tools.
//! - **Limits** ([`EngineLimits`](crate::crates::core::engine::EngineLimits)) are enforced inside the ops: input
//!   size and page count at load time for every op, output count in
//!   `split`, combined page count in `merge`, pixel budgets for
//!   rendered/decoded images.
//! - **No panics**: malformed input returns [`PdfError`]; the `lopdf`
//!   load is additionally wrapped in `catch_unwind` for hostile PDFs.

mod engine;
mod error;
mod ops;
#[cfg(test)]
mod tests;

pub use engine::PdfEngine;
pub use error::{PdfError, PdfResult};
pub use ops::{
    AnnotFilter, AnnotationInfo, BlankReport, ColorReport, CommentIcon, CommentSpec,
    CompareMode, CompareReport, ConvertTarget, CryptoSignatureInfo, CryptoSignatureReport,
    DiffEntry, DiffKind, DrawTextSpec, DupPos, EmbeddedImage, FreeTextSpec, GcReport,
    HeaderFooterSpec, InsertPos, LinearizeReport, MarkPosition, PageImage, PageImageReport,
    PageNumberStyle, PageSize, PageText, PdfDate, PdfInfo, PdfMetadata, PdfPermissions, PdfaLevel,
    PdfaReport, RemoveObjectsReport, RenderFormat, RepairReport, ResizeSpec, RotateDegrees,
    SignCredentials, SignOptions, SignatureInfo, SignatureReport, StampOptions,
    ValidationReport, WatermarkOptions, XmpMetadata, add_comment, add_freetext,
    add_page_numbers, compare, compress, convert, crop, decrypt, draw_text, duplicate_pages,
    encrypt, extract_images, extract_metadata, extract_pages, extract_text, flatten,
    garbage_collect, get_xmp, grayscale, headers_footers, insert_blank, inspect, interleave,
    invert_colors, linearize, list_annotations, merge, ocr, ocr_languages, overlay, page_count,
    parse_page_range, redact, remove_annotations,
    remove_blank_pages, remove_embedded_files, remove_javascript_names, remove_named_objects,
    remove_pages, remove_signatures, render, repair, resize, rotate, sanitize, scale_content,
    set_xmp, sign, split, stamp, to_pdfa, validate, verify_signatures,
    verify_signatures_crypto, watermark,
};
