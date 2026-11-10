//! Inbox folder watching — filesystem helpers for the
//! `storage.scan_inbox` job kind.
//!
//! Producers drop files into `<storage_root>/inbox/`; the
//! [`crate::crates::application::job_handlers::StorageScanInboxHandler`]
//! scans that directory, enqueues a `document.ingest` job per file,
//! and moves the file into `inbox/.processed/` so it is not picked up
//! again. This module owns only the *filesystem* side of that
//! protocol — walking, filtering, and the processed-marker move — so
//! it stays synchronous, `std::fs`-only, and trivially testable.
//!
//! # Conventions
//!
//! - The inbox lives directly under the storage root:
//!   `<root>/inbox/`. It is created on first scan when missing.
//! - Processed files move to `<root>/inbox/.processed/` preserving
//!   their path relative to the inbox root (so `inbox/sub/a.pdf`
//!   becomes `inbox/.processed/sub/a.pdf` when `recurse` is on).
//! - **Hidden entries are skipped**: any file or directory whose name
//!   starts with `.` — which also excludes `.processed` itself — is
//!   never scanned or descended into.
//! - **Symlinks are never followed**: entries are classified with
//!   [`std::fs::DirEntry::file_type`], which does not traverse links,
//!   so a symlink (to a file *or* a directory, inside or outside the
//!   storage root) is simply skipped. Combined with the hidden-file
//!   rule there is no way for a scan to escape `<root>/inbox/`.
//! - Only **regular files** are collected; fifos, sockets, devices,
//!   and directories (when `recurse` is off) are ignored.
//!
//! # Processed-marker / at-least-once semantics
//!
//! The `.processed` move is the idempotency marker: a file that was
//! successfully moved will never be scanned again. The handler moves
//! a file **only after** its ingest job has been enqueued, so a crash
//! between enqueue and move can cause the file to be scanned twice —
//! producing a duplicate document + ingest job. That is the
//! documented at-least-once behaviour: the marker guarantees *at
//! most* one move per file, not exactly-once ingestion.

use std::fs;
use std::io;
use std::path::{Path, PathBuf};

/// Directory under the storage root that receives dropped files.
pub const INBOX_DIR_NAME: &str = "inbox";

/// Hidden marker directory inside the inbox holding processed files.
pub const PROCESSED_DIR_NAME: &str = ".processed";

/// Default cap on files handled per scan when the payload omits
/// `max_files`.
pub const DEFAULT_MAX_FILES: usize = 1_000;

/// Upper bound applied to a caller-supplied `max_files` so a bad
/// payload can't turn one scan into an unbounded walk.
pub const MAX_MAX_FILES: usize = 10_000;

/// One regular file discovered under the inbox.
#[derive(Debug, Clone)]
pub struct InboxFile {
    /// Absolute path to the file on disk.
    pub path: PathBuf,
    /// Path relative to the inbox root (`sub/a.pdf` when recursing).
    /// This is the path preserved under `.processed/` on the move.
    pub relative: PathBuf,
    /// Bare file name (lossy UTF-8) for payloads and display.
    pub file_name: String,
    /// Size in bytes at scan time.
    pub size_bytes: u64,
}

/// `<storage_root>/inbox/`.
pub fn inbox_dir(storage_root: &Path) -> PathBuf {
    storage_root.join(INBOX_DIR_NAME)
}

/// `<inbox>/.processed/` — the marker directory for a given inbox.
pub fn processed_dir(inbox_root: &Path) -> PathBuf {
    inbox_root.join(PROCESSED_DIR_NAME)
}

/// Returns true when a directory entry name is hidden (starts with
/// `.`). Hidden entries — including `.processed` — are never scanned
/// or descended into.
fn is_hidden(name: &std::ffi::OsStr) -> bool {
    name.to_str().map(|s| s.starts_with('.')).unwrap_or(false)
}

/// Collects up to `max_files` regular, non-hidden files under
/// `inbox_root`, creating the directory first when missing.
///
/// `recurse` controls whether subdirectories are descended into;
/// either way hidden directories are skipped. `max_files == 0`
/// returns an empty list without touching the filesystem beyond the
/// directory creation. Ordering is filesystem order (not sorted) —
/// callers that need determinism should sort the result.
///
/// Blocking; callers on an async runtime should wrap in
/// `spawn_blocking` (the job handler does).
pub fn scan_inbox(
    inbox_root: &Path,
    recurse: bool,
    max_files: usize,
) -> io::Result<Vec<InboxFile>> {
    fs::create_dir_all(inbox_root)?;
    let mut out = Vec::new();
    if max_files == 0 {
        return Ok(out);
    }
    // Iterative walk so deep trees can't overflow the stack. `dirs`
    // holds directories still to visit; each entry is pushed as
    // (dir path, relative path from inbox_root).
    let mut dirs: Vec<(PathBuf, PathBuf)> = vec![(inbox_root.to_path_buf(), PathBuf::new())];
    while let Some((dir, rel_dir)) = dirs.pop() {
        let entries = match fs::read_dir(&dir) {
            Ok(e) => e,
            // A directory that vanished or can't be read mid-scan is
            // skipped — the next run picks it up.
            Err(_) => continue,
        };
        for entry in entries.flatten() {
            if out.len() >= max_files {
                return Ok(out);
            }
            let name = entry.file_name();
            if is_hidden(&name) {
                continue;
            }
            // file_type() does not follow symlinks: links report as
            // symlink and are skipped below.
            let Ok(ft) = entry.file_type() else {
                continue;
            };
            let rel = rel_dir.join(&name);
            if ft.is_dir() {
                if recurse {
                    dirs.push((entry.path(), rel));
                }
                continue;
            }
            if !ft.is_file() {
                continue;
            }
            let Ok(meta) = entry.metadata() else {
                continue;
            };
            out.push(InboxFile {
                path: entry.path(),
                relative: rel,
                file_name: name.to_string_lossy().into_owned(),
                size_bytes: meta.len(),
            });
        }
    }
    Ok(out)
}

