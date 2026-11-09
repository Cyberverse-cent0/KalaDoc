//! Command handlers — one module per command group.
//!
//! Handlers return the process exit code (`0` on success) or a
//! [`CliError`]. JSON results go through [`Context::emit_json`]; table
//! output is printed directly. Diagnostics always go to stderr.

pub mod auth;
pub mod config;
pub mod doc;
pub mod doctor;
pub mod engine;
pub mod init;
pub mod jobs;
pub mod serve;
pub mod user;
pub mod version;

pub(crate) use super::{CliError, Context};
