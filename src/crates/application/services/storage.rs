//! Storage service — the application-layer facade over the storage
//! engine.
//!
//! [`StorageService`] owns the local [`FaultInjectingFs`] backend built
//! from a [`StorageConfig`]. It is the only place adapters should touch
//! storage: CLI/MCP handlers call `put`/`get`/`delete`/`status` (and the
//! streaming `begin_upload`/`open_reader`/`sweep_staging` family) here
//! and never see filesystem paths or `StorageError` directly —
//! everything is mapped into [`ServiceError`].
//!
//! All operations take `&mut self` because the backend does (quota
//! counters and fault-injection state need interior mutability). They
//! are synchronous and blocking — async callers must wrap them in
//! `spawn_blocking`. Hold the service in the command context and borrow
//! it mutably per operation.
//!
//! The backend is held **concretely** rather than as
//! `Box<dyn StorageBackend>`: the streaming API is expressed through
//! the GAT-based [`StreamingBackend`] trait, which trait objects cannot
//! name. `FaultInjectingFs` is currently the only backend
//! implementation; when a second one lands, this service can be
//! generalized (e.g. an enum of backends) at that point.

use std::io::Read;
use std::time::Duration;

use serde::Serialize;

use crate::crates::storage::backend::StorageBackend;
use crate::crates::storage::local::config::StorageConfig;
use crate::crates::storage::local::debug_when_full::FaultInjectingFs;
use crate::crates::storage::local::setup;
use crate::crates::storage::stream::{StreamLimits, StreamingBackend};

use super::error::ServiceError;

/// How long (24h) abandoned staging uploads may linger before
/// [`StorageService::sweep_staging`] reclaims them. Applied once at
/// construction as crash recovery for uploads that never reached
/// `commit`/`abort`.
const DEFAULT_STAGING_MAX_AGE: Duration = Duration::from_secs(24 * 60 * 60);

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
/// one-time [`setup::setup`] preparation, builds the backend, calls
/// [`StorageBackend::prepare`], and sweeps stale staging uploads.
pub struct StorageService {
    /// Concrete local backend — see the module docs for why this is not
    /// a `Box<dyn StorageBackend>`.
    backend: FaultInjectingFs,
    /// Kept for status reporting (`quota_bytes`, `root_configured`).
    config: StorageConfig,
}

impl StorageService {
    /// Prepares `config.root_dir`, constructs the local backend, and
    /// prepares it for use. Abandoned staging uploads older than 24h
    /// are swept before returning; a sweep failure is logged and does
    /// not fail startup.
    ///
    /// # Errors
    /// Returns [`ServiceError::Io`] when the root directory cannot be
    /// created, or [`ServiceError::Storage`] when backend preparation
    /// fails.
    pub fn from_config(config: &StorageConfig) -> Result<Self, ServiceError> {
        setup::setup(config)?;
        let mut backend = FaultInjectingFs::new(config.clone())?;
        backend.prepare()?;
        let mut svc = Self {
            backend,
            config: config.clone(),
        };
        svc.sweep_staging_on_start();
        Ok(svc)
    }

    /// Wraps an already-constructed backend — used by tests (e.g. to
    /// inject custom [`StreamLimits`]) and by future config-driven
    /// backend selection. Like [`StorageService::from_config`], sweeps
    /// staging uploads older than 24h; failures are logged, not fatal.
    pub fn from_backend(backend: FaultInjectingFs, config: StorageConfig) -> Self {
        let mut svc = Self { backend, config };
        svc.sweep_staging_on_start();
        svc
    }

    /// Reclaims staging files abandoned by crashed/killed uploads.
    /// Called once during construction; a failure only means stale
    /// staging lingers until the next start, so it is logged as a
    /// warning rather than propagated.
    fn sweep_staging_on_start(&mut self) {
        if let Err(e) = self.sweep_staging(DEFAULT_STAGING_MAX_AGE) {
            tracing::warn!(error = %e, "storage staging sweep failed at startup");
        }
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
        StorageBackend::used_bytes(&self.backend)
    }

    // ------------------------------------------------------------------
    // Streaming uploads / readers
    // ------------------------------------------------------------------
    //
    // These forward to [`StreamingBackend`] on the concrete
    // [`FaultInjectingFs`]. All are synchronous and blocking like the
    // rest of this service — callers on async runtimes must use
    // `spawn_blocking`.

    /// The per-upload limits this service enforces: maximum object size
    /// and maximum single-chunk size accepted by
    /// [`StorageService::begin_upload`].
    pub fn stream_limits(&self) -> StreamLimits {
        self.backend.stream_limits()
    }

