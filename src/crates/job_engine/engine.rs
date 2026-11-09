//! The job engine: claims due jobs from `jobs_tbl` and dispatches them
//! to registered [`JobHandler`]s.
//!
//! Single-process design — one [`JobEngine`] per pool. SQLite's single
//! writer plus the atomic `UPDATE ... RETURNING` claim makes multiple
//! engines safe-ish, but handlers must still be idempotent because a
//! crash between claim and completion leaves the job `running` until
//! it is reclaimed (see [`JobEngine::reap_stale`]).

use std::collections::HashMap;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::Arc;
use std::time::Duration;

use sqlx::SqlitePool;

use super::handler::{JobContext, SharedHandler};
use super::job::{Job, NewJob};
use super::store;
use super::JobError;

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
            ctx: JobContext {
                pool: pool.clone(),
            },
            pool,
            config,
            handlers: HashMap::new(),
            worker_id: format!(
                "worker-{}-{}",
                std::process::id(),
                seq
            ),
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

    /// Claims and executes at most one job. Returns `true` when a job
    /// was processed. This is the unit [`JobEngine::run`] repeats.
    pub async fn run_once(&self) -> Result<bool, JobError> {
        let Some(job) = store::claim_next(&self.pool, &self.worker_id, now_unix()).await?
        else {
            return Ok(false);
        };

        let result = match self.handlers.get(&job.kind) {
            Some(handler) => handler.handle(&self.ctx, &job).await,
            None => Err(JobError::NoHandler(job.kind.clone())),
        };

        let err = result.err().map(|e| e.to_string());
        store::complete(
            &self.pool,
            job.id,
            err.map_or(Ok(()), Err),
            self.config.backoff_base.as_secs(),
            now_unix(),
        )
        .await?;

        Ok(true)
    }

    /// Returns `running` jobs older than `stale_after` to `pending`.
    /// Call periodically (or on startup) to recover from crashes
    /// between claim and completion. Returns rows reclaimed.
    pub async fn reap_stale(&self) -> Result<u64, JobError> {
        let cutoff = now_unix() - self.config.stale_after.as_secs() as i64;
        let done = sqlx::query(
            "UPDATE jobs_tbl
             SET status = 'pending', worker_id = NULL, started_at = NULL,
                 run_at = ?, last_error = 'reaped: stale running job'
             WHERE status = 'running' AND started_at < ?",
        )
        .bind(now_unix())
        .bind(cutoff)
        .execute(&self.pool)
        .await?;
        Ok(done.rows_affected())
    }

    /// Poll loop: reap stale jobs, then drain due work until the queue
    /// is empty, sleeping `poll_interval` between empty polls. Runs
    /// forever; spawn it and abort on shutdown.
    pub async fn run(&self) {
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

fn now_unix() -> i64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs() as i64)
        .unwrap_or(0)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::crates::job_engine::job::JobStatus;
    use crate::crates::job_engine::JobHandler;
    use serde_json::json;
    use sqlx::sqlite::SqlitePoolOptions;
    use std::future::Future;
    use std::pin::Pin;
    use std::sync::atomic::{AtomicUsize, Ordering as AtomicOrdering};

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
        assert!(jobs[0].last_error.as_deref().unwrap().contains("no handler"));
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
}
