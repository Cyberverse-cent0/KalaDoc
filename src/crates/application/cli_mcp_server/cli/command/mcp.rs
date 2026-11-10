//! `kaladoc mcp serve` — dispatch to the MCP server adapter.
//!
//! Thin wrapper: validates the transport selection and hands off to
//! [`crate::crates::application::cli_mcp_server::mcp::serve_stdio`],
//! which owns the protocol loop. No business logic lives here.

use super::super::args::{McpArgs, McpTransport};
use super::{CliError, Context};

pub async fn run(ctx: &Context, args: &McpArgs) -> Result<i32, CliError> {
    match args {
        McpArgs::Serve { transport } => match transport {
            McpTransport::Stdio => {
                crate::crates::application::cli_mcp_server::mcp::serve_stdio(
                    ctx.globals.config.as_deref(),
                )
                .await
            }
        },
    }
}
