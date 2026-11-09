# Stirling-PDF Advanced Features — Target Documentation

Reference inventory of Stirling-PDF's advanced capabilities, captured for
archival by Applaicon and used as the feature-parity target for KalaDoc.

> **Legend.** `[x]` = implemented in KalaDoc (partial implementations are
> annotated). `[ ]` = not implemented. This is a feature *reference*, not
> an implementation requirement — reliable document primitives come
> first, then higher-level engines.

## 1. Advanced PDF editing

- [ ] Edit existing PDF text.
- [ ] Edit or replace images inside PDFs.
- [ ] Add text at arbitrary page coordinates. *(partial: `stamp`/`watermark`
  place text or images at anchored positions, not arbitrary coordinates)*
- [x] Add images, logos and watermarks. *(pdf-engine `stamp` — text or PNG
  image — and `watermark`; image-engine `watermark` for raster images)*
- [ ] Draw shapes and freehand annotations.
- [ ] Highlight, underline and strike through text.
- [ ] Add comments and sticky notes.
- [ ] Edit document metadata. *(inspection exists via `extract_metadata`;
  no write path)*
- [ ] Edit bookmarks and table of contents.
- [ ] Edit interactive PDF forms.
- [ ] Fill existing forms.
- [ ] Detect and create form fields automatically.
- [x] Flatten interactive forms. *(best-effort via `flatten` — merges
  annotation/appearance content into page streams)*
- [ ] Unlock read-only form fields.
- [ ] Remove annotations. *(flatten merges rather than removes)*
- [ ] Remove selected images.
- [x] Compare two PDF documents. *(pdf-engine `compare` — text, structure
  and byte modes with diff report)*
- [x] Find differences between documents. *(same `compare` op)*

## 2. Advanced page manipulation

- [x] Merge multiple PDFs. *(pdf-engine `merge`; note: outlines,
  signatures and document-level structures are not carried over)*
- [x] Split PDFs by page ranges. *(via `extract_pages` + `remove_pages`
  composition; `split` itself splits by count only)*
- [x] Split PDFs into individual pages. *(split with `pages_per_file = 1`)*
- [x] Split by page count. *(pdf-engine `split`)*
- [ ] Split by bookmarks.
- [x] Extract selected pages. *(1-based range spec `"1-3,8"`)*
- [x] Delete selected pages. *(pdf-engine `remove_pages`)*
- [ ] Remove blank pages automatically.
- [ ] Detect page orientation and auto-rotate.
- [x] Rotate individual pages. *(range spec + 90/180/270)*
- [x] Rearrange pages. *(pdf-engine `reorder` — full permutation spec;
  programmatic, not drag-and-drop UI)*
- [ ] Duplicate pages. *(reorder requires each page exactly once)*
- [ ] Insert blank pages.
- [x] Crop pages manually. *(sets `/CropBox`)*
- [x] Resize pages. *(page boxes only — content is not scaled)*
- [ ] Scale page content.
- [ ] Add page numbers.
- [ ] Add headers and footers.
- [ ] Multi-page layouts (N-up).
- [ ] Booklet imposition.
- [ ] Poster printing / tile splitting.
- [ ] Convert PDF into a single large page.
- [x] Overlay multiple PDFs. *(pdf-engine `overlay` — form XObjects,
  optional page selection)*
- [ ] Interleave pages from multiple documents.

## 3. Document conversion engine

Supported formats today: `pdf`, `txt`, `markdown`, `html`, `png`, `jpeg`,
`docx`, `odt` — backed by pandoc, LibreOffice, poppler (external) plus a
pure-Rust fallback, dispatched with automatic fallback.

### To PDF

- [x] DOCX → PDF *(pandoc / LibreOffice)*
- [ ] XLS / XLSX → PDF *(format not in the `Format` enum)*
- [ ] PPT / PPTX → PDF
- [x] ODT → PDF *(LibreOffice / pandoc)*
- [x] HTML → PDF *(pandoc)*
- [x] Markdown → PDF *(pandoc + Rust backend)*
- [x] TXT → PDF *(pandoc + Rust backend)*
- [ ] Images → PDF
- [ ] EPUB → PDF
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
- [x] PDF → Markdown *(pdf-engine `convert` — best-effort text-layer
  conversion)*
- [ ] PDF → XML
- [x] PDF → JSON-oriented extraction workflows. *(partial:
  `extract_metadata` returns structured metadata; no general JSON export)*
