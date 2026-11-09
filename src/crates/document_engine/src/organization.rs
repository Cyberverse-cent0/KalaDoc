//! Document organization objects: folders, tags, and tag assignments.

use serde::{Deserialize, Serialize};

use crate::crates::document_engine::ids::{DocumentId, FolderId};

/// Logical folder in the user's document hierarchy.
///
/// Hierarchy is expressed via `parent_id`; a `None` parent means the
/// folder sits at the root. Cycles are a storage-layer concern.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Folder {
    pub id: FolderId,
    pub name: String,
    pub parent_id: Option<FolderId>,
    /// Owning user/organization identifier.
    pub owner_id: String,
    /// RFC 3339 timestamps.
    pub created_at: String,
    pub updated_at: String,
}

/// A reusable, user-defined document tag.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct DocumentTag {
    /// Tag identifier — stable across documents.
    pub id: String,
    pub name: String,
    /// Optional display color (hex, e.g. `"#ff8800"`).
    pub color: Option<String>,
    /// RFC 3339 timestamp.
    pub created_at: String,
}

/// Associates a [`DocumentTag`] with a [`crate::crates::document_engine::document::Document`].
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct TagAssignment {
    pub document_id: DocumentId,
    pub tag_id: String,
    /// User who applied the tag.
    pub assigned_by: Option<String>,
    /// RFC 3339 timestamp.
    pub assigned_at: String,
}
