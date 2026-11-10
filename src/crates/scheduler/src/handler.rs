//! `scheduler.tick` job handler — lets the job engine drive the
//! scheduler with no separate background loop.
//!
//! Each invocation runs [`crate::crates::scheduler::tick`] once, then
//! enqueues the *next* tick job `tick_interval_secs` in the future, so
//! the chain is self-perpetuating. To start the chain call
//! [`crate::crates::scheduler::enqueue_tick`] once at startup.

use std::future::Future;
use std::pin::Pin;

use crate::crates::job_engine::{Job, JobContext, JobError, JobHandler, NewJob, enqueue};

use super::SchedulerError;
use super::store::{SCHEDULER_TICK_KIND, tick};

/// Tunables for [`SchedulerTickHandler`].
#[derive(Debug, Clone)]
pub struct SchedulerConfig {
    /// Seconds between scheduler ticks. Each tick enqueues the next
    /// one this far out.
    pub tick_interval_secs: u64,
    /// Priority for the tick jobs themselves (schedules keep their own
    /// per-schedule priority for the jobs they produce).
    pub tick_priority: i64,
    /// `max_attempts` for tick jobs. `None` = engine default.
    pub tick_max_attempts: Option<u32>,
}

impl Default for SchedulerConfig {
    fn default() -> Self {
        Self {
            tick_interval_secs: 60,
            tick_priority: 0,
            tick_max_attempts: Some(3),
        }
    }
}

/// Handles `scheduler.tick` jobs: fires due schedules and schedules
/// the next tick.
pub struct SchedulerTickHandler {
    config: SchedulerConfig,
}

impl SchedulerTickHandler {
    pub fn new(config: SchedulerConfig) -> Self {
        Self { config }
    }
}

impl JobHandler for SchedulerTickHandler {
    fn handle<'a>(
        &'a self,
        ctx: &'a JobContext,
        _job: &'a Job,
    ) -> Pin<Box<dyn Future<Output = Result<(), JobError>> + Send + 'a>> {
        Box::pin(async move {
            let now = now_unix();
            let outcome = tick(&ctx.pool, now).await.map_err(into_job_error)?;
            if outcome.enqueued > 0 {
                tracing::debug!(enqueued = outcome.enqueued, "scheduler tick fired jobs");
            }

            // Self-perpetuate: enqueue the next tick. If this insert
            // fails the job retries and the chain continues via the
            // retry; worst case the chain stalls until restart, where
            // `enqueue_tick` is called again.
            enqueue(
                &ctx.pool,
                &NewJob {
                    kind: SCHEDULER_TICK_KIND.into(),
                    payload: serde_json::json!({}),
                    priority: self.config.tick_priority,
                    run_at: Some(now + self.config.tick_interval_secs as i64),
                    max_attempts: self.config.tick_max_attempts,
                },
                self.config.tick_max_attempts.unwrap_or(1),
            )
            .await?;

            Ok(())
        })
    }
}

