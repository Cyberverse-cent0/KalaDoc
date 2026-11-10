//! The job engine: claims due jobs from `jobs_tbl` and dispatches them
//! to registered [`JobHandler`]s.
//!
//! Single-process design — one [`JobEngine`] per pool. SQLite's single
//! writer plus the atomic `UPDATE ... RETURNING` claim makes multiple
//! engines safe-ish, but handlers must still be idempotent because a
//! crash between claim and completion leaves the job `running` until
//! [`JobEngine::run`] starts, which immediately requeues all `running`
//! rows via [`JobEngine::recover`] (see also [`JobEngine::reap_stale`]
//! for stale jobs orphaned by a crashed engine).

use std::collections::HashMap;
use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::Duration;

use sqlx::SqlitePool;

use super::JobError;
use super::handler::{JobContext, SharedHandler};
use super::job::{BatchProgress, Job, NewJob};
use super::store;

/// Engine tunables. Built from [`crate::config::AppConfig`] via
/// `to_job_config()` or constructed directly.
#[derive(Debug, Clone)]
pub struct JobEngineConfig {
    /// How often the run loop polls for due jobs.
    pub poll_interval: Duration,
    /// Default `max_attempts` for jobs that don't specify one.
    pub default_max_attempts: u32,
    /// Base seconds for exponential retry backoff
    /// (`base * 2^attempts`, capped at 1 h in `store::complete`).
    pub backoff_base: Duration,
    /// A `running` job older than this is assumed orphaned by a crash
    /// and returned to `pending` on the next reap.
    pub stale_after: Duration,
}

impl Default for JobEngineConfig {
    fn default() -> Self {
        Self {
            poll_interval: Duration::from_secs(2),
            default_max_attempts: 3,
            backoff_base: Duration::from_secs(30),
            stale_after: Duration::from_secs(600),
        }
    }
}

static ENGINE_SEQ: AtomicU64 = AtomicU64::new(0);

/// Dispatches claimed jobs to registered handlers.
pub struct JobEngine {
    pool: SqlitePool,
    config: JobEngineConfig,
    handlers: HashMap<String, SharedHandler>,
    worker_id: String,
    ctx: JobContext,
}

impl JobEngine {
    pub fn new(pool: SqlitePool, config: JobEngineConfig) -> Self {
        let seq = ENGINE_SEQ.fetch_add(1, Ordering::Relaxed);
        Self {
            ctx: JobContext { pool: pool.clone() },
            pool,
            config,
            handlers: HashMap::new(),
            worker_id: format!("worker-{}-{}", std::process::id(), seq),
        }
    }

    /// Registers `handler` for jobs of `kind`. Later registrations for
    /// the same kind replace earlier ones.
    pub fn register(&mut self, kind: impl Into<String>, handler: SharedHandler) {
        self.handlers.insert(kind.into(), handler);
    }

    pub fn worker_id(&self) -> &str {
        &self.worker_id
    }

    /// Enqueues a job. Thin wrapper over [`store::enqueue`] using this
    /// engine's default `max_attempts`.
    pub async fn enqueue(&self, new: &NewJob) -> Result<Job, JobError> {
        store::enqueue(&self.pool, new, self.config.default_max_attempts).await
    }

    /// Enqueues `jobs` as one batch (see [`store::enqueue_batch`]).
    /// Every item is an ordinary job — same kinds, payloads,
    /// priorities, retries — sharing a `batch_id` for aggregate
    /// progress and batch cancellation.
    pub async fn enqueue_batch(&self, jobs: &[NewJob]) -> Result<Vec<Job>, JobError> {
        store::enqueue_batch(&self.pool, jobs, self.config.default_max_attempts).await
    }

    /// Aggregate progress for a batch created by [`JobEngine::enqueue_batch`].
    /// `None` when no jobs carry `batch_id`.
    pub async fn batch_progress(&self, batch_id: i64) -> Result<Option<BatchProgress>, JobError> {
        store::batch_progress(&self.pool, batch_id).await
    }

    /// Every job belonging to `batch_id`, in enqueue order.
    pub async fn batch_jobs(&self, batch_id: i64) -> Result<Vec<Job>, JobError> {
        store::batch_jobs(&self.pool, batch_id).await
    }

    /// Cancels all non-terminal jobs in `batch_id` (whole-batch
    /// cancel). Returns how many jobs were cancelled.
    pub async fn cancel_batch(&self, batch_id: i64) -> Result<u64, JobError> {
        store::cancel_batch(&self.pool, batch_id).await
    }

    /// Claims and executes at most one job. Returns `true` when a job
    /// was processed. This is the unit [`JobEngine::run`] repeats.
    pub async fn run_once(&self) -> Result<bool, JobError> {
        let Some(job) = store::claim_next(&self.pool, &self.worker_id, now_unix()).await? else {
            return Ok(false);
        };

        let result = match self.handlers.get(&job.kind) {
            Some(handler) => {
                // A panicking handler must not take down the whole
                // engine task — convert the panic into a job failure
                // so the job retries (or fails) like any other error.
                handle_catching_panic(handler, &self.ctx, &job).await
            }
            None => Err(JobError::NoHandler(job.kind.clone())),
        };

        let err = result.err().map(|e| e.to_string());
        store::complete(
            &self.pool,
            &job,
            err.map_or(Ok(()), Err),
            self.config.backoff_base.as_secs(),
            now_unix(),
        )
        .await?;

        Ok(true)
    }

    /// Requeues every job still `running`, assuming a previous engine
    /// process died before completing it. Called once at startup by
    /// [`JobEngine::run`]; safe because one engine owns a database.
    /// Returns rows reclaimed.
    pub async fn recover(&self) -> Result<u64, JobError> {
        let reclaimed = store::requeue_running(&self.pool).await?;
        if reclaimed > 0 {
            tracing::info!(reclaimed, "job engine recovered interrupted jobs");
        }
        Ok(reclaimed)
    }

    /// Returns `running` jobs older than `stale_after` to `pending`.
    /// Call periodically (or on startup) to recover from crashes
    /// between claim and completion. `running` rows with a NULL
    /// `started_at` (e.g. written by an older schema or a manual
    /// update) have no age to compare, so they are treated as stale.
    /// Returns rows reclaimed.
    pub async fn reap_stale(&self) -> Result<u64, JobError> {
        let cutoff = now_unix() - self.config.stale_after.as_secs() as i64;
        let done = sqlx::query(
            "UPDATE jobs_tbl
             SET status = 'pending', worker_id = NULL, started_at = NULL,
                 finished_at = NULL, run_at = ?, progress_json = NULL,
                 last_error = 'reaped: stale running job'
             WHERE status = 'running'
               AND (started_at IS NULL OR started_at < ?)",
        )
        .bind(now_unix())
        .bind(cutoff)
        .execute(&self.pool)
        .await?;
        Ok(done.rows_affected())
    }

