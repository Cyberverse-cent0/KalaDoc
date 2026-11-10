# Implementation Report — KalaDoc Document Engine (current state)

Date: 2025-11-10 (rewrite pass covering the post-milestone engine wave:
commit `d2493e3` "document engine completion + engine surfaces wave",
`43fbe57` multimedia batch-2 wiring, plus uncommitted fixes).

Scope: single crate `KalaDoc` (`src/crates/*` are modules, not workspace
members). Covers storage streaming + CAS dedup, document-engine
persistence, application services, job handlers, the pdf / image /
conversion / multimedia / pipeline engines, the axum HTTP API with
session + API-key auth, the CLI (`doc`/`pdf`/`image`/`pipeline`/`media`/
`convert`/`schedule`/`storage`/`mcp`/`serve --worker`), the MCP server,
and Docker/Kubernetes deployment artifacts.

## Verification status

| Gate | Result |
|---|---|
| `cargo check --all-targets` | clean, 0 errors (re-verified this pass, ~2m23s cold) |
| `cargo fmt --check` / `cargo clippy` | last verified clean in the original milestone pass; engine-wave code has since landed additional commits — re-run before release |
| `cargo test` | **~1,800 `#[test]`/`#[tokio::test]` attributes** across `src/` + `tests/` (see Test inventory); last full green run reported 466 lib tests at the original milestone — the suite has grown ~4× since |

## Engine surfaces

Five processing engines live under `src/crates/`. Reachability summary:

| Engine | Crate | Ops exposed | Reachable via |
|---|---|---|---|
| PDF | `pdf-engine` | **151 ops** in `PDF_OPS` (`application/job_handlers.rs`); ~127 also exposed as pipeline `pdf.*` step ops | `document.pdf_op` job kind (`enqueue_pdf_op`); API `POST /documents/{id}/ops/{op}` (`pdf.<op>` or bare op name) and `POST /documents/{id}/process`; CLI `kaladoc doc` pdf-op enqueue + `kaladoc pdf`; pipeline `pdf.*` steps |
| Image | `image-engine` | 19 op modules (`ops/`); **11 synchronous ops** via API `image.*` (`inspect`, `metadata`, `validate`, `convert`, `resize`, `rotate`, `flip`, `crop`, `thumbnail`, `compress`, `watermark`); **17 ops** as pipeline `image.*` steps (adds `adjust`, `color`, `levels`, `photocopy`, `replace_color`, `cmyk`) | API `image.<op>` synchronous ops (byte outputs stored as artifacts under `documents/{id}/outputs/`); pipeline `image.*` steps; CLI `kaladoc image`; `scan_fx`/`ocr` exported from `image-engine` (`ops/scan_fx.rs`, `ops/ocr.rs` — ocr delegates to external `tesseract`) |
| Conversion | `conversion-engine` | **17 formats** (`Format::ALL`): pdf, txt, markdown, html, png, jpeg, docx, odt, xlsx, xls, pptx, ppt, epub, eml, url, cbz, cbr | `convert.run` job kind (`ConvertRunHandler`); CLI `kaladoc convert --from --to`; pipeline `convert.convert` step; backends: in-process `rust` backend + external `pandoc`/`libreoffice`/`poppler` backends with availability probing |
| Multimedia | `multimedia-engine` | **79 ops** in `MEDIA_OPS` (`application/services/media_ops.rs`): probe/analysis/audio/video/frames/subtitles/packaging/metadata/encoding/filters (`media.op` dispatch) | `media.op` job kind (`MediaOpHandler`, shares `services::media_ops::run_media_op` with the CLI); CLI `kaladoc media op [--list-ops]`; enqueued via `kaladoc jobs enqueue`, `POST /jobs/batch`, MCP `kaladoc.job_enqueue`, or schedules. Pipeline `media.*` steps are **probe-report only** (`probe`/`streams`/`chapters`) |
| Pipeline | `pipeline-engine` | `PipelineDefinition` (linear step list) + `DagDefinition` (fan-out DAG); step engines: `pdf`, `security`, `image`, `convert`, `media` | `pipeline.run` job kind (`PipelineRunHandler`); API `GET /pipelines`, `GET /pipelines/{name}`, `POST /pipelines/run`, `pipeline.run` via `POST /documents/{id}/ops/pipeline.run`; CLI `kaladoc pipeline`; MCP `kaladoc.pipeline_list`/`pipeline_run`. **2 built-in pipelines**: `pdf.intake`, `pdf.extract-text` (`pipeline-engine/src/builtin.rs`) |

