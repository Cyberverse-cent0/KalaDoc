//! Job queue domain types.

use serde::{Deserialize, Serialize};
use serde_json::Value;

/// Lifecycle states stored in `jobs_tbl.status`.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum JobStatus {
    /// Waiting for `run_at` to pass; eligible for claiming.
    Pending,
    /// Claimed by a worker and currently executing.
    Running,
    /// Handler returned `Ok`.
    Succeeded,
    /// Handler failed on the final allowed attempt.
    Failed,
    /// Cancelled before it ran (reserved for future use).
    Cancelled,
    /// Handler reported the item as intentionally not processed
    /// (e.g. a batch item that was already up to date). Terminal,
    /// distinct from `succeeded` so batch aggregates can tell
    /// "did work" apart from "nothing to do".
    Skipped,
}

impl JobStatus {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Pending => "pending",
            Self::Running => "running",
            Self::Succeeded => "succeeded",
            Self::Failed => "failed",
            Self::Cancelled => "cancelled",
            Self::Skipped => "skipped",
        }
    }

    pub fn parse(s: &str) -> Option<Self> {
        match s {
            "pending" => Some(Self::Pending),
            "running" => Some(Self::Running),
            "succeeded" => Some(Self::Succeeded),
            "failed" => Some(Self::Failed),
            "cancelled" => Some(Self::Cancelled),
            "skipped" => Some(Self::Skipped),
            _ => None,
        }
    }
}

/// A row from `jobs_tbl`.
#[derive(Debug, Clone)]
pub struct Job {
    pub id: i64,
    /// Handler registry key, e.g. `"document.ingest"`.
    pub kind: String,
    /// JSON payload handed to the handler.
    pub payload: Value,
    pub status: JobStatus,
    /// Higher runs first among jobs due at the same time.
    pub priority: i64,
    /// Claims so far (incremented at claim time).
    pub attempts: u32,
    pub max_attempts: u32,
    /// Unix seconds — earliest time the job may be claimed.
    pub run_at: i64,
    /// Unix seconds.
    pub created_at: i64,
    pub started_at: Option<i64>,
    pub finished_at: Option<i64>,
    /// Error message from the most recent failed attempt.
    pub last_error: Option<String>,
    /// Worker id that claimed the job.
    pub worker_id: Option<String>,
    /// Groups jobs enqueued together via
    /// [`crate::crates::job_engine::enqueue_batch`]. `None` for
    /// ordinary single jobs. The value is the `id` of the batch's
    /// first job, so it is stable and needs no extra table.
    pub batch_id: Option<i64>,
    /// Latest progress snapshot reported by the running handler via
    /// [`crate::crates::job_engine::JobContext::report_progress`].
    /// Arbitrary JSON (convention: an object like
    /// `{"pct": 40, "detail": "..."}`); `None` until first reported.
    /// Cleared when the job is (re)claimed for a new attempt.
    pub progress: Option<Value>,
}

/// Aggregate view of one batch (all jobs sharing a `batch_id`).
///
/// `items_done` counts jobs in a terminal state (`succeeded`,
/// `failed`, `skipped`, `cancelled`); `items_pending` counts jobs
/// still queued or running. `items_total` is the batch size at
/// enqueue time and never shrinks — a batch with no jobs returns
/// `None` from [`crate::crates::job_engine::batch_progress`] instead.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct BatchProgress {
    pub batch_id: i64,
    pub items_total: u64,
    pub items_done: u64,
    pub items_pending: u64,
    pub items_succeeded: u64,
    pub items_failed: u64,
    pub items_skipped: u64,
    pub items_cancelled: u64,
}

impl BatchProgress {
    /// Whole-batch completion percentage, 0–100. A finished batch
    /// reports 100 even if every item failed — check the per-status
    /// counts for outcome.
    pub fn pct(&self) -> u64 {
        if self.items_total == 0 {
            return 0;
        }
        self.items_done.saturating_mul(100) / self.items_total
    }

    /// `true` once every item reached a terminal state.
    pub fn is_complete(&self) -> bool {
        self.items_pending == 0
    }
}

/// Input for [`crate::crates::job_engine::enqueue`].
#[derive(Debug, Clone)]
pub struct NewJob {
    pub kind: String,
    pub payload: Value,
    pub priority: i64,
    /// Delay before the job becomes claimable. `None` = now.
    pub run_at: Option<i64>,
    /// `None` uses the engine/config default.
    pub max_attempts: Option<u32>,
}
