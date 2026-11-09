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
