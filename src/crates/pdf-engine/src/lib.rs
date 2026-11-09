//! PDF processing engine — merge, split, rotate, extract, inspect.
//!
//! This crate implements the first concrete [`ProcessingEngine`](crate::crates::core::engine::ProcessingEngine):
//! [`PdfEngine`], backed by the in-process, pure-Rust `lopdf` library.
//! There are no external binaries or services — everything operates on
//! in-memory byte buffers and returns `Vec<u8>` (or structured reports).
//!
//! # Operations
//!
//! | Engine operation | Function | Notes |
//! |---|---|---|
//! | `merge` | [`merge`] / `PdfEngine::merge` | Concatenates PDFs in input order; page content/resources preserved, outlines/signatures not carried over |
//! | `split` | [`split`] / `PdfEngine::split` | Fixed pages-per-file chunks |
//! | `rotate` | [`rotate`] / `PdfEngine::rotate` | 90/180/270°, 1-based page ranges |
//! | `extract_pages` | [`extract_pages`] / `PdfEngine::extract_pages` | 1-based range spec, e.g. `"1-3,8,10-12"` |
//! | `inspect` | [`inspect`] / `PdfEngine::inspect` | [`PdfInfo`] metadata report |
//! | `validate` | [`validate`] / `PdfEngine::validate` | [`ValidationReport`] structural checks |
//! | `extract_text` | [`extract_text`] / `PdfEngine::extract_text` | Best-effort per-page text |
//!
//! Unsupported operations (`compress`, `watermark`, `convert`, `ocr`,
//! `encrypt`, `decrypt`, ...) are **not** advertised in
//! [`EngineCapabilities`](crate::crates::core::engine::EngineCapabilities) and have no entry points here — the engine
//! never silently approximates unsupported behavior.
//!
//! # Contracts
//!
//! - **Page numbers are 1-based** everywhere, matching the CLI/MCP spec.
//! - **Page ranges** use one shared grammar parsed by
//!   [`parse_page_range`]: `"1-3,8,10-12"`, `"all"`; validated fully
//!   (including bounds) before any document work happens.
//! - **Encryption is rejected** for all transforming ops
//!   ([`PdfError::Encrypted`]); `inspect`/`validate` tolerate encrypted
//!   input and report it explicitly. No password-based decryption is
//!   implemented.
//! - **Limits** ([`EngineLimits`](crate::crates::core::engine::EngineLimits)) are enforced inside the ops: input
//!   size and page count at load time for every op, output count in
//!   `split`, and combined page count in `merge`. See [`ops`] for the
//!   exact enforcement points.
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
    PageText, PdfInfo, RotateDegrees, ValidationReport, extract_pages, extract_text, inspect,
    merge, parse_page_range, rotate, split, validate,
};
