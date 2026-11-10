//! Streaming storage ingestion: bounded-chunk uploads, atomic
//! publication, streaming reads, and staging cleanup.
//!
//! The whole-object [`StorageBackend::write`] API requires callers to
//! hold the entire object in memory. This module adds a streaming
//! counterpart built on three pieces:
//!
//! - [`StreamLimits`] — per-upload bounds: an optional per-object byte
//!   cap (`max_object_bytes`, independent of the backend-wide quota)
//!   and a per-`write_chunk` call cap (`max_chunk_bytes`).
//! - [`StreamingBackend`] — a trait extending [`StorageBackend`] with
//!   `begin_upload`, `open_reader`, and `sweep_staging`. It uses a GAT
//!   (`type Upload<'a>`) so each backend chooses its own writer type:
//!   the local backend returns a writer borrowing the backend
//!   mutably; a remote backend could return an owned writer holding a
//!   client handle instead.
//! - [`LocalUploadWriter`] — the concrete writer for
//!   [`FaultInjectingFs`]. Bytes go to a unique file under
//!   `<root>/.staging/` (never the final path), are hashed
//!   incrementally with SHA-256, and are moved into place by an atomic
//!   rename on [`StreamUpload::commit`].
//!
//! # Semantics
//!
//! - **Staging**: staged files live in `<root>/.staging/` — a reserved
//!   directory name; do not store user objects at `.staging/...`.
//! - **Atomicity**: the final object only ever appears via `rename`,
//!   so readers never observe a partially written object. `commit`
//!   replaces an existing object (last commit wins).
//! - **Quota**: staged bytes count toward `max_size_bytes` as they are
//!   written; `commit` subtracts the replaced object's old size;
//!   `abort`/`Drop` release the staged bytes. Quota is therefore
//!   enforced *during* streaming, not just at commit.
//! - **Cancellation**: `abort()` or dropping the writer removes the
//!   staged file and releases its accounted bytes. `Drop` is
//!   best-effort — callers that need a guarantee should call
//!   `abort()` explicitly. [`StreamingBackend::sweep_staging`] removes
//!   staged files left behind by crashes (entries older than a given
//!   age).
//! - **Borrowing**: `LocalUploadWriter<'a>` holds `&'a mut
//!   FaultInjectingFs`, so only one upload can be in flight per backend
//!   instance. For parallel uploads, shard backends (one per worker)
//!   or wrap the backend so each worker gets its own handle; staged
//!   bytes from all instances still share the on-disk root and are
//!   covered by `sweep_staging`.
//! - **Sync API**: everything here is blocking `std::fs` I/O, matching
//!   the rest of the storage engine. Async callers should drive it
//!   through `tokio::task::spawn_blocking` (or equivalent).
//!
//! # Example
//!
//! ```no_run
//! use kaladoc::crates::storage::local::config::StorageConfig;
//! use kaladoc::crates::storage::local::debug_when_full::FaultInjectingFs;
//! use kaladoc::crates::storage::stream::{StreamLimits, StreamUpload, StreamingBackend};
//!
//! let mut fs = FaultInjectingFs::new(
//!     StorageConfig::new("./storage").with_quota(1 << 30),
//! )
//! .unwrap()
//! .with_stream_limits(StreamLimits::new(Some(64 << 20)));
//!
//! # let payload: &[u8] = b"example payload";
//! let mut upload = fs.begin_upload("documents/report.pdf").unwrap();
//! for chunk in payload.chunks(1 << 20) {
//!     upload.write_chunk(chunk).unwrap();
//! }
//! let receipt = upload.commit().unwrap();
//! println!("published {} bytes, sha256={}", receipt.bytes, receipt.sha256);
//! ```

use std::fs::{self, File};
use std::io::{self, Read, Write};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};

use super::backend::StorageBackend;
use super::local::debug_when_full::{
    FaultInjectingFs, StorageError, StorageResult, delete_faults, file_len, read_faults,
    write_faults,
};

/// Name of the reserved staging directory inside the storage root.
/// Uploads in flight live here until committed; crashed uploads are
/// reclaimed by [`StreamingBackend::sweep_staging`].
pub const STAGING_DIR_NAME: &str = ".staging";

/// Default cap on a single `write_chunk` call: 8 MiB. Larger slices
/// are rejected with [`StorageError::TooLarge`] so a caller can't
/// smuggle an unbounded buffer through the streaming API.
pub const DEFAULT_MAX_CHUNK_BYTES: usize = 8 * 1024 * 1024;

