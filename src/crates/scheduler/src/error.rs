//! Scheduler error type.

use crate::crates::job_engine::JobError;

#[derive(Debug, thiserror::Error)]
pub enum SchedulerError {
    #[error(transparent)]
    Db(#[from] sqlx::Error),
    #[error("schedule payload is not valid JSON: {0}")]
    Payload(#[from] serde_json::Error),
    #[error("job engine error: {0}")]
    Job(#[from] JobError),
    #[error("invalid schedule: {0}")]
    Invalid(String),
}
