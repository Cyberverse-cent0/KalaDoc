//! SQLite persistence for the job queue (`jobs_tbl`).
//!
//! All functions take `&SqlitePool`. Timestamps are Unix seconds
//! (`INTEGER`), matching `sessions_tbl`.

use sqlx::sqlite::SqliteRow;
use sqlx::{Executor, Row, SqlitePool};

use super::JobError;
use super::job::{BatchProgress, Job, JobStatus, NewJob};

/// Creates `jobs_tbl` and its indexes. Idempotent.
///
/// There is no schema-version table in this codebase; migrations are
/// the same `CREATE TABLE IF NOT EXISTS` plus idempotent
/// `ALTER TABLE ... ADD COLUMN` guards below, so calling this on an
/// existing database upgrades it in place.
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
            worker_id     TEXT,
            batch_id      INTEGER,
            progress_json TEXT
        );
        "#,
    )
    .await?;

    // Columns added after the first release — `ALTER TABLE` is the
    // only way to bring pre-existing `jobs_tbl` rows up to date.
    // `ensure_column` no-ops when the column is already present.
    ensure_column(pool, "jobs_tbl", "batch_id", "INTEGER").await?;
    ensure_column(pool, "jobs_tbl", "progress_json", "TEXT").await?;

    // Hot path: find the next due pending job.
    pool.execute(
        "CREATE INDEX IF NOT EXISTS idx_jobs_due ON jobs_tbl(status, run_at, priority DESC, id);",
    )
    .await?;

    // Batch lookups: aggregate progress and per-item listing.
    pool.execute("CREATE INDEX IF NOT EXISTS idx_jobs_batch ON jobs_tbl(batch_id);")
        .await?;

    Ok(())
}

/// Adds `column` to `table` when missing. `table`/`column`/`decl`
/// are compile-time constants at every call site — never user input.
async fn ensure_column(
    pool: &SqlitePool,
    table: &str,
    column: &str,
    decl: &str,
) -> Result<(), JobError> {
    let exists = sqlx::query(&format!("PRAGMA table_info({table})"))
        .fetch_all(pool)
        .await?
        .iter()
        .any(|row| row.get::<String, _>("name") == column);
    if !exists {
        sqlx::query(&format!("ALTER TABLE {table} ADD COLUMN {column} {decl}"))
            .execute(pool)
            .await?;
    }
    Ok(())
}

/// Inserts a new pending job and returns it.
pub async fn enqueue(
    pool: &SqlitePool,
    new: &NewJob,
    default_max_attempts: u32,
) -> Result<Job, JobError> {
    insert_job(pool, new, default_max_attempts).await
}

/// Enqueues `jobs` as one batch in a single transaction.
///
/// Design: every item is an ordinary `jobs_tbl` row — same kinds,
/// payloads, priorities, retries, and per-item cancellation as a
/// single [`enqueue`]. The only addition is a shared `batch_id`
/// (the `id` of the first inserted row) that ties the group
/// together for progress aggregation and batch cancel. This was
/// chosen over a payload convention (`{items: [...]}`) because it
/// keeps per-item retry/backoff and crash recovery for free and
/// needs no new table.
///
/// Cancellation semantics: [`cancel_batch`] cancels *all*
/// non-terminal items in the batch (whole-batch cancel); individual
/// items can still be cancelled one at a time via [`cancel_job`].
/// There is no per-item cancel inside a running batch handler —
/// handlers see each item as a separate job claim.
///
/// Returns the inserted jobs in input order. An empty slice returns
/// an empty `Vec` without touching the database.
pub async fn enqueue_batch(
    pool: &SqlitePool,
    jobs: &[NewJob],
    default_max_attempts: u32,
) -> Result<Vec<Job>, JobError> {
    if jobs.is_empty() {
        return Ok(Vec::new());
    }

    let mut tx = pool.begin().await?;
    let mut out = Vec::with_capacity(jobs.len());

    // The first row's id becomes the batch_id for the whole group.
    // Insert it first so its id is known before the rest are written.
    let first = insert_job(&mut *tx, &jobs[0], default_max_attempts).await?;
    let batch_id = first.id;
    out.push(first);

    for new in &jobs[1..] {
        let mut job = insert_job(&mut *tx, new, default_max_attempts).await?;
        sqlx::query("UPDATE jobs_tbl SET batch_id = ? WHERE id = ?")
            .bind(batch_id)
            .bind(job.id)
            .execute(&mut *tx)
            .await?;
        job.batch_id = Some(batch_id);
        out.push(job);
    }

    // Point the first row at itself so every member shares batch_id.
    sqlx::query("UPDATE jobs_tbl SET batch_id = ? WHERE id = ?")
        .bind(batch_id)
        .bind(batch_id)
        .execute(&mut *tx)
        .await?;
    if let Some(first) = out.first_mut() {
        first.batch_id = Some(batch_id);
    }

    tx.commit().await?;
    Ok(out)
}

