//! MCP server adapter — exposes application services to MCP clients.
//!
//! [`McpServer`] is a thin adapter over [`AppServices`]: every tool is a
//! small wrapper around `DocumentService`, the ingestion pipeline, the
//! job engine store, and the engine registry. No business logic lives
//! here — see `.doc/cli_mcp_docs/KalaDoc_CLI_MCP_Specification (1).md`
//! §10 for the tool design rules this module follows:
//!
//! - One tool = one coherent application action, named
//!   `kaladoc.<domain>_<verb>`.
//! - Long-running work is *enqueued* (`job_enqueue`, processing jobs)
//!   and returns a job id rather than blocking the tool call.
//! - Destructive operations map to the store's soft-delete semantics;
//!   there is no bypass.
//! - Failures are reported as MCP *tool* errors
//!   ([`CallToolResult::error`]) so the caller sees the message;
//!   malformed arguments are rejected by the framework as
//!   `invalid_params` before the handler runs.
//!
//! There is no authenticated principal yet — `owner_id` parameters
//! default to `"local"` and callers are expected to be trusted local
//! MCP clients. When authentication lands, the principal should be
//! bound once at server construction and enforced by the services.

mod handlers;
mod server;

use std::sync::Arc;

use rmcp::serve_server;
use rmcp::transport;
use serde_json::Value;

use crate::crates::application::services::{AppServices, ServiceError};
use crate::crates::job_engine::JobError;

pub use server::McpServer;

/// Default `owner_id` for documents/folders created through MCP when
/// the caller does not supply one. There is no auth layer yet, so all
/// MCP-created objects are owned by this principal.
pub(crate) const DEFAULT_OWNER: &str = "local";

/// Upper bound (decoded bytes) for `kaladoc.document_ingest` payloads.
/// Keeps a single tool call from exhausting memory; larger files
/// should go through the ingestion pipeline / inbox scan instead.
pub(crate) const MAX_INGEST_BYTES: u64 = 64 * 1024 * 1024;

/// Tool-call failure, already classified for the caller.
///
/// Every handler returns `Result<Value, ToolFault>`; the `#[tool]`
/// wrappers turn `Ok` into [`CallToolResult::structured`] and `Err`
/// into a tool-level error carrying a stable `kind:` prefix.
#[derive(Debug)]
pub(crate) enum ToolFault {
    /// Caller-fixable semantic problem (unknown folder, bad version,
    /// etc.). Malformed JSON shapes are rejected earlier, during
    /// argument deserialization.
    Validation(String),
    /// Referenced document/folder/version does not exist.
    NotFound(String),
    /// Storage, database, or other infrastructure failure.
    Internal(String),
}

impl std::fmt::Display for ToolFault {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Validation(m) | Self::NotFound(m) | Self::Internal(m) => f.write_str(m),
        }
    }
}

impl std::error::Error for ToolFault {}

impl ToolFault {
    fn validation(msg: impl Into<String>) -> Self {
        Self::Validation(msg.into())
    }
    fn not_found(msg: impl Into<String>) -> Self {
        Self::NotFound(msg.into())
    }
    fn internal(msg: impl Into<String>) -> Self {
        Self::Internal(msg.into())
    }

    /// Human-readable prefix used in tool error text, mirroring the
    /// CLI's stable error codes.
    pub(crate) fn kind(&self) -> &'static str {
        match self {
            Self::Validation(_) => "validation_failed",
            Self::NotFound(_) => "not_found",
            Self::Internal(_) => "internal",
        }
    }
}

impl From<ServiceError> for ToolFault {
    fn from(e: ServiceError) -> Self {
        use crate::crates::document_engine::store::StoreError;
        match e {
            ServiceError::Store(StoreError::NotFound { entity, id }) => {
                Self::not_found(format!("{entity} not found: {id}"))
            }
            ServiceError::Store(StoreError::Validation { field, reason }) => {
                Self::validation(format!("invalid {field}: {reason}"))
            }
            ServiceError::Store(StoreError::Document(d)) => Self::validation(d.to_string()),
            ServiceError::Store(StoreError::Serde(s)) => {
                Self::internal(format!("document store serialization error: {s}"))
            }
            ServiceError::Store(StoreError::Sqlx(s)) => {
                Self::internal(format!("database error: {s}"))
            }
            ServiceError::Config(c) => Self::internal(format!("configuration error: {c}")),
            ServiceError::Db(d) => Self::internal(format!("database error: {d}")),
            ServiceError::Io(i) => Self::internal(format!("i/o error: {i}")),
            ServiceError::Storage(s) => Self::internal(format!("storage error: {s}")),
        }
    }
}

impl From<JobError> for ToolFault {
    fn from(e: JobError) -> Self {
        Self::internal(e.to_string())
    }
}

/// Maps a handler result onto the tool response convention: `Ok` →
/// structured success (JSON text + `structuredContent`), `Err` →
/// tool-level error the MCP client renders.
pub(crate) fn into_result(result: Result<Value, ToolFault>) -> rmcp::model::CallToolResult {
    match result {
        Ok(value) => rmcp::model::CallToolResult::structured(value),
        Err(fault) => rmcp::model::CallToolResult::error(vec![rmcp::model::ContentBlock::text(
            format!("{}: {}", fault.kind(), fault),
        )]),
    }
}

/// JSON view of a [`job_engine::Job`] — the store type is not
/// `Serialize`, so tools emit this stable projection instead.
pub(crate) fn job_view(job: &crate::crates::job_engine::Job) -> Value {
    serde_json::json!({
        "id": job.id,
        "kind": job.kind,
        "payload": job.payload,
        "status": job.status.as_str(),
        "priority": job.priority,
        "attempts": job.attempts,
        "max_attempts": job.max_attempts,
        "run_at": job.run_at,
        "created_at": job.created_at,
        "started_at": job.started_at,
        "finished_at": job.finished_at,
        "last_error": job.last_error,
        "worker_id": job.worker_id,
        "batch_id": job.batch_id,
        "progress": job.progress,
    })
}

/// Runs the MCP server over stdio until the client disconnects.
///
/// `default_max_attempts` comes from `jobs.default_max_attempts` so
/// `job_enqueue` matches the worker's retry policy.
pub async fn serve_stdio(
    services: Arc<AppServices>,
    default_max_attempts: u32,
) -> Result<(), rmcp::ErrorData> {
    let server = McpServer::new(services, default_max_attempts);
    let running = serve_server(server, transport::stdio())
        .await
        .map_err(|e| {
            rmcp::ErrorData::internal_error(format!("MCP server failed to start: {e}"), None)
        })?;
    running.waiting().await.map_err(|e| {
        rmcp::ErrorData::internal_error(format!("MCP server task failed: {e}"), None)
    })?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn service_error_maps_to_fault_kinds() {
        use crate::crates::document_engine::store::StoreError;
        let f: ToolFault = ServiceError::Store(StoreError::NotFound {
            entity: "document",
            id: "d1".into(),
        })
        .into();
        assert_eq!(f.kind(), "not_found");

        let f: ToolFault = ServiceError::Store(StoreError::Validation {
            field: "name",
            reason: "empty".into(),
        })
        .into();
        assert_eq!(f.kind(), "validation_failed");

        let f: ToolFault = ServiceError::Io(std::io::Error::other("boom")).into();
        assert_eq!(f.kind(), "internal");
    }
}