- [x] PDF → Images. *(partial: poppler backend rasterizes **first page
  only**, PNG or JPEG)*
- [x] PDF → Plain text. *(pdf-engine `convert`/`extract_text`)*
- [ ] PDF → EPUB
- [ ] PDF → PDF/A
- [ ] PDF → PDF/UA
- [ ] PDF → RTF
- [ ] PDF → CBZ / CBR
- [ ] PDF → Single-page images. *(partial — first page only, see above)*

> Actual format availability depends on the installed conversion engines.
> Docx↔Odt conversion is supported via LibreOffice.

## 4. OCR and intelligent document processing

- [x] OCR scanned documents. *(partial: `ocr` returns per-page text via
  `pdftoppm` + `tesseract`; it does **not** produce a searchable PDF)*
- [ ] Make scanned PDFs searchable.
- [x] Extract text from images. *(partial: tesseract path via PDF raster;
  image-engine has no direct OCR op)*
- [x] Support multiple OCR languages. *(`ocr_languages` lists installed
  tesseract codes; per-request language selection not exposed)*
- [ ] Automatically rotate scanned pages.
- [ ] Deskew scanned documents.
- [ ] Remove scanning noise.
- [ ] Clean scanned backgrounds.
- [ ] Detect text orientation.
- [x] Extract images from PDFs. *(pdf-engine `extract_images`)*
- [ ] Extract photographs from scanned pages.
- [ ] Detect and split multiple photographs.
- [ ] Automatically rename PDFs based on document content.
- [x] Detect document structure. *(partial: `inspect`,
  `extract_metadata`, pipeline `pdf.intake` reports)*
- [x] Extract document information. *(pdf-engine `extract_metadata` —
  Info dict, dates, keywords, XMP)*
- [ ] Automatic form detection.
- [x] Document classification and processing workflows. *(partial:
  linear pipeline definitions + ingestion/metadata/index jobs)*

## 5. Advanced security and digital signatures

- [x] Password-protect PDFs. *(AES-256 encrypt, user/owner passwords)*
- [x] Remove PDF passwords. *(decrypt)*
- [x] Change PDF permissions. *(restrict printing/copying/editing via
  `PdfPermissions` flags)*
- [x] Restrict printing.
- [x] Restrict copying.
- [x] Restrict editing.
- [ ] Apply digital certificate signatures. *(`sign` exists but always
  returns `UnsupportedOperation` — no CMS/certificate infrastructure)*
- [ ] Sign using PEM certificates.
- [ ] Sign using PKCS#12/P12 certificates.
- [x] Validate digital signatures. *(partial: `verify_signatures`
  produces a **structural** report — presence and byte-range sanity,
  not cryptographic verification)*
- [ ] Verify certificate chains.
- [ ] Remove certificate signatures.
- [ ] Add timestamp signatures.
- [ ] RFC 3161 timestamping.
- [ ] Permanently redact sensitive content.
- [x] Redact selected text. *(partial: `redact` removes matching text
  strings from content streams — best-effort, text terms only; no
  region/page redaction)*
- [ ] Redact selected regions.
- [ ] Redact entire pages.
- [x] Sanitize PDF documents. *(pdf-engine `sanitize`)*
- [x] Remove embedded JavaScript. *(via `sanitize`)*
- [x] Remove potentially dangerous embedded files. *(via `sanitize`)*
- [x] Flatten forms and annotations. *(via `flatten`)*
- [x] Validate PDF compliance. *(pdf-engine `validate` →
  `ValidationReport`)*

> **Important:** True redaction must remove underlying content. Current
> `redact` is best-effort text removal — region/page-level permanent
> redaction is still open.

## 6. Compression and optimization

- [x] Compress PDF files. *(partial: `compress` recompresses object/
  xref streams via lopdf; no image-level optimization)*
- [ ] Optimize embedded images.
- [ ] Reduce image resolution. *(image-engine `compress`/`resize` cover
  raster inputs, not embedded PDF images)*
- [ ] Adjust JPEG quality. *(image-engine `compress` supports quality
  for JPEG output)*
- [ ] Remove unnecessary objects.
- [ ] Remove unused resources.
- [ ] Optimize PDF structure.
- [ ] Reduce redundant content.
- [ ] Linearize PDFs for faster web viewing.
- [ ] Convert images to more efficient formats. *(image-engine
  `convert` covers png/jpeg/gif/webp/bmp for standalone images)*
- [ ] Optimize documents for printing.
- [ ] Optimize documents for archival storage.

