//! [`PdfEngine`] — the `ProcessingEngine` implementation backed by the
//! in-process `lopdf` library.
//!
//! Most operations are synchronous, CPU-bound functions over
//! in-memory byte buffers (see the `ops` module). They are exposed
//! here as plain methods rather than futures: callers running them on
//! an async runtime should wrap them in `spawn_blocking` themselves.
//! The engine itself holds no mutable state — only an
//! [`EngineLimits`] snapshot — so it is trivially `Send + Sync`.
//!
//! A few ops (`render`, `ocr`, `ocr_languages`) additionally shell out
//! to external binaries (`pdftoppm`, `tesseract`); when those are
//! missing the ops fail with [`PdfError::MissingTool`](crate::crates::pdf_engine::PdfError::MissingTool) and
//! [`ProcessingEngine::health`] reports [`EngineHealthStatus::Degraded`]
//! naming the absent tools. Nothing is ever faked.

use crate::crates::core::engine::{
    EngineCapabilities, EngineFuture, EngineHealth, EngineHealthStatus, EngineKind, EngineLimits,
    EngineOperation, ProcessingEngine,
};

use crate::crates::pdf_engine::error::PdfResult;
use crate::crates::pdf_engine::ops::raster;
use crate::crates::pdf_engine::ops::{self, PageText, PdfInfo, RotateDegrees, ValidationReport};

/// The PDF processing engine.
///
/// Backed by `lopdf`, which is compiled into this crate — there is no
/// external binary or service required for the core operations, so
/// [`ProcessingEngine::health`] reports
/// [`EngineHealthStatus::Healthy`] unless the optional raster/OCR tools
/// (`pdftoppm`, `tesseract`) are missing, in which case it reports
/// [`EngineHealthStatus::Degraded`] with the missing tool names.
///
/// # Limits
///
/// [`PdfEngine::new`] uses [`EngineLimits::default`] (no engine-side
/// limits; upstream application limits still apply). Use
/// [`PdfEngine::with_limits`] to enforce `max_input_bytes`, `max_pages`,
/// `max_pixels` and `max_outputs`. Enforcement happens inside the ops —
/// see the `ops` module for details.
#[derive(Debug, Clone)]
pub struct PdfEngine {
    limits: EngineLimits,
}

/// External tools probed by [`PdfEngine::health`]. `pdftoppm` backs
/// `render`/`ocr`; `tesseract` backs `ocr`/`ocr_languages`; `openssl`
/// backs `sign`/`verify_signatures_crypto`.
const RENDER_TOOL: &str = raster::RENDER_TOOL;
const OCR_TOOL: &str = raster::OCR_TOOL;
const SIGN_TOOL: &str = "openssl";

impl PdfEngine {
    /// Create an engine with no engine-side resource limits.
    pub fn new() -> Self {
        Self::with_limits(EngineLimits::default())
    }

    /// Create an engine enforcing the given [`EngineLimits`].
    pub fn with_limits(limits: EngineLimits) -> Self {
        Self { limits }
    }

    /// The limits this engine enforces.
    pub fn limits(&self) -> &EngineLimits {
        &self.limits
    }

    // -- page structure -------------------------------------------------

    /// Number of pages in the document.
    pub fn page_count(&self, input: &[u8]) -> PdfResult<u32> {
        ops::page_count(input, &self.limits)
    }

    /// Merge multiple PDFs into one, in input order.
    ///
    /// See [`ops::merge`] for semantics and limitations (outlines,
    /// signatures and other document-level structures are not carried
    /// over).
    pub fn merge(&self, inputs: &[&[u8]]) -> PdfResult<Vec<u8>> {
        ops::merge(inputs, &self.limits)
    }

    /// Split a PDF into files of at most `pages_per_file` pages.
    ///
    /// See [`ops::split`].
    pub fn split(&self, input: &[u8], pages_per_file: u32) -> PdfResult<Vec<Vec<u8>>> {
        ops::split(input, pages_per_file, &self.limits)
    }

    /// Extract the pages selected by a 1-based range spec into a new PDF.
    pub fn extract_pages(&self, input: &[u8], pages: &str) -> PdfResult<Vec<u8>> {
        ops::extract_pages(input, pages, &self.limits)
    }

