# KalaDoc Job Engine

Durable, SQLite-backed job queue. Producers enqueue rows into
`jobs_tbl`; a `JobEngine` claims due jobs and dispatches them to
registered `JobHandler`s. Failed jobs retry with exponential backoff.

Source: `src/crates/job_engine/`

## Concepts

| Piece | Where | Purpose |
|---|---|---|
| `jobs_tbl` | `store.rs` | Queue table: `kind`, JSON `payload`, `status` (`pending`/`running`/`succeeded`/`failed`), `priority`, `attempts`/`max_attempts`, `run_at`, `last_error`, `worker_id` |
| `JobEngine` | `engine.rs` | Owns the pool + handler registry; `run()` is the poll loop |
| `JobHandler` | `handler.rs` | Trait handlers implement; object-safe boxed-future `handle()` |
| `JobContext` | `handler.rs` | Shared services handed to handlers (currently the `SqlitePool`) |
| `NewJob` / `Job` | `job.rs` | Enqueue input / row shape |

Claiming is atomic: `UPDATE ... WHERE id = (SELECT ... pending ...)`
with `RETURNING`, so two engines can share one database safely.
Handlers should still be **idempotent** — a crash between claim and
completion leaves the job `running` until `reap_stale()` returns it to
`pending` after `stale_after`.

## Wiring it up

```rust
use kaladoc::config::AppConfig;
use kaladoc::crates::job_engine::{JobEngine, JobEngineConfig, JobHandler, JobContext, Job, JobError, NewJob};
use std::sync::Arc;

// 1. Config → engine
let app = AppConfig::load()?;
let pool = init_db::init_db_with(&app.to_db_config()).await?; // creates jobs_tbl
let engine = Arc::new(JobEngine::new(pool.clone(), app.to_job_config()));

// 2. Register handlers by kind
engine.register("document.ingest", Arc::new(DocumentIngestHandler));

// 3. Spawn the poll loop (abort the JoinHandle on shutdown)
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

## Failure semantics

- Handler `Err` → job returns to `pending` with
  `run_at = now + backoff_base * 2^attempts` (cap 1 h) until
  `attempts == max_attempts`, then `failed`.
- Unknown `kind` → `failed` immediately with "no handler registered".
- `run_once()` returns `Ok(false)` when the queue is drained — useful
  in tests; `run()` loops forever on `poll_interval`.

## Not yet implemented

- No CLI commands for `jobs list/enqueue/cancel` — add under
  `cli_mcp_server::cli::command` when needed.
- No priorities beyond the integer column; no per-kind concurrency
  limits; no dead-letter queue (failed rows stay in `jobs_tbl`).
- Single-writer SQLite means one engine per database is the intended
  deployment; multi-engine claiming works but throughput is bounded by
  SQLite's writer lock.
