//! Content-addressed storage (CAS) and ingest-time deduplication.
//!
//! When [`StorageConfig::dedup`] is enabled, objects can be
//! *registered* under a content-addressed namespace after they are
//! committed: the object's bytes are linked (hardlink, with copy
//! fallback) into `<root>/cas/<sha256-hex>` and a name→checksum
//! index is persisted at `<root>/.cas-map.json`.
//!
//! # Layout
//!
//! - `cas/<64 lowercase hex>` — one file per distinct content. The
//!   filename *is* the SHA-256 of the content, so the namespace needs
//!   no separate checksum→blob index; listing `cas/` reveals every
//!   stored digest.
//! - `.cas-map.json` — JSON object `{ "<object name>": "<sha256>",
//!   ... }` mapping storage object names to the checksum they were
//!   last registered with. Written atomically (temp file + rename).
//!   This is the only index file; it answers "which name holds this
//!   content?" for [`FaultInjectingFs::lookup_name_by_checksum`].
//!
//! # Semantics
//!
//! - **Opt-in**: every API is a no-op (`Ok(None)` / `None` / `0`)
//!   unless [`StorageConfig::dedup`] is set. Nothing changes for
//!   existing deployments.
//! - **Explicit registration**: dedup is *not* wired into
//!   [`crate::crates::storage::stream::StreamUpload::commit`] — the
//!   ingestion layer calls [`FaultInjectingFs::register_receipt`]
//!   (or [`FaultInjectingFs::register_checksum`]) after commit. This
//!   keeps the streaming path untouched and lets callers decide
//!   per-object whether dedup applies. Wiring it into `commit` later
//!   is a one-line call once `dedup` is enabled.
//! - **Dedup hit**: if `cas/<sha256>` already exists, no bytes are
//!   copied — only the name→checksum mapping is recorded and
//!   [`CasOutcome::deduplicated`] is `true`. Ingestion can then use
//!   [`FaultInjectingFs::lookup_name_by_checksum`] to find the
//!   canonical name for identical content.
//! - **Hardlinks**: preferred over copies so deduplicated content
//!   costs no extra disk. Because `commit` publishes via `rename`,
//!   replacing an object never mutates a CAS entry — the old inode
//!   stays alive under `cas/` until [`FaultInjectingFs::sweep_orphaned_cas`]
//!   reclaims it. Falls back to `fs::copy` when hardlinks are
//!   unavailable (some filesystems/network mounts).
//! - **Reserved names**: when dedup is enabled, `cas/...` and
//!   `.cas-map.json` under the root are engine-managed; do not store
//!   user objects at those paths.
//! - **Quota**: CAS files are ordinary files under the root, so they
//!   count toward `dir_size`-based quota reconciliation exactly like
//!   any other entry (a hardlinked inode is counted once per name —
//!   conservative but consistent with `reconcile`).
//! - **Crash safety**: a crash between commit and registration just
//!   means a missing mapping (re-register later); a crash mid-map-write
//!   leaves at worst a stale temp file. A corrupt `.cas-map.json` is
//!   treated as empty (with a warning) rather than fatal.
//! - **Concurrency**: the map is read-modify-write per call with
//!   last-writer-wins semantics across processes sharing a root —
//!   acceptable for an ingestion hint, not a transactional store.

use std::collections::HashMap;
use std::fs;
use std::io;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};

use serde::{Deserialize, Serialize};

use super::local::debug_when_full::{FaultInjectingFs, StorageError, StorageResult, file_len};
use super::stream::CommitReceipt;

/// Directory (storage-relative) holding content-addressed objects:
/// `<root>/cas/<sha256-hex>`.
pub const CAS_DIR_NAME: &str = "cas";

/// Filename (storage-relative, at the root) of the name→checksum
/// index: `<root>/.cas-map.json`.
pub const CAS_MAP_NAME: &str = ".cas-map.json";

/// Length of a lowercase hex SHA-256 digest.
const SHA256_HEX_LEN: usize = 64;