    /// Remove the pages selected by a 1-based range spec.
    pub fn remove_pages(&self, input: &[u8], pages: &str) -> PdfResult<Vec<u8>> {
        ops::remove_pages(input, pages, &self.limits)
    }

    /// Reorder all pages: `order` is a comma-separated permutation of
    /// every page number exactly once (1-based, e.g. `"3,1,2"`).
    pub fn reorder(&self, input: &[u8], order: &str) -> PdfResult<Vec<u8>> {
        ops::reorder(input, order, &self.limits)
    }

    /// Rotate pages selected by a 1-based range spec
    /// (`"1-3,8"`, `"all"`) by `degrees`.
    ///
    /// `degrees` may also be given as a raw `u16` via
    /// [`RotateDegrees::try_from`], which rejects anything but
    /// 90/180/270.
    pub fn rotate(&self, input: &[u8], pages: &str, degrees: RotateDegrees) -> PdfResult<Vec<u8>> {
        ops::rotate(input, pages, degrees, &self.limits)
    }

    /// Convenience wrapper accepting raw degree values.
    pub fn rotate_degrees(&self, input: &[u8], pages: &str, degrees: u16) -> PdfResult<Vec<u8>> {
        self.rotate(input, pages, RotateDegrees::try_from(degrees)?)
    }

    /// Set the `/CropBox` of selected pages to `crop_box`
    /// (`[x0, y0, x1, y1]` in points).
    pub fn crop(&self, input: &[u8], pages: &str, crop_box: [f32; 4]) -> PdfResult<Vec<u8>> {
        ops::crop(input, pages, crop_box, &self.limits)
    }

    /// Resize the page boxes of selected pages per `spec`
    /// ([`ops::ResizeSpec`]). Page content is not scaled — see
    /// [`ops::resize`] docs.
    pub fn resize(&self, input: &[u8], pages: &str, spec: &ops::ResizeSpec) -> PdfResult<Vec<u8>> {
        ops::resize(input, pages, spec, &self.limits)
    }

    // -- document-level ops ----------------------------------------------

    /// Recompress the document (object streams, xref streams, stream
    /// recompression where lopdf supports it).
    pub fn compress(&self, input: &[u8]) -> PdfResult<Vec<u8>> {
        ops::compress(input, &self.limits)
    }

    /// Attempt a normalized repair: the load already recovers broken
    /// xref tables; the report lists what was found.
    pub fn repair(&self, input: &[u8]) -> PdfResult<ops::RepairReport> {
        ops::repair(input, &self.limits)
    }

    /// Best-effort flattening of annotation/appearance content into
    /// page content streams.
    pub fn flatten(&self, input: &[u8]) -> PdfResult<Vec<u8>> {
        ops::flatten(input, &self.limits)
    }

    /// Overlay one PDF's pages onto another's as form XObjects.
    pub fn overlay(
        &self,
        input: &[u8],
        overlay_pdf: &[u8],
        pages: Option<&str>,
    ) -> PdfResult<Vec<u8>> {
        ops::overlay(input, overlay_pdf, pages, &self.limits)
    }

    /// Strip active-content vectors (OpenAction, JavaScript, Launch
    /// actions, embedded files, ...). See [`ops::sanitize`] for the
    /// exact list.
    pub fn sanitize(&self, input: &[u8]) -> PdfResult<Vec<u8>> {
        ops::sanitize(input, &self.limits)
    }

    // -- marks ------------------------------------------------------------

    /// Apply a text watermark per [`ops::WatermarkOptions`].
    pub fn watermark(&self, input: &[u8], opts: &ops::WatermarkOptions) -> PdfResult<Vec<u8>> {
        ops::watermark(input, opts, &self.limits)
    }

    /// Apply a text or image stamp per [`ops::StampOptions`].
    pub fn stamp(&self, input: &[u8], opts: &ops::StampOptions) -> PdfResult<Vec<u8>> {
        ops::stamp(input, opts, &self.limits)
    }

    /// Draw text at arbitrary page coordinates per [`ops::DrawTextSpec`]
    /// (x, y in points, bottom-left origin).
    pub fn draw_text(&self, input: &[u8], spec: &ops::DrawTextSpec) -> PdfResult<Vec<u8>> {
        ops::draw_text(input, spec, &self.limits)
    }

    // -- security ---------------------------------------------------------

