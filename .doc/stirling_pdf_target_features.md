# Stirling-PDF Advanced Features — Target Documentation

Reference inventory of Stirling-PDF's advanced capabilities, captured for
archival by Applaicon and used as the feature-parity target for KalaDoc.

> **Legend.** `[x]` = wired end-to-end: an ops module on disk, a
> `PdfEngine` (or image/conversion/multimedia-engine) method, **and** a
> reachable surface — a `document.pdf_op` arm in
> `job_handlers.rs::dispatch_pdf_op` (`PDF_OPS`, 148 ops), a named step
> in a `pipeline-engine` `PipelineDefinition` reachable via the
> `pipeline.run` job kind / `POST /pipelines/run` (and
> `POST /documents/{id}/ops/{op}`), a registered job kind, or a CLI/API
> route. `[ ]` = not wired; annotations say how far it gets
> (`engine only` = ops module + engine method exist but there is no
> dispatch arm and no pipeline step; `pipeline step only` = reachable
> exclusively as a named pipeline step; `job-kind only` = a handler is
> registered but nothing enqueues that kind outside the generic jobs
> API; `library only` = compiled and exported but no job/pipeline/CLI
> path reaches it). Section 13 uses ✅/🔁/⚠️/❌ for the same tiers.
> This is a feature *reference*, not an implementation requirement —
> reliable document primitives come first, then higher-level engines.

## Scope for KalaDoc

- **Frontend features are out of scope for now.** Anything that exists
  only to serve a web/desktop UI — drag-and-drop page rearrangement,
  custom branding, multi-language interface, admin consoles, SSO/OIDC/LDAP
  portals — is deferred. Engine capabilities underneath them remain in
  scope where they make sense as library/API operations.
- **The backend is the absolute target.** All work targets the engine
  crates (`pdf-engine`, `conversion-engine`, `image-engine`,
  `multimedia-engine`, `pipeline-engine`, `job_engine`,
  `security-engine`, `document_engine`) and the services above them.
- **The backend API exists and covers the dispatch surface.** The axum
  REST API (`kaladoc serve`, `api.addr` config, `api/src/lib.rs` +
  `handlers.rs` + `openapi.rs`) exposes 44 route paths / 59 method
  endpoints: document upload/download, CRUD, restore, versions +
  version restore, copy/archive/unarchive/purge, tags, per-document
  settings and metadata (+ custom fields), history, permissions and
  relationships, folders (incl. PATCH/DELETE), artifact download,
  `POST /documents/{id}/process` (`document.pdf_op` — 148 dispatched
  pdf ops), the generic `POST /documents/{id}/ops/{op}` (synchronous
  `image.*` ops; `pdf.*`/bare pdf ops and `pipeline.run` enqueue
  jobs), `GET /pipelines` + `GET /pipelines/{name}` +
  `POST /pipelines/run` (`pipeline.run` — 128 `pdf.*` steps plus
  `image.*`, `convert.*`, `media.*` and `security.*` steps), job
  list/get/cancel/retry, `POST /jobs/batch` (+ get/cancel,
  `job_engine::enqueue_batch`), recurring `/schedules` CRUD +
  `/schedules/tick`, engines, storage, health and
  `GET /openapi.json`. Auth is Bearer session tokens **or** static API
  keys (`api.api_keys` / `KALADOC_API_KEYS`, `name:key` pairs; callers
  act as `apikey:<name>`), and route groups can be disabled via
  `api.disabled_groups` / `KALADOC_API_DISABLE`. Per-operation REST
  routes for every op are still not built one-to-one; ops are reached
  through the dispatch paths above. See section 10 and
  `job_handlers::PDF_OPS` / `pipeline-engine::operation` (section 13)
  for the current op lists.
- **MCP is still unimplemented.** `src/crates/application/cli_mcp_server/`
  contains only the `cli/` adapter (`kaladoc` binary: `version`,
  `doctor`, `init`, `config`, `auth`, `user`, `jobs`, `engine`, `doc`,
  `pdf`, `image`, `pipeline`, `serve`); no MCP server exists yet.
- **Pipelines are still linear — and DAGs are unreachable.**
  `pipeline-engine` now has `dag.rs` (`DagDefinition`/`DagNode`/
  `DagEdge`/`run_dag`, re-exported from `lib.rs`), but nothing calls
  it: `pipeline.run` payloads and `POST /pipelines/run` only accept a
  `PipelineDefinition` (or a built-in name), and there is no API/CLI/
  job path that accepts a DAG. `PipelineDefinition.steps` remains an
  ordered `Vec<PipelineStep>` with no branches, parallel steps, or
  dependencies.

## 1. Advanced PDF editing

- [x] Edit existing PDF text. *(pipeline step `pdf.edit_text` →
  `ops/edit_text.rs` rewrites text-showing operators; best-effort
  text-layer edit, not a content-aware reflow)*
- [x] Edit or replace images inside PDFs. *(dispatched
  `replace_image` — `ops/replace_image.rs` swaps an image XObject's
  stream data via a `document.pdf_op` arm; no pipeline step)*
- [x] Add text at arbitrary page coordinates. *(pipeline step
  `pdf.draw_text` → `ops/draw_text.rs`, x/y in points, bottom-left
  origin)*
- [x] Add images, logos and watermarks. *(dispatched `stamp` — text or
  PNG image — and `watermark`; pipeline `pdf.stamp`/`pdf.watermark`;
  image-engine `watermark` for raster images)*
- [x] Draw shapes and freehand annotations. *(pipeline steps
  `pdf.draw_shapes` (rectangle/line/ellipse/arrow via `ops/draw_shapes.rs`)
  and `pdf.add_ink`/`pdf.remove_ink`/`pdf.list_ink` freehand strokes via
  `ops/freehand.rs`)*
- [x] Highlight, underline and strike through text. *(pipeline step
  `pdf.text_markup` → `ops/text_markup.rs` highlight/underline/
  strikeout markup annotations)*
- [x] Add comments and sticky notes. *(pipeline steps `pdf.add_comment`
  (sticky-note icons) and `pdf.add_freetext` (freetext callouts) via
  `ops/annotations.rs`; `list_annotations`/`remove_annotations` also
  pipeline steps)*
- [x] Edit document metadata. *(pipeline steps `pdf.set_metadata`,
  `pdf.clear_metadata`, `pdf.set_xmp`, `pdf.get_xmp` via
  `ops/set_metadata.rs`/`set_xmp_metadata.rs`; dispatched
  `set_document_dates`/`get_document_dates` via `ops/doc_time.rs`)*
- [x] Edit bookmarks and table of contents. *(pipeline steps
  `pdf.set_outline`/`pdf.remove_outline`/`pdf.list_outline` via
  `ops/outline.rs`; dispatched `build_outline_from_text` generates an
  outline from heading text via `ops/outline_from_headings.rs`)*
