//! Generic job queue backed by `jobs_tbl`.
//!
//! Jobs track long-running document operations (imports, exports,
//! conversions, bulk edits). The table stores the job envelope only —
//! the `job_type`/`payload`/`result` JSON shapes are defined by the
//! subsystems that enqueue them.

use serde::{Deserialize, Serialize};
use serde_json::Value;
use sqlx::{Row, SqlitePool};

use super::{StoreResult, enum_parse, enum_str, gen_id, now_rfc3339};

/// Lifecycle status of a [`Job`].
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum JobStatus {
    /// Waiting for a worker to pick it up.
    Pending,
    /// Claimed by a worker, in progress.
    Running,
    /// Finished successfully; `result` holds the outcome.
    Completed,
    /// Finished with an error; `error` holds the message.
    Failed,
}

/// A row from `jobs_tbl`.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Job {
    pub id: String,
    /// Operation discriminator, e.g. `"import_documents"`.
    pub job_type: String,
    /// JSON input for the operation.
    pub payload: Option<Value>,
    pub status: JobStatus,
    /// JSON outcome, set on completion.
    pub result: Option<Value>,
    /// Failure message, set on failure.
    pub error: Option<String>,
    /// How many times a worker has claimed this job.
    pub attempts: i64,
    /// RFC 3339 timestamps.
    pub created_at: String,
    pub started_at: Option<String>,
    pub finished_at: Option<String>,
}

const JOB_COLS: &str =
    "id, job_type, payload, status, result, error, attempts, created_at, started_at, finished_at";

fn job_from_row(row: &sqlx::sqlite::SqliteRow) -> StoreResult<Job> {
    let payload: Option<String> = row.get("payload");
    let result: Option<String> = row.get("result");
    Ok(Job {
        id: row.get("id"),
        job_type: row.get("job_type"),
        payload: payload
            .map(|p| serde_json::from_str(&p))
            .transpose()?,
        status: enum_parse(&row.get::<String, _>("status"))?,
        result: result.map(|r| serde_json::from_str(&r)).transpose()?,
        error: row.get("error"),
        attempts: row.get("attempts"),
        created_at: row.get("created_at"),
        started_at: row.get("started_at"),
        finished_at: row.get("finished_at"),
    })
}

/// Enqueues a new job and returns its id.
pub async fn enqueue_job(
    pool: &SqlitePool,
    job_type: &str,
    payload: Option<&Value>,
) -> StoreResult<String> {
    let id = gen_id("job");
    let now = now_rfc3339();
    sqlx::query(
        "INSERT INTO jobs_tbl (id, job_type, payload, status, created_at)
         VALUES (?, ?, ?, 'pending', ?)",
    )
    .bind(&id)
    .bind(job_type)
    .bind(payload.map(|p| p.to_string()))
    .bind(&now)
    .execute(pool)
    .await?;
    Ok(id)
}

/// Fetches a job by id. Returns `None` when no such job exists.
pub async fn get_job(pool: &SqlitePool, id: &str) -> StoreResult<Option<Job>> {
    let row = sqlx::query(&format!("SELECT {JOB_COLS} FROM jobs_tbl WHERE id = ?"))
        .bind(id)
        .fetch_optional(pool)
        .await?;
    row.map(|r| job_from_row(&r)).transpose()
}

/// Lists jobs, optionally filtered by status, oldest first.
pub async fn list_jobs(
    pool: &SqlitePool,
    status: Option<JobStatus>,
    limit: i64,
) -> StoreResult<Vec<Job>> {
    let mut qb = sqlx::QueryBuilder::<sqlx::Sqlite>::new(format!(
        "SELECT {JOB_COLS} FROM jobs_tbl"
    ));
    if let Some(status) = status {
        qb.push(" WHERE status = ").push_bind(enum_str(&status)?);
    }
    qb.push(" ORDER BY created_at LIMIT ").push_bind(limit);
    let rows = qb.build().fetch_all(pool).await?;
    rows.iter().map(job_from_row).collect()
}

/// Claims a pending job for a worker: `pending → running`, bumping
/// `attempts` and stamping `started_at`. Returns `false` if the job is
/// missing or not pending (another worker may have claimed it).
pub async fn start_job(pool: &SqlitePool, id: &str) -> StoreResult<bool> {
    let done = sqlx::query(
        "UPDATE jobs_tbl
         SET status = 'running', attempts = attempts + 1, started_at = ?
         WHERE id = ? AND status = 'pending'",
    )
    .bind(now_rfc3339())
    .bind(id)
    .execute(pool)
    .await?;
    Ok(done.rows_affected() > 0)
}

/// Marks a running job completed and stores its JSON result.
pub async fn complete_job(
    pool: &SqlitePool,
    id: &str,
    result: Option<&Value>,
) -> StoreResult<bool> {
    let done = sqlx::query(
        "UPDATE jobs_tbl
         SET status = 'completed', result = ?, finished_at = ?
         WHERE id = ? AND status = 'running'",
    )
    .bind(result.map(|r| r.to_string()))
    .bind(now_rfc3339())
    .bind(id)
    .execute(pool)
    .await?;
    Ok(done.rows_affected() > 0)
}

/// Marks a running job failed and stores the error message.
pub async fn fail_job(pool: &SqlitePool, id: &str, error: &str) -> StoreResult<bool> {
    let done = sqlx::query(
        "UPDATE jobs_tbl
         SET status = 'failed', error = ?, finished_at = ?
         WHERE id = ? AND status = 'running'",
    )
    .bind(error)
    .bind(now_rfc3339())
    .bind(id)
    .execute(pool)
    .await?;
    Ok(done.rows_affected() > 0)
}
