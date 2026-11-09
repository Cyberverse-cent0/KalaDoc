use std::fmt;
use std::io;
use std::path::{Path, PathBuf};
use std::time::Duration;

use super::config::{FaultInjectionConfig, FaultKind, StorageConfig};

/// Result alias used across the storage engine.
pub type StorageResult<T> = Result<T, StorageError>;

/// Rich error type for all storage operations.
///
/// Wraps `io::Error` but adds storage-specific context (which path,
/// whether the failure was injected, quota state) so callers can
/// distinguish "real disk full" from "simulated fault" and react
/// accordingly (retry, degrade, surface to user).
#[derive(Debug)]
pub enum StorageError {
    /// Path escapes the configured storage root.
    PathOutsideRoot { path: PathBuf },
    /// A real `std::io` failure, tagged with the offending path.
    Io { path: PathBuf, source: io::Error },
    /// Simulated fault from the injector (debug/test only).
    InjectedFault {
        kind: FaultKind,
        op: &'static str,
        path: PathBuf,
    },
    /// Write would exceed `max_size_bytes`.
    QuotaExceeded { path: PathBuf, limit: u64 },
    /// Entry not found under the storage root.
    NotFound { path: PathBuf },
}

impl fmt::Display for StorageError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::PathOutsideRoot { path } => {
                write!(f, "path escapes storage root: {}", path.display())
            }
            Self::Io { path, source } => {
                write!(f, "io error on {}: {source}", path.display())
            }
            Self::InjectedFault { kind, op, path } => {
                write!(f, "injected {kind:?} during {op} on {}", path.display())
            }
            Self::QuotaExceeded { path, limit } => {
                write!(
                    f,
                    "quota exceeded (limit {limit} bytes) for {}",
                    path.display()
                )
            }
            Self::NotFound { path } => write!(f, "not found: {}", path.display()),
        }
    }
}

impl std::error::Error for StorageError {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        match self {
            Self::Io { source, .. } => Some(source),
            _ => None,
        }
    }
}

impl From<StorageError> for io::Error {
    fn from(e: StorageError) -> Self {
        match e {
            StorageError::Io { source, .. } => source,
            other => io::Error::other(other),
        }
    }
}

/// Fault kinds eligible for each operation type.
fn write_faults() -> &'static [FaultKind] {
    &[
        FaultKind::DiskFull,
        FaultKind::PermissionDenied,
        FaultKind::IoError,
        FaultKind::Timeout,
        FaultKind::CorruptedWrite,
    ]
}

fn read_faults() -> &'static [FaultKind] {
    &[
        FaultKind::DiskFull,
        FaultKind::PermissionDenied,
        FaultKind::IoError,
        FaultKind::Timeout,
    ]
}

fn delete_faults() -> &'static [FaultKind] {
    &[
        FaultKind::PermissionDenied,
        FaultKind::IoError,
        FaultKind::Timeout,
    ]
}

/// Deterministic fault injector: fails every Nth operation with a
/// pseudo-random pick from the configured allowed kinds, so tests are
/// reproducible run-to-run.
#[derive(Debug)]
pub struct FaultInjector {
    config: FaultInjectionConfig,
    op_count: u64,
    seed: u64,
}

impl FaultInjector {
    pub fn new(config: FaultInjectionConfig) -> Self {
        Self {
            config,
            op_count: 0,
            seed: 0x9E3779B97F4A7C15,
        }
    }

    /// Disabled injector — passes all operations through.
    pub fn disabled() -> Self {
        Self::new(FaultInjectionConfig::default())
    }

    /// Returns `Err(StorageError::InjectedFault)` when this op should fail.
    pub fn maybe_fail(
        &mut self,
        candidates: &[FaultKind],
        op: &'static str,
        path: &Path,
    ) -> StorageResult<()> {
        if !self.config.enabled || self.config.every_n_ops == 0 {
            return Ok(());
        }
        self.op_count += 1;
        if self.op_count % self.config.every_n_ops as u64 != 0 {
            return Ok(());
        }

        // Pick deterministically from the intersection of per-op
        // candidates and the configured allowed kinds.
        let pool: Vec<FaultKind> = candidates
            .iter()
            .copied()
            .filter(|k| self.config.allowed_kinds.contains(k))
            .collect();
        if pool.is_empty() {
            return Ok(());
        }

        // xorshift-ish LCG for a reproducible "random" choice.
        self.seed = self
            .seed
            .wrapping_mul(6364136223846793005)
            .wrapping_add(1442695040888963407);
        let kind = pool[(self.seed >> 33) as usize % pool.len()];

        if kind == FaultKind::Timeout {
            std::thread::sleep(Duration::from_millis(50));
        }

        Err(StorageError::InjectedFault {
            kind,
            op,
            path: path.to_path_buf(),
        })
    }
}