- [x] Edit interactive PDF forms. *(pipeline steps `pdf.list_form_fields`,
  `pdf.create_text_field`, `pdf.delete_form_field`,
  `pdf.edit_field_props` — the last also dispatched via
  `ops/form_fields.rs`/`ops/edit_field_props.rs`)*
- [x] Fill existing forms. *(pipeline steps `pdf.fill_form` (name→value
  map) and `pdf.clear_form` via `ops/fill_form.rs`)*
- [x] Detect and create form fields automatically. *(partial:
  `pdf.list_form_fields` enumerates AcroForm fields and
  `pdf.create_text_field` adds text fields as pipeline steps;
  `extract_metadata` reports field names/paths — but there are no
  detection heuristics that infer fields from content)*
- [x] Flatten interactive forms. *(dispatched `flatten` — best-effort,
  merges annotation/appearance content into page streams)*
- [x] Unlock read-only form fields. *(pipeline steps `pdf.unlock_fields`/
  `pdf.lock_fields` flag patching via `ops/unlock_fields.rs`)*
- [x] Remove annotations. *(pipeline steps `remove_annotations`,
  `remove_articles`, `remove_ink` with `AnnotFilter` via
  `ops/annotations.rs`/`article_threads.rs`/`freehand.rs`; `flatten`
  merges rather than removes)*
- [x] Remove selected images. *(pipeline step `pdf.remove_images`
  per-page sweep returning a `RemoveReport` via `ops/remove_images.rs`)*
- [x] Compare two PDF documents. *(dispatched `compare` — text,
  structure and byte modes with diff report; needs a second artifact
  via `params.other_artifact_id`/`other_document_id`; also pipeline
  step `pdf.compare` with base64 `other_inputs`)*
- [x] Find differences between documents. *(same `compare` op)*

## 2. Advanced page manipulation

- [x] Merge multiple PDFs. *(pipeline step `pdf.merge` via
  `ops/merge.rs` — other documents supplied as base64 `other_inputs`
  params; note: outlines, signatures and document-level structures
  are not carried over)*
- [x] Split PDFs by page ranges. *(dispatched `extract_pages` — 1-based
  range spec `"1-3,8"`; also pipeline `pdf.extract_pages`)*
- [x] Split PDFs into individual pages. *(dispatched `split` with
  `pages_per_file = 1`; pipeline `pdf.split`)*
- [x] Split by page count. *(dispatched `split`; pipeline
  `pdf.split_mode` `count`/`range`/`every_nth` modes via
  `ops/split_modes.rs`)*
- [x] Split by bookmarks. *(pipeline steps `pdf.split_at_outline`
  (splits on outline levels) and `pdf.split_mode` via
  `ops/split_bookmarks.rs`/`ops/split_modes.rs`)*
- [x] Extract selected pages. *(dispatched `extract_pages`, 1-based
  range spec `"1-3,8"`)*
- [x] Delete selected pages. *(dispatched `remove_pages`; pipeline
  `pdf.remove_pages`)*
- [x] Remove blank pages automatically. *(pipeline step
  `pdf.remove_blank_pages` → `ops/remove_blank_pages.rs` —
  conservative content-stream heuristic, keeps
  annotated/undecodable pages, fails rather than emit a page-less PDF;
  returns a `BlankReport`)*
- [x] Detect page orientation and auto-rotate. *(dispatched
  `detect_orientation` (per-page geometry report) and
  `normalize_orientation` (`/Rotate` portrait/landscape/as-is modes)
  via `ops/auto_orient.rs` — geometry-only detection, no text-direction
  CV)*
- [x] Rotate individual pages. *(dispatched `rotate` — range spec +
  90/180/270 via `degrees`; pipeline `pdf.rotate`,
  `pdf.set_page_rotation`, `pdf.get_page_rotations`,
  `pdf.normalize_rotations` via `ops/rotate.rs`/`ops/rotate_view.rs`)*
- [x] Rearrange pages. *(dispatched `reorder` — full permutation spec;
  programmatic, not drag-and-drop UI)*
- [x] Duplicate pages. *(pipeline step `pdf.duplicate_pages` with
  position spec via `ops/duplicate_pages.rs`)*
- [x] Insert blank pages. *(pipeline step `pdf.insert_blank` at
  position spec with size via `ops/insert_blank.rs`)*
- [x] Crop pages manually. *(pipeline step `pdf.crop` sets `/CropBox`
  via `ops/crop.rs`; view-crop variant `pdf.set_view_crop`/
  `pdf.clear_crop` via `ops/crop_view.rs`)*
- [x] Resize pages. *(pipeline step `pdf.resize` — page boxes only,
  content is not scaled; `pdf.set_page_size` (dispatched
  `set_page_size`/`get_page_size`) also resizes boxes with optional
  `scale_content`)*)
- [x] Scale page content. *(pipeline step `pdf.scale_content` —
  transforms page content streams via `ops/scale_content.rs`)*
- [x] Add page numbers. *(pipeline step `pdf.add_page_numbers` with
  `PageNumberStyle` via `ops/headers_footers.rs`)*
- [x] Add headers and footers. *(pipeline step `pdf.headers_footers`
  text placement via `ops/headers_footers.rs`)*
- [x] Multi-page layouts (N-up). *(pipeline step `pdf.nup` via
  `ops/nup.rs`)*
- [x] Booklet imposition. *(pipeline step `pdf.booklet` with
  `BookletSpec` size/mode via `ops/booklet.rs`)*
- [x] Poster printing / tile splitting. *(pipeline step `pdf.poster`
  via `ops/poster.rs`)*
- [x] Convert PDF into a single large page. *(pipeline step
  `pdf.single_page` via `ops/single_page.rs`)*
- [x] Overlay multiple PDFs. *(pipeline step `pdf.overlay` — form
  XObjects, optional page selection, other input as base64 param via
  `ops/overlay.rs`)*
- [x] Interleave pages from multiple documents. *(pipeline steps
  `pdf.interleave` and `pdf.merge_mode` (`MergeMode` sequential/
  interleave/reverse) via `ops/interleave.rs`/`ops/merge_modes.rs`)*
- [x] Crop-box vs. view-crop and page-size control. *(pipeline steps
  `pdf.set_view_crop`/`pdf.clear_crop`, `pdf.set_page_size`/
  `pdf.get_page_sizes` (also dispatched `set_page_size`/`get_page_size`),
  `pdf.set_page_labels`/`pdf.clear_page_labels`/`pdf.get_page_labels`
  via `ops/page_labels.rs`, and dispatched page-box/trim-box ops
  `get_page_boxes`/`set_page_box`/`remove_page_box`/
  `apply_trim_preset`/`get_trim_boxes` via `ops/page_boxes.rs`/
  `ops/trim_boxes.rs`)*

## 3. Document conversion engine

