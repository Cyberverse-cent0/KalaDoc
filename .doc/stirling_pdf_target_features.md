# Stirling-PDF Advanced Features — Target Documentation

Reference inventory of Stirling-PDF's advanced capabilities, captured for
archival by Applaicon and used as the feature-parity target for KalaDoc.

> **Legend.** `[x]` = fully wired: op module on disk **and** `PdfEngine`
> method **and** a `document.pdf_op` dispatch arm in
> `job_handlers.rs::dispatch_pdf_op`. `[ ]` = not fully wired; annotations
> say how far it gets (`backend only` = ops module + `PdfEngine` method
> exist but there is no job dispatch; `ops module only` = the file exists
> but has no `PdfEngine` wrapper). Section 13 uses ✅/⚠️/❌ for the same
> three tiers. This is a feature *reference*, not an implementation
> requirement — reliable document primitives come first, then
> higher-level engines.

## Scope for KalaDoc

- **Frontend features are out of scope for now.** Anything that exists
  only to serve a web/desktop UI — drag-and-drop page rearrangement,
  custom branding, multi-language interface, admin consoles, SSO/OIDC/LDAP
  portals — is deferred. Engine capabilities underneath them remain in
  scope where they make sense as library/API operations.
- **The backend is the absolute target.** All work targets the engine
  crates (`pdf-engine`, `conversion-engine`, `image-engine`,
  `pipeline-engine`, `job_engine`, `security-engine`, `document_engine`)
  and the services above them.
- **The backend API is under construction.** The axum REST API exists
  (`kaladoc serve`, `api.addr` config) with document upload/download,
  CRUD, versions, tags, history, folders, jobs, engines, storage and
  health endpoints; the full Stirling-style surface (per-operation
  process endpoints for every engine op, batch jobs, pipeline
  execution, OpenAPI) is still to be built on top of the engines —
  see section 10. Today `POST /documents/{id}/process` accepts the 25
  ops in `job_handlers::PDF_OPS` as `document.pdf_op` jobs (see
  section 13); every other engine method is reachable only in-process.

## 1. Advanced PDF editing

- [ ] Edit existing PDF text. *(backend only: pdf-engine `edit_text`
  (`ops/edit_text.rs`) rewrites text-showing operators; no dispatch)*