## Job kinds

All kinds registered by `register_builtin`
(`src/crates/application/job_handlers.rs:550`) — 14 total; storage-
dependent handlers log a warning and skip registration when storage
config is unavailable:

| Kind | Handler | Notes |
|---|---|---|
| `document.ingest` | `DocumentIngestHandler` | Streaming staged upload → artifact + version 1 + metadata + `Imported` audit |
| `system.ping` | `SystemPingHandler` | Health check / smoke test |
| `pdf.inspect` | `PdfInspectHandler` | Legacy single-purpose inspect job |
| `storage.gc_staging` | `StorageGcStagingHandler` | Sweeps abandoned `.staging/` uploads |
| `document.metadata_extract` | `DocumentMetadataExtractHandler` | Populates `document_metadata_tbl` |
| `document.index` | `DocumentIndexHandler` | Extracted text → metadata `custom_fields` (no FTS table yet) |
| `thumbnail.generate` | `ThumbnailGenerateHandler` | Extracts embedded PDF images only (no rasterization) |
| `document.pdf_op` | `PdfOpHandler` | Dispatches all 151 `PDF_OPS`; per-doc progress in metadata custom fields |
| `pipeline.run` | `PipelineRunHandler` | Runs `PipelineDefinition`/`DagDefinition` against a document artifact |
| `storage.scan_inbox` | `StorageScanInboxHandler` | Inbox-folder watch ingest |
| `document.auto_rename` | `DocumentAutoRenameHandler` | Document-store only; no storage config needed |
| `media.op` | `MediaOpHandler` | 79 ffmpeg-backed ops; results as `media_ops.{op}.result` metadata or `*-result.json` artifacts |
| `convert.run` | `ConvertRunHandler` | Format conversion via `ConversionEngine` backend fallback chain |
| `document.ingest_recover` | `IngestRecoverHandler` | Crash-recovery sweep for interrupted ingests |

Additionally, `scheduler.tick` (`SCHEDULER_TICK_KIND`) is registered by
`kaladoc jobs worker` (`cli/command/jobs.rs:70-77`) together with
`SchedulerTickHandler`, which fires due schedules and re-enqueues
itself — so `kaladoc serve --worker` gets scheduling for free.

## API surface

`src/crates/api/` — axum router (`api/src/lib.rs:131`). Public:
`GET /health`, `GET /openapi.json`. Everything else behind
`auth::require_auth` (Bearer) + `flags::require_enabled` (feature-flag
groups 404 before auth).

Route groups (all authenticated):

- **Engines/storage**: `GET /engines`, `GET /engines/{name}/operations`,
  `GET /storage`.
- **Documents**: CRUD (`POST/GET /documents`, `GET/PATCH/DELETE
  /documents/{id}`), `restore`, `copy`, `archive`/`unarchive`, `purge`,
  `versions` (list/create/restore), `permissions` (+ subject-scoped
  list/revoke), `relationships` (+ `related`), `tags`, `history`,
  `artifacts/{aid}/download`, `process`, `ops/{op}` (image.* sync /
  pdf.*+bare pdf ops → `document.pdf_op` job / `pipeline.run`),
  `settings`, `metadata` (+ per-field put/delete).
- **Pipelines**: `GET /pipelines`, `GET /pipelines/{name}`,
  `POST /pipelines/run`.