    /// Remove the given text strings from page content (best-effort —
    /// see [`ops::redact`] docs).
    pub fn redact(
        &self,
        input: &[u8],
        terms: &[String],
        pages: Option<&str>,
    ) -> PdfResult<Vec<u8>> {
        ops::redact(input, terms, pages, &self.limits)
    }

    /// Encrypt the document (AES-256) with user/owner passwords and
    /// permission flags.
    pub fn encrypt(
        &self,
        input: &[u8],
        user_password: &str,
        owner_password: Option<&str>,
        permissions: ops::PdfPermissions,
    ) -> PdfResult<Vec<u8>> {
        ops::encrypt(
            input,
            user_password,
            owner_password,
            permissions,
            &self.limits,
        )
    }

    /// Decrypt an encrypted document with `password`.
    pub fn decrypt(&self, input: &[u8], password: &str) -> PdfResult<Vec<u8>> {
        ops::decrypt(input, password, &self.limits)
    }

    /// Structural signature report — presence and byte-range sanity,
    /// **not** cryptographic verification.
    pub fn verify_signatures(&self, input: &[u8]) -> PdfResult<ops::SignatureReport> {
        ops::verify_signatures(input, &self.limits)
    }

    /// Cryptographic signature verification via `openssl cms -verify`:
    /// digest validity over the declared `/ByteRange`, signer certificate
    /// subject, and — when `trust_anchors` (PEM CA certificates) are
    /// supplied — chain verification. Timestamp tokens are detected but
    /// never verified; revocation is never checked. Requires `openssl`
    /// on `PATH` when signatures with `/Contents` are present.
    pub fn verify_signatures_crypto(
        &self,
        input: &[u8],
        trust_anchors: Option<&[u8]>,
    ) -> PdfResult<ops::CryptoSignatureReport> {
        ops::verify_signatures_crypto(input, &self.limits, trust_anchors)
    }

    /// Digitally sign `input` with an X.509 certificate, producing a
    /// real detached-CMS signature (`/ByteRange` + `/Contents`) via
    /// `openssl cms -sign`. See [`ops::sign`] for the produced
    /// structure and its limitations (whole-document signature, full
    /// rewrite that invalidates existing signatures, no RFC 3161
    /// timestamping). Requires `openssl` on `PATH`.
    pub fn sign(
        &self,
        input: &[u8],
        credentials: &ops::SignCredentials,
        options: &ops::SignOptions,
    ) -> PdfResult<Vec<u8>> {
        ops::sign(input, credentials, options, &self.limits)
    }

    /// Remove all signature fields, their widget annotations and the
    /// catalog `/Perms` signature references. See [`ops::remove_signatures`]
    /// for what is and is not stripped.
    pub fn remove_signatures(&self, input: &[u8]) -> PdfResult<Vec<u8>> {
        ops::remove_signatures(input, &self.limits)
    }

    // -- inspection / extraction -------------------------------------------

    /// Inspect a PDF: page count, metadata, encryption state, size.
    ///
    /// Tolerates encrypted input (reported via [`PdfInfo::encrypted`]).
    pub fn inspect(&self, input: &[u8]) -> PdfResult<PdfInfo> {
        ops::inspect(input, &self.limits)
    }

    /// Extended metadata extraction: Info dictionary, dates,
    /// keywords, XMP, page size.
    pub fn extract_metadata(&self, input: &[u8]) -> PdfResult<ops::PdfMetadata> {
        ops::extract_metadata(input, &self.limits)
    }

    /// Validate PDF structure; returns a report rather than failing on
    /// the first anomaly. Malformed/unloadable input is still an error.
    pub fn validate(&self, input: &[u8]) -> PdfResult<ValidationReport> {
        ops::validate(input, &self.limits)
    }

    /// Extract embedded images from page resources.
    pub fn extract_images(
        &self,
        input: &[u8],
        pages: Option<&str>,
    ) -> PdfResult<Vec<ops::EmbeddedImage>> {
        ops::extract_images(input, pages, &self.limits)
    }

    /// Best-effort text extraction, one [`PageText`] per page.
    ///
    /// `pages` is an optional range spec; `None` extracts every page.
    pub fn extract_text(&self, input: &[u8], pages: Option<&str>) -> PdfResult<Vec<PageText>> {
        ops::extract_text(input, pages, &self.limits)
    }

