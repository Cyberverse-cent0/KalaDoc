# Stirling-PDF Advanced Features — Target Documentation

Reference inventory of Stirling-PDF's advanced capabilities, captured for
archival by Applaicon and used as the feature-parity target for KalaDoc.

> **Legend.** `[x]` = implemented in KalaDoc (partial implementations are
> annotated). `[ ]` = not implemented. Engine-level ops are the source of
> truth — API/CLI/MCP exposure is tracked separately in section 10.

## Scope for KalaDoc

- **Frontend features are out of scope for now.** Anything that exists
  only to serve a web/desktop UI — drag-and-drop page rearrangement,
  custom branding, multi-language interface, admin consoles, SSO/OIDC/LDAP
  portals — is deferred. Engine capabilities underneath them remain in
  scope where they make sense as library/API operations.
- **The backend is the absolute target.** All work targets the engine
  crates (`pdf-engine`, `conversion-engine`, `image-engine`,
  `pipeline-engine`, `job_engine`, `security-engine`, `document_engine`,
  `multimedia-engine`) and the services above them.
- **The backend API is being constructed.** The axum REST API now exposes
  documents, folders, versions, tags, history, jobs (incl. batches),
  pipelines, engines, storage, health, OpenAPI, and a generic
  `document.pdf_op` surface covering the pdf-engine ops; an MCP server
  (`cli_mcp_server/mcp`) exposes the same services to AI agents.

## 1. Advanced PDF editing

- [x] Edit existing PDF text. *(pdf-engine `edit_text`)*
- [x] Edit or replace images inside PDFs. *(pdf-engine `replace_image`)*
- [x] Add text at arbitrary page coordinates. *(pdf-engine `draw_text`)*
- [x] Add images, logos and watermarks. *(pdf-engine `stamp`,
  `watermark`; image-engine `watermark`)*
- [x] Draw shapes and freehand annotations. *(pdf-engine `draw_shapes`,
  `freehand`)*
- [x] Highlight, underline and strike through text. *(pdf-engine
  `text_markup`)*
- [x] Add comments and sticky notes. *(pdf-engine `annotations`)*
- [x] Edit document metadata. *(pdf-engine `set_metadata`,
  `set_xmp_metadata`, `doc_time`)*
- [x] Edit bookmarks and table of contents. *(pdf-engine `outline`,
  `outline_from_headings`, `named_destinations`)*
- [x] Edit interactive PDF forms. *(pdf-engine `form_fields`,
  `edit_field_props`)*
- [x] Fill existing forms. *(pdf-engine `fill_form`)*
- [x] Detect and create form fields automatically. *(pdf-engine
  `form_fields` detection)*
- [x] Flatten interactive forms. *(pdf-engine `flatten`,
  `unlock_fields`)*
- [x] Unlock read-only form fields. *(pdf-engine `unlock_fields`)*
- [x] Remove annotations. *(pdf-engine `annotations` removal)*
- [x] Remove selected images. *(pdf-engine `remove_images`,
  `dedup_images`)*
- [x] Compare two PDF documents. *(pdf-engine `compare`)*
- [x] Find differences between documents. *(same `compare` op)*

## 2. Advanced page manipulation

- [x] Merge multiple PDFs. *(pdf-engine `merge`; document-level
  structures not carried over)*
- [x] Split PDFs by page ranges. *(`extract_pages` + `remove_pages`,
  `split_modes`)*
- [x] Split PDFs into individual pages. *(split `pages_per_file = 1` /
  `single_page`)*
- [x] Split by page count. *(pdf-engine `split`)*
- [x] Split by bookmarks. *(pdf-engine `split_bookmarks`)*
- [x] Extract selected pages. *(1-based range spec)*
- [x] Delete selected pages. *(pdf-engine `remove_pages`)*
- [x] Remove blank pages automatically. *(pdf-engine
  `remove_blank_pages`)*
- [x] Detect page orientation and auto-rotate. *(pdf-engine
  `auto_orient`)*
- [x] Rotate individual pages. *(range spec + 90/180/270)*
- [x] Rearrange pages. *(pdf-engine `reorder`, `swap_pages`; also
  `rotate_view`, `crop_view`)*
- [x] Duplicate pages. *(pdf-engine `duplicate_pages`)*
- [x] Insert blank pages. *(pdf-engine `insert_blank`)*
- [x] Crop pages manually. *(pdf-engine `crop`, `crop_view`,
  `trim_boxes`)*
