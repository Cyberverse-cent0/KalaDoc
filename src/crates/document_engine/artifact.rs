//! Document artifact and checksum objects.

use serde::{Deserialize, Serialize};

use crate::crates::document_engine::ids::DocumentId;

/// Integrity fingerprint algorithm used by [`DocumentChecksum`].
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ChecksumAlgorithm {
    Sha256,
    Sha512,
    Blake3,
    #[serde(other)]
    Other,
}

/// Integrity fingerprint of an artifact's bytes.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct DocumentChecksum {
    pub algorithm: ChecksumAlgorithm,
    /// Lowercase hex-encoded digest.
    pub value: String,
}

/// A physical file associated with a document.
///
/// One document may have several artifacts over time (original upload,
/// converted copy, thumbnail, etc.); each [`crate::crates::document_engine::version::DocumentVersion`]
/// points at the artifact that represents it.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct DocumentArtifact {
    /// Storage-level artifact identifier (e.g. object-store key or row id).
    pub id: String,
    pub document_id: DocumentId,
    /// Where the bytes live: filesystem path, object-store URI, etc.
    pub storage_uri: String,
    /// Original file name as supplied by the uploader.
    pub file_name: String,
    pub mime_type: Option<String>,
    pub size_bytes: u64,
    pub checksum: Option<DocumentChecksum>,
    /// RFC 3339 timestamp.
    pub created_at: String,
}