/// Per-upload byte limits enforced by streaming writers.
///
/// `max_object_bytes` is the total-size cap for one uploaded object —
/// distinct from the backend-wide `max_size_bytes` quota, which caps
/// *all* bytes under the root. `None` means no per-object cap (the
/// quota still applies). `max_chunk_bytes` bounds each individual
/// `write_chunk`/`write` call.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default)]
pub struct StreamLimits {
    /// Maximum total bytes accepted for one uploaded object.
    pub max_object_bytes: Option<u64>,
    /// Maximum bytes accepted in a single `write_chunk` call.
    pub max_chunk_bytes: usize,
}

impl Default for StreamLimits {
    fn default() -> Self {
        Self {
            max_object_bytes: None,
            max_chunk_bytes: DEFAULT_MAX_CHUNK_BYTES,
        }
    }
}

impl StreamLimits {
    /// Limits with the given per-object cap and the default chunk cap.
    pub fn new(max_object_bytes: Option<u64>) -> Self {
        Self {
            max_object_bytes,
            ..Default::default()
        }
    }

    /// Builder: override the per-chunk call cap.
    pub fn with_max_chunk_bytes(mut self, bytes: usize) -> Self {
        self.max_chunk_bytes = bytes;
        self
    }
}

/// Proof that an upload was published: the final object name, its
/// size in bytes, and the lowercase-hex SHA-256 of its content.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CommitReceipt {
    /// The confined object name the bytes were published to.
    pub name: String,
    /// Total bytes committed.
    pub bytes: u64,
    /// Lowercase hex SHA-256 digest of the committed content.
    pub sha256: String,
}

/// An in-flight staged upload.
///
/// Implementations must guarantee:
///
/// - `write_chunk` accepts bounded chunks and enforces the backend's
///   [`StreamLimits`]; failures return [`StorageError::TooLarge`] for
///   limit violations and never publish anything.
/// - `commit` publishes the staged bytes at the target name atomically
///   (the final path never shows partial content) and returns a
///   [`CommitReceipt`] carrying the SHA-256 digest.
/// - `abort` discards the staged bytes; dropping the writer without
///   committing must also clean up (best-effort).
/// - No failure path leaves a partially written object at the final
///   name or leaks accounted quota.
pub trait StreamUpload {
    /// Append `chunk` to the staged object.
    ///
    /// # Errors
    /// - [`StorageError::TooLarge`] when `chunk` exceeds
    ///   `max_chunk_bytes` or would push the object past
    ///   `max_object_bytes`.
    /// - [`StorageError::QuotaExceeded`] when the backend quota would
    ///   be exceeded.
    /// - [`StorageError::Io`]/[`StorageError::InjectedFault`] on
    ///   staging I/O failures.
    fn write_chunk(&mut self, chunk: &[u8]) -> StorageResult<()>;

    /// Publish the staged bytes at the target name atomically and
    /// return the commit receipt (name, size, SHA-256 hex digest).
    ///
    /// Consumes the writer: on success the staged file no longer
    /// exists; on failure the staged file and its quota accounting
    /// are cleaned up before the error is returned.
    fn commit(self) -> StorageResult<CommitReceipt>;

    /// Discard the staged bytes. Consumes the writer. Deleting an
    /// already-cleaned upload is not possible — `commit`/`abort`
    /// consume `self`, so each upload ends exactly once.
    fn abort(self) -> StorageResult<()>;

    /// Bytes accepted so far (0 before the first chunk).
    fn staged_bytes(&self) -> u64;
}

/// Streaming extension to [`StorageBackend`].
///
/// Kept as a separate trait (rather than growing `StorageBackend`)
/// because streaming is an *optional capability*: small-object
/// backends and the remote stub need not implement it, and the GAT
/// lets each backend pick its writer representation. Any backend
/// that implements this also satisfies the base contract, so callers
/// can use `dyn StorageBackend` for whole-object ops and concrete or
/// generic types for streaming.
///
/// All methods are synchronous/blocking like the rest of the storage
/// engine; async callers should wrap them in `spawn_blocking`.
pub trait StreamingBackend: StorageBackend {
    /// The writer type for in-flight uploads on this backend.
    type Upload<'a>: StreamUpload
    where
        Self: 'a;

