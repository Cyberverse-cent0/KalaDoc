//! MCP server adapter (spec §10–§13).
//!
//! `kaladoc mcp serve --transport stdio` speaks the Model Context
//! Protocol over stdio: newline-delimited JSON-RPC 2.0 on stdin/stdout,
//! implemented by the `rmcp` crate already in the dependency tree. The
//! server is a thin adapter over the same application services the CLI
//! uses — no business logic lives here, and it never opens a network
//! listener.

mod server;

pub use server::McpServer;

use std::path::Path;

use super::cli::CliError;

/// Run the MCP server on stdio until the client disconnects.
///
/// `config_path` is the `--config` override honored by every command;
/// `None` uses the default configuration search. Returns the process
/// exit code (`0` on clean shutdown).
pub async fn serve_stdio(config_path: Option<&Path>) -> Result<i32, CliError> {
    let server = McpServer::new(config_path.map(Path::to_path_buf));
    // stdio_pipes uses the non-blocking Linux pipe transport when both
    // streams are FIFOs and falls back to blocking stdio elsewhere.
    let (reader, writer) = rmcp::transport::io::stdio_pipes();
    let running = rmcp::serve_server(server, (reader, writer))
        .await
        .map_err(|e| CliError::Mcp(format!("MCP handshake failed: {e}")))?;
    tracing::info!("MCP server listening on stdio");
    match running.waiting().await {
        Ok(_) => Ok(0),
        Err(e) => Err(CliError::Mcp(format!("MCP server task failed: {e}"))),
    }
}