/// A validated SHA-256 content address: exactly 64 hex characters,
/// normalized to lowercase.
///
/// This is a newtype so raw strings can't accidentally flow into
/// path construction unchecked — [`ContentAddress::path_segment`]
/// is guaranteed filesystem-safe.
#[derive(Debug, Clone, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(transparent)]
pub struct ContentAddress {
    /// Lowercase hex digest (64 chars).
    hex: String,
}

impl ContentAddress {
    /// Parses and validates a hex SHA-256 digest. Accepts upper- or
    /// lowercase; the stored form is always lowercase. Returns `None`
    /// for anything that isn't exactly 64 hex chars.
    pub fn parse(input: &str) -> Option<Self> {
        let trimmed = input.trim();
        if trimmed.len() != SHA256_HEX_LEN || !trimmed.bytes().all(|b| b.is_ascii_hexdigit()) {
            return None;
        }
        Some(Self {
            hex: trimmed.to_ascii_lowercase(),
        })
    }

    /// The lowercase hex digest.
    pub fn as_str(&self) -> &str {
        &self.hex
    }

    /// The digest as owned `String` (for APIs taking `String`).
    pub fn into_hex(self) -> String {
        self.hex
    }

    /// Whether `input` is a syntactically valid SHA-256 hex digest.
    pub fn is_valid(input: &str) -> bool {
        Self::parse(input).is_some()
    }

    /// Storage-relative CAS path for this content:
    /// `cas/<hex>`. Always safe — `hex` is validated.
    pub fn cas_path(&self) -> String {
        format!("{CAS_DIR_NAME}/{}", self.hex)
    }

    /// Filesystem path of the CAS object under `root`.
    pub(crate) fn abs_path(&self, root: &Path) -> PathBuf {
        root.join(CAS_DIR_NAME).join(&self.hex)
    }
}

impl std::fmt::Display for ContentAddress {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(&self.hex)
    }
}

/// Result of registering an object's checksum with the CAS layer.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CasOutcome {
    /// Storage-relative path of the content-addressed object
    /// (`cas/<hex>`).
    pub cas_ref: String,
    /// The object name the checksum was registered under.
    pub name: String,
    /// The validated content address.
    pub checksum: ContentAddress,
    /// `true` when `cas/<hex>` already existed — the bytes were *not*
    /// copied again (a dedup hit). `false` when this registration
    /// stored the content for the first time.
    pub deduplicated: bool,
}

/// The persisted name→checksum index. Serialized as a flat JSON
/// object: `{ "<name>": "<sha256-hex>" }`.
type ChecksumMap = HashMap<String, String>;

/// Monotonic counter for unique CAS-map temp file names.
static MAP_TMP_COUNTER: AtomicU64 = AtomicU64::new(0);

/// Absolute path of the checksum index file.
fn map_path(root: &Path) -> PathBuf {
    root.join(CAS_MAP_NAME)
}

/// Loads the checksum index. A missing file yields an empty map; a
/// corrupt/unreadable file is treated as empty (with a warning) so a
/// damaged index degrades to "no dedup info" rather than breaking
/// storage operations. The next successful registration rewrites a
/// clean map.
fn load_map(root: &Path) -> ChecksumMap {
    let path = map_path(root);
    let contents = match fs::read_to_string(&path) {
        Ok(c) => c,
        Err(e) if e.kind() == io::ErrorKind::NotFound => return ChecksumMap::new(),
        Err(e) => {
            tracing::warn!(error = %e, path = %path.display(),
                "cas index unreadable; treating as empty");
            return ChecksumMap::new();
        }
    };
    match serde_json::from_str::<ChecksumMap>(&contents) {
        Ok(map) => map,
        Err(e) => {
            tracing::warn!(error = %e, path = %path.display(),
                "cas index corrupt; treating as empty");
            ChecksumMap::new()
        }
    }
}