Supported formats today: all 13 `Format` variants (`model.rs`) now have
at least one backend path — pandoc reads `txt`, `markdown`, `html`,
`docx`, `odt`, `epub` and writes `txt`/`markdown`/`html`/`docx`/`odt`/
`epub` (+`pdf` when a pdf-engine is installed); LibreOffice converts
`docx`, `odt`, `xlsx`, `xls`, `pptx`, `ppt`, `png`, `jpeg` → `pdf` and
`docx`↔`odt`; the pure-Rust fallback covers `txt`/`markdown`/`pdf` →
`txt`/`markdown` and `png`/`jpeg` → `pdf`. Backends are probed on
`PATH` with automatic fallback (`ConversionEngine::new`). The engine
is registered in `engine-registry` and reachable as the pipeline step
`convert.convert` (`input_format`/`output_format` params) — but not as
a `document.pdf_op` arm (the documented `convert.run` job kind has no
registered handler).

### To PDF

- [x] DOCX → PDF *(pandoc / LibreOffice)*
- [x] XLS / XLSX → PDF *(LibreOffice `--convert-to pdf`)*
- [x] PPT / PPTX → PDF *(LibreOffice)*
- [x] ODT → PDF *(LibreOffice / pandoc)*
- [x] HTML → PDF *(pandoc)*
- [x] Markdown → PDF *(pandoc + Rust backend)*
- [x] TXT → PDF *(pandoc + Rust backend)*
- [x] Images → PDF *(PNG/JPEG → PDF via pure-Rust backend and
  LibreOffice; multi-page/other formats not covered)*
- [x] EPUB → PDF *(pandoc reads EPUB; `→ pdf` requires a pdf-engine
  such as wkhtmltopdf/weasyprint on `PATH` — `pdf_engine` probe)*
- [ ] Email (EML) → PDF
- [ ] URL → PDF
- [ ] HTML files → PDF *(partial: pandoc converts HTML bytes/string →
  PDF; no file/URL fetching layer)*
- [ ] CBZ / CBR → PDF

### From PDF

- [ ] PDF → Word
- [ ] PDF → Excel
- [ ] PDF → PowerPoint
- [ ] PDF → CSV
- [ ] PDF → HTML
- [x] PDF → Markdown *(dispatched pdf-engine `convert` with
  `target=markdown` + Rust conversion backend — best-effort
  text-layer conversion)*
- [ ] PDF → XML
- [x] PDF → JSON-oriented extraction workflows. *(partial:
  dispatched `extract_metadata`/`inspect` return structured metadata;
  no general JSON export)*
- [x] PDF → Images. *(dispatched `render` — shells out to `pdftoppm`
  (poppler-utils, external tool); PNG or JPEG, page-range selectable;
  `MissingTool` error when poppler is absent)*
- [x] PDF → Plain text. *(dispatched `convert`/`extract_text` + Rust
  `pdf → txt` backend)*
- [ ] PDF → EPUB
- [x] PDF → PDF/A. *(partial: pipeline step `pdf.pdfa` →
  `ops/pdfa.rs::to_pdfa` writes an honest `pdfaid:` XMP declaration —
  deliberately **no** ICC/OutputIntent, fonts or conformance claim;
  `PdfaReport::warnings` lists validator gaps)*
- [x] PDF → PDF/UA. *(partial: pipeline step `pdf.pdfua` →
  `ops/pdfua.rs::to_pdfua`, same honest-declaration approach)*
- [x] PDF → PDF/X. *(partial: pipeline step `pdf.pdfx` →
  `ops/pdfx.rs::to_pdfx`, same honest-declaration approach)*
- [ ] PDF → RTF
- [ ] PDF → CBZ / CBR
- [x] PDF → Single-page images. *(same dispatched `render`; any page
  range, not just page 1)*

> Actual format availability depends on the installed conversion engines
> (pandoc / LibreOffice / poppler probes). LibreOffice deliberately does
> not advertise PDF *import* (weak fidelity); pandoc cannot read PDF —
> so all `PDF → <office format>` conversions remain unimplemented.

## 4. OCR and intelligent document processing

- [x] OCR scanned documents. *(dispatched `ocr` returns per-page text
  via `pdftoppm` + `tesseract` — **two external tools**; does **not**
  produce a searchable PDF by itself; also pipeline step `pdf.ocr`)*
- [x] Make scanned PDFs searchable. *(dispatched `make_searchable` —
  `ops/searchable_pdf.rs` injects an invisible OCR text layer:
  rasterizes via `pdftoppm`, tesseract TSV word boxes → positioned
  `3 Tr` text; **honest degraded mode**: pages where TSV parsing fails
  fall back to one unpositioned text block, reported via
  `SearchableReport::positioned=false` + warnings. Reachable via
  `document.pdf_op` / `POST /documents/{id}/process`; deliberately
  **not** a pipeline step — the modified bytes cannot be chained)*
- [x] Extract text from images. *(partial: tesseract path via PDF
  raster; image-engine has no direct OCR op)*
- [x] Support multiple OCR languages. *(pipeline step
  `pdf.ocr_languages` lists installed tesseract codes;
  per-request language selection is not exposed)*
- [x] Automatically rotate scanned pages. *(dispatched
  `normalize_orientation` — portrait/landscape/as-is `/Rotate` modes)*
- [x] Deskew scanned documents. *(dispatched `deskew` — projection-
  profile deskew; honest raster-rebuild path, per-page report)*
- [x] Remove scanning noise. *(dispatched `clean_scan` `Despeckle`
  mode — grayscale + 3×3 median despeckle raster rebuild; no ML
  denoise)*
- [x] Clean scanned backgrounds. *(dispatched `clean_scan` —
  grayscale/threshold/despeckle raster rebuild; no ML background
  removal)*
- [x] Detect text orientation. *(dispatched `detect_orientation` —
  geometry heuristics only, no text-direction/OCR detection)*
- [x] Extract images from PDFs. *(dispatched `extract_images`)*
- [ ] Extract photographs from scanned pages.
- [ ] Detect and split multiple photographs.
- [x] Automatically rename PDFs based on document content. *(partial:
  `document.auto_rename` job kind is registered — renames to
  `{title}-{id8}.{ext}` from extracted PDF metadata, once — but its
  enqueue helper has no caller; reachable only by enqueueing that kind
  through the generic jobs API / `kaladoc jobs enqueue`)*
- [x] Detect document structure. *(dispatched `inspect`,
  `extract_metadata`, `validate`, `inspect_structure` — the last is
  the PDF/UA structure-tree report via `ops/structure_tree.rs`, now
  engine-wrapped and dispatched)*
- [x] Extract document information. *(dispatched `extract_metadata` —
  Info dict, dates, keywords, XMP)*
- [x] Automatic form detection. *(partial: pipeline steps
  `pdf.list_form_fields`/`pdf.create_text_field` cover AcroForm
  enumeration and text-field creation; `extract_metadata` reports
  field names/paths; still no detection heuristics that infer fields
  from page content)*
- [x] Document classification and processing workflows. *(partial:
  linear pipeline definitions (128 `pdf.*` steps + `image.*`/
  `convert.*`/`media.*`/`security.*`) + ingestion/
  metadata/index jobs; no classifier)*

## 5. Advanced security and digital signatures

