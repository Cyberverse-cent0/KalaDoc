//! End-to-end integration tests for the streaming storage foundation.
//!
//! Exercises the public surface only: `StreamingBackend` /
//! `StreamUpload` on `FaultInjectingFs`, plus the application-layer
//! `StorageService` facade. Covers the staged-upload lifecycle,
//! size/quota enforcement, atomic publish semantics, staging sweeps,
//! path confinement, and concurrent uploads across backend instances.

use std::io::Read;
use std::path::{Path, PathBuf};
use std::process::Command;
use std::time::Duration;

use kaladoc::crates::application::services::storage::StorageService;
use kaladoc::crates::storage::local::config::StorageConfig;
use kaladoc::crates::storage::local::debug_when_full::{FaultInjectingFs, StorageError};
use kaladoc::crates::storage::stream::{
    STAGING_DIR_NAME, StreamLimits, StreamUpload, StreamingBackend,
};
use sha2::{Digest, Sha256};

/// Deterministic pseudo-random payload so digests are reproducible.
fn payload(seed: u8, len: usize) -> Vec<u8> {
    (0..len)
        .map(|i| seed.wrapping_add((i % 251) as u8))
        .collect()
}

fn sha256_hex(data: &[u8]) -> String {
    format!("{:x}", Sha256::digest(data))
}

fn fs_at(root: &Path) -> FaultInjectingFs {
    FaultInjectingFs::new(StorageConfig::new(root)).unwrap()
}

/// Paths of files currently under `<root>/.staging/`.
fn staging_files(root: &Path) -> Vec<PathBuf> {
    let dir = root.join(STAGING_DIR_NAME);
    match std::fs::read_dir(&dir) {
        Ok(entries) => entries.flatten().map(|e| e.path()).collect(),
        Err(_) => Vec::new(),
    }
}

// ---------------------------------------------------------------------
// 1. Staged upload lifecycle
// ---------------------------------------------------------------------

#[test]
fn staged_upload_lifecycle_commit_and_streaming_read() {
    let tmp = tempfile::tempdir().unwrap();
    let root = tmp.path();
    let mut fs = fs_at(root);

    // ~300 KiB payload written in irregular chunks.
    let data = payload(7, 300 * 1024 + 123);
    let mut upload = fs.begin_upload("docs/report.bin").unwrap();
    for (i, chunk) in data.chunks(64 * 1024).enumerate() {
        // Vary chunk boundaries a little.
        let _ = i;
        upload.write_chunk(chunk).unwrap();
    }
    assert_eq!(upload.staged_bytes(), data.len() as u64);

    let receipt = upload.commit().unwrap();
    assert_eq!(receipt.name, "docs/report.bin");
    assert_eq!(receipt.bytes, data.len() as u64);
    assert_eq!(receipt.sha256, sha256_hex(&data));

    // Whole-object read matches.
    assert_eq!(fs.read("docs/report.bin").unwrap(), data);

    // Streaming reader yields the same bytes without buffering.
    let mut reader = fs.open_reader("docs/report.bin").unwrap();
    let mut streamed = Vec::new();
    let mut buf = [0u8; 7777]; // odd-sized reads to exercise partial fills
    loop {
        let n = reader.read(&mut buf).unwrap();
        if n == 0 {
            break;
        }
        streamed.extend_from_slice(&buf[..n]);
    }
    assert_eq!(streamed, data);

    // Commit consumed the staged file.
    assert!(staging_files(root).is_empty());
}

// ---------------------------------------------------------------------
// 2. Size enforcement
// ---------------------------------------------------------------------

