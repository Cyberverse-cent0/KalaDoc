# KalaDoc Architecture

KalaDoc is a Rust application for ingesting, storing, and processing documents.
This document describes the architecture and the modules that exist today.

## High-level view

```
                        ┌──────────────────┐
        CLI  ─────────▶ │  cli_mcp_server  │   kaladoc <cmd>
                        └────────┬─────────┘
        HTTP client ──▶ ┌─────────▼─────────┐
                        │   api (axum)      │   /documents /jobs /engines ...
                        └────────┬─────────┘
                                 │ AppServices facade
                        ┌────────▼─────────┐
                        │  application     │   documents, ingestion, engines
                        └────────┬─────────┘
              ┌──────────────────┼───────────────────┐
              ▼                  ▼                   ▼
        ┌───────────┐    ┌──────────────┐    ┌──────────────┐
        │  storage  │    │ document_    │    │   job_engine │
        │ local/S3  │    │ engine       │    │  (+scheduler)│
        └───────────┘    └──────────────┘    └──────┬───────┘
              ▲                                     │ dispatch
              │                                     ▼
              │              engine_registry → pdf / image / multimedia /
              │                            conversion / pipeline / security
              └── shared_public (platform detection)
```

Dependency rule: lower layers never depend on higher layers.
`shared_public` has no dependencies. `storage` depends only on
`shared_public`. Engines implement the `core::engine::ProcessingEngine`
contract and are held as trait objects by `engine_registry`; adapters
(CLI, API, job handlers) never call engines directly.

## Current module tree

```
src/
├── main.rs                          # binary entry point (kaladoc)
├── lib.rs                           # library root — declares all modules
├── config/
│   └── mod.rs                       # AppConfig: defaults → TOML → env
├── core/
│   └── auth.rs                      # AuthUser row type
├── crates/
│   ├── api/                         # HTTP API (axum router, handlers, OpenAPI)
│   ├── application/
│   │   ├── Organization/            # init_db (DbConfig, pool, schema),
│   │   │                            #   auth_user (Argon2id), session (tokens)
│   │   ├── services/                # AppServices facade (storage, documents,
│   │   │                            #   ingestion, engine registry)
│   │   ├── job_handlers.rs          # Built-in job kinds → handlers
│   │   └── cli_mcp_server/cli/      # CLI adapter (clap command tree)
│   ├── core/                        # ProcessingEngine contract, EngineKind,
│   │   └── engine.rs                #   capabilities, health, limits
│   ├── conversion-engine/           # Format conversion via pluggable backends
│   │                                #   (rust/lopdf, pandoc, libreoffice, poppler)
│   ├── document_engine/             # Document domain + SQL store
│   │   └── store/                   #   documents, versions, artifacts,
│   │                                #   folders, tags, history, permissions
│   ├── engine-registry/             # EngineRegistry: register/list/health
│   ├── image-engine/                # Image ops (rotate, watermark, ...)
│   ├── job_engine/                  # Durable job queue + dispatcher (jobs_tbl)
│   ├── multimedia-engine/           # ffmpeg/ffprobe-backed media ops
│   │   └── runtime/                 #   discovery (PATH), hwaccel, process runner
│   ├── observability/               # Logging init, health, request IDs
│   ├── pdf-engine/                  # PDF ops (100+ operations)
│   │   └── ops/                     #   merge, split, sign, encrypt, ocr, ...
│   ├── pipeline-engine/             # Multi-step PipelineDefinition runner
│   ├── scheduler/                   # Recurring schedules driven by job queue
│   ├── security-engine/             # Upload validation, filename sanitize,
│   │                                #   magic-byte sniffing, ResourceLimits
│   ├── shared_public/
│   │   └── platform.rs              # Platform detection (OS/arch/family)
│   └── storage/
│       ├── backend.rs               # StorageBackend trait
│       ├── local/                   # Local FS backend (config, create_fs,
│       │                            #   debug_when_full, setup)
│       ├── remote/                  # S3-compatible backend (env-only config)
│       └── stream.rs                # Streaming helpers
└── deployments/                     # OS/distro checks + release packaging
    └── require.rs                   # rust/cargo/tar/zip presence, hints
```

## Module responsibilities

### `core::engine` (crates/core)
The `ProcessingEngine` trait every engine implements: `name`, `version`,
`kind` ([`EngineKind`]: pdf, image, conversion, security, pipeline,
multimedia), `capabilities` (operations, formats, `EngineLimits`), and
`health`. This is the only contract adapters use to reach engines.

### `engine_registry`
`EngineRegistry::builtin()` registers the six built-in engines —
`ConversionEngine`, `ImageEngine`, `MediaEngine`, `PdfEngine`,
`PipelineEngine`, `SecurityEngine` — and exposes `list`, `get`,
`health`, and `health_all`. Re-registering a name replaces the engine.

