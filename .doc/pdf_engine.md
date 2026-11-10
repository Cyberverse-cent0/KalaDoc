# PDF Engine

`src/crates/pdf-engine` implements `PdfEngine`, a
[`ProcessingEngine`](../src/crates/core/engine.rs) of kind `Pdf`. It is a
stateless, `Send + Sync` engine: all operations are synchronous functions
over in-memory byte buffers (`&[u8] -> PdfResult<Vec<u8>>` or structured
reports). Async callers (job handlers, API) must wrap calls in
`tokio::task::spawn_blocking`.

Backends:

- **`lopdf` 0.47** — parsing, object model, encryption, most
  transformations. Pure Rust, in-process.
- **`pdftoppm`** (Poppler) — rasterization for `render`, `ocr`,
  `searchable_pdf`, `clean_scan`, `deskew`-adjacent flows.
- **`tesseract`** — OCR text recognition (`ocr`, `ocr_languages`).
- **`openssl`** — CMS sign/verify (`sign`, `verify_signatures_crypto`,
  RFC 3161 `timestamp_document` via a TSA).

Missing tools never produce fake output: affected ops return
`PdfError::MissingTool` / `PdfError::ExternalTool`, and
`PdfEngine::health()` reports `Degraded` naming the absent binaries.

## Contracts

- **Page numbers are 1-based** everywhere (CLI, MCP, engine, reports).
- **Page ranges** share one grammar parsed by
  `ops::pages::parse_page_range` / `PdfEngine::parse_page_range`:
  `"1-3,8,10-12"`, `"all"`. Specs are fully validated (bounds included)
  before document work begins; malformed specs return
  `PdfError::InvalidPageRange`.
- **`EngineLimits`** are enforced centrally in `ops::load::load_pdf`
  (`max_input_bytes`, `max_pages`) plus per-op output/pixel budgets
  (`max_outputs`, `max_pixels`). `merge` enforces `max_pages` on the
  *combined* count.
- **Encryption policy**: transforming ops reject encrypted input with
  `PdfError::Encrypted` (use `decrypt` first). `inspect`, `validate`,
  `page_count`, and metadata readers tolerate encrypted files and
  report the state honestly.
- **No panics on hostile input**: `lopdf` load is wrapped in
  `catch_unwind`; malformed PDFs return typed `PdfError`.
- **Errors are sanitized**: external-tool failures report the tool name
  and a bounded detail string, never document-derived stderr dumps.

## Capability surface

`PdfEngine::capabilities()` advertises the full implemented op set via
`EngineOperation` (~150 variants). `sign` is advertised — it performs a
real CMS signature through `openssl` (see Security). The op modules live
in `src/crates/pdf-engine/src/ops/`; each has doc comments describing
parameters, limits, and limitations.

### Page structure & imposition

| Op | Module | Notes |
|---|---|---|
| `merge` | `merge.rs` | Concatenates inputs in order; outlines/signatures/collections are not carried over |
| `split` | `split.rs`, `split_modes.rs`, `split_bookmarks.rs` | By count, range spec, stride (`EveryNth`), or outline level |
| `extract_pages` / `remove_pages` | `extract.rs`, `remove_pages.rs` | Range-spec page selection; removal rejects emptying the doc |
| `reorder` | `reorder.rs` | Strict full permutation `"3,1,2"`; duplicates/out-of-range rejected |
| `rotate` / `set_page_rotation` / `get_page_rotations` / `normalize_rotations` | `rotate.rs`, `rotate_view.rs` | `/Rotate` delta vs. absolute set; normalize folds >360 values |
| `crop` / `set_view_crop` / `clear_crop` | `crop.rs`, `crop_view.rs` | Absolute `/CropBox` semantics |
| `resize` | `resize.rs` | `ResizeSpec::Scale` or `Size{width_pt,height_pt}`; resizes boxes, **content is not scaled** |
| `insert_blank`, `duplicate_pages`, `remove_blank_pages` | `insert_blank.rs`, `duplicate_pages.rs`, `remove_blank_pages.rs` | Page insertion/duplication; blank-page detection |
| `interleave`, `merge_mode`, `nup`, `single_page`, `poster`, `booklet` | `interleave.rs`, `merge_modes.rs`, `nup.rs`, `single_page.rs`, `poster.rs`, `booklet.rs` | Imposition and multi-input merge layouts |
| `page_boxes`, `trim_boxes`, `page_size`, `crop_view` | `page_boxes.rs`, `trim_boxes.rs`, `page_size.rs`, `crop_view.rs` | MediaBox/CropBox/TrimBox get/set, trim presets, physical size units |
| `scale_content` | `scale_content.rs` | Content-stream scaling (distinct from `resize`) |
| `page_labels` | `page_labels.rs` | `/PageLabels` number trees |
| `page_import` | `page_import.rs` | Import pages from another PDF |
| `portfolio` | `portfolio.rs` | `/Collection` navigational portfolios (`create_portfolio`, `list_portfolio`) — not file-attachment portfolios |

