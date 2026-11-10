//! Command handlers — one module per command group.
//!
//! Handlers return the process exit code (`0` on success) or a
//! [`CliError`]. JSON results go through [`Context::emit_json`]; table
//! output is printed directly. Diagnostics always go to stderr.

pub mod auth;
pub mod config;
pub mod convert;
pub mod doc;
pub mod doctor;
pub mod engine;
pub mod image;
pub mod init;
pub mod jobs;
pub mod mcp;
pub mod media;
pub mod pdf;
pub mod pipeline;
pub mod schedule;
pub mod serve;
pub mod storage;
pub mod user;
pub mod version;
pub mod video;

pub(crate) use super::{CliError, Context};
