//! Audit and history objects — records of document-related events.

use serde::{Deserialize, Serialize};

use crate::crates::document_engine::ids::DocumentId;

/// Classification of a document audit event.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum DocumentAuditEventType {
    Created,
    Updated,
    Accessed,
    Shared,
    PermissionChanged,
    VersionCreated,
    Imported,
    Exported,
    Archived,
    Trashed,
    Restored,
    Deleted,
    MetadataUpdated,
    RelationshipCreated,
    RelationshipDeleted,
    /// A document processing job (metadata extract, index, thumbnail,
    /// pdf op, ...) began running.
    ProcessingStarted,
    /// A document processing job completed successfully.
    ProcessingCompleted,
    /// A document processing job failed permanently (retries exhausted).
    ProcessingFailed,
    /// Searchable text was extracted into the document index.
    Indexed,
    /// A thumbnail image was generated for the document.
    ThumbnailGenerated,
    #[serde(other)]
    Other,
}

/// An immutable record that something happened to a document.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct DocumentAuditEvent {
    pub id: String,
    pub document_id: DocumentId,
    /// User/system actor responsible, when known.
    pub actor_id: Option<String>,
    pub event_type: DocumentAuditEventType,
    /// RFC 3339 timestamp of when the event occurred.
    pub occurred_at: String,
    /// Optional structured details (old/new values, share target, ...).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub details: Option<serde_json::Value>,
}