    /// The per-upload limits this backend enforces.
    fn stream_limits(&self) -> StreamLimits;

    /// Begin a staged upload destined for `name`.
    ///
    /// `name` is validated eagerly: traversal/absolute paths fail with
    /// [`StorageError::PathOutsideRoot`] here, before any bytes move.
    /// The returned writer borrows the backend mutably for its
    /// lifetime (see module docs for the single-in-flight-upload
    /// consequence).
    fn begin_upload(&mut self, name: &str) -> StorageResult<Self::Upload<'_>>;

    /// Open a streaming reader for `name`.
    ///
    /// Returns an `io::Read` over the stored bytes so callers never
    /// buffer the whole object; use [`StorageBackend::read`] when the
    /// whole object is genuinely needed. Fails with
    /// [`StorageError::NotFound`] when `name` does not exist.
    fn open_reader(&mut self, name: &str) -> StorageResult<Box<dyn Read + Send>>;

    /// Delete staged-upload leftovers older than `max_age`.
    ///
    /// Staged files are normally removed by `abort`/`Drop`/`commit`,
    /// but a process crash can strand them. Call this at startup (or
    /// periodically) so crashes don't leak disk. Returns the number
    /// of entries removed; a missing staging directory is a no-op
    /// returning `0`. Per-entry failures are skipped (they'll be
    /// retried on the next sweep) rather than aborting the sweep.
    fn sweep_staging(&mut self, max_age: Duration) -> StorageResult<usize>;
}

/// Monotonic-ish counter for unique staging names within a process.
static STAGED_COUNTER: AtomicU64 = AtomicU64::new(0);

/// Generates a collision-resistant staging file name. Combines pid,
/// a process-local counter, and wall-clock nanos — unique enough that
/// two writers (even across processes sharing a root) won't collide.
fn unique_staged_name() -> String {
    let nanos = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_nanos())
        .unwrap_or(0);
    let seq = STAGED_COUNTER.fetch_add(1, Ordering::Relaxed);
    format!("{}-{:x}-{:x}.part", std::process::id(), seq, nanos)
}

/// Local-backend upload writer: staged file + incremental SHA-256 +
/// quota accounting, published by rename on `commit`.
///
/// See the [module docs](self) for staging layout, quota semantics,
/// and cancellation guarantees.
pub struct LocalUploadWriter<'a> {
    fs: &'a mut FaultInjectingFs,
    /// Final confined object name (as passed to `begin_upload`).
    name: String,
    /// Resolved absolute path of the final object.
    final_path: PathBuf,
    /// Path of the staged file under `<root>/.staging/`.
    staged_path: PathBuf,
    /// Open handle to the staged file.
    file: Option<File>,
    /// Incremental SHA-256 over accepted chunks.
    hasher: Sha256,
    /// Bytes accepted so far (also the amount added to quota usage).
    bytes_written: u64,
    /// Limits captured at `begin_upload`.
    limits: StreamLimits,
    /// Set once the writer has been committed or aborted so `Drop`
    /// doesn't redo cleanup.
    finished: bool,
}

impl<'a> LocalUploadWriter<'a> {
    /// Begins a staged upload on `fs` for final object `name`.
    ///
    /// Validates `name` (traversal fails here), creates the staging
    /// dir if needed, and creates a uniquely named staged file.
    pub fn begin(fs: &'a mut FaultInjectingFs, name: &str) -> StorageResult<Self> {
        let limits = fs.stream_limits();
        let final_path = fs.resolve(name)?;
        fs.inject(write_faults(), "begin_upload", &final_path)?;

        let staging_dir = fs.root_dir().join(STAGING_DIR_NAME);
        fs::create_dir_all(&staging_dir).map_err(|source| StorageError::Io {
            path: staging_dir.clone(),
            source,
        })?;
        let staged_path = staging_dir.join(unique_staged_name());
        let file = File::create(&staged_path).map_err(|source| StorageError::Io {
            path: staged_path.clone(),
            source,
        })?;

        Ok(Self {
            fs,
            name: name.to_string(),
            final_path,
            staged_path,
            file: Some(file),
            hasher: Sha256::new(),
            bytes_written: 0,
            limits,
            finished: false,
        })
    }

