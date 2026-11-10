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

#[cfg(test)]
mod tests {
    use super::*;
    use crate::crates::document_engine::ids::FolderId;

    #[test]
    fn document_name_trims_and_accepts_valid() {
        let name = DocumentName::new("  report.pdf  ").expect("valid name");
        assert_eq!(name.as_str(), "report.pdf");
        assert_eq!(
            DocumentName::new("a".repeat(DocumentName::MAX_LEN))
                .expect("max len")
                .as_str(),
            "a".repeat(DocumentName::MAX_LEN)
        );
    }

    #[test]
    fn document_name_rejects_invalid() {
        for bad in ["", "   ", "\t\n"] {
            assert!(
                DocumentName::new(bad).is_err(),
                "name {bad:?} should be rejected"
            );
        }
        // One over the limit.
        assert!(DocumentName::new("a".repeat(DocumentName::MAX_LEN + 1)).is_err());
        // Path separators and control characters.
        for bad in ["a/b.pdf", "a\\b.pdf", "a\u{7}b.pdf", "a\nb.pdf"] {
            assert!(
                DocumentName::new(bad).is_err(),
                "name {bad:?} should be rejected"
            );
        }
    }

    #[test]
    fn document_name_serde_roundtrip() {
        let name = DocumentName::new("report.pdf").expect("name");
        // Serializes transparently as a plain string.
        let json = serde_json::to_string(&name).expect("serialize");
        assert_eq!(json, "\"report.pdf\"");
        let back: DocumentName = serde_json::from_str(&json).expect("deserialize");
        assert_eq!(back, name);

        // Deserialization runs the same validation.
        assert!(serde_json::from_str::<DocumentName>("\"a/b\"").is_err());
        assert!(serde_json::from_str::<DocumentName>("\"\"").is_err());

        // TryFrom<String> / Into<String> conversions.
        let owned: String = name.clone().into();
        assert_eq!(owned, "report.pdf");
        let again = DocumentName::try_from(owned).expect("try_from");
        assert_eq!(again, name);
    }

    #[test]
    fn document_type_serde_snake_case_with_other() {
        assert_eq!(
            serde_json::to_value(DocumentType::Pdf).unwrap(),
            serde_json::json!("pdf")
        );
        assert_eq!(
            serde_json::to_value(DocumentType::Spreadsheet).unwrap(),
            serde_json::json!("spreadsheet")
        );
        assert_eq!(
            serde_json::from_str::<DocumentType>("\"image\"").unwrap(),
            DocumentType::Image
        );
        // Unknown variants fall back to `Other` via #[serde(other)].
        assert_eq!(
            serde_json::from_str::<DocumentType>("\"hologram\"").unwrap(),
            DocumentType::Other
        );
    }

    #[test]
    fn document_status_and_origin_serde() {
        for (status, s) in [
            (DocumentStatus::Active, "active"),
            (DocumentStatus::Archived, "archived"),
            (DocumentStatus::Trashed, "trashed"),
            (DocumentStatus::Deleted, "deleted"),
        ] {
            assert_eq!(serde_json::to_value(status).unwrap(), serde_json::json!(s));
            assert_eq!(
                serde_json::from_str::<DocumentStatus>(&format!("\"{s}\"")).unwrap(),
                status
            );
        }

        for (origin, s) in [
            (DocumentOrigin::Upload, "upload"),
            (DocumentOrigin::Import, "import"),
            (DocumentOrigin::Generated, "generated"),
            (DocumentOrigin::External, "external"),
        ] {
            assert_eq!(serde_json::to_value(origin).unwrap(), serde_json::json!(s));
        }
        assert_eq!(
            serde_json::from_str::<DocumentOrigin>("\"mystery\"").unwrap(),
            DocumentOrigin::Other
        );
    }

    #[test]
    fn document_source_new_defaults() {
        let source = DocumentSource::new(DocumentOrigin::External);
        assert_eq!(source.origin, DocumentOrigin::External);
        assert_eq!(source.uri, None);
        assert_eq!(source.external_reference, None);
        assert_eq!(source.imported_at, None);
    }

    #[test]
    fn document_new_defaults_and_serde_roundtrip() {
        let doc = Document::new(
            DocumentId::new("doc-1"),
            DocumentName::new("report.pdf").unwrap(),
            DocumentType::Pdf,
            "user-1",
            DocumentSource::new(DocumentOrigin::Upload),
            "2024-01-01T00:00:00Z",
        );
        assert_eq!(doc.status, DocumentStatus::Active);
        assert_eq!(doc.folder_id, None);
        assert_eq!(doc.created_at, "2024-01-01T00:00:00Z");
        assert_eq!(doc.updated_at, doc.created_at);

        let json = serde_json::to_string(&doc).expect("serialize");
        let back: Document = serde_json::from_str(&json).expect("deserialize");
        assert_eq!(back, doc);

        // With a folder assigned, the roundtrip preserves it.
        let mut with_folder = doc.clone();
        with_folder.folder_id = Some(FolderId::new("fld-9"));
        let json = serde_json::to_string(&with_folder).expect("serialize");
        let back: Document = serde_json::from_str(&json).expect("deserialize");
        assert_eq!(back.folder_id, Some(FolderId::new("fld-9")));
    }
}