/// Atomically persists the checksum index: write to a unique temp
/// file in the same directory, then rename over the target (same
/// filesystem ⇒ atomic replace on Unix; Windows falls back to
/// remove-then-rename, matching `rename_replace` in `stream.rs`).
fn save_map(root: &Path, map: &ChecksumMap) -> StorageResult<()> {
    let path = map_path(root);
    let tmp = root.join(format!(
        "{CAS_MAP_NAME}.tmp-{}-{}",
        std::process::id(),
        MAP_TMP_COUNTER.fetch_add(1, Ordering::Relaxed)
    ));
    let json = serde_json::to_string(map).map_err(|source| StorageError::Io {
        path: path.clone(),
        source: io::Error::other(source),
    })?;
    fs::write(&tmp, json).map_err(|source| StorageError::Io {
        path: tmp.clone(),
        source,
    })?;
    match fs::rename(&tmp, &path) {
        Ok(()) => Ok(()),
        Err(e) if e.kind() == io::ErrorKind::AlreadyExists && path.exists() => {
            fs::remove_file(&path).map_err(|source| StorageError::Io {
                path: path.clone(),
                source,
            })?;
            fs::rename(&tmp, &path).map_err(|source| StorageError::Io {
                path: path.clone(),
                source,
            })
        }
        Err(source) => {
            let _ = fs::remove_file(&tmp);
            Err(StorageError::Io {
                path: path.clone(),
                source,
            })
        }
    }
}

impl FaultInjectingFs {
    /// Whether content-addressed dedup is enabled for this backend
    /// ([`StorageConfig::dedup`]).
    pub fn dedup_enabled(&self) -> bool {
        self.config().dedup
    }

    /// Storage-relative CAS path (`cas/<hex>`) for a SHA-256 hex
    /// digest, or `None` when dedup is disabled or the digest is
    /// malformed. Does not check whether the object exists — use
    /// [`FaultInjectingFs::exists`] or [`FaultInjectingFs::read`] on
    /// the returned name for that.
    pub fn cas_path_for(&self, sha256: &str) -> Option<String> {
        if !self.dedup_enabled() {
            return None;
        }
        ContentAddress::parse(sha256).map(|addr| addr.cas_path())
    }

    /// Registers `name`'s content checksum with the CAS layer.
    ///
    /// If `cas/<sha256>` doesn't exist yet, the object's bytes are
    /// hardlinked into place (copy fallback); the name→checksum index
    /// is then updated atomically. Returns `Ok(None)` when dedup is
    /// disabled — a no-op.
    ///
    /// # Errors
    /// - [`StorageError::Io`] wrapping [`StorageError::NotFound`]-style
    ///   failure when `sha256` is malformed — reported as `Io` with an
    ///   `InvalidData`-flavored message via `io::Error::other`.
    /// - [`StorageError::NotFound`] when `name` doesn't exist under
    ///   the root.
    /// - [`StorageError::Io`] on filesystem or index-write failures.
    pub fn register_checksum(
        &mut self,
        name: &str,
        sha256: &str,
    ) -> StorageResult<Option<CasOutcome>> {
        if !self.dedup_enabled() {
            return Ok(None);
        }
        let addr = ContentAddress::parse(sha256).ok_or_else(|| StorageError::Io {
            path: self.root_dir().join(CAS_MAP_NAME),
            source: io::Error::new(
                io::ErrorKind::InvalidData,
                format!("invalid sha256 checksum: {sha256:?}"),
            ),
        })?;

        let obj_path = self.resolve(name)?;
        if !obj_path.is_file() {
            return Err(StorageError::NotFound { path: obj_path });
        }

        let cas_abs = addr.abs_path(self.root_dir());
        let deduplicated = cas_abs.exists();
        if !deduplicated {
            let cas_dir = self.root_dir().join(CAS_DIR_NAME);
            fs::create_dir_all(&cas_dir).map_err(|source| StorageError::Io {
                path: cas_dir.clone(),
                source,
            })?;
            // Hardlink first: zero extra disk for duplicate content.
            // Copy fallback covers filesystems without hardlink
            // support (some network mounts, certain fuse setups).
            if fs::hard_link(&obj_path, &cas_abs).is_err() {
                fs::copy(&obj_path, &cas_abs).map_err(|source| StorageError::Io {
                    path: cas_abs.clone(),
                    source,
                })?;
            }
            // The new cas/<hex> entry counts toward dir_size-based
            // quota like any other file under the root.
            let cas_len = file_len(&cas_abs);
            self.track_bytes(0, cas_len);
        }

        let mut map = load_map(self.root_dir());
        map.insert(name.to_string(), addr.as_str().to_string());
        save_map(self.root_dir(), &map)?;

        Ok(Some(CasOutcome {
            cas_ref: addr.cas_path(),
            name: name.to_string(),
            checksum: addr,
            deduplicated,
        }))
    }