- [ ] Edit or replace images inside PDFs. *(backend only:
  `replace_image` — swaps an image XObject's stream data; no dispatch)*
- [ ] Add text at arbitrary page coordinates. *(backend only:
  `draw_text` places text at x/y in points, bottom-left origin; no
  dispatch — `stamp`/`watermark` remain the dispatched anchored
  alternatives)*
- [x] Add images, logos and watermarks. *(pdf-engine `stamp` — text or
  PNG image — and `watermark`, both dispatched; image-engine
  `watermark` for raster images)*
- [ ] Draw shapes and freehand annotations. *(backend only:
  `draw_shapes` vector shapes + `add_ink` freehand strokes; no
  dispatch)*
- [ ] Highlight, underline and strike through text. *(backend only:
  `text_markup` — highlight/underline/strikeout markup annotations;
  no dispatch)*
- [ ] Add comments and sticky notes. *(backend only: `add_comment`
  (sticky-note icons) and `add_freetext` (freetext callouts); no
  dispatch)*
- [ ] Edit document metadata. *(backend only: `set_metadata`,
  `clear_metadata`, `set_xmp`/`get_xmp`; `extract_metadata` inspection
  is dispatched)*
- [ ] Edit bookmarks and table of contents. *(backend only:
  `set_outline`/`remove_outline`/`list_outline`; no dispatch)*
- [ ] Edit interactive PDF forms. *(backend only: `list_form_fields`,
  `create_text_field`, `delete_form_field`, `edit_field_props` is
  ops-module only; no dispatch)*
- [ ] Fill existing forms. *(backend only: `fill_form`, `clear_form`;
  no dispatch)*
- [ ] Detect and create form fields automatically. *(partial:
  `list_form_fields` enumerates AcroForm fields backend-only;
  `create_text_field` can add text fields; no heuristics, no
  dispatch)*
- [x] Flatten interactive forms. *(dispatched `flatten` — best-effort,
  merges annotation/appearance content into page streams)*
- [ ] Unlock read-only form fields. *(backend only: `unlock_fields` /
  `lock_fields` flag patching; no dispatch)*
- [ ] Remove annotations. *(backend only: `remove_annotations`,
  `remove_articles`, `remove_ink` with `AnnotFilter`; `flatten` merges
  rather than removes)*
- [ ] Remove selected images. *(backend only: `remove_images` per-page
  sweep returning a `RemoveReport`; no dispatch)*
- [x] Compare two PDF documents. *(dispatched `compare` — text,
  structure and byte modes with diff report; needs a second artifact
  via `params.other_artifact_id`/`other_document_id`)*
- [x] Find differences between documents. *(same `compare` op)*

## 2. Advanced page manipulation

- [ ] Merge multiple PDFs. *(backend only: `merge`; note: outlines,
  signatures and document-level structures are not carried over; no
  dispatch)*
- [x] Split PDFs by page ranges. *(dispatched `extract_pages` — 1-based
  range spec `"1-3,8"`)*
- [x] Split PDFs into individual pages. *(dispatched `split` with
  `pages_per_file = 1`)*
- [x] Split by page count. *(dispatched `split`)*
- [ ] Split by bookmarks. *(backend only: `split_at_outline` splits on
  outline levels; also `split_mode` for size/count modes; no
  dispatch)*
- [x] Extract selected pages. *(dispatched `extract_pages`, 1-based
  range spec `"1-3,8"`)*
- [x] Delete selected pages. *(dispatched `remove_pages`)*
- [ ] Remove blank pages automatically. *(backend only:
  `remove_blank_pages` — conservative content-stream heuristic,
  keeps annotated/undecodable pages, fails rather than emit a
  page-less PDF; returns a `BlankReport`; no dispatch)*
- [ ] Detect page orientation and auto-rotate. *(❌ `ops/auto_orient.rs`
  exists on disk but is **not declared in `ops/mod.rs`** — dead file,
  no engine method; geometry-only detection anyway, no text-direction
  CV)*
- [x] Rotate individual pages. *(dispatched `rotate` — range spec +
  90/180/270 via `degrees`; backend-only extras: `set_page_rotation`,
  `get_page_rotations`, `normalize_rotations`)*
- [x] Rearrange pages. *(dispatched `reorder` — full permutation spec;
  programmatic, not drag-and-drop UI)*
- [ ] Duplicate pages. *(backend only: `duplicate_pages` with position
  spec; `reorder` still requires each page exactly once; no dispatch)*
- [ ] Insert blank pages. *(backend only: `insert_blank` at position
  spec with size; no dispatch)*
- [ ] Crop pages manually. *(backend only: `crop` sets `/CropBox`;
  dispatched ops have no crop arm)*
- [ ] Resize pages. *(backend only: `resize` — page boxes only,
  content is not scaled; no dispatch)*
- [ ] Scale page content. *(backend only: `scale_content` — transforms
  page content streams; no dispatch)*
- [ ] Add page numbers. *(backend only: `add_page_numbers` with
  `PageNumberStyle`; no dispatch)*
- [ ] Add headers and footers. *(backend only: `headers_footers` text
  placement; no dispatch)*
- [ ] Multi-page layouts (N-up). *(backend only: `nup`; no dispatch)*
- [ ] Booklet imposition. *(backend only: `booklet` with `BookletSpec`
  size/mode; no dispatch)*
- [ ] Poster printing / tile splitting. *(backend only: `poster`; no
  dispatch)*
- [ ] Convert PDF into a single large page. *(backend only:
  `single_page`; no dispatch)*
- [ ] Overlay multiple PDFs. *(backend only: `overlay` — form XObjects,
  optional page selection; no dispatch)*
- [ ] Interleave pages from multiple documents. *(backend only:
  `interleave` + `merge_mode` (`MergeMode`); no dispatch)*
- [ ] Crop-box vs. view-crop and page-size control. *(backend only:
  `set_view_crop`/`clear_crop` (`ops/crop_view.rs`), page-label ops in
  `ops/page_labels.rs` via `set_page_labels`/`clear_page_labels`/
  `get_page_labels`; no dispatch)*

## 3. Document conversion engine

Supported formats today: `pdf`, `txt`, `markdown`, `html`, `png`, `jpeg`,
`docx`, `odt` — backed by pandoc, LibreOffice, poppler (external) plus a
pure-Rust fallback, dispatched with automatic fallback. The `Format`
enum additionally declares `xlsx`, `xls`, `pptx`, `ppt` and `epub`, but
no backend reads them yet, so those conversions are not advertised.

### To PDF

- [x] DOCX → PDF *(pandoc / LibreOffice)*
- [ ] XLS / XLSX → PDF *(format not in the `Format` enum)*
- [ ] PPT / PPTX → PDF
- [x] ODT → PDF *(LibreOffice / pandoc)*
- [x] HTML → PDF *(pandoc)*
- [x] Markdown → PDF *(pandoc + Rust backend)*
- [x] TXT → PDF *(pandoc + Rust backend)*
- [x] Images → PDF *(partial: pure-Rust backend converts PNG/JPEG → PDF;
  multi-page/other formats not covered)*
- [ ] EPUB → PDF *(enum value exists; no backend reads EPUB yet)*
- [ ] Email (EML) → PDF
- [ ] URL → PDF
- [ ] HTML files → PDF *(HTML string/bytes covered; file/URL fetching no)*
- [ ] CBZ / CBR → PDF

### From PDF

- [ ] PDF → Word
- [ ] PDF → Excel
- [ ] PDF → PowerPoint
- [ ] PDF → CSV
- [ ] PDF → HTML
- [x] PDF → Markdown *(dispatched pdf-engine `convert` with
  `target=markdown` — best-effort text-layer conversion)*
- [ ] PDF → XML
- [x] PDF → JSON-oriented extraction workflows. *(partial:
  dispatched `extract_metadata`/`inspect` return structured metadata;
  no general JSON export)*
- [x] PDF → Images. *(dispatched `render` — shells out to `pdftoppm`
  (poppler-utils, external tool); PNG or JPEG, page-range selectable;
  `MissingTool` error when poppler is absent)*
- [x] PDF → Plain text. *(dispatched `convert`/`extract_text`)*
- [ ] PDF → EPUB
- [ ] PDF → PDF/A. *(backend only: `to_pdfa` writes a `pdfaid:` XMP
  declaration — deliberately **no** ICC/OutputIntent, fonts or
  conformance claim; `PdfaReport::warnings` lists validator gaps; no
  dispatch)*
- [ ] PDF → PDF/UA. *(backend only: `to_pdfua`, same honest-declaration
  approach; no dispatch)*
- [ ] PDF → PDF/X. *(backend only: `to_pdfx`; no dispatch)*
- [ ] PDF → RTF
- [ ] PDF → CBZ / CBR
- [x] PDF → Single-page images. *(same dispatched `render`; any page
  range, not just page 1)*

> Actual format availability depends on the installed conversion engines.
> Docx↔Odt conversion is supported via LibreOffice.

## 4. OCR and intelligent document processing

- [x] OCR scanned documents. *(dispatched `ocr` returns per-page text
  via `pdftoppm` + `tesseract` — **two external tools**; does **not**
  produce a searchable PDF by itself)*
- [ ] Make scanned PDFs searchable. *(backend only: `make_searchable`
  injects an invisible OCR text layer — rasterizes via `pdftoppm`,
  tesseract TSV word boxes → positioned `3 Tr` text; **honest degraded
  mode**: pages where TSV parsing fails fall back to one unpositioned
  text block, reported via `SearchableReport::positioned=false` +
  warnings; no dispatch)*
- [x] Extract text from images. *(partial: tesseract path via PDF raster;
  image-engine has no direct OCR op)*
- [x] Support multiple OCR languages. *(`ocr_languages` lists installed
  tesseract codes — backend only, not dispatched; per-request language
  selection not exposed)*
- [ ] Automatically rotate scanned pages. *(❌ `auto_orient` is a dead
  file — see section 2)*
- [ ] Deskew scanned documents. *(❌ no deskew op exists anywhere in
  the workspace)*
- [ ] Remove scanning noise. *(❌ no denoise/despeckle op; image-engine
  `effects` has `photocopy`/`levels`/`replace_color` but those
  simulate or correct tone, they do not clean scans)*
- [ ] Clean scanned backgrounds. *(❌ same — no background-removal op)*
- [ ] Detect text orientation. *(❌ `auto_orient` dead file; even it is
  geometry-only, no text-direction detection)*
- [x] Extract images from PDFs. *(dispatched `extract_images`)*
- [ ] Extract photographs from scanned pages.
- [ ] Detect and split multiple photographs.
- [ ] Automatically rename PDFs based on document content.
- [x] Detect document structure. *(dispatched `inspect`,
  `extract_metadata`, `validate`; `ops/structure_tree.rs` exists but
  has **no engine method** — PDF/UA structure inspection is
  ops-module only)*
- [x] Extract document information. *(dispatched `extract_metadata` —
  Info dict, dates, keywords, XMP)*
- [x] Automatic form detection. *(partial: backend-only
  `list_form_fields`/`create_text_field` cover AcroForm enumeration and
  text-field creation; `extract_metadata` reports field names/paths;
  no detection heuristics, no dispatch)*
- [x] Document classification and processing workflows. *(partial:
  linear pipeline definitions + ingestion/metadata/index jobs)*

## 5. Advanced security and digital signatures

- [x] Password-protect PDFs. *(dispatched `encrypt` — AES-256,
  user/owner passwords)*
- [x] Remove PDF passwords. *(dispatched `decrypt`)*
- [x] Change PDF permissions. *(dispatched `encrypt` `PdfPermissions`
  flags — also accepted nested under `params.permissions`)*
- [x] Restrict printing. *(same `allow_print` flag)*
- [x] Restrict copying. *(same `allow_copy` flag)*
- [x] Restrict editing. *(same `allow_modify`/`allow_annotations`
  flags)*
- [ ] Apply digital certificate signatures. *(backend only: `sign`
  produces a real detached CMS/PKCS#7 signature via the `openssl` CLI
  (`adbe.pkcs7.detached` + `/ByteRange`); **limitations**: whole-
  document signatures only, full rewrite invalidates prior signatures
  (input with existing signatures is refused — call
  `remove_signatures` first), no incremental updates, requires
  `openssl` on `PATH`; no dispatch)*
- [ ] Sign using PEM certificates. *(backend only: `SignCredentials`
  accepts PEM cert + key paths; no dispatch)*
- [ ] Sign using PKCS#12/P12 certificates. *(backend only:
  `SignCredentials::Pkcs12` — `openssl pkcs12 -nodes` extracts to PEM
  in a tempdir; no dispatch)*
