//! Recurring-schedule service — thin async facade over
//! `scheduler::store` for the HTTP API's `/schedules` routes.
//!
//! Schedules are global (not owner-scoped): any authenticated caller
//! may list/upsert/remove them, matching the trust level of the
//! `/jobs` endpoints. If per-owner schedules are ever needed, add an
//! `owner_id` column in `scheduled_jobs` — the store schema has none.
//!
//! [`ScheduleService::connect`] runs the idempotent
//! `create_schedule_table` migration so embedders that never started
//! the scheduler CLI still get a working table; it is safe (and cheap)
//! to call per request.

use sqlx::SqlitePool;

use crate::crates::scheduler::{
    NewSchedule, Schedule, SchedulerError, TickOutcome, create_schedule_table, list_schedules,
    remove_schedule, tick, upsert_schedule,
};

/// Application-facing schedule service over the shared pool.
#[derive(Clone)]
pub struct ScheduleService {
    pool: SqlitePool,
}

impl ScheduleService {
    /// Wraps an existing pool **without** ensuring the schema — use
    /// [`ScheduleService::connect`] unless the table is known to exist.
    pub fn new(pool: SqlitePool) -> Self {
        Self { pool }
    }

    /// Ensures `scheduled_jobs` exists (idempotent `CREATE TABLE IF
    /// NOT EXISTS`) and returns a ready service.
    ///
    /// # Errors
    /// [`SchedulerError::Db`] on migration failure.
    pub async fn connect(pool: &SqlitePool) -> Result<Self, SchedulerError> {
        create_schedule_table(pool).await?;
        Ok(Self::new(pool.clone()))
    }

    /// Lists all schedules ordered by name.
    ///
    /// # Errors
    /// [`SchedulerError::Db`] on query failure.
    pub async fn list(&self) -> Result<Vec<Schedule>, SchedulerError> {
        list_schedules(&self.pool).await
    }

    /// Inserts or replaces the schedule named `new.name`.
    ///
    /// # Errors
    /// [`SchedulerError::Invalid`] on blank name/kind or
    /// `interval_secs == 0`; [`SchedulerError::Db`] on failure.
    pub async fn upsert(&self, new: &NewSchedule) -> Result<Schedule, SchedulerError> {
        upsert_schedule(&self.pool, new).await
    }

    /// Deletes the schedule named `name`; `false` when none existed.
    ///
    /// # Errors
    /// [`SchedulerError::Db`] on failure.
    pub async fn remove(&self, name: &str) -> Result<bool, SchedulerError> {
        remove_schedule(&self.pool, name).await
    }

    /// Runs one scheduler tick at `now` (Unix seconds; `None` =
    /// current time), enqueueing a job for every due schedule.
    /// Exposed for manual/administrative ticking — the engine-driven
    /// `scheduler.tick` job kind does the same in production.
    ///
    /// # Errors
    /// [`SchedulerError::Db`]/[`SchedulerError::Job`] on failure.
    pub async fn tick(&self, now: Option<i64>) -> Result<TickOutcome, SchedulerError> {
        let now = now.unwrap_or_else(|| {
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .map(|d| d.as_secs() as i64)
                .unwrap_or(0)
        });
        tick(&self.pool, now).await
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;
    use sqlx::sqlite::SqlitePoolOptions;

    async fn test_pool() -> SqlitePool {
        SqlitePoolOptions::new()
            .max_connections(1)
            .connect("sqlite::memory:")
            .await
            .expect("in-memory sqlite pool")
    }

    fn new_schedule(name: &str) -> NewSchedule {
        NewSchedule {
            name: name.into(),
            kind: "system.ping".into(),
            payload: json!({}),
            interval_secs: 60,
            priority: None,
            next_run_at: None,
            enabled: None,
        }
    }

    #[tokio::test]
    async fn connect_creates_table_and_crud_roundtrips() {
        let pool = test_pool().await;
        let svc = ScheduleService::connect(&pool).await.expect("connect");

        assert!(svc.list().await.unwrap().is_empty());

        let stored = svc.upsert(&new_schedule("gc")).await.unwrap();
        assert_eq!(stored.name, "gc");
        assert_eq!(svc.list().await.unwrap().len(), 1);

        // Upsert replaces by name.
        let mut s2 = new_schedule("gc");
        s2.interval_secs = 120;
        let stored = svc.upsert(&s2).await.unwrap();
        assert_eq!(stored.interval_secs, 120);
        assert_eq!(svc.list().await.unwrap().len(), 1);

        assert!(svc.remove("gc").await.unwrap());
        assert!(!svc.remove("gc").await.unwrap());
    }

    #[tokio::test]
    async fn invalid_input_surfaces_scheduler_error() {
        let pool = test_pool().await;
        let svc = ScheduleService::connect(&pool).await.unwrap();
        let mut bad = new_schedule("bad");
        bad.interval_secs = 0;
        assert!(matches!(
            svc.upsert(&bad).await,
            Err(SchedulerError::Invalid(_))
        ));
    }
}
