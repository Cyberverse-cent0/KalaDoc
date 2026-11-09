//! SQLite persistence for recurring schedules (`scheduled_jobs`) and
//! the `tick` routine that turns due schedules into `jobs_tbl` rows.
//!
//! All functions take `&SqlitePool`. Timestamps are Unix seconds
//! (`INTEGER`), matching `jobs_tbl` and `sessions_tbl`.

use sqlx::sqlite::SqliteRow;
use sqlx::{Executor, Row, SqlitePool};

use crate::crates::job_engine::{NewJob, enqueue as job_enqueue};

use super::SchedulerError;
use super::schedule::{NewSchedule, Schedule};

/// Job kind the scheduler uses for its own periodic tick.
pub const SCHEDULER_TICK_KIND: &str = "scheduler.tick";

/// Creates `scheduled_jobs` and its index. Idempotent.
pub async fn create_schedule_table(pool: &SqlitePool) -> Result<(), SchedulerError> {
    pool.execute(
        r#"
        CREATE TABLE IF NOT EXISTS scheduled_jobs (
            id            INTEGER PRIMARY KEY AUTOINCREMENT,
            name          TEXT NOT NULL UNIQUE,
            kind          TEXT NOT NULL,
            payload       TEXT NOT NULL,
            interval_secs INTEGER NOT NULL,
            priority      INTEGER NOT NULL DEFAULT 0,
            next_run_at   INTEGER NOT NULL,
            created_at    INTEGER NOT NULL,
            last_run_at   INTEGER,
            enabled       INTEGER NOT NULL DEFAULT 1
        );
        "#,
    )
    .await?;

    // Hot path for tick: find enabled schedules that are due.
    pool.execute(
        "CREATE INDEX IF NOT EXISTS idx_scheduled_jobs_due
         ON scheduled_jobs(enabled, next_run_at);",
    )
    .await?;

    Ok(())
}

/// Inserts or replaces the schedule named `new.name` and returns the
/// stored row. Upserting an existing name resets `next_run_at` to the
/// value in `new` (or `now + interval_secs` when omitted).
pub async fn upsert_schedule(
    pool: &SqlitePool,
    new: &NewSchedule,
) -> Result<Schedule, SchedulerError> {
    if new.name.trim().is_empty() {
        return Err(SchedulerError::Invalid(
            "schedule name must not be empty".into(),
        ));
    }
    if new.kind.trim().is_empty() {
        return Err(SchedulerError::Invalid(
            "schedule kind must not be empty".into(),
        ));
    }
    if new.interval_secs == 0 {
        return Err(SchedulerError::Invalid("interval_secs must be >= 1".into()));
    }

    let now = now_unix();
    let next_run_at = new.next_run_at.unwrap_or(now + new.interval_secs as i64);
    let payload = serde_json::to_string(&new.payload)?;
    let priority = new.priority.unwrap_or(0);
    let enabled = new.enabled.unwrap_or(true);

    sqlx::query(
        "INSERT INTO scheduled_jobs
            (name, kind, payload, interval_secs, priority, next_run_at,
             created_at, last_run_at, enabled)
         VALUES (?, ?, ?, ?, ?, ?, ?, NULL, ?)
         ON CONFLICT(name) DO UPDATE SET
            kind = excluded.kind,
            payload = excluded.payload,
            interval_secs = excluded.interval_secs,
            priority = excluded.priority,
            next_run_at = excluded.next_run_at,
            last_run_at = NULL,
            enabled = excluded.enabled",
    )
    .bind(&new.name)
    .bind(&new.kind)
    .bind(&payload)
    .bind(new.interval_secs as i64)
    .bind(priority)
    .bind(next_run_at)
    .bind(now)
    .bind(enabled)
    .execute(pool)
    .await?;

    list_schedules(pool)
        .await?
        .into_iter()
        .find(|s| s.name == new.name)
        .ok_or_else(|| SchedulerError::Invalid("upserted schedule not found".into()))
}

/// Lists all schedules ordered by name.
pub async fn list_schedules(pool: &SqlitePool) -> Result<Vec<Schedule>, SchedulerError> {
    let rows = sqlx::query(&format!(
        "{SCHEDULE_COLUMNS} FROM scheduled_jobs ORDER BY name"
    ))
    .fetch_all(pool)
    .await?;
    rows.into_iter().map(schedule_from_row).collect()
}

/// Deletes the schedule named `name`. Returns `true` when a row was
/// removed, `false` when no schedule had that name.
pub async fn remove_schedule(pool: &SqlitePool, name: &str) -> Result<bool, SchedulerError> {
    let done = sqlx::query("DELETE FROM scheduled_jobs WHERE name = ?")
        .bind(name)
        .execute(pool)
        .await?;
    Ok(done.rows_affected() > 0)
}

/// Counts due schedules enqueued by one [`tick`] call.
#[derive(Debug, Default, Clone, Copy, PartialEq, Eq)]
pub struct TickOutcome {
    /// Jobs enqueued into `jobs_tbl`.
    pub enqueued: u64,
}