    /// Convert the text layer to plain text or Markdown bytes.
    pub fn convert(
        &self,
        input: &[u8],
        target: ops::ConvertTarget,
        pages: Option<&str>,
    ) -> PdfResult<Vec<u8>> {
        ops::convert(input, target, pages, &self.limits)
    }

    /// Render a page to PNG or JPEG via `pdftoppm` (first selected
    /// page only — see [`ops::render`]).
    pub fn render(
        &self,
        input: &[u8],
        pages: Option<&str>,
        dpi: Option<u32>,
        format: ops::RenderFormat,
    ) -> PdfResult<Vec<u8>> {
        ops::render(input, pages, dpi, format, &self.limits)
    }

    /// OCR selected pages via `pdftoppm` + `tesseract`.
    pub fn ocr(&self, input: &[u8], pages: Option<&str>) -> PdfResult<Vec<PageText>> {
        ops::ocr(input, pages, &self.limits)
    }

    /// List installed tesseract language codes.
    pub fn ocr_languages(&self) -> PdfResult<Vec<String>> {
        ops::ocr_languages()
    }

    /// Compare two PDFs (text diff / structure / bytes).
    pub fn compare(
        &self,
        input_a: &[u8],
        input_b: &[u8],
        mode: ops::CompareMode,
    ) -> PdfResult<ops::CompareReport> {
        ops::compare(input_a, input_b, mode, &self.limits)
    }
}

impl Default for PdfEngine {
    fn default() -> Self {
        Self::new()
    }
}

impl ProcessingEngine for PdfEngine {
    fn name(&self) -> &'static str {
        "pdf"
    }

    fn version(&self) -> &'static str {
        env!("CARGO_PKG_VERSION")
    }

    fn kind(&self) -> EngineKind {
        EngineKind::Pdf
    }

    fn capabilities(&self) -> EngineCapabilities {
        EngineCapabilities {
            operations: vec![
                EngineOperation::Merge,
                EngineOperation::Split,
                EngineOperation::Rotate,
                EngineOperation::Resize,
                EngineOperation::Crop,
                EngineOperation::Reorder,
                EngineOperation::RemovePages,
                EngineOperation::ExtractPages,
                EngineOperation::Compress,
                EngineOperation::Repair,
                EngineOperation::Flatten,
                EngineOperation::Overlay,
                EngineOperation::Watermark,
                EngineOperation::Stamp,
                EngineOperation::DrawText,
                EngineOperation::Redact,
                EngineOperation::Sanitize,
                EngineOperation::VerifySignature,
                EngineOperation::Sign,
                EngineOperation::RemoveSignatures,
                EngineOperation::Validate,
                EngineOperation::Inspect,
                EngineOperation::PageCount,
                EngineOperation::ExtractMetadata,
                EngineOperation::ExtractImages,
                EngineOperation::ExtractText,
                EngineOperation::Render,
                EngineOperation::Compare,
                EngineOperation::Convert,
                EngineOperation::Ocr,
                EngineOperation::Encrypt,
                EngineOperation::Decrypt,
            ],
            input_formats: vec!["pdf".to_string()],
            output_formats: vec![
                "pdf".to_string(),
                "text".to_string(),
                "markdown".to_string(),
                "png".to_string(),
                "jpeg".to_string(),
            ],
            limits: self.limits.clone(),
        }
    }

    fn health<'a>(&'a self) -> EngineFuture<'a, EngineHealth> {
        Box::pin(async move {
            // lopdf is embedded — the core ops need nothing external.
            // `render`/`ocr`/`sign` degrade gracefully to MissingTool
            // errors; report that here so `engine health`/`doctor` see it.
            let mut missing = Vec::new();
            if !raster::tool_available(RENDER_TOOL) {
                missing.push(RENDER_TOOL);
            }
            if !raster::tool_available(OCR_TOOL) {
                missing.push(OCR_TOOL);
            }
            if !raster::tool_available(SIGN_TOOL) {
                missing.push(SIGN_TOOL);
            }
            if missing.is_empty() {
                EngineHealth {
                    status: EngineHealthStatus::Healthy,
                    message: None,
                }
            } else {
                EngineHealth {
                    status: EngineHealthStatus::Degraded,
                    message: Some(format!(
                        "external tools not found on PATH (render/ocr/sign/crypto-verify unavailable): {}",
                        missing.join(", ")
                    )),
                }
            }
        })
    }
}
