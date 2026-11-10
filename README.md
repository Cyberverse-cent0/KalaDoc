# KalaDoc

**A Rust-powered document management and processing platform.**

KalaDoc ingests, organizes, stores, and processes documents through a single
application with three interfaces: a command-line tool (`kaladoc`), an HTTP API
(`kaladoc serve`), and MCP tools for AI assistants (`kaladoc mcp`).

This document describes what KalaDoc can do **today** — the current release
baseline — and how to get started with it.

`APP_SECRET_KEY` must be at least 32 characters (see Configuration).

---

## What KalaDoc can do

KalaDoc ships **7 processing engines** behind one registry. Every engine is
reachable three ways: directly from the CLI, as a durable background job, and
through the HTTP API. `kaladoc engine list` and `kaladoc engine health` show
what is registered and which external tools are available.

| Engine | What it does |
|---|---|
| **PDF** | 155 operations — the full Stirling-PDF-style surface |
| **Image** | Inspect, resize, convert, rotate, compress, thumbnail, metadata, validate |
| **Multimedia** | Audio/video via managed `ffmpeg`/`ffprobe` processes |
| **Video** | ~94 subcommands — the largest media surface |
| **Conversion** | Format conversion via pandoc / LibreOffice / poppler |
| **Pipeline** | Multi-step document processing DAGs |
| **Security** | Upload validation, magic-byte sniffing, resource limits |

### PDF operations (155)

Organize & pages: `merge`, `merge_mode`, `split`, `split_mode`,
`split_at_outline`, `single_page`, `extract_pages`, `remove_pages`,
`replace_pages`, `reorder`, `rotate`, `swap_pages`, `duplicate_pages`,
`insert_blank`, `remove_blank_pages`, `interleave`, `nup`, `booklet`,
`poster`, `page_count`, `page_labels` (`get/set/clear`), `page_boxes`
(`get/set/remove`), `page_rotations`, `page_size` (`get/set`), `resize`,
`crop`/`clear_crop`, `trim_preset`, `scale_content`, `normalize_orientation`,
`normalize_rotations`, `set_view_crop`, `repair`

Content & editing: `edit_text`, `add_freetext`, `add_ink`, `list_ink`,
`remove_ink`, `draw_text`, `draw_shapes`, `text_markup`, `overlay`, `stamp`,
`watermark`, `add_page_numbers`, `headers_footers`, `bates`, `redact`,
`redact_pages`, `redact_regions`, `remove_hidden_text`, `add_comment`,
`remove_annotations`, `list_annotations`, `add_links`, `list_links`,
`remove_links`, `set_transition`, `list_transitions`, `clear_transitions`,
`set_outline`, `list_outline`, `remove_outline`, `build_outline_from_text`

Forms & fields: `list_form_fields`, `fill_form`, `create_text_field`,
`edit_field_props`, `delete_form_field`, `lock_fields`, `unlock_fields`,
`clear_form`

Security & signatures: `encrypt`, `decrypt`, `sign`, `verify_signatures`,
`signer_certificates`, `parse_certificate`, `timestamp_document`,
`timestamp_query`, `list_signature_timestamps`, `has_timestamps`,
`build_timestamp_request`, `request_external_timestamp`, `permissions_report`,
`sanitize`, `validate`, `audit_content`, `inspect_javascript`, `to_pdfa`,
`to_pdfua`, `to_pdfx`

Extraction & inspection: `extract_text`, `extract_spans`, `search_text`,
`extract_images`, `remove_images`, `dedup_images`, `replace_image`,
`extract_tables`, `extract_key_values`, `extract_metadata`, `set_metadata`,
`clear_metadata`, `get_xmp`, `set_xmp`, `get_document_dates`,
`set_document_dates`, `inspect`, `inspect_structure`, `object_report`,
`inspect_fonts`, `audit_fonts`, `list_layers`, `set_layer_visibility`,
`list_oc_marked_content`, `remove_oc_properties`, `list_articles`,
`remove_articles`, `compare`, `document_digest`

