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
