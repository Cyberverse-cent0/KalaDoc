//! Pipeline model — serde-serializable definitions, steps, and outputs.

use serde::{Deserialize, Serialize};

/// Operation names a [`PipelineStep`] may request, as plain strings so
/// JSON/YAML definitions stay readable. Dispatch is validated against
/// the target engine's real capabilities at run time — an unknown
/// `(engine, operation)` pair fails before any work happens.
///
/// Supported pairs today:
///
/// | engine | operation | params | effect |
/// |---|---|---|---|
/// | `security` | `validate` | `max_bytes` (opt) | gate: PDF structure/encryption check, bytes unchanged |
/// | `security` | `validate_upload` | `filename` (req), `expected_format` (opt) | filename + size + magic-byte gate, bytes unchanged |
/// | `pdf` | `validate` | — | structural report, bytes unchanged |
/// | `pdf` | `inspect` | — | metadata report, bytes unchanged |
/// | `pdf` | `page_count` | — | page count report, bytes unchanged |
/// | `pdf` | `extract_text` | `pages` (opt range spec) | per-page text report, bytes unchanged |
/// | `pdf` | `extract_metadata` | — | extended metadata report, bytes unchanged |
/// | `pdf` | `extract_images` | `pages` (opt) | embedded-image report, bytes unchanged |
/// | `pdf` | `extract_pages` | `pages` (req range spec) | bytes ← selected pages |
/// | `pdf` | `remove_pages` | `pages` (req range spec) | bytes ← document minus pages |
/// | `pdf` | `reorder` | `order` (req permutation) | bytes ← reordered pages |
/// | `pdf` | `rotate` | `pages` (req), `degrees` (req: 90/180/270) | bytes ← rotated pages |
/// | `pdf` | `split` | `pages_per_file` (req) | terminal multi-output step |
/// | `pdf` | `split_mode` | `mode` (req: `count`/`range`/`every_nth`), `pages_per_file`/`spec`/`stride` | terminal multi-output step |
/// | `pdf` | `split_at_outline` | `level` (req, 0-based) | terminal multi-output step |
/// | `pdf` | `merge` | `other_inputs` (opt array of base64 PDFs), `mode` (opt: `sequential`/`interleave`/`reverse`) | bytes ← merged document |
/// | `pdf` | `interleave` | `other_inputs` (req ≥1 base64 PDFs) | bytes ← round-robin merge |
/// | `pdf` | `overlay` | `other_input` (req base64 PDF), `pages` (opt) | bytes ← overlay applied |
/// | `pdf` | `compare` | `other_input` (req base64 PDF), `mode` (opt: `text`/`structure`/`bytes`) | diff report, bytes unchanged |
/// | `pdf` | `create_portfolio` | `other_inputs` (opt base64 PDFs), `names` (opt) | bytes ← portfolio PDF |
/// | `pdf` | `watermark` | `text` (req), `font_size`, `opacity`, `rotation_degrees`, `position`, `pages`, `color` | bytes ← watermarked |
/// | `pdf` | `stamp` | `text` or `image_png` (b64, one req), `position`, `scale`, `opacity`, `margin`, `pages` | bytes ← stamped |
/// | `pdf` | `draw_text` | `text` (req), `x`, `y`, `font_size`, `color`, `opacity`, `rotation`, `pages` | bytes ← text drawn |
/// | `pdf` | `draw_shapes` | `shapes` (req array), `pages` | bytes ← shapes drawn |
/// | `pdf` | `text_markup` | `kind` (req: `highlight`/`underline`/`strike`), `pages`, `color` | bytes ← markup applied |
/// | `pdf` | `edit_text` | `find` (req), `replace`, `pages`, `case_sensitive` | bytes ← text edited + report |
/// | `pdf` | `headers_footers` | `header`, `footer`, `page_numbers`, `font_size`, `margin`, `pages` | bytes ← headers/footers added |
/// | `pdf` | `add_page_numbers` | `style` (req page-number style) | bytes ← page numbers added |
/// | `pdf` | `nup` | `cols` (req), `rows` (req), `pages` | bytes ← n-up imposition |
/// | `pdf` | `booklet` | `sheet_size`, `imposition` | bytes ← booklet imposition |
/// | `pdf` | `single_page` | `pages`, `cols` | bytes ← all pages on one sheet |
/// | `pdf` | `poster` | `tile_cols` (req), `tile_rows` (req), `pages` | bytes ← tiled poster pages |
/// | `pdf` | `insert_blank` | `count` (req), `position`/`page`, `size` | bytes ← blanks inserted + report |
/// | `pdf` | `duplicate_pages` | `pages` (req), `copies` (req), `position`, `page` | bytes ← pages duplicated + report |
/// | `pdf` | `crop` | `pages` (req), `x0`/`y0`/`x1`/`y1` or `box` | bytes ← crop box set |
/// | `pdf` | `resize` | `pages` (req), `scale` or `width_pt`+`height_pt` | bytes ← page boxes resized |
/// | `pdf` | `scale_content` | `pages` (req), `scale_x` (req), `scale_y` | bytes ← content scaled |
/// | `pdf` | `set_view_crop` | `pages` (req), `box` or `x0`/`y0`/`x1`/`y1` | bytes ← view crop + clip |
/// | `pdf` | `clear_crop` | `pages` (req) | bytes ← crop cleared |
/// | `pdf` | `set_page_rotation` | `pages` (req), `degrees` (req multiple of 90) | bytes ← `/Rotate` set |
/// | `pdf` | `normalize_rotations` | — | bytes ← redundant `/Rotate` removed |
/// | `pdf` | `set_page_labels` | `labels` (req `[{page,label}]`) | bytes ← labels set |
/// | `pdf` | `clear_page_labels` | — | bytes ← labels removed |
/// | `pdf` | `get_page_labels` | — | labels report, bytes unchanged |
/// | `pdf` | `get_page_rotations` | — | rotation report, bytes unchanged |
/// | `pdf` | `grayscale` | `pages` (opt) | bytes ← grayscale + report |
/// | `pdf` | `invert_colors` | `pages` (opt) | bytes ← inverted + report |
/// | `pdf` | `compress` | — | bytes ← recompressed |
/// | `pdf` | `repair` | — | bytes ← repaired + notes report |
/// | `pdf` | `linearize` | — | bytes ← re-saved + report |
/// | `pdf` | `garbage_collect` | — | bytes ← GC'd + report |
/// | `pdf` | `flatten` | — | bytes ← annotations flattened |
/// | `pdf` | `sanitize` | — | bytes ← active content stripped |
/// | `pdf` | `remove_blank_pages` | — | bytes ← blank pages removed + report |
/// | `pdf` | `remove_images` | `pages` (opt) | bytes ← images removed + report |
/// | `pdf` | `remove_embedded_files` | — | bytes ← embedded files removed |
/// | `pdf` | `remove_javascript_names` | — | bytes ← JS name tree removed |
/// | `pdf` | `remove_signatures` | — | bytes ← signatures stripped |
/// | `pdf` | `remove_annotations` | `all`, `pages`, `subtypes` | bytes ← annotations removed |
/// | `pdf` | `list_annotations` | — | annotation report, bytes unchanged |
/// | `pdf` | `add_comment` | `text` (req), `pages`, `rect`, `author`, `icon` | bytes ← comment added |
/// | `pdf` | `add_freetext` | `text` (req), `pages`, `rect`, `author`, `font_size`, `color`, `da` | bytes ← freetext added |
/// | `pdf` | `add_ink` | `strokes` (req), `pages`, `color`, `width` | bytes ← ink added |
/// | `pdf` | `list_ink` | — | ink report, bytes unchanged |
/// | `pdf` | `remove_ink` | `all`, `pages`, `subtypes` | bytes ← ink removed |
/// | `pdf` | `list_articles` | — | article-thread report, bytes unchanged |
/// | `pdf` | `remove_articles` | — | bytes ← article threads removed |
/// | `pdf` | `set_metadata` | `title`, `author`, `subject`, `keywords`, `creator`, `producer`, `creation_date`, `mod_date` | bytes ← `/Info` updated |
/// | `pdf` | `clear_metadata` | — | bytes ← `/Info` cleared |
/// | `pdf` | `set_xmp` | `xmp` (req base64 packet) | bytes ← XMP packet written |
/// | `pdf` | `get_xmp` | — | XMP packet report, bytes unchanged |
/// | `pdf` | `encrypt` | `user_password` (req), `owner_password`, `allow_*` flags | bytes ← encrypted |
/// | `pdf` | `decrypt` | `password` (req) | bytes ← decrypted |
/// | `pdf` | `redact` | `terms` (req string array), `pages` | bytes ← terms removed |
/// | `pdf` | `verify_signatures` | — | signature report, bytes unchanged |
/// | `pdf` | `verify_signatures_crypto` | `trust_anchors` (opt base64 PEM) | crypto signature report, bytes unchanged |
/// | `pdf` | `sign` | `credentials` (req), `reason`, `location`, `contact_info`, `signer_name` | bytes ← signed (needs `openssl`) |
/// | `pdf` | `list_form_fields` | — | form-field report, bytes unchanged |
/// | `pdf` | `fill_form` | `values` (req object name→value) | bytes ← form filled |
/// | `pdf` | `clear_form` | — | bytes ← form cleared |
/// | `pdf` | `create_text_field` | `name`, `page`, `rect`, `value`, `read_only` | bytes ← field added |
/// | `pdf` | `delete_form_field` | `name` (req) | bytes ← field removed |
/// | `pdf` | `unlock_fields` | `names` (opt string array) | bytes ← readonly cleared + report |
/// | `pdf` | `lock_fields` | `names` (opt string array) | bytes ← readonly set + report |
/// | `pdf` | `set_outline` | `items` (req outline spec array) | bytes ← outline replaced |
/// | `pdf` | `remove_outline` | — | bytes ← outline removed |
/// | `pdf` | `list_outline` | — | outline report, bytes unchanged |
/// | `pdf` | `pdfa` | `level` (req: `1b`/`2b`/`3b`) | bytes ← PDF/A declared + report |
/// | `pdf` | `pdfua` | `level` (req: `1`/`ua1`) | bytes ← PDF/UA declared + report |
/// | `pdf` | `pdfx` | `level` (req: `1a`/`3`) | bytes ← PDF/X declared + report |
/// | `pdf` | `convert` | `target` (req: `text`/`markdown`), `pages` | bytes ← text/markdown |
/// | `pdf` | `render` | `pages`, `dpi`, `format` (`png`/`jpeg`) | bytes ← page image (needs `pdftoppm`) |
/// | `pdf` | `ocr` | `pages` | per-page OCR text report (needs `pdftoppm`+`tesseract`) |
/// | `pdf` | `ocr_languages` | — | tesseract language report, bytes unchanged |
/// | `pdf` | `document_digest` | `algorithm` (opt: `sha256`/`sha512`) | digest report, bytes unchanged |
///
/// `merge`, `interleave`, `overlay`, `compare` and `create_portfolio`
/// are multi-input operations: a pipeline is a single stream, so the
/// *other* documents are supplied inline as base64 strings in `params`
/// (`other_inputs`: array of base64-encoded PDFs appended after the
/// stream input; `other_input`: a single base64-encoded PDF). This
/// keeps definitions self-contained — `PipelineEngine::run_pipeline`
/// has no access to the document/artifact store, so `other_artifact_id`
///-style references cannot be resolved here (a job-handler layer that
/// *can* resolve artifacts may inject decoded bytes into these params
/// before running the pipeline).
///
/// Not exposed as pipeline operations: `searchable_pdf` (its engine op
/// returns a report only — the modified bytes never leave the function,
/// so there is nothing to chain), `timestamp_document` /
/// `timestamp_query` / `build_timestamp_request` (require an external
/// RFC 3161 TSA), and `remove_named_objects` (report-only variant of
/// `remove_embedded_files`/`remove_javascript_names`).
pub mod operation {
    /// `security.validate` — deep PDF gate (`validate_pdf`).
    pub const VALIDATE: &str = "validate";
    /// `security.validate_upload` — filename/size/sniff gate.
    pub const VALIDATE_UPLOAD: &str = "validate_upload";
    /// `pdf.validate` — structural `ValidationReport`.
    pub const PDF_VALIDATE: &str = "validate";
    /// `pdf.inspect` — `PdfInfo` metadata report.
    pub const PDF_INSPECT: &str = "inspect";
    /// `pdf.extract_text` — per-page `PageText` report.
    pub const PDF_EXTRACT_TEXT: &str = "extract_text";
    /// `pdf.extract_pages` — byte transform to a page subset.
    pub const PDF_EXTRACT_PAGES: &str = "extract_pages";
    /// `pdf.rotate` — byte transform rotating selected pages.
    pub const PDF_ROTATE: &str = "rotate";
    /// `pdf.split` — terminal step producing multiple PDFs.
    pub const PDF_SPLIT: &str = "split";
    /// `pdf.page_count` — page-count report.
    pub const PDF_PAGE_COUNT: &str = "page_count";
    /// `pdf.extract_metadata` — extended metadata report.
    pub const PDF_EXTRACT_METADATA: &str = "extract_metadata";
    /// `pdf.extract_images` — embedded-image report.
    pub const PDF_EXTRACT_IMAGES: &str = "extract_images";
    /// `pdf.remove_pages` — byte transform dropping selected pages.
    pub const PDF_REMOVE_PAGES: &str = "remove_pages";
    /// `pdf.reorder` — byte transform permuting all pages.
    pub const PDF_REORDER: &str = "reorder";
    /// `pdf.split_mode` — terminal multi-output step with a mode spec.
    pub const PDF_SPLIT_MODE: &str = "split_mode";
    /// `pdf.split_at_outline` — terminal step splitting at bookmarks.
    pub const PDF_SPLIT_AT_OUTLINE: &str = "split_at_outline";
    /// `pdf.merge` — byte transform merging stream + `other_inputs`.
    pub const PDF_MERGE: &str = "merge";
    /// `pdf.interleave` — byte transform round-robin merging inputs.
    pub const PDF_INTERLEAVE: &str = "interleave";
    /// `pdf.overlay` — byte transform painting `other_input` pages on.
    pub const PDF_OVERLAY: &str = "overlay";
    /// `pdf.compare` — two-document diff report.
    pub const PDF_COMPARE: &str = "compare";
    /// `pdf.create_portfolio` — byte transform building a portfolio.
    pub const PDF_CREATE_PORTFOLIO: &str = "create_portfolio";
    /// `pdf.watermark` — byte transform adding a text watermark.
    pub const PDF_WATERMARK: &str = "watermark";
    /// `pdf.stamp` — byte transform adding a text/image stamp.
    pub const PDF_STAMP: &str = "stamp";
    /// `pdf.draw_text` — byte transform drawing text at coordinates.
    pub const PDF_DRAW_TEXT: &str = "draw_text";
    /// `pdf.draw_shapes` — byte transform drawing vector shapes.
    pub const PDF_DRAW_SHAPES: &str = "draw_shapes";
    /// `pdf.text_markup` — byte transform highlighting/striking text.
    pub const PDF_TEXT_MARKUP: &str = "text_markup";
    /// `pdf.edit_text` — byte transform replacing text in content.
    pub const PDF_EDIT_TEXT: &str = "edit_text";
    /// `pdf.headers_footers` — byte transform adding header/footer text.
    pub const PDF_HEADERS_FOOTERS: &str = "headers_footers";
    /// `pdf.add_page_numbers` — byte transform adding page numbers.
    pub const PDF_ADD_PAGE_NUMBERS: &str = "add_page_numbers";
    /// `pdf.nup` — byte transform for n-up imposition.
    pub const PDF_NUP: &str = "nup";
    /// `pdf.booklet` — byte transform for booklet imposition.
    pub const PDF_BOOKLET: &str = "booklet";
    /// `pdf.single_page` — byte transform tiling pages onto one sheet.
    pub const PDF_SINGLE_PAGE: &str = "single_page";
    /// `pdf.poster` — byte transform splitting pages into tiles.
    pub const PDF_POSTER: &str = "poster";
    /// `pdf.insert_blank` — byte transform inserting blank pages.
    pub const PDF_INSERT_BLANK: &str = "insert_blank";
    /// `pdf.duplicate_pages` — byte transform duplicating pages.
    pub const PDF_DUPLICATE_PAGES: &str = "duplicate_pages";
    /// `pdf.crop` — byte transform setting `/CropBox`.
    pub const PDF_CROP: &str = "crop";
    /// `pdf.resize` — byte transform resizing page boxes.
    pub const PDF_RESIZE: &str = "resize";
    /// `pdf.scale_content` — byte transform scaling content streams.
    pub const PDF_SCALE_CONTENT: &str = "scale_content";
    /// `pdf.set_view_crop` — byte transform setting view crop + clip.
    pub const PDF_SET_VIEW_CROP: &str = "set_view_crop";
    /// `pdf.clear_crop` — byte transform clearing view crop.
    pub const PDF_CLEAR_CROP: &str = "clear_crop";
    /// `pdf.set_page_rotation` — byte transform setting `/Rotate`.
    pub const PDF_SET_PAGE_ROTATION: &str = "set_page_rotation";
    /// `pdf.normalize_rotations` — byte transform removing `/Rotate` 0.
    pub const PDF_NORMALIZE_ROTATIONS: &str = "normalize_rotations";
    /// `pdf.get_page_rotations` — per-page rotation report.
    pub const PDF_GET_PAGE_ROTATIONS: &str = "get_page_rotations";
    /// `pdf.set_page_labels` — byte transform setting page labels.
    pub const PDF_SET_PAGE_LABELS: &str = "set_page_labels";
    /// `pdf.clear_page_labels` — byte transform removing page labels.
    pub const PDF_CLEAR_PAGE_LABELS: &str = "clear_page_labels";
    /// `pdf.get_page_labels` — page-label report.
    pub const PDF_GET_PAGE_LABELS: &str = "get_page_labels";
    /// `pdf.grayscale` — byte transform converting to grayscale.
    pub const PDF_GRAYSCALE: &str = "grayscale";
    /// `pdf.invert_colors` — byte transform inverting colors.
    pub const PDF_INVERT_COLORS: &str = "invert_colors";
    /// `pdf.compress` — byte transform recompressing the document.
    pub const PDF_COMPRESS: &str = "compress";
    /// `pdf.repair` — byte transform normalizing a damaged document.
    pub const PDF_REPAIR: &str = "repair";
    /// `pdf.linearize` — byte transform re-saving for fast web view.
    pub const PDF_LINEARIZE: &str = "linearize";
    /// `pdf.garbage_collect` — byte transform pruning unreachable objects.
    pub const PDF_GARBAGE_COLLECT: &str = "garbage_collect";
    /// `pdf.flatten` — byte transform flattening annotation appearances.
    pub const PDF_FLATTEN: &str = "flatten";
    /// `pdf.sanitize` — byte transform stripping active content.
    pub const PDF_SANITIZE: &str = "sanitize";
    /// `pdf.remove_blank_pages` — byte transform dropping blank pages.
    pub const PDF_REMOVE_BLANK_PAGES: &str = "remove_blank_pages";
    /// `pdf.remove_images` — byte transform stripping image XObjects.
    pub const PDF_REMOVE_IMAGES: &str = "remove_images";
    /// `pdf.remove_embedded_files` — byte transform dropping embedded files.
    pub const PDF_REMOVE_EMBEDDED_FILES: &str = "remove_embedded_files";
    /// `pdf.remove_javascript_names` — byte transform dropping the JS name tree.
    pub const PDF_REMOVE_JAVASCRIPT_NAMES: &str = "remove_javascript_names";
    /// `pdf.remove_signatures` — byte transform stripping signatures.
    pub const PDF_REMOVE_SIGNATURES: &str = "remove_signatures";
    /// `pdf.remove_annotations` — byte transform removing annotations.
    pub const PDF_REMOVE_ANNOTATIONS: &str = "remove_annotations";
    /// `pdf.list_annotations` — annotation report.
    pub const PDF_LIST_ANNOTATIONS: &str = "list_annotations";
    /// `pdf.add_comment` — byte transform adding a comment annotation.
    pub const PDF_ADD_COMMENT: &str = "add_comment";
    /// `pdf.add_freetext` — byte transform adding a FreeText annotation.
    pub const PDF_ADD_FREETEXT: &str = "add_freetext";
    /// `pdf.add_ink` — byte transform adding ink annotations.
    pub const PDF_ADD_INK: &str = "add_ink";
    /// `pdf.list_ink` — ink-annotation report.
    pub const PDF_LIST_INK: &str = "list_ink";
    /// `pdf.remove_ink` — byte transform removing ink annotations.
    pub const PDF_REMOVE_INK: &str = "remove_ink";
    /// `pdf.list_articles` — article-thread report.
    pub const PDF_LIST_ARTICLES: &str = "list_articles";
    /// `pdf.remove_articles` — byte transform removing article threads.
    pub const PDF_REMOVE_ARTICLES: &str = "remove_articles";
    /// `pdf.set_metadata` — byte transform updating `/Info`.
    pub const PDF_SET_METADATA: &str = "set_metadata";
    /// `pdf.clear_metadata` — byte transform clearing `/Info`.
    pub const PDF_CLEAR_METADATA: &str = "clear_metadata";
    /// `pdf.set_xmp` — byte transform writing an XMP packet.
    pub const PDF_SET_XMP: &str = "set_xmp";
    /// `pdf.get_xmp` — XMP packet report.
    pub const PDF_GET_XMP: &str = "get_xmp";
    /// `pdf.encrypt` — byte transform encrypting the document.
    pub const PDF_ENCRYPT: &str = "encrypt";
    /// `pdf.decrypt` — byte transform decrypting the document.
    pub const PDF_DECRYPT: &str = "decrypt";
    /// `pdf.redact` — byte transform removing text matches.
    pub const PDF_REDACT: &str = "redact";
    /// `pdf.verify_signatures` — structural signature report.
    pub const PDF_VERIFY_SIGNATURES: &str = "verify_signatures";
    /// `pdf.verify_signatures_crypto` — CMS signature verification report.
    pub const PDF_VERIFY_SIGNATURES_CRYPTO: &str = "verify_signatures_crypto";
    /// `pdf.sign` — byte transform adding a CMS signature.
    pub const PDF_SIGN: &str = "sign";
    /// `pdf.list_form_fields` — AcroForm field report.
    pub const PDF_LIST_FORM_FIELDS: &str = "list_form_fields";
    /// `pdf.fill_form` — byte transform filling form fields.
    pub const PDF_FILL_FORM: &str = "fill_form";
    /// `pdf.clear_form` — byte transform clearing form values.
    pub const PDF_CLEAR_FORM: &str = "clear_form";
    /// `pdf.create_text_field` — byte transform adding a text field.
    pub const PDF_CREATE_TEXT_FIELD: &str = "create_text_field";
    /// `pdf.delete_form_field` — byte transform removing a form field.
    pub const PDF_DELETE_FORM_FIELD: &str = "delete_form_field";
    /// `pdf.unlock_fields` — byte transform clearing read-only flags.
    pub const PDF_UNLOCK_FIELDS: &str = "unlock_fields";
    /// `pdf.lock_fields` — byte transform setting read-only flags.
    pub const PDF_LOCK_FIELDS: &str = "lock_fields";
    /// `pdf.set_outline` — byte transform replacing the outline.
    pub const PDF_SET_OUTLINE: &str = "set_outline";
    /// `pdf.remove_outline` — byte transform removing the outline.
    pub const PDF_REMOVE_OUTLINE: &str = "remove_outline";
    /// `pdf.list_outline` — outline report.
    pub const PDF_LIST_OUTLINE: &str = "list_outline";
    /// `pdf.list_portfolio` — collection-dictionary entry report.
    pub const PDF_LIST_PORTFOLIO: &str = "list_portfolio";
    /// `pdf.pdfa` — byte transform declaring PDF/A + report.
    pub const PDF_PDFA: &str = "pdfa";
    /// `pdf.pdfua` — byte transform declaring PDF/UA + report.
    pub const PDF_PDFUA: &str = "pdfua";
    /// `pdf.pdfx` — byte transform declaring PDF/X + report.
    pub const PDF_PDFX: &str = "pdfx";
    /// `pdf.convert` — byte transform to plain text or Markdown.
    pub const PDF_CONVERT: &str = "convert";
    /// `pdf.render` — byte transform rasterizing a page to an image.
    pub const PDF_RENDER: &str = "render";
    /// `pdf.ocr` — per-page OCR text report.
    pub const PDF_OCR: &str = "ocr";
    /// `pdf.ocr_languages` — installed tesseract languages report.
    pub const PDF_OCR_LANGUAGES: &str = "ocr_languages";
    /// `pdf.document_digest` — hash digest report.
    pub const PDF_DOCUMENT_DIGEST: &str = "document_digest";
}

