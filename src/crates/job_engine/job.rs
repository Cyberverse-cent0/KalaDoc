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

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn job_status_as_str_parse_roundtrip() {
        for status in [
            JobStatus::Pending,
            JobStatus::Running,
            JobStatus::Succeeded,
            JobStatus::Failed,
            JobStatus::Cancelled,
            JobStatus::Skipped,
        ] {
            assert_eq!(JobStatus::parse(status.as_str()), Some(status));
        }
    }

    #[test]
    fn job_status_parse_rejects_unknown_and_is_case_sensitive() {
        assert_eq!(JobStatus::parse("bogus"), None);
        assert_eq!(JobStatus::parse(""), None);
        assert_eq!(JobStatus::parse("Pending"), None);
        assert_eq!(JobStatus::parse(" pending"), None);
        assert_eq!(JobStatus::parse("pending "), None);
    }

    #[test]
    fn job_status_serializes_snake_case() {
        assert_eq!(
            serde_json::to_value(JobStatus::Succeeded).unwrap(),
            json!("succeeded")
        );
        assert_eq!(
            serde_json::to_value(JobStatus::Skipped).unwrap(),
            json!("skipped")
        );
        assert_eq!(
            serde_json::from_value::<JobStatus>(json!("running")).unwrap(),
            JobStatus::Running
        );
        assert!(serde_json::from_value::<JobStatus>(json!("bogus")).is_err());
    }

    #[test]
    fn batch_progress_pct_and_completion() {
        let base = BatchProgress {
            batch_id: 1,
            items_total: 4,
            items_done: 1,
            items_pending: 3,
            items_succeeded: 1,
            items_failed: 0,
            items_skipped: 0,
            items_cancelled: 0,
        };
        assert_eq!(base.pct(), 25);
        assert!(!base.is_complete());

        let done = BatchProgress {
            items_done: 4,
            items_pending: 0,
            items_failed: 2,
            items_cancelled: 1,
            ..base
        };
        assert_eq!(done.pct(), 100);
        assert!(done.is_complete());

        // Empty batch never divides by zero.
        let empty = BatchProgress {
            items_total: 0,
            items_done: 0,
            items_pending: 0,
            items_succeeded: 0,
            items_failed: 0,
            items_skipped: 0,
            items_cancelled: 0,
            ..base
        };
        assert_eq!(empty.pct(), 0);
        assert!(empty.is_complete());
    }

    #[test]
    fn new_job_is_constructible_and_cloneable() {
        let new = NewJob {
            kind: "document.ingest".into(),
            payload: json!({"path": "/tmp/a.pdf"}),
            priority: 5,
            run_at: Some(1_700_000_000),
            max_attempts: Some(7),
        };
        let cloned = new.clone();
        assert_eq!(cloned.kind, "document.ingest");
        assert_eq!(cloned.priority, 5);
        assert_eq!(cloned.max_attempts, Some(7));
        assert_eq!(cloned.payload["path"], "/tmp/a.pdf");

        let defaulted = NewJob {
            kind: "k".into(),
            payload: json!(null),
            priority: 0,
            run_at: None,
            max_attempts: None,
        };
        assert_eq!(defaulted.run_at, None);
        assert_eq!(defaulted.max_attempts, None);
    }
}