Attachments & portfolio: `list_attachments`, `add_attachment`,
`extract_attachment`, `remove_attachment`, `create_portfolio`,
`list_portfolio`

Output & optimization: `render`, `render_pages`, `ocr`, `ocr_detailed`,
`make_searchable`, `clean_scan`, `deskew`, `detect_orientation`,
`grayscale`, `invert_colors`, `compress`, `recompress_streams`,
`garbage_collect`, `linearize`, `flatten`, `convert`, `import_pages`,
`add_named_destinations`, `list_named_destinations`,
`remove_named_destination`, `remove_named_objects`, `add_print_marks`,
`set_page_box`, `remove_page_box`, `clear_page_labels`

### Image operations

`inspect`, `resize`, `convert`, `rotate`, `compress`, `thumbnail`,
`metadata`, `validate` — plus the full engine-level operation set registered
in the engine registry (format conversion across PNG/JPEG/WebP/TIFF/GIF and
CMYK handling).

### Video operations (~94 `kaladoc video` subcommands)

- **Probe/inspect:** `probe`, `thumbnail`, `inspect`
- **Transcode/encode:** `transcode`, `encode`, `remux`, `encode presets`
- **Geometry:** `resize`, `crop`, `pad`, `rotate`, `flip`, `letterbox`,
  `pillarbox`, `autocrop`, `transpose`, `orient`
- **Timing:** `trim`, `speed`, `reverse`, `loop`, `freeze`
- **Audio:** `volume`, `normalize`, `audio-merge`, `audio-concat`, `fade`,
  `remove-audio`, `channels`, `eq`, `denoise`, `compress-audio`, `pitch`,
  `spectrogram`, `mix`, `crossfade`, `delay`, `extract-audio`, `replace-audio`
- **Frames/slideshows:** `frames`, `image-sequence`, `contact-sheet`,
  `scene-frames`, `slideshow`, `to-gif`, `to-webp`, `to-apng`,
  `audio-to-video`, `gif-to-video`
- **Subtitles/streams:** `subtitles`, `subs-convert`, `subs-shift`,
  `burn-subs`, `extract-streams`, `select-streams`
- **Compositing:** `filter`, `filters`, `overlay`, `text`, `chroma-key`
- **Packaging/analysis:** HLS/DASH packaging, concat, loudness/quality
  analysis, hardware-acceleration discovery

All video operations are also available as `video.*` background jobs (50 job
kinds) and `/media/*` REST endpoints.

### Conversion

Pluggable backends (`pandoc`, `libreoffice`, `poppler`) convert between
document formats including HTML→PDF, URL→PDF, EPUB, CBZ/CBR, image→PDF,
EML→PDF, and the pandoc matrix (docx, odt, markdown, rst, ...). Backends are
detected at runtime; missing tools report `unavailable`, never crash.

### Pipelines

Define multi-step processing DAGs and run them as a unit — steps dispatch to
any engine's operations. Available via `kaladoc pipeline list|run|run-dag`,
`POST /pipelines/run`, and MCP `pipeline_run`.

### Background jobs & scheduling

- Durable SQLite-backed job queue: `jobs worker|list|enqueue|cancel|retry|recover`
- Crash recovery reaps stale `running` jobs automatically
- Recurring schedules: `kaladoc schedule list|create|delete|tick`
- Job kinds: `document.pdf_op`, `document.convert`, `image.*`, `video.*`
  (50 kinds), `media.*`, `pipeline.run`, and more (64 job handlers total)

### Storage

- Local filesystem storage with per-document versioning, folders, quotas,
  and atomic upload staging (`storage/stream`)
- Optional S3-compatible remote backend (AWS S3, MinIO, R2) — env-only config
- Documents support metadata, tags, related-document links, audit history,
  restore/purge lifecycle

### Security