- [x] Validate digital signatures. *(dispatched `verify_signatures` —
  **structural** report: presence and byte-range sanity, not
  cryptographic verification; backend-only `verify_signatures_crypto`
  does real `openssl cms -verify` digest + chain checking when trust
  anchors are supplied — timestamps detected but never verified,
  revocation never checked)*
- [ ] Verify certificate chains. *(partial backend only:
  `verify_signatures_crypto` chain-verifies against caller-supplied
  PEM trust anchors; no dispatch)*
- [ ] Remove certificate signatures. *(backend only:
  `remove_signatures` strips signature fields/widgets/`/Perms`; no
  dispatch)*
- [ ] Add timestamp signatures. *(backend only:
  `timestamp_document` performs RFC 3161 digest→TSQ→TSA→token against a
  caller-supplied `https://` TSA URL — **no offline fallback, no
  token is ever fabricated**; embedding a token into a signature
  dictionary is not implemented; `sign` rejects
  `timestamp_authority` as `UnsupportedOperation`; no dispatch)*
- [ ] RFC 3161 timestamping. *(same backend-only `timestamp_*` family:
  `document_digest`, `timestamp_query`, `build_timestamp_request`,
  `request_external_timestamp`, `list_signature_timestamps`,
  `has_timestamps`; DER TSQ/TSR path needs no external tools, the TSA
  call is the only network dependency)*