    /// Removes the staged file and releases its quota accounting.
    /// Shared by `abort`, `Drop`, and `commit`'s error path.
    ///
    /// On a remove failure the bytes stay accounted — the file is
    /// still on disk, so the count remains accurate until a later
    /// `sweep_staging`/reconcile corrects it.
    fn discard(&mut self) -> StorageResult<()> {
        match fs::remove_file(&self.staged_path) {
            Ok(()) => {}
            Err(e) if e.kind() == io::ErrorKind::NotFound => {}
            Err(source) => {
                return Err(StorageError::Io {
                    path: self.staged_path.clone(),
                    source,
                });
            }
        }
        // Staged bytes were added to usage chunk-by-chunk; removing the
        // file releases exactly `bytes_written`.
        self.fs.track_bytes(self.bytes_written, 0);
        self.bytes_written = 0;
        Ok(())
    }

    /// Fallible part of `commit`: sync, rename into place, fix quota.
    /// On `Err` the staged file is guaranteed to still exist at
    /// `staged_path` with its bytes still accounted, so the caller can
    /// safely `discard`.
    fn commit_inner(&mut self) -> StorageResult<CommitReceipt> {
        // Flush + fsync so the rename publishes durable bytes.
        if let Some(file) = self.file.take() {
            file.sync_all().map_err(|source| StorageError::Io {
                path: self.staged_path.clone(),
                source,
            })?;
        }

        self.fs
            .inject(write_faults(), "commit_upload", &self.final_path)?;

        // Bytes the final object currently occupies (0 if absent);
        // staged bytes are already inside `used_bytes`.
        let old_len = file_len(&self.final_path);

        if let Some(parent) = self.final_path.parent() {
            fs::create_dir_all(parent).map_err(|source| StorageError::Io {
                path: parent.to_path_buf(),
                source,
            })?;
        }

        rename_replace(&self.staged_path, &self.final_path)?;

        // Rename moved `bytes_written` bytes from staging to the final
        // path and removed `old_len` bytes of any previous object.
        // Staged bytes were already counted, so only the replaced
        // object's size leaves the usage counter.
        self.fs.track_bytes(old_len, 0);

        let sha256 = hex_lower(&self.hasher.clone().finalize());
        Ok(CommitReceipt {
            name: self.name.clone(),
            bytes: self.bytes_written,
            sha256,
        })
    }
}

impl StreamUpload for LocalUploadWriter<'_> {
    fn write_chunk(&mut self, chunk: &[u8]) -> StorageResult<()> {
        if self.finished {
            return Err(StorageError::Io {
                path: self.staged_path.clone(),
                source: io::Error::other("upload already finished"),
            });
        }
        let chunk_len = chunk.len() as u64;

        if chunk_len > self.limits.max_chunk_bytes as u64 {
            return Err(StorageError::TooLarge {
                path: self.final_path.clone(),
                max_bytes: self.limits.max_chunk_bytes as u64,
            });
        }
        if let Some(max) = self.limits.max_object_bytes
            && self.bytes_written.saturating_add(chunk_len) > max
        {
            return Err(StorageError::TooLarge {
                path: self.final_path.clone(),
                max_bytes: max,
            });
        }

        self.fs
            .inject(write_faults(), "upload_write", &self.staged_path)?;
        // Quota: staged bytes occupy the root now, so each accepted
        // chunk must fit under `max_size_bytes` immediately.
        self.fs.check_quota(0, chunk_len, &self.final_path)?;

        let file = self.file.as_mut().ok_or_else(|| StorageError::Io {
            path: self.staged_path.clone(),
            source: io::Error::other("upload file closed"),
        })?;
        file.write_all(chunk).map_err(|source| StorageError::Io {
            path: self.staged_path.clone(),
            source,
        })?;

        self.hasher.update(chunk);
        self.bytes_written += chunk_len;
        self.fs.track_bytes(0, chunk_len);
        Ok(())
    }

    fn commit(mut self) -> StorageResult<CommitReceipt> {
        match self.commit_inner() {
            Ok(receipt) => {
                self.finished = true;
                Ok(receipt)
            }
            Err(e) => {
                // commit_inner only fails before the rename, so the
                // staged file (and its quota) is still ours to clean.
                let _ = self.discard();
                self.finished = true;
                Err(e)
            }
        }
    }

    fn abort(mut self) -> StorageResult<()> {
        let result = self.discard();
        self.finished = true;
        result
    }

    fn staged_bytes(&self) -> u64 {
        self.bytes_written
    }
}

