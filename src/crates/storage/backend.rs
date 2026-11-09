//! Common storage backend interface.
//!
//! [`StorageBackend`] is the object-level contract every storage
//! implementation must satisfy: confined byte reads/writes/deletes
//! under a configured root (a directory for the local backend, a
//! bucket/prefix for a remote one), optional usage accounting for
//! quota enforcement, and idempotent preparation.
//!
//! Device-level concerns — probing a target ([`crate::crates::storage::local::create_fs`]),
//! formatting disks, fault injection — are *not* part of this trait.
//! They are local-backend specifics; a remote backend would replace
//! them with its own provisioning/health-check story.
//!
//! All errors are reported through
//! [`crate::crates::storage::local::debug_when_full::StorageError`],
//! which already models the failure modes every backend needs to
//! express (path escape, not found, quota exceeded, injected faults,
//! I/O errors). If a backend needs richer error types later, this
//! error type can be lifted into this module without breaking the
//! trait.

use super::local::debug_when_full::StorageResult;

/// Object-level storage operations shared by all backends.
///
/// Implementations must treat `name` as a relative identifier confined
/// to their storage root: path traversal or absolute paths must fail
/// rather than touch anything outside the root.
///
/// Methods take `&mut self` because backends may need interior
/// mutability for bookkeeping (quota counters, connection pools,
/// fault-injection state) even for nominally read-only ops.
pub trait StorageBackend {
    /// Write `data` to `name`, replacing any existing content.
    /// Subject to the backend's quota, if one is configured.
    fn write(&mut self, name: &str, data: &[u8]) -> StorageResult<()>;

    /// Read all bytes stored at `name`.
    ///
    /// # Errors
    /// Returns [`StorageError::NotFound`](crate::crates::storage::local::debug_when_full::StorageError::NotFound)
    /// when `name` does not exist.
    fn read(&mut self, name: &str) -> StorageResult<Vec<u8>>;

    /// Delete `name`. Deleting a missing entry is a no-op.
    fn delete(&mut self, name: &str) -> StorageResult<()>;

    /// Whether `name` currently exists under the storage root.
    fn exists(&mut self, name: &str) -> StorageResult<bool>;

    /// Bytes currently stored under the root, when the backend tracks
    /// usage for quota enforcement. `None` means usage is unknown or
    /// unbounded (no quota configured).
    fn used_bytes(&self) -> Option<u64>;

    /// Ensures the backend is ready to serve operations: creates the
    /// local root directory, verifies credentials/connectivity for a
    /// remote backend, etc. Idempotent — calling it on an already
    /// prepared backend is a no-op.
    fn prepare(&mut self) -> StorageResult<()>;
}