- [ ] Permanently redact sensitive content.
- [x] Redact selected text. *(dispatched `redact` — removes matching
  text strings from content streams; **semantics**: best-effort text-
  terms only, `params.terms` required; underlying bytes are not proven
  gone, no region/page redaction)*
- [ ] Redact selected regions.
- [ ] Redact entire pages.
- [x] Sanitize PDF documents. *(dispatched `sanitize`)*
- [x] Remove embedded JavaScript. *(dispatched `sanitize`; backend-only
  `remove_javascript_names` is a narrower variant)*
- [x] Remove potentially dangerous embedded files. *(dispatched
  `sanitize`; backend-only `remove_embedded_files`)*
- [x] Flatten forms and annotations. *(dispatched `flatten`)*
- [x] Validate PDF compliance. *(dispatched `validate` →
  `ValidationReport`; backend-only `to_pdfa`/`to_pdfua`/`to_pdfx`
  emit honest compliance *declarations*, not validated files)*

> **Important:** True redaction must remove underlying content. Current
> `redact` is best-effort text removal — region/page-level permanent
> redaction is still open.

## 6. Compression and optimization

- [x] Compress PDF files. *(dispatched `compress` recompresses object/
  xref streams via lopdf; no image-level optimization)*
- [ ] Optimize embedded images. *(backend only: `remove_images` +
  `replace_image` can strip/swap image XObjects; no dispatch)*
- [x] Reduce image resolution. *(image-engine `resize`/`thumbnail` cover
  raster inputs, not embedded PDF images)*
- [x] Adjust JPEG quality. *(image-engine `compress` — quality 1–100 for
  JPEG; PNG uses compression effort; WebP/GIF/BMP use codec defaults)*
- [ ] Remove unnecessary objects. *(backend only: `garbage_collect`
  returns a `GcReport`; `remove_named_objects` deletes objects by id;
  no dispatch)*
- [ ] Remove unused resources. *(same backend-only GC/remove-objects
  pair)*
- [ ] Optimize PDF structure. *(backend only: `garbage_collect`;
  no dispatch)*
- [ ] Reduce redundant content. *(backend only at best:
  `garbage_collect`/`compress`; no content-dedup op)*