/// Engine names recognized by the dispatcher.
pub mod engine_name {
    /// The PDF engine (`PdfEngine`).
    pub const PDF: &str = "pdf";
    /// The security engine (`SecurityEngine`).
    pub const SECURITY: &str = "security";
}

/// One step in a [`PipelineDefinition`]: target engine, operation, and
/// a JSON object of operation-specific parameters.
///
/// `params` is deliberately untyped (`serde_json::Value`) — each
/// operation interprets only the keys it knows and ignores the rest,
/// so definitions stay forward-compatible. Unknown *operations* are
/// rejected; unknown *params* are not (documented per operation in
/// [`operation`]).
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct PipelineStep {
    /// Human-readable step name used in errors and reports. Must be
    /// unique within a definition for readable output (not enforced).
    pub name: String,
    /// Target engine: `"pdf"` or `"security"` (see [`engine_name`]).
    pub engine: String,
    /// Operation name (see [`operation`]).
    pub operation: String,
    /// Operation parameters as a JSON object. Missing keys use the
    /// operation's defaults; a non-object value is a definition error.
    #[serde(default)]
    pub params: serde_json::Value,
}

impl PipelineStep {
    /// Convenience constructor for a parameterless step.
    pub fn new(
        name: impl Into<String>,
        engine: impl Into<String>,
        operation: impl Into<String>,
    ) -> Self {
        Self {
            name: name.into(),
            engine: engine.into(),
            operation: operation.into(),
            params: serde_json::Value::Object(serde_json::Map::new()),
        }
    }

