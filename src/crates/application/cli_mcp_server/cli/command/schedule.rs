//! `kaladoc schedule list|create|delete|tick`
//!
//! Thin adapter over [`ScheduleService`] (which wraps the
//! `scheduler::store` functions): schedules are fixed-interval job
//! definitions stored in `scheduled_jobs`; `tick` fires every due
//! schedule once. The schema is ensured via
//! [`ScheduleService::connect`] so the command works on databases
//! where the scheduler was never started.

use serde::Serialize;
use serde_json::Value;

use crate::crates::application::Organization::init_db;
use crate::crates::application::services::schedules::ScheduleService;
use crate::crates::scheduler::{NewSchedule, Schedule};

use super::super::args::ScheduleArgs;
use super::{CliError, Context};

pub async fn run(ctx: &Context, args: &ScheduleArgs) -> Result<i32, CliError> {
    let db = ctx.db_config()?;
    let pool = init_db::init_db_with(&db).await?;
    let schedules = ScheduleService::connect(&pool).await?;

    let result = match args {
        ScheduleArgs::List => list(ctx, &schedules).await,
        ScheduleArgs::Create {
            name,
            kind,
            payload,
            interval_secs,
            priority,
            next_run_at,
            disabled,
        } => {
            create(
                ctx,
                &schedules,
                name,
                kind,
                payload.as_deref(),
                *interval_secs,
                *priority,
                *next_run_at,
                *disabled,
            )
            .await
        }
        ScheduleArgs::Delete { name } => delete(ctx, &schedules, name).await,
        ScheduleArgs::Tick { now } => tick(ctx, &schedules, *now).await,
    };

    pool.close().await;
    result
}

/// Serializable view of a [`Schedule`] — `Schedule` itself is not
/// `Serialize` (payload is a `serde_json::Value`).
#[derive(Debug, Serialize)]
struct ScheduleView {
    id: i64,
    name: String,
    kind: String,
    payload: Value,
    interval_secs: u64,
    priority: i64,
    next_run_at: i64,
    created_at: i64,
    last_run_at: Option<i64>,
    enabled: bool,
}

impl From<&Schedule> for ScheduleView {
    fn from(s: &Schedule) -> Self {
        Self {
            id: s.id,
            name: s.name.clone(),
            kind: s.kind.clone(),
            payload: s.payload.clone(),
            interval_secs: s.interval_secs,
            priority: s.priority,
            next_run_at: s.next_run_at,
            created_at: s.created_at,
            last_run_at: s.last_run_at,
            enabled: s.enabled,
        }
    }
}

async fn list(ctx: &Context, schedules: &ScheduleService) -> Result<i32, CliError> {
    let rows = schedules.list().await?;
    let views: Vec<ScheduleView> = rows.iter().map(ScheduleView::from).collect();

    if ctx.emit_structured(&views)? {
        return Ok(0);
    }
    println!(
        "{:<24} {:<24} {:<10} {:<12} {:<12} {:<6} next_run_at",
        "name", "kind", "interval_s", "priority", "last_run_at", "enabled"
    );
    for s in &views {
        println!(
            "{:<24} {:<24} {:<10} {:<12} {:<12} {:<6} {}",
            s.name,
            s.kind,
            s.interval_secs,
            s.priority,
            s.last_run_at
                .map(|t| t.to_string())
                .unwrap_or_else(|| "-".to_string()),
            s.enabled,
            s.next_run_at
        );
    }
    if views.is_empty() {
        println!("(no schedules)");
    }
    Ok(0)
}

#[derive(Debug, Serialize)]
struct ScheduleReport {
    schedule: ScheduleView,
}

async fn create(
    ctx: &Context,
    schedules: &ScheduleService,
    name: &str,
    kind: &str,
    payload: Option<&str>,
    interval_secs: u64,
    priority: Option<i64>,
    next_run_at: Option<i64>,
    disabled: bool,
) -> Result<i32, CliError> {
    let payload: Value = match payload {
        Some(p) => serde_json::from_str(p)?,
        None => Value::Object(Default::default()),
    };
    if interval_secs == 0 {
        return Err(CliError::Usage(
            "--interval-secs must be positive".to_string(),
        ));
    }

    let new = NewSchedule {
        name: name.to_string(),
        kind: kind.to_string(),
        payload,
        interval_secs,
        priority,
        next_run_at,
        enabled: disabled.then_some(false),
    };
    let schedule = schedules.upsert(&new).await?;
    let report = ScheduleReport {
        schedule: ScheduleView::from(&schedule),
    };

    if ctx.emit_structured(&report)? {
        return Ok(0);
    }
    println!(
        "{} schedule {} (kind {}, every {}s, next run at {})",
        if schedule.enabled {
            "created"
        } else {
            "created (paused)"
        },
        schedule.name,
        schedule.kind,
        schedule.interval_secs,
        schedule.next_run_at
    );
    Ok(0)
}

#[derive(Debug, Serialize)]
struct DeleteReport {
    name: String,
    deleted: bool,
}

async fn delete(ctx: &Context, schedules: &ScheduleService, name: &str) -> Result<i32, CliError> {
    let deleted = schedules.remove(name).await?;
    let report = DeleteReport {
        name: name.to_string(),
        deleted,
    };

    if ctx.emit_structured(&report)? {
        return Ok(0);
    }
    if deleted {
        println!("deleted schedule {name}");
    } else {
        println!("no schedule named {name}");
    }
    Ok(0)
}

#[derive(Debug, Serialize)]
struct TickReport {
    enqueued: u64,
}

async fn tick(
    ctx: &Context,
    schedules: &ScheduleService,
    now: Option<i64>,
) -> Result<i32, CliError> {
    let outcome = schedules.tick(now).await?;
    let report = TickReport {
        enqueued: outcome.enqueued,
    };

    if ctx.emit_structured(&report)? {
        return Ok(0);
    }
    println!("tick complete: {} job(s) enqueued", outcome.enqueued);
    Ok(0)
}