- [ ] Linearize PDFs for faster web viewing. *(backend only:
  `linearize` — **honest no-op**: `lopdf` 0.47 accepts
  `SaveOptions::linearize` but never writes linearization data, so the
  op re-parses its own output and `LinearizeReport::linearized` is
  always `false` today; it reports that plainly rather than claiming a
  Fast Web View file; no dispatch)*
- [x] Convert images to more efficient formats. *(image-engine
  `convert`/`compress` cover png/jpeg/gif/webp/bmp for standalone
  images)*
- [ ] Optimize documents for printing. *(backend only:
  `add_print_marks` crop/registration marks; no dispatch)*
- [ ] Optimize documents for archival storage. *(backend only:
  `to_pdfa` declaration subset; no dispatch)*

## 7. Advanced visual processing

*(image-engine ops apply to raster images — PNG/JPEG/GIF/WebP/BMP —
not directly to PDF page content. Implemented engine ops: `inspect`,
`resize`, `resize_to`, `convert`, `rotate` (90/180/270), `crop`,
`flip`/`flip_mirror`, `adjust` (brightness/contrast/hue), `color`
(saturation ±100%, per-channel RGB gain/offset, high-contrast),
`levels` (black/white point + gamma), `photocopy` (scanner
simulation), `replace_color` (near-color swap), `cmyk` (RGB→CMYK TIFF),
`thumbnail`, `compress`, `watermark` (image overlay), `validate`,
`metadata`. pdf-engine additionally has backend-only `grayscale` and
`invert_colors` for page content.)*

- [x] Adjust brightness. *(image-engine `adjust` — ±100% via
  `imageops::brighten`)*
- [x] Adjust contrast. *(image-engine `adjust`/`color`)*
- [x] Adjust saturation. *(image-engine `color` — `saturation` ±100%
  via HSV rescale)*
- [x] Adjust RGB channels independently. *(image-engine `color` —
  per-channel gain/offset)*
- [ ] Invert document colors. *(backend only: pdf-engine
  `invert_colors` on page content + image-engine equivalent via
  `adjust`; no dispatch)*
- [ ] Replace foreground and background colors. *(backend only:
  image-engine `replace_color` near-color matching; no dispatch)*
- [ ] Create high-contrast documents. *(backend only: image-engine
  `color` high-contrast + `levels` black/white point; no dispatch)*
- [ ] Convert colors to CMYK. *(backend only: image-engine `cmyk` —
  naive RGB→CMYK written as uncompressed CMYK TIFF; **alpha is
  flattened over white (lossy)**; output contract differs from other
  ops; no dispatch)*
- [ ] Apply scanner effects. *(backend only: image-engine
  `photocopy` — deterministic grain simulation; no dispatch)*
- [ ] Simulate photocopy effects. *(same `photocopy` op)*
- [ ] Apply grayscale. *(backend only: pdf-engine `grayscale` on page
  content + image-engine via `color` saturation=-100; no dispatch)*
- [x] Repair damaged PDFs. *(dispatched `repair` — normalized load +
  repair report)*
- [x] Adjust hue. *(image-engine `adjust` — `hue_degrees`)*
- [ ] Overlay documents with foreground/background control. *(partial:
  backend-only pdf-engine `overlay` composites PDFs as form XObjects;
  no fg/bg channel control; no dispatch)*

## 8. Attachments and embedded document management

- [ ] Add attachments to PDFs.
- [ ] Extract embedded attachments.
- [ ] List attachments.
- [ ] Remove attachments. *(partial backend only:
  `remove_embedded_files` strips the embedded-files name tree; no
  dispatch)*
- [ ] Extract individual attachments.
- [ ] Process multiple attachments in batches.
- [ ] Create PDF portfolios. *(backend only: `create_portfolio` +
  `list_portfolio`; no dispatch)*
- [ ] Manage embedded files. *(partial: dispatched `sanitize` strips
  embedded files; backend-only `remove_embedded_files`; still no read
  access)*
- [x] Inspect document metadata. *(dispatched `inspect` /
  `extract_metadata`)*
- [ ] Inspect PDF fonts. *(❌ `ops/structure_tree.rs` and font data
  live in ops modules without engine wrappers)*
- [x] Inspect page dimensions. *(via dispatched `inspect`/
  `extract_metadata`)*
- [x] Inspect permissions and document properties. *(dispatched
  `inspect` reports encryption state + permission flags)*
- [ ] Inspect embedded JavaScript. *(❌ `ops/inspect_javascript.rs`
  exists but has **no engine method** — ops-module only)*

## 9. Automation and workflow processing

- [x] Build reusable processing workflows. *(pipeline-engine
  `PipelineDefinition` — serde-serializable, ordered steps)*
