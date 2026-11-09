//! Globally unique identifier newtypes for document-engine aggregates.
//!
//! IDs are opaque strings so the storage layer can choose the generation
//! strategy (UUID, ULID, database sequence, etc.) without changing the
//! domain model.

use std::fmt;

use serde::{Deserialize, Serialize};

/// Globally unique identifier for a [`crate::crates::document_engine::document::Document`].
#[derive(Debug, Clone, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(transparent)]
pub struct DocumentId(String);

impl DocumentId {
    pub fn new(id: impl Into<String>) -> Self {
        Self(id.into())
    }

    pub fn as_str(&self) -> &str {
        &self.0
    }
}

impl fmt::Display for DocumentId {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.0)
    }
}

impl From<String> for DocumentId {
    fn from(id: String) -> Self {
        Self(id)
    }
}

/// Unique identifier for a [`crate::crates::document_engine::organization::Folder`].
#[derive(Debug, Clone, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(transparent)]
pub struct FolderId(String);

impl FolderId {
    pub fn new(id: impl Into<String>) -> Self {
        Self(id.into())
    }

    pub fn as_str(&self) -> &str {
        &self.0
    }
}

impl fmt::Display for FolderId {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.0)
    }
}

impl From<String> for FolderId {
    fn from(id: String) -> Self {
        Self(id)
    }
}