- Bearer-token auth on the HTTP API (`kaladoc auth login`)
- Magic-byte format sniffing (PDF, images, audio, video) — never trusts
  extensions
- Filename sanitization, path confinement, size/page/output limits
- Passwords hashed with Argon2id; `APP_SECRET_KEY` ≥ 32 chars

### MCP (AI assistant) tools

`kaladoc mcp` exposes ~28 tools: `system_info`, `system_health`,
`engine_list`, `engine_health`, `storage_status`, `document_list`,
`document_search`, `document_get`, `document_history`, `document_versions`,
`document_artifacts`, `document_tags`, `document_create`, `document_ingest`,
`document_rename`, `document_move`, `document_delete`, `document_restore`,
`document_restore_version`, `folder_list`, `folder_create`, `job_enqueue`,
`job_list`, `job_cancel`, `pipeline_list`, `pipeline_run`.

---

## Getting started

### Prerequisites

- Rust stable toolchain + Cargo
- Git
- External tools are **detected, not bundled** — install what you need:
  - `ffmpeg` + `ffprobe` — video/multimedia engine
  - `pdftoppm` + `tesseract` — PDF rendering & OCR
  - `openssl` — digital signing
  - `pandoc` / `libreoffice` — conversion backends

### Build

```bash
git clone https://github.com/Cyberverse-cent0/KalaDoc.git
cd KalaDoc
cargo build --release
```

### Initialize

```bash
export APP_SECRET_KEY="your-application-secret"   # >= 32 chars
export APP_DB_USER="your-database-user"
export APP_DB_PASSWORD="your-database-password"

cargo run --release -- init        # creates/migrates the SQLite database
cargo run --release -- doctor      # verify config, database, storage health
```

### Configuration

Config resolves in layers: built-in defaults → `kaladoc.toml` (or
`KALADOC_CONFIG` / `--config`) → environment variables. Secrets are never
written to the config file.

```toml
[database]
path = "db.sqlite"
max_connections = 8

[storage]
root_dir = "./storage"
max_size_bytes = 10737418240   # optional quota

[api]
addr = "127.0.0.1:8080"

[jobs]
poll_interval_secs = 2
default_max_attempts = 3
backoff_base_secs = 30
stale_after_secs = 600         # crash-recovery timeout
```

Notable env vars: `DB_PATH`, `KALADOC_STORAGE_DIR`, `KALADOC_API_ADDR`,
`KALADOC_LOG_LEVEL`, `JOB_POLL_INTERVAL_SECS`, `JOB_STALE_AFTER_SECS`
(alias `JOB_RECOVERY_TIMEOUT_SECS`). Inspect the merged config with
`kaladoc config show` / `kaladoc config get <dotted.key>`.

---

## Usage

All commands support `--output json|yaml`, `--config <path>`, and `--quiet`.

### Command line

```text
kaladoc version                     # version/build info
kaladoc doctor                      # config, database, storage health
kaladoc init                        # create/migrate the database
kaladoc config show|get|validate    # inspect resolved configuration
kaladoc auth login|logout|status|whoami|session ...
kaladoc user create|list|delete
kaladoc jobs worker|list|enqueue|cancel|retry|recover
kaladoc engine list|health          # registered processing engines
kaladoc doc list|get|create|rename|move|delete|restore|purge|
             upload|download|process|folders|metadata ...
kaladoc pdf <op>                    # 155 PDF ops on local files
kaladoc image <op>                  # image ops on local files
kaladoc video <subcommand>          # ~94 media subcommands
kaladoc media op                    # generic media operation
kaladoc pipeline list|run|run-dag   # multi-step pipelines
kaladoc schedule list|create|delete|tick
kaladoc storage ...                 # storage inspection/management
kaladoc convert ...                 # format conversion
kaladoc serve [--addr host:port]    # start the HTTP API
kaladoc mcp                         # start the MCP server (stdio)
```

Examples:

