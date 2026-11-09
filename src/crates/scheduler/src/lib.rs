//! Recurring job support for the job engine.
//!
//! Schedules are rows in `scheduled_jobs` (SQLite) created via
//! [`create_schedule_table`]. [`tick`] scans for due schedules,
//! atomically advances their `next_run_at`, and enqueues a regular
//! job for each — so a missed interval (downtime) never double-fires.
//!
//! Driving: register [`SchedulerTickHandler`] on the [`JobEngine`]
//! under the [`SCHEDULER_TICK_KIND`] kind and enqueue the first tick
//! with [`enqueue_tick`]. Each tick enqueues the next one, so the
//! engine itself keeps the scheduler alive — no separate loop needed.
//! Schedules persist across restarts; on startup call [`enqueue_tick`]
//! once (it is safe to call again — an extra tick job is harmless
//! because [`tick`] is idempotent).
//!
//! See `.doc/job_engine.md` for the underlying engine; see
//! `.doc/scheduler.md` for scheduling examples.

mod error;
mod handler;
mod schedule;
mod store;

pub use error::SchedulerError;
pub use handler::{SchedulerConfig, SchedulerTickHandler};
pub use schedule::{NewSchedule, Schedule};
pub use store::{
    SCHEDULER_TICK_KIND, TickOutcome, create_schedule_table, enqueue_tick, list_schedules,
    remove_schedule, tick, upsert_schedule,
};
