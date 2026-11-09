//! Storage service — the application-layer facade over the storage
//! engine.
//!
//! [`StorageService`] owns a [`StorageBackend`] (today always the local
//! [`FaultInjectingFs``]) built from a [`StorageConfig`]. It is the only
//! place adapters should touch storage: CLI/MCP handlers call
//! `put`/`get`/`delete`/`status` here and never see filesystem paths or
//! `StorageError` directly — everything is mapped into
//! [`ServiceError`].
//!
//! All operations take `&mut self` because the backend trait does
//! (quota counters and fault-injection state need interior mutability).
//! Hold the service in the command context and borrow it mutably per
//! operation.

use serde::Serialize;

use crate::crates::storage::backend::StorageBackend;
use crate::crates::storage::local::config::StorageConfig;
use crate::crates::storage::local::debug_when_full::FaultInjectingFs;
use crate::crates::storage::local::setup;

use super::error::ServiceError;

/// Snapshot of storage health, safe to serialize for CLI `--json` or MCP
/// tool output.
///
/// Deliberately contains **no filesystem paths** — adapters must not
/// leak server-side directory layout. `root_configured` reports only
/// whether a root was supplied in configuration.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct StorageStatus {
    /// Backend kind, e.g. `"local"`. Remote backends report their own.
    pub backend: &'static str,
    /// Whether a storage root was configured (non-empty `root_dir`).
    pub root_configured: bool,
    /// Bytes currently accounted under the root. `None` when the backend
    /// does not track usage (no quota configured).
    pub used_bytes: Option<u64>,
    /// Configured quota in bytes, if any.
    pub quota_bytes: Option<u64>,
}

/// Application-facing storage service.
///
/// Construct via [`StorageService::from_config`], which runs the
/// one-time [`setup::setup`] preparation, builds the backend, and calls
/// [`StorageBackend::prepare`].
pub struct StorageService {
    backend: Box<dyn StorageBackend>,
    /// Kept for status reporting (`quota_bytes`, `root_configured`).
    config: StorageConfig,
}

impl StorageService {
    /// Prepares `config.root_dir`, constructs the local backend, and
    /// prepares it for use.
    ///
    /// # Errors
    /// Returns [`ServiceError::Io`] when the root directory cannot be
    /// created, or [`ServiceError::Storage`] when backend preparation
    /// fails.
    pub fn from_config(config: &StorageConfig) -> Result<Self, ServiceError> {
        setup::setup(config)?;
        let mut backend = FaultInjectingFs::new(config.clone())?;
        backend.prepare()?;
        Ok(Self {
            backend: Box::new(backend),
            config: config.clone(),
        })
    }

    /// Wraps an already-constructed backend — used by tests and by
    /// future config-driven backend selection.
    pub fn from_backend(backend: Box<dyn StorageBackend>, config: StorageConfig) -> Self {
        Self { backend, config }
    }

    /// Writes `data` to `name`, replacing existing content. `name` is a
    /// relative identifier confined to the storage root; traversal is
    /// rejected by the backend.
    pub fn put(&mut self, name: &str, data: &[u8]) -> Result<(), ServiceError> {
        Ok(self.backend.write(name, data)?)
    }

    /// Reads all bytes stored at `name`.
    ///
    /// # Errors
    /// [`ServiceError::Storage`]`(StorageError::NotFound)` when `name`
    /// does not exist.
    pub fn get(&mut self, name: &str) -> Result<Vec<u8>, ServiceError> {
        Ok(self.backend.read(name)?)
    }

    /// Deletes `name`; deleting a missing entry is a no-op.
    pub fn delete(&mut self, name: &str) -> Result<(), ServiceError> {
        Ok(self.backend.delete(name)?)
    }

    /// Whether `name` currently exists under the storage root.
    pub fn exists(&mut self, name: &str) -> Result<bool, ServiceError> {
        Ok(self.backend.exists(name)?)
    }

    /// Bytes currently stored, when the backend tracks usage.
    pub fn used_bytes(&self) -> Option<u64> {
        self.backend.used_bytes()
    }

    /// Storage health snapshot for diagnostics (`doctor`, `status`
    /// commands). Contains no absolute paths.
    pub fn status(&self) -> StorageStatus {
        StorageStatus {
            backend: "local",
            root_configured: !self.config.root_dir.as_os_str().is_empty(),
            used_bytes: self.backend.used_bytes(),
            quota_bytes: self.config.max_size_bytes,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::path::PathBuf;
    use std::sync::atomic::{AtomicU64, Ordering};

    static NEXT_DIR: AtomicU64 = AtomicU64::new(0);

    /// Unique tempdir per test; callers remove it on drop via
    /// [`TempRoot`].
    struct TempRoot(PathBuf);

    impl TempRoot {
        fn new() -> Self {
            let dir = std::env::temp_dir().join(format!(
                "kaladoc-service-test-{}-{}",
                std::process::id(),
                NEXT_DIR.fetch_add(1, Ordering::Relaxed)
            ));
            std::fs::create_dir_all(&dir).unwrap();
            Self(dir)
        }

        fn config(&self) -> StorageConfig {
            StorageConfig::new(&self.0)
        }
    }

    impl Drop for TempRoot {
        fn drop(&mut self) {
            let _ = std::fs::remove_dir_all(&self.0);
        }
    }

    #[test]
    fn put_get_delete_exists_roundtrip() {
        let root = TempRoot::new();
        let mut svc = StorageService::from_config(&root.config()).unwrap();

        assert!(!svc.exists("a.bin").unwrap());
        svc.put("a.bin", b"hello").unwrap();
        assert!(svc.exists("a.bin").unwrap());
        assert_eq!(svc.get("a.bin").unwrap(), b"hello");

        // Nested names work; missing reads report NotFound.
        svc.put("dir/b.bin", b"world").unwrap();
        assert_eq!(svc.get("dir/b.bin").unwrap(), b"world");
        assert!(matches!(
            svc.get("missing.bin"),
            Err(ServiceError::Storage(
                crate::crates::storage::local::debug_when_full::StorageError::NotFound { .. }
            ))
        ));

        svc.delete("a.bin").unwrap();
        assert!(!svc.exists("a.bin").unwrap());
        // Deleting a missing entry is a no-op.
        svc.delete("a.bin").unwrap();
    }

    #[test]
    fn quota_is_enforced_and_reported() {
        let root = TempRoot::new();
        let config = root.config().with_quota(10);
        let mut svc = StorageService::from_config(&config).unwrap();

        svc.put("a.bin", &[0u8; 6]).unwrap();
        assert!(matches!(
            svc.put("b.bin", &[0u8; 5]),
            Err(ServiceError::Storage(
                crate::crates::storage::local::debug_when_full::StorageError::QuotaExceeded { .. }
            ))
        ));
        assert_eq!(svc.used_bytes(), Some(6));

        let status = svc.status();
        assert_eq!(status.backend, "local");
        assert!(status.root_configured);
        assert_eq!(status.used_bytes, Some(6));
        assert_eq!(status.quota_bytes, Some(10));
        // Status must never leak the absolute temp path.
        let json = serde_json::to_string(&status).unwrap();
        assert!(!json.contains(root.0.to_str().unwrap()));
    }

    #[test]
    fn path_escape_is_rejected() {
        let root = TempRoot::new();
        let mut svc = StorageService::from_config(&root.config()).unwrap();
        assert!(matches!(
            svc.put("../escape.bin", b"x"),
            Err(ServiceError::Storage(
                crate::crates::storage::local::debug_when_full::StorageError::PathOutsideRoot { .. }
            ))
        ));
    }
}