```bash
kaladoc pdf merge in1.pdf in2.pdf -o merged.pdf
kaladoc pdf split document.pdf --pages 1-3,7-9 -o parts/
kaladoc pdf ocr scanned.pdf -o searchable.pdf   # needs tesseract
kaladoc pdf sign doc.pdf --cert cert.pem --key key.pem -o signed.pdf
kaladoc video transcode input.mkv -o output.mp4 --preset fast
kaladoc video thumbnails input.mp4 -o thumbs/ --interval 5
kaladoc image compress photo.png -o photo_small.png
kaladoc doc upload report.pdf && kaladoc doc process <id> pdf.merge ...
```

### HTTP API

```bash
kaladoc serve            # binds api.addr (127.0.0.1:8080 default)
```

```bash
curl -s http://127.0.0.1:8080/health        # public: aggregated engine health
curl -s http://127.0.0.1:8080/openapi.json  # public: OpenAPI 3.x spec
```

All other endpoints require `Authorization: Bearer <session-token>` — get one
via `kaladoc auth login` (the token is stored for reuse).

Route groups:

- `/documents` — CRUD, versions, tags, metadata, folders, history,
  related documents, restore/purge, `/documents/{id}/process`,
  `/documents/{id}/ops/{op}`
- `/jobs` — enqueue/list/get/cancel/retry, batch jobs
- `/engines` — registry listing and health
- `/pipelines` — list/get/run
- `/media/*` — video engine REST surface
- `/storage`, `/health`, `/openapi.json`

### MCP server

```bash
kaladoc mcp    # stdio transport for MCP-compatible AI clients
```

Exposes the ~28 tools listed above for document, job, pipeline, engine, and
storage operations.

### Remote storage (S3)

The storage layer supports S3-compatible backends (AWS S3, MinIO, R2),
configured **only via environment variables**:

| Variable | Purpose | Default |
|---|---|---|
| `KALADOC_S3_ENDPOINT` | Endpoint URL (empty = derive from region) | `https://s3.{region}.amazonaws.com` |
| `KALADOC_S3_BUCKET` | Target bucket (required) | — |
| `KALADOC_S3_REGION` | AWS region for SigV4 signing | `us-east-1` |
| `KALADOC_S3_PREFIX` | Key prefix inside the bucket | `""` |
| `KALADOC_S3_ACCESS_KEY` | Access key id (required) | — |
| `KALADOC_S3_SECRET_KEY` | Secret access key (required) | — |
| `KALADOC_S3_TIMEOUT_SECS` | Per-request timeout | `30` |
| `KALADOC_S3_MAX_ATTEMPTS` | Retry attempts | `4` |

---

## Development

```bash
cargo check --all-targets   # fast compile check incl. tests
cargo test                  # full test suite (~1,900 tests)
cargo clippy                # lints
cargo fmt --all             # format
```

The project is a single Rust crate; `src/crates/*` are modules.
`ffmpeg`/`ffprobe` must be on `PATH` for video features — they are detected at
runtime and reported honestly in `kaladoc doctor` / `engine health`.

## Project status

KalaDoc is under active development. The current baseline includes the full
platform described above — document management, durable jobs, scheduling,
seven processing engines, three interfaces (CLI / HTTP API / MCP) — and is the
foundation for the upcoming UI and tooling additions.

## Documentation

Deeper design and per-engine references live in `.doc/`:

- [Architecture](.doc/Architecture.md) — system design and module boundaries
- [PDF engine](.doc/pdf_engine.md) — PDF operation catalog and contracts
- [Video engine](.doc/video_engine.md) — media engine details
- [Job engine](.doc/job_engine.md) — queue, recovery, scheduling
- [Stirling-PDF target features](.doc/stirling_pdf_target_features.md)

## Contributing

Contributions, bug reports, and architectural feedback are welcome. See
`AGENTS.md` for branch/commit conventions before making changes.

## License

See the repository's license file for licensing terms.

---

**KalaDoc — A unified foundation for document management and processing.**