/// Filesystem wrapper that adds fault injection, path confinement,
/// and quota checks on top of `std::fs`.
///
/// Works on any filesystem `std` supports (ext4, btrfs, APFS, NTFS,
/// tmpfs, network mounts) because it delegates to `std::fs` and only
/// intercepts at the operation level — no fs-specific syscalls.
///
/// Quota accounting is kept in memory: [`FaultInjectingFs::new`] scans
/// `root_dir` once, then `write`/`delete` adjust the counter in O(1)
/// instead of re-walking the tree on every write. Every
/// [`RECONCILE_EVERY_OPS`] mutating operations (or an explicit
/// [`FaultInjectingFs::reconcile`]) a full `dir_size` scan re-syncs the
/// counter, bounding drift from out-of-band changes or multiple
/// instances sharing the same root.
pub struct FaultInjectingFs {
    config: StorageConfig,
    injector: FaultInjector,
    /// Bytes currently stored under `root_dir` (quota accounting).
    used_bytes: u64,
    /// Mutating ops since the last full reconcile.
    ops_since_reconcile: u64,
}

/// Mutating ops between full `dir_size` reconciliations.
const RECONCILE_EVERY_OPS: u64 = 256;

impl FaultInjectingFs {
    pub fn new(config: StorageConfig) -> io::Result<Self> {
        std::fs::create_dir_all(&config.root_dir)?;
        let injector = FaultInjector::new(config.fault_injection.clone());
        // Only pay for the initial scan when a quota is configured.
        let used_bytes = if config.max_size_bytes.is_some() {
            dir_size(&config.root_dir)?
        } else {
            0
        };
        Ok(Self {
            config,
            injector,
            used_bytes,
            ops_since_reconcile: 0,
        })
    }

    /// Bytes currently accounted under the storage root.
    pub fn used_bytes(&self) -> u64 {
        self.used_bytes
    }

    /// Recomputes `used_bytes` with a full tree scan, correcting any
    /// drift from out-of-band writes or other `FaultInjectingFs`
    /// instances sharing the root. Returns the reconciled total.
    pub fn reconcile(&mut self) -> io::Result<u64> {
        self.ops_since_reconcile = 0;
        self.used_bytes = dir_size(&self.config.root_dir)?;
        Ok(self.used_bytes)
    }

    /// Resolve `name` under the root, rejecting traversal outside it.
    fn resolve(&self, name: &str) -> StorageResult<PathBuf> {
        let path = self.config.root_dir.join(name);
        // Normalize `.`/`..` components to catch escapes.
        let mut normalized = PathBuf::new();
        for comp in path.components() {
            match comp {
                std::path::Component::ParentDir => {
                    normalized.pop();
                }
                c => normalized.push(c.as_os_str()),
            }
        }
        if !normalized.starts_with(&self.config.root_dir) {
            return Err(StorageError::PathOutsideRoot { path });
        }
        Ok(normalized)
    }

    /// Write bytes to `name`. Subject to fault injection and quota.
    pub fn write(&mut self, name: &str, data: &[u8]) -> StorageResult<()> {
        let path = self.resolve(name)?;
        self.injector.maybe_fail(write_faults(), "write", &path)?;
        let old_len = file_len(&path);
        self.check_quota(old_len, data.len() as u64, &path)?;

        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent).map_err(|e| StorageError::Io {
                path: parent.to_path_buf(),
                source: e,
            })?;
        }
        std::fs::write(&path, data).map_err(|e| StorageError::Io {
            path: path.clone(),
            source: e,
        })?;

        // The write replaced `old_len` bytes with `data.len()` bytes.
        self.track_bytes(old_len, data.len() as u64);

        Ok(())
    }

    /// Read all bytes of `name`.
    pub fn read(&mut self, name: &str) -> StorageResult<Vec<u8>> {
        let path = self.resolve(name)?;
        self.injector.maybe_fail(read_faults(), "read", &path)?;
        std::fs::read(&path).map_err(|e| {
            if e.kind() == io::ErrorKind::NotFound {
                StorageError::NotFound { path }
            } else {
                StorageError::Io {
                    path: path.clone(),
                    source: e,
                }
            }
        })
    }

    /// Delete `name` if present; deleting a missing file is a no-op.
    pub fn delete(&mut self, name: &str) -> StorageResult<()> {
        let path = self.resolve(name)?;
        self.injector.maybe_fail(delete_faults(), "delete", &path)?;
        let old_len = file_len(&path);
        match std::fs::remove_file(&path) {
            Ok(()) => {
                self.track_bytes(old_len, 0);
                Ok(())
            }
            Err(e) if e.kind() == io::ErrorKind::NotFound => Ok(()),
            Err(e) => Err(StorageError::Io { path, source: e }),
        }
    }

    pub fn exists(&mut self, name: &str) -> StorageResult<bool> {
        let path = self.resolve(name)?;
        Ok(path.exists())
    }

    /// O(1) quota check against the in-memory counter. `old_len` is the
    /// size of the file being replaced (0 for new files) so overwrites
    /// don't double-count.
    fn check_quota(&self, old_len: u64, incoming: u64, path: &Path) -> StorageResult<()> {
        let Some(limit) = self.config.max_size_bytes else {
            return Ok(());
        };
        let projected = self
            .used_bytes
            .saturating_sub(old_len)
            .saturating_add(incoming);
        if projected > limit {
            return Err(StorageError::QuotaExceeded {
                path: path.to_path_buf(),
                limit,
            });
        }
        Ok(())
    }

    /// Adjusts the usage counter after a mutation (`old_len` bytes
    /// replaced by `new_len`). No-op when no quota is configured —
    /// nothing consumes `used_bytes` in that case.
    fn track_bytes(&mut self, old_len: u64, new_len: u64) {
        if self.config.max_size_bytes.is_none() {
            return;
        }
        self.used_bytes = self
            .used_bytes
            .saturating_sub(old_len)
            .saturating_add(new_len);
        self.ops_since_reconcile += 1;
        if self.ops_since_reconcile >= RECONCILE_EVERY_OPS {
            if let Err(e) = self.reconcile() {
                tracing::warn!(error = %e, "storage quota reconcile failed; keeping last known usage");
            }
        }
    }
}