#[test]
fn oversized_stream_fails_with_too_large_and_leaves_nothing() {
    let tmp = tempfile::tempdir().unwrap();
    let root = tmp.path();
    let limits = StreamLimits::new(Some(1 << 20)); // 1 MiB object cap
    let mut fs = FaultInjectingFs::new(StorageConfig::new(root))
        .unwrap()
        .with_stream_limits(limits);
    assert_eq!(fs.stream_limits(), limits);

    let mut upload = fs.begin_upload("big.bin").unwrap();
    upload.write_chunk(&[0u8; 600 * 1024]).unwrap();
    upload.write_chunk(&[1u8; 400 * 1024]).unwrap();
    assert_eq!(upload.staged_bytes(), 1_000 * 1024);

    // This chunk would push the object to ~1.53 MiB > 1 MiB.
    let err = upload.write_chunk(&[2u8; 600 * 1024]).unwrap_err();
    assert!(matches!(
        err,
        StorageError::TooLarge { max_bytes, .. } if max_bytes == 1 << 20
    ));
    upload.abort().unwrap();

    // No final object on the backend view or on disk.
    assert!(!fs.exists("big.bin").unwrap());
    assert!(!root.join("big.bin").exists());
    // Staging cleaned after abort.
    assert!(staging_files(root).is_empty());

    // Per-chunk cap is enforced independently of the object cap.
    let limits = StreamLimits::new(None).with_max_chunk_bytes(1024);
    let mut fs = FaultInjectingFs::new(StorageConfig::new(root))
        .unwrap()
        .with_stream_limits(limits);
    let mut upload = fs.begin_upload("chunky.bin").unwrap();
    assert!(matches!(
        upload.write_chunk(&[0u8; 2048]),
        Err(StorageError::TooLarge { .. })
    ));
    upload.abort().unwrap();
}

// ---------------------------------------------------------------------
// 3. Atomicity
// ---------------------------------------------------------------------

#[test]
fn aborted_and_dropped_uploads_never_publish_and_commit_replaces() {
    let tmp = tempfile::tempdir().unwrap();
    let root = tmp.path();
    let mut fs = fs_at(root);

    // v1 committed.
    let mut u1 = fs.begin_upload("obj.bin").unwrap();
    u1.write_chunk(b"version-one").unwrap();
    u1.commit().unwrap();
    assert_eq!(fs.read("obj.bin").unwrap(), b"version-one");

    // An aborted upload to the same name must not disturb v1.
    let mut u2 = fs.begin_upload("obj.bin").unwrap();
    u2.write_chunk(b"aborted-garbage").unwrap();
    u2.abort().unwrap();
    assert_eq!(fs.read("obj.bin").unwrap(), b"version-one");

    // A dropped (never committed/aborted) upload also leaves v1 intact.
    {
        let mut u3 = fs.begin_upload("obj.bin").unwrap();
        u3.write_chunk(b"dropped-garbage").unwrap();
        // dropped here — Drop must clean the staged file
    }
    assert_eq!(fs.read("obj.bin").unwrap(), b"version-one");

    // Commit v2 atomically replaces v1 (last commit wins).
    let v2 = payload(3, 50_000);
    let mut u4 = fs.begin_upload("obj.bin").unwrap();
    u4.write_chunk(&v2).unwrap();
    let receipt = u4.commit().unwrap();
    assert_eq!(receipt.bytes, v2.len() as u64);
    assert_eq!(fs.read("obj.bin").unwrap(), v2);

    // An upload to a never-before-seen name that is dropped leaves no
    // final object at all.
    {
        let mut u5 = fs.begin_upload("never.bin").unwrap();
        u5.write_chunk(b"partial").unwrap();
    }
    assert!(!fs.exists("never.bin").unwrap());
    assert!(!root.join("never.bin").exists());

    assert!(staging_files(root).is_empty());
}

// ---------------------------------------------------------------------
// 4. sweep_staging
// ---------------------------------------------------------------------

