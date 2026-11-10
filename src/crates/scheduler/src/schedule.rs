//! Schedule domain types.

use serde_json::Value;

/// A row from `scheduled_jobs` — a recurring job definition.
#[derive(Debug, Clone)]
pub struct Schedule {
    pub id: i64,
    /// Human-readable unique name used as the upsert key.
    pub name: String,
    /// Job kind handed to the engine, e.g. `"storage.gc_staging"`.
    pub kind: String,
    /// JSON payload copied into every enqueued occurrence.
    pub payload: Value,
    /// Seconds between occurrences.
    pub interval_secs: u64,
    /// Priority passed to the enqueued job.
    pub priority: i64,
    /// Unix seconds — next time an occurrence should be enqueued.
    pub next_run_at: i64,
    /// Unix seconds.
    pub created_at: i64,
    /// Unix seconds of the most recent enqueue, if any.
    pub last_run_at: Option<i64>,
    /// `false` pauses the schedule without deleting it.
    pub enabled: bool,
}

/// Input for [`crate::crates::scheduler::upsert_schedule`].
///
/// `next_run_at` defaults to `now + interval_secs` when omitted; set it
/// explicitly to control when the first occurrence fires.
#[derive(Debug, Clone)]
pub struct NewSchedule {
    /// Unique name; upserting with an existing name replaces the row.
    pub name: String,
    pub kind: String,
    pub payload: Value,
    pub interval_secs: u64,
    /// `None` = 0.
    pub priority: Option<i64>,
    /// Unix seconds for the first occurrence. `None` =
    /// `now + interval_secs`.
    pub next_run_at: Option<i64>,
    /// `None` = `true`.
    pub enabled: Option<bool>,
}

#[cfg(test)]
mod tests {
    //! Coverage for schedule creation validation, `next_run_at`
    //! computation, payload (de)serialization, and the enabled
    //! pause/resume lifecycle.
    //!
    //! Semantics documented by these tests:
    //! - There is **no cron support** — recurrence is a fixed
    //!   `interval_secs` counted in Unix seconds. No timezone or DST
    //!   handling exists; all timestamps are absolute Unix seconds.
    //! - There is **no "completed" state** — a schedule recurs
    //!   forever. The only lifecycle transitions are
    //!   enabled → disabled (pause) → enabled (resume) via
    //!   `upsert_schedule`, or deletion via `remove_schedule`.
    //! - `Schedule`/`NewSchedule` do **not** implement serde
    //!   traits; the only serialized field is `payload` (a
    //!   `serde_json::Value` stored as TEXT in `scheduled_jobs`).

    use super::*;
    use crate::crates::job_engine::create_jobs_table;
    use crate::crates::scheduler::{create_schedule_table, list_schedules, tick, upsert_schedule};
    use serde_json::json;
    use sqlx::SqlitePool;
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
    async fn upsert_applies_documented_defaults() {
        let pool = test_pool().await;
        let before = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_secs() as i64;

        let stored = upsert_schedule(&pool, &new_schedule("defaults", 300))
            .await
            .unwrap();
        let after = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_secs() as i64;

        // None priority -> 0, None enabled -> true, None
        // next_run_at -> created_at + interval_secs.
        assert_eq!(stored.priority, 0);
        assert!(stored.enabled);
        assert_eq!(stored.last_run_at, None);
        assert!(stored.id > 0);
        assert!(stored.created_at >= before && stored.created_at <= after);
        // next_run_at defaults to `now + interval_secs` where `now`
        // is the same timestamp stored as created_at.
        assert_eq!(stored.next_run_at - stored.created_at, 300);
        assert!(stored.next_run_at >= before + 300 && stored.next_run_at <= after + 300);
    }

    #[tokio::test]
    async fn explicit_next_run_at_is_stored_verbatim() {
        let pool = test_pool().await;
        let t = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_secs() as i64;

        // Past next_run_at: stored exactly, fires on next tick.
        let mut past = new_schedule("past", 60);
        past.next_run_at = Some(t - 100);
        let stored = upsert_schedule(&pool, &past).await.unwrap();
        assert_eq!(stored.next_run_at, t - 100);

        // Far-future next_run_at: stored exactly, never fires early.
        let mut future = new_schedule("future", 60);
        future.next_run_at = Some(t + 86_400);
        let stored = upsert_schedule(&pool, &future).await.unwrap();
        assert_eq!(stored.next_run_at, t + 86_400);

        let out = tick(&pool, t).await.unwrap();
        assert_eq!(out.enqueued, 1); // only "past" is due
    }

