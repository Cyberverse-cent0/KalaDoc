//! Job engine error type.

#[derive(Debug, thiserror::Error)]
pub enum JobError {
    #[error(transparent)]
    Db(#[from] sqlx::Error),
    #[error("job payload is not valid JSON: {0}")]
    Payload(#[from] serde_json::Error),
    #[error("no handler registered for job kind {0:?}")]
    NoHandler(String),
    /// Handler-reported failure; the message is stored in
    /// `jobs_tbl.last_error` and triggers a retry.
    #[error("{0}")]
    Failed(String),
}