#[test]
fn sweep_staging_removes_stale_files_and_tolerates_missing_dir() {
    let tmp = tempfile::tempdir().unwrap();
    let root = tmp.path();
    let mut fs = fs_at(root);

    // Missing staging dir → Ok(0).
    assert!(!root.join(STAGING_DIR_NAME).exists());
    assert_eq!(fs.sweep_staging(Duration::from_secs(3600)).unwrap(), 0);

    // Plant staged leftovers.
    let staging = root.join(STAGING_DIR_NAME);
    std::fs::create_dir_all(&staging).unwrap();
    let stale_a = staging.join("stale-a.part");
    let stale_b = staging.join("stale-b.part");
    std::fs::write(&stale_a, b"aaa").unwrap();
    std::fs::write(&stale_b, b"bbb").unwrap();
    // A nested dir must be ignored by the sweep.
    let subdir = staging.join("nested");
    std::fs::create_dir_all(&subdir).unwrap();
    std::fs::write(subdir.join("inner.part"), b"inner").unwrap();

    // Huge max_age keeps everything.
    assert_eq!(fs.sweep_staging(Duration::from_secs(3600)).unwrap(), 0);
    assert!(stale_a.exists() && stale_b.exists());

    // max_age = 0 removes every regular file (all are "older than 0").
    assert_eq!(fs.sweep_staging(Duration::ZERO).unwrap(), 2);
    assert!(!stale_a.exists() && !stale_b.exists());
    assert!(subdir.join("inner.part").exists());
}

// ---------------------------------------------------------------------
// 5. Quota lifecycle
// ---------------------------------------------------------------------

#[test]
fn staged_bytes_count_toward_quota_and_lifecycle_releases_them() {
    let tmp = tempfile::tempdir().unwrap();
    let root = tmp.path();
    let mut fs = FaultInjectingFs::new(StorageConfig::new(root).with_quota(1000)).unwrap();

    let mut up = fs.begin_upload("q.bin").unwrap();
    up.write_chunk(&[0u8; 400]).unwrap();
    up.write_chunk(&[0u8; 400]).unwrap();

    // Staged bytes are already on disk: a fresh backend instance on the
    // same root scans them into its usage counter.
    let probe = FaultInjectingFs::new(StorageConfig::new(root).with_quota(1000)).unwrap();
    assert_eq!(probe.used_bytes(), 800);

    // 800 staged + 300 more would exceed the 1000-byte quota mid-stream.
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
    // staged 50 replaces the 800-byte q.bin -> net 50.
    let mut up3 = fs.begin_upload("q.bin").unwrap();
    up3.write_chunk(&[2u8; 50]).unwrap();
    up3.commit().unwrap();
    assert_eq!(fs.used_bytes(), 50);
    assert_eq!(fs.read("q.bin").unwrap(), vec![2u8; 50]);
}

// ---------------------------------------------------------------------
// 6. Path traversal
// ---------------------------------------------------------------------

#[test]
fn path_traversal_rejected_for_upload_names_and_reader_names() {
    let tmp = tempfile::tempdir().unwrap();
    let root = tmp.path();
    let mut fs = fs_at(root);

    let unique = format!("kaladoc_e2e_escape_{}.bin", std::process::id());
    let bad_names = [
        "../escape.bin".to_string(),
        "a/../../escape.bin".to_string(),
        "/tmp/abs.bin".to_string(),
        format!("../{unique}"),
    ];

    for bad in &bad_names {
        assert!(
            matches!(
                fs.begin_upload(bad),
                Err(StorageError::PathOutsideRoot { .. })
            ),
            "begin_upload accepted {bad:?}"
        );
        assert!(
            matches!(
                fs.open_reader(bad),
                Err(StorageError::PathOutsideRoot { .. })
            ),
            "open_reader accepted {bad:?}"
        );
        // Whole-object ops share the same confinement.
        assert!(matches!(
            fs.write(bad, b"x"),
            Err(StorageError::PathOutsideRoot { .. })
        ));
    }

    // Nothing escaped the root.
    assert!(!root.parent().unwrap().join(&unique).exists());
    assert!(staging_files(root).is_empty());
}

// ---------------------------------------------------------------------
// 7. Concurrent uploads to distinct names
// ---------------------------------------------------------------------

