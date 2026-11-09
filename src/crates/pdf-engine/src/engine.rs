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

    // -- page labels / outline ---------------------------------------------

    /// Parse a 1-based page-range spec against `page_count`.
    ///
    /// Pure helper shared by the ops; exposed for callers that need to
    /// validate a spec before dispatching work.
    pub fn parse_page_range(&self, spec: &str, page_count: u32) -> PdfResult<Vec<u32>> {
        ops::parse_page_range(spec, page_count)
    }

    /// Set literal page labels (`/PageLabels` number tree).
    ///
    /// `labels` is a list of `(page, label)` pairs; `page` is 1-based
    /// and `label` is the complete literal text for that page — see
    /// [`ops::set_page_labels`] docs.
    pub fn set_page_labels(&self, input: &[u8], labels: &[(u32, String)]) -> PdfResult<Vec<u8>> {
        ops::set_page_labels(input, labels, &self.limits)
    }

    /// Remove the document's `/PageLabels` entry.
    pub fn clear_page_labels(&self, input: &[u8]) -> PdfResult<Vec<u8>> {
        ops::clear_page_labels(input, &self.limits)
    }

    /// Read the effective page labels, one entry per page.
    pub fn get_page_labels(&self, input: &[u8]) -> PdfResult<Vec<Option<String>>> {
        ops::get_page_labels(input, &self.limits)
    }

    /// Replace the document outline with `items`.
    pub fn set_outline(&self, input: &[u8], items: &[ops::OutlineItemSpec]) -> PdfResult<Vec<u8>> {
        ops::set_outline(input, items, &self.limits)
    }

    /// Remove the document outline (`/Outlines` tree).
    pub fn remove_outline(&self, input: &[u8]) -> PdfResult<Vec<u8>> {
        ops::remove_outline(input, &self.limits)
    }

    /// List the document outline as flat [`ops::OutlineEntry`] rows.
    pub fn list_outline(&self, input: &[u8]) -> PdfResult<Vec<ops::OutlineEntry>> {
        ops::list_outline(input, &self.limits)
    }

    // -- page manipulation ---------------------------------------------------

    /// Set the `/Rotate` of selected pages (absolute value, not added).
    pub fn set_page_rotation(&self, input: &[u8], pages: &str, degrees: u16) -> PdfResult<Vec<u8>> {
        ops::set_page_rotation(input, pages, degrees, &self.limits)
    }

    /// Read the effective `/Rotate` of every page as `(page, degrees)`.
    pub fn get_page_rotations(&self, input: &[u8]) -> PdfResult<Vec<(u32, u16)>> {
        ops::get_page_rotations(input, &self.limits)
    }

    /// Normalize all page rotations into the content stream, removing
    /// redundant `/Rotate` entries.
    pub fn normalize_rotations(&self, input: &[u8]) -> PdfResult<Vec<u8>> {
        ops::normalize_rotations(input, &self.limits)
    }

    /// Set a `/CropBox` *view* crop on selected pages (unlike
    /// [`PdfEngine::crop`], which rewrites the box itself).
    pub fn set_view_crop(&self, input: &[u8], pages: &str, rect: [f64; 4]) -> PdfResult<Vec<u8>> {
        ops::set_view_crop(input, pages, rect, &self.limits)
    }

    /// Clear the `/CropBox` of selected pages.
    pub fn clear_crop(&self, input: &[u8], pages: &str) -> PdfResult<Vec<u8>> {
        ops::clear_crop(input, pages, &self.limits)
    }

    /// Insert `count` blank pages at `position`.
    ///
    /// Returns the output bytes plus the 1-based page numbers the new
    /// pages occupy.
    pub fn insert_blank(
        &self,
        input: &[u8],
        position: ops::InsertPos,
        count: u32,
        size: Option<ops::PageSize>,
    ) -> PdfResult<(Vec<u8>, Vec<u32>)> {
        ops::insert_blank(input, position, count, size, &self.limits)
    }

    /// Duplicate the pages selected by `pages`, `copies` times each.
    ///
    /// Returns the output bytes plus the sorted 1-based page numbers
    /// the copies occupy.
    pub fn duplicate_pages(
        &self,
        input: &[u8],
        pages: &str,
        copies: u32,
        position: ops::DupPos,
    ) -> PdfResult<(Vec<u8>, Vec<u32>)> {
        ops::duplicate_pages(input, pages, copies, position, &self.limits)
    }

    /// Remove provably blank pages. Returns the rewritten PDF plus a
    /// [`ops::BlankReport`] account.
    pub fn remove_blank_pages(&self, input: &[u8]) -> PdfResult<ops::BlankReport> {
        ops::remove_blank_pages(input, &self.limits)
    }

    /// Interleave pages of multiple inputs round-robin.
    pub fn interleave(&self, inputs: &[&[u8]]) -> PdfResult<Vec<u8>> {
        ops::interleave(inputs, &self.limits)
    }

    /// Merge multiple inputs using a [`ops::MergeMode`] ordering.
    pub fn merge_mode(&self, inputs: &[&[u8]], mode: ops::MergeMode) -> PdfResult<Vec<u8>> {
        ops::merge_mode(inputs, mode, &self.limits)
    }

    /// Imposition: tile `cols`×`rows` pages per sheet for the selected
    /// pages.
    pub fn nup(
        &self,
        input: &[u8],
        cols: u32,
        rows: u32,
        pages: Option<&str>,
    ) -> PdfResult<Vec<u8>> {
        ops::nup(input, cols, rows, pages, &self.limits)
    }

    /// Lay out the selected pages on a single large page.
    pub fn single_page(
        &self,
        input: &[u8],
        pages: Option<&str>,
        cols: Option<u32>,
    ) -> PdfResult<Vec<u8>> {
        ops::single_page(input, pages, cols, &self.limits)
    }

    /// Posterize: split each selected page into a `tile_cols`×`tile_rows`
    /// grid.
    pub fn poster(
        &self,
        input: &[u8],
        pages: Option<&str>,
        tile_cols: u32,
        tile_rows: u32,
    ) -> PdfResult<Vec<u8>> {
        ops::poster(input, pages, tile_cols, tile_rows, &self.limits)
    }

    /// Arrange pages for booklet printing per [`ops::BookletSpec`].
    pub fn booklet(&self, input: &[u8], spec: ops::BookletSpec) -> PdfResult<Vec<u8>> {
        ops::booklet(input, spec, &self.limits)
    }

    /// Scale page content per [`ops::scale_content`].
    pub fn scale_content(
        &self,
        input: &[u8],
        pages: &str,
        scale_x: f64,
        scale_y: Option<f64>,
    ) -> PdfResult<Vec<u8>> {
        ops::scale_content(input, pages, scale_x, scale_y, &self.limits)
    }

    /// Split `input` into multiple documents per [`ops::SplitMode`].
    pub fn split_mode(&self, input: &[u8], mode: ops::SplitMode<'_>) -> PdfResult<Vec<Vec<u8>>> {
        ops::split_mode(input, mode, &self.limits)
    }

    /// Split at outline/bookmark boundaries at the given depth.
    pub fn split_at_outline(&self, input: &[u8], level: u32) -> PdfResult<Vec<Vec<u8>>> {
        ops::split_at_outline(input, level, &self.limits)
    }

    /// Split the portfolio `input` into its embedded member documents.
    /// See [`ops::portfolio`] module docs.
    pub fn list_portfolio(&self, input: &[u8]) -> PdfResult<Vec<ops::PortfolioEntry>> {
        ops::list_portfolio(input, &self.limits)
    }

    /// Create a PDF portfolio from `inputs`, named by `names`
    /// (empty names get generated ones — see [`ops::create_portfolio`]).
    pub fn create_portfolio(&self, inputs: &[&[u8]], names: &[String]) -> PdfResult<Vec<u8>> {
        ops::create_portfolio(inputs, names, &self.limits)
    }

    // -- color / content ------------------------------------------------------

    /// Convert color operators on the selected pages to grayscale.
    ///
    /// Returns the rewritten bytes plus a [`ops::ColorReport`].
    pub fn grayscale(
        &self,
        input: &[u8],
        pages: Option<&str>,
    ) -> PdfResult<(Vec<u8>, ops::ColorReport)> {
        ops::grayscale(input, pages, &self.limits)
    }

    /// Invert color operators on the selected pages.
    ///
    /// Returns the rewritten bytes plus a [`ops::ColorReport`].
    pub fn invert_colors(
        &self,
        input: &[u8],
        pages: Option<&str>,
    ) -> PdfResult<(Vec<u8>, ops::ColorReport)> {
        ops::invert_colors(input, pages, &self.limits)
    }

    /// Draw vector shapes per [`ops::DrawSpec`].
    pub fn draw_shapes(&self, input: &[u8], spec: &ops::DrawSpec) -> PdfResult<Vec<u8>> {
        ops::draw_shapes(input, spec, &self.limits)
    }

    /// Remove image XObjects from the selected pages.
    ///
    /// Returns a [`ops::RemoveReport`] that includes the rewritten
    /// bytes in its `bytes` field.
    pub fn remove_images(&self, input: &[u8], pages: Option<&str>) -> PdfResult<ops::RemoveReport> {
        ops::remove_images(input, pages, &self.limits)
    }

    /// Replace one image XObject on a page with new PNG/JPEG bytes.
    pub fn replace_image(
        &self,
        input: &[u8],
        page: u32,
        xobject_name: &str,
        new_image: &[u8],
        format: ops::ImageInputFormat,
    ) -> PdfResult<Vec<u8>> {
        ops::replace_image(input, page, xobject_name, new_image, format, &self.limits)
    }

    // -- annotations / freehand / text markup --------------------------------

    /// Add a sticky-note comment annotation per [`ops::CommentSpec`].
    pub fn add_comment(&self, input: &[u8], spec: &ops::CommentSpec) -> PdfResult<Vec<u8>> {
        ops::add_comment(input, spec, &self.limits)
    }

    /// Add a free-text annotation per [`ops::FreeTextSpec`].
    pub fn add_freetext(&self, input: &[u8], spec: &ops::FreeTextSpec) -> PdfResult<Vec<u8>> {
        ops::add_freetext(input, spec, &self.limits)
    }

    /// List annotations as flat [`ops::AnnotationInfo`] rows.
    pub fn list_annotations(&self, input: &[u8]) -> PdfResult<Vec<ops::AnnotationInfo>> {
        ops::list_annotations(input, &self.limits)
    }

    /// Remove annotations matching `filter`.
    pub fn remove_annotations(
        &self,
        input: &[u8],
        filter: &ops::AnnotFilter,
    ) -> PdfResult<Vec<u8>> {
        ops::remove_annotations(input, filter, &self.limits)
    }

    /// List article threads as [`ops::ArticleInfo`] rows.
    pub fn list_articles(&self, input: &[u8]) -> PdfResult<Vec<ops::ArticleInfo>> {
        ops::list_articles(input, &self.limits)
    }

    /// Remove all article threads.
    pub fn remove_articles(&self, input: &[u8]) -> PdfResult<Vec<u8>> {
        ops::remove_articles(input, &self.limits)
    }

    /// Add freehand ink annotations per [`ops::InkSpec`].
    pub fn add_ink(&self, input: &[u8], spec: &ops::InkSpec) -> PdfResult<Vec<u8>> {
        ops::add_ink(input, spec, &self.limits)
    }

    /// List ink annotations as [`ops::InkInfo`] rows.
    pub fn list_ink(&self, input: &[u8]) -> PdfResult<Vec<ops::InkInfo>> {
        ops::list_ink(input, &self.limits)
    }

    /// Remove ink annotations matching `filter`.
    pub fn remove_ink(&self, input: &[u8], filter: &ops::AnnotFilter) -> PdfResult<Vec<u8>> {
        ops::remove_ink(input, filter, &self.limits)
    }

    /// Highlight/underline/strike through text per [`ops::MarkupKind`].
    pub fn text_markup(
        &self,
        input: &[u8],
        kind: ops::MarkupKind,
        pages: Option<&str>,
        color: Option<[u8; 3]>,
    ) -> PdfResult<Vec<u8>> {
        ops::text_markup(input, kind, pages, color, &self.limits)
    }

    /// Find-and-replace text strings in page content streams.
    ///
    /// Returns the rewritten bytes plus an [`ops::EditReport`].
    pub fn edit_text(&self, input: &[u8], edit: &ops::TextEdit) -> PdfResult<ops::EditOutcome> {
        ops::edit_text(input, edit, &self.limits)
    }

    // -- forms ---------------------------------------------------------------

    /// List AcroForm fields as [`ops::FieldInfo`] rows.
    pub fn list_form_fields(&self, input: &[u8]) -> PdfResult<Vec<ops::FieldInfo>> {
        ops::list_form_fields(input, &self.limits)
    }

    /// Create a text form field per [`ops::TextFieldSpec`].
    pub fn create_text_field(&self, input: &[u8], spec: &ops::TextFieldSpec) -> PdfResult<Vec<u8>> {
        ops::create_text_field(input, spec, &self.limits)
    }

    /// Delete the form field named `name` (and its widget).
    pub fn delete_form_field(&self, input: &[u8], name: &str) -> PdfResult<Vec<u8>> {
        ops::delete_form_field(input, name, &self.limits)
    }

    /// Fill form fields from `(name, value)` pairs.
    pub fn fill_form(&self, input: &[u8], values: &[(String, String)]) -> PdfResult<Vec<u8>> {
        ops::fill_form(input, values, &self.limits)
    }

    /// Clear all form field values.
    pub fn clear_form(&self, input: &[u8]) -> PdfResult<Vec<u8>> {
        ops::clear_form(input, &self.limits)
    }

    /// Clear the ReadOnly flag on the named fields (`None` = all).
    ///
    /// Returns the rewritten bytes plus a [`ops::FieldFlagReport`].
    pub fn unlock_fields(
        &self,
        input: &[u8],
        names: Option<&[String]>,
    ) -> PdfResult<(Vec<u8>, ops::FieldFlagReport)> {
        ops::unlock_fields(input, names, &self.limits)
    }

    /// Set the ReadOnly flag on the named fields (`None` = all).
    ///
    /// Returns the rewritten bytes plus a [`ops::FieldFlagReport`].
    pub fn lock_fields(
        &self,
        input: &[u8],
        names: Option<&[String]>,
    ) -> PdfResult<(Vec<u8>, ops::FieldFlagReport)> {
        ops::lock_fields(input, names, &self.limits)
    }

    // -- headers / footers / marks -------------------------------------------

    /// Draw header/footer text and page numbers per
    /// [`ops::HeaderFooterSpec`].
    pub fn headers_footers(
        &self,
        input: &[u8],
        spec: &ops::HeaderFooterSpec,
        pages: Option<&str>,
    ) -> PdfResult<Vec<u8>> {
        ops::headers_footers(input, spec, pages, &self.limits)
    }

    /// Add page numbers in the given [`ops::PageNumberStyle`].
    pub fn add_page_numbers(
        &self,
        input: &[u8],
        style: ops::PageNumberStyle,
    ) -> PdfResult<Vec<u8>> {
        ops::add_page_numbers(input, style, &self.limits)
    }

    /// Add print marks (crop marks, registration mark) per
    /// [`ops::PrintMarksSpec`].
    pub fn add_print_marks(
        &self,
        input: &[u8],
        spec: &ops::PrintMarksSpec<'_>,
    ) -> PdfResult<Vec<u8>> {
        ops::add_print_marks(input, spec, &self.limits)
    }

    // -- metadata / conformance ----------------------------------------------

    /// Patch the trailer `/Info` dictionary per [`ops::MetadataPatch`].
    pub fn set_metadata(&self, input: &[u8], patch: &ops::MetadataPatch) -> PdfResult<Vec<u8>> {
        ops::set_metadata(input, patch, &self.limits)
    }

    /// Clear the trailer `/Info` dictionary.
    pub fn clear_metadata(&self, input: &[u8]) -> PdfResult<Vec<u8>> {
        ops::clear_metadata(input, &self.limits)
    }

    /// Replace the XMP metadata packet with `xmp_packet` bytes.
    pub fn set_xmp(&self, input: &[u8], xmp_packet: &[u8]) -> PdfResult<Vec<u8>> {
        ops::set_xmp(input, xmp_packet, &self.limits)
    }

    /// Extract the raw XMP metadata packet bytes.
    pub fn get_xmp(&self, input: &[u8]) -> PdfResult<Vec<u8>> {
        ops::get_xmp(input, &self.limits)
    }

    /// Declare PDF/A conformance via XMP. Returns the rewritten bytes
    /// plus a [`ops::PdfaReport`] — a declaration, not validated
    /// conformance.
    pub fn to_pdfa(&self, input: &[u8], level: ops::PdfaLevel) -> PdfResult<ops::PdfaReport> {
        ops::to_pdfa(input, level, &self.limits)
    }

    /// Declare PDF/UA conformance via XMP.
    pub fn to_pdfua(&self, input: &[u8], level: ops::PdfuaLevel) -> PdfResult<ops::PdfuaReport> {
        ops::to_pdfua(input, level, &self.limits)
    }

    /// Declare PDF/X conformance via XMP.
    pub fn to_pdfx(&self, input: &[u8], level: ops::PdfxLevel) -> PdfResult<ops::PdfxReport> {
        ops::to_pdfx(input, level, &self.limits)
    }

    /// Request Fast Web View linearization. The returned
    /// [`ops::LinearizeReport`] honestly reports whether the output
    /// really carries `/Linearized` data (today: never — see op docs).
    pub fn linearize(&self, input: &[u8]) -> PdfResult<ops::LinearizeReport> {
        ops::linearize(input, &self.limits)
    }

    /// Garbage-collect unreachable objects and recompress.
    ///
    /// Returns a [`ops::GcReport`] whose `bytes` field holds the
    /// rewritten document.
    pub fn garbage_collect(&self, input: &[u8]) -> PdfResult<ops::GcReport> {
        ops::garbage_collect(input, &self.limits)
    }

    /// Inject an invisible OCR text layer over the selected pages.
    ///
    /// Note: the underlying op discards its rewritten bytes — only the
    /// [`ops::SearchableReport`] is returned (see op docs).
    pub fn make_searchable(
        &self,
        input: &[u8],
        pages: Option<&str>,
    ) -> PdfResult<ops::SearchableReport> {
        ops::make_searchable(input, pages, &self.limits)
    }

    // -- object removal --------------------------------------------------------

    /// Remove the named indirect objects.
    pub fn remove_named_objects(
        &self,
        input: &[u8],
        names: &[String],
    ) -> PdfResult<ops::RemoveObjectsReport> {
        ops::remove_named_objects(input, names, &self.limits)
    }

    /// Remove all embedded files.
    pub fn remove_embedded_files(&self, input: &[u8]) -> PdfResult<Vec<u8>> {
        ops::remove_embedded_files(input, &self.limits)
    }

    /// Remove all JavaScript name-tree entries.
    pub fn remove_javascript_names(&self, input: &[u8]) -> PdfResult<Vec<u8>> {
        ops::remove_javascript_names(input, &self.limits)
    }

    // -- signatures / timestamps -----------------------------------------------

    /// List document-timestamp status of every signature field.
    pub fn list_signature_timestamps(&self, input: &[u8]) -> PdfResult<Vec<ops::SignedTimestamp>> {
        ops::list_signature_timestamps(input, &self.limits)
    }

    /// Whether the document carries at least one document timestamp.
    pub fn has_timestamps(&self, input: &[u8]) -> PdfResult<bool> {
        ops::has_timestamps(input, &self.limits)
    }

    /// Compute a digest over `input` per RFC 3161 timestamping.
    pub fn document_digest(
        &self,
        input: &[u8],
        algorithm: ops::TimestampAlgorithm,
    ) -> PdfResult<ops::DigestReport> {
        ops::document_digest(input, algorithm, &self.limits)
    }

    /// Digest `input` and build the DER `TimeStampReq` for it.
    pub fn timestamp_query(
        &self,
        input: &[u8],
        algorithm: ops::TimestampAlgorithm,
    ) -> PdfResult<Vec<u8>> {
        ops::timestamp_query(input, algorithm, &self.limits)
    }

    /// Build a DER `TimeStampReq` for an already-computed `digest`.
    pub fn build_timestamp_request(
        &self,
        digest: &[u8],
        algorithm: ops::TimestampAlgorithm,
    ) -> PdfResult<Vec<u8>> {
        ops::build_timestamp_request(digest, algorithm)
    }

    /// POST a `TimeStampReq` to an external TSA (`https://` only) and
    /// return the raw token bytes.
    pub fn request_external_timestamp(
        &self,
        tsq: &[u8],
        tsa_url: &str,
    ) -> PdfResult<ops::TimestampToken> {
        ops::request_external_timestamp(tsq, tsa_url, &self.limits)
    }

    /// End-to-end RFC 3161 timestamping: digest → TSQ → TSA → token.
    pub fn timestamp_document(
        &self,
        input: &[u8],
        algorithm: ops::TimestampAlgorithm,
        tsa_url: &str,
    ) -> PdfResult<ops::DocumentTimestamp> {
        ops::timestamp_document(input, algorithm, tsa_url, &self.limits)
    }

    // ---- round-2 advanced ops ----

    pub fn set_transition(
        &self,
        input: &[u8],
        pages: &str,
        spec: &ops::TransitionSpec,
    ) -> PdfResult<Vec<u8>> {
        ops::set_transition(input, pages, spec, &self.limits)
    }

    pub fn clear_transitions(&self, input: &[u8], pages: Option<&str>) -> PdfResult<Vec<u8>> {
        ops::clear_transitions(input, pages, &self.limits)
    }

    pub fn list_transitions(&self, input: &[u8]) -> PdfResult<Vec<ops::PageTransition>> {
        ops::list_transitions(input, &self.limits)
    }

    pub fn package_documents(
        &self,
        inputs: &[&[u8]],
        cover_title: &str,
        layout: ops::PackageLayout,
    ) -> PdfResult<Vec<u8>> {
        ops::package_documents(inputs, cover_title, layout, &self.limits)
    }

    pub fn detect_orientation(&self, input: &[u8]) -> PdfResult<Vec<ops::PageOrientation>> {
        ops::detect_orientation(input, &self.limits)
    }

    pub fn normalize_orientation(&self, input: &[u8], mode: ops::OrientMode) -> PdfResult<Vec<u8>> {
        ops::normalize_orientation(input, mode, &self.limits)
    }

    pub fn redact_regions(
        &self,
        input: &[u8],
        regions: &[ops::Redaction],
        label: Option<&str>,
    ) -> PdfResult<(Vec<u8>, ops::RedactReport)> {
        ops::redact_regions(input, regions, label, &self.limits)
    }

    pub fn redact_pages(
        &self,
        input: &[u8],
        pages: &str,
        replacement: ops::RedactReplacement,
    ) -> PdfResult<Vec<u8>> {
        ops::redact_pages(input, pages, replacement, &self.limits)
    }

    pub fn clean_scan(
        &self,
        input: &[u8],
        pages: Option<&str>,
        mode: ops::CleanMode,
    ) -> PdfResult<(Vec<u8>, ops::CleanReport)> {
        ops::clean_scan(input, pages, mode, &self.limits)
    }

    pub fn inspect_fonts(&self, input: &[u8]) -> PdfResult<Vec<ops::FontInfo>> {
        ops::inspect_fonts(input, &self.limits)
    }

    pub fn inspect_javascript(&self, input: &[u8]) -> PdfResult<Vec<ops::JsInfo>> {
        ops::inspect_javascript(input, &self.limits)
    }

    pub fn get_document_dates(&self, input: &[u8]) -> PdfResult<ops::DocDates> {
        ops::get_document_dates(input, &self.limits)
    }

    pub fn set_document_dates(
        &self,
        input: &[u8],
        creation: Option<&str>,
        mod_date: Option<&str>,
    ) -> PdfResult<Vec<u8>> {
        ops::set_document_dates(input, creation, mod_date, &self.limits)
    }

    pub fn set_page_size(
        &self,
        input: &[u8],
        pages: &str,
        spec: &ops::PageSizeSpec,
    ) -> PdfResult<Vec<u8>> {
        ops::set_page_size(input, pages, spec, &self.limits)
    }

    pub fn get_page_sizes(&self, input: &[u8]) -> PdfResult<Vec<ops::PageSizeInfo>> {
        ops::get_page_sizes(input, &self.limits)
    }

    pub fn list_layers(&self, input: &[u8]) -> PdfResult<Vec<ops::LayerInfo>> {
        ops::list_layers(input, &self.limits)
    }

    pub fn set_layer_visibility(
        &self,
        input: &[u8],
        names: &[String],
        visible: bool,
    ) -> PdfResult<Vec<u8>> {
        ops::set_layer_visibility(input, names, visible, &self.limits)
    }

    pub fn remove_oc_properties(&self, input: &[u8]) -> PdfResult<Vec<u8>> {
        ops::remove_oc_properties(input, &self.limits)
    }

    pub fn inspect_structure(&self, input: &[u8]) -> PdfResult<ops::StructReport> {
        ops::inspect_structure(input, &self.limits)
    }

    pub fn add_named_destinations(
        &self,
        input: &[u8],
        additions: &[ops::NamedDest],
    ) -> PdfResult<Vec<u8>> {
        ops::add_named_destinations(input, additions, &self.limits)
    }

    pub fn list_named_destinations(&self, input: &[u8]) -> PdfResult<Vec<ops::NamedDestInfo>> {
        ops::list_named_destinations(input, &self.limits)
    }

    pub fn remove_named_destination(&self, input: &[u8], name: &str) -> PdfResult<Vec<u8>> {
        ops::remove_named_destination(input, name, &self.limits)
    }

    pub fn edit_field_props(
        &self,
        input: &[u8],
        patches: &[ops::FieldPatch],
    ) -> PdfResult<(Vec<u8>, ops::FieldPatchReport)> {
        ops::edit_field_props(input, patches, &self.limits)
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