    /// Poll loop: first recovers jobs orphaned by a previous engine
    /// process, then repeatedly reaps stale jobs and drains due work,
    /// sleeping `poll_interval` between empty polls. Runs forever;
    /// spawn it and abort on shutdown.
    pub async fn run(&self) {
        if let Err(e) = self.recover().await {
            tracing::warn!(error = %e, "job engine startup recovery failed; will continue");
        }
        let mut ticker = tokio::time::interval(self.config.poll_interval);
        ticker.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);
        loop {
            ticker.tick().await;
            if let Err(e) = self.reap_stale().await {
                tracing::warn!(error = %e, "job engine reap failed; will retry");
            }
            loop {
                match self.run_once().await {
                    Ok(true) => {}
                    Ok(false) => break,
                    Err(e) => {
                        tracing::warn!(error = %e, "job dispatch failed; will retry");
                        break;
                    }
                }
            }
        }
    }

    /// Spawns [`JobEngine::run`] on the Tokio runtime.
    pub fn spawn(self: Arc<Self>) -> tokio::task::JoinHandle<()> {
        tokio::spawn(async move { self.run().await })
    }
}

/// Drives `handler.handle(...)` to completion, turning a panic during
/// any poll into `Err(JobError::Failed)` instead of unwinding through
/// the engine's run loop.
async fn handle_catching_panic(
    handler: &SharedHandler,
    ctx: &JobContext,
    job: &Job,
) -> Result<(), JobError> {
    let mut fut = handler.handle(ctx, job);
    std::future::poll_fn(|cx| {
        match std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
            std::future::Future::poll(fut.as_mut(), cx)
        })) {
            Ok(std::task::Poll::Ready(res)) => std::task::Poll::Ready(res),
            Ok(std::task::Poll::Pending) => std::task::Poll::Pending,
            Err(payload) => std::task::Poll::Ready(Err(JobError::Failed(format!(
                "handler panicked: {}",
                panic_message(&*payload)
            )))),
        }
    })
    .await
}

