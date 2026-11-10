# KalaDoc Job Engine

Durable, SQLite-backed job queue. Producers enqueue rows into
`jobs_tbl`; a `JobEngine` claims due jobs and dispatches them to
registered `JobHandler`s. Failed jobs retry with exponential backoff.
A worker process (`kaladoc jobs worker`) owns the run loop; jobs left
`running` by a crash are recovered on the next engine start.

Source: `src/crates/job_engine/`

## Concepts

| Piece | Where | Purpose |
|---|---|---|
| `jobs_tbl` | `store.rs` | Queue table: `kind`, JSON `payload`, `status` (`pending`/`running`/`succeeded`/`failed`/`cancelled`), `priority`, `attempts`/`max_attempts`, `run_at`, `last_error`, `worker_id` |
| `JobEngine` | `engine.rs` | Owns the pool + handler registry; `run()` is the poll loop |
| `JobHandler` | `handler.rs` | Trait handlers implement; object-safe boxed-future `handle()` |
| `JobContext` | `handler.rs` | Shared services handed to handlers (currently the `SqlitePool`) |
| `NewJob` / `Job` | `job.rs` | Enqueue input / row shape |
| Built-in handlers | `application/job_handlers.rs` | `document.*`, `pdf.inspect`, `video.*`, `storage.gc_staging`, `system.ping` |

Claiming is atomic: `UPDATE ... WHERE id = (SELECT ... pending ...)`
with `RETURNING`, so two engines can share one database safely.
Handlers should still be **idempotent** — a crash between claim and
completion leaves the job `running` until recovery returns it to
`pending` (see below).

## Crash recovery

Two mechanisms return interrupted work to the queue:

- **Startup recovery** — `JobEngine::run()` calls
  [`JobEngine::recover`] once before polling: every `running` row is
  requeued as `pending` (`run_at = now`, `worker_id`/`started_at`
  cleared, `last_error = 'recovered: interrupted by shutdown'`).
  Single-engine-per-database is the intended deployment, so any
  `running` row seen at startup belongs to a dead process. Do not run
  two engines against the same database concurrently — the second
  engine's startup recovery would steal the first engine's in-flight
  jobs.
- **Stale reaping** — [`JobEngine::reap_stale`] runs on every poll
  tick and returns `running` jobs older than `stale_after` to
  `pending`. This covers a live engine whose handler hung or was
  orphaned without a process restart.

The same primitives back the CLI: `kaladoc jobs recover` requeues
interrupted jobs on demand, `kaladoc jobs worker` performs startup
recovery before entering the poll loop.

## CLI

```
kaladoc jobs worker                       # run the engine poll loop
kaladoc jobs list [--status S] [--limit N]
kaladoc jobs enqueue --kind K [--payload JSON] [--priority N]
                            [--run-at UNIX] [--max-attempts N]
kaladoc jobs cancel <id>                  # pending|running → cancelled
kaladoc jobs retry <id>                   # failed → pending (attempts reset)
kaladoc jobs recover                      # requeue all running jobs now
```

All commands support `--output json|yaml` via the global flags.

## Built-in handlers

`application::job_handlers::register_builtin` registers every known
job `kind` on the engine at worker startup:

