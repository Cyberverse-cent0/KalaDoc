//! Built-in job handlers registered on the engine at startup.
//!
//! Long-running entry points (e.g. `kaladoc jobs worker`) call
//! [`register_builtin`] before starting the run loop so every known
//! job `kind` has a handler. Add new handlers here as they come
//! online — job kinds are the contract between producers and
//! consumers.

use crate::crates::job_engine::JobEngine;

/// Registers all built-in job handlers on `engine`.
pub fn register_builtin(_engine: &mut JobEngine) {}