- [x] Password-protect PDFs. *(dispatched `encrypt` — AES-256,
  user/owner passwords; also pipeline `pdf.encrypt`)*
- [x] Remove PDF passwords. *(dispatched `decrypt`; pipeline
  `pdf.decrypt`)*
- [x] Change PDF permissions. *(dispatched `encrypt` `PdfPermissions`
  flags — also accepted nested under `params.permissions`; pipeline
  `pdf.encrypt` `allow_*` flags)*
- [x] Restrict printing. *(same `allow_print` flag)*
- [x] Restrict copying. *(same `allow_copy` flag)*
- [x] Restrict editing. *(same `allow_modify`/`allow_annotations`
  flags)*
- [x] Apply digital certificate signatures. *(pipeline step `pdf.sign`
  → `ops/sign.rs` produces a real detached CMS/PKCS#7 signature via the
  `openssl` CLI (`adbe.pkcs7.detached` + `/ByteRange`);
  **limitations**: whole-document signatures only, full rewrite
  invalidates prior signatures (input with existing signatures is
  refused — call `remove_signatures` first), no incremental updates,
  requires `openssl` on `PATH`)*
- [x] Sign using PEM certificates. *(same `pdf.sign` step —
  `SignCredentials` accepts PEM cert + key)*
- [x] Sign using PKCS#12/P12 certificates. *(same `pdf.sign` step —
  `SignCredentials::Pkcs12` runs `openssl pkcs12 -nodes` to extract
  to PEM in a tempdir)*
- [x] Validate digital signatures. *(dispatched `verify_signatures` —
  **structural** report: presence and byte-range sanity, not
  cryptographic verification; pipeline `pdf.verify_signatures_crypto`
  does real `openssl cms -verify` digest + chain checking when trust
  anchors are supplied — timestamps detected but never verified,
  revocation never checked)*
- [x] Verify certificate chains. *(pipeline step
  `pdf.verify_signatures_crypto` + `kaladoc pdf` CLI op chain-verify
  against caller-supplied PEM trust anchors; no `document.pdf_op`
  arm, revocation and timestamp verification still absent)*
- [x] Remove certificate signatures. *(pipeline step
  `pdf.sign`'s sibling `remove_signatures` → `ops/sign.rs` strips
  signature fields/widgets/`/Perms`)*
- [x] Add timestamp signatures. *(dispatched `timestamp_document` +
  `build_timestamp_request`/`request_external_timestamp` —
  `ops/timestamp.rs` performs RFC 3161 digest→TSQ→TSA→token against a
  caller-supplied `https://` TSA URL — **no offline fallback, no token
  is ever fabricated**; embedding a token into a signature dictionary
  is still not implemented)*
- [x] RFC 3161 timestamping. *(dispatched `timestamp_*` family:
  `document_digest`, `timestamp_query`, `build_timestamp_request`,
  `request_external_timestamp`, `list_signature_timestamps`,
  `has_timestamps`; DER TSQ/TSR path needs no external tools, the TSA
  call is the only network dependency; `pdf.document_digest` is also
  a pipeline step for the offline digest half)*
- [x] Permanently redact sensitive content. *(dispatched
  `redact_regions`/`redact_pages` — true region/page redaction that
  removes overlapping content + annots and paints black fills)*
- [x] Redact selected text. *(dispatched `redact` — removes matching
  text strings from content streams; **semantics**: best-effort text-
  terms only, `params.terms` required; underlying bytes are not proven
  gone)*
- [x] Redact selected regions. *(dispatched `redact_regions` —
  rectangle list + optional label, overlap report)*
- [x] Redact entire pages. *(dispatched `redact_pages` — blank or
  replacement-text page wipe)*
- [x] Sanitize PDF documents. *(dispatched `sanitize`; pipeline
  `pdf.sanitize`)*
- [x] Remove embedded JavaScript. *(dispatched `sanitize` + pipeline
  `pdf.remove_javascript_names`/`pdf.inspect_javascript` via
  `ops/remove_objects.rs`/`ops/inspect_javascript.rs`)*
- [x] Remove potentially dangerous embedded files. *(dispatched
  `sanitize` + pipeline `pdf.remove_embedded_files` via
  `ops/remove_objects.rs`)*
- [x] Flatten forms and annotations. *(dispatched `flatten`)*
- [x] Validate PDF compliance. *(dispatched `validate` →
  `ValidationReport`; pipeline `pdf.pdfa`/`pdf.pdfua`/`pdf.pdfx`
  emit honest compliance *declarations*, not validated files)*

> **Note:** True redaction is now covered — `redact_regions` and
> `redact_pages` remove underlying content (ops + annots) and paint
> black fills, and both are dispatched. `redact` remains the
> best-effort text-terms variant.

## 6. Compression and optimization

- [x] Compress PDF files. *(dispatched `compress` recompresses object/
  xref streams via lopdf; pipeline `pdf.compress`; no image-level
  optimization)*
- [x] Optimize embedded images. *(partial: dispatched
  `replace_image` swaps image XObject stream data and pipeline step
  `pdf.remove_images` strips image XObjects per page; no
  image-level recompression of embedded PDF images)*
- [x] Reduce image resolution. *(image-engine `resize`/`thumbnail`
  cover raster inputs, not embedded PDF images)*
- [x] Adjust JPEG quality. *(image-engine `compress` — quality 1–100
  for JPEG; PNG uses compression effort; WebP/GIF/BMP use codec
  defaults)*
- [x] Remove unnecessary objects. *(dispatched
  `remove_named_objects` + pipeline steps `pdf.garbage_collect`
  (`ops/garbage_collect.rs` → `GcReport`) and
  `pdf.remove_embedded_files`/`pdf.remove_javascript_names` via
  `ops/remove_objects.rs`)*
- [x] Remove unused resources. *(same GC/remove-objects pipeline
  steps)*
- [x] Optimize PDF structure. *(pipeline step `pdf.garbage_collect`)*
- [x] Reduce redundant content. *(dispatched `dedup_images` —
  content-hash image dedup; pipeline `pdf.recompress_streams` Flate
  recompression with real counts via `ops/dedup_images.rs`/
  `ops/stream_recompress.rs`)*
- [x] Linearize PDFs for faster web viewing. *(partial: pipeline step
  `pdf.linearize` → `ops/linearize.rs` is wired, but an **honest
  no-op**: `lopdf` 0.47 accepts `SaveOptions::linearize` yet never
  writes linearization data, so `LinearizeReport::linearized` is
  always `false` today; it reports that plainly rather than claiming
  a Fast Web View file)*
- [x] Convert images to more efficient formats. *(image-engine
  `convert`/`compress` cover png/jpeg/gif/webp/bmp for standalone
  images)*
- [x] Optimize documents for printing. *(dispatched
  `apply_trim_preset` / `get_trim_boxes` / `get_page_boxes` /
  `set_page_box` / `remove_page_box` trim-box presets; `add_print_marks`
  crop/registration marks is also dispatched — `document.pdf_op` arm,
  no pipeline step)*