    #[tokio::test]
    async fn payload_json_roundtrips_through_storage() {
        let pool = test_pool().await;
        let mut s = new_schedule("payload", 60);
        s.payload = json!({
            "nested": {"a": [1, 2, 3], "b": null, "c": true},
            "unicode": "héllo wörld — ünïcödé",
            "float": 1.5,
            "empty_obj": {},
            "empty_arr": []
        });
        upsert_schedule(&pool, &s).await.unwrap();

        let listed = list_schedules(&pool).await.unwrap();
        assert_eq!(listed.len(), 1);
        assert_eq!(listed[0].payload, s.payload);

        // The TEXT column must hold valid JSON that re-parses to an
        // identical Value (serde roundtrip of the only serialized
        // field on the schedule row).
        let text = serde_json::to_string(&listed[0].payload).unwrap();
        let reparsed: Value = serde_json::from_str(&text).unwrap();
        assert_eq!(reparsed, s.payload);
    }

    #[tokio::test]
    async fn pause_and_resume_via_enabled_flag() {
        let pool = test_pool().await;
        let t = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_secs() as i64;

        // Create paused with a due next_run_at.
        let mut s = new_schedule("toggle", 60);
        s.next_run_at = Some(t - 1);
        s.enabled = Some(false);
        let stored = upsert_schedule(&pool, &s).await.unwrap();
        assert!(!stored.enabled);

        // Paused (disabled) schedules are skipped by tick.
        let out = tick(&pool, t + 10).await.unwrap();
        assert_eq!(out.enqueued, 0);

        // Resume: upsert same name with enabled = true (and a due
        // next_run_at again, since upsert replaces the row).
        let mut s2 = new_schedule("toggle", 60);
        s2.next_run_at = Some(t - 1);
        s2.enabled = Some(true);
        let stored = upsert_schedule(&pool, &s2).await.unwrap();
        assert!(stored.enabled);
        // Upsert resets last_run_at to NULL.
        assert_eq!(stored.last_run_at, None);

        let out = tick(&pool, t + 10).await.unwrap();
        assert_eq!(out.enqueued, 1);
    }

    #[tokio::test]
    async fn interval_bounds_minimum_accepted() {
        let pool = test_pool().await;
        // interval_secs = 1 is the smallest legal interval.
        let stored = upsert_schedule(&pool, &new_schedule("min", 1))
            .await
            .unwrap();
        assert_eq!(stored.interval_secs, 1);
        // There is no documented upper bound on interval_secs; see
        // the ignored tests below for the > i64::MAX overflow bug.
    }

    /// BUG: `upsert_schedule` computes
    /// `next_run_at = now + interval_secs as i64` with plain `+`.
    /// For `interval_secs = i64::MAX` the addition overflows i64 and
    /// panics in debug builds (wraps in release). A correct
    /// implementation should saturate.
    #[tokio::test]
    #[ignore = "bug: next_run_at = now + interval_secs as i64 overflows i64 for interval_secs near i64::MAX"]
    async fn huge_interval_does_not_overflow_next_run_at() {
        let pool = test_pool().await;
        let stored = upsert_schedule(&pool, &new_schedule("huge", i64::MAX as u64))
            .await
            .expect("upsert should not panic/overflow");
        let now = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_secs() as i64;
        assert!(stored.next_run_at > now);
    }

    /// BUG: for `interval_secs > i64::MAX` the `as i64` casts wrap
    /// negative. `next_run_at` becomes `now - 1` (fires immediately
    /// instead of ~never) and `advance_past`'s `interval as i64`
    /// goes negative, so `next_run_at` lands at `tick_now - 1` —
    /// still due — and every subsequent tick re-enqueues the same
    /// occurrence forever (burst-fire).
    #[tokio::test]
    #[ignore = "bug: interval_secs > i64::MAX wraps negative via `as i64`; schedule fires immediately then burst-fires every tick"]
    async fn interval_above_i64_max_does_not_burst_fire() {
        let pool = test_pool().await;
        let t = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_secs() as i64;

        let stored = upsert_schedule(&pool, &new_schedule("wrapped", u64::MAX))
            .await
            .unwrap();
        // Sanity: the row round-trips the u64 interval.
        assert_eq!(stored.interval_secs, u64::MAX);

        let out = tick(&pool, t).await.unwrap();
        assert_eq!(out.enqueued, 1);

        let sched = &list_schedules(&pool).await.unwrap()[0];
        // Correct behavior: next occurrence must be in the future.
        assert!(
            sched.next_run_at > t,
            "next_run_at {:?} should be > now {:?} — negative interval math keeps it due",
            sched.next_run_at,
            t
        );

        // And a second tick must not re-fire the same occurrence.
        let out = tick(&pool, t).await.unwrap();
        assert_eq!(out.enqueued, 0);
    }
}