    /// Sets `params` (typically `serde_json::json!({...})`).
    pub fn with_params(mut self, params: serde_json::Value) -> Self {
        self.params = params;
        self
    }
}

/// A named, ordered sequence of [`PipelineStep`]s.
///
/// Definitions are pure data — construct them in code, deserialize
/// them from JSON/YAML config, or use
/// [`crate::crates::pipeline_engine::builtin_definitions`]. Validation
/// happens at run time ([`PipelineEngine::run_pipeline`](crate::crates::pipeline_engine::PipelineEngine::run_pipeline))
/// so a bad definition is reported with step context, not at parse
/// time.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct PipelineDefinition {
    /// Stable identifier, e.g. `"pdf.intake"`. Used by
    /// [`PipelineEngine::run_named`](crate::crates::pipeline_engine::PipelineEngine::run_named)
    /// for built-ins and as the `pipeline` field of [`PipelineOutput`].
    pub name: String,
    /// Optional human description shown in listings.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub description: Option<String>,
    /// Ordered steps. Must be non-empty and within
    /// [`PipelineLimits::max_steps`](crate::crates::pipeline_engine::PipelineLimits).
    pub steps: Vec<PipelineStep>,
}

/// The data a pipeline carries between steps and returns to the caller.
///
/// Most steps transform [`PipelineArtifact::Bytes`]; `pdf.split`,
/// `pdf.split_mode` and `pdf.split_at_outline` are the operations
/// producing [`PipelineArtifact::Multiple`], which must be the
/// terminal step (a later step needing a single byte stream fails
/// fast).
///
/// Serialized manually (rather than derived) so byte payloads encode
/// as base64 strings via [`super::b64_serde`] — serde's `with` helper
/// cannot annotate the `Vec<u8>` inside `Vec<Vec<u8>>`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum PipelineArtifact {
    /// A single byte stream — the common case.
    Bytes(Vec<u8>),
    /// Multiple byte streams (e.g. `pdf.split` output).
    Multiple(Vec<Vec<u8>>),
}

