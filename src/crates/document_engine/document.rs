//! Core document objects: `Document`, its classification enums, and
//! `DocumentSource` describing document origin.

use serde::{Deserialize, Serialize};

use crate::crates::document_engine::error::DocumentError;
use crate::crates::document_engine::ids::{DocumentId, FolderId};

/// Classification of a document's content type.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum DocumentType {
    Pdf,
    Image,
    Spreadsheet,
    Presentation,
    Text,
    Archive,
    #[serde(other)]
    Other,
}

/// Coarse lifecycle status of a document's stored representation.
///
/// Fine-grained lifecycle transitions are tracked by
/// [`crate::crates::document_engine::lifecycle::DocumentLifecycle`]; this enum answers "can the
/// document be opened/served" at a glance.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum DocumentStatus {
    Active,
    Archived,
    Trashed,
    Deleted,
}

/// How the document originally entered the system.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum DocumentOrigin {
    /// Uploaded by a user through the application.
    Upload,
    /// Imported from another system or batch job.
    Import,
    /// Produced by a generator (e.g. report engine).
    Generated,
    /// Referenced from an external provider/repository.
    External,
    #[serde(other)]
    Other,
}

/// Describes where the original document came from.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct DocumentSource {
    pub origin: DocumentOrigin,
    /// URI or path of the original source, when known.
    pub uri: Option<String>,
    /// Identifier in the external system, for [`DocumentOrigin::External`].
    pub external_reference: Option<String>,
    /// RFC 3339 timestamp of ingestion.
    pub imported_at: Option<String>,
}

impl DocumentSource {
    pub fn new(origin: DocumentOrigin) -> Self {
        Self {
            origin,
            uri: None,
            external_reference: None,
            imported_at: None,
        }
    }
}

/// Validated document display name.
///
/// Names must be non-empty, at most 255 characters, and must not contain
/// path separators or control characters.
#[derive(Debug, Clone, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(try_from = "String", into = "String")]
pub struct DocumentName(String);

impl DocumentName {
    pub const MAX_LEN: usize = 255;

    pub fn new(name: impl Into<String>) -> Result<Self, DocumentError> {
        let name = name.into();
        let trimmed = name.trim();
        if trimmed.is_empty() {
            return Err(DocumentError::validation("name", "must not be empty"));
        }
        if trimmed.chars().count() > Self::MAX_LEN {
            return Err(DocumentError::validation(
                "name",
                format!("must be at most {} characters", Self::MAX_LEN),
            ));
        }
        if trimmed
            .chars()
            .any(|c| c.is_control() || matches!(c, '/' | '\\'))
        {
            return Err(DocumentError::validation(
                "name",
                "must not contain path separators or control characters",
            ));
        }
        Ok(Self(trimmed.to_string()))
    }

    pub fn as_str(&self) -> &str {
        &self.0
    }
}

impl TryFrom<String> for DocumentName {
    type Error = DocumentError;

    fn try_from(value: String) -> Result<Self, Self::Error> {
        Self::new(value)
    }
}

impl From<DocumentName> for String {
    fn from(name: DocumentName) -> Self {
        name.0
    }
}

/// Main logical representation of a document.
///
/// `Document` is the aggregate root: artifacts, versions, metadata, tags
/// and lifecycle state all hang off it via [`DocumentId`].
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Document {
    pub id: DocumentId,
    pub name: DocumentName,
    pub document_type: DocumentType,
    pub status: DocumentStatus,
    pub source: DocumentSource,
    /// Folder this document lives in, if any.
    pub folder_id: Option<FolderId>,
    /// Owning user/organization identifier (interpreted by the auth layer).
    pub owner_id: String,
    /// RFC 3339 timestamps.
    pub created_at: String,
    pub updated_at: String,
}

impl Document {
    #[allow(clippy::too_many_arguments)]
    pub fn new(
        id: DocumentId,
        name: DocumentName,
        document_type: DocumentType,
        owner_id: impl Into<String>,
        source: DocumentSource,
        now: impl Into<String>,
    ) -> Self {
        let now = now.into();
        Self {
            id,
            name,
            document_type,
            status: DocumentStatus::Active,
            source,
            folder_id: None,
            owner_id: owner_id.into(),
            created_at: now.clone(),
            updated_at: now,
        }
    }
}