- [x] Chain multiple PDF operations. *(linear step sequences over
  `pdf`/`security` engines)*
- [x] Save automation pipelines. *(partial: definitions are data and
  serializable; built-ins `pdf.intake`/`pdf.extract-text`; no persisted
  user pipeline store)*
- [x] Run workflows repeatedly. *(via job engine / CLI)*
- [x] Process multiple documents. *(durable job queue, batch enqueue)*
- [x] Batch-process files. *(partial: jobs are per-document; no
  multi-file batch job primitive)*
- [x] Automate document conversion. *(conversion engine + jobs)*
- [ ] Automate OCR. *(partial: `ocr` **is** dispatched as a
  `document.pdf_op` job op, but it is not a named pipeline step)*
- [x] Automate compression. *(dispatched `compress` op; not a named
  pipeline step)*
- [ ] Automate document renaming. *(security filename sanitization
  exists; no content-based auto-rename)*
- [x] Process files through APIs. *(REST API: raw-body upload with
  magic-byte sniffing + `x-file-name`, artifact download, and
  `/documents/{id}/process` enqueueing metadata/index/thumbnail jobs
  plus the 25 pdf ops)*
- [ ] Automate folder scanning.
- [ ] Watch folders for new documents.
- [ ] Automatically process incoming files. *(partial: ingest can be
  enqueued as `document.ingest` job; no filesystem watcher)*
- [x] Route processed documents into output folders. *(partial: folder
  model + `move_document`; no automated routing rules)*
- [x] Configure pipeline processing stages. *(per-step `params`)*

## 10. Developer and integration features

- [x] REST API. *(axum server in `src/crates/api` via `kaladoc serve` —
  document create/list/get/patch/delete, restore, versions +
  version restore, tags, history, artifact download, `/process` job
  enqueue (metadata_extract/index/thumbnail + `PDF_OPS`), folders,
  jobs list/get/cancel/retry, engines, storage, health; Bearer session
  auth on all but `/health`)*
- [x] Command-line interface. *(not a Stirling feature, but the primary
  current surface: `version`, `doctor`, `init` (auto `.env` secrets),
  `config`, `auth`, `user`, `jobs` (worker/list/enqueue/cancel/retry/
  recover), `engine list|health`, `doc` CRUD + folders, `serve`;
  stable exit codes + `--output json`)*
- [ ] API documentation. *(endpoint table in crate docs; no OpenAPI)*
- [x] API authentication. *(Bearer session tokens validated against
  `sessions_tbl` on every protected route)*
- [x] API endpoint configuration. *(partial: `api.addr` bind address via
  `kaladoc.toml`/`KALADOC_API_ADDR`; no per-endpoint enable/disable)*
- [ ] OpenAPI integration.
- [x] Batch processing through API. *(partial: job enqueue endpoints;
  no bulk-operation endpoint)*
- [ ] Automated folder scanning.
- [ ] MCP integration for AI agents. *(adapter layer named
  `cli_mcp_server` exists; no MCP server implementation found)*
- [ ] Workflow automation APIs. *(pipeline engine is library-level;
  not exposed over HTTP)*
- [x] External engine integration. *(conversion-engine backend trait +
  pandoc/libreoffice/poppler detection; pdf-engine shells out to
  `pdftoppm`/`tesseract`/`openssl` with `MissingTool` probing)*
- [x] Configurable feature flags. *(partial: `KALADOC_*` env config,
  `AppConfig`; no per-endpoint flags)*
- [x] Custom application configuration. *(builder + env-based config)*
- [x] Health and operational endpoints. *(public `GET /health`,
  authenticated `/engines`, `/storage`; pdf-engine `health` reports
  degraded when `pdftoppm`/`tesseract`/`openssl` are missing;
  `doctor` CLI)*

## 11. Enterprise and administration

- [x] User management. *(CLI `user` commands + Organization store)*
- [x] Authentication. *(session tokens, `auth` CLI)*
- [x] Role-based access control. *(partial: `permissions` store with
  user/group/role queries; enforcement depth TBD)*
- [ ] Single Sign-On (SSO).
- [ ] OAuth/OIDC integrations.
- [ ] LDAP integrations.
- [x] Audit logging. *(document history events + audit store)*
- [ ] API key management.
- [x] Feature-level permissions. *(partial: permissions store)*
- [ ] Endpoint enable/disable controls.
- [x] User-specific settings. *(partial: per-owner scoping)*
- [x] Server configuration. *(env + config file)*
- [ ] Custom branding.
- [ ] Multi-language interface.
- [x] Docker deployment. *(partial: `src/deployments` packaging for
  Arch/deb/fedora/tar/zip; no Dockerfiles)*
