//! `kaladoc storage status|limits|sweep`
//!
//! Thin adapter over [`StorageService`]: builds the service straight
//! from the resolved storage config (no database needed) and calls
//! the status/limits/sweep APIs. Backend operations are synchronous
//! and blocking, so `sweep` runs on the blocking pool.

use std::time::Duration;

use serde::Serialize;

use crate::crates::application::services::error::ServiceError;
use crate::crates::application::services::storage::StorageService;

use super::super::args::StorageArgs;
use super::{CliError, Context};

pub async fn run(ctx: &Context, args: &StorageArgs) -> Result<i32, CliError> {
    match args {
        StorageArgs::Status => status(ctx).await,
        StorageArgs::Limits => limits(ctx).await,
        StorageArgs::Sweep { max_age_secs } => sweep(ctx, *max_age_secs).await,
    }
}

/// Builds the storage service from the strict config — same rule as
/// the `doc`/`jobs` commands (required secrets must be present).
fn service(ctx: &Context) -> Result<StorageService, CliError> {
    let config = ctx.config()?;
    StorageService::from_config(&config.to_storage_config()).map_err(svc_err)
}

/// [`doc.rs`]'s `svc_err` is private to that module; the mapping is
/// duplicated here (small, stable) rather than shared across modules.
fn svc_err(e: ServiceError) -> CliError {
    match e {
        ServiceError::Config(c) => CliError::Config(c),
        ServiceError::Io(i) => CliError::Io(i),
        ServiceError::Db(d) => CliError::Db(d),
        ServiceError::Storage(s) => CliError::Internal(s.to_string()),
        ServiceError::Store(s) => CliError::Internal(s.to_string()),
    }
}

async fn status(ctx: &Context) -> Result<i32, CliError> {
    let storage = service(ctx)?;
    let status = storage.status();

    if ctx.emit_structured(&status)? {
        return Ok(0);
    }
    println!("backend:        {}", status.backend);
    println!("root_configured: {}", status.root_configured);
    println!(
        "used_bytes:     {}",
        status
            .used_bytes
            .map(|b| b.to_string())
            .unwrap_or_else(|| "(untracked)".to_string())
    );
    println!(
        "quota_bytes:    {}",
        status
            .quota_bytes
            .map(|b| b.to_string())
            .unwrap_or_else(|| "(unlimited)".to_string())
    );
    Ok(0)
}

#[derive(Debug, Serialize)]
struct StreamLimitsView {
    max_object_bytes: Option<u64>,
    max_chunk_bytes: usize,
}

async fn limits(ctx: &Context) -> Result<i32, CliError> {
    let storage = service(ctx)?;
    let limits = storage.stream_limits();
    let view = StreamLimitsView {
        max_object_bytes: limits.max_object_bytes,
        max_chunk_bytes: limits.max_chunk_bytes,
    };

    if ctx.emit_structured(&view)? {
        return Ok(0);
    }
    println!(
        "max_object_bytes: {}",
        view.max_object_bytes
            .map(|b| b.to_string())
            .unwrap_or_else(|| "(unlimited)".to_string())
    );
    println!("max_chunk_bytes:  {}", view.max_chunk_bytes);
    Ok(0)
}

#[derive(Debug, Serialize)]
struct SweepReport {
    max_age_secs: u64,
    removed: usize,
}

async fn sweep(ctx: &Context, max_age_secs: u64) -> Result<i32, CliError> {
    if max_age_secs == 0 {
        return Err(CliError::Usage(
            "--max-age-secs must be positive".to_string(),
        ));
    }
    let mut storage = service(ctx)?;
    let max_age = Duration::from_secs(max_age_secs);

    // sweep_staging is synchronous filesystem work — run it on the
    // blocking pool with the owned service.
    let removed = tokio::task::spawn_blocking(move || storage.sweep_staging(max_age))
        .await
        .map_err(|e| CliError::Internal(format!("storage worker task failed: {e}")))?
        .map_err(svc_err)?;

    let report = SweepReport {
        max_age_secs,
        removed,
    };
    if ctx.emit_structured(&report)? {
        return Ok(0);
    }
    println!("removed {removed} staged upload file(s)");
    Ok(0)
}