### Document health & rewrite

| Op | Module | Notes |
|---|---|---|
| `compress` | `compress.rs` | Object-stream/xref-stream recompression via `lopdf`; **no image recompression is claimed** |
| `repair` | `repair.rs` | Normalized rewrite + `RepairReport` (xref recovery, dangling refs, stream issues). It is a rewrite, not byte-level surgery |
| `linearize` | `linearize.rs` | Fast-web-view linearization + `LinearizeReport` |
| `garbage_collect` | `garbage_collect.rs` | `GcReport` orphan/unused-object sweep |
| `stream_recompress` | `stream_recompress.rs` | Per-stream filter recompression |
| `dedup_images` | `dedup_images.rs` | Shared image XObject deduplication |
| `object_report` | `object_report.rs` | Structural object census |
| `content_audit` | `content_audit.rs` | Content-stream inventory |
| `remove_objects` | `remove_objects.rs` | Named-object / embedded-file / JS-name removal |

### Marks, drawing & content ops

| Op | Module | Notes |
|---|---|---|
| `watermark` | `watermark.rs` | Text watermark: position, font size, opacity, rotation, RGB, page range; resource names collision-safe |
| `stamp` | `stamp.rs` | Text **or** PNG image stamp (`image` crate decode, pixel limits enforced) |
| `overlay` | `overlay.rs` | Paint another PDF's pages as Form XObjects |
| `draw_text` | `draw_text.rs` | Arbitrary-position text (x,y in points) |
| `draw_shapes` | `draw_shapes.rs` | Lines, rectangles, ellipses, arrows, polylines |
| `headers_footers`, `add_page_numbers`, `add_print_marks`, `bates` | `headers_footers.rs`, `print_marks.rs`, `bates.rs` | Running headers/footers, page numbering styles, print marks, Bates numbering |
| `grayscale`, `invert_colors` | `grayscale.rs`, `invert_colors.rs` | Color-space/content rewrites with `ColorReport` |
| `remove_images`, `replace_image` | `remove_images.rs`, `replace_image.rs` | Image XObject removal/replacement |
| `redact_regions`, `redact_pages`, `remove_hidden_text` | `redact_regions.rs`, `redact.rs` family | Region/page redaction action and hidden-text removal |
| `text_markup`, `edit_text` | `text_markup.rs`, `edit_text.rs` | Highlight/underline markup; in-place text edits with `EditReport` |
| `searchable_pdf` | `searchable_pdf.rs` | TSV word-position → invisible text layer (OCR output pipeline) |
| `clean_scan`, `deskew`, `auto_orient` | `clean_scan.rs`, `deskew.rs`, `auto_orient.rs` | Scan cleanup via rasterization |

### Annotations, articles, freehand