/// `Drop` is the safety net for forgotten uploads: removes the staged
/// file and releases its accounted bytes. Best-effort — errors are
/// swallowed because `Drop` can't report them; `sweep_staging`
/// catches anything left behind (e.g. a failed remove on Windows
/// while the file handle was still open).
impl Drop for LocalUploadWriter<'_> {
    fn drop(&mut self) {
        if self.finished {
            return;
        }
        // Close the handle first so Windows can delete the file.
        drop(self.file.take());
        let _ = self.discard();
    }
}

/// `io::Write` adapter so callers can use `write_all`/`write_vectored`
/// ergonomics; each call maps to one bounded `write_chunk`.
impl Write for LocalUploadWriter<'_> {
    fn write(&mut self, buf: &[u8]) -> io::Result<usize> {
        self.write_chunk(buf).map_err(io::Error::from)?;
        Ok(buf.len())
    }

    fn write_all(&mut self, buf: &[u8]) -> io::Result<()> {
        self.write_chunk(buf).map_err(io::Error::from)
    }

    fn flush(&mut self) -> io::Result<()> {
        match self.file.as_mut() {
            Some(file) => file.flush(),
            None => Ok(()),
        }
    }
}

impl StreamingBackend for FaultInjectingFs {
    type Upload<'a> = LocalUploadWriter<'a>;

    fn stream_limits(&self) -> StreamLimits {
        FaultInjectingFs::stream_limits(self)
    }

    fn begin_upload(&mut self, name: &str) -> StorageResult<Self::Upload<'_>> {
        LocalUploadWriter::begin(self, name)
    }

    fn open_reader(&mut self, name: &str) -> StorageResult<Box<dyn Read + Send>> {
        let path = self.resolve(name)?;
        self.inject(read_faults(), "open_read", &path)?;
        let file = File::open(&path).map_err(|e| {
            if e.kind() == io::ErrorKind::NotFound {
                StorageError::NotFound { path }
            } else {
                StorageError::Io {
                    path: path.clone(),
                    source: e,
                }
            }
        })?;
        Ok(Box::new(file))
    }

    fn sweep_staging(&mut self, max_age: Duration) -> StorageResult<usize> {
        let staging_dir = self.root_dir().join(STAGING_DIR_NAME);
        self.inject(delete_faults(), "sweep_staging", &staging_dir)?;

        let entries = match fs::read_dir(&staging_dir) {
            Ok(entries) => entries,
            // Missing staging dir = nothing to sweep.
            Err(e) if e.kind() == io::ErrorKind::NotFound => return Ok(0),
            Err(source) => {
                return Err(StorageError::Io {
                    path: staging_dir,
                    source,
                });
            }
        };

        let mut removed = 0usize;
        for entry in entries.flatten() {
            let path = entry.path();
            if !path.is_file() {
                continue;
            }
            // Keep entries whose age can't be determined or is fresh.
            let stale = entry
                .metadata()
                .and_then(|m| m.modified())
                .ok()
                .and_then(|t| t.elapsed().ok())
                .is_some_and(|age| age > max_age);
            if !stale {
                continue;
            }
            match fs::remove_file(&path) {
                Ok(()) => removed += 1,
                // Raced with another sweeper or still locked — skip.
                Err(_) => continue,
            }
        }
        Ok(removed)
    }
}

/// Renames `staged` over `final_path`, replacing any existing object.
///
/// `rename` is atomic when both paths are on the same filesystem —
/// guaranteed here because staging lives inside the storage root.
/// On Unix an existing destination is replaced atomically. On Windows
/// `std::fs::rename` fails with `AlreadyExists` when the destination
/// exists, so we fall back to remove-then-rename (a small non-atomic
/// window documented for callers who need strict Windows semantics).
fn rename_replace(staged: &Path, final_path: &Path) -> StorageResult<()> {
    match fs::rename(staged, final_path) {
        Ok(()) => Ok(()),
        Err(e) if e.kind() == io::ErrorKind::AlreadyExists && final_path.exists() => {
            fs::remove_file(final_path).map_err(|source| StorageError::Io {
                path: final_path.to_path_buf(),
                source,
            })?;
            fs::rename(staged, final_path).map_err(|source| StorageError::Io {
                path: final_path.to_path_buf(),
                source,
            })
        }
        Err(source) => Err(StorageError::Io {
            path: final_path.to_path_buf(),
            source,
        }),
    }
}