- [x] Resize pages. *(pdf-engine `resize`, `page_size`, `page_boxes`)*
- [x] Scale page content. *(pdf-engine `scale_content`)*
- [x] Add page numbers. *(pdf-engine `headers_footers`,
  `print_marks`)*
- [x] Add headers and footers. *(pdf-engine `headers_footers`)*
- [x] Multi-page layouts (N-up). *(pdf-engine `nup`)*
- [x] Booklet imposition. *(pdf-engine `booklet`)*
- [x] Poster printing / tile splitting. *(pdf-engine `poster`)*
- [x] Convert PDF into a single large page. *(pdf-engine `single_page`)*
- [x] Overlay multiple PDFs. *(pdf-engine `overlay`, `merge_modes`)*
- [x] Interleave pages from multiple documents. *(pdf-engine
  `interleave`)*

## 3. Document conversion engine

`Format` now covers pdf, txt, markdown, html, png, jpeg, docx, odt,
xlsx, xls, pptx, ppt, epub, cbz, cbr (and more) — backends: pure-Rust
(lopdf/image), pandoc, LibreOffice, poppler, plus archive handling in the
Rust backend.

### To PDF

- [x] DOC/DOCX → PDF *(LibreOffice/pandoc)*
- [x] XLS/XLSX → PDF *(LibreOffice)*
- [x] PPT/PPTX → PDF *(LibreOffice)*
- [x] ODT/ODS/ODP → PDF *(LibreOffice/pandoc)*
- [x] HTML → PDF *(pandoc)*
- [x] Markdown → PDF *(pandoc + Rust backend)*
- [x] TXT → PDF *(pandoc + Rust backend)*
- [x] Images → PDF *(Rust backend embeds PNG/JPEG; LibreOffice fallback)*
- [x] EPUB → PDF *(pandoc, when a PDF engine is present)*
- [ ] Email (EML) → PDF
- [ ] URL → PDF
- [x] HTML files → PDF *(HTML input covered)*
- [x] CBZ / CBR → PDF *(Rust backend archive reader)*

### From PDF

- [ ] PDF → Word
- [ ] PDF → Excel
- [ ] PDF → PowerPoint
- [ ] PDF → CSV. *(partial: `extract_tables` can emit tabular data)*
- [x] PDF → HTML *(poppler `pdftohtml`)*
- [x] PDF → Markdown *(pdf-engine `convert` + Rust backend)*
- [ ] PDF → XML
- [x] PDF → JSON-oriented extraction workflows. *(partial:
  `extract_metadata`, `extract_tables`, structured reports)*
- [x] PDF → Images *(poppler `pdftoppm`; first-page contract)*
- [x] PDF → Plain text *(pdf-engine `convert`, poppler `pdftotext`)*
- [x] PDF → EPUB *(pandoc)*
- [x] PDF → PDF/A *(pdf-engine `pdfa` — honest declaration subset;
  full conformance needs external validation)*
- [x] PDF → PDF/UA *(pdf-engine `pdfua` — same honest-partial model)*
- [ ] PDF → RTF
- [x] PDF → CBZ / CBR *(archive packaging ops)*
- [x] PDF → Single-page images. *(partial: first page via poppler;
  `render_pages` covers multi-page)*

> Actual format availability depends on the installed conversion
> engines; backends degrade gracefully via availability detection.

## 4. OCR and intelligent document processing

- [x] OCR scanned documents. *(pdf-engine `ocr`, `searchable_pdf`)*
- [x] Make scanned PDFs searchable. *(pdf-engine `searchable_pdf` —
  invisible text layer over raster)*
- [x] Extract text from images. *(image-engine `ocr` via tesseract)*
- [x] Support multiple OCR languages. *(`ocr_languages`; per-request
  language selection where exposed)*
- [x] Automatically rotate scanned pages. *(pdf-engine `auto_orient`)*
- [x] Deskew scanned documents. *(pdf-engine `deskew`)*
- [x] Remove scanning noise. *(pdf-engine `clean_scan`)*
- [x] Clean scanned backgrounds. *(pdf-engine `clean_scan`)*
- [x] Detect text orientation. *(pdf-engine `auto_orient`)*
- [x] Extract images from PDFs. *(pdf-engine `extract_images`)*
- [x] Extract photographs from scanned pages. *(via `extract_images` +
  image pipeline)*