    /// Convenience wrapper: registers a [`CommitReceipt`] produced by
    /// [`crate::crates::storage::stream::StreamUpload::commit`].
    ///
    /// Typical ingestion flow:
    ///
    /// ```no_run
    /// # use kaladoc::crates::storage::local::config::StorageConfig;
    /// # use kaladoc::crates::storage::local::debug_when_full::FaultInjectingFs;
    /// # use kaladoc::crates::storage::stream::StreamingBackend;
    /// # let mut fs = FaultInjectingFs::new(StorageConfig::new("./s").with_dedup(true)).unwrap();
    /// # let data: &[u8] = b"payload";
    /// let mut up = fs.begin_upload("docs/a.pdf").unwrap();
    /// up.write_chunk(data).unwrap();
    /// let receipt = up.commit().unwrap();
    /// // Post-commit, opt-in dedup registration:
    /// let outcome = fs.register_receipt(&receipt).unwrap();
    /// ```
    ///
    /// Returns `Ok(None)` when dedup is disabled.
    pub fn register_receipt(
        &mut self,
        receipt: &CommitReceipt,
    ) -> StorageResult<Option<CasOutcome>> {
        self.register_checksum(&receipt.name, &receipt.sha256)
    }

    /// Looks up the object name last registered with `sha256`.
    ///
    /// Returns `None` when dedup is disabled, the checksum is
    /// malformed, the index is missing/corrupt, or no name maps to
    /// it. When several names share the checksum, prefers one whose
    /// object file still exists; falls back to any recorded name.
    pub fn lookup_name_by_checksum(&self, sha256: &str) -> Option<String> {
        if !self.dedup_enabled() {
            return None;
        }
        let addr = ContentAddress::parse(sha256)?;
        let hex = addr.as_str();
        let map = load_map(self.root_dir());
        let mut fallback: Option<String> = None;
        for (name, sum) in &map {
            if sum.eq_ignore_ascii_case(hex) {
                if fallback.is_none() {
                    fallback = Some(name.clone());
                }
                // Prefer a name whose object still exists on disk.
                if let Ok(p) = self.resolve(name)
                    && p.is_file()
                {
                    return Some(name.clone());
                }
            }
        }
        fallback
    }

