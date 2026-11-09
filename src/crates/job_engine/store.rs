//! SQLite persistence for the job queue (`jobs_tbl`).
//!
//! All functions take `&SqlitePool`. Timestamps are Unix seconds
//! (`INTEGER`), matching `sessions_tbl`.

use sqlx::sqlite::SqliteRow;
use sqlx::{Executor, Row, SqlitePool};

use super::JobError;
use super::job::{Job, JobStatus, NewJob};

/// Creates `jobs_tbl` and its indexes. Idempotent.
pub async fn create_jobs_table(pool: &SqlitePool) -> Result<(), JobError> {
    pool.execute(
        r#"
        CREATE TABLE IF NOT EXISTS jobs_tbl (
            id            INTEGER PRIMARY KEY AUTOINCREMENT,
            kind          TEXT NOT NULL,
            payload       TEXT NOT NULL,
            status        TEXT NOT NULL DEFAULT 'pending',
            priority      INTEGER NOT NULL DEFAULT 0,
            attempts      INTEGER NOT NULL DEFAULT 0,
            max_attempts  INTEGER NOT NULL DEFAULT 3,
            run_at        INTEGER NOT NULL,
            created_at    INTEGER NOT NULL,
            started_at    INTEGER,
            finished_at   INTEGER,
            last_error    TEXT,
            worker_id     TEXT
        );
        "#,
    )
    .await?;

    // Hot path: find the next due pending job.
    pool.execute(
        "CREATE INDEX IF NOT EXISTS idx_jobs_due ON jobs_tbl(status, run_at, priority DESC, id);",
    )
    .await?;

    Ok(())
}

/// Inserts a new pending job and returns it.
pub async fn enqueue(
    pool: &SqlitePool,
    new: &NewJob,
    default_max_attempts: u32,
) -> Result<Job, JobError> {
    let now = now_unix();
    let payload = serde_json::to_string(&new.payload)?;
    let max_attempts = new.max_attempts.unwrap_or(default_max_attempts);
    let run_at = new.run_at.unwrap_or(now);

    let id = sqlx::query(
        "INSERT INTO jobs_tbl
            (kind, payload, status, priority, attempts, max_attempts, run_at, created_at)
         VALUES (?, ?, 'pending', ?, 0, ?, ?, ?)",
    )
    .bind(&new.kind)
    .bind(&payload)
    .bind(new.priority)
    .bind(max_attempts as i64)
    .bind(run_at)
    .bind(now)
    .execute(pool)
    .await?
    .last_insert_rowid();

    Ok(Job {
        id,
        kind: new.kind.clone(),
        payload: new.payload.clone(),
        status: JobStatus::Pending,
        priority: new.priority,
        attempts: 0,
        max_attempts,
        run_at,
        created_at: now,
        started_at: None,
        finished_at: None,
        last_error: None,
        worker_id: None,
    })
}

/// Atomically claims the next due pending job for `worker_id`.
///
/// The `UPDATE ... WHERE id = (SELECT ...)` subquery plus the
/// `status='pending'` predicate makes the claim safe even if two
/// workers race — SQLite serializes writers, and the loser sees
/// `rows_affected() == 0` and moves on.
pub async fn claim_next(
    pool: &SqlitePool,
    worker_id: &str,
    now: i64,
) -> Result<Option<Job>, JobError> {
    let row = sqlx::query(
        "UPDATE jobs_tbl
         SET status = 'running', worker_id = ?, started_at = ?,
             attempts = attempts + 1, last_error = NULL
         WHERE id = (
             SELECT id FROM jobs_tbl
             WHERE status = 'pending' AND run_at <= ?
             ORDER BY priority DESC, id
             LIMIT 1
         )
         RETURNING id, kind, payload, status, priority, attempts, max_attempts,
                   run_at, created_at, started_at, finished_at, last_error, worker_id",
    )
    .bind(worker_id)
    .bind(now)
    .bind(now)
    .fetch_optional(pool)
    .await?;

    Ok(row.map(job_from_row).transpose()?)
}

/// Marks a claimed job succeeded or schedules its next retry.
///
/// Every UPDATE is guarded by `status = 'running' AND worker_id = ?
/// AND attempts = ?` so completion only applies while the job is still
/// owned by this worker's claim. If the job was cancelled mid-flight
/// (`cancel_job`), reaped as stale and re-claimed, or otherwise moved
/// on, the update is a no-op and the current status is preserved —
/// the handler's side effects already happened, but a stale worker
/// must not resurrect a `cancelled` job or clobber a newer attempt.
///
/// On failure with attempts remaining, the job returns to `pending`
/// with `run_at = now + backoff_secs` (exponential backoff capped at
/// [`MAX_BACKOFF_SECS`]). Otherwise it becomes `failed`.
pub async fn complete(
    pool: &SqlitePool,
    job: &Job,
    result: Result<(), String>,
    backoff_secs: u64,
    now: i64,
) -> Result<(), JobError> {
    let worker_id = job.worker_id.as_deref().unwrap_or("");
    let attempts = job.attempts as i64;
    match result {
        Ok(()) => {
            sqlx::query(
                "UPDATE jobs_tbl SET status = 'succeeded', finished_at = ?, last_error = NULL
                 WHERE id = ? AND status = 'running' AND worker_id = ? AND attempts = ?",
            )
            .bind(now)
            .bind(job.id)
            .bind(worker_id)
            .bind(attempts)
            .execute(pool)
            .await?;
        }
        Err(err) => {
            // Re-read attempts/max_attempts to decide retry vs fail,
            // still scoped to this worker's claim.
            let row = sqlx::query(
                "SELECT attempts, max_attempts FROM jobs_tbl
                 WHERE id = ? AND status = 'running' AND worker_id = ? AND attempts = ?",
            )
            .bind(job.id)
            .bind(worker_id)
            .bind(attempts)
            .fetch_optional(pool)
            .await?;
            let Some(row) = row else {
                // Job left `running` under this claim (cancelled,
                // reaped, re-claimed) — nothing to do.
                return Ok(());
            };
            let attempts_now: i64 = row.get("attempts");
            let max_attempts: i64 = row.get("max_attempts");

            if attempts_now < max_attempts {
                let delay = backoff_secs
                    .saturating_mul(1u64 << attempts_now.min(20) as u32)
                    .min(MAX_BACKOFF_SECS);
                sqlx::query(
                    "UPDATE jobs_tbl
                     SET status = 'pending', run_at = ?, finished_at = NULL,
                         worker_id = NULL, started_at = NULL, last_error = ?
                     WHERE id = ? AND status = 'running' AND worker_id = ? AND attempts = ?",
                )
                .bind(now + delay as i64)
                .bind(&err)
                .bind(job.id)
                .bind(worker_id)
                .bind(attempts_now)
                .execute(pool)
                .await?;
            } else {
                sqlx::query(
                    "UPDATE jobs_tbl SET status = 'failed', finished_at = ?, last_error = ?
                     WHERE id = ? AND status = 'running' AND worker_id = ? AND attempts = ?",
                )
                .bind(now)
                .bind(&err)
                .bind(job.id)
                .bind(worker_id)
                .bind(attempts_now)
                .execute(pool)
                .await?;
            }
        }
    }
    Ok(())
}

