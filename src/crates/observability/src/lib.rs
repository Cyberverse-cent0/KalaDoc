//! Observability — tracing setup, request IDs, and health aggregation.
//!
//! - [`logging`]: one-time global `tracing` subscriber installation
//!   (`init_tracing`), idempotent and `RUST_LOG`-aware.
//! - [`request_id`]: validated UUID v4 correlation IDs with serde
//!   support.
//! - [`health`]: per-component health results and worst-status-wins
//!   aggregation for `doctor`/`system_health` reporting.
//!
//! This crate never installs a subscriber on its own — binaries call
//! [`logging::init_tracing`] explicitly.

pub mod health;
pub mod logging;
pub mod request_id;

pub use health::{ComponentHealth, HealthStatus, OverallHealth, aggregate};
pub use logging::{LoggingError, build_env_filter, init_tracing, init_tracing_with_level_filter};
pub use request_id::RequestId;
