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
}

impl JobStatus {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Pending => "pending",
            Self::Running => "running",
            Self::Succeeded => "succeeded",
            Self::Failed => "failed",
            Self::Cancelled => "cancelled",
        }
    }

    pub fn parse(s: &str) -> Option<Self> {
        match s {
            "pending" => Some(Self::Pending),
            "running" => Some(Self::Running),
            "succeeded" => Some(Self::Succeeded),
            "failed" => Some(Self::Failed),
            "cancelled" => Some(Self::Cancelled),
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