- **Folders**: list/create, patch/delete by id.
- **Jobs**: list/get/cancel/retry, `POST /jobs/batch` (+ get/cancel).
- **Schedules**: `GET/POST /schedules`, `DELETE /schedules/{name}`,
  `POST /schedules/tick` (manual tick).

## AuthN/Z model

- **Authentication** (`api/src/auth.rs`): `Authorization: Bearer
  <token>` resolved two ways — (1) static API keys from
  `[[api.api_keys]]` / `KALADOC_API_KEYS` (`name:key` pairs,
  constant-time compare, actor `apikey:<name>`, no session); (2)
  session tokens validated against `sessions_tbl` via
  `application/Organization/session::validate_session`. Resolved
  `AuthUser { owner_id, actor_id, session_id }` goes into request
  extensions; unknown key vs bad session both → `401`.
- **Authorization**: `require_document_access(state, document_id, id,
  user, DocumentAccessLevel::{Read,Write})` (`handlers.rs:1400`) is
  invoked on every document-scoped route (~30 call sites); document
  permissions live in `document_permissions_tbl`. CLI auth commands
  (`kaladoc auth`/`user`) manage sessions/accounts.
- API keys are config-level (env/TOML), not DB-managed — no key
  rotation/revocation API yet.

## CLI command groups

`kaladoc` (`cli/args.rs` `Commands` enum): `version`, `doctor`, `init`,
`config`, `auth`, `user`, `jobs` (worker/list/enqueue/cancel/retry/
recover; worker also registers + ticks the scheduler), `engine`,
`doc` (document store + pdf-op enqueue), `pdf`, `image`, `pipeline`,
`storage`, `schedule`, `media` (`op --list-ops`), `convert`
(`--from/--to/--options/--output-file`), `serve` (`--addr`,
`--worker` embedded job worker), `mcp` (stdio transport only).

## MCP server

`cli_mcp_server/mcp/server.rs` — rmcp-based server named `kaladoc`,
stdio transport only (no unauthenticated network listener, per spec).
**26 tools**, all prefixed `kaladoc.` (count asserted by unit test):

`system_info`, `system_health`, `engine_list`, `engine_health`,
`storage_status`, `document_list`, `document_search`, `document_get`,
`document_history`, `document_versions`, `document_artifacts`,
`document_tags`, `document_create`, `document_ingest`, `document_rename`,
`document_move`, `document_delete`, `document_restore`,
`document_restore_version`, `folder_list`, `folder_create`,
`job_enqueue`, `job_list`, `job_cancel`, `pipeline_list`,
`pipeline_run`.

Server instructions warn that document content/metadata is untrusted
data, never instructions.

## Deployment artifacts

- `Dockerfile` — multi-stage (`rust:1-bookworm` builder →
  `debian:bookworm-slim` runtime, unprivileged user). Runtime deps:
  `ca-certificates`, `ffmpeg` (multimedia engine), `poppler-utils`
  (`pdftoppm`/`pdfinfo`). State under `/data` (`DB_PATH`,
  `KALADOC_STORAGE_DIR`, `KALADOC_API_ADDR=0.0.0.0:8080`).
- `docker-compose.yml`.
- `deploy/kubernetes/` — `deployment.yaml`, `worker.yaml` (job worker
  Deployment), `configmap.yaml`, `pvc.yaml`, `secret.example.yaml`,
  `kustomization.yaml`; `deploy/README.md` documents env config.

## Database schema (document store)

`document_engine/src/store/schema.rs` — idempotent `IF NOT EXISTS`
migrations. Tables: `folders_tbl`, `documents_tbl`, `artifacts_tbl`,
`revisions_tbl`, `versions_tbl`, `tags_tbl`, `tag_assignments_tbl`,
`document_permissions_tbl`, `document_metadata_tbl`,
`document_settings_tbl` (new since original report — per-document
settings JSON + `is_locked`), `document_relationships_tbl`,
`document_audit_tbl`. The `document_metadata_tbl` and
`document_relationships_tbl` definitions from the original report are
unchanged and verified verbatim against `schema.rs:195-250`.