- [ ] Kubernetes deployment.
- [x] Desktop deployment. *(partial: Windows/Linux packaging targets)*
- [x] Air-gapped deployment. *(partial: fully local stack — SQLite +
  filesystem/S3 storage, no external service calls; external conversion
  tools are optional probes, not requirements)*
- [x] On-premises hosting. *(local SQLite + filesystem/S3 storage)*

## 12. AI and intelligent features

- [ ] AI assistant.
- [x] Document ingestion. *(streaming ingestion pipeline: staged write,
  sniffing, version/artifact registration, recovery)*
- [x] Document parsing. *(partial: text/metadata extraction, page
  structure)*
- [ ] Document classification.
- [x] AI-assisted document workflows. *(partial: pipeline engine +
  job orchestration; no model integration)*
- [ ] MCP-based AI tool invocation.
- [x] Automated document processing. *(job engine: durable queue,
  retries with backoff, cancel, scheduler)*
- [x] Structured document extraction. *(partial: metadata extract,
  text extract, image extract; no table/KV extraction)*

## 13. pdf-engine surface — implementation audit

Round-2 audit of `src/crates/pdf-engine`. Three tiers:

- ✅ **dispatched** — ops file + `PdfEngine` method + `dispatch_pdf_op`
  arm (reachable via `document.pdf_op` jobs / `POST
  /documents/{id}/process`).
- ⚠️ **backend only** — ops file + `PdfEngine` method, no dispatch arm.
- ❌ **not wired** — ops file exists but has no `PdfEngine` wrapper
  (❌† = additionally not declared in `ops/mod.rs`, i.e. a dead file).

`ops/load.rs`, `ops/raster.rs` (`pdftoppm` rasterization shared by
`render`/`ocr`/`searchable_pdf`) and `ops/mod.rs` are infrastructure,
not operations.

### 13.1 `ops/*.rs` modules (84 files)

| Status | Modules |
|---|---|
| ✅ dispatched (25 op modules) | `compare`, `compress`, `convert`, `decrypt`, `encrypt`, `extract` (pages), `extract_images`, `extract_metadata`, `flatten`, `info`, `ocr`, `page_count`, `redact`, `remove_pages`, `render`, `reorder`, `repair`, `sanitize`, `split`, `stamp`, `text` (extract_text), `validate`, `verify_signature`, `watermark` |
| ⚠️ backend only — engine method, no dispatch | `annotations`, `article_threads`, `booklet`, `crop`, `crop_view`, `draw_shapes`, `draw_text`, `duplicate_pages`, `edit_text`, `form_fields`, `freehand`, `garbage_collect`, `grayscale`, `headers_footers`, `insert_blank`, `interleave`, `invert_colors`, `linearize`, `merge`, `merge_modes`, `nup`, `outline`, `overlay`, `page_labels`, `pdfa`, `pdfua`, `pdfx`, `portfolio`, `poster`, `print_marks`, `remove_blank_pages`, `remove_images`, `remove_objects`, `replace_image`, `resize`, `rotate_view`, `scale_content`, `searchable_pdf`, `set_metadata`, `set_xmp_metadata`, `sign`, `sign_timestamp`, `single_page`, `split_bookmarks`, `split_modes`, `text_markup`, `timestamp`, `unlock_fields` |
| ❌ ops module only — no `PdfEngine` method | `cert_info`, `doc_time`, `edit_field_props`, `inspect_javascript`, `page_size`, `structure_tree` |
| ❌† dead file — not even declared in `ops/mod.rs` | `auto_orient` |

(`raster` is shared `pub(crate)` infrastructure used by dispatched
`render`/`ocr`/`searchable_pdf`, not an op; `load` is the shared
loader and `pages` the shared range parser — `PdfEngine::
parse_page_range` exposes the latter directly.)

### 13.2 `PdfEngine` public methods (`engine.rs`)

~105 `pub fn`s (plus `new`/`with_limits`/`limits`/`Default` and the
`ProcessingEngine` impl — capabilities list 33 `EngineOperation`s;
`health` probes `pdftoppm`/`tesseract`/`openssl` and reports
`Degraded` when absent):

- **Page ops:** `page_count`, `merge`, `split`, `extract_pages`,
  `remove_pages`, `reorder`, `rotate`, `rotate_degrees`, `crop`,
  `resize`, `parse_page_range`.
- **Document ops:** `compress`, `repair`, `flatten`, `overlay`,
  `sanitize`, `insert_blank`, `duplicate_pages`, `remove_blank_pages`,
  `interleave`, `merge_mode`, `nup`, `single_page`, `poster`,
  `booklet`, `scale_content`, `split_mode`, `split_at_outline`,
  `grayscale`, `invert_colors`, `draw_shapes`, `remove_images`,
  `replace_image`, `linearize`, `garbage_collect`, `make_searchable`,
  `remove_named_objects`, `remove_embedded_files`,
  `remove_javascript_names`.