`annotations.rs` (`add_comment`, `add_freetext`, `list_annotations`,
`remove_annotations`), `freehand.rs` (`add_ink`/`list_ink`/`remove_ink`
— Ink annotations), `article_threads.rs` (bead/article threads),
`structure_tree.rs` (`inspect_structure` — tagged-PDF structure).

### Forms

`form_fields.rs` (`list_form_fields`, `create_text_field`,
`delete_form_field`), `fill_form.rs` (`fill_form`, `clear_form`),
`unlock_fields.rs` (`unlock_fields`, `lock_fields` — `/Ff` ReadOnly
flag), `edit_field_props.rs`.

### Security

| Op | Module | Notes |
|---|---|---|
| `encrypt` | `encrypt.rs` | AES-256 (V=5/R=6), user+owner passwords, `PdfPermissions` flags; rejects already-encrypted input |
| `decrypt` | `decrypt.rs` | Password-based decryption via `lopdf` |
| `permissions_report` | `permissions_report.rs` | Effective permission inspection |
| `sign` | `sign.rs` | Real detached-CMS signature via `openssl cms -sign` (`SignCredentials::Pem`/`Pkcs12`, `SignOptions`). Whole-document signature; rewriting invalidates prior signatures; **no RFC 3161 timestamping** (requesting one errors) |
| `verify_signature` / `verify_signatures_crypto` | `verify_signature.rs` | Structural report always; crypto mode verifies `/ByteRange` digests + optional CA chain via openssl. Timestamp tokens detected, never verified; revocation never checked |
| `remove_signatures` | `sign.rs` | Strips signature fields/widgets/`/Perms` refs |
| `sign_timestamp`, `timestamp` | `sign_timestamp.rs`, `timestamp.rs` | Signature timestamp inventory; RFC 3161 request build + external TSA call (`request_external_timestamp`) + embed (`timestamp_document`) |
| `cert_info` | `cert_info.rs` | Certificate inspection helpers |
| `redact` | `redact.rs` | **Best-effort** text removal for supported text operators — does not cover every text source or metadata |
| `sanitize` | `sanitize.rs` | Removes OpenAction/JavaScript/Launch/embedded-file/rich-media vectors; benign navigation preserved where possible |
| `inspect_javascript` | `inspect_javascript.rs` | JS name-tree inventory |

### Extraction, inspection & conversion

| Op | Module | Notes |
|---|---|---|
| `inspect` / `info` | `info.rs` | `PdfInfo`: pages, Info-dict strings, encrypted flag, size |
| `page_count` | `page_count.rs` | Structural count; tolerates encrypted input |
| `validate` | `validate.rs` | `ValidationReport` structural checks (report, not fail-fast) |
| `extract_metadata` | `extract_metadata.rs` | Info dict, `PdfDate`s, keywords, `XmpMetadata`, page size |
| `doc_time` | `doc_time.rs` | Creation/modification date get/set |
| `extract_text` / `text_spans` | `text.rs`, `text_spans.rs` | Per-page text; span-level extraction with font/encoding tables |
| `extract_images` | `extract_images.rs` | Embedded image XObjects: dims, color space, bpc, filter, **raw still-filtered stream bytes** |
| `extract_tables` | `extract_tables.rs` | Table detection from text geometry |
| `inspect_fonts` / `font_audit` | `inspect_fonts.rs`, `font_audit.rs` | Font inventory/audit |
| `convert` | `convert.rs` | Text layer → plain text or Markdown (`ConvertTarget`); no layout/OCR/table claim |
| `render` | `render.rs` | First selected page → PNG/JPEG via `pdftoppm` (`RenderFormat`) |
| `ocr` / `ocr_languages` | `ocr.rs` | Rasterize pages + `tesseract` → per-page `PageText`; default DPI/lang |
| `compare` | `compare.rs` | `CompareMode::{Text,Structure,Bytes}` → `CompareReport` with `DiffEntry`s |
| `links`, `attachments`, `named_destinations`, `outline`, `outline_from_headings` | `links.rs`, `attachments.rs`, `named_destinations.rs`, `outline.rs`, `outline_from_headings.rs` | Link/attachment/name-tree/outline inspection and editing |
| `layers`, `optional_content` | `layers.rs`, `optional_content.rs` | OCProperties / layer visibility |
| `page_transitions`, `package_files`, `article_threads` | `page_transitions.rs`, `package_files.rs` | Transitions, file packaging |
| `set_metadata`, `set_xmp_metadata`, `doc_time` | `set_metadata.rs`, `set_xmp_metadata.rs`, `doc_time.rs` | Info/XMP/date mutation |
| `to_pdfa`, `to_pdfua`, `to_pdfx` | `pdfa.rs`, `pdfua.rs`, `pdfx.rs` | Conformance tagging + reports (structural intent, not certified conformance) |
| `make_searchable` | `searchable_pdf.rs` | See above |