/// Enqueues one `scheduler.tick` job so the engine drives the
/// scheduler. Safe to call at startup and after every restart — an
/// extra tick job is harmless because [`tick`] is idempotent.
///
/// `run_at` defaults to now; pass `Some(now + interval)` to schedule
/// the *next* tick after the current one (used by
/// [`crate::crates::scheduler::SchedulerTickHandler`]).
pub async fn enqueue_tick(
    pool: &SqlitePool,
    run_at: Option<i64>,
    max_attempts: Option<u32>,
) -> Result<i64, SchedulerError> {
    let job = job_enqueue(
        pool,
        &NewJob {
            kind: SCHEDULER_TICK_KIND.into(),
            payload: serde_json::json!({}),
            priority: 0,
            run_at,
            max_attempts,
        },
        max_attempts.unwrap_or(1),
    )
    .await?;
    Ok(job.id)
}

/// Fires every schedule whose `next_run_at <= now`.
///
/// For each due schedule this function, **atomically**:
///
/// 1. advances `next_run_at` past `now` (by whole `interval_secs`
///    steps, so downtime does not burst-fire missed occurrences) and
///    stamps `last_run_at`, then
/// 2. enqueues a `jobs_tbl` row with the schedule's kind/payload at
///    the occurrence time.
///
/// The `UPDATE ... WHERE next_run_at = ?` guard means two racing
/// ticks cannot double-enqueue the same occurrence — the loser sees
/// `rows_affected() == 0` and skips the insert. Returns how many jobs
/// were enqueued.
pub async fn tick(pool: &SqlitePool, now: i64) -> Result<TickOutcome, SchedulerError> {
    let rows = sqlx::query(&format!(
        "{SCHEDULE_COLUMNS} FROM scheduled_jobs
         WHERE enabled = 1 AND next_run_at <= ?
         ORDER BY id"
    ))
    .bind(now)
    .fetch_all(pool)
    .await?;

    let mut enqueued = 0u64;
    for row in rows {
        let sched = schedule_from_row(row)?;
        let next = advance_past(sched.next_run_at, sched.interval_secs, now);

        // Claim the occurrence: only proceed if next_run_at is still
        // the value we read (no other tick advanced it meanwhile).
        let claimed = sqlx::query(
            "UPDATE scheduled_jobs
             SET next_run_at = ?, last_run_at = ?
             WHERE id = ? AND next_run_at = ?",
        )
        .bind(next)
        .bind(now)
        .bind(sched.id)
        .bind(sched.next_run_at)
        .execute(pool)
        .await?
        .rows_affected();

        if claimed == 0 {
            continue;
        }

        job_enqueue(
            pool,
            &NewJob {
                kind: sched.kind.clone(),
                payload: sched.payload.clone(),
                priority: sched.priority,
                run_at: Some(sched.next_run_at),
                max_attempts: None,
            },
            1,
        )
        .await?;
        enqueued += 1;
    }

    Ok(TickOutcome { enqueued })
}

/// Smallest `t > now` reachable from `start` by adding `interval`
/// steps. Uses saturating arithmetic so a pathological interval
/// cannot overflow.
fn advance_past(start: i64, interval: u64, now: i64) -> i64 {
    let interval = interval.max(1) as i64;
    if start > now {
        return start;
    }
    // steps needed so that start + steps*interval > now
    let elapsed = now.saturating_sub(start);
    let steps = elapsed / interval + 1;
    start.saturating_add(steps.saturating_mul(interval))
}

const SCHEDULE_COLUMNS: &str =
    "SELECT id, name, kind, payload, interval_secs, priority, next_run_at,
            created_at, last_run_at, enabled";