/// Size in bytes of the file at `path`, or 0 when absent/not a file.
fn file_len(path: &Path) -> u64 {
    std::fs::metadata(path)
        .ok()
        .filter(|m| m.is_file())
        .map(|m| m.len())
        .unwrap_or(0)
}

/// Total size in bytes of all files under `dir`.
pub fn dir_size(dir: &Path) -> io::Result<u64> {
    let mut total = 0u64;
    for entry in std::fs::read_dir(dir)? {
        let entry = entry?;
        let path = entry.path();
        if path.is_dir() {
            total = total.saturating_add(dir_size(&path)?);
        } else {
            total = total.saturating_add(entry.metadata()?.len());
        }
    }
    Ok(total)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::atomic::{AtomicU64, Ordering};

    static NEXT_DIR: AtomicU64 = AtomicU64::new(0);

    fn temp_root() -> PathBuf {
        let dir = std::env::temp_dir().join(format!(
            "kaladoc-storage-test-{}-{}",
            std::process::id(),
            NEXT_DIR.fetch_add(1, Ordering::Relaxed)
        ));
        std::fs::create_dir_all(&dir).unwrap();
        dir
    }

    fn fs_with_quota(root: &Path, quota: u64) -> FaultInjectingFs {
        FaultInjectingFs::new(StorageConfig::new(root).with_quota(quota)).unwrap()
    }

    #[test]
    fn quota_tracks_writes_overwrites_and_deletes() {
        let root = temp_root();
        let mut fs = fs_with_quota(&root, 100);

        fs.write("a.bin", &[0u8; 60]).unwrap();
        assert_eq!(fs.used_bytes(), 60);

        // Overwrite with a smaller file frees quota headroom.
        fs.write("a.bin", &[0u8; 10]).unwrap();
        assert_eq!(fs.used_bytes(), 10);

        // 60 more fits (10 + 60 = 70 <= 100), 91 does not.
        fs.write("b.bin", &[0u8; 60]).unwrap();
        assert!(matches!(
            fs.write("c.bin", &[0u8; 91]),
            Err(StorageError::QuotaExceeded { .. })
        ));
        assert_eq!(fs.used_bytes(), 70);

        fs.delete("a.bin").unwrap();
        assert_eq!(fs.used_bytes(), 60);

        fs.write("c.bin", &[0u8; 40]).unwrap();
        assert_eq!(fs.used_bytes(), 100);

        let _ = std::fs::remove_dir_all(&root);
    }

    #[test]
    fn quota_seeded_from_existing_files() {
        let root = temp_root();
        std::fs::write(root.join("old.bin"), [0u8; 30]).unwrap();

        let mut fs = fs_with_quota(&root, 50);
        assert_eq!(fs.used_bytes(), 30);

        // Only 20 bytes of headroom remain.
        assert!(matches!(
            fs.write("new.bin", &[0u8; 21]),
            Err(StorageError::QuotaExceeded { .. })
        ));
        fs.write("new.bin", &[0u8; 20]).unwrap();
        assert_eq!(fs.used_bytes(), 50);

        let _ = std::fs::remove_dir_all(&root);
    }

    #[test]
    fn reconcile_repairs_drift() {
        let root = temp_root();
        let mut fs = fs_with_quota(&root, 1000);
        fs.write("a.bin", &[0u8; 10]).unwrap();

        // Out-of-band change the counter can't see.
        std::fs::write(root.join("external.bin"), [0u8; 25]).unwrap();
        assert_eq!(fs.used_bytes(), 10);

        assert_eq!(fs.reconcile().unwrap(), 35);
        assert_eq!(fs.used_bytes(), 35);

        let _ = std::fs::remove_dir_all(&root);
    }

    #[test]
    fn no_quota_means_no_accounting() {
        let root = temp_root();
        let mut fs = FaultInjectingFs::new(StorageConfig::new(&root)).unwrap();
        fs.write("big.bin", &[0u8; 1 << 20]).unwrap();
        assert_eq!(fs.used_bytes(), 0);
        let _ = std::fs::remove_dir_all(&root);
    }
}
