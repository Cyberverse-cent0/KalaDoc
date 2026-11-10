//! `kaladoc serve [--addr <socket-addr>] [--worker]`
//!
//! Starts the HTTP API ([`crate::crates::api`]) on the configured
//! `api.addr` (or the `--addr` override) until the process is
//! interrupted. State is built once at startup via
//! [`ApiState::init`]; graceful shutdown on Ctrl+C / SIGTERM is
//! handled inside [`crate::crates::api::serve`].
//!
//! `--worker` (alias `--embedded-worker`) additionally spawns the
//! durable job worker in the same process — the same engine
//! `kaladoc jobs worker` runs, including scheduler ticks — so queued
//! jobs are processed without a separate worker process.

use std::net::SocketAddr;
use std::sync::Arc;

use crate::crates::api::{self, ApiState};

use super::{CliError, Context};

pub async fn run(ctx: &Context, addr: Option<&str>, worker: bool) -> Result<i32, CliError> {
    let config = ctx.config()?;

    let bind: SocketAddr = match addr {
        Some(raw) => raw
            .parse()
            .map_err(|_| CliError::Usage(format!("invalid --addr {raw:?}: expected host:port")))?,
        None => config.api.addr.parse().map_err(|_| {
            CliError::Usage(format!(
                "invalid api.addr {:?}: expected a socket address like 127.0.0.1:8080",
                config.api.addr
            ))
        })?,
    };

    // Spawn the embedded worker before binding so a worker setup
    // failure (bad DB config, unrecoverable engine state) fails fast
    // instead of leaving a half-started server.
    if worker {
        // The returned Arc can be dropped: the spawned task holds its
        // own reference and keeps polling until the process exits.
        super::jobs::spawn_worker(ctx).await?;
        eprintln!("embedded job worker running alongside the HTTP server");
    }

    let state = ApiState::init(&config).await?;
    api::serve(Arc::new(state), bind).await?;
    Ok(0)
}