Job/schedule/session tables live in `job_engine`/`scheduler`/
`application::Organization` stores respectively.

## Test inventory

~1,800 test attributes total (`grep -c '#\[test\]\|#\[tokio::test\]'`
over `src/` + `tests/`). Per-area breakdown (unit tests in
`src/crates/*/src` + colocated `src/crates/pdf-engine/tests/`):

| Area | Tests |
|---|---|
| pdf-engine (src + tests/) | ~725 (659 src + 66 integration-suite bodies) |
| multimedia-engine | ~258 |
| image-engine | ~135 |
| conversion-engine | ~112 |
| application (job_handlers, services, cli, mcp) | ~98 |
| document_engine | ~83 |
| job_engine | ~46 |
| pipeline-engine | ~33 |
| scheduler | ~18 |
| api | ~19 |
| observability | ~17 |
| security-engine | ~49 |
| storage | ~51 |
| engine-registry | ~9 |
| core + config | ~29 |

Top-level `tests/` integration suites (shims `include!` the colocated
pdf-engine suites for `advanced_ops`/`pdf_roundtrip`/`pdf_perf`/
`enc_probe`):

| Suite | Tests | Covers |
|---|---|---|
| `api_pdf_e2e.rs` | 4 | API ↔ pdf engine end-to-end |
| `conversion_e2e.rs` | 25 | Format conversion incl. external backends |
| `document_e2e.rs` | 6 | Document lifecycle via API/services |
| `image_security_e2e.rs` | 27 | Image ops + security-engine validation |
| `ingestion_e2e.rs` | 7 | Ingest → version → restore flows |
| `jobs_e2e.rs` | 9 | Job queue, handlers, recovery |
| `multimedia_e2e.rs` | 19 | media.op dispatch, ffmpeg probing/fallbacks |
| `pipeline_scheduler_e2e.rs` | 11 | Pipelines, DAGs, schedules, ticks |
| `storage_e2e.rs` | 8 | Streaming upload, CAS dedup, staging sweep |

## Files changed (by area, cumulative)

- **Storage**: `storage/stream.rs` (StreamingBackend/StreamUpload/
  staging/atomic commit), `storage/remote/{mod,config,s3}.rs` (S3
  SigV4 backend), `storage/dedup.rs` (CAS + ingest dedup, hardlink-
  preferred, opt-in via `StorageConfig::dedup`), `storage/local`,
  `storage/backend.rs`.
- **Document engine**: `document_engine/src/store/{schema,metadata,
  relationships,documents,folders,artifacts,versions,tags,permissions,
  settings,history}.rs`, `audit.rs` (expanded event set),
  `settings.rs`, `organization.rs` + `Organization/session.rs`
  (sessions/accounts).
- **Application**: `services/{app,documents,storage,ingestion,
  media_ops,schedules,engines,access,folder_watch}.rs`,
  `job_handlers.rs` (~8k lines: 14 kinds, PDF_OPS/MEDIA_OPS dispatch,
  enqueue helpers, per-op status/audit), `cli_mcp_server/` (clap CLI +
  rmcp MCP server + shared mcp handlers).
- **Engines**: `pdf-engine` (ops/ — ~80 op modules: page structure,
  marks, security, editing, portfolios, timestamps, OCR prep),
  `image-engine` (ops/ — 19 modules incl. scan_fx, ocr),
  `conversion-engine` (backends/{rust,external/{pandoc,libreoffice,
  poppler}}, archive/eml/url), `multimedia-engine` (97 files: 47+
  operation modules over ffmpeg/ffprobe), `pipeline-engine`
  (model/dag/engine/builtin/limits), `security-engine` (filename/
  validate/engine), `engine-registry`.
- **API**: `api/src/{lib,auth,error,flags,handlers,openapi,state,
  tests}.rs` — full REST surface above.
- **Jobs/scheduling**: `job_engine` (engine/store), `scheduler`
  (schedule/store/handler — fixed-interval, not cron).