fn into_job_error(e: SchedulerError) -> JobError {
    match e {
        SchedulerError::Job(j) => j,
        other => JobError::Failed(other.to_string()),
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
    //! Coverage for `SchedulerTickHandler`: it must run one `tick`,
    //! enqueue *only* the schedule jobs that are due, skip
    //! not-due/disabled schedules, self-perpetuate by enqueueing the
    //! next `scheduler.tick` job, and propagate enqueue failures as
    //! job errors (the engine then retries the tick job).
    //!
    //! Semantics documented by these tests:
    //! - The produced schedule job's `run_at` is the schedule's
    //!   *original* `next_run_at` (the occurrence time), which is in
    //!   the past for a due schedule — so produced jobs are
    //!   immediately claimable.
    //! - Produced schedule jobs get `max_attempts = 1` (tick passes
    //!   `max_attempts: None` with engine default 1) regardless of
    //!   the schedule — schedules carry no per-schedule
    //!   `max_attempts`.
    //! - If `tick` claims an occurrence but the `jobs_tbl` insert
    //!   fails, the occurrence is *consumed*: `next_run_at` was
    //!   already advanced and `last_run_at` stamped, and there is no
    //!   retry of that occurrence — the schedule simply continues
    //!   from the advanced `next_run_at`. (At-least-once is NOT
    //!   guaranteed for produced jobs.)

    use super::*;
    use crate::crates::job_engine::{
        JobEngine, JobEngineConfig, JobStatus, create_jobs_table, list_jobs as job_list,
    };
    use crate::crates::scheduler::{
        NewSchedule, create_schedule_table, enqueue_tick, list_schedules, upsert_schedule,
    };
    use serde_json::json;
    use sqlx::SqlitePool;
    use sqlx::sqlite::SqlitePoolOptions;
    use std::sync::Arc;

    async fn test_pool() -> SqlitePool {
        let pool = SqlitePoolOptions::new()
            .max_connections(1)
            .connect("sqlite::memory:")
            .await
            .unwrap();
        create_jobs_table(&pool).await.unwrap();
        create_schedule_table(&pool).await.unwrap();
        pool
    }

    fn ctx(pool: &SqlitePool) -> JobContext {
        JobContext { pool: pool.clone() }
    }

    /// A `scheduler.tick` job as the engine would hand it to the
    /// handler (status/attempts are irrelevant to `handle`).
    fn tick_job() -> Job {
        Job {
            id: 1,
            kind: SCHEDULER_TICK_KIND.into(),
            payload: json!({}),
            status: JobStatus::Running,
            priority: 0,
            attempts: 1,
            max_attempts: 3,
            run_at: 0,
            created_at: 0,
            started_at: None,
            finished_at: None,
            last_error: None,
            worker_id: Some("test-worker".into()),
            batch_id: None,
            progress: None,
        }
    }

    fn test_handler() -> SchedulerTickHandler {
        SchedulerTickHandler::new(SchedulerConfig {
            tick_interval_secs: 30,
            tick_priority: 7,
            tick_max_attempts: Some(2),
        })
    }

    fn schedule(name: &str, kind: &str, next_run_at: i64, enabled: Option<bool>) -> NewSchedule {
        NewSchedule {
            name: name.into(),
            kind: kind.into(),
            payload: json!({"sched": name}),
            interval_secs: 60,
            priority: Some(9),
            next_run_at: Some(next_run_at),
            enabled,
        }
    }

    fn now() -> i64 {
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_secs() as i64
    }

    #[tokio::test]
    async fn handle_fires_due_schedule_and_enqueues_next_tick() {
        let pool = test_pool().await;
        let t0 = now();
        let occurrence = t0 - 5;
        upsert_schedule(&pool, &schedule("gc", "system.ping", occurrence, None))
            .await
            .unwrap();

        test_handler()
            .handle(&ctx(&pool), &tick_job())
            .await
            .unwrap();
        let t1 = now();

        let jobs = job_list(&pool, None, 10).await.unwrap();
        assert_eq!(jobs.len(), 2, "expected one schedule job + one next tick");

        // Schedule -> job payload mapping: kind, payload and
        // priority are copied verbatim from the schedule row.
        let ping = jobs.iter().find(|j| j.kind == "system.ping").unwrap();
        assert_eq!(ping.payload, json!({"sched": "gc"}));
        assert_eq!(ping.priority, 9);
        assert_eq!(ping.status, JobStatus::Pending);
        // run_at is the occurrence time (in the past) so the job is
        // immediately runnable.
        assert_eq!(ping.run_at, occurrence);
        // tick() enqueues with max_attempts: None and default 1.
        assert_eq!(ping.max_attempts, 1);

        // The next scheduler.tick job: empty payload, configured
        // priority/max_attempts, run_at = handle-time now + interval.
        let next_tick = jobs.iter().find(|j| j.kind == SCHEDULER_TICK_KIND).unwrap();
        assert_eq!(next_tick.payload, json!({}));
        assert_eq!(next_tick.priority, 7);
        assert_eq!(next_tick.max_attempts, 2);
        assert!(
            next_tick.run_at >= t0 + 30 && next_tick.run_at <= t1 + 30,
            "next tick run_at {} not in [{}, {}]",
            next_tick.run_at,
            t0 + 30,
            t1 + 30
        );

        // The schedule advanced past now and recorded the run.
        let s = &list_schedules(&pool).await.unwrap()[0];
        assert!(s.last_run_at.unwrap() >= t0);
        assert!(s.next_run_at > t1);
    }

    #[tokio::test]
    async fn handle_skips_not_due_and_disabled_schedules() {
        let pool = test_pool().await;
        let t = now();
        // Not due yet.
        upsert_schedule(&pool, &schedule("future", "system.ping", t + 3600, None))
            .await
            .unwrap();
        // Due but disabled (paused).
        upsert_schedule(
            &pool,
            &schedule("paused", "system.ping", t - 10, Some(false)),
        )
        .await
        .unwrap();
        // Due and enabled — the only one that may fire.
        upsert_schedule(&pool, &schedule("due", "system.other", t - 3, Some(true)))
            .await
            .unwrap();

        test_handler()
            .handle(&ctx(&pool), &tick_job())
            .await
            .unwrap();

        let jobs = job_list(&pool, None, 10).await.unwrap();
        let pings: Vec<_> = jobs
            .iter()
            .filter(|j| j.kind != SCHEDULER_TICK_KIND)
            .collect();
        assert_eq!(pings.len(), 1);
        assert_eq!(pings[0].kind, "system.other");
        assert_eq!(pings[0].payload, json!({"sched": "due"}));

        // Neither skipped schedule advanced.
        let schedules = list_schedules(&pool).await.unwrap();
        let future = schedules.iter().find(|s| s.name == "future").unwrap();
        assert_eq!(future.next_run_at, t + 3600);
        assert_eq!(future.last_run_at, None);
        let paused = schedules.iter().find(|s| s.name == "paused").unwrap();
        assert_eq!(paused.next_run_at, t - 10);
        assert_eq!(paused.last_run_at, None);
    }

    #[tokio::test]
    async fn handle_does_not_refire_same_occurrence() {
        let pool = test_pool().await;
        let t = now();
        upsert_schedule(&pool, &schedule("once", "system.ping", t - 5, None))
            .await
            .unwrap();

        let h = test_handler();
        h.handle(&ctx(&pool), &tick_job()).await.unwrap();
        h.handle(&ctx(&pool), &tick_job()).await.unwrap();

        let jobs = job_list(&pool, None, 20).await.unwrap();
        let pings: Vec<_> = jobs.iter().filter(|j| j.kind == "system.ping").collect();
        // Idempotent: two ticks inside the same interval window
        // produce exactly one schedule job.
        assert_eq!(pings.len(), 1);
        // Each handle() still enqueues its own next-tick job.
        let ticks: Vec<_> = jobs
            .iter()
            .filter(|j| j.kind == SCHEDULER_TICK_KIND)
            .collect();
        assert_eq!(ticks.len(), 2);
    }

    /// If the produced-job insert fails, `handle` returns `Err` and
    /// — because `tick` claims (advances `next_run_at`, stamps
    /// `last_run_at`) *before* inserting — the occurrence is lost:
    /// no retry of that occurrence exists. The error propagates so
    /// the engine retries the *tick job* itself, which then
    /// self-perpetuates the chain on success.
    #[tokio::test]
    async fn handle_propagates_enqueue_failure_and_loses_occurrence() {
        let pool = SqlitePoolOptions::new()
            .max_connections(1)
            .connect("sqlite::memory:")
            .await
            .unwrap();
        // Deliberately create ONLY the schedule table: tick()'s
        // claim UPDATE succeeds but job_enqueue fails on the
        // missing jobs_tbl.
        create_schedule_table(&pool).await.unwrap();

        let t = now();
        upsert_schedule(&pool, &schedule("doomed", "system.ping", t - 5, None))
            .await
            .unwrap();

        let result = test_handler().handle(&ctx(&pool), &tick_job()).await;
        assert!(result.is_err(), "enqueue failure must surface as JobError");

        // The occurrence was consumed even though no job row exists.
        let s = &list_schedules(&pool).await.unwrap()[0];
        assert!(s.last_run_at.is_some());
        assert!(s.next_run_at > t);

        // Once jobs_tbl exists, ticking again does NOT resurrect the
        // lost occurrence — the schedule is simply on its next
        // interval. (Documents at-most-once-per-claim semantics.)
        create_jobs_table(&pool).await.unwrap();
        let out = tick(&pool, now() + 120).await.unwrap();
        // next_run_at advanced by whole 60s intervals past the
        // original occurrence; with interval 60 and ~2min elapsed it
        // may legitimately be due again — what matters is it fires
        // at most once per interval, never catching up missed runs.
        assert!(out.enqueued <= 1);
    }

    #[tokio::test]
    async fn default_config_values() {
        let c = SchedulerConfig::default();
        assert_eq!(c.tick_interval_secs, 60);
        assert_eq!(c.tick_priority, 0);
        assert_eq!(c.tick_max_attempts, Some(3));
    }

    struct NoopHandler;

    impl JobHandler for NoopHandler {
        fn handle<'a>(
            &'a self,
            _ctx: &'a JobContext,
            _job: &'a Job,
        ) -> std::pin::Pin<Box<dyn std::future::Future<Output = Result<(), JobError>> + Send + 'a>>
        {
            Box::pin(async { Ok(()) })
        }
    }

    /// End-to-end: a `scheduler.tick` job row driven by a real
    /// `JobEngine` produces a `jobs_tbl` row for the due schedule,
    /// and the chain enqueues the next tick ~interval out.
    #[tokio::test]
    async fn engine_tick_produces_schedule_job_row() {
        let pool = test_pool().await;
        let mut eng = JobEngine::new(
            pool.clone(),
            JobEngineConfig {
                poll_interval: std::time::Duration::from_millis(10),
                default_max_attempts: 3,
                backoff_base: std::time::Duration::from_secs(0),
                stale_after: std::time::Duration::from_secs(60),
            },
        );
        eng.register(SCHEDULER_TICK_KIND, Arc::new(test_handler()));
        eng.register("system.ping", Arc::new(NoopHandler));

        let t = now();
        upsert_schedule(&pool, &schedule("e2e", "system.ping", t - 5, None))
            .await
            .unwrap();
        enqueue_tick(&pool, Some(t), None).await.unwrap();

        for _ in 0..10 {
            if !eng.run_once().await.unwrap() {
                break;
            }
        }

        let jobs = job_list(&pool, None, 50).await.unwrap();
        let ping = jobs.iter().find(|j| j.kind == "system.ping").unwrap();
        assert_eq!(ping.status, JobStatus::Succeeded);
        assert_eq!(ping.payload, json!({"sched": "e2e"}));

        let done_tick = jobs
            .iter()
            .find(|j| j.kind == SCHEDULER_TICK_KIND && j.status == JobStatus::Succeeded)
            .unwrap();
        assert!(done_tick.finished_at.is_some());

        // Next tick is pending, ~30s out (test_handler interval).
        let pending_tick = jobs
            .iter()
            .find(|j| j.kind == SCHEDULER_TICK_KIND && j.status == JobStatus::Pending)
            .unwrap();
        assert!(pending_tick.run_at > t + 20);
    }
}
