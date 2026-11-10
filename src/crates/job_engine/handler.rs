//! Handler trait and execution context.
//!
//! Handlers are plain `Send + Sync` objects registered on the engine by
//! job `kind`. The trait uses a boxed-future method instead of
//! `async fn` so it stays object-safe without an `async-trait` dep.

use std::future::Future;
use std::pin::Pin;
use std::sync::Arc;

use sqlx::SqlitePool;

use super::JobError;
use super::job::Job;

/// Shared services handed to every handler invocation.
///
/// Cloning is cheap (the pool is reference-counted). Add fields here as
/// new services come online (storage engine, document engine) rather
/// than growing handler-specific state.
#[derive(Clone)]
pub struct JobContext {
    pub pool: SqlitePool,
}

impl JobContext {
    /// Reports mid-run progress for `job_id` (see
    /// [`crate::crates::job_engine::report_progress`]). Handlers call
    /// this as work advances; the snapshot is exposed on `Job.progress`
    /// and via the jobs API. Returns `false` when the job is no
    /// longer `running` (cancelled or finished) — the report is then
    /// dropped.
    pub async fn report_progress(
        &self,
        job_id: i64,
        progress: &serde_json::Value,
    ) -> Result<bool, JobError> {
        super::store::report_progress(&self.pool, job_id, progress).await
    }
}

/// A unit of work the engine can execute.
pub trait JobHandler: Send + Sync + 'static {
    /// Execute `job`. Return `Ok(())` for success; `Err` retries the
    /// job (with backoff) until `max_attempts` is exhausted.
    fn handle<'a>(
        &'a self,
        ctx: &'a JobContext,
        job: &'a Job,
    ) -> Pin<Box<dyn Future<Output = Result<(), JobError>> + Send + 'a>>;
}

/// Type-erased handler stored in the engine registry.
pub type SharedHandler = Arc<dyn JobHandler>;

#[cfg(test)]
mod tests {
    use super::*;
    use crate::crates::job_engine::job::{JobStatus, NewJob};
    use crate::crates::job_engine::store;
    use serde_json::json;
    use sqlx::sqlite::SqlitePoolOptions;
    use std::future::Future;
    use std::pin::Pin;
    use std::sync::Arc;

    async fn test_pool() -> SqlitePool {
        let pool = SqlitePoolOptions::new()
            .max_connections(1)
            .connect("sqlite::memory:")
            .await
            .unwrap();
        store::create_jobs_table(&pool).await.unwrap();
        pool
    }

    fn new_job(kind: &str) -> NewJob {
        NewJob {
            kind: kind.into(),
            payload: json!({"x": 1}),
            priority: 0,
            run_at: None,
            max_attempts: None,
        }
    }

    fn now_unix() -> i64 {
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|d| d.as_secs() as i64)
            .unwrap_or(0)
    }

    /// Records the job it was invoked with so tests can assert the
    /// handler contract end-to-end through a `SharedHandler`.
    struct EchoHandler {
        seen_kind: std::sync::Mutex<Option<String>>,
        seen_payload: std::sync::Mutex<Option<serde_json::Value>>,
    }

    impl JobHandler for EchoHandler {
        fn handle<'a>(
            &'a self,
            _ctx: &'a JobContext,
            job: &'a Job,
        ) -> Pin<Box<dyn Future<Output = Result<(), JobError>> + Send + 'a>> {
            Box::pin(async move {
                *self.seen_kind.lock().unwrap() = Some(job.kind.clone());
                *self.seen_payload.lock().unwrap() = Some(job.payload.clone());
                Ok(())
            })
        }
    }

    #[tokio::test]
    async fn shared_handler_invocation_receives_job_and_ctx() {
        let pool = test_pool().await;
        let job = store::enqueue(&pool, &new_job("test.echo"), 3)
            .await
            .unwrap();

        let concrete = Arc::new(EchoHandler {
            seen_kind: std::sync::Mutex::new(None),
            seen_payload: std::sync::Mutex::new(None),
        });
        let handler: SharedHandler = concrete.clone();
        let ctx = JobContext { pool: pool.clone() };

        // Exercise through the trait object, as the engine does.
        let result = handler.handle(&ctx, &job).await;
        assert!(result.is_ok());

        assert_eq!(
            concrete.seen_kind.lock().unwrap().as_deref(),
            Some("test.echo")
        );
        assert_eq!(
            concrete.seen_payload.lock().unwrap().as_ref().unwrap()["x"],
            json!(1)
        );
    }

    #[tokio::test]
    async fn ctx_report_progress_roundtrip_on_running_job() {
        let pool = test_pool().await;
        let job = store::enqueue(&pool, &new_job("x"), 3).await.unwrap();
        let ctx = JobContext { pool: pool.clone() };

        // Pending job: report is rejected.
        assert!(
            !ctx.report_progress(job.id, &json!({"pct": 10}))
                .await
                .unwrap()
        );

        // Claim the job so it is `running`.
        let claimed = store::claim_next(&pool, "w1", now_unix() + 10)
            .await
            .unwrap()
            .unwrap();
        assert_eq!(claimed.status, JobStatus::Running);

        assert!(
            ctx.report_progress(job.id, &json!({"pct": 55, "detail": "halfway"}))
                .await
                .unwrap()
        );

        let listed = store::list_jobs(&pool, None, 10).await.unwrap();
        assert_eq!(
            listed[0].progress,
            Some(json!({"pct": 55, "detail": "halfway"}))
        );
    }

    #[tokio::test]
    async fn handler_error_maps_to_job_error_display() {
        struct Failing;
        impl JobHandler for Failing {
            fn handle<'a>(
                &'a self,
                _ctx: &'a JobContext,
                _job: &'a Job,
            ) -> Pin<Box<dyn Future<Output = Result<(), JobError>> + Send + 'a>> {
                Box::pin(async move { Err(JobError::Failed("nope".into())) })
            }
        }

        let pool = test_pool().await;
        let ctx = JobContext { pool };
        let job = Job {
            id: 1,
            kind: "k".into(),
            payload: json!({}),
            status: JobStatus::Pending,
            priority: 0,
            attempts: 0,
            max_attempts: 1,
            run_at: 0,
            created_at: 0,
            started_at: None,
            finished_at: None,
            last_error: None,
            worker_id: None,
            batch_id: None,
            progress: None,
        };
        let err = Failing.handle(&ctx, &job).await.unwrap_err();
        assert_eq!(err.to_string(), "nope");
    }
}
