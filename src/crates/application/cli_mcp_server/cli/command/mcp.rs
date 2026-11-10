//! `kaladoc mcp serve [--transport stdio]`
//!
//! Starts the MCP server ([`crate::crates::application::cli_mcp_server::mcp`])
//! over stdio for local MCP clients. Configuration and services are
//! built once at startup via [`AppServices::init`] (strict config —
//! required secrets must be present, same as `serve`/`jobs worker`).
//! The process runs until the MCP client disconnects.

use std::sync::Arc;

use crate::crates::application::cli_mcp_server::mcp;
use crate::crates::application::services::app::AppServices;
use crate::crates::application::services::error::ServiceError;

use super::super::args::McpArgs;
use super::{CliError, Context};

pub async fn run(ctx: &Context, args: &McpArgs) -> Result<i32, CliError> {
    match args {
        McpArgs::Serve { transport } => serve(ctx, transport).await,
    }
}

async fn serve(ctx: &Context, transport: &str) -> Result<i32, CliError> {
    if transport != "stdio" {
        return Err(CliError::Unsupported(format!(
            "mcp transport {transport:?} is not implemented; only `stdio` is \
             supported (remote MCP transports require authentication and \
             are not exposed by default)"
        )));
    }

    let config = ctx.config()?;
    let default_max_attempts = config.jobs.default_max_attempts;
    let services = AppServices::init(&config).await.map_err(svc_err)?;

    mcp::serve_stdio(Arc::new(services), default_max_attempts)
        .await
        .map_err(|e| CliError::Mcp(e.to_string()))?;
    Ok(0)
}

/// Same mapping as `command::doc::svc_err` — store "not found" /
/// "validation" keep dedicated exit codes; everything else is
/// internal/io/config/db.
fn svc_err(e: ServiceError) -> CliError {
    match e {
        ServiceError::Store(crate::crates::document_engine::store::StoreError::NotFound {
            entity,
            id,
        }) => CliError::NotFound(format!("{entity} {id}")),
        ServiceError::Store(crate::crates::document_engine::store::StoreError::Validation {
            field,
            reason,
        }) => CliError::Validation(format!("{field}: {reason}")),
        ServiceError::Store(crate::crates::document_engine::store::StoreError::Document(d)) => {
            CliError::Validation(d.to_string())
        }
        ServiceError::Store(crate::crates::document_engine::store::StoreError::Serde(s)) => {
            CliError::Json(s)
        }
        ServiceError::Store(crate::crates::document_engine::store::StoreError::Sqlx(s)) => {
            CliError::Internal(format!("database error: {s}"))
        }
        ServiceError::Config(c) => CliError::Config(c),
        ServiceError::Db(d) => CliError::Db(d),
        ServiceError::Io(i) => CliError::Io(i),
        ServiceError::Storage(s) => CliError::Internal(s.to_string()),
    }
}