/// Lowercase hex encoding of a digest (no `hex` crate dependency).
fn hex_lower(bytes: &[u8]) -> String {
    const HEX: &[u8; 16] = b"0123456789abcdef";
    let mut out = String::with_capacity(bytes.len() * 2);
    for &b in bytes {
        out.push(HEX[(b >> 4) as usize] as char);
        out.push(HEX[(b & 0x0f) as usize] as char);
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::crates::storage::local::config::StorageConfig;
    use std::fs as stdfs;
    use std::sync::atomic::{AtomicU64, Ordering};

    static NEXT_DIR: AtomicU64 = AtomicU64::new(0);

    fn temp_root() -> PathBuf {
        let dir = std::env::temp_dir().join(format!(
            "kaladoc-stream-test-{}-{}",
            std::process::id(),
            NEXT_DIR.fetch_add(1, Ordering::Relaxed)
        ));
        std::fs::create_dir_all(&dir).unwrap();
        dir
    }

    fn fs_at(root: &Path) -> FaultInjectingFs {
        FaultInjectingFs::new(StorageConfig::new(root)).unwrap()
    }

    fn sha256_hex(data: &[u8]) -> String {
        hex_lower(&Sha256::digest(data))
    }

    fn staging_files(root: &Path) -> Vec<PathBuf> {
        let dir = root.join(STAGING_DIR_NAME);
        match stdfs::read_dir(&dir) {
            Ok(entries) => entries.flatten().map(|e| e.path()).collect(),
            Err(_) => Vec::new(),
        }
    }

    #[test]
    fn chunked_upload_matches_write_and_checksum() {
        let root = temp_root();
        let mut fs = fs_at(&root);

        // Deterministic multi-MiB payload (3 MiB + odd tail).
        let mut payload = vec![0u8; 3 * 1024 * 1024 + 12345];
        for (i, b) in payload.iter_mut().enumerate() {
            *b = (i % 251) as u8;
        }

        let mut upload = fs.begin_upload("docs/blob.bin").unwrap();
        for chunk in payload.chunks(64 * 1024) {
            upload.write_chunk(chunk).unwrap();
        }
        let receipt = upload.commit().unwrap();

        assert_eq!(receipt.name, "docs/blob.bin");
        assert_eq!(receipt.bytes, payload.len() as u64);
        assert_eq!(receipt.sha256, sha256_hex(&payload));
        assert_eq!(fs.read("docs/blob.bin").unwrap(), payload);

        // Final content equals what a whole-object write() would store.
        let mut fs2 = fs_at(&root);
        fs2.write("docs/whole.bin", &payload).unwrap();
        assert_eq!(fs.read("docs/whole.bin").unwrap(), payload);

        // Nothing left staged.
        assert!(staging_files(&root).is_empty());
        let _ = stdfs::remove_dir_all(&root);
    }

    #[test]
    fn oversized_upload_fails_and_abort_leaves_no_residue() {
        let root = temp_root();
        let limits = StreamLimits::new(Some(1 << 20)); // 1 MiB object cap
        let mut fs = FaultInjectingFs::new(StorageConfig::new(&root))
            .unwrap()
            .with_stream_limits(limits);

        let mut upload = fs.begin_upload("big.bin").unwrap();
        upload.write_chunk(&[0u8; 600 * 1024]).unwrap();
        assert_eq!(upload.staged_bytes(), 600 * 1024);

        // This chunk would push the object to ~1.17 MiB > 1 MiB.
        let err = upload.write_chunk(&[0u8; 600 * 1024]).unwrap_err();
        assert!(matches!(err, StorageError::TooLarge { max_bytes, .. } if max_bytes == 1 << 20));

        upload.abort().unwrap();
        assert!(!fs.exists("big.bin").unwrap());
        assert!(staging_files(&root).is_empty());
        assert_eq!(fs.used_bytes(), 0);
        let _ = stdfs::remove_dir_all(&root);
    }

    #[test]
    fn chunk_larger_than_cap_rejected() {
        let root = temp_root();
        let limits = StreamLimits::new(None).with_max_chunk_bytes(1024);
        let mut fs = FaultInjectingFs::new(StorageConfig::new(&root))
            .unwrap()
            .with_stream_limits(limits);

        let mut upload = fs.begin_upload("x.bin").unwrap();
        let err = upload.write_chunk(&[0u8; 2048]).unwrap_err();
        assert!(matches!(err, StorageError::TooLarge { .. }));
        upload.abort().unwrap();
        let _ = stdfs::remove_dir_all(&root);
    }

    #[test]
    fn commit_publishes_atomically_and_replaces() {
        let root = temp_root();
        let mut fs = fs_at(&root);

        // Pre-existing object gets replaced.
        fs.write("obj.bin", b"old-content").unwrap();

        let mut u1 = fs.begin_upload("obj.bin").unwrap();
        u1.write_chunk(b"new-content-v1").unwrap();
        let receipt = u1.commit().unwrap();
        assert_eq!(receipt.bytes, 14);
        assert_eq!(fs.read("obj.bin").unwrap(), b"new-content-v1");
        assert_eq!(receipt.sha256, sha256_hex(b"new-content-v1"));

        // Same-name last-commit-wins.
        let mut u3 = fs.begin_upload("obj.bin").unwrap();
        u3.write_chunk(b"v2-wins").unwrap();
        u3.commit().unwrap();
        assert_eq!(fs.read("obj.bin").unwrap(), b"v2-wins");

        // An aborted same-name upload leaves the committed object intact.
        let mut u4 = fs.begin_upload("obj.bin").unwrap();
        u4.write_chunk(b"aborted-garbage").unwrap();
        u4.abort().unwrap();
        assert_eq!(fs.read("obj.bin").unwrap(), b"v2-wins");

        assert!(staging_files(&root).is_empty());
        let _ = stdfs::remove_dir_all(&root);
    }

    #[test]
    fn aborted_upload_never_replaces_existing_final() {
        let root = temp_root();
        let mut fs = fs_at(&root);
        fs.write("keep.bin", b"original").unwrap();

        {
            let mut up = fs.begin_upload("keep.bin").unwrap();
            up.write_chunk(b"partial-garbage").unwrap();
            up.abort().unwrap();
        }
        assert_eq!(fs.read("keep.bin").unwrap(), b"original");
        assert!(staging_files(&root).is_empty());

        // Drop-without-commit also cleans up.
        {
            let mut up = fs.begin_upload("keep.bin").unwrap();
            up.write_chunk(b"never-committed").unwrap();
            // dropped here
        }
        assert_eq!(fs.read("keep.bin").unwrap(), b"original");
        assert!(staging_files(&root).is_empty());
        let _ = stdfs::remove_dir_all(&root);
    }

    #[test]
    fn sweep_staging_removes_only_stale_entries() {
        let root = temp_root();
        let mut fs = fs_at(&root);

        // No staging dir yet → no-op.
        assert_eq!(fs.sweep_staging(Duration::from_secs(60)).unwrap(), 0);

        // Create two staged leftovers manually.
        let staging = root.join(STAGING_DIR_NAME);
        stdfs::create_dir_all(&staging).unwrap();
        let fresh = staging.join("fresh.part");
        let old = staging.join("old.part");
        stdfs::write(&fresh, b"x").unwrap();
        stdfs::write(&old, b"y").unwrap();
        // Age `old` by writing then... can't set mtime without extra
        // crates; instead sweep with max_age=0 removes everything,
        // and a huge max_age removes nothing.
        assert_eq!(fs.sweep_staging(Duration::from_secs(3600)).unwrap(), 0);
        assert!(fresh.exists() && old.exists());
        assert_eq!(fs.sweep_staging(Duration::ZERO).unwrap(), 2);
        assert!(!fresh.exists() && !old.exists());
        let _ = stdfs::remove_dir_all(&root);
    }

    #[test]
    fn path_traversal_rejected_for_uploads_and_readers() {
        let root = temp_root();
        let mut fs = fs_at(&root);

        for bad in ["../escape.bin", "a/../../escape.bin", "/tmp/abs.bin"] {
            assert!(matches!(
                fs.begin_upload(bad),
                Err(StorageError::PathOutsideRoot { .. })
            ));
            assert!(matches!(
                fs.open_reader(bad),
                Err(StorageError::PathOutsideRoot { .. })
            ));
        }
        // And nothing escaped the root.
        assert!(!root.parent().unwrap().join("escape.bin").exists());
        let _ = stdfs::remove_dir_all(&root);
    }

    #[test]
    fn open_reader_streams_without_full_buffer() {
        let root = temp_root();
        let mut fs = fs_at(&root);
        let payload: Vec<u8> = (0..200_000u32).map(|i| (i % 256) as u8).collect();
        fs.write("stream.bin", &payload).unwrap();

        let mut reader = fs.open_reader("stream.bin").unwrap();
        let mut buf = Vec::new();
        reader.read_to_end(&mut buf).unwrap();
        assert_eq!(buf, payload);

        assert!(matches!(
            fs.open_reader("missing.bin"),
            Err(StorageError::NotFound { .. })
        ));
        let _ = stdfs::remove_dir_all(&root);
    }

    #[test]
    fn quota_accounts_staged_bytes_through_lifecycle() {
        let root = temp_root();
        let mut fs = FaultInjectingFs::new(StorageConfig::new(&root).with_quota(1000)).unwrap();

        let mut up = fs.begin_upload("q.bin").unwrap();
        up.write_chunk(&[0u8; 400]).unwrap();
        up.write_chunk(&[0u8; 400]).unwrap();

        // While the writer holds &mut fs we can't call fs methods, but
        // a *separate* backend instance on the same root scans the
        // staging file and sees the staged bytes on disk.
        let probe = FaultInjectingFs::new(StorageConfig::new(&root).with_quota(1000)).unwrap();
        assert_eq!(probe.used_bytes(), 800);

        // 800 staged + 300 more would exceed the 1000-byte quota.
        assert!(matches!(
            up.write_chunk(&[0u8; 300]),
            Err(StorageError::QuotaExceeded { .. })
        ));

        up.commit().unwrap();
        assert_eq!(fs.used_bytes(), 800);
        assert_eq!(fs.read("q.bin").unwrap().len(), 800);

        // Aborting a staged upload releases its bytes.
        let mut up2 = fs.begin_upload("tmp.bin").unwrap();
        up2.write_chunk(&[1u8; 100]).unwrap();
        up2.abort().unwrap();
        assert_eq!(fs.used_bytes(), 800);

        // Committing over an existing object subtracts the old size:
        // 50 staged replaces the 800-byte q.bin -> net 50.
        let mut up3 = fs.begin_upload("q.bin").unwrap();
        up3.write_chunk(&[2u8; 50]).unwrap();
        up3.commit().unwrap();
        assert_eq!(fs.used_bytes(), 50);
        assert_eq!(fs.read("q.bin").unwrap(), vec![2u8; 50]);
        let _ = stdfs::remove_dir_all(&root);
    }

    #[test]
    fn uploads_to_distinct_names_and_last_commit_wins() {
        let root = temp_root();
        let mut fs = fs_at(&root);

        // Writers hold &mut fs, so uploads are sequential per backend;
        // distinct names and same-name replacement both work.
        for name in ["a.bin", "b.bin", "c.bin"] {
            let mut up = fs.begin_upload(name).unwrap();
            up.write_chunk(name.as_bytes()).unwrap();
            up.commit().unwrap();
        }
        assert_eq!(fs.read("a.bin").unwrap(), b"a.bin");
        assert_eq!(fs.read("b.bin").unwrap(), b"b.bin");
        assert_eq!(fs.read("c.bin").unwrap(), b"c.bin");

        let mut up = fs.begin_upload("a.bin").unwrap();
        up.write_chunk(b"a.bin-v2").unwrap();
        up.commit().unwrap();
        assert_eq!(fs.read("a.bin").unwrap(), b"a.bin-v2");

        // Incomplete upload never appears at a final path.
        let mut up = fs.begin_upload("never.bin").unwrap();
        up.write_chunk(b"partial").unwrap();
        drop(up);
        assert!(!fs.exists("never.bin").unwrap());
        let _ = stdfs::remove_dir_all(&root);
    }

    #[test]
    fn empty_upload_commits_empty_object() {
        let root = temp_root();
        let mut fs = fs_at(&root);
        let receipt = fs.begin_upload("empty.bin").unwrap().commit().unwrap();
        assert_eq!(receipt.bytes, 0);
        assert_eq!(receipt.sha256, sha256_hex(b""));
        assert_eq!(fs.read("empty.bin").unwrap(), b"");
        let _ = stdfs::remove_dir_all(&root);
    }

    #[test]
    fn io_write_adapter_works() {
        let root = temp_root();
        let mut fs = fs_at(&root);
        let mut up = fs.begin_upload("w.bin").unwrap();
        up.write_all(b"via-io-write").unwrap();
        up.commit().unwrap();
        assert_eq!(fs.read("w.bin").unwrap(), b"via-io-write");
        let _ = stdfs::remove_dir_all(&root);
    }
}
