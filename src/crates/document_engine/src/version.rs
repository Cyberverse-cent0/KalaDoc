//! Document version and revision objects.

use serde::{Deserialize, Serialize};

use crate::crates::document_engine::ids::DocumentId;

/// A logical change to a document ("edited section 3", "fixed typo").
///
/// Revisions group the physical changes that produced a
/// [`DocumentVersion`]; several revisions may roll up into one version.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct DocumentRevision {
    pub id: String,
    pub document_id: DocumentId,
    /// Human-readable summary of the change.
    pub summary: String,
    /// User who authored the change.
    pub author_id: String,
    /// RFC 3339 timestamp.
    pub created_at: String,
}

/// A particular revision of a document — an immutable snapshot pointing
/// at the [`crate::crates::document_engine::artifact::DocumentArtifact`] that holds its bytes.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct DocumentVersion {
    pub document_id: DocumentId,
    /// Monotonic per-document version number, starting at 1.
    pub version_number: u32,
    /// Artifact holding this version's bytes.
    pub artifact_id: Option<String>,
    /// Revisions rolled up into this version.
    pub revision_ids: Vec<String>,
    /// User who created this version.
    pub created_by: String,
    /// Optional version note / commit message.
    pub note: Option<String>,
    /// Whether this is the document's current version.
    pub is_current: bool,
    /// RFC 3339 timestamp.
    pub created_at: String,
}