- [ ] Detect and split multiple photographs.
- [x] Automatically rename PDFs based on document content.
  *(job kind `document.auto_rename`)*
- [x] Detect document structure. *(`inspect`, `extract_metadata`,
  `structure_tree`, `content_audit`, pipeline `pdf.intake`)*
- [x] Extract document information. *(pdf-engine `extract_metadata`)*
- [x] Automatic form detection. *(pdf-engine `form_fields`)*
- [x] Document classification and processing workflows. *(pipeline
  engine + ingestion/index/auto-rename jobs)*
- [x] Extract tables. *(pdf-engine `extract_tables`)*

## 5. Advanced security and digital signatures

- [x] Password-protect PDFs. *(AES-256 `encrypt`, user/owner passwords)*
- [x] Remove PDF passwords. *(pdf-engine `decrypt`)*
- [x] Change PDF permissions. *(`PdfPermissions` flags)*
- [x] Restrict printing.
- [x] Restrict copying.
- [x] Restrict editing.
- [x] Apply digital certificate signatures. *(pdf-engine `sign` —
  CMS via `openssl cms -sign`, PEM and PKCS#12 credentials)*
- [x] Sign using PEM certificates.
- [x] Sign using PKCS#12/P12 certificates.
- [x] Validate digital signatures. *(pdf-engine
  `verify_signatures_crypto` — digest + optional chain verify)*
- [x] Verify certificate chains. *(when trust anchors supplied)*
- [x] Remove certificate signatures. *(pdf-engine
  `remove_signatures`)*
- [x] Add timestamp signatures. *(partial: `timestamp`,
  `sign_timestamp` provide RFC 3161 plumbing/inspection; embedding a
  DocTimeStamp inside a signature is documented unsupported)*
- [x] RFC 3161 timestamping. *(partial — see above)*
- [x] Permanently redact sensitive content. *(pdf-engine
  `redact_regions`, `redact_pages`)*
- [x] Redact selected text. *(pdf-engine `redact`)*
- [x] Redact selected regions. *(pdf-engine `redact_regions`)*
- [x] Redact entire pages. *(pdf-engine `redact_pages`)*
- [x] Sanitize PDF documents. *(pdf-engine `sanitize`, `remove_objects`,
  `remove_hidden_text`)*
- [x] Remove embedded JavaScript. *(pdf-engine `sanitize`,
  `inspect_javascript`)*
- [x] Remove potentially dangerous embedded files. *(pdf-engine
  `remove_objects`)*
- [x] Flatten forms and annotations. *(pdf-engine `flatten`)*
- [x] Validate PDF compliance. *(pdf-engine `validate`, `pdfx`)*

> Redaction removes underlying page content (regions/pages/text), not
> just paints over it.

## 6. Compression and optimization

- [x] Compress PDF files. *(pdf-engine `compress`,
  `stream_recompress`)*
- [x] Optimize embedded images. *(pdf-engine `dedup_images`;
  image-engine `compress`)*
- [x] Reduce image resolution. *(image-engine `resize`/`compress`)*
- [x] Adjust JPEG quality. *(image-engine `compress`)*
- [x] Remove unnecessary objects. *(pdf-engine `garbage_collect`,
  `object_report`)*
- [x] Remove unused resources. *(pdf-engine `garbage_collect`)*
- [x] Optimize PDF structure. *(pdf-engine `compress`,
  `stream_recompress`, `linearize`)*
- [x] Reduce redundant content. *(pdf-engine `dedup_images`,
  `garbage_collect`)*
- [x] Linearize PDFs for faster web viewing. *(pdf-engine
  `linearize`)*
- [x] Convert images to more efficient formats. *(image-engine
  `convert`)*
- [ ] Optimize documents for printing. *(partial: `print_marks` adds
  printer marks; no dedicated print optimizer)*
- [x] Optimize documents for archival storage. *(pdf-engine
  `compress`, `linearize`, `pdfa`)*

## 7. Advanced visual processing

- [x] Adjust brightness. *(image-engine `adjust`)*
- [x] Adjust contrast. *(image-engine `adjust`)*
- [x] Adjust saturation. *(image-engine `color`)*
- [x] Adjust hue. *(image-engine `adjust`/`color`)*
- [x] Adjust RGB channels independently. *(image-engine `color`
  `ChannelAdjust`)*
- [x] Invert document colors. *(image-engine `color`/`effects`;
  pdf-engine `invert_colors`, `grayscale`)*
- [x] Replace foreground and background colors. *(image-engine
  `replace_color`)*
- [x] Create high-contrast documents. *(image-engine `color`
  high-contrast, `photocopy`, `clean_scan`)*
- [x] Convert colors to CMYK. *(image-engine `cmyk` → CMYK TIFF)*
- [x] Apply scanner effects. *(image-engine `scan_fx`)*
- [x] Simulate photocopy effects. *(image-engine `photocopy`)*
- [x] Apply grayscale. *(image-engine `color`/`effects`; pdf-engine
  `grayscale`)*
- [x] Levels/gamma curves. *(image-engine `levels`)*
- [x] Repair damaged PDFs. *(pdf-engine `repair`)*
- [x] Overlay documents with foreground/background control. *(partial:
  pdf-engine `overlay`, `merge_modes`; no fg/bg channel control)*

## 8. Attachments and embedded document management

- [x] Add attachments to PDFs. *(pdf-engine `attachments`,
  `package_files`)*
- [x] Extract embedded attachments. *(pdf-engine `attachments`)*
- [x] List attachments. *(pdf-engine `attachments`)*
- [x] Remove attachments. *(pdf-engine `remove_objects`)*
- [x] Extract individual attachments. *(pdf-engine `attachments`)*
- [x] Process multiple attachments in batches. *(job engine batch
  support)*
- [x] Create PDF portfolios. *(pdf-engine `portfolio`)*
- [x] Manage embedded files. *(pdf-engine `attachments`,
  `remove_objects`)*
- [x] Inspect document metadata. *(pdf-engine `inspect`,
  `extract_metadata`)*
- [x] Inspect PDF fonts. *(pdf-engine `inspect_fonts`, `font_audit`)*
- [x] Inspect page dimensions. *(pdf-engine `page_size`,
  `page_boxes`, `extract_metadata`)*
- [x] Inspect permissions and document properties. *(pdf-engine
  `permissions_report`, `info`)*
- [x] Inspect embedded JavaScript. *(pdf-engine
  `inspect_javascript`)*
- [x] Inspect document structure/optional content/layers.
  *(pdf-engine `structure_tree`, `optional_content`, `layers`,
  `article_threads`, `bates`, `page_labels`, `page_transitions`,
  `links`, `cert_info`, `content_audit`)*

## 9. Automation and workflow processing

- [x] Build reusable processing workflows. *(pipeline-engine
  `PipelineDefinition` — ~150 operations)*
- [x] Chain multiple PDF operations. *(ordered steps across pdf /
  security / conversion / image / multimedia engines)*
- [x] Save automation pipelines. *(serde-serializable definitions +
  built-ins)*
- [x] Run workflows repeatedly. *(job engine + `pipeline.run` jobs)*
- [x] Process multiple documents. *(durable queue, `enqueue_batch`,
  per-item results)*
- [x] Batch-process files. *(job_engine `batch_id` groups,
  `batch_progress`, `cancel_batch`)*
- [x] Automate document conversion. *(conversion engine + `convert.run`)*
- [x] Automate OCR. *(pdf `ocr`/`searchable_pdf` pipeline steps)*
- [x] Automate compression. *(pipeline step)*
- [x] Automate document renaming. *(`document.auto_rename` job)*
- [x] Process files through APIs. *(REST + MCP)*
- [x] Automate folder scanning. *(application `folder_watch`
  service)*
- [x] Watch folders for new documents. *(folder_watch inbox scan)*
- [x] Automatically process incoming files. *(folder_watch →
  `document.ingest`)*
- [x] Route processed documents into output folders. *(folder model +
  `move_document`)*
- [x] Configure pipeline processing stages. *(per-step `params`,
  `PipelineLimits`)*
- [x] Recurring/scheduled jobs. *(scheduler crate: schedules table,
  tick chain)*
- [x] Job progress reporting. *(job_engine `progress_json` +
  `report_progress`)*

## 10. Developer and integration features

- [x] REST API. *(axum: documents, folders, versions, tags, history,
  artifacts, process, jobs/batches, pipelines, engines, storage,
  schedules)*
- [x] API documentation. *(`GET /openapi.json` OpenAPI 3.x + crate-doc
  endpoint table)*
- [x] API authentication. *(Bearer session tokens)*
- [x] API endpoint configuration. *(partial: feature-flag-style
  enablement in config)*
- [x] OpenAPI integration. *(served spec)*
- [x] Batch processing through API. *(`/jobs/batch` routes)*
- [x] Automated folder scanning. *(folder_watch)*
- [x] MCP integration for AI agents. *(rmcp-based MCP server in
  `cli_mcp_server/mcp` — document, job, pipeline, engine tools)*
- [x] Workflow automation APIs. *(`/pipelines`, `/pipelines/run`)*
- [x] External engine integration. *(backend trait + availability
  detection + fallback; multimedia ffmpeg runtime)*
- [x] Configurable feature flags. *(partial: `KALADOC_*` config)*
- [x] Custom application configuration. *(builder + env config)*
- [x] Health and operational endpoints. *(`/health`, `/engines`,
  `/storage`, `doctor`)*
- [x] CLI. *(init, doctor, config, auth, user, jobs, engine, doc,
  serve)*

## 11. Enterprise and administration

- [x] User management. *(CLI + Organization store)*
- [x] Authentication. *(session tokens)*
- [x] Role-based access control. *(permissions store)*
- [ ] Single Sign-On (SSO).
- [ ] OAuth/OIDC integrations.
- [ ] LDAP integrations.
- [x] Audit logging. *(document history + audit store)*
- [ ] API key management.
- [x] Feature-level permissions. *(partial)*
- [x] Endpoint enable/disable controls. *(partial: config groups)*
- [x] User-specific settings. *(owner scoping)*
- [x] Server configuration. *(env + config file)*
- [ ] Custom branding.
- [ ] Multi-language interface.
- [x] Docker deployment. *(partial: packaging targets; no Dockerfiles)*
- [ ] Kubernetes deployment.
- [x] Desktop deployment. *(partial: Windows/Linux packaging)*
- [ ] Air-gapped deployment.
- [x] On-premises hosting. *(local SQLite + filesystem/S3 storage)*

## 12. AI and intelligent features

- [ ] AI assistant.
- [x] Document ingestion. *(streaming pipeline + folder watch)*
- [x] Document parsing. *(text/metadata/tables/structure extraction)*
- [ ] Document classification.
- [x] AI-assisted document workflows. *(pipelines + MCP tools for
  agents)*
- [x] MCP-based AI tool invocation. *(MCP server)*
- [x] Automated document processing. *(job engine + scheduler +
  batches)*
- [x] Structured document extraction. *(metadata, tables, text,
  images, structure tree)*

## Beyond-parity target capabilities — status

- [x] Streaming document processing.
- [x] Incremental processing. *(page-range ops + per-page render)*
- [x] Plugin-based conversion engines.
- [x] Transactional document operations. *(partial: staged writes +
  ingestion recovery)*
- [x] Document versioning.
- [x] Job orchestration. *(durable queue, retries, cancel, scheduler,
  batches, progress, crash recovery)*
- [ ] Workflow DAGs. *(pipelines are linear — no branches/parallel)*
- [x] Content-addressed storage. *(partial: SHA-256 checksums;
  dedup via `dedup_images`)*
- [x] Document fingerprints. *(SHA-256 + compare fingerprints)*
- [x] Structured extraction. *(tables, metadata, text, structure)*
- [x] Resource limits. *(EngineLimits, security limits, pipeline
  limits, upload caps)*
- [ ] Sandboxed external engines. *(timeouts/size caps; no isolation)*
- [x] Content validation. *(validate ops, output sanity checks,
  security upload gate)*
- [ ] Event-driven processing. *(no event bus / webhooks)*
- [x] Resumable jobs. *(reaping + ingestion recovery + scheduler
  persistence)*
- [x] Multimedia engine. *(ffmpeg-backed probe/convert/video/audio/
  frames/subtitles/analysis/encoding — beyond Stirling scope)*

## Official references

- Stirling-PDF tool catalogue: <https://docs.stirlingpdf.com/Functionality/Recommended-Tools/>
- Stirling-PDF advanced tools: <https://docs.stirlingpdf.com/Functionality/Advanced-Tools/>
- Feature flags and endpoint configuration: <https://docs.stirlingpdf.com/Server/Customisation/Features-and-Endpoints/>
- Stirling-PDF GitHub repository: <https://github.com/Stirling-Tools/Stirling-PDF>