/// Extracts a readable message from a panic payload (`String` or
/// `&'static str` are the usual shapes; anything else is opaque).
fn panic_message(payload: &(dyn std::any::Any + Send)) -> String {
    if let Some(s) = payload.downcast_ref::<&str>() {
        (*s).to_string()
    } else if let Some(s) = payload.downcast_ref::<String>() {
        s.clone()
    } else {
        "unknown panic payload".to_string()
    }
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
    use crate::crates::job_engine::JobHandler;
    use crate::crates::job_engine::job::JobStatus;
    use serde_json::json;
    use sqlx::sqlite::{SqliteConnectOptions, SqlitePoolOptions};
    use std::collections::HashMap;
    use std::future::Future;
    use std::pin::Pin;
    use std::sync::Mutex;
    use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering as AtomicOrdering};
    use std::time::Instant;

    async fn test_pool() -> SqlitePool {
        let pool = SqlitePoolOptions::new()
            .max_connections(1)
            .connect("sqlite::memory:")
            .await
            .unwrap();
        store::create_jobs_table(&pool).await.unwrap();
        pool
    }

    struct CountingHandler {
        calls: AtomicUsize,
        fail_times: usize,
    }

    impl JobHandler for CountingHandler {
        fn handle<'a>(
            &'a self,
            _ctx: &'a JobContext,
            _job: &'a Job,
        ) -> Pin<Box<dyn Future<Output = Result<(), JobError>> + Send + 'a>> {
            Box::pin(async move {
                let n = self.calls.fetch_add(1, AtomicOrdering::SeqCst);
                if n < self.fail_times {
                    Err(JobError::Failed("boom".into()))
                } else {
                    Ok(())
                }
            })
        }
    }

    fn engine(pool: SqlitePool) -> JobEngine {
        JobEngine::new(
            pool,
            JobEngineConfig {
                poll_interval: Duration::from_millis(10),
                default_max_attempts: 3,
                backoff_base: Duration::from_secs(0),
                stale_after: Duration::from_secs(60),
            },
        )
    }

    #[tokio::test]
    async fn enqueue_run_once_succeeds() {
        let pool = test_pool().await;
        let mut eng = engine(pool.clone());
        eng.register(
            "test.ping",
            Arc::new(CountingHandler {
                calls: AtomicUsize::new(0),
                fail_times: 0,
            }),
        );

        let job = eng
            .enqueue(&NewJob {
                kind: "test.ping".into(),
                payload: json!({"n": 1}),
                priority: 0,
                run_at: None,
                max_attempts: None,
            })
            .await
            .unwrap();
        assert_eq!(job.status, JobStatus::Pending);

        assert!(eng.run_once().await.unwrap());
        assert!(!eng.run_once().await.unwrap()); // queue drained

        let jobs = store::list_jobs(&pool, None, 10).await.unwrap();
        assert_eq!(jobs[0].status, JobStatus::Succeeded);
        assert_eq!(jobs[0].attempts, 1);
    }

    #[tokio::test]
    async fn failing_job_retries_then_fails() {
        let pool = test_pool().await;
        let mut eng = engine(pool.clone());
        eng.register(
            "test.fail",
            Arc::new(CountingHandler {
                calls: AtomicUsize::new(0),
                fail_times: usize::MAX,
            }),
        );

        eng.enqueue(&NewJob {
            kind: "test.fail".into(),
            payload: json!({}),
            priority: 0,
            run_at: None,
            max_attempts: Some(2),
        })
        .await
        .unwrap();

        // backoff_base = 0 → run_at stays ~now, retries are immediate.
        assert!(eng.run_once().await.unwrap());
        let jobs = store::list_jobs(&pool, None, 10).await.unwrap();
        assert_eq!(jobs[0].status, JobStatus::Pending);
        assert_eq!(jobs[0].attempts, 1);
        assert_eq!(jobs[0].last_error.as_deref(), Some("boom"));

        assert!(eng.run_once().await.unwrap());
        let jobs = store::list_jobs(&pool, None, 10).await.unwrap();
        assert_eq!(jobs[0].status, JobStatus::Failed);
        assert_eq!(jobs[0].attempts, 2);
    }

    #[tokio::test]
    async fn unregistered_kind_fails_without_handler() {
        let pool = test_pool().await;
        let eng = engine(pool.clone());
        eng.enqueue(&NewJob {
            kind: "no.such".into(),
            payload: json!({}),
            priority: 0,
            run_at: None,
            max_attempts: Some(1),
        })
        .await
        .unwrap();

        assert!(eng.run_once().await.unwrap());
        let jobs = store::list_jobs(&pool, None, 10).await.unwrap();
        assert_eq!(jobs[0].status, JobStatus::Failed);
        assert!(
            jobs[0]
                .last_error
                .as_deref()
                .unwrap()
                .contains("no handler")
        );
    }

    struct PanickingHandler;

    impl JobHandler for PanickingHandler {
        fn handle<'a>(
            &'a self,
            _ctx: &'a JobContext,
            _job: &'a Job,
        ) -> Pin<Box<dyn Future<Output = Result<(), JobError>> + Send + 'a>> {
            Box::pin(async move { panic!("handler exploded") })
        }
    }

    #[tokio::test]
    async fn panicking_handler_fails_job_without_killing_engine() {
        let pool = test_pool().await;
        let mut eng = engine(pool.clone());
        eng.register("test.panic", Arc::new(PanickingHandler));

        eng.enqueue(&NewJob {
            kind: "test.panic".into(),
            payload: json!({}),
            priority: 0,
            run_at: None,
            max_attempts: Some(1),
        })
        .await
        .unwrap();

        // The panic is caught per-poll and recorded as a job failure;
        // the engine keeps running and can process the next job.
        assert!(eng.run_once().await.unwrap());
        let jobs = store::list_jobs(&pool, None, 10).await.unwrap();
        assert_eq!(jobs[0].status, JobStatus::Failed);
        assert!(
            jobs[0]
                .last_error
                .as_deref()
                .unwrap()
                .contains("handler panicked")
        );

        // Engine still works afterwards.
        eng.register(
            "test.after",
            Arc::new(CountingHandler {
                calls: AtomicUsize::new(0),
                fail_times: 0,
            }),
        );
        eng.enqueue(&NewJob {
            kind: "test.after".into(),
            payload: json!({}),
            priority: 0,
            run_at: None,
            max_attempts: None,
        })
        .await
        .unwrap();
        assert!(eng.run_once().await.unwrap());
    }

    #[tokio::test]
    async fn delayed_job_not_claimed_early() {
        let pool = test_pool().await;
        let eng = engine(pool.clone());
        eng.enqueue(&NewJob {
            kind: "later".into(),
            payload: json!({}),
            priority: 0,
            run_at: Some(now_unix() + 3600),
            max_attempts: None,
        })
        .await
        .unwrap();

        assert!(!eng.run_once().await.unwrap());
    }

    #[tokio::test]
    async fn requeue_running_returns_job_to_pending() {
        let pool = test_pool().await;
        let eng = engine(pool.clone());
        let job = eng
            .enqueue(&NewJob {
                kind: "x".into(),
                payload: json!({}),
                priority: 0,
                run_at: None,
                max_attempts: None,
            })
            .await
            .unwrap();

        // Simulate a dead worker mid-flight.
        sqlx::query(
            "UPDATE jobs_tbl SET status = 'running', worker_id = 'dead-worker',
             started_at = ?, attempts = 1, last_error = 'boom'
             WHERE id = ?",
        )
        .bind(now_unix())
        .bind(job.id)
        .execute(&pool)
        .await
        .unwrap();

        assert_eq!(store::requeue_running(&pool).await.unwrap(), 1);
        let jobs = store::list_jobs(&pool, None, 10).await.unwrap();
        assert_eq!(jobs[0].status, JobStatus::Pending);
        assert_eq!(jobs[0].worker_id, None);
        assert_eq!(jobs[0].started_at, None);
        assert_eq!(jobs[0].finished_at, None);
        assert_eq!(
            jobs[0].last_error.as_deref(),
            Some("recovered: interrupted by shutdown")
        );
    }

    #[tokio::test]
    async fn cancel_job_cancels_pending_but_not_succeeded() {
        let pool = test_pool().await;
        let eng = engine(pool.clone());
        let pending = eng
            .enqueue(&NewJob {
                kind: "x".into(),
                payload: json!({}),
                priority: 0,
                run_at: None,
                max_attempts: None,
            })
            .await
            .unwrap();
        let done = eng
            .enqueue(&NewJob {
                kind: "x".into(),
                payload: json!({}),
                priority: 0,
                run_at: None,
                max_attempts: None,
            })
            .await
            .unwrap();
        sqlx::query("UPDATE jobs_tbl SET status = 'succeeded', finished_at = ? WHERE id = ?")
            .bind(now_unix())
            .bind(done.id)
            .execute(&pool)
            .await
            .unwrap();

        assert!(store::cancel_job(&pool, pending.id).await.unwrap());
        assert!(!store::cancel_job(&pool, done.id).await.unwrap());

        let jobs = store::list_jobs(&pool, None, 10).await.unwrap();
        let by_id = |id: i64| jobs.iter().find(|j| j.id == id).unwrap().status;
        assert_eq!(by_id(pending.id), JobStatus::Cancelled);
        assert_eq!(by_id(done.id), JobStatus::Succeeded);
    }

    #[tokio::test]
    async fn retry_job_resets_failed_but_not_pending() {
        let pool = test_pool().await;
        let eng = engine(pool.clone());
        let failed = eng
            .enqueue(&NewJob {
                kind: "x".into(),
                payload: json!({}),
                priority: 0,
                run_at: None,
                max_attempts: None,
            })
            .await
            .unwrap();
        let pending = eng
            .enqueue(&NewJob {
                kind: "x".into(),
                payload: json!({}),
                priority: 0,
                run_at: None,
                max_attempts: None,
            })
            .await
            .unwrap();
        sqlx::query(
            "UPDATE jobs_tbl SET status = 'failed', attempts = 3,
             finished_at = ?, last_error = 'boom' WHERE id = ?",
        )
        .bind(now_unix())
        .bind(failed.id)
        .execute(&pool)
        .await
        .unwrap();

        assert!(store::retry_job(&pool, failed.id).await.unwrap());
        assert!(!store::retry_job(&pool, pending.id).await.unwrap());

        let jobs = store::list_jobs(&pool, None, 10).await.unwrap();
        let job = jobs.iter().find(|j| j.id == failed.id).unwrap();
        assert_eq!(job.status, JobStatus::Pending);
        assert_eq!(job.attempts, 0);
        assert_eq!(job.last_error, None);
        assert_eq!(job.finished_at, None);
        assert_eq!(job.worker_id, None);
    }

    #[tokio::test]
    async fn recover_requeues_running_job() {
        let pool = test_pool().await;
        let eng = engine(pool.clone());
        let job = eng
            .enqueue(&NewJob {
                kind: "x".into(),
                payload: json!({}),
                priority: 0,
                run_at: None,
                max_attempts: None,
            })
            .await
            .unwrap();
        // Simulate an engine that died after claiming the job.
        sqlx::query(
            "UPDATE jobs_tbl SET status = 'running', worker_id = 'old-worker',
             started_at = ? WHERE id = ?",
        )
        .bind(now_unix())
        .bind(job.id)
        .execute(&pool)
        .await
        .unwrap();

        assert_eq!(eng.recover().await.unwrap(), 1);
        let jobs = store::list_jobs(&pool, None, 10).await.unwrap();
        assert_eq!(jobs[0].status, JobStatus::Pending);
        assert_eq!(jobs[0].worker_id, None);
        assert_eq!(jobs[0].started_at, None);

        // A second recover finds nothing.
        assert_eq!(eng.recover().await.unwrap(), 0);
    }

    /// Marks `job_id` as `running` with the given `started_at` (Unix
    /// seconds; `None` leaves the column NULL).
    async fn mark_running(pool: &SqlitePool, job_id: i64, started_at: Option<i64>) {
        sqlx::query(
            "UPDATE jobs_tbl SET status = 'running', worker_id = 'dead-worker',
             started_at = ?, attempts = 1 WHERE id = ?",
        )
        .bind(started_at)
        .bind(job_id)
        .execute(pool)
        .await
        .unwrap();
    }

    async fn enqueue_test_job(eng: &JobEngine) -> Job {
        eng.enqueue(&NewJob {
            kind: "x".into(),
            payload: json!({}),
            priority: 0,
            run_at: None,
            max_attempts: None,
        })
        .await
        .unwrap()
    }

    #[tokio::test]
    async fn reap_stale_requeues_only_jobs_older_than_stale_after() {
        let pool = test_pool().await;
        let eng = engine(pool.clone()); // stale_after = 60s in test config

        let stale = enqueue_test_job(&eng).await;
        let fresh = enqueue_test_job(&eng).await;
        // Engine test config uses stale_after = 60s.
        mark_running(&pool, stale.id, Some(now_unix() - 120)).await;
        mark_running(&pool, fresh.id, Some(now_unix())).await;

        assert_eq!(eng.reap_stale().await.unwrap(), 1);

        let jobs = store::list_jobs(&pool, None, 10).await.unwrap();
        let by_id = |id: i64| jobs.iter().find(|j| j.id == id).unwrap();

        let stale_job = by_id(stale.id);
        assert_eq!(stale_job.status, JobStatus::Pending);
        assert_eq!(stale_job.worker_id, None);
        assert_eq!(stale_job.started_at, None);
        assert_eq!(
            stale_job.last_error.as_deref(),
            Some("reaped: stale running job")
        );
        // Reclaimed jobs are runnable immediately.
        assert!(stale_job.run_at <= now_unix());

        // A job still within the stale window keeps running — it may
        // belong to a live peer engine.
        assert_eq!(by_id(fresh.id).status, JobStatus::Running);
    }

    #[tokio::test]
    async fn reap_stale_requeues_running_job_without_started_at() {
        let pool = test_pool().await;
        let eng = engine(pool.clone());
        let job = enqueue_test_job(&eng).await;
        // No started_at — no age to compare, so it must not be stuck
        // in `running` forever.
        mark_running(&pool, job.id, None).await;

        assert_eq!(eng.reap_stale().await.unwrap(), 1);
        let jobs = store::list_jobs(&pool, None, 10).await.unwrap();
        assert_eq!(jobs[0].status, JobStatus::Pending);
    }

    #[tokio::test]
    async fn reap_stale_leaves_terminal_and_pending_jobs_alone() {
        let pool = test_pool().await;
        let eng = engine(pool.clone());
        let pending = enqueue_test_job(&eng).await;
        let done = enqueue_test_job(&eng).await;
        sqlx::query("UPDATE jobs_tbl SET status = 'succeeded', finished_at = ? WHERE id = ?")
            .bind(now_unix())
            .bind(done.id)
            .execute(&pool)
            .await
            .unwrap();

        assert_eq!(eng.reap_stale().await.unwrap(), 0);
        let jobs = store::list_jobs(&pool, None, 10).await.unwrap();
        let by_id = |id: i64| jobs.iter().find(|j| j.id == id).unwrap().status;
        assert_eq!(by_id(pending.id), JobStatus::Pending);
        assert_eq!(by_id(done.id), JobStatus::Succeeded);
    }

    #[tokio::test]
    async fn recover_then_run_once_executes_recovered_job() {
        // End-to-end crash recovery: a job left `running` by a dead
        // worker is requeued by recover() and then actually executes.
        let pool = test_pool().await;
        let mut eng = engine(pool.clone());
        let calls = Arc::new(AtomicUsize::new(0));
        struct Spy {
            calls: Arc<AtomicUsize>,
        }
        impl JobHandler for Spy {
            fn handle<'a>(
                &'a self,
                _ctx: &'a JobContext,
                _job: &'a Job,
            ) -> Pin<Box<dyn Future<Output = Result<(), JobError>> + Send + 'a>> {
                Box::pin(async move {
                    self.calls.fetch_add(1, AtomicOrdering::SeqCst);
                    Ok(())
                })
            }
        }
        eng.register(
            "test.spy",
            Arc::new(Spy {
                calls: calls.clone(),
            }),
        );

        let job = eng
            .enqueue(&NewJob {
                kind: "test.spy".into(),
                payload: json!({}),
                priority: 0,
                run_at: None,
                max_attempts: None,
            })
            .await
            .unwrap();
        // Simulate crash after claim, before completion.
        mark_running(&pool, job.id, Some(now_unix())).await;

        assert_eq!(eng.recover().await.unwrap(), 1);
        assert!(eng.run_once().await.unwrap());
        assert_eq!(calls.load(AtomicOrdering::SeqCst), 1);
        let jobs = store::list_jobs(&pool, None, 10).await.unwrap();
        assert_eq!(jobs[0].status, JobStatus::Succeeded);
    }

    #[tokio::test]
    async fn recover_with_fresh_engine_runs_job_to_succeeded() {
        // Issue #1 scenario: engine A claims a job then "crashes";
        // a brand-new engine B starts, recovers the orphaned job,
        // and runs it to completion.
        let pool = test_pool().await;
        let eng_a = engine(pool.clone());
        let job = eng_a
            .enqueue(&NewJob {
                kind: "x".into(),
                payload: json!({}),
                priority: 0,
                run_at: None,
                max_attempts: None,
            })
            .await
            .unwrap();
        // Simulate crash after claim: running with a dead worker_id.
        mark_running(&pool, job.id, Some(now_unix())).await;

        // Engine B is a different worker_id; its recover() must pick
        // up the orphan regardless of which worker claimed it.
        let mut eng_b = engine(pool.clone());
        let calls = Arc::new(AtomicUsize::new(0));
        struct Spy {
            calls: Arc<AtomicUsize>,
        }
        impl JobHandler for Spy {
            fn handle<'a>(
                &'a self,
                _ctx: &'a JobContext,
                _job: &'a Job,
            ) -> Pin<Box<dyn Future<Output = Result<(), JobError>> + Send + 'a>> {
                Box::pin(async move {
                    self.calls.fetch_add(1, AtomicOrdering::SeqCst);
                    Ok(())
                })
            }
        }
        eng_b.register(
            "x",
            Arc::new(Spy {
                calls: calls.clone(),
            }),
        );

        assert_eq!(eng_b.recover().await.unwrap(), 1);
        assert!(eng_b.run_once().await.unwrap());
        assert_eq!(calls.load(AtomicOrdering::SeqCst), 1);
        let jobs = store::list_jobs(&pool, None, 10).await.unwrap();
        assert_eq!(jobs[0].status, JobStatus::Succeeded);
        assert_eq!(jobs[0].worker_id.as_deref(), Some(eng_b.worker_id()));
    }

    #[tokio::test]
    async fn complete_after_cancel_keeps_cancelled() {
        let pool = test_pool().await;
        let eng = engine(pool.clone());

        // Ok path: claim → cancel mid-flight → complete(Ok) must not
        // resurrect the job to `succeeded`.
        let job_ok = enqueue_test_job(&eng).await;
        let claimed_ok = store::claim_next(&pool, eng.worker_id(), now_unix())
            .await
            .unwrap()
            .unwrap();
        assert_eq!(claimed_ok.id, job_ok.id);
        assert!(store::cancel_job(&pool, job_ok.id).await.unwrap());
        store::complete(&pool, &claimed_ok, Ok(()), 0, now_unix())
            .await
            .unwrap();

        // Err path: same race on the failure branch must not flip
        // `cancelled` back to `pending`/`failed` either.
        let job_err = enqueue_test_job(&eng).await;
        let claimed_err = store::claim_next(&pool, eng.worker_id(), now_unix())
            .await
            .unwrap()
            .unwrap();
        assert_eq!(claimed_err.id, job_err.id);
        assert!(store::cancel_job(&pool, job_err.id).await.unwrap());
        store::complete(
            &pool,
            &claimed_err,
            Err("late failure".into()),
            0,
            now_unix(),
        )
        .await
        .unwrap();

        let jobs = store::list_jobs(&pool, None, 10).await.unwrap();
        for id in [job_ok.id, job_err.id] {
            let job = jobs.iter().find(|j| j.id == id).unwrap();
            assert_eq!(job.status, JobStatus::Cancelled, "job {id} resurrected");
        }
    }

    #[tokio::test]
    async fn complete_ignores_stale_claim_after_requeue_and_reclaim() {
        let pool = test_pool().await;
        let eng = engine(pool.clone());
        let job = enqueue_test_job(&eng).await;
        let claimed = store::claim_next(&pool, eng.worker_id(), now_unix())
            .await
            .unwrap()
            .unwrap();

        // Simulate reap_stale + re-claim by another worker: same row,
        // new ownership, bumped attempts.
        sqlx::query(
            "UPDATE jobs_tbl SET status = 'running', worker_id = 'other-worker',
             attempts = attempts + 1 WHERE id = ?",
        )
        .bind(job.id)
        .execute(&pool)
        .await
        .unwrap();

        // The stale worker's completion must not touch the new claim.
        store::complete(&pool, &claimed, Ok(()), 0, now_unix())
            .await
            .unwrap();
        store::complete(&pool, &claimed, Err("stale failure".into()), 0, now_unix())
            .await
            .unwrap();

        let jobs = store::list_jobs(&pool, None, 10).await.unwrap();
        assert_eq!(jobs[0].status, JobStatus::Running);
        assert_eq!(jobs[0].worker_id.as_deref(), Some("other-worker"));
    }

    fn batch_newjob(kind: &str, n: i64) -> NewJob {
        NewJob {
            kind: kind.into(),
            payload: json!({"n": n}),
            priority: 0,
            run_at: None,
            max_attempts: None,
        }
    }

    #[tokio::test]
    async fn enqueue_batch_groups_items_under_first_job_id() {
        let pool = test_pool().await;
        let eng = engine(pool.clone());

        let jobs = eng
            .enqueue_batch(&[
                batch_newjob("test.batch", 1),
                batch_newjob("test.batch", 2),
                batch_newjob("test.batch", 3),
            ])
            .await
            .unwrap();
        assert_eq!(jobs.len(), 3);
        let batch_id = jobs[0].id;
        assert!(jobs.iter().all(|j| j.batch_id == Some(batch_id)));

        // Persisted rows agree with the returned structs.
        let listed = store::batch_jobs(&pool, batch_id).await.unwrap();
        assert_eq!(listed.len(), 3);
        assert!(listed.iter().all(|j| j.batch_id == Some(batch_id)));
        assert!(listed.iter().all(|j| j.status == JobStatus::Pending));

        let progress = eng.batch_progress(batch_id).await.unwrap().unwrap();
        assert_eq!(progress.items_total, 3);
        assert_eq!(progress.items_pending, 3);
        assert_eq!(progress.items_done, 0);
        assert_eq!(progress.pct(), 0);
        assert!(!progress.is_complete());

        // Unknown batch id → None.
        assert!(eng.batch_progress(batch_id + 1000).await.unwrap().is_none());
    }

    #[tokio::test]
    async fn batch_progress_tracks_per_item_outcomes() {
        let pool = test_pool().await;
        let mut eng = engine(pool.clone());
        eng.register(
            "test.batch",
            Arc::new(CountingHandler {
                calls: AtomicUsize::new(0),
                fail_times: 0,
            }),
        );

        let jobs = eng
            .enqueue_batch(&[
                batch_newjob("test.batch", 1),
                batch_newjob("test.batch", 2),
                batch_newjob("test.batch", 3),
            ])
            .await
            .unwrap();
        let batch_id = jobs[0].id;

        // Two items succeed, one is skipped.
        assert!(eng.run_once().await.unwrap());
        assert!(eng.run_once().await.unwrap());
        assert!(
            store::skip_job(&pool, jobs[2].id, Some("already up to date"))
                .await
                .unwrap()
        );

        let progress = eng.batch_progress(batch_id).await.unwrap().unwrap();
        assert_eq!(progress.items_total, 3);
        assert_eq!(progress.items_done, 3);
        assert_eq!(progress.items_succeeded, 2);
        assert_eq!(progress.items_skipped, 1);
        assert_eq!(progress.items_pending, 0);
        assert_eq!(progress.pct(), 100);
        assert!(progress.is_complete());

        // The skipped job kept its reason in last_error.
        let listed = store::batch_jobs(&pool, batch_id).await.unwrap();
        let skipped = listed.iter().find(|j| j.id == jobs[2].id).unwrap();
        assert_eq!(skipped.status, JobStatus::Skipped);
        assert_eq!(skipped.last_error.as_deref(), Some("already up to date"));
    }

    #[tokio::test]
    async fn cancel_batch_cancels_only_non_terminal_items() {
        let pool = test_pool().await;
        let eng = engine(pool.clone());

        let jobs = eng
            .enqueue_batch(&[
                batch_newjob("x", 1),
                batch_newjob("x", 2),
                batch_newjob("x", 3),
            ])
            .await
            .unwrap();
        let batch_id = jobs[0].id;

        // One item already finished — it must keep its outcome.
        sqlx::query("UPDATE jobs_tbl SET status = 'succeeded', finished_at = ? WHERE id = ?")
            .bind(now_unix())
            .bind(jobs[0].id)
            .execute(&pool)
            .await
            .unwrap();

        assert_eq!(eng.cancel_batch(batch_id).await.unwrap(), 2);

        let progress = eng.batch_progress(batch_id).await.unwrap().unwrap();
        assert_eq!(progress.items_total, 3);
        assert_eq!(progress.items_done, 3);
        assert_eq!(progress.items_succeeded, 1);
        assert_eq!(progress.items_cancelled, 2);
        assert!(progress.is_complete());
    }

    /// Handler that reports progress mid-run, then succeeds.
    struct ProgressHandler;

    impl JobHandler for ProgressHandler {
        fn handle<'a>(
            &'a self,
            ctx: &'a JobContext,
            job: &'a Job,
        ) -> Pin<Box<dyn Future<Output = Result<(), JobError>> + Send + 'a>> {
            Box::pin(async move {
                assert!(
                    ctx.report_progress(job.id, &json!({"pct": 50, "detail": "half"}))
                        .await
                        .unwrap()
                );
                Ok(())
            })
        }
    }

    #[tokio::test]
    async fn report_progress_lands_on_running_job_and_survives_completion() {
        let pool = test_pool().await;
        let mut eng = engine(pool.clone());
        eng.register("test.progress", Arc::new(ProgressHandler));

        let job = eng
            .enqueue(&NewJob {
                kind: "test.progress".into(),
                payload: json!({}),
                priority: 0,
                run_at: None,
                max_attempts: None,
            })
            .await
            .unwrap();

        assert!(eng.run_once().await.unwrap());
        let jobs = store::list_jobs(&pool, None, 10).await.unwrap();
        let job = jobs.iter().find(|j| j.id == job.id).unwrap();
        assert_eq!(job.status, JobStatus::Succeeded);
        assert_eq!(job.progress, Some(json!({"pct": 50, "detail": "half"})));
    }

    #[tokio::test]
    async fn report_progress_rejected_for_non_running_job() {
        let pool = test_pool().await;
        let eng = engine(pool.clone());
        let job = eng
            .enqueue(&NewJob {
                kind: "x".into(),
                payload: json!({}),
                priority: 0,
                run_at: None,
                max_attempts: None,
            })
            .await
            .unwrap();

        // Still pending — a report must not land.
        assert!(
            !store::report_progress(&pool, job.id, &json!({"pct": 1}))
                .await
                .unwrap()
        );
        let jobs = store::list_jobs(&pool, None, 10).await.unwrap();
        assert_eq!(jobs[0].progress, None);
    }

    #[tokio::test]
    async fn progress_cleared_when_job_is_retried() {
        let pool = test_pool().await;
        let eng = engine(pool.clone());
        let job = eng
            .enqueue(&NewJob {
                kind: "x".into(),
                payload: json!({}),
                priority: 0,
                run_at: None,
                max_attempts: None,
            })
            .await
            .unwrap();

        // Simulate a failed attempt that left progress behind, then
        // a retry: progress must reset so the next attempt starts clean.
        // `complete` is claim-guarded (worker_id + attempts), so the
        // row must look like this worker's live claim.
        sqlx::query(
            "UPDATE jobs_tbl SET status = 'running', worker_id = ?,
             attempts = 1, progress_json = '{\"pct\": 90}' WHERE id = ?",
        )
        .bind(eng.worker_id())
        .bind(job.id)
        .execute(&pool)
        .await
        .unwrap();
        let claimed = store::list_jobs(&pool, None, 10)
            .await
            .unwrap()
            .into_iter()
            .find(|j| j.id == job.id)
            .unwrap();
        store::complete(&pool, &claimed, Err("boom".into()), 0, now_unix())
            .await
            .unwrap();

        let jobs = store::list_jobs(&pool, None, 10).await.unwrap();
        assert_eq!(jobs[0].status, JobStatus::Pending);
        assert_eq!(jobs[0].progress, None);
    }

    // ------------------------------------------------------------------
    // Multi-engine tests. `sqlite::memory:` databases are per-pool, so
    // these use a shared on-disk file pool that several `JobEngine`s
    // can connect to concurrently.
    // ------------------------------------------------------------------

    /// A pool backed by a real file so multiple engines (and multiple
    /// connections) share one database. Returns the tempdir guard —
    /// keep it alive for the test's duration.
    async fn shared_file_pool() -> (tempfile::TempDir, SqlitePool) {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("jobs.db");
        let opts = SqliteConnectOptions::new()
            .filename(&path)
            .create_if_missing(true);
        let pool = SqlitePoolOptions::new()
            .max_connections(8)
            .connect_with(opts)
            .await
            .unwrap();
        store::create_jobs_table(&pool).await.unwrap();
        (dir, pool)
    }

    /// Polls `list_jobs` until `job_id` reaches `status` or ~5s pass.
    async fn wait_for_job_status(pool: &SqlitePool, job_id: i64, status: JobStatus) {
        let deadline = Instant::now() + Duration::from_secs(5);
        loop {
            let jobs = store::list_jobs(pool, None, 200).await.unwrap();
            if let Some(job) = jobs.iter().find(|j| j.id == job_id) {
                if job.status == status {
                    return;
                }
            }
            assert!(
                Instant::now() < deadline,
                "timed out waiting for job {job_id} to reach {status:?}"
            );
            tokio::time::sleep(Duration::from_millis(2)).await;
        }
    }

    /// Counts every `handle()` invocation per job id, tracks peak
    /// handler concurrency, and yields briefly so concurrent claims
    /// overlap in time.
    struct RaceHandler {
        entries: Mutex<HashMap<i64, u32>>,
        inflight: AtomicUsize,
        max_inflight: AtomicUsize,
    }

    impl RaceHandler {
        fn new() -> Self {
            Self {
                entries: Mutex::new(HashMap::new()),
                inflight: AtomicUsize::new(0),
                max_inflight: AtomicUsize::new(0),
            }
        }

        fn entry_count(&self, job_id: i64) -> u32 {
            self.entries
                .lock()
                .unwrap()
                .get(&job_id)
                .copied()
                .unwrap_or(0)
        }
    }

    impl JobHandler for RaceHandler {
        fn handle<'a>(
            &'a self,
            _ctx: &'a JobContext,
            job: &'a Job,
        ) -> Pin<Box<dyn Future<Output = Result<(), JobError>> + Send + 'a>> {
            Box::pin(async move {
                *self.entries.lock().unwrap().entry(job.id).or_insert(0) += 1;
                let now = self.inflight.fetch_add(1, AtomicOrdering::SeqCst) + 1;
                self.max_inflight.fetch_max(now, AtomicOrdering::SeqCst);
                // Widen the claim→complete window so a buggy double
                // claim would actually overlap two handler runs.
                tokio::time::sleep(Duration::from_millis(2)).await;
                self.inflight.fetch_sub(1, AtomicOrdering::SeqCst);
                Ok(())
            })
        }
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 4)]
    async fn multiple_engines_claim_each_job_exactly_once() {
        let (_dir, pool) = shared_file_pool().await;
        const N_JOBS: usize = 60;
        const N_ENGINES: usize = 4;

        let handler = Arc::new(RaceHandler::new());
        let mut engines = Vec::new();
        for _ in 0..N_ENGINES {
            let mut eng = engine(pool.clone());
            eng.register("race.ping", handler.clone());
            engines.push(Arc::new(eng));
        }

        // Seed the queue with a single batch insert.
        let jobs: Vec<NewJob> = (0..N_JOBS)
            .map(|n| NewJob {
                kind: "race.ping".into(),
                payload: json!({"n": n}),
                priority: 0,
                run_at: None,
                max_attempts: None,
            })
            .collect();
        store::enqueue_batch(&pool, &jobs, 3).await.unwrap();

        // Every engine drains the shared queue concurrently until the
        // monitor below observes all jobs succeeded.
        let stop = Arc::new(AtomicBool::new(false));
        let mut handles = Vec::new();
        for eng in &engines {
            let eng = eng.clone();
            let stop = stop.clone();
            handles.push(tokio::spawn(async move {
                while !stop.load(AtomicOrdering::SeqCst) {
                    match eng.run_once().await {
                        Ok(true) => {}
                        Ok(false) => tokio::time::sleep(Duration::from_millis(1)).await,
                        Err(_) => break,
                    }
                }
            }));
        }

        // Wait until every job is terminal-success, then stop engines.
        let deadline = Instant::now() + Duration::from_secs(8);
        loop {
            let listed = store::list_jobs(&pool, None, 500).await.unwrap();
            let done = listed
                .iter()
                .filter(|j| j.status == JobStatus::Succeeded)
                .count();
            if done == N_JOBS {
                break;
            }
            assert!(
                Instant::now() < deadline,
                "timed out draining queue: {done}/{N_JOBS} succeeded, \
                 statuses: {:?}",
                listed.iter().map(|j| j.status).collect::<Vec<_>>()
            );
            tokio::time::sleep(Duration::from_millis(5)).await;
        }
        stop.store(true, AtomicOrdering::SeqCst);
        for h in handles {
            h.await.unwrap();
        }

        // Exactly-once: each job id was handed to a handler once, was
        // claimed once (attempts == 1), and finished succeeded.
        let listed = store::list_jobs(&pool, None, 500).await.unwrap();
        assert_eq!(listed.len(), N_JOBS);
        for job in &listed {
            assert_eq!(
                job.status,
                JobStatus::Succeeded,
                "job {} not succeeded",
                job.id
            );
            assert_eq!(job.attempts, 1, "job {} claimed more than once", job.id);
            assert_eq!(
                handler.entry_count(job.id),
                1,
                "job {} executed {} times",
                job.id,
                handler.entry_count(job.id)
            );
        }
        let total: u32 = listed.iter().map(|j| handler.entry_count(j.id)).sum();
        assert_eq!(total, N_JOBS as u32);
        // Evidence for the report: handlers did overlap in time.
        assert!(
            handler.max_inflight.load(AtomicOrdering::SeqCst) > 1,
            "expected handler executions to overlap across engines"
        );
    }

    /// Handler that never completes — simulates a worker killed
    /// mid-job when the engine task is aborted.
    struct HangHandler;

    impl JobHandler for HangHandler {
        fn handle<'a>(
            &'a self,
            _ctx: &'a JobContext,
            _job: &'a Job,
        ) -> Pin<Box<dyn Future<Output = Result<(), JobError>> + Send + 'a>> {
            Box::pin(async move {
                std::future::pending::<()>().await;
                Ok(())
            })
        }
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 4)]
    async fn engine_crash_between_claim_and_finish_is_recovered_once() {
        let (_dir, pool) = shared_file_pool().await;

        // Engine A claims the job then "crashes" (task aborted) before
        // store::complete runs — the row stays `running`.
        let mut eng_a = engine(pool.clone());
        eng_a.register("crash.hang", Arc::new(HangHandler));
        let job = eng_a
            .enqueue(&NewJob {
                kind: "crash.hang".into(),
                payload: json!({}),
                priority: 0,
                run_at: None,
                max_attempts: None,
            })
            .await
            .unwrap();

        let eng_a = Arc::new(eng_a);
        let crashed = {
            let eng_a = eng_a.clone();
            tokio::spawn(async move { eng_a.run_once().await })
        };
        wait_for_job_status(&pool, job.id, JobStatus::Running).await;
        crashed.abort();
        let _ = crashed.await;

        // Engine B starts up: recover() requeues the orphaned row and
        // a single run_once completes it exactly once.
        let mut eng_b = engine(pool.clone());
        let handler = Arc::new(CountingHandler {
            calls: AtomicUsize::new(0),
            fail_times: 0,
        });
        eng_b.register("crash.hang", handler.clone());

        assert_eq!(eng_b.recover().await.unwrap(), 1);
        assert!(eng_b.run_once().await.unwrap());
        // Queue drained — no phantom re-execution of the orphan.
        assert!(!eng_b.run_once().await.unwrap());

        assert_eq!(handler.calls.load(AtomicOrdering::SeqCst), 1);
        let jobs = store::list_jobs(&pool, None, 10).await.unwrap();
        let job = jobs.iter().find(|j| j.id == job.id).unwrap();
        assert_eq!(job.status, JobStatus::Succeeded);
        // Claimed twice (crash + recovery); recover() intentionally
        // does not reset attempts.
        assert_eq!(job.attempts, 2);
    }

    /// Fails every job whose payload `n` equals `fail_n`.
    struct FailOnN {
        fail_n: i64,
        calls: Mutex<HashMap<i64, u32>>,
    }

    impl JobHandler for FailOnN {
        fn handle<'a>(
            &'a self,
            _ctx: &'a JobContext,
            job: &'a Job,
        ) -> Pin<Box<dyn Future<Output = Result<(), JobError>> + Send + 'a>> {
            Box::pin(async move {
                let n = job.payload["n"].as_i64().unwrap_or(-1);
                *self.calls.lock().unwrap().entry(n).or_insert(0) += 1;
                if n == self.fail_n {
                    Err(JobError::Failed(format!("item {n} failed")))
                } else {
                    Ok(())
                }
            })
        }
    }

    #[tokio::test]
    async fn batch_partial_failure_leaves_siblings_unaffected() {
        let pool = test_pool().await;
        let mut eng = engine(pool.clone());
        let handler = Arc::new(FailOnN {
            fail_n: 2,
            calls: Mutex::new(HashMap::new()),
        });
        eng.register("test.partial", handler.clone());

        // One batch where the middle item is capped at 2 attempts and
        // always fails; siblings use the engine default (3).
        let mk = |n: i64, max: Option<u32>| NewJob {
            kind: "test.partial".into(),
            payload: json!({"n": n}),
            priority: 0,
            run_at: None,
            max_attempts: max,
        };
        let jobs = eng
            .enqueue_batch(&[mk(1, None), mk(2, Some(2)), mk(3, None)])
            .await
            .unwrap();
        let batch_id = jobs[0].id;

        // Drain until the batch reaches a terminal aggregate state.
        // backoff_base = 0 → the failing item's retries are immediate.
        for _ in 0..50 {
            let _ = eng.run_once().await.unwrap();
            if eng
                .batch_progress(batch_id)
                .await
                .unwrap()
                .unwrap()
                .is_complete()
            {
                break;
            }
            tokio::time::sleep(Duration::from_millis(1)).await;
        }

        let progress = eng.batch_progress(batch_id).await.unwrap().unwrap();
        assert!(progress.is_complete());
        assert_eq!(progress.items_total, 3);
        assert_eq!(progress.items_succeeded, 2);
        assert_eq!(progress.items_failed, 1);
        assert_eq!(progress.items_pending, 0);

        let listed = store::batch_jobs(&pool, batch_id).await.unwrap();
        let by_n = |n: i64| {
            listed
                .iter()
                .find(|j| j.payload["n"].as_i64() == Some(n))
                .unwrap()
        };
        assert_eq!(by_n(1).status, JobStatus::Succeeded);
        assert_eq!(by_n(1).attempts, 1);
        assert_eq!(by_n(3).status, JobStatus::Succeeded);
        assert_eq!(by_n(3).attempts, 1);

        // The failing item retried exactly up to its own max_attempts
        // and failed terminally; siblings ran exactly once.
        let failed = by_n(2);
        assert_eq!(failed.status, JobStatus::Failed);
        assert_eq!(failed.attempts, 2);
        assert_eq!(failed.last_error.as_deref(), Some("item 2 failed"));
        let calls = handler.calls.lock().unwrap();
        assert_eq!(calls.get(&1), Some(&1));
        assert_eq!(calls.get(&2), Some(&2));
        assert_eq!(calls.get(&3), Some(&1));
    }

    #[tokio::test]
    async fn list_jobs_filters_by_status_orders_newest_first_and_limits() {
        let pool = test_pool().await;
        let mut eng = engine(pool.clone());
        eng.register(
            "test.ping",
            Arc::new(CountingHandler {
                calls: AtomicUsize::new(0),
                fail_times: 0,
            }),
        );

        // Three jobs: run two to success, leave one pending, and add
        // one manually-failed row for filter coverage.
        let mut ids = Vec::new();
        for _ in 0..3 {
            ids.push(
                eng.enqueue(&NewJob {
                    kind: "test.ping".into(),
                    payload: json!({}),
                    priority: 0,
                    run_at: None,
                    max_attempts: None,
                })
                .await
                .unwrap()
                .id,
            );
        }
        eng.run_once().await.unwrap();
        eng.run_once().await.unwrap();

        // Highest-priority/id ordering: jobs are claimed oldest-first,
        // so ids[0] and ids[1] succeeded, ids[2] stays pending.
        sqlx::query("UPDATE jobs_tbl SET status = 'failed', finished_at = ? WHERE id = ?")
            .bind(now_unix())
            .bind(ids[2])
            .execute(&pool)
            .await
            .unwrap();

        let succeeded = store::list_jobs(&pool, Some(JobStatus::Succeeded), 10)
            .await
            .unwrap();
        assert_eq!(succeeded.len(), 2);
        assert!(succeeded.iter().all(|j| j.status == JobStatus::Succeeded));
        // Newest first.
        assert!(succeeded[0].id > succeeded[1].id);
        assert_eq!(succeeded[0].id, ids[1]);
        assert_eq!(succeeded[1].id, ids[0]);

        let pending = store::list_jobs(&pool, Some(JobStatus::Pending), 10)
            .await
            .unwrap();
        assert!(pending.is_empty());

        let failed = store::list_jobs(&pool, Some(JobStatus::Failed), 10)
            .await
            .unwrap();
        assert_eq!(failed.len(), 1);
        assert_eq!(failed[0].id, ids[2]);

        // Limit applies after ordering: newest two of all three.
        let all = store::list_jobs(&pool, None, 2).await.unwrap();
        assert_eq!(all.len(), 2);
        assert_eq!(all[0].id, ids[2]);
        assert_eq!(all[1].id, ids[1]);
    }
}