/// Inserts one pending job; `batch_id` is left NULL (batch members
/// get it via [`enqueue_batch`]). Generic over the executor so it
/// works on the pool and inside a transaction.
async fn insert_job<'e, E>(ex: E, new: &NewJob, default_max_attempts: u32) -> Result<Job, JobError>
where
    E: Executor<'e, Database = sqlx::Sqlite>,
{
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
    .execute(ex)
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
        batch_id: None,
        progress: None,
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
             attempts = attempts + 1, last_error = NULL, progress_json = NULL
         WHERE id = (
             SELECT id FROM jobs_tbl
             WHERE status = 'pending' AND run_at <= ?
             ORDER BY priority DESC, id
             LIMIT 1
         )
         RETURNING id, kind, payload, status, priority, attempts, max_attempts,
                   run_at, created_at, started_at, finished_at, last_error, worker_id,
                   batch_id, progress_json",
    )
    .bind(worker_id)
    .bind(now)
    .bind(now)
    .fetch_optional(pool)
    .await?;

    row.map(job_from_row).transpose()
}

/// Marks a claimed job succeeded or schedules its next retry.
///
/// On failure with attempts remaining, the job returns to `pending`
/// with `run_at = now + backoff_secs` (exponential backoff capped at
/// [`MAX_BACKOFF_SECS`]). Otherwise it becomes `failed`.
pub async fn complete(
    pool: &SqlitePool,
    job_id: i64,
    result: Result<(), String>,
    backoff_secs: u64,
    now: i64,
) -> Result<(), JobError> {
    match result {
        Ok(()) => {
            sqlx::query(
                "UPDATE jobs_tbl SET status = 'succeeded', finished_at = ?, last_error = NULL
                 WHERE id = ?",
            )
            .bind(now)
            .bind(job_id)
            .execute(pool)
            .await?;
        }
        Err(err) => {
            // Re-read attempts/max_attempts to decide retry vs fail.
            let row = sqlx::query("SELECT attempts, max_attempts FROM jobs_tbl WHERE id = ?")
                .bind(job_id)
                .fetch_one(pool)
                .await?;
            let attempts: i64 = row.get("attempts");
            let max_attempts: i64 = row.get("max_attempts");

            if attempts < max_attempts {
                let delay = backoff_secs
                    .saturating_mul(1u64 << attempts.min(20) as u32)
                    .min(MAX_BACKOFF_SECS);
                sqlx::query(
                    "UPDATE jobs_tbl
                     SET status = 'pending', run_at = ?, finished_at = NULL,
                         last_error = ?, progress_json = NULL
                     WHERE id = ?",
                )
                .bind(now + delay as i64)
                .bind(&err)
                .bind(job_id)
                .execute(pool)
                .await?;
            } else {
                sqlx::query(
                    "UPDATE jobs_tbl SET status = 'failed', finished_at = ?, last_error = ?
                     WHERE id = ?",
                )
                .bind(now)
                .bind(&err)
                .bind(job_id)
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
             finished_at = NULL, run_at = ?, progress_json = NULL,
             last_error = 'recovered: interrupted by shutdown'
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
             last_error = NULL, progress_json = NULL
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
            created_at, started_at, finished_at, last_error, worker_id,
            batch_id, progress_json";

fn job_from_row(row: SqliteRow) -> Result<Job, JobError> {
    let payload_text: String = row.get("payload");
    let status_text: String = row.get("status");
    let progress_text: Option<String> = row.get("progress_json");
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
        batch_id: row.get("batch_id"),
        progress: progress_text
            .map(|text| serde_json::from_str(&text))
            .transpose()?,
    })
}

/// Marks a `pending` or `running` job `skipped` — a terminal state
/// for work the handler intentionally did not perform (e.g. a batch
/// item already up to date). `reason` is stored in `last_error` for
/// observability. Returns `true` when the job was updated; `false`
/// when it does not exist or already reached a terminal state.
///
/// Skipped jobs count toward `items_done` (not `items_pending`) in
/// [`batch_progress`], so a batch of all-skipped items still
/// completes.
pub async fn skip_job(
    pool: &SqlitePool,
    job_id: i64,
    reason: Option<&str>,
) -> Result<bool, JobError> {
    let done = sqlx::query(
        "UPDATE jobs_tbl SET status = 'skipped', finished_at = ?, last_error = ?
         WHERE id = ? AND status IN ('pending', 'running')",
    )
    .bind(now_unix())
    .bind(reason)
    .bind(job_id)
    .execute(pool)
    .await?;
    Ok(done.rows_affected() > 0)
}

/// Stores a handler-reported progress snapshot on a `running` job.
///
/// `progress` is arbitrary JSON — the convention is an object like
/// `{"pct": 40, "detail": "compressing page 3/10"}`. Returns `true`
/// when the update landed; `false` when the job is not currently
/// `running` (e.g. it was cancelled or already finished), in which
/// case the report is dropped.
///
/// Progress is cleared automatically when the job is claimed for a
/// new attempt, on retry, and on crash recovery, so a stale
/// percentage never survives into the next run.
pub async fn report_progress(
    pool: &SqlitePool,
    job_id: i64,
    progress: &serde_json::Value,
) -> Result<bool, JobError> {
    let text = serde_json::to_string(progress)?;
    let done =
        sqlx::query("UPDATE jobs_tbl SET progress_json = ? WHERE id = ? AND status = 'running'")
            .bind(&text)
            .bind(job_id)
            .execute(pool)
            .await?;
    Ok(done.rows_affected() > 0)
}

/// Lists every job belonging to `batch_id`, in enqueue order
/// (ascending id). Returns an empty `Vec` for unknown batches.
pub async fn batch_jobs(pool: &SqlitePool, batch_id: i64) -> Result<Vec<Job>, JobError> {
    let rows = sqlx::query(&format!(
        "{JOB_COLUMNS} FROM jobs_tbl WHERE batch_id = ? ORDER BY id"
    ))
    .bind(batch_id)
    .fetch_all(pool)
    .await?;
    rows.into_iter().map(job_from_row).collect()
}

/// Aggregate progress for one batch: total items, how many reached a
/// terminal state, and a per-status breakdown. Returns `None` when
/// no jobs carry `batch_id`.
///
/// Terminal states are `succeeded`, `failed`, `skipped`, and
/// `cancelled`; `pending` and `running` count as not done.
pub async fn batch_progress(
    pool: &SqlitePool,
    batch_id: i64,
) -> Result<Option<BatchProgress>, JobError> {
    let rows = sqlx::query(
        "SELECT status, COUNT(*) AS n FROM jobs_tbl WHERE batch_id = ? GROUP BY status",
    )
    .bind(batch_id)
    .fetch_all(pool)
    .await?;

    if rows.is_empty() {
        return Ok(None);
    }

    let mut total = 0u64;
    let mut pending = 0u64;
    let mut succeeded = 0u64;
    let mut failed = 0u64;
    let mut skipped = 0u64;
    let mut cancelled = 0u64;
    for row in &rows {
        let n: i64 = row.get("n");
        let count = n.max(0) as u64;
        total += count;
        match row.get::<String, _>("status").as_str() {
            "pending" | "running" => pending += count,
            "succeeded" => succeeded += count,
            "failed" => failed += count,
            "skipped" => skipped += count,
            "cancelled" => cancelled += count,
            _ => {}
        }
    }

    let done = succeeded + failed + skipped + cancelled;
    Ok(Some(BatchProgress {
        batch_id,
        items_total: total,
        items_done: done,
        items_pending: pending,
        items_succeeded: succeeded,
        items_failed: failed,
        items_skipped: skipped,
        items_cancelled: cancelled,
    }))
}

/// Cancels every non-terminal (`pending` or `running`) job in the
/// batch — whole-batch cancellation. Items already in a terminal
/// state keep their outcome. Individual items can still be cancelled
/// separately via [`cancel_job`]. Returns the number of jobs
/// cancelled.
pub async fn cancel_batch(pool: &SqlitePool, batch_id: i64) -> Result<u64, JobError> {
    let done = sqlx::query(
        "UPDATE jobs_tbl SET status = 'cancelled', finished_at = ?
         WHERE batch_id = ? AND status IN ('pending', 'running')",
    )
    .bind(now_unix())
    .bind(batch_id)
    .execute(pool)
    .await?;
    Ok(done.rows_affected())
}

fn now_unix() -> i64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs() as i64)
        .unwrap_or(0)
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;
    use sqlx::sqlite::SqlitePoolOptions;

    async fn test_pool() -> SqlitePool {
        let pool = SqlitePoolOptions::new()
            .max_connections(1)
            .connect("sqlite::memory:")
            .await
            .unwrap();
        create_jobs_table(&pool).await.unwrap();
        pool
    }

    fn now() -> i64 {
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|d| d.as_secs() as i64)
            .unwrap_or(0)
    }

    fn new_job(kind: &str) -> NewJob {
        NewJob {
            kind: kind.into(),
            payload: json!({"k": "v"}),
            priority: 0,
            run_at: None,
            max_attempts: None,
        }
    }

    fn job_by_id(jobs: &[Job], id: i64) -> &Job {
        jobs.iter().find(|j| j.id == id).expect("job present")
    }

    #[tokio::test]
    async fn create_jobs_table_is_idempotent() {
        let pool = test_pool().await;
        // Second call must not error on existing schema/indexes.
        create_jobs_table(&pool).await.unwrap();
    }

    #[tokio::test]
    async fn enqueue_returns_pending_job_with_defaults() {
        let pool = test_pool().await;
        let before = now();
        let job = enqueue(&pool, &new_job("a.b"), 5).await.unwrap();
        let after = now();

        assert!(job.id > 0);
        assert_eq!(job.kind, "a.b");
        assert_eq!(job.payload, json!({"k": "v"}));
        assert_eq!(job.status, JobStatus::Pending);
        assert_eq!(job.priority, 0);
        assert_eq!(job.attempts, 0);
        assert_eq!(job.max_attempts, 5); // engine default applied
        assert!((before..=after).contains(&job.run_at));
        assert!((before..=after).contains(&job.created_at));
        assert_eq!(job.started_at, None);
        assert_eq!(job.finished_at, None);
        assert_eq!(job.last_error, None);
        assert_eq!(job.worker_id, None);
        assert_eq!(job.batch_id, None);
        assert_eq!(job.progress, None);
    }

    #[tokio::test]
    async fn enqueue_respects_explicit_fields() {
        let pool = test_pool().await;
        let mut new = new_job("prio");
        new.priority = 9;
        new.max_attempts = Some(1);
        new.run_at = Some(now() + 3600);
        let job = enqueue(&pool, &new, 5).await.unwrap();

        assert_eq!(job.priority, 9);
        assert_eq!(job.max_attempts, 1); // explicit beats default
        assert!(job.run_at > now());
    }

    #[tokio::test]
    async fn enqueue_batch_empty_and_grouping() {
        let pool = test_pool().await;
        assert!(enqueue_batch(&pool, &[], 3).await.unwrap().is_empty());

        let jobs = vec![new_job("b"), new_job("b"), new_job("b")];
        let out = enqueue_batch(&pool, &jobs, 3).await.unwrap();
        assert_eq!(out.len(), 3);
        let batch_id = out[0].id;
        assert!(out.iter().all(|j| j.batch_id == Some(batch_id)));
    }

    #[tokio::test]
    async fn claim_next_orders_by_priority_then_id() {
        let pool = test_pool().await;
        // Enqueue low priority first so id order alone would pick it.
        let low = enqueue(
            &pool,
            &NewJob {
                priority: -5,
                ..new_job("k")
            },
            3,
        )
        .await
        .unwrap();
        let high_a = enqueue(
            &pool,
            &NewJob {
                priority: 10,
                ..new_job("k")
            },
            3,
        )
        .await
        .unwrap();
        let high_b = enqueue(
            &pool,
            &NewJob {
                priority: 10,
                ..new_job("k")
            },
            3,
        )
        .await
        .unwrap();

        let t = now() + 10;
        // Same priority → lowest id first.
        let c1 = claim_next(&pool, "w", t).await.unwrap().unwrap();
        assert_eq!(c1.id, high_a.id);
        let c2 = claim_next(&pool, "w", t).await.unwrap().unwrap();
        assert_eq!(c2.id, high_b.id);
        let c3 = claim_next(&pool, "w", t).await.unwrap().unwrap();
        assert_eq!(c3.id, low.id);
        assert!(claim_next(&pool, "w", t).await.unwrap().is_none());
    }

    #[tokio::test]
    async fn claim_next_skips_future_and_non_pending_jobs() {
        let pool = test_pool().await;
        let future = enqueue(
            &pool,
            &NewJob {
                run_at: Some(now() + 3600),
                ..new_job("later")
            },
            3,
        )
        .await
        .unwrap();
        let due = enqueue(&pool, &new_job("now"), 3).await.unwrap();

        let claimed = claim_next(&pool, "w1", now()).await.unwrap().unwrap();
        assert_eq!(claimed.id, due.id);

        // Only the future job remains → nothing claimable yet.
        assert!(claim_next(&pool, "w1", now()).await.unwrap().is_none());
        // Once its run_at passes it is claimed.
        let claimed = claim_next(&pool, "w1", now() + 7200)
            .await
            .unwrap()
            .unwrap();
        assert_eq!(claimed.id, future.id);
    }

    #[tokio::test]
    async fn claim_next_marks_running_and_increments_attempts() {
        let pool = test_pool().await;
        let job = enqueue(&pool, &new_job("k"), 3).await.unwrap();

        let claimed = claim_next(&pool, "worker-7", now() + 5)
            .await
            .unwrap()
            .unwrap();
        assert_eq!(claimed.status, JobStatus::Running);
        assert_eq!(claimed.id, job.id);
        assert_eq!(claimed.attempts, 1);
        assert_eq!(claimed.worker_id.as_deref(), Some("worker-7"));
        assert!(claimed.started_at.is_some());
        assert!(claimed.run_at <= now() + 5);

        // A running job is not re-claimable.
        assert!(
            claim_next(&pool, "other", now() + 10)
                .await
                .unwrap()
                .is_none()
        );
    }

    #[tokio::test]
    async fn complete_success_marks_succeeded() {
        let pool = test_pool().await;
        let job = enqueue(&pool, &new_job("k"), 3).await.unwrap();
        claim_next(&pool, "w", now()).await.unwrap();

        let t = now();
        complete(&pool, job.id, Ok(()), 30, t).await.unwrap();

        let jobs = list_jobs(&pool, None, 10).await.unwrap();
        let j = job_by_id(&jobs, job.id);
        assert_eq!(j.status, JobStatus::Succeeded);
        assert_eq!(j.finished_at, Some(t));
        assert_eq!(j.last_error, None);
    }

    #[tokio::test]
    async fn complete_failure_retries_with_exponential_backoff() {
        let pool = test_pool().await;
        let job = enqueue(&pool, &new_job("k"), 3).await.unwrap(); // max_attempts = 3

        // Attempt 1 fails → pending, run_at = now + backoff * 2^1.
        let t1 = now();
        claim_next(&pool, "w", t1).await.unwrap();
        complete(&pool, job.id, Err("e1".into()), 10, t1)
            .await
            .unwrap();

        let jobs = list_jobs(&pool, None, 10).await.unwrap();
        let j = job_by_id(&jobs, job.id);
        assert_eq!(j.status, JobStatus::Pending);
        assert_eq!(j.attempts, 1);
        assert_eq!(j.run_at, t1 + 20); // 10 * 2^1
        assert_eq!(j.last_error.as_deref(), Some("e1"));
        assert_eq!(j.finished_at, None);

        // Attempt 2 fails → delay 10 * 2^2 = 40.
        let t2 = t1 + 20;
        let claimed = claim_next(&pool, "w", t2).await.unwrap().unwrap();
        assert_eq!(claimed.attempts, 2);
        complete(&pool, job.id, Err("e2".into()), 10, t2)
            .await
            .unwrap();

        let jobs = list_jobs(&pool, None, 10).await.unwrap();
        let j = job_by_id(&jobs, job.id);
        assert_eq!(j.status, JobStatus::Pending);
        assert_eq!(j.run_at, t2 + 40);
        assert_eq!(j.last_error.as_deref(), Some("e2"));

        // Attempt 3 (final) fails → failed, terminal.
        let t3 = t2 + 40;
        claim_next(&pool, "w", t3).await.unwrap();
        complete(&pool, job.id, Err("e3".into()), 10, t3)
            .await
            .unwrap();

        let jobs = list_jobs(&pool, None, 10).await.unwrap();
        let j = job_by_id(&jobs, job.id);
        assert_eq!(j.status, JobStatus::Failed);
        assert_eq!(j.finished_at, Some(t3));
        assert_eq!(j.last_error.as_deref(), Some("e3"));
    }

    #[tokio::test]
    async fn complete_failure_backoff_is_capped() {
        let pool = test_pool().await;
        // Force attempts high via direct update, then complete once.
        let job = enqueue(&pool, &new_job("k"), 50).await.unwrap();
        sqlx::query("UPDATE jobs_tbl SET attempts = 30 WHERE id = ?")
            .bind(job.id)
            .execute(&pool)
            .await
            .unwrap();
        let t = now();
        complete(&pool, job.id, Err("boom".into()), u64::MAX / 2, t)
            .await
            .unwrap();

        let jobs = list_jobs(&pool, None, 10).await.unwrap();
        let j = job_by_id(&jobs, job.id);
        // delay capped at MAX_BACKOFF_SECS (3600), saturating math.
        assert_eq!(j.run_at, t + 3600);
        assert_eq!(j.status, JobStatus::Pending);
    }

    #[tokio::test]
    async fn cancel_job_semantics() {
        let pool = test_pool().await;
        let pending = enqueue(&pool, &new_job("p"), 3).await.unwrap();
        let running = enqueue(&pool, &new_job("r"), 3).await.unwrap();
        let done = enqueue(&pool, &new_job("d"), 3).await.unwrap();
        let queued = enqueue(&pool, &new_job("q"), 3).await.unwrap();
        claim_next(&pool, "w", now()).await.unwrap(); // claims `pending` (lowest id)

        // Make `running` actually running and `done` succeeded via SQL.
        sqlx::query("UPDATE jobs_tbl SET status = 'running', worker_id = 'w' WHERE id = ?")
            .bind(running.id)
            .execute(&pool)
            .await
            .unwrap();
        sqlx::query("UPDATE jobs_tbl SET status = 'succeeded', finished_at = ? WHERE id = ?")
            .bind(now())
            .bind(done.id)
            .execute(&pool)
            .await
            .unwrap();

        assert!(cancel_job(&pool, pending.id).await.unwrap()); // was claimed → running
        assert!(cancel_job(&pool, running.id).await.unwrap());
        assert!(cancel_job(&pool, queued.id).await.unwrap()); // still pending
        assert!(!cancel_job(&pool, done.id).await.unwrap()); // terminal
        assert!(!cancel_job(&pool, 999_999).await.unwrap()); // missing
        // Already-cancelled is no longer pending/running.
        assert!(!cancel_job(&pool, pending.id).await.unwrap());

        let jobs = list_jobs(&pool, None, 10).await.unwrap();
        assert_eq!(job_by_id(&jobs, pending.id).status, JobStatus::Cancelled);
        assert_eq!(job_by_id(&jobs, running.id).status, JobStatus::Cancelled);
        assert_eq!(job_by_id(&jobs, queued.id).status, JobStatus::Cancelled);
        assert!(job_by_id(&jobs, pending.id).finished_at.is_some());
    }

    #[tokio::test]
    async fn retry_job_only_resets_failed() {
        let pool = test_pool().await;
        let failed = enqueue(&pool, &new_job("f"), 3).await.unwrap();
        let pending = enqueue(&pool, &new_job("p"), 3).await.unwrap();
        let succeeded = enqueue(&pool, &new_job("s"), 3).await.unwrap();

        sqlx::query(
            "UPDATE jobs_tbl SET status = 'failed', attempts = 3,
             finished_at = ?, last_error = 'x', worker_id = 'w' WHERE id = ?",
        )
        .bind(now())
        .bind(failed.id)
        .execute(&pool)
        .await
        .unwrap();
        sqlx::query("UPDATE jobs_tbl SET status = 'succeeded', finished_at = ? WHERE id = ?")
            .bind(now())
            .bind(succeeded.id)
            .execute(&pool)
            .await
            .unwrap();

        assert!(retry_job(&pool, failed.id).await.unwrap());
        assert!(!retry_job(&pool, pending.id).await.unwrap());
        assert!(!retry_job(&pool, succeeded.id).await.unwrap());
        assert!(!retry_job(&pool, 999_999).await.unwrap());

        let jobs = list_jobs(&pool, None, 10).await.unwrap();
        let j = job_by_id(&jobs, failed.id);
        assert_eq!(j.status, JobStatus::Pending);
        assert_eq!(j.attempts, 0);
        assert_eq!(j.last_error, None);
        assert_eq!(j.worker_id, None);
        assert_eq!(j.finished_at, None);
        assert_eq!(j.started_at, None);
        assert!(j.run_at <= now()); // runnable immediately
    }

    #[tokio::test]
    async fn skip_job_marks_non_terminal_with_reason() {
        let pool = test_pool().await;
        let job = enqueue(&pool, &new_job("k"), 3).await.unwrap();

        assert!(
            skip_job(&pool, job.id, Some("already current"))
                .await
                .unwrap()
        );
        assert!(!skip_job(&pool, job.id, Some("again")).await.unwrap()); // terminal now

        let jobs = list_jobs(&pool, None, 10).await.unwrap();
        let j = job_by_id(&jobs, job.id);
        assert_eq!(j.status, JobStatus::Skipped);
        assert_eq!(j.last_error.as_deref(), Some("already current"));
        assert!(j.finished_at.is_some());
    }

    #[tokio::test]
    async fn report_progress_only_on_running_job() {
        let pool = test_pool().await;
        let job = enqueue(&pool, &new_job("k"), 3).await.unwrap();

        // Pending → rejected.
        assert!(
            !report_progress(&pool, job.id, &json!({"pct": 1}))
                .await
                .unwrap()
        );

        claim_next(&pool, "w", now()).await.unwrap();
        assert!(
            report_progress(&pool, job.id, &json!({"pct": 42}))
                .await
                .unwrap()
        );

        // Succeeded → rejected again.
        complete(&pool, job.id, Ok(()), 0, now()).await.unwrap();
        assert!(
            !report_progress(&pool, job.id, &json!({"pct": 99}))
                .await
                .unwrap()
        );

        let jobs = list_jobs(&pool, None, 10).await.unwrap();
        assert_eq!(job_by_id(&jobs, job.id).progress, Some(json!({"pct": 42})));
    }

    #[tokio::test]
    async fn requeue_running_clears_execution_state() {
        let pool = test_pool().await;
        let a = enqueue(&pool, &new_job("a"), 3).await.unwrap();
        let b = enqueue(&pool, &new_job("b"), 3).await.unwrap();
        let pending_only = enqueue(&pool, &new_job("c"), 3).await.unwrap();

        // Claim a and b so both are running; leave c pending.
        claim_next(&pool, "w1", now()).await.unwrap();
        claim_next(&pool, "w2", now()).await.unwrap();

        assert_eq!(requeue_running(&pool).await.unwrap(), 2);

        let jobs = list_jobs(&pool, None, 10).await.unwrap();
        for id in [a.id, b.id] {
            let j = job_by_id(&jobs, id);
            assert_eq!(j.status, JobStatus::Pending);
            assert_eq!(j.worker_id, None);
            assert_eq!(j.started_at, None);
            assert_eq!(
                j.last_error.as_deref(),
                Some("recovered: interrupted by shutdown")
            );
        }
        assert_eq!(job_by_id(&jobs, pending_only.id).status, JobStatus::Pending);
        // Idempotent: nothing left running.
        assert_eq!(requeue_running(&pool).await.unwrap(), 0);
    }

    #[tokio::test]
    async fn list_jobs_filters_by_status_orders_desc_and_limits() {
        let pool = test_pool().await;
        let j1 = enqueue(&pool, &new_job("k"), 3).await.unwrap();
        let j2 = enqueue(&pool, &new_job("k"), 3).await.unwrap();
        let j3 = enqueue(&pool, &new_job("k"), 3).await.unwrap();
        // j1 succeeds, j2 fails, j3 stays pending.
        claim_next(&pool, "w", now()).await.unwrap(); // j1
        complete(&pool, j1.id, Ok(()), 0, now()).await.unwrap();
        claim_next(&pool, "w", now()).await.unwrap(); // j2
        // Exhaust j2's attempts directly for a terminal failed state.
        sqlx::query("UPDATE jobs_tbl SET attempts = max_attempts WHERE id = ?")
            .bind(j2.id)
            .execute(&pool)
            .await
            .unwrap();
        complete(&pool, j2.id, Err("x".into()), 0, now())
            .await
            .unwrap();

        let all = list_jobs(&pool, None, 10).await.unwrap();
        assert_eq!(all.len(), 3);
        // Newest (highest id) first.
        assert_eq!(all[0].id, j3.id);
        assert_eq!(all[2].id, j1.id);

        let pending = list_jobs(&pool, Some(JobStatus::Pending), 10)
            .await
            .unwrap();
        assert_eq!(pending.len(), 1);
        assert_eq!(pending[0].id, j3.id);

        let succeeded = list_jobs(&pool, Some(JobStatus::Succeeded), 10)
            .await
            .unwrap();
        assert_eq!(succeeded.len(), 1);
        assert_eq!(succeeded[0].id, j1.id);

        // Limit truncates after ordering.
        let limited = list_jobs(&pool, None, 2).await.unwrap();
        assert_eq!(limited.len(), 2);
        assert_eq!(limited[0].id, j3.id);
    }

    #[tokio::test]
    async fn batch_progress_and_cancel_batch_at_store_level() {
        let pool = test_pool().await;
        let jobs = vec![new_job("b"), new_job("b")];
        let inserted = enqueue_batch(&pool, &jobs, 3).await.unwrap();
        let batch_id = inserted[0].id;

        // Progress before anything runs.
        let p = batch_progress(&pool, batch_id).await.unwrap().unwrap();
        assert_eq!(p.items_total, 2);
        assert_eq!(p.items_pending, 2);
        assert!(!p.is_complete());

        // Finish one, cancel the batch → other becomes cancelled.
        claim_next(&pool, "w", now()).await.unwrap();
        complete(&pool, inserted[0].id, Ok(()), 0, now())
            .await
            .unwrap();

        assert_eq!(cancel_batch(&pool, batch_id).await.unwrap(), 1);

        let p = batch_progress(&pool, batch_id).await.unwrap().unwrap();
        assert_eq!(p.items_done, 2);
        assert_eq!(p.items_succeeded, 1);
        assert_eq!(p.items_cancelled, 1);
        assert!(p.is_complete());

        // batch_jobs lists members in enqueue order.
        let members = batch_jobs(&pool, batch_id).await.unwrap();
        assert_eq!(members.len(), 2);
        assert!(members[0].id < members[1].id);

        // Unknown batch → None / empty.
        assert!(
            batch_progress(&pool, batch_id + 999)
                .await
                .unwrap()
                .is_none()
        );
        assert!(batch_jobs(&pool, batch_id + 999).await.unwrap().is_empty());
    }
}
