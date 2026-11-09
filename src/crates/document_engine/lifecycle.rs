//! Document lifecycle objects.
//!
//! Lifecycle state is tracked separately from [`crate::crates::document_engine::document::DocumentStatus`]
//! so that archival, expiry, trash/restore and locking semantics can evolve
//! without changing the coarse "is it servable" status.
//!
//! Suggested state machine:
//!
//! ```text
//! Created
//!    |
//!    v
//! Active
//!    |
//!    +----> Locked
//!    |
//!    +----> Archived
//!    |
//!    +----> Expired
//!    |
//!    +----> Trashed
//!               |
//!               +----> Restored
//!               |
//!               +----> Permanently Deleted
//! ```

use serde::{Deserialize, Serialize};

use crate::crates::document_engine::ids::DocumentId;

/// Lifecycle states a document can occupy.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum DocumentLifecycleStatus {
    Created,
    Active,
    Locked,
    Archived,
    Expired,
    Trashed,
    Restored,
    PermanentlyDeleted,
}

impl DocumentLifecycleStatus {
    /// Whether a transition from `self` to `target` is permitted by the
    /// lifecycle state machine.
    pub fn can_transition_to(self, target: Self) -> bool {
        use DocumentLifecycleStatus::*;
        matches!(
            (self, target),
            (Created, Active)
                | (Active, Locked)
                | (Active, Archived)
                | (Active, Expired)
                | (Active, Trashed)
                | (Locked, Active)
                | (Archived, Active)
                | (Expired, Trashed)
                | (Trashed, Restored)
                | (Trashed, PermanentlyDeleted)
                | (Restored, Active)
        )
    }
}

/// Current lifecycle state of a document.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct DocumentLifecycle {
    pub document_id: DocumentId,
    pub status: DocumentLifecycleStatus,
    /// RFC 3339 timestamp of when the document entered `status`.
    pub entered_at: String,
}

/// Resource-level access level on a document.
///
/// Permissions are *records* only — actual authorization decisions are
/// evaluated by the central authorization service, never independently
/// inside the Document Engine.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum DocumentAccessLevel {
    Read,
    Write,
    Manage,
}

/// A permission grant on a document for a subject (user, group, or role —
/// the meaning of `subject_id` is defined by the auth layer).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct DocumentPermission {
    pub document_id: DocumentId,
    /// Subject the permission applies to (user id, group id, role id, ...).
    pub subject_id: String,
    pub access_level: DocumentAccessLevel,
    /// Who granted the permission.
    pub granted_by: Option<String>,
    /// RFC 3339 timestamp.
    pub granted_at: String,
}
