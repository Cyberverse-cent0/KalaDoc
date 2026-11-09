//! Document metadata, properties, and statistics.
//!
//! These are deliberately schema-flexible: `custom_fields` carries
//! type-specific metadata (financial year, department, ...) without
//! hardcoding every field into the core model.

use serde::{Deserialize, Serialize};

use crate::crates::document_engine::ids::DocumentId;

/// Structured descriptive metadata about a document.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct DocumentMetadata {
    pub document_id: DocumentId,
    pub title: Option<String>,
    pub author: Option<String>,
    pub subject: Option<String>,
    /// BCP-47 language tag, e.g. `"en"`, `"de"`.
    pub language: Option<String>,
    pub keywords: Vec<String>,
    /// Type-specific fields, e.g. `{"financial_year": 2026}`.
    #[serde(default)]
    pub custom_fields: serde_json::Map<String, serde_json::Value>,
}

/// General document properties typically embedded in or derived from the
/// file itself (PDF Info dict, Office core properties, ...).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct DocumentProperties {
    pub document_id: DocumentId,
    /// Application that created the file.
    pub creator: Option<String>,
    /// Application that last modified/produced the file.
    pub producer: Option<String>,
    /// RFC 3339 timestamps from the file, when available.
    pub creation_date: Option<String>,
    pub modification_date: Option<String>,
}

/// Computed statistics about a document's contents.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct DocumentStatistics {
    pub document_id: DocumentId,
    pub page_count: Option<u32>,
    pub word_count: Option<u64>,
    pub character_count: Option<u64>,
    pub paragraph_count: Option<u32>,
    /// Size of the primary artifact in bytes.
    pub size_bytes: u64,
}