- **Marks:** `watermark`, `stamp`, `draw_text`, `headers_footers`,
  `add_page_numbers`, `add_print_marks`.
- **Annotations/articles/ink:** `add_comment`, `add_freetext`,
  `list_annotations`, `remove_annotations`, `list_articles`,
  `remove_articles`, `add_ink`, `list_ink`, `remove_ink`.
- **Text & forms:** `text_markup`, `edit_text`, `list_form_fields`,
  `create_text_field`, `delete_form_field`, `fill_form`, `clear_form`,
  `unlock_fields`, `lock_fields`.
- **Metadata/structure:** `set_page_labels`, `clear_page_labels`,
  `get_page_labels`, `set_outline`, `remove_outline`, `list_outline`,
  `set_page_rotation`, `get_page_rotations`, `normalize_rotations`,
  `set_view_crop`, `clear_crop`, `set_metadata`, `clear_metadata`,
  `set_xmp`, `get_xmp`, `to_pdfa`, `to_pdfua`, `to_pdfx`.
- **Security/signatures/timestamps:** `redact`, `encrypt`, `decrypt`,
  `verify_signatures`, `verify_signatures_crypto`, `sign`,
  `remove_signatures`, `list_signature_timestamps`, `has_timestamps`,
  `document_digest`, `timestamp_query`, `build_timestamp_request`,
  `request_external_timestamp`, `timestamp_document`.
- **Inspection/extraction/conversion:** `inspect`, `extract_metadata`,
  `validate`, `extract_images`, `extract_text`, `convert`, `render`,
  `ocr`, `ocr_languages`, `compare`.
- **Portfolio:** `list_portfolio`, `create_portfolio`.

### 13.3 `dispatch_pdf_op` arms (`job_handlers.rs`)

`PDF_OPS` / `dispatch_pdf_op` expose exactly 25 ops — the ✅ set in
13.1: `page_count`, `inspect`, `validate`, `repair`,
`verify_signatures`, `extract_metadata`, `extract_text`,
`extract_images`, `ocr`, `compare`, `convert`, `render`, `compress`,
`flatten`, `sanitize`, `rotate`, `split`, `extract_pages`,
`remove_pages`, `reorder`, `watermark`, `stamp`, `encrypt`, `decrypt`,
`redact`. Everything else with an engine method is **backend only**;
adding a dispatch arm means: a `PDF_OPS` entry, a `match` arm in
`dispatch_pdf_op`, and (for byte outputs) the `pdf_outcome`/`OpFile`
plumbing already used by `render`/`split`.

## Beyond-parity target capabilities — status

- [x] Streaming document processing. *(streaming storage backend +
  staged ingestion)*
- [ ] Incremental processing. *(partial: page-range specs allow
  per-page ops; no page-level job granularity)*
- [x] Plugin-based conversion engines. *(backend trait + availability
  detection + fallback chain)*
- [x] Transactional document operations. *(partial: staged writes +
  ingestion recovery; ops themselves are not transactional)*
- [x] Document versioning. *(versions store + restore)*
- [x] Job orchestration. *(durable SQLite queue, priorities, retries,
  cancel, scheduler ticks, crash recovery)*
- [ ] Workflow DAGs. *(pipelines are linear only — no branches,
  parallel steps, or dependencies)*
- [ ] Content-addressed storage. *(partial: SHA-256 checksums stored
  per artifact; no dedup on ingest)*
- [x] Document fingerprints. *(partial: SHA-256 + compare-mode
  structure fingerprints)*
- [ ] Structured extraction. *(partial: metadata/text/images only)*
- [x] Resource limits. *(EngineLimits + security limits + pipeline
  limits + upload caps)*
- [ ] Sandboxed external engines. *(external tools run unsandboxed;
  timeouts and size caps only)*
- [x] Content validation. *(partial: conversion output sanity checks,
  dispatched `validate` op, security upload gate)*
- [ ] Event-driven processing. *(no event bus / webhooks)*
- [x] Resumable jobs. *(job engine reaping + ingestion `recover`)*

## Official references

- Stirling-PDF tool catalogue: <https://docs.stirlingpdf.com/Functionality/Recommended-Tools/>
- Stirling-PDF advanced tools: <https://docs.stirlingpdf.com/Functionality/Advanced-Tools/>
- Feature flags and endpoint configuration: <https://docs.stirlingpdf.com/Server/Customisation/Features-and-Endpoints/>
- Stirling-PDF GitHub repository: <https://github.com/Stirling-Tools/Stirling-PDF>
