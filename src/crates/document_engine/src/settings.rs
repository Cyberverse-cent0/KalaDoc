//! Per-document settings — an application-level JSON blob plus
//! document-level flags that do not belong in descriptive
//! [`crate::crates::document_engine::metadata::DocumentMetadata`].
//!
//! `settings` is deliberately schema-flexible (like
//! `DocumentMetadata::custom_fields`): it carries operational
//! preferences — default view mode, retention hints, integration
//! toggles, ... — without hardcoding every knob into the core model.
//! `is_locked` is the one first-class flag: a locked document rejects
//! content mutations at higher layers (enforcement lives in the
//! service/adapter layer, not the store).

use serde::{Deserialize, Serialize};

use crate::crates::document_engine::ids::DocumentId;

/// Maximum serialized size of the `settings` JSON object, in bytes.
/// Keeps a single row bounded regardless of caller input.
pub const MAX_SETTINGS_BYTES: usize = 64 * 1024;

/// Maximum length of a single settings key.
pub const MAX_SETTINGS_KEY_LEN: usize = 128;

/// Per-document settings: a JSON object of free-form keys plus the
/// `is_locked` flag. Stored as one row per document; the row is created
/// lazily on first write.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct DocumentSettings {
    pub document_id: DocumentId,
    /// Free-form settings object, e.g. `{"default_view": "continuous"}`.
    #[serde(default)]
    pub settings: serde_json::Map<String, serde_json::Value>,
    /// When `true`, higher layers should reject content mutations.
    #[serde(default)]
    pub is_locked: bool,
    /// RFC 3339 timestamps.
    pub created_at: String,
    pub updated_at: String,
}