## CLI surface

`kaladoc pdf <op>` (`cli/command/pdf.rs`, args in `cli/args.rs`) covers
the spec surface as local-file operations: `merge`, `split`,
`rotate`, `crop`, `resize`, `reorder`, `remove-pages`, `extract-pages`,
`compress`, `repair`, `flatten`, `sanitize`, `overlay`, `watermark`,
`stamp`, `redact`, `encrypt`, `decrypt`, `sign`, `remove-signatures`,
`verify-signature`, `info` (alias `inspect`), `page-count`, `validate`,
`extract-metadata`, `extract-images`, `extract-text`, `compare`,
`render`, `convert`, `ocr`, `ocr-languages`.

Conventions: `--pages` is the shared 1-based range grammar;
`--output-file`/`--output-dir` receive byte outputs (atomic
write-then-rename); report ops emit JSON (table mode falls back to
pretty JSON); `--password -` prompts via `rpassword`. Passwords are
never logged.

The durable-job surface is `document.pdf_op`
(`application/job_handlers.rs`, `PDF_OPS` whitelist) plus `pdf.inspect`;
payloads reference stored artifacts, not local paths.

## External tool requirements

| Tool | Used by | Absent behavior |
|---|---|---|
| `pdftoppm` | `render`, `ocr`, `searchable_pdf`, `clean_scan`, `deskew` | `MissingTool` |
| `tesseract` | `ocr`, `ocr_languages` | `MissingTool` |
| `openssl` | `sign`, `verify_signatures_crypto`, `timestamp_document` | `MissingTool`/`ExternalTool` |

`PdfEngine::health()` probes these and reports `Degraded` with the
missing tool names; `kaladoc engine health` and `doctor` surface it.

## Honest limitations

- **`redact`** removes only text it can reach through supported
  content-stream operators; text in form XObjects, annotations,
  metadata, or encoded forms may survive. It is not a certified
  data-destruction guarantee.
- **`flatten`** flattens supported annotation appearances only.
- **`sanitize`** strips known active-content vectors; it is not a
  full script sandbox.
- **`compress`** recompresses object streams/xref — it does not
  recompress image data.
- **`sign`** produces a whole-document CMS signature via openssl; it
  invalidates existing signatures and does not RFC 3161-timestamp.
- **`verify_signatures_crypto`** checks digest integrity and optional
  chain trust; it does not check revocation or timestamp validity.
- **`to_pdfa`/`to_pdfua`/`to_pdfx`** add conformance structure — they
  are not certification.
- **`render`** rasterizes only the first selected page.
- **`extract_images`** reports page-level image XObjects with raw
  filtered bytes; it does not promise every nested form's images.
- **`convert`** converts the extractable text layer only — no layout,
  OCR, or table reconstruction.
- **`merge`** does not carry outlines, named destinations, or
  signatures into the merged catalog.
- Many advanced ops (portfolio, PDF/A, timestamps, forms) implement
  real structures but remain **best-effort** where the PDF spec allows
  producer/consumer variance; each op's rustdoc states its exact
  coverage.
