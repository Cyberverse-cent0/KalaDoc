//! `kaladoc serve [--addr <socket-addr>]`
//!
//! Starts the HTTP API ([`crate::crates::api`]) on the configured
//! `api.addr` (or the `--addr` override) until the process is
//! interrupted. State is built once at startup via
//! [`ApiState::init`]; graceful shutdown on Ctrl+C / SIGTERM is
//! handled inside [`crate::crates::api::serve`].

use std::net::SocketAddr;
use std::sync::Arc;

use crate::crates::api::{self, ApiState};

use super::{CliError, Context};

pub async fn run(ctx: &Context, addr: Option<&str>) -> Result<i32, CliError> {
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

    let state = ApiState::init(&config).await?;
    api::serve(Arc::new(state), bind).await?;
    Ok(0)
}
