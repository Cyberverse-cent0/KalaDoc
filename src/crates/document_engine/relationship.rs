//! Document relationship objects — links between documents.

use serde::{Deserialize, Serialize};

use crate::crates::document_engine::ids::DocumentId;

/// Classification of a relationship between two documents.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum DocumentRelationshipType {
    /// Source is a parent/container of the target.
    Parent,
    /// Source is a child/part of the target.
    Child,
    /// Generic association with no containment semantics.
    Related,
    /// Target is an attachment of the source.
    Attachment,
    /// Target is a duplicate of the source.
    Duplicate,
    /// Source was derived from the target.
    DerivedFrom,
    /// Source supersedes/replaces the target.
    Supersedes,
    #[serde(other)]
    Other,
}

/// A directed link from one document to another.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct DocumentRelationship {
    pub id: String,
    pub source_document_id: DocumentId,
    pub target_document_id: DocumentId,
    pub relationship_type: DocumentRelationshipType,
    /// User who created the link, if user-initiated.
    pub created_by: Option<String>,
    /// RFC 3339 timestamp.
    pub created_at: String,
}