    /// Removes orphaned CAS objects and stale index entries.
    ///
    /// A CAS object `cas/<hex>` is orphaned when no name in the
    /// checksum index maps to that digest — e.g. the last object with
    /// that content was deleted or re-committed with different bytes.
    /// Index entries whose object file no longer exists are also
    /// pruned. Returns the number of CAS files removed; `0` when dedup
    /// is disabled or the `cas/` directory doesn't exist.
    ///
    /// Per-file failures are skipped (retried next sweep) rather than
    /// aborting the sweep, matching `sweep_staging` semantics.
    pub fn sweep_orphaned_cas(&mut self) -> usize {
        if !self.dedup_enabled() {
            return 0;
        }
        let root = self.root_dir().to_path_buf();
        let cas_dir = root.join(CAS_DIR_NAME);

        let mut map = load_map(&root);
        // Prune index entries whose object file vanished.
        let mut map_dirty = false;
        map.retain(|name, _| match self.resolve(name) {
            Ok(p) => {
                let exists = p.is_file();
                if !exists {
                    map_dirty = true;
                }
                exists
            }
            Err(_) => {
                map_dirty = true;
                false
            }
        });
        if map_dirty {
            if let Err(e) = save_map(&root, &map) {
                tracing::warn!(error = %e, "cas index prune save failed");
            }
        }

        // Digests still referenced by at least one live name.
        let referenced: std::collections::HashSet<String> =
            map.values().map(|v| v.to_ascii_lowercase()).collect();

        let entries = match fs::read_dir(&cas_dir) {
            Ok(e) => e,
            Err(e) if e.kind() == io::ErrorKind::NotFound => return 0,
            Err(e) => {
                tracing::warn!(error = %e, path = %cas_dir.display(),
                    "cas sweep: cannot read cas dir");
                return 0;
            }
        };

        let mut removed = 0usize;
        for entry in entries.flatten() {
            let path = entry.path();
            if !path.is_file() {
                continue;
            }
            // Only valid digest filenames are CAS objects; anything
            // else under cas/ is left alone.
            let Some(name_os) = path.file_name().and_then(|n| n.to_str()) else {
                continue;
            };
            let Some(addr) = ContentAddress::parse(name_os) else {
                continue;
            };
            if referenced.contains(addr.as_str()) {
                continue;
            }
            // Length before removal — `file_len` is 0 afterwards.
            let len = file_len(&path);
            match fs::remove_file(&path) {
                Ok(()) => {
                    removed += 1;
                    self.track_bytes(len, 0);
                }
                Err(_) => continue,
            }
        }
        removed
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::crates::storage::local::config::StorageConfig;
    use crate::crates::storage::stream::{StreamLimits, StreamUpload, StreamingBackend};
    use sha2::{Digest, Sha256};
    use std::sync::atomic::{AtomicU64, Ordering};

    static NEXT_DIR: AtomicU64 = AtomicU64::new(0);

    fn temp_root() -> PathBuf {
        let dir = std::env::temp_dir().join(format!(
            "kaladoc-dedup-test-{}-{}",
            std::process::id(),
            NEXT_DIR.fetch_add(1, Ordering::Relaxed)
        ));
        std::fs::create_dir_all(&dir).unwrap();
        dir
    }

    fn fs_dedup(root: &Path) -> FaultInjectingFs {
        FaultInjectingFs::new(StorageConfig::new(root).with_dedup(true)).unwrap()
    }

    fn fs_plain(root: &Path) -> FaultInjectingFs {
        FaultInjectingFs::new(StorageConfig::new(root)).unwrap()
    }

    fn sha256_hex(data: &[u8]) -> String {
        let digest = Sha256::digest(data);
        digest.iter().map(|b| format!("{b:02x}")).collect()
    }

    /// Commits `data` at `name` via the streaming API and registers
    /// the receipt with CAS. Returns the receipt.
    fn commit_and_register(fs: &mut FaultInjectingFs, name: &str, data: &[u8]) -> CommitReceipt {
        let mut up = fs.begin_upload(name).unwrap();
        up.write_chunk(data).unwrap();
        let receipt = up.commit().unwrap();
        fs.register_receipt(&receipt).unwrap();
        receipt
    }

    #[test]
    fn content_address_validates_hex() {
        let good = sha256_hex(b"x");
        let addr = ContentAddress::parse(&good).unwrap();
        assert_eq!(addr.as_str(), good);
        assert_eq!(addr.cas_path(), format!("cas/{good}"));

        // Uppercase accepted, normalized to lowercase.
        let upper = good.to_uppercase();
        assert_eq!(ContentAddress::parse(&upper).unwrap().as_str(), good);

        for bad in [
            "",
            "abc",
            &good[..63],
            &format!("{good}0"),
            "zzzzzzzzzzzzzzzzzzzzzzzzzzzzzzzzzzzzzzzzzzzzzzzzzzzzzzzzzzzzzzzz",
            "../etc/passwd",
            "cas/../../escape",
        ] {
            assert!(ContentAddress::parse(bad).is_none(), "accepted {bad:?}");
        }
    }

    #[test]
    fn dedup_disabled_by_default_is_noop() {
        let root = temp_root();
        let mut fs = fs_plain(&root);
        assert!(!fs.dedup_enabled());
        assert_eq!(fs.cas_path_for(&sha256_hex(b"x")), None);

        let mut up = fs.begin_upload("a.bin").unwrap();
        up.write_chunk(b"data").unwrap();
        let receipt = up.commit().unwrap();

        // All dedup APIs are silent no-ops when disabled.
        assert_eq!(fs.register_receipt(&receipt).unwrap(), None);
        assert_eq!(
            fs.register_checksum("a.bin", &receipt.sha256).unwrap(),
            None
        );
        assert_eq!(fs.lookup_name_by_checksum(&receipt.sha256), None);
        assert_eq!(fs.sweep_orphaned_cas(), 0);
        assert!(!root.join(CAS_DIR_NAME).exists());
        assert!(!root.join(CAS_MAP_NAME).exists());
        let _ = std::fs::remove_dir_all(&root);
    }

    #[test]
    fn register_lookup_roundtrip_and_dedup_hit() {
        let root = temp_root();
        let mut fs = fs_dedup(&root);
        assert!(fs.dedup_enabled());

        let data_a = b"identical content".to_vec();
        let data_b = b"different content".to_vec();
        let sha_a = sha256_hex(&data_a);
        let sha_b = sha256_hex(&data_b);

        // cas_path_for returns the storage-relative CAS path.
        assert_eq!(
            fs.cas_path_for(&sha_a).as_deref(),
            Some(format!("cas/{sha_a}").as_str())
        );
        // Malformed digests yield None.
        assert_eq!(fs.cas_path_for("not-hex"), None);

        // Commit WITHOUT registering: `commit_and_register` would
        // already have created `cas/<sha>`, making the explicit
        // registration below a dedup hit instead of a first store.
        let mut up = fs.begin_upload("docs/a.pdf").unwrap();
        up.write_chunk(&data_a).unwrap();
        let r1 = up.commit().unwrap();
        assert_eq!(r1.sha256, sha_a);

        // First registration stores the CAS object (no dedup hit).
        let outcome = fs.register_receipt(&r1).unwrap().unwrap();
        assert!(!outcome.deduplicated);
        assert_eq!(outcome.cas_ref, format!("cas/{sha_a}"));
        assert_eq!(outcome.name, "docs/a.pdf");
        assert!(root.join(CAS_DIR_NAME).join(&sha_a).is_file());
        assert_eq!(fs.read(&outcome.cas_ref).unwrap(), data_a);

        // Second object, same bytes, different name → dedup hit:
        // cas object already exists, only the mapping is added.
        let r2 = commit_and_register(&mut fs, "docs/b-copy.pdf", &data_a);
        assert_eq!(r2.sha256, sha_a);
        let outcome2 = fs
            .register_checksum("docs/b-copy.pdf", &sha_a)
            .unwrap()
            .unwrap();
        assert!(outcome2.deduplicated);

        // Different content gets its own CAS entry.
        commit_and_register(&mut fs, "docs/c.pdf", &data_b);
        assert!(root.join(CAS_DIR_NAME).join(&sha_b).is_file());

        // Lookup by checksum finds a live name.
        let found = fs.lookup_name_by_checksum(&sha_a).unwrap();
        assert!(found == "docs/a.pdf" || found == "docs/b-copy.pdf");
        assert_eq!(fs.lookup_name_by_checksum(&sha256_hex(b"absent")), None);
        assert_eq!(fs.lookup_name_by_checksum("junk"), None);

        // Index file exists and is valid JSON.
        let raw = std::fs::read_to_string(root.join(CAS_MAP_NAME)).unwrap();
        let parsed: serde_json::Value = serde_json::from_str(&raw).unwrap();
        assert_eq!(parsed["docs/a.pdf"], serde_json::json!(sha_a));
        let _ = std::fs::remove_dir_all(&root);
    }

    #[test]
    fn register_rejects_bad_checksum_and_missing_name() {
        let root = temp_root();
        let mut fs = fs_dedup(&root);
        fs.write("real.bin", b"x").unwrap();

        let err = fs.register_checksum("real.bin", "not-a-sha").unwrap_err();
        assert!(matches!(err, StorageError::Io { .. }));

        let err = fs
            .register_checksum("ghost.bin", &sha256_hex(b"x"))
            .unwrap_err();
        assert!(matches!(err, StorageError::NotFound { .. }));
        let _ = std::fs::remove_dir_all(&root);
    }

    #[test]
    fn index_survives_restart_and_corruption_is_tolerated() {
        let root = temp_root();
        let sha = sha256_hex(b"persistent");
        {
            let mut fs = fs_dedup(&root);
            commit_and_register(&mut fs, "keep.bin", b"persistent");
        }
        // New backend instance on the same root sees the index.
        let fs2 = fs_dedup(&root);
        assert_eq!(
            fs2.lookup_name_by_checksum(&sha).as_deref(),
            Some("keep.bin")
        );

        // Corrupt the index: lookups degrade to None, and a fresh
        // registration rewrites a clean map without error.
        std::fs::write(root.join(CAS_MAP_NAME), b"{ not json !!!").unwrap();
        let mut fs3 = fs_dedup(&root);
        assert_eq!(fs3.lookup_name_by_checksum(&sha), None);
        fs3.write("new.bin", b"fresh").unwrap();
        let receipt_sha = sha256_hex(b"fresh");
        fs3.register_checksum("new.bin", &receipt_sha).unwrap();
        // Map now contains only the new entry (corrupt entries lost,
        // which is the documented degrade-to-empty behavior).
        assert_eq!(
            fs3.lookup_name_by_checksum(&receipt_sha).as_deref(),
            Some("new.bin")
        );
        let _ = std::fs::remove_dir_all(&root);
    }

    #[test]
    fn sweep_removes_only_unreferenced_cas_objects() {
        let root = temp_root();
        let mut fs = fs_dedup(&root);

        let sha_keep = sha256_hex(b"keep me");
        let sha_drop = sha256_hex(b"drop me");
        commit_and_register(&mut fs, "keep.bin", b"keep me");
        commit_and_register(&mut fs, "drop.bin", b"drop me");
        // A second name sharing keep.bin's content keeps it referenced.
        fs.write("keep-alias.bin", b"keep me").unwrap();
        fs.register_checksum("keep-alias.bin", &sha_keep).unwrap();

        // Orphan: delete drop.bin's object but leave its index entry —
        // sweep prunes the stale entry AND the now-unreferenced cas.
        fs.delete("drop.bin").unwrap();

        // Manually plant an unreferenced cas file (no index entry).
        let stray = root.join(CAS_DIR_NAME).join(sha256_hex(b"stray"));
        std::fs::create_dir_all(root.join(CAS_DIR_NAME)).unwrap();
        std::fs::write(&stray, b"stray").unwrap();

        let removed = fs.sweep_orphaned_cas();
        assert_eq!(removed, 2); // drop.bin's cas + stray
        assert!(root.join(CAS_DIR_NAME).join(&sha_keep).is_file());
        assert!(!root.join(CAS_DIR_NAME).join(&sha_drop).exists());
        assert!(!stray.exists());

        // Index no longer maps the deleted name; the shared checksum
        // still resolves to one of its live names.
        assert_eq!(fs.lookup_name_by_checksum(&sha_drop), None);
        let found = fs.lookup_name_by_checksum(&sha_keep).unwrap();
        assert!(found == "keep.bin" || found == "keep-alias.bin");

        // Second sweep is a no-op.
        assert_eq!(fs.sweep_orphaned_cas(), 0);
        let _ = std::fs::remove_dir_all(&root);
    }

    #[test]
    fn dedup_works_with_stream_limits_and_quota() {
        let root = temp_root();
        let limits = StreamLimits::new(Some(1 << 20));
        let mut fs = FaultInjectingFs::new(
            StorageConfig::new(&root)
                .with_quota(1 << 20)
                .with_dedup(true),
        )
        .unwrap()
        .with_stream_limits(limits);

        commit_and_register(&mut fs, "q.bin", &[7u8; 500]);
        // cas/<sha> adds another accounted entry under the root.
        let sha = sha256_hex(&[7u8; 500]);
        assert!(root.join(CAS_DIR_NAME).join(&sha).is_file());
        let _ = std::fs::remove_dir_all(&root);
    }
}