/// Moves `inbox_root/<relative>` to
/// `inbox_root/.processed/<relative>`, creating destination
/// directories as needed. On a name collision a `-N` suffix is added
/// before the extension (`a.pdf` → `a-1.pdf`, `a-2.pdf`, ...; a
/// name without an extension gets `a-1`).
///
/// Returns the destination path. Fails when the source can't be
/// renamed (missing, permissions, cross-device) — callers treat that
/// as a per-file error, leaving the source in place.
pub fn move_to_processed(inbox_root: &Path, relative: &Path) -> io::Result<PathBuf> {
    let source = inbox_root.join(relative);
    let file_name = relative
        .file_name()
        .ok_or_else(|| io::Error::new(io::ErrorKind::InvalidInput, "empty relative path"))?;
    let parent_rel = relative.parent().unwrap_or_else(|| Path::new(""));
    let dest_dir = processed_dir(inbox_root).join(parent_rel);
    fs::create_dir_all(&dest_dir)?;

    let mut dest = dest_dir.join(file_name);
    if !dest_exists(&dest) {
        fs::rename(&source, &dest)?;
        return Ok(dest);
    }
    // Collision: suffix before the extension, then retry.
    let stem = file_name
        .to_str()
        .and_then(|n| n.rsplit_once('.'))
        .map(|(s, _)| s)
        .filter(|s| !s.is_empty())
        .map(str::to_string);
    let ext = file_name
        .to_str()
        .and_then(|n| n.rsplit_once('.'))
        .map(|(_, e)| e.to_string());
    for n in 1u32.. {
        let candidate = match (&stem, &ext) {
            (Some(s), Some(e)) => dest_dir.join(format!("{s}-{n}.{e}")),
            _ => dest_dir.join(format!("{}-{n}", file_name.to_string_lossy())),
        };
        if !dest_exists(&candidate) {
            dest = candidate;
            break;
        }
    }
    fs::rename(&source, &dest)?;
    Ok(dest)
}

/// `try_exists` that treats any error as "occupied" — conservative:
/// never overwrite an existing marker file.
fn dest_exists(path: &Path) -> bool {
    path.try_exists().unwrap_or(true)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::atomic::{AtomicU64, Ordering};

    static NEXT_DIR: AtomicU64 = AtomicU64::new(0);

    fn temp_dir() -> PathBuf {
        let dir = std::env::temp_dir().join(format!(
            "kaladoc-folder-watch-{}-{}",
            std::process::id(),
            NEXT_DIR.fetch_add(1, Ordering::Relaxed)
        ));
        fs::create_dir_all(&dir).unwrap();
        dir
    }

    #[test]
    fn scan_collects_regular_files_and_skips_hidden() {
        let inbox = temp_dir().join("inbox");
        fs::create_dir_all(&inbox).unwrap();
        fs::write(inbox.join("a.pdf"), b"a").unwrap();
        fs::write(inbox.join(".hidden"), b"h").unwrap();
        fs::create_dir_all(inbox.join("sub")).unwrap();
        fs::write(inbox.join("sub/b.txt"), b"b").unwrap();
        // Non-regular file: a fifo would need libc; a symlink is
        // portable enough on unix — skip assertion if unsupported.
        #[cfg(unix)]
        {
            std::os::unix::fs::symlink(inbox.join("a.pdf"), inbox.join("link.pdf")).unwrap();
            std::os::unix::fs::symlink("/etc", inbox.join("dirlink")).unwrap();
        }

        // Non-recursive: only top-level files.
        let files = scan_inbox(&inbox, false, DEFAULT_MAX_FILES).unwrap();
        let names: Vec<_> = files.iter().map(|f| f.file_name.as_str()).collect();
        assert_eq!(names, vec!["a.pdf"]);

        // Recursive: subdir files too, still no hidden/symlink.
        let files = scan_inbox(&inbox, true, DEFAULT_MAX_FILES).unwrap();
        let mut names: Vec<_> = files.iter().map(|f| f.file_name.as_str()).collect();
        names.sort_unstable();
        assert_eq!(names, vec!["a.pdf", "b.txt"]);

        // max_files caps the result.
        let files = scan_inbox(&inbox, true, 1).unwrap();
        assert_eq!(files.len(), 1);

        // Missing inbox is created, not an error.
        let missing = temp_dir().join("nope").join("inbox");
        let files = scan_inbox(&missing, false, DEFAULT_MAX_FILES).unwrap();
        assert!(files.is_empty());
        assert!(missing.is_dir());
    }

    #[test]
    fn move_to_processed_preserves_relative_path_and_dedupes() {
        let inbox = temp_dir().join("inbox");
        fs::create_dir_all(inbox.join("sub")).unwrap();
        fs::write(inbox.join("sub/a.pdf"), b"one").unwrap();

        let dest = move_to_processed(&inbox, Path::new("sub/a.pdf")).unwrap();
        assert_eq!(
            dest,
            inbox.join(PROCESSED_DIR_NAME).join("sub").join("a.pdf")
        );
        assert!(!inbox.join("sub/a.pdf").exists());
        assert_eq!(fs::read(&dest).unwrap(), b"one");

        // Second file with the same relative name collides → suffix.
        fs::write(inbox.join("sub/a.pdf"), b"two").unwrap();
        let dest2 = move_to_processed(&inbox, Path::new("sub/a.pdf")).unwrap();
        assert!(dest2.ends_with("a-1.pdf"));
        assert_eq!(fs::read(&dest2).unwrap(), b"two");
    }
}