### `api`
Thin axum adapter over `AppServices`: bearer-token auth against
`sessions_tbl` on every request (except public `GET /health` and
`GET /openapi.json`), upload validation via `security_engine`,
persistence via the document store, follow-up work via `job_engine`.
No business rules live here. Served by `kaladoc serve` on `api.addr`.

### `application`
- `Organization` — SQLite users/sessions (see `config` below for knobs).
- `services` — `AppServices` facade wiring storage, document store,
  ingestion pipeline, and the engine registry for CLI and API callers.
- `job_handlers` — built-in job kinds: `document.ingest`,
  `document.metadata_extract`, `document.index`, `thumbnail.generate`,
  `document.pdf_op` (any advertised pdf op), `pipeline.run`,
  `pdf.inspect`, `storage.gc_staging`, `system.ping`, plus the scheduler's
  `scheduler.tick`.
- `cli_mcp_server/cli` — clap command tree: `version`, `doctor`, `init`,
  `config`, `auth`, `user`, `jobs`, `engine`, `doc`, `pdf`, `serve`.

### `storage`
- `backend.rs` — `StorageBackend` trait shared by local and remote.
- `local/` — `StorageConfig` (root dir, optional quota, target
  filesystem, fault injection), `FaultInjectingFs` (path confinement,
  in-memory quota accounting, deterministic fault injection),
  `create_fs` device probing/`format_disk`.
- `remote/` — `S3Backend`, an S3-compatible backend (AWS S3, MinIO, R2,
  ...) with path-style URLs and SigV4 auth. Configured **only** via
  `KALADOC_S3_*` env vars (`RemoteStorageConfig::from_env`) — credentials
  never come from files. Retries transient failures per `RetryPolicy`;
  `4xx` is never retried.
- `stream.rs` — streaming read/write helpers used by ingestion.

### `document_engine`
Document domain model (documents, versions, artifacts, folders, tags,
relationships, audit, lifecycle state machine) plus a SQL persistence
layer under `store/` (documents, versions, artifacts, folders, tags,
history, permissions, metadata, relationships, schema).

### `job_engine`
Durable job queue over SQLite (`jobs_tbl`). Producers call
`JobEngine::enqueue`; `run()` claims due jobs atomically
(`UPDATE ... RETURNING`) and dispatches to `JobHandler`s. Failures retry
with exponential backoff (`backoff_base * 2^attempts`, cap 1 h) up to
`max_attempts`. Crash recovery: `run()` calls `recover()` at startup to
requeue `running` rows, and `reap_stale` returns jobs older than
`stale_after` to `pending` every poll tick. See `.doc/job_engine.md`.

### `scheduler`
Recurring schedules persisted in SQLite. A self-perpetuating
`scheduler.tick` job (enqueued by `enqueue_tick`, driven by
`SchedulerTickHandler`) fires due schedules each tick and enqueues the
next tick `tick_interval_secs` later, so the job engine owns the loop
with no separate background task. `SchedulerConfig` currently uses its
built-in defaults (60 s tick) — it is not yet wired to `AppConfig`.

### `security_engine`
Upload/request validation: filename sanitization, magic-byte content
sniffing (never trusts extensions), `ResourceLimits` (size, page, pixel,
output budgets), PDF structure validation. Enforced by the API upload
path and reused by job handlers.

### `pdf_engine`
In-process PDF manipulation built on `lopdf` plus external tools
(`pdftoppm`, `tesseract`, `openssl` for render/OCR/signing). ~100 ops
covering page ops, encryption, signatures, forms, redaction, inspection,
and conversion helpers. Ops are advertised through `EngineCapabilities`.

### `image_engine`
Image processing ops (resize, rotate, watermark, effects, ...) over
common raster formats.

### `multimedia_engine`
Audio/video processing backed by managed `ffmpeg`/`ffprobe` processes.
`runtime::discovery` locates both binaries on `PATH` (required — no
config keys yet) and caches capability sets (codecs, filters, hwaccels,
protocols) so `health`/`capabilities` report honestly per FFmpeg build.
`MediaLimits` bounds input/output bytes, per-invocation timeout, and
output file count. Ops span probe/inspect, conversion/transcode,
extraction, video, audio, frames, subtitles, analysis, packaging
(HLS/DASH), metadata, and encoding.

### `conversion_engine`
Document format conversion (`pdf`, `txt`, `markdown`, `html`, `png`,
`jpeg`, `docx`, `odt`) through pluggable backends tried in order:
`rust` (in-process lopdf/pdf-engine), `pandoc`, `libreoffice`, `poppler`
(externals detected on `PATH`; honest `supports()` matrices, fallback on
backend failure). See `.doc/conversion_engine.md`.