| Kind | Payload | Behavior |
|---|---|---|
| `system.ping` | any (ignored) | No-op success; queue smoke test / health probe. |
| `document.ingest` | `{document_id, content_b64` or `content`, `file_name?`, `mime_type?}` | Writes payload bytes into storage and registers the document artifact. |
| `document.metadata_extract` | `{document_id, artifact_id?}` | Sniffs the format and extracts PDF metadata into document metadata. |
| `document.index` | `{document_id, artifact_id?}` | Extracts searchable text into an `index` metadata field. |
| `thumbnail.generate` | `{document_id, artifact_id?}` | Extracts an embedded PDF thumbnail image into storage. |
| `pdf.inspect` | `{ "path": "<relative under storage root>", "max_bytes"?: u64 }` | Size-checks before reading (default cap 100 MiB), validates bytes via `security_engine::validate_pdf`, inspects via `pdf_engine`, writes `<path>.inspect.json` next to the input. Paths are confined to the storage root — absolute paths and `..` are rejected. |
| `video.probe` | `{ "path": "<relative under storage root>" }` | Runs `ffprobe` on the media file and writes `<path>.probe.json` next to the input with container/stream/duration metadata. Requires `ffprobe` on PATH. |
| `video.thumbnail` | `{ "path": "<relative under storage root>", "at_secs"?, "max_bytes"? }` | Extracts a PNG frame via `ffmpeg` into storage as a thumbnail artifact. |
| `video.transcode` | `{ "path": "<relative under storage root>", "target_ext", "max_bytes"? }` | Re-encodes the media via `ffmpeg`; writes `<path>.<target_ext>`. |
| `video.trim` | `{ "path": "<relative under storage root>", "start_secs", "end_secs"?, "reencode"?, "max_bytes"? }` | Cuts a segment via `ffmpeg`; stream-copy by default (keyframe-aligned), re-encode when frame accuracy is requested. |
| `video.remux` | `{ "path": "<relative under storage root>", "target_ext", "max_bytes"? }` | Stream-copy remux to `target_ext`; writes `<path>.<target_ext>`. |
| `video.extract_frames` | `{ "path": "...", "every_secs"?, "fps"?, "all"?, "keyframes_only"?, "format"?, "max_bytes"? }` | Extracts frames; writes files under `<path>.frames/`. |
| `video.subtitles_extract` | `{ "path": "...", "track"?, "format"?, "max_bytes"? }` | Extracts a subtitle track; writes `<path>.subs.<fmt>`. |
| `video.analyze` | `{ "path": "...", "max_bytes"? }` | Full media analysis report; writes `<path>.analysis.json`. |
| `video.package_hls` | `{ "path": "...", "max_bytes"? }` | HLS package; writes files under `<path>.hls/`. |
| `video.metadata_set` | `{ "path": "...", "tags": {"k": "v"}, "max_bytes"? }` | Sets container tags; writes `<path>.tagged<ext>`. |
| `video.metadata_strip` | `{ "path": "...", "max_bytes"? }` | Strips all metadata; writes `<path>.stripped<ext>`. |
| `video.extract_streams` | `{ "path": "...", "subtitles"?, "attachments"?, "cover_art"?, "chapters"?, "data"?, "subtitle_format"?, "max_bytes"? }` | Extracts embedded streams; writes files under `<path>.streams/`. |
| `video.audio_volume` | `{ "path": "...", "gain_db", "max_bytes"? }` | Applies gain in dB; writes `<path>.vol<ext>`. |
| `video.contact_sheet` | `{ "path": "...", "cols"?, "rows"?, "thumb_width"?, "interval_secs"?, "max_bytes"? }` | Builds a contact-sheet PNG; writes `<path>.sheet.png`. |
| `video.speed` | `{ "path": "...", "factor", "max_bytes"? }` | Changes playback speed; writes `<path>.speed<ext>`. |
| `video.reverse` | `{ "path": "...", "max_bytes"? }` | Reverses the clip; writes `<path>.reverse<ext>`. |
| `video.loop` | `{ "path": "...", "count", "max_bytes"? }` | Loops the clip; writes `<path>.loop<ext>`. |
| `video.freeze` | `{ "path": "...", "at_secs", "duration_secs", "max_bytes"? }` | Freezes a frame; writes `<path>.freeze<ext>`. |
| `video.resize` | `{ "path": "...", "width", "height", "fit"?, "max_bytes"? }` | Resizes per fit mode; writes `<path>.resize<ext>`. |
| `video.crop` | `{ "path": "...", "x", "y", "w", "h", "max_bytes"? }` | Crops to a rect; writes `<path>.crop<ext>`. |
| `video.rotate` | `{ "path": "...", "degrees", "max_bytes"? }` | Rotates; writes `<path>.rotate<ext>`. |
| `video.flip` | `{ "path": "...", "horizontal"?, "max_bytes"? }` | Mirrors; writes `<path>.flip<ext>`. |
| `video.gif` | `{ "path": "...", "fps"?, "width"?, "max_bytes"? }` | Video → animated GIF; writes `<path>.gif`. |
| `video.slideshow` | `{ "paths": ["..."], "fps"?, "duration_per_image"?, "target_ext", "max_bytes"? }` | Images → video; writes `slideshow.<target_ext>` next to the first input. |
| `video.normalize` | `{ "path": "...", "target_lufs"?, "max_bytes"? }` | EBU R128 loudness normalize; writes `<path>.norm<ext>`. |
| `video.loudness` | `{ "path": "...", "max_bytes"? }` | Measures loudness; writes `<path>.loudness.json`. |
| `video.scene_frames` | `{ "path": "...", "threshold"?, "max_bytes"? }` | Scene-change keyframes; writes files under `<path>.scenes/`. |
| `video.merge_av` | `{ "video_path": "...", "audio_path": "...", "target_ext"?, "max_bytes"? }` | Merges an audio track into a video; writes `<video_path>.merged<ext>`. |

All `video.*` kinds share the `video_engine` contract: `path` (and
`video_path`/`audio_path`/`paths` entries) are **relative paths
confined to the storage root** — absolute paths and `..` are
rejected — `max_bytes` defaults to 4 GiB, and every op requires
`ffmpeg`/`ffprobe` on `PATH`. Multi-input kinds read several confined
paths. See `.doc/video_engine.md` for the full ops catalog and CLI
surface; kinds not listed here (e.g. `concat`, `extract_audio`,
subtitles, filters, packaging variants) are CLI-only for now.
| `storage.gc_staging` | `{ "retention_secs"?: u64 }` | Deletes `<storage_root>/staging/` entries older than the retention window (default 86400 s). Abandoned staging objects from interrupted uploads are cleaned up here — the issue's crash-recovery requirement. |

The `document.*` handlers share these properties (see the module docs
in `job_handlers.rs` for the full contract):