impl PipelineArtifact {
    /// Total bytes held by this artifact, for cumulative limit checks.
    pub fn len_bytes(&self) -> u64 {
        match self {
            Self::Bytes(b) => b.len() as u64,
            Self::Multiple(v) => v.iter().map(|b| b.len() as u64).sum(),
        }
    }

    /// Borrow the single byte stream, or `None` for multi-output
    /// artifacts. Steps that consume bytes use this and fail with a
    /// step-context error when it returns `None`.
    pub fn as_bytes(&self) -> Option<&[u8]> {
        match self {
            Self::Bytes(b) => Some(b),
            Self::Multiple(_) => None,
        }
    }
}

impl Serialize for PipelineArtifact {
    fn serialize<S: serde::Serializer>(&self, serializer: S) -> Result<S::Ok, S::Error> {
        use serde::ser::SerializeMap;
        match self {
            Self::Bytes(b) => {
                let mut map = serializer.serialize_map(Some(2))?;
                map.serialize_entry("kind", "bytes")?;
                map.serialize_entry("data", &super::b64_serde::encode(b))?;
                map.end()
            }
            Self::Multiple(v) => {
                let encoded: Vec<String> = v.iter().map(|b| super::b64_serde::encode(b)).collect();
                let mut map = serializer.serialize_map(Some(2))?;
                map.serialize_entry("kind", "multiple")?;
                map.serialize_entry("data", &encoded)?;
                map.end()
            }
        }
    }
}