/// Reclaims every job still marked `running` by returning it to
/// `pending` with all execution bookkeeping cleared.
///
/// Intended for engine startup recovery: with a single engine per
/// database, any `running` row at startup belongs to a dead process,
/// so it is safe to requeue immediately rather than waiting for
/// `stale_after`. Returns rows reclaimed.
pub async fn requeue_running(pool: &SqlitePool) -> Result<u64, JobError> {
    let done = sqlx::query(
        "UPDATE jobs_tbl
         SET status = 'pending', worker_id = NULL, started_at = NULL,
             finished_at = NULL, run_at = ?, last_error = 'recovered: interrupted by shutdown'
         WHERE status = 'running'",
    )
    .bind(now_unix())
    .execute(pool)
    .await?;
    Ok(done.rows_affected())
}

/// Cancels a `pending` or `running` job. Returns `true` when a row
/// was updated; `false` when the job does not exist or already
/// reached a terminal state.
pub async fn cancel_job(pool: &SqlitePool, job_id: i64) -> Result<bool, JobError> {
    let done = sqlx::query(
        "UPDATE jobs_tbl SET status = 'cancelled', finished_at = ?
         WHERE id = ? AND status IN ('pending', 'running')",
    )
    .bind(now_unix())
    .bind(job_id)
    .execute(pool)
    .await?;
    Ok(done.rows_affected() > 0)
}

/// Manually retries a `failed` job: resets it to a fresh `pending`
/// state (attempts cleared, scheduling fields reset). Returns `false`
/// when the job does not exist or is not `failed`.
pub async fn retry_job(pool: &SqlitePool, job_id: i64) -> Result<bool, JobError> {
    let done = sqlx::query(
        "UPDATE jobs_tbl
         SET status = 'pending', attempts = 0, run_at = ?,
             finished_at = NULL, started_at = NULL, worker_id = NULL,
             last_error = NULL
         WHERE id = ? AND status = 'failed'",
    )
    .bind(now_unix())
    .bind(job_id)
    .execute(pool)
    .await?;
    Ok(done.rows_affected() > 0)
}

/// Cap for exponential retry backoff (1 hour).
const MAX_BACKOFF_SECS: u64 = 3600;

/// Lists jobs, optionally filtered by status, newest first.
pub async fn list_jobs(
    pool: &SqlitePool,
    status: Option<JobStatus>,
    limit: i64,
) -> Result<Vec<Job>, JobError> {
    let rows = match status {
        Some(s) => {
            sqlx::query(&format!(
                "{JOB_COLUMNS} FROM jobs_tbl WHERE status = ? ORDER BY id DESC LIMIT ?"
            ))
            .bind(s.as_str())
            .bind(limit)
            .fetch_all(pool)
            .await?
        }
        None => {
            sqlx::query(&format!(
                "{JOB_COLUMNS} FROM jobs_tbl ORDER BY id DESC LIMIT ?"
            ))
            .bind(limit)
            .fetch_all(pool)
            .await?
        }
    };
    rows.into_iter().map(job_from_row).collect()
}

const JOB_COLUMNS: &str =
    "SELECT id, kind, payload, status, priority, attempts, max_attempts, run_at,
            created_at, started_at, finished_at, last_error, worker_id";

fn job_from_row(row: SqliteRow) -> Result<Job, JobError> {
    let payload_text: String = row.get("payload");
    let status_text: String = row.get("status");
    Ok(Job {
        id: row.get("id"),
        kind: row.get("kind"),
        payload: serde_json::from_str(&payload_text)?,
        status: JobStatus::parse(&status_text).unwrap_or(JobStatus::Pending),
        priority: row.get("priority"),
        attempts: row.get::<i64, _>("attempts") as u32,
        max_attempts: row.get::<i64, _>("max_attempts") as u32,
        run_at: row.get("run_at"),
        created_at: row.get("created_at"),
        started_at: row.get("started_at"),
        finished_at: row.get("finished_at"),
        last_error: row.get("last_error"),
        worker_id: row.get("worker_id"),
    })
}

fn now_unix() -> i64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs() as i64)
        .unwrap_or(0)
}