- [x] Optimize documents for archival storage. *(partial: pipeline
  step `pdf.pdfa` declaration subset; no validated archival profile)*

## 7. Advanced visual processing

*(image-engine ops apply to raster images — PNG/JPEG/GIF/WebP/BMP —
not directly to PDF page content. `ops/` is a private module but every
op is re-exported at the crate root and most also have `ImageEngine`
methods: `inspect`, `resize`, `resize_to`, `convert`, `rotate`
(90/180/270), `crop`, `flip`/`flip_mirror`, `adjust`
(brightness/contrast/hue), `color` (saturation ±100%, per-channel RGB
gain/offset, high-contrast), `levels`/`photocopy`/`replace_color` (in
`ops/effects.rs`), `cmyk` (RGB→CMYK TIFF), `thumbnail`, `compress`,
`watermark` (image overlay), `validate`, `metadata`. Seventeen of
these are also `image.*` pipeline steps reachable via `pipeline.run` /
`POST /documents/{id}/ops/image.<op>` — the latter runs them
**synchronously** against a document artifact. The scan-effects free
functions (`deskew`, `clean_background`, `denoise`, `scanner_preset`
in `ops/scan_fx.rs`) and `ocr_image`/`ocr_languages` (`ops/ocr.rs`)
are likewise re-exported at the crate root — **library-only**: no
`ImageEngine` method wraps them, pipeline-engine deliberately does not
expose `image.ocr`/scan ops, and no job kind reaches them. pdf-engine
additionally has `grayscale` and `invert_colors` for page content,
both reachable as `document.pdf_op` arms and pipeline steps.)*

- [x] Adjust brightness. *(image-engine `adjust` — ±100% via
  `imageops::brighten`)*
- [x] Adjust contrast. *(image-engine `adjust`/`color`)*
- [x] Adjust saturation. *(image-engine `color` — `saturation` ±100%
  via HSV rescale)*
- [x] Adjust RGB channels independently. *(image-engine `color` —
  per-channel gain/offset)*
- [x] Invert document colors. *(pipeline step `pdf.invert_colors` →
  `ops/invert_colors.rs` on page content + image-engine equivalent
  via `adjust`)*
- [x] Replace foreground and background colors. *(pipeline step
  `image.replace_color` + `ImageEngine::replace_color` — near-color
  matching in `ops/effects.rs`; reachable via `pipeline.run` /
  `POST /documents/{id}/ops/image.replace_color`)*
- [x] Create high-contrast documents. *(pipeline steps `image.color`
  high-contrast + `image.levels` black/white point; `ImageEngine`
  methods `color`/`levels`)*
- [x] Convert colors to CMYK. *(pipeline step `image.cmyk` +
  `ImageEngine::cmyk` — naive RGB→CMYK written as uncompressed CMYK
  TIFF; **alpha is flattened over white (lossy)**; output contract
  differs from other ops)*
- [x] Apply scanner effects. *(library only: `scanner_preset` +
  `deskew`/`clean_background`/`denoise` free functions in
  `ops/scan_fx.rs`, re-exported at the crate root — deterministic
  grain/background simulation; no `ImageEngine` method, no pipeline
  step, no job kind)*
- [x] Simulate photocopy effects. *(pipeline step `image.photocopy` +
  `ImageEngine::photocopy` — deterministic grain simulation)*
- [x] Apply grayscale. *(pipeline step `pdf.grayscale` →
  `ops/grayscale.rs` on page content + image-engine via `color`
  saturation=-100)*
- [x] Repair damaged PDFs. *(dispatched `repair` — normalized load +
  repair report; pipeline `pdf.repair`)*
- [x] Adjust hue. *(image-engine `adjust` — `hue_degrees`)*
- [x] Overlay documents with foreground/background control. *(partial:
  pipeline step `pdf.overlay` composites PDFs as form XObjects;
  no fg/bg channel control)*

## 8. Attachments and embedded document management

- [x] Add attachments to PDFs. *(dispatched `add_attachment` via
  `ops/attachments.rs`)*
- [x] Extract embedded attachments. *(dispatched `extract_attachment` —
  emits the file bytes as a generic artifact)*
- [x] List attachments. *(dispatched `list_attachments`)*
- [x] Remove attachments. *(dispatched `remove_attachment` per-name;
  `remove_embedded_files` pipeline step strips the whole tree)*
- [x] Extract individual attachments. *(dispatched `extract_attachment`
  takes a `name` param)*
- [ ] Process multiple attachments in batches.
- [x] Create PDF portfolios. *(pipeline steps `pdf.create_portfolio` +
  `pdf.list_portfolio` via `ops/portfolio.rs` — multi-input, other
  documents as base64 `other_inputs`)*
- [x] Manage embedded files. *(dispatched `list_attachments` /
  `extract_attachment` / `add_attachment` / `remove_attachment`; plus
  `sanitize`/pipeline `remove_embedded_files` for stripping;
  `package_documents` (`ops/package_files.rs`) packs files into a
  portfolio PDF but remains **engine-only** — no dispatch arm, no
  pipeline step)*
- [x] Inspect document metadata. *(dispatched `inspect` /
  `extract_metadata`)*
- [x] Inspect PDF fonts. *(dispatched `inspect_fonts` (per-font
  inventory incl. embedding) + `audit_fonts` (risk report))*
- [x] Inspect page dimensions. *(dispatched `get_page_size`/
  `get_page_sizes` + `get_page_boxes`/`get_trim_boxes`; also via
  `inspect`/`extract_metadata`)*
- [x] Inspect permissions and document properties. *(dispatched
  `inspect` reports encryption state + permission flags;
  `permissions_report` gives the full `PermissionsReport`)*
- [x] Inspect embedded JavaScript. *(dispatched `inspect_javascript`
  action inventory + `audit_content` JS/launch/XFA risk scan)*

## 9. Automation and workflow processing