## 7. Advanced visual processing

*(image-engine ops apply to raster images — PNG/JPEG/GIF/WebP/BMP —
not directly to PDF page content)*

- [x] Adjust brightness. *(image-engine `adjust` — ±100% via
  `imageops::brighten`)*
- [x] Adjust contrast. *(image-engine `adjust`)*
- [ ] Adjust saturation.
- [ ] Adjust RGB channels independently.
- [ ] Invert document colors.
- [ ] Replace foreground and background colors.
- [ ] Create high-contrast documents.
- [ ] Convert colors to CMYK.
- [ ] Apply scanner effects.
- [ ] Simulate photocopy effects.
- [ ] Apply grayscale.
- [x] Repair damaged PDFs. *(pdf-engine `repair` — normalized load +
  repair report)*
- [x] Adjust hue. *(image-engine `adjust` — `hue_degrees`)*
- [x] Overlay documents with foreground/background control. *(partial:
  pdf-engine `overlay` composites PDFs; no fg/bg channel control)*

## 8. Attachments and embedded document management

- [ ] Add attachments to PDFs.
- [ ] Extract embedded attachments.
- [ ] List attachments.
- [ ] Remove attachments.
- [ ] Extract individual attachments.
- [ ] Process multiple attachments in batches.
- [ ] Create PDF portfolios.
- [ ] Manage embedded files. *(partial: `sanitize` strips embedded
  files; no read access)*
- [x] Inspect document metadata. *(pdf-engine `inspect` /
  `extract_metadata`)*
- [ ] Inspect PDF fonts.
- [x] Inspect page dimensions. *(via `inspect`/`extract_metadata`)*
- [x] Inspect permissions and document properties. *(encryption state +
  permission flags reported)*
- [ ] Inspect embedded JavaScript.

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
- [ ] Automate OCR. *(ocr op exists; not wired as a named pipeline
  operation)*
- [x] Automate compression. *(engine op; not a named pipeline step)*
- [ ] Automate document renaming. *(security filename sanitization
  exists; no content-based auto-rename)*
- [x] Process files through APIs. *(REST API upload + process
  endpoints)*
- [ ] Automate folder scanning.
- [ ] Watch folders for new documents.
- [ ] Automatically process incoming files. *(partial: ingest can be
  enqueued as `document.ingest` job; no filesystem watcher)*
- [x] Route processed documents into output folders. *(partial: folder
  model + `move_document`; no automated routing rules)*
- [x] Configure pipeline processing stages. *(per-step `params`)*

## 10. Developer and integration features

- [x] REST API. *(axum server, ~20 endpoints: documents, folders,
  versions, tags, history, jobs, engines, storage, health)*
- [ ] API documentation. *(endpoint table in crate docs; no OpenAPI)*
- [x] API authentication. *(Bearer session tokens validated against
  `sessions_tbl` on every request)*
- [ ] API endpoint configuration.
- [ ] OpenAPI integration.
- [x] Batch processing through API. *(partial: job enqueue endpoints;
  no bulk-operation endpoint)*
- [ ] Automated folder scanning.
- [ ] MCP integration for AI agents. *(adapter layer named
  `cli_mcp_server` exists; no MCP server implementation found)*
- [ ] Workflow automation APIs. *(pipeline engine is library-level;
  not exposed over HTTP)*
- [x] External engine integration. *(conversion-engine backend trait +
  pandoc/libreoffice/poppler detection)*
- [x] Configurable feature flags. *(partial: `KALADOC_*` env config,
  `AppConfig`; no per-endpoint flags)*
- [x] Custom application configuration. *(builder + env-based config)*
- [x] Health and operational endpoints. *(public `GET /health`,
  authenticated `/engines`, `/storage`; `doctor` CLI)*

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
- [ ] Air-gapped deployment.
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
  `validate` op, security upload gate)*
- [ ] Event-driven processing. *(no event bus / webhooks)*
- [x] Resumable jobs. *(job engine reaping + ingestion `recover`)*

## Official references

- Stirling-PDF tool catalogue: <https://docs.stirlingpdf.com/Functionality/Recommended-Tools/>
- Stirling-PDF advanced tools: <https://docs.stirlingpdf.com/Functionality/Advanced-Tools/>
- Feature flags and endpoint configuration: <https://docs.stirlingpdf.com/Server/Customisation/Features-and-Endpoints/>
- Stirling-PDF GitHub repository: <https://github.com/Stirling-Tools/Stirling-PDF>
