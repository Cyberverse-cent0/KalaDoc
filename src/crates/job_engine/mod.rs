//! Durable job execution engine.
//!
//! Jobs are rows in `jobs_tbl` (SQLite). Producers call
//! [`JobEngine::enqueue`]; the engine's [`JobEngine::run`] loop claims
//! due jobs atomically and dispatches them to [`JobHandler`]s
//! registered by kind. Failed jobs retry with exponential backoff up
//! to `max_attempts`, then become `failed`.
//!
//! See `.doc/job_engine.md` for usage examples.

mod engine;
mod error;
mod handler;
mod job;
mod store;

pub use engine::{JobEngine, JobEngineConfig};
pub use error::JobError;
pub use handler::{JobContext, JobHandler, SharedHandler};
pub use job::{BatchProgress, Job, JobStatus, NewJob};
pub use store::{
    batch_jobs, batch_progress, cancel_batch, cancel_job, claim_next, complete, create_jobs_table,
    enqueue, enqueue_batch, list_jobs, report_progress, requeue_running, retry_job, skip_job,
};
