//! Handler trait and execution context.
//!
//! Handlers are plain `Send + Sync` objects registered on the engine by
//! job `kind`. The trait uses a boxed-future method instead of
//! `async fn` so it stays object-safe without an `async-trait` dep.

use std::future::Future;
use std::pin::Pin;
use std::sync::Arc;

use sqlx::SqlitePool;

use super::job::Job;
use super::JobError;

/// Shared services handed to every handler invocation.
///
/// Cloning is cheap (the pool is reference-counted). Add fields here as
/// new services come online (storage engine, document engine) rather
/// than growing handler-specific state.
#[derive(Clone)]
pub struct JobContext {
    pub pool: SqlitePool,
}

/// A unit of work the engine can execute.
pub trait JobHandler: Send + Sync + 'static {
    /// Execute `job`. Return `Ok(())` for success; `Err` retries the
    /// job (with backoff) until `max_attempts` is exhausted.
    fn handle<'a>(
        &'a self,
        ctx: &'a JobContext,
        job: &'a Job,
    ) -> Pin<Box<dyn Future<Output = Result<(), JobError>> + Send + 'a>>;
}

/// Type-erased handler stored in the engine registry.
pub type SharedHandler = Arc<dyn JobHandler>;