- `artifact_id` is optional and selects a specific artifact; when
  omitted the most recently created artifact is used.
- Artifact bytes are read through the streaming backend with a hard
  cap (`MAX_PROCESSING_BYTES`, 64 MiB); larger objects are recorded as
  `skipped_too_large` rather than read into memory.
- They are idempotent — deterministic metadata fields and storage
  object names are overwritten, so job retries and crash-recovery
  replays are safe.
- Per-document progress is reported through document metadata custom
  fields (`processing_status`: `queued` → `running` → `succeeded` /
  `failed` / `skipped_too_large`) backed by audit events.

Producers should enqueue via the `enqueue_ingest`,
`enqueue_metadata_extract`, `enqueue_index`, and `enqueue_thumbnail`
helpers rather than hand-building payloads — they construct the
documented payload contracts and stamp `processing_status = queued`.

Handlers resolve the storage root via `AppConfig::load_lenient()` →
`to_storage_config().root_dir`.

## Wiring it up

```rust
use kaladoc::config::AppConfig;
use kaladoc::crates::job_engine::{JobEngine, JobEngineConfig, JobHandler, JobContext, Job, JobError, NewJob};
use std::sync::Arc;

// 1. Config → engine
let app = AppConfig::load()?;
let pool = init_db::init_db_with(&app.to_db_config()).await?; // creates jobs_tbl
let mut engine = JobEngine::new(pool.clone(), app.to_job_config());

// 2. Register handlers by kind (or all built-ins at once)
kaladoc::crates::application::job_handlers::register_builtin(&mut engine);

// 3. Spawn the poll loop — run() recovers interrupted jobs first
//    (abort the JoinHandle on shutdown)
let engine = Arc::new(engine);
let handle = engine.clone().spawn();
```

## Enqueueing a job

```rust
engine.enqueue(&NewJob {
    kind: "document.ingest".into(),
    payload: serde_json::json!({ "document_id": 42, "path": "inbox/a.pdf" }),
    priority: 0,                    // higher runs first
    run_at: None,                   // None = now; Some(unix) = delayed
    max_attempts: Some(5),          // None = config default (3)
}).await?;
```

## Writing a handler

```rust
struct DocumentIngestHandler;

impl JobHandler for DocumentIngestHandler {
    fn handle<'a>(
        &'a self,
        ctx: &'a JobContext,
        job: &'a Job,
    ) -> std::pin::Pin<Box<dyn std::future::Future<Output = Result<(), JobError>> + Send + 'a>> {
        Box::pin(async move {
            let doc_id = job.payload["document_id"].as_i64()
                .ok_or_else(|| JobError::Failed("missing document_id".into()))?;
            // ... do work via ctx.pool ...
            Ok(())
        })
    }
}
```

Return `Err(JobError::Failed("reason".into()))` (or any `JobError`) to
trigger a retry; the message lands in `jobs_tbl.last_error`.

## Configuration

`kaladoc.toml`:

```toml
[jobs]
poll_interval_secs = 2      # engine poll cadence
default_max_attempts = 3    # retry budget when NewJob.max_attempts is None
backoff_base_secs = 30      # delay = base * 2^attempts, capped at 1h
stale_after_secs = 600      # running jobs older than this are reaped
```

Env overrides: `JOB_POLL_INTERVAL_SECS`, `JOB_MAX_ATTEMPTS`,
`JOB_BACKOFF_BASE_SECS`, `JOB_STALE_AFTER_SECS`.

`poll_interval_secs = 0` and `default_max_attempts = 0` are rejected at
config load.

## Inspecting the queue

```rust
use kaladoc::crates::job_engine::{list_jobs, JobStatus};

let pending = list_jobs(&pool, Some(JobStatus::Pending), 50).await?;
let recent  = list_jobs(&pool, None, 50).await?; // newest first
```

Or from the shell: `kaladoc jobs list --status running`.

## Failure semantics

- Handler `Err` → job returns to `pending` with
  `run_at = now + backoff_base * 2^attempts` (cap 1 h) until
  `attempts == max_attempts`, then `failed`.
- Unknown `kind` → `failed` immediately with "no handler registered".
- A panicking handler is caught per-poll and recorded as a job failure
  (`handler panicked: ...`) — the engine keeps running.
- `run_once()` returns `Ok(false)` when the queue is drained — useful
  in tests; `run()` loops forever on `poll_interval`.
- `kaladoc jobs retry <id>` resets a `failed` job to `pending` with
  `attempts = 0` for a fresh budget.

## Not yet implemented

- No per-kind concurrency limits; no dead-letter queue (failed rows
  stay in `jobs_tbl`, inspectable via `kaladoc jobs list`).
- `jobs worker` has no graceful-drain shutdown — Ctrl+C terminates the
  process; in-flight jobs are recovered on next start.
- Single-writer SQLite means one engine per database is the intended
  deployment; multi-engine claiming works but throughput is bounded by
  SQLite's writer lock, and concurrent engines' startup recovery can
  steal each other's `running` jobs.