- [x] Build reusable processing workflows. *(pipeline-engine
  `PipelineDefinition` — serde-serializable, ordered steps; 128 `pdf.*`
  operations plus `image.*`/`convert.*`/`media.*`/`security.*` steps
  (`security.validate`/`validate_upload`)*
- [x] Chain multiple PDF operations. *(linear step sequences over
  `pdf`/`image`/`convert`/`media`/`security` engines, bytes or reports
  passed step to step)*
- [x] Save automation pipelines. *(partial: definitions are data and
  serializable — inline JSON via `POST /pipelines/run` or built-ins
  `pdf.intake`/`pdf.extract-text`; still no persisted user pipeline
  store)*
- [x] Run workflows repeatedly. *(via job engine / CLI `kaladoc
  pipeline` / `POST /pipelines/run` → `pipeline.run` jobs)*
- [x] Process multiple documents. *(durable job queue, batch enqueue)*
- [x] Batch-process files. *(`POST /jobs/batch` +
  `job_engine::enqueue_batch` group up to `MAX_BATCH_JOBS` jobs under
  one batch id with aggregate progress and batch cancel)*
- [x] Automate document conversion. *(conversion engine is reachable
  as pipeline step `convert.convert` (`input_format`/`output_format`
  params) and — on paper — via the documented `convert.run` job kind,
  but no `ConvertRunHandler` is registered, so the job kind is dead;
  `media.op`/`run_media_op` is likewise dead code)*
- [x] Automate OCR. *(`pdf.ocr`/`ocr_detailed`/`ocr_languages` are
  dispatched `document.pdf_op` ops and named pipeline steps)*
- [x] Automate compression. *(dispatched `compress` op + pipeline
  `pdf.compress`)*
- [x] Automate document renaming. *(partial: `document.auto_rename`
  job kind is registered — `{title}-{id8}.{ext}` from extracted PDF
  metadata, once — but its enqueue helper has no caller; reachable
  only via the generic jobs enqueue API)*
- [x] Process files through APIs. *(REST API: raw-body upload with
  magic-byte sniffing + `x-file-name`, artifact download,
  `POST /documents/{id}/process` (148 pdf ops),
  `POST /documents/{id}/ops/{op}`, `POST /pipelines/run`,
  and `POST /jobs/batch`)*
- [x] Automate folder scanning. *(partial: `storage.scan_inbox` job
  kind is registered — ingests files dropped in
  `<storage_root>/inbox/` and moves them to `inbox/.processed/` — but
  nothing enqueues it automatically; reachable only via the generic
  jobs enqueue API)*
- [ ] Watch folders for new documents. *(no continuous watcher daemon;
  `storage.scan_inbox` is a one-shot scan per job)*
- [x] Automatically process incoming files. *(partial: ingest can be
  enqueued as `document.ingest` job and batches via `/jobs/batch`; no
  filesystem watcher)*
- [x] Route processed documents into output folders. *(partial: folder
  model + `move_document`; no automated routing rules)*
- [x] Configure pipeline processing stages. *(per-step `params`)*

## 10. Developer and integration features

- [x] REST API. *(axum server in `src/crates/api` via `kaladoc serve` —
  44 route paths / 59 method endpoints: document create/list/get/
  patch/delete, restore, versions + version restore, copy/archive/
  unarchive/purge, tags, per-document settings + metadata (+ custom
  fields), permissions + relationships, history, artifact download,
  `/documents/{id}/process` job enqueue, generic
  `/documents/{id}/ops/{op}`, `/pipelines` list/get + `/pipelines/run`,
  folders (+PATCH/DELETE), jobs list/get/cancel/retry, `/jobs/batch`
  (+get/cancel), `/schedules` CRUD + `/schedules/tick`, engines,
  storage, health; Bearer session **or** static API-key auth on all but
  `/health` and `/openapi.json`; disabled route groups answer 404.
  `kaladoc serve` runs **only** the HTTP API — there is no embedded
  job worker; jobs are driven by the separate `kaladoc jobs worker`
  CLI process)*
- [x] Command-line interface. *(not a Stirling feature, but the primary
  current surface: `version`, `doctor`, `init` (auto `.env` secrets),
  `config`, `auth`, `user`, `jobs` (worker/list/enqueue/cancel/retry/
  recover), `engine list|health`, `doc` CRUD + folders, `pdf <op>`
  local-file PDF operations, `image`, `pipeline`, `serve`; stable exit
  codes + `--output json`)*
- [x] API documentation. *(crate-docs endpoint table +
  `GET /openapi.json` generated spec in `api/src/openapi.rs`)*
- [x] OpenAPI integration. *(partial: `/openapi.json` serves a spec
  covering the axum routes; pdf ops reach the API through
  `POST /documents/{id}/process` `op`+`params` and
  `POST /pipelines/run`, not per-op endpoints)*
- [x] API authentication. *(Bearer session tokens validated against
  `sessions_tbl` on every protected route **or** static API keys from
  `api.api_keys` / `KALADOC_API_KEYS` — `name:key` pairs; API-key
  callers act as `apikey:<name>` and follow the same document-level
  authorization rules)*
- [x] API endpoint configuration. *(partial: `api.addr` bind address
  via `kaladoc.toml`/`KALADOC_API_ADDR`; per-group kill-switches via
  `api.disabled_groups` / `KALADOC_API_DISABLE` — comma list of
  `documents`, `folders`, `jobs`, `pipelines`, `engines`, `storage`,
  `schedules`; disabled groups answer `404` before auth runs — but no
  per-endpoint granularity)*
- [x] Batch processing through API. *(`POST /jobs/batch` enqueues up
  to `MAX_BATCH_JOBS` jobs as one batch — shared `batch_id`,
  `GET /jobs/batch/{id}` aggregate progress, batch cancel)*
- [x] Automated folder scanning. *(partial: `storage.scan_inbox` job
  kind registered — one-shot scan of `<storage_root>/inbox/` per job;
  no scheduler entry or watcher enqueues it automatically)*
- [ ] MCP integration for AI agents. *(adapter directory
  `cli_mcp_server` exists but contains only the `cli/` adapter — no
  MCP server implementation found)*
- [x] Workflow automation APIs. *(`POST /pipelines/run` accepts either
  a built-in pipeline name or a full `PipelineDefinition` JSON and
  enqueues a `pipeline.run` job)*
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
- [x] Role-based access control. *(partial→document-level: every
  `/documents/{id}/…` route enforces `services::access::
  check_document_access` — owner implies `Manage`, otherwise an active
  `document_permissions_tbl` grant must meet the required level
  (`read` < `write` < `manage`); `GET/POST/DELETE
  /documents/{id}/permissions` manages grants. **No admin role or
  wildcard**; folder mutations require folder ownership; the
  `permissions` store's group/role subject kinds exist in the schema
  but enforcement is per-subject only)*
- [ ] Single Sign-On (SSO).
- [ ] OAuth/OIDC integrations.
- [ ] LDAP integrations.
- [x] Audit logging. *(document history events + audit store)*
- [x] API key management. *(partial: config-managed only —
  `api.api_keys` / `KALADOC_API_KEYS` `name:key` pairs; no API to
  create/revoke keys at runtime; callers act as `apikey:<name>`)*
- [x] Feature-level permissions. *(partial: per-document permission
  grants enforced on every document route; no global feature flags
  beyond `disabled_groups`)*
- [x] Endpoint enable/disable controls. *(partial: route-group
  kill-switches `api.disabled_groups` / `KALADOC_API_DISABLE` —
  `documents`, `folders`, `jobs`, `pipelines`, `engines`, `storage`,
  `schedules`; 404 before auth; no per-endpoint toggles)*
- [x] User-specific settings. *(partial: per-owner scoping)*
- [x] Server configuration. *(env + config file)*
- [ ] Custom branding.
- [ ] Multi-language interface.
- [x] Docker deployment. *(partial: `Dockerfile` +
  `docker-compose.yml` at repo root; `src/deployments` packaging for
  Arch/deb/fedora/tar/zip; manifests provided, not deployed)*
- [x] Kubernetes deployment. *(partial: `deploy/kubernetes/` —
  deployment, worker, configmap, PVC, secret example, kustomization;
  manifests provided, not deployed)*
- [x] Desktop deployment. *(partial: Windows/Linux packaging targets)*
- [x] Air-gapped deployment. *(partial: fully local stack — SQLite +
  filesystem/S3 storage, no external service calls; external
  conversion tools are optional probes, not requirements)*
- [x] On-premises hosting. *(local SQLite + filesystem/S3 storage)*

## 12. AI and intelligent features

- [ ] AI assistant.
- [x] Document ingestion. *(streaming ingestion pipeline: staged
  write, sniffing, version/artifact registration, recovery)*
- [x] Document parsing. *(partial: text/metadata extraction, page
  structure)*
- [ ] Document classification.
- [x] AI-assisted document workflows. *(partial: pipeline engine +
  job orchestration; no model integration)*
- [ ] MCP-based AI tool invocation.
- [x] Automated document processing. *(job engine: durable queue,
  retries with backoff, cancel, scheduler; `/jobs/batch`)*
- [x] Structured document extraction. *(partial: metadata extract,
  text extract, image extract dispatched; `extract_tables`/
  `extract_key_values` (`ops/extract_tables.rs`, engine methods
  `PdfEngine::extract_tables`/`extract_key_values`) exist but have **no
  dispatch arm, pipeline step, or CLI surface** — engine-only)*

## 13. pdf-engine surface — implementation audit

Re-audit of `src/crates/pdf-engine` (109 `.rs` files in `ops/` —
~105 op modules plus `mod.rs`, `load.rs`/`pages.rs`/`raster.rs`
infrastructure and the `extract_tables` module described below).
Four tiers:

- ✅ **dispatched** — ops file + `PdfEngine` method + a
  `dispatch_pdf_op` arm (`PDF_OPS`, 148 ops) reachable via
  `document.pdf_op` jobs / `POST /documents/{id}/process` (and
  `POST /documents/{id}/ops/{op}`).
- 🔁 **pipeline step** — ops file + `PdfEngine` method + a named
  `pipeline-engine` operation (`pipeline-engine/src/model.rs::
  operation`, 128 `pdf.*` steps) reachable via `pipeline.run` jobs /
  `POST /pipelines/run`, but no `document.pdf_op` arm.
- ⚠️ **backend only** — ops file + `PdfEngine` method, no dispatch
  arm and no pipeline step.
- ❌ **not wired** — ops file exists but has no `PdfEngine` wrapper
  (❌† = additionally not declared in `ops/mod.rs`, i.e. a dead
  file). **None remain** — every module is declared and wrapped.

`ops/load.rs`, `ops/raster.rs` (`pdftoppm` rasterization shared by
`render`/`ocr`/`searchable_pdf`) and `ops/pages.rs`/`ops/mod.rs` are
infrastructure, not operations.

### 13.1 `ops/*.rs` modules (109 files)

| Status | Modules |
|---|---|
| ✅ dispatched — every engine op has a `PDF_OPS` arm | `annotations` (add_comment, add_freetext, list/remove_annotations), `article_threads` (list/remove_articles), `attachments` (add/extract/list/remove_attachment), `auto_orient` (detect_orientation, normalize_orientation), `bates`, `booklet`, `cert_info` (`extract_signer_certs` is 🔁; `parse_certificate` is ⚠️ — mixed), `clean_scan`, `compare`, `compress`, `content_audit` (audit_content), `convert`, `crop`, `crop_view`, `dedup_images`, `decrypt`, `deskew`, `doc_time` (get/set_document_dates), `draw_shapes`, `draw_text`, `duplicate_pages`, `edit_field_props`, `edit_text`, `encrypt`, `extract` (extract_pages), `extract_images`, `extract_metadata`, `fill_form` (fill_form, clear_form), `flatten`, `font_audit` (audit_fonts), `form_fields` (list/create/delete_form_field), `freehand` (add/list/remove_ink), `garbage_collect`, `grayscale`, `headers_footers` (headers_footers, add_page_numbers), `info` (inspect), `insert_blank`, `inspect_fonts`, `inspect_javascript`, `interleave`, `invert_colors`, `layers` (list_oc_marked_content), `linearize`, `links` (add/list/remove_links), `merge`, `merge_modes` (merge_mode), `named_destinations` (add/list/remove), `nup`, `object_report`, `ocr` (`ocr`, `ocr_detailed`; `ocr_languages` is 🔁), `optional_content` (list_layers, set_layer_visibility, remove_oc_properties), `outline` (set/remove/list_outline), `outline_from_headings` (build_outline_from_text), `overlay`, `page_boxes` (get/set/remove_page_box), `page_count`, `page_labels` (set/clear/get), `page_size` (get/set_page_size; `get_page_sizes` is 🔁), `page_transitions` (set/clear/list_transitions), `pdfa`/`pdfua`/`pdfx` (`to_pdfa`/`to_pdfua`/`to_pdfx` dispatched; the `pdfa`/`pdfua`/`pdfx` pipeline aliases are 🔁), `permissions_report`, `portfolio` (create/list_portfolio), `poster`, `print_marks` (add_print_marks), `redact`, `redact_pages`, `redact_regions`, `remove_blank_pages`, `remove_hidden_text`, `remove_images`, `remove_objects` (`remove_named_objects` dispatched; `remove_embedded_files`/`remove_javascript_names` are 🔁), `remove_pages`, `render` (render, render_pages), `reorder` (reorder, swap_pages), `repair`, `replace_image`, `resize`, `rotate`, `rotate_view` (set/get/normalize page rotations), `sanitize`, `scale_content`, `searchable_pdf` (make_searchable), `set_metadata` (set/clear_metadata), `set_xmp_metadata` (set/get_xmp), `sign` (`sign` dispatched; `remove_signatures` is 🔁), `sign_timestamp` (list_signature_timestamps, has_timestamps), `single_page`, `split`, `split_bookmarks` (split_at_outline), `split_modes` (split_mode), `stamp`, `stream_recompress` (recompress_streams), `structure_tree` (inspect_structure), `text` (extract_text), `text_markup`, `timestamp` (timestamp_document, timestamp_query, build_timestamp_request, request_external_timestamp; `document_digest` is also 🔁), `trim_boxes` (apply_trim_preset, get_trim_boxes), `unlock_fields` (unlock/lock_fields), `validate`, `verify_signature` (`verify_signatures` dispatched; `verify_signatures_crypto` is 🔁), `watermark` |
| 🔁 pipeline step only — engine method, no `document.pdf_op` arm | residual ops inside otherwise-dispatched modules: `cert_info::extract_signer_certs` (`pdf.extract_signer_certs`), `ocr::ocr_languages` (`pdf.ocr_languages`), `page_size::get_page_sizes` (`pdf.get_page_sizes`), `pdfa`/`pdfua`/`pdfx` pipeline aliases, `remove_objects::remove_embedded_files`/`remove_javascript_names`, `sign::remove_signatures`, `timestamp::document_digest` (`pdf.document_digest`), `verify_signature::verify_signatures_crypto` (`pdf.verify_signatures_crypto` — also exposed via `kaladoc pdf` CLI). Ten pipeline `pdf.*` operations have no `PDF_OPS` counterpart |
| ⚠️ backend only — engine method, no dispatch, no pipeline step | `package_files` (`package_documents`), `extract_tables` (`extract_tables`, `extract_key_values` — table/grid and key/value heuristic extraction; **no dispatch arm, pipeline step, or CLI surface**), `cert_info::parse_certificate` (input is a raw DER blob, not the page byte stream — deliberately unexposed per `pipeline-engine/src/model.rs`) |

(`raster` is shared `pub(crate)` infrastructure used by dispatched
`render`/`ocr`/`searchable_pdf`, not an op; `load` is the shared
loader and `pages` the shared range parser — `PdfEngine::
parse_page_range` exposes the latter directly.)

### 13.2 `PdfEngine` public methods (`engine.rs`)

~155 `pub fn`s (plus `new`/`with_limits`/`limits`/`Default` and the
`ProcessingEngine` impl — capabilities list 37 `EngineOperation`s;
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
  `remove_javascript_names`, `package_documents`, `add_bates`,
  `deskew`, `clean_scan`, `dedup_images`, `recompress_streams`,
  `remove_hidden_text`, `build_outline_from_text`, `object_report`,
  `audit_content`, `audit_fonts`, `permissions_report`.
- **Marks:** `watermark`, `stamp`, `draw_text`, `headers_footers`,
  `add_page_numbers`, `add_print_marks`.
- **Annotations/articles/ink:** `add_comment`, `add_freetext`,
  `list_annotations`, `remove_annotations`, `list_articles`,
  `remove_articles`, `add_ink`, `list_ink`, `remove_ink`.
- **Links/destinations/layers/transitions:** `add_links`,
  `list_links`, `remove_links`, `add_named_destinations`,
  `list_named_destinations`, `remove_named_destination`, `list_layers`,
  `set_layer_visibility`, `remove_oc_properties`,
  `list_oc_marked_content`, `set_transition`, `clear_transitions`,
  `list_transitions`.
- **Text & forms:** `text_markup`, `edit_text`, `list_form_fields`,
  `create_text_field`, `delete_form_field`, `fill_form`, `clear_form`,
  `unlock_fields`, `lock_fields`, `edit_field_props`.
- **Metadata/structure/dates:** `set_page_labels`, `clear_page_labels`,
  `get_page_labels`, `set_outline`, `remove_outline`, `list_outline`,
  `build_outline_from_text`, `set_page_rotation`, `get_page_rotations`,
  `normalize_rotations`, `detect_orientation`, `normalize_orientation`,
  `set_view_crop`, `clear_crop`, `set_metadata`, `clear_metadata`,
  `set_xmp`, `get_xmp`, `set_document_dates`, `get_document_dates`,
  `get_page_size`, `get_page_sizes`, `set_page_size`, `get_page_boxes`,
  `set_page_box`, `remove_page_box`, `apply_trim_preset`,
  `get_trim_boxes`, `get_trim_boxes_for_pages`, `inspect_structure`,
  `inspect_fonts`, `inspect_javascript`, `to_pdfa`, `to_pdfua`, `to_pdfx`.
- **Security/signatures/timestamps:** `redact`, `redact_regions`,
  `redact_pages`, `encrypt`, `decrypt`, `verify_signatures`,
  `verify_signatures_crypto`, `sign`, `signer_certificates`,
  `parse_certificate`, `remove_signatures`, `list_signature_timestamps`,
  `has_timestamps`, `document_digest`, `timestamp_query`,
  `build_timestamp_request`, `request_external_timestamp`,
  `timestamp_document`.
- **Inspection/extraction/conversion:** `inspect`, `extract_metadata`,
  `validate`, `extract_images`, `extract_text`, `extract_tables`,
  `extract_key_values`, `convert`, `render`, `render_pages`, `ocr`,
  `ocr_detailed`, `ocr_languages`, `compare`.
- **Portfolio/attachments:** `list_portfolio`, `create_portfolio`,
  `list_attachments`, `extract_attachment`, `add_attachment`,
  `remove_attachment`.

### 13.3 Dispatch surface

`PDF_OPS` / `dispatch_pdf_op` (`src/crates/application/
job_handlers.rs`, const at ~line 309, dispatch arms ~3344–4972)
expose **148 ops** — the ✅ set in 13.1 — reachable via
`document.pdf_op` jobs, `POST /documents/{id}/process`, and the
generic `POST /documents/{id}/ops/{op}` route. The residual 🔁 ops in
13.1 are reachable as named steps in `pipeline-engine::operation`
(`pdf.*`, 128 ops incl. multi-input ops via base64 `other_inputs`)
through `pipeline.run` jobs and `POST /pipelines/run`. Only three
engine capabilities have **no** job/pipeline dispatch path at all:
`package_documents` (`ops/package_files.rs`), `parse_certificate`
(`ops/cert_info.rs` — DER-blob input), and `extract_tables`/
`extract_key_values` (`ops/extract_tables.rs`). `verify_signatures_crypto`
has no `PDF_OPS` arm but is a `pdf.verify_signatures_crypto` pipeline
step **and** a `kaladoc pdf` CLI op. Separately, `pipeline-engine`
also dispatches `image.*` (17 ops), `convert.convert`,
`media.probe`/`streams`/`chapters`, and `security.validate`/
`validate_upload` steps; the `media.op`/`convert.run` job kinds
documented in `job_handlers.rs`'s header table have **no registered
handler** (`run_media_op` in `services/media_ops.rs` has no callers)
— dead surface. One pipeline-engine quirk: `pdf.list_portfolio` has a
dispatch arm and a `PDF_LIST_PORTFOLIO` const but is missing from the
`PDF_OPERATIONS` validation list, so `check_definition` may reject it
up-front. Adding a
`document.pdf_op` arm means: a `PDF_OPS` entry, a `match` arm in
`dispatch_pdf_op`, and (for byte outputs) the
`pdf_outcome`/`OpFile` plumbing already used by `render`/`split`.

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
  cancel, scheduler ticks, crash recovery, `/jobs/batch`)*
- [ ] Workflow DAGs. *(partial: `pipeline-engine/src/dag.rs` exists —
  `DagDefinition`/`DagNode`/`DagEdge`/`run_dag`, re-exported from
  `lib.rs` — but is **unreachable**: `pipeline.run` and
  `POST /pipelines/run` only accept `PipelineDefinition`; no API/CLI/
  job path accepts a DAG)*
- [ ] Content-addressed storage. *(partial: SHA-256 checksums stored
  per artifact; no dedup on ingest)*
- [x] Document fingerprints. *(partial: SHA-256 + compare-mode
  structure fingerprints; `pdf.document_digest` pipeline step)*
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