#[test]
fn concurrent_uploads_to_distinct_names_across_instances() {
    let tmp = tempfile::tempdir().unwrap();
    let root = tmp.path().to_path_buf();

    const THREADS: usize = 4;
    let handles: Vec<_> = (0..THREADS)
        .map(|i| {
            let root = root.clone();
            std::thread::spawn(move || {
                // Each worker owns its backend instance on the shared root.
                let mut fs = fs_at(&root);
                let name = format!("worker-{i}/blob.bin");
                let data = payload(i as u8, 100_000 + i * 10_000);
                let mut upload = fs.begin_upload(&name).unwrap();
                for chunk in data.chunks(8192) {
                    upload.write_chunk(chunk).unwrap();
                }
                let receipt = upload.commit().unwrap();
                assert_eq!(receipt.sha256, sha256_hex(&data));
                // The same instance can read the object back.
                assert_eq!(fs.read(&name).unwrap(), data);
                (name, receipt.bytes)
            })
        })
        .collect();

    for (i, handle) in handles.into_iter().enumerate() {
        let (name, bytes) = handle.join().unwrap();
        assert_eq!(name, format!("worker-{i}/blob.bin"));
        assert_eq!(bytes, (100_000 + i * 10_000) as u64);
    }

    // All objects are present and the staging dir is drained.
    let mut fs = fs_at(&root);
    for i in 0..THREADS {
        assert!(fs.exists(&format!("worker-{i}/blob.bin")).unwrap());
    }
    assert!(staging_files(&root).is_empty());
}

// ---------------------------------------------------------------------
// 8. StorageService level
// ---------------------------------------------------------------------

/// Backdate a file's mtime so it looks older than the service's
/// 24-hour startup sweep threshold. Uses the system `touch` binary;
/// the test environment is Linux so this is available.
fn backdate(path: &Path, days: u32) {
    let status = Command::new("touch")
        .arg("-d")
        .arg(format!("{days} days ago"))
        .arg(path)
        .status()
        .expect("touch -d should run");
    assert!(status.success(), "touch -d failed for {}", path.display());
}

#[test]
fn storage_service_lifecycle_and_startup_staging_sweep() {
    let tmp = tempfile::tempdir().unwrap();
    let root = tmp.path().to_path_buf();

    // Plant staging leftovers BEFORE constructing the service:
    // one stale (>24h) that the startup sweep must reclaim, one fresh
    // that must survive it.
    let staging = root.join(STAGING_DIR_NAME);
    std::fs::create_dir_all(&staging).unwrap();
    let stale = staging.join("crashed-upload.part");
    let fresh = staging.join("in-flight.part");
    std::fs::write(&stale, b"stale").unwrap();
    std::fs::write(&fresh, b"fresh").unwrap();
    backdate(&stale, 2);

    let config = StorageConfig::new(&root).with_quota(1 << 20);
    let mut svc = StorageService::from_config(&config).unwrap();

    // Startup sweep ran: stale file gone, fresh file kept.
    assert!(!stale.exists());
    assert!(fresh.exists());

    // status() reports the configured backend/quota without paths.
    let status = svc.status();
    assert_eq!(status.backend, "local");
    assert!(status.root_configured);
    assert_eq!(status.quota_bytes, Some(1 << 20));
    let json = serde_json::to_string(&status).unwrap();
    assert!(!json.contains(root.to_str().unwrap()));

    // begin_upload -> write_chunk -> commit -> open_reader roundtrip.
    let data = payload(9, 150_000);
    let receipt = {
        let mut upload = svc.begin_upload("svc/object.bin").unwrap();
        for chunk in data.chunks(30_000) {
            upload.write_chunk(chunk).unwrap();
        }
        upload.commit().unwrap()
    };
    assert_eq!(receipt.name, "svc/object.bin");
    assert_eq!(receipt.bytes, data.len() as u64);
    assert_eq!(receipt.sha256, sha256_hex(&data));

    let mut reader = svc.open_reader("svc/object.bin").unwrap();
    let mut streamed = Vec::new();
    reader.read_to_end(&mut streamed).unwrap();
    assert_eq!(streamed, data);
    assert_eq!(svc.get("svc/object.bin").unwrap(), data);

    // Manual sweep with max_age=0 reclaims the remaining fresh leftover.
    assert_eq!(svc.sweep_staging(Duration::ZERO).unwrap(), 1);
    assert!(!fresh.exists());

    // Aborted service-level upload publishes nothing.
    {
        let mut upload = svc.begin_upload("svc/aborted.bin").unwrap();
        upload.write_chunk(b"partial").unwrap();
        upload.abort().unwrap();
    }
    assert!(!svc.exists("svc/aborted.bin").unwrap());
}
