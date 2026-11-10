//! CLI and MCP server adapters.
//!
//! Both interfaces are thin adapters over the application services in
//! [`super`] — they contain no business logic of their own. See
//! `.doc/cli_mcp_docs/KalaDoc_CLI_MCP_Specification (1).md`.

pub mod cli;
pub mod mcp;