impl<'de> Deserialize<'de> for PipelineArtifact {
    fn deserialize<D: serde::Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
        use serde::de::{MapAccess, Visitor};
        use std::fmt;

        struct ArtifactVisitor;

        impl<'de> Visitor<'de> for ArtifactVisitor {
            type Value = PipelineArtifact;

            fn expecting(&self, f: &mut fmt::Formatter) -> fmt::Result {
                f.write_str("a pipeline artifact object with `kind` and `data` fields")
            }

            fn visit_map<A: MapAccess<'de>>(self, mut map: A) -> Result<Self::Value, A::Error> {
                let mut kind: Option<String> = None;
                let mut data: Option<serde_json::Value> = None;
                while let Some(key) = map.next_key::<String>()? {
                    match key.as_str() {
                        "kind" => kind = Some(map.next_value()?),
                        "data" => data = Some(map.next_value()?),
                        _ => {
                            let _: serde::de::IgnoredAny = map.next_value()?;
                        }
                    }
                }
                let kind = kind.ok_or_else(|| serde::de::Error::missing_field("kind"))?;
                let data = data.ok_or_else(|| serde::de::Error::missing_field("data"))?;
                match kind.as_str() {
                    "bytes" => {
                        let s = data.as_str().ok_or_else(|| {
                            serde::de::Error::custom(
                                "bytes artifact `data` must be a base64 string",
                            )
                        })?;
                        super::b64_serde::decode(s)
                            .map(PipelineArtifact::Bytes)
                            .map_err(serde::de::Error::custom)
                    }
                    "multiple" => {
                        let list = data.as_array().ok_or_else(|| {
                            serde::de::Error::custom(
                                "multiple artifact `data` must be an array of base64 strings",
                            )
                        })?;
                        let mut out = Vec::with_capacity(list.len());
                        for item in list {
                            let s = item.as_str().ok_or_else(|| {
                                serde::de::Error::custom(
                                    "multiple artifact entries must be base64 strings",
                                )
                            })?;
                            out.push(
                                super::b64_serde::decode(s).map_err(serde::de::Error::custom)?,
                            );
                        }
                        Ok(PipelineArtifact::Multiple(out))
                    }
                    other => Err(serde::de::Error::custom(format!(
                        "unknown pipeline artifact kind `{other}`"
                    ))),
                }
            }
        }

        deserializer.deserialize_map(ArtifactVisitor)
    }
}

/// Per-step record in [`PipelineOutput::steps`]: which step ran and
/// what it produced (for report steps) or a short summary.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct StepReport {
    /// 0-based position in the definition.
    pub index: usize,
    /// Step name from the definition.
    pub name: String,
    /// Engine that ran the step.
    pub engine: String,
    /// Operation that ran.
    pub operation: String,
    /// Structured output for report steps (`validate`, `inspect`,
    /// `extract_text`, `compare`, ...), serialized to JSON. `None`
    /// for byte transforms and multi-output steps — except transforms
    /// that also report (`repair`, `pdfa`, ...), which carry both.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub output: Option<serde_json::Value>,
}

/// Result of a successful
/// [`PipelineEngine::run_pipeline`](crate::crates::pipeline_engine::PipelineEngine::run_pipeline).
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct PipelineOutput {
    /// Name of the definition that ran.
    pub pipeline: String,
    /// Final artifact — bytes for most pipelines, multiple documents
    /// when the last step was `pdf.split`.
    pub artifact: PipelineArtifact,
    /// One [`StepReport`] per executed step, in order.
    pub steps: Vec<StepReport>,
}
