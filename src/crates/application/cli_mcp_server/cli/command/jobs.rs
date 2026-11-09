//! `kaladoc jobs worker|list|enqueue|cancel|retry|recover`
//!
//! Thin adapter over `crate::crates::job_engine`: the worker builds a
//! [`JobEngine`] from the resolved configuration, registers the
//! built-in handlers, and runs the poll loop until the process is
//! interrupted; `list`/`enqueue` go straight to the `jobs_tbl` store.

use std::sync::Arc;

use serde::Serialize;
use serde_json::Value;

use crate::crates::application::Organization::init_db;
use crate::crates::job_engine::{self, Job, JobEngine, JobStatus, NewJob};

use super::super::args::JobsArgs;
use super::{CliError, Context};

pub async fn run(ctx: &Context, args: &JobsArgs) -> Result<i32, CliError> {
    match args {
        JobsArgs::Worker => worker(ctx).await,
        JobsArgs::List { status, limit } => list(ctx, status.as_deref(), *limit).await,
        JobsArgs::Enqueue {
            kind,
            payload,
            priority,
            run_at,
            max_attempts,
        } => {
            enqueue(
                ctx,
                kind,
                payload.as_deref(),
                *priority,
                *run_at,
                *max_attempts,
            )
            .await
        }
        JobsArgs::Cancel { id } => cancel(ctx, *id).await,
        JobsArgs::Retry { id } => retry(ctx, *id).await,
        JobsArgs::Recover => recover(ctx).await,
    }
}

// ---------------------------------------------------------------------------
// worker
// ---------------------------------------------------------------------------

/// Runs the engine's poll loop forever. The JoinHandle only resolves if
/// the task panics — Ctrl+C terminates the process normally.
async fn worker(ctx: &Context) -> Result<i32, CliError> {
    let db_config = ctx.db_config()?;
    let pool = init_db::init_db_with(&db_config).await?;

    let app_config = ctx.config()?;
    let mut engine = JobEngine::new(pool, app_config.to_job_config());
    crate::crates::application::job_handlers::register_builtin(&mut engine);

    // Reclaim jobs a previous crashed worker left in `running` before
    // this worker starts claiming new ones.
    let recovered = engine.recover().await?;
    if recovered > 0 {
        eprintln!("recovered {recovered} interrupted job(s) left in running state");
    }

    let engine = Arc::new(engine);
    eprintln!("job worker {} started", engine.worker_id());

    engine
        .clone()
        .spawn()
        .await
        .map_err(|e| CliError::Internal(format!("job worker task failed: {e}")))?;
    Ok(0)
}

// ---------------------------------------------------------------------------
// list
// ---------------------------------------------------------------------------

async fn list(ctx: &Context, status: Option<&str>, limit: i64) -> Result<i32, CliError> {
    if limit <= 0 {
        return Err(CliError::Usage("--limit must be positive".to_string()));
    }
    let status = match status {
        Some(s) => Some(JobStatus::parse(s).ok_or_else(|| {
            CliError::Usage(format!(
                "invalid --status {s:?}; expected pending, running, \
                     succeeded, failed, or cancelled"
            ))
        })?),
        None => None,
    };

    let db = ctx.db_config()?;
    let pool = init_db::init_db_with(&db).await?;
    let jobs = job_engine::list_jobs(&pool, status, limit).await?;
    pool.close().await;

    let views: Vec<JobView> = jobs.iter().map(JobView::from).collect();
    if ctx.emit_structured(&views)? {
        return Ok(0);
    }
    println!(
        "{:<5} {:<24} {:<10} {:<9} {:<8} {:<12} last_error",
        "id", "kind", "status", "attempts", "priority", "run_at"
    );
    for j in &views {
        println!(
            "{:<5} {:<24} {:<10} {:<9} {:<8} {:<12} {}",
            j.id,
            j.kind,
            j.status,
            format!("{}/{}", j.attempts, j.max_attempts),
            j.priority,
            j.run_at,
            j.last_error.as_deref().unwrap_or("")
        );
    }
    if views.is_empty() {
        println!("(no jobs)");
    }
    Ok(0)
}

// ---------------------------------------------------------------------------
// enqueue
// ---------------------------------------------------------------------------