### `pipeline_engine`
Runs multi-step `PipelineDefinition`s — each step invokes an engine op
over byte streams — with `PipelineLimits` (per-step input bytes, total
materialized bytes, step count, page/output budgets) carried per
definition, not globally configured.

### `observability`
Logging initialization, health aggregation helpers, and request-ID
middleware for the API.

### `config`
Global [`AppConfig`] resolved in layers: built-in defaults → optional
TOML file (`KALADOC_CONFIG`, `--config`, or `kaladoc.toml` in the cwd) →
environment variables. Secrets (`APP_SECRET_KEY`, `APP_DB_USER`,
`APP_DB_PASSWORD`) are env-only. `load()` validates the merged result —
zero intervals/pool sizes, malformed `log_level`, path-like filesystem
names, and bad `api.addr` socket addresses are rejected with
[`ConfigError`]. Sections: `database` (+`session`), `storage`, `runtime`,
`jobs`, `api` — see the env-var table on `AppConfig::apply_env`.

Configured outside `AppConfig` by design: remote S3 storage
(`KALADOC_S3_*` env vars only, via `RemoteStorageConfig::from_env`),
multimedia tool discovery (`PATH` only), scheduler tick settings
(built-in defaults), and pipeline limits (per-definition).

### `core::auth` (src/core)
`AuthUser` — the `users_tbl` row shape shared by the application layer.

### `application::Organization`
User accounts and sessions over SQLite (`sqlx`):

- `init_db` loads `DbConfig` via `crate::config::AppConfig` (defaults →
  optional `kaladoc.toml`/`KALADOC_CONFIG` → env), opens a tuned WAL
  pool, and creates `users_tbl`/`sessions_tbl` plus the job/scheduler/
  document tables. Session knobs: `SESSION_TTL_SECS`,
  `SESSION_ABSOLUTE_TTL_SECS`, `SESSION_RENEW_ON_USE`,
  `SESSION_TOUCH_INTERVAL_SECS`, `SESSION_CLEANUP_INTERVAL_SECS`.
- `auth_user` handles registration, lookup, Argon2id password
  verification, and deletion.
- `session` issues opaque bearer tokens (only `SHA-256(token)` is
  stored), validates them with sliding expiry capped by an optional
  absolute TTL, throttles `last_seen_at` writes via `touch_interval`
  (validation is read-only when nothing changed), supports per-token,
  per-id, and per-user revocation plus `list_active_sessions`, deletes
  expired rows via `cleanup_expired_sessions`, and `spawn_cleanup_task`
  repeats that on the configured interval.

### `video_engine`
Video/audio processing engine (`crates/video-engine`) implementing
`ProcessingEngine` as `name = "video"`, `kind = EngineKind::Video`.
All operations shell out to external `ffmpeg`/`ffprobe` binaries on
`PATH` — never bundled — and are exposed via the `kaladoc video`
CLI (~80 subcommands covering probe/trim/transcode/remux, geometry,
tempo, audio processing, frames/slideshows, subtitles, filters,
overlays, stream ops, analysis/loudness/quality, HLS/DASH packaging,
and runtime capability discovery) and the `video.*` job kinds.
Runtime capability discovery (`media_capabilities`,
`ffmpeg -encoders`-backed encode presets) advertises only what the
host ffmpeg build supports. Registered in `EngineRegistry::builtin()`
alongside the other engines. See `.doc/video_engine.md`.

### `deployments`
Deployment prerequisite checker and release packaging: `require.rs`
verifies `rust`, `cargo`, `tar`, `zip`, `ffmpeg`, and `ffprobe` are on
PATH (`which`/`where`), detects OS via `std::env::consts::OS` and Linux
distro via `/etc/os-release`, and prints per-distro install hints;
`package.rs`, `release.rs`, and `archive.rs` plus per-distro manifests
support release builds (`src/bin/kaladoc-release.rs`).

## Design principles

1. **`std`-first where possible.** Storage delegates to `std::fs` and
   intercepts at the operation level; engines reuse proven libraries
   (lopdf, image, sqlx, axum) rather than reimplementing them.
2. **Probe, don't guess.** `StorageTarget` fields are `Option`; engine
   `health`/`capabilities` report what the installed toolchain
   (ffmpeg build, external converters) actually supports.
3. **Errors carry context.** `StorageError`/`FsError`/`MediaError`/
   `PipelineError` implement `Display` + `Error::source`, tagging
   failures with the offending path or step.
4. **Destructive ops are explicit.** `format_disk` and equivalents are
   separate, clearly named functions — never invoked implicitly.
5. **Fault injection is opt-in.** Disabled by default; enabled via config
   or `KALADOC_FAULT_INJECTION` env vars for reproducible failure testing.
6. **Secrets stay out of files.** `APP_*` and `KALADOC_S3_*` credentials
   are environment-only by design.