- **Deploy/config**: `Dockerfile`, `docker-compose.yml`,
  `deploy/kubernetes/*`, `src/config/mod.rs` (env/TOML incl.
  `api.api_keys`).
- **Docs/tests**: `.doc/*` reports, `tests/*_e2e.rs` + shims,
  colocated engine test suites.

## Known limitations

- **SQLite single-writer**: the whole platform shares one SQLite pool;
  job workers, API, and scheduler ticks contend on a single writer.
  Fine for single-node deployments; no multi-writer/HA story.
- **External tool absence is honest but limiting**: multimedia ops
  need `ffmpeg`/`ffprobe` (MissingTool → structured `MediaError`);
  image/pdf `ocr` needs `tesseract` (engine reports `Degraded`
  health); pdf rasterization needs `pdftoppm`; conversions degrade to
  whatever external backends (pandoc/libreoffice/poppler) are
  installed. Docker image ships ffmpeg+poppler but **not tesseract**.
- **Pipeline DAGs are sequential and same-input**: `run_dag` computes
  topological waves but executes them in deterministic order (no task
  spawning); every node sees the *same* input bytes — edges express
  dependency/failure propagation only, never data flow between nodes.
  `max_total_bytes` across a DAG is an approximation.
- **Media ops use file-based temp files**: ffmpeg/ffprobe exchange via
  on-disk temp files rather than pipes; large media jobs are I/O bound
  and temp-space constrained.
- **API keys are static config** (`api.api_keys`/`KALADOC_API_KEYS`),
  not DB-managed — no per-key scopes, rotation, or revocation API.
  AuthZ granularity is per-document Read/Write only.
- **Schedules are fixed-interval, not cron**; a missed interval never
  double-fires (documented semantics).
- **`document.index` has no FTS**: extracted text lands in metadata
  `custom_fields`; `document_search` (MCP/API) is a case-insensitive
  substring match on document *names* only — indexed text is not
  searchable.
- **`thumbnail.generate` doesn't rasterize** PDF pages (embedded images
  only). (`pdf sign` is now advertised via `EngineOperation::Sign` —
  the original report's caveat is stale.)
- **MCP is stdio-only** by design; no remote MCP transport/auth.
- **`StreamingBackend` is synchronous** (callers use `spawn_blocking`)
  and `LocalUploadWriter` is single-flight per backend instance;
  S3 `StreamingBackend` impl still absent (S3 backend exists for
  regular ops only — verify before assuming streaming-over-S3).

## Remaining work

- **PDF→office conversions are partial**: the in-process `rust`
  backend covers txt/markdown/html→pdf, pdf→txt/markdown, images→pdf,
  eml→pdf; office formats (docx/odt/xlsx/pptx) depend on external
  pandoc/libreoffice backends and degrade when absent. epub archive/
  url fetch landed recently — coverage matrix still uneven.
- **SSO/OIDC**: sessions + static API keys only; no OIDC/SAML/LDAP.
- **AI features**: none (no embeddings, no LLM-assisted ops).
- **Event bus / webhooks**: audit events are DB rows; no pub/sub,
  webhook, or streaming notification channel.
- **Persisted pipeline store**: pipelines are built-in definitions or
  inline JSON per request — no CRUD API/DB table for saved pipelines.
- **DAG parallelism**: waves computed but executed sequentially; no
  data-dependent routing between nodes.
- **Full-text search** for indexed document text.
- **Multi-node/job-queue scaling**: SQLite job engine is single-node.

## Original-milestone verification notes (still accurate)

The original report's schema SQL for `document_metadata_tbl` /
`document_relationships_tbl`, the service-layer descriptions
(`DocumentService`, `StorageService`, `DocumentIngestionService`,
`AppServices`), streaming-upload semantics, and the fixes list were
re-checked against current sources and remain accurate. New since then:
`document_settings_tbl`, `document_audit_tbl` persistence,
`storage/dedup.rs` CAS, `Organization/session` auth, the entire
engine/API/CLI/MCP/deploy surface above.