    /// Begins a staged upload for `name`. The returned writer accepts
    /// chunks up to `max_chunk_bytes` via `write_chunk`; `commit`
    /// verifies the SHA-256 checksum and atomically publishes the
    /// object, `abort` discards the staged file. Staged data lives
    /// under a dedicated staging directory until commit, so failed
    /// uploads never expose partial objects.
    ///
    /// The concrete writer type is the backend's GAT
    /// (`<FaultInjectingFs as StreamingBackend>::Upload<'_>`, i.e.
    /// `LocalUploadWriter`); it borrows this service mutably for its
    /// lifetime.
    ///
    /// # Errors
    /// [`ServiceError::Storage`] on path-escape or backend failures.
    pub fn begin_upload(
        &mut self,
        name: &str,
    ) -> Result<<FaultInjectingFs as StreamingBackend>::Upload<'_>, ServiceError> {
        Ok(StreamingBackend::begin_upload(&mut self.backend, name)?)
    }

    /// Opens a streaming reader over the stored object `name`, avoiding
    /// a full in-memory buffer for large files. Read it like any
    /// [`std::io::Read`].
    ///
    /// # Errors
    /// [`ServiceError::Storage`]`(StorageError::NotFound)` when `name`
    /// does not exist.
    pub fn open_reader(&mut self, name: &str) -> Result<Box<dyn Read + Send>, ServiceError> {
        Ok(StreamingBackend::open_reader(&mut self.backend, name)?)
    }

    /// Deletes staging files older than `max_age` — crash recovery for
    /// uploads abandoned before `commit`/`abort`. Returns the number of
    /// staged files removed.
    ///
    /// # Errors
    /// [`ServiceError::Storage`] when the staging directory cannot be
    /// scanned or cleaned.
    pub fn sweep_staging(&mut self, max_age: Duration) -> Result<usize, ServiceError> {
        Ok(StreamingBackend::sweep_staging(&mut self.backend, max_age)?)
    }

    /// Storage health snapshot for diagnostics (`doctor`, `status`
    /// commands). Contains no absolute paths.
    pub fn status(&self) -> StorageStatus {
        StorageStatus {
            backend: "local",
            root_configured: !self.config.root_dir.as_os_str().is_empty(),
            used_bytes: StorageBackend::used_bytes(&self.backend),
            quota_bytes: self.config.max_size_bytes,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::crates::storage::local::debug_when_full::StorageError;
    use crate::crates::storage::stream::{DEFAULT_MAX_CHUNK_BYTES, StreamUpload};
    use sha2::{Digest, Sha256};
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

    #[test]
    fn streamed_upload_commit_get_roundtrip() {
        let root = TempRoot::new();
        let mut svc = StorageService::from_config(&root.config()).unwrap();

        // Deterministic non-trivial payload, written in several chunks.
        let payload: Vec<u8> = (0..8192u32).map(|i| (i % 251) as u8).collect();
        let mut upload = svc.begin_upload("streamed.bin").unwrap();
        for chunk in payload.chunks(1000) {
            upload.write_chunk(chunk).unwrap();
        }
        let receipt = upload.commit().unwrap();
        assert_eq!(receipt.name, "streamed.bin");
        assert_eq!(receipt.bytes, payload.len() as u64);
        // The commit receipt carries the SHA-256 of the published bytes.
        assert_eq!(receipt.sha256, format!("{:x}", Sha256::digest(&payload)));

        // The committed object is readable through both APIs.
        assert_eq!(svc.get("streamed.bin").unwrap(), payload);
        let mut reader = svc.open_reader("streamed.bin").unwrap();
        let mut streamed = Vec::new();
        reader.read_to_end(&mut streamed).unwrap();
        assert_eq!(streamed, payload);
        assert!(svc.exists("streamed.bin").unwrap());
    }

    #[test]
    fn staged_upload_abort_leaves_nothing() {
        let root = TempRoot::new();
        let mut svc = StorageService::from_config(&root.config()).unwrap();

        let mut upload = svc.begin_upload("aborted.bin").unwrap();
        upload.write_chunk(b"partial").unwrap();
        upload.abort().unwrap();

        assert!(!svc.exists("aborted.bin").unwrap());
        assert!(matches!(
            svc.get("aborted.bin"),
            Err(ServiceError::Storage(StorageError::NotFound { .. }))
        ));
    }

    #[test]
    fn oversized_chunk_is_rejected_and_maps_to_service_error() {
        let root = TempRoot::new();
        let mut svc = StorageService::from_config(&root.config()).unwrap();

        // A single chunk larger than the per-chunk cap must be refused.
        // The writer borrows `svc` mutably until consumed, so the whole
        // exchange happens in a scope: `commit`/`abort` consume it.
        let result = {
            let mut upload = svc.begin_upload("big.bin").unwrap();
            let chunk = vec![0u8; DEFAULT_MAX_CHUNK_BYTES + 1];
            match upload.write_chunk(&chunk) {
                Ok(()) => upload.commit().map(|_| ()),
                Err(e) => {
                    let _ = upload.abort();
                    Err(e)
                }
            }
        };
        let storage_err = result.expect_err("oversized upload must fail");
        assert!(matches!(storage_err, StorageError::TooLarge { .. }));
        // ...and it crosses the service boundary as ServiceError::Storage.
        let service_err: ServiceError = storage_err.into();
        assert!(matches!(
            service_err,
            ServiceError::Storage(StorageError::TooLarge { .. })
        ));
        assert!(!svc.exists("big.bin").unwrap());
    }

    #[test]
    fn sweep_staging_noop_when_nothing_staged() {
        let root = TempRoot::new();
        let mut svc = StorageService::from_config(&root.config()).unwrap();
        // Construction already swept; a second sweep finds nothing.
        assert_eq!(svc.sweep_staging(Duration::from_secs(0)).unwrap(), 0);
    }

    #[test]
    fn stream_limits_are_reported() {
        let root = TempRoot::new();
        let svc = StorageService::from_config(&root.config()).unwrap();
        let limits = svc.stream_limits();
        assert!(limits.max_chunk_bytes > 0);
    }
}