#[derive(Debug, Serialize)]
struct EnqueueReport {
    job: JobView,
}

async fn enqueue(
    ctx: &Context,
    kind: &str,
    payload: Option<&str>,
    priority: i64,
    run_at: Option<i64>,
    max_attempts: Option<u32>,
) -> Result<i32, CliError> {
    let payload: Value = match payload {
        Some(p) => serde_json::from_str(p)?,
        None => Value::Object(Default::default()),
    };

    let db = ctx.db_config()?;
    let pool = init_db::init_db_with(&db).await?;

    // The engine's configured default applies when --max-attempts is
    // omitted; we only need the config for that one field.
    let default_max_attempts = ctx.config()?.to_job_config().default_max_attempts;
    let job = job_engine::enqueue(
        &pool,
        &NewJob {
            kind: kind.to_string(),
            payload,
            priority,
            run_at,
            max_attempts,
        },
        default_max_attempts,
    )
    .await?;
    pool.close().await;

    let report = EnqueueReport {
        job: JobView::from(&job),
    };

    if ctx.emit_structured(&report)? {
        return Ok(0);
    }
    println!("enqueued job {} ({})", report.job.id, report.job.kind);
    Ok(0)
}

// ---------------------------------------------------------------------------
// cancel / retry / recover
// ---------------------------------------------------------------------------

#[derive(Debug, Serialize)]
struct JobActionReport {
    id: i64,
    cancelled: bool,
}

async fn cancel(ctx: &Context, id: i64) -> Result<i32, CliError> {
    let db = ctx.db_config()?;
    let pool = init_db::init_db_with(&db).await?;
    let cancelled = job_engine::cancel_job(&pool, id).await?;
    pool.close().await;

    let report = JobActionReport { id, cancelled };
    if ctx.emit_structured(&report)? {
        return Ok(0);
    }
    if cancelled {
        println!("job {id} cancelled");
    } else {
        println!("job {id} was not cancellable (already finished)");
    }
    Ok(0)
}

#[derive(Debug, Serialize)]
struct RetryReport {
    id: i64,
    requeued: bool,
}

async fn retry(ctx: &Context, id: i64) -> Result<i32, CliError> {
    let db = ctx.db_config()?;
    let pool = init_db::init_db_with(&db).await?;
    let requeued = job_engine::retry_job(&pool, id).await?;
    pool.close().await;

    let report = RetryReport { id, requeued };
    if ctx.emit_structured(&report)? {
        return Ok(0);
    }
    if requeued {
        println!("job {id} requeued for another attempt");
    } else {
        println!("job {id} was not retryable (not failed)");
    }
    Ok(0)
}

#[derive(Debug, Serialize)]
struct RecoverReport {
    recovered: u64,
}

async fn recover(ctx: &Context) -> Result<i32, CliError> {
    let db = ctx.db_config()?;
    let pool = init_db::init_db_with(&db).await?;
    let recovered = job_engine::requeue_running(&pool).await?;
    pool.close().await;

    let report = RecoverReport { recovered };
    if ctx.emit_structured(&report)? {
        return Ok(0);
    }
    println!("reclaimed {recovered} interrupted job(s)");
    Ok(0)
}

// ---------------------------------------------------------------------------
// Internals
// ---------------------------------------------------------------------------

/// Serializable view of a [`Job`] — `Job` itself is not `Serialize`.
#[derive(Debug, Serialize)]
struct JobView {
    id: i64,
    kind: String,
    payload: Value,
    status: String,
    priority: i64,
    attempts: u32,
    max_attempts: u32,
    run_at: i64,
    created_at: i64,
    started_at: Option<i64>,
    finished_at: Option<i64>,
    last_error: Option<String>,
    worker_id: Option<String>,
}

impl From<&Job> for JobView {
    fn from(job: &Job) -> Self {
        Self {
            id: job.id,
            kind: job.kind.clone(),
            payload: job.payload.clone(),
            status: job.status.as_str().to_string(),
            priority: job.priority,
            attempts: job.attempts,
            max_attempts: job.max_attempts,
            run_at: job.run_at,
            created_at: job.created_at,
            started_at: job.started_at,
            finished_at: job.finished_at,
            last_error: job.last_error.clone(),
            worker_id: job.worker_id.clone(),
        }
    }
}