fn schedule_from_row(row: SqliteRow) -> Result<Schedule, SchedulerError> {
    let payload_text: String = row.get("payload");
    Ok(Schedule {
        id: row.get("id"),
        name: row.get("name"),
        kind: row.get("kind"),
        payload: serde_json::from_str(&payload_text)?,
        interval_secs: row.get::<i64, _>("interval_secs") as u64,
        priority: row.get("priority"),
        next_run_at: row.get("next_run_at"),
        created_at: row.get("created_at"),
        last_run_at: row.get("last_run_at"),
        enabled: row.get::<i64, _>("enabled") != 0,
    })
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
    use crate::crates::job_engine::{create_jobs_table, list_jobs as job_list};
    use serde_json::json;
    use sqlx::sqlite::SqlitePoolOptions;

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

    fn new_schedule(name: &str, interval: u64) -> NewSchedule {
        NewSchedule {
            name: name.into(),
            kind: "system.ping".into(),
            payload: json!({"n": name}),
            interval_secs: interval,
            priority: None,
            next_run_at: None,
            enabled: None,
        }
    }

    #[tokio::test]
    async fn tick_enqueues_due_schedule_once() {
        let pool = test_pool().await;
        let now = now_unix();
        let mut s = new_schedule("gc", 60);
        s.next_run_at = Some(now - 10);
        upsert_schedule(&pool, &s).await.unwrap();

        let out = tick(&pool, now).await.unwrap();
        assert_eq!(out.enqueued, 1);

        // Second tick at same `now` must not double-enqueue.
        let out = tick(&pool, now).await.unwrap();
        assert_eq!(out.enqueued, 0);

        let jobs = job_list(&pool, None, 10).await.unwrap();
        assert_eq!(jobs.len(), 1);
        assert_eq!(jobs[0].kind, "system.ping");
        assert_eq!(jobs[0].payload, json!({"n": "gc"}));
        assert_eq!(jobs[0].run_at, now - 10);

        let sched = &list_schedules(&pool).await.unwrap()[0];
        assert_eq!(sched.last_run_at, Some(now));
        assert!(sched.next_run_at > now);
    }

    #[tokio::test]
    async fn missed_intervals_do_not_burst_fire() {
        let pool = test_pool().await;
        let now = now_unix();
        let mut s = new_schedule("hourly", 3600);
        s.next_run_at = Some(now - 10_000); // ~3 intervals missed
        upsert_schedule(&pool, &s).await.unwrap();

        let out = tick(&pool, now).await.unwrap();
        assert_eq!(out.enqueued, 1); // exactly one occurrence

        let jobs = job_list(&pool, None, 10).await.unwrap();
        assert_eq!(jobs.len(), 1);

        let sched = &list_schedules(&pool).await.unwrap()[0];
        // next_run_at advanced past now by whole intervals.
        assert!(sched.next_run_at > now);
        assert_eq!((sched.next_run_at - (now - 10_000)) % 3600, 0);
    }

    #[tokio::test]
    async fn upsert_replaces_and_remove_deletes() {
        let pool = test_pool().await;
        let mut s = new_schedule("job", 60);
        s.payload = json!({"v": 1});
        upsert_schedule(&pool, &s).await.unwrap();

        let mut s2 = new_schedule("job", 120);
        s2.payload = json!({"v": 2});
        s2.enabled = Some(false);
        let stored = upsert_schedule(&pool, &s2).await.unwrap();
        assert_eq!(stored.interval_secs, 120);
        assert_eq!(stored.payload, json!({"v": 2}));
        assert!(!stored.enabled);
        assert_eq!(list_schedules(&pool).await.unwrap().len(), 1);

        // Disabled schedule never fires.
        let out = tick(&pool, now_unix() + 10_000).await.unwrap();
        assert_eq!(out.enqueued, 0);

        assert!(remove_schedule(&pool, "job").await.unwrap());
        assert!(!remove_schedule(&pool, "job").await.unwrap());
        assert!(list_schedules(&pool).await.unwrap().is_empty());
    }

    #[tokio::test]
    async fn invalid_schedule_rejected() {
        let pool = test_pool().await;
        let mut s = new_schedule("bad", 0);
        assert!(upsert_schedule(&pool, &s).await.is_err());
        s = new_schedule("  ", 60);
        assert!(upsert_schedule(&pool, &s).await.is_err());
    }

    #[tokio::test]
    async fn tick_job_drives_scheduler_end_to_end() {
        use crate::crates::job_engine::{JobEngine, JobEngineConfig, JobStatus};
        use crate::crates::scheduler::{SchedulerConfig, SchedulerTickHandler};
        use std::sync::Arc;

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
        eng.register(
            SCHEDULER_TICK_KIND,
            Arc::new(SchedulerTickHandler::new(SchedulerConfig {
                tick_interval_secs: 60,
                tick_priority: 0,
                tick_max_attempts: None,
            })),
        );

        // A due schedule plus the first tick job.
        let now = now_unix();
        let mut s = new_schedule("gc", 60);
        s.next_run_at = Some(now - 5);
        upsert_schedule(&pool, &s).await.unwrap();
        enqueue_tick(&pool, Some(now), None).await.unwrap();

        // Drain due work: the tick job runs tick() (enqueues the
        // system.ping job and the next tick job) — loop until empty.
        for _ in 0..10 {
            if !eng.run_once().await.unwrap() {
                break;
            }
        }

        let jobs = job_list(&pool, None, 50).await.unwrap();
        let pings: Vec<_> = jobs.iter().filter(|j| j.kind == "system.ping").collect();
        assert_eq!(pings.len(), 1);
        let ticks_done: Vec<_> = jobs
            .iter()
            .filter(|j| j.kind == SCHEDULER_TICK_KIND && j.status == JobStatus::Succeeded)
            .collect();
        assert_eq!(ticks_done.len(), 1);

        // The next tick job is scheduled ~60s out, not due yet.
        let future_tick = jobs
            .iter()
            .find(|j| j.kind == SCHEDULER_TICK_KIND && j.status == JobStatus::Pending)
            .unwrap();
        assert!(future_tick.run_at > now + 30);
    }
}
