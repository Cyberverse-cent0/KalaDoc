//! Integration, stress, and recovery tests for
//! [`DocumentIngestionService`].
//!
//! These complement the unit tests inside
//! `src/crates/application/services/ingestion.rs` — they exercise the
//! service through its public API only (`kaladoc::crates::...`), cover
//! scenarios the unit tests don't (multi-MiB streamed uploads, wide
//! concurrency, mid-stream cancellation, planted recovery fixtures,
//! and the full `IngestRequest` option surface), and assert the
//! documented crash-safety / recovery semantics end to end.
//!
//! Run with: `cargo test --test ingestion_e2e`

use std::io;
use std::path::{Path, PathBuf};
use std::pin::Pin;
use std::sync::atomic::{AtomicU64, Ordering};
use std::task::{Context, Poll};
use std::time::{Duration, Instant};

use kaladoc::crates::application::services::documents::{DocumentService, NewArtifact};
use kaladoc::crates::application::services::ingestion::{
    DocumentIngestionService, IngestRequest, RecoveryOptions,
};
use kaladoc::crates::document_engine::artifact::ChecksumAlgorithm;
use kaladoc::crates::document_engine::audit::DocumentAuditEventType;
use kaladoc::crates::document_engine::document::{
    DocumentOrigin, DocumentSource, DocumentStatus, DocumentType,
};
use kaladoc::crates::document_engine::store::documents::NewDocument;
use kaladoc::crates::document_engine::store::schema::init_document_store;
use kaladoc::crates::job_engine::create_jobs_table;
use kaladoc::crates::storage::STAGING_DIR_NAME;
use kaladoc::crates::storage::local::config::StorageConfig;
use sha2::{Digest, Sha256};
use sqlx::SqlitePool;
use sqlx::sqlite::{SqliteConnectOptions, SqlitePoolOptions};
use tokio::io::{AsyncRead, AsyncWriteExt, ReadBuf, duplex};

// ---------------------------------------------------------------------------
// Test scaffolding
// ---------------------------------------------------------------------------

static NEXT_DIR: AtomicU64 = AtomicU64::new(0);

/// Unique tempdir per test; removed on drop.
struct TempRoot(PathBuf);

impl TempRoot {
    fn new() -> Self {
        let dir = std::env::temp_dir().join(format!(
            "kaladoc-ingest-e2e-{}-{}",
            std::process::id(),
            NEXT_DIR.fetch_add(1, Ordering::Relaxed)
        ));
        std::fs::create_dir_all(&dir).unwrap();
        Self(dir)
    }

    fn config(&self) -> StorageConfig {
        StorageConfig::new(&self.0)
    }

    fn path(&self) -> &Path {
        &self.0
    }
}

impl Drop for TempRoot {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.0);
    }
}

/// In-memory pool (single connection — `sqlite::memory:` is per
/// connection) with both the document-store and job-engine schemas.
async fn memory_pool() -> SqlitePool {
    let pool = SqlitePoolOptions::new()
        .max_connections(1)
        .connect("sqlite::memory:")
        .await
        .expect("in-memory sqlite pool");
    init_schemas(&pool).await;
    pool
}

/// File-backed pool with several connections — needed when many
/// ingests hit the store concurrently (a single-connection memory DB
/// would serialize everything and hide cross-talk bugs).
async fn file_pool(db_path: &Path, max_connections: u32) -> SqlitePool {
    let options = SqliteConnectOptions::new()
        .filename(db_path)
        .create_if_missing(true)
        .busy_timeout(Duration::from_secs(10));
    let pool = SqlitePoolOptions::new()
        .max_connections(max_connections)
        .connect_with(options)
        .await
        .expect("file-backed sqlite pool");
    init_schemas(&pool).await;
    pool
}

async fn init_schemas(pool: &SqlitePool) {
    init_document_store(pool)
        .await
        .expect("init document store");
    create_jobs_table(pool).await.expect("init jobs table");
}

/// Deterministic pseudo-random payload of `len` bytes. The byte
/// distribution includes NULs and C0 controls, so the sniffer classifies
/// it as `application/octet-stream` regardless of file name.
fn binary_payload(len: usize, seed: u64) -> Vec<u8> {
    (0..len)
        .map(|i| ((i as u64).wrapping_mul(31).wrapping_add(seed * 7) % 251) as u8)
        .collect()
}

fn hex_sha256(data: &[u8]) -> String {
    format!("{:x}", Sha256::digest(data))
}

fn pdf_bytes() -> Vec<u8> {
    b"%PDF-1.7\n1 0 obj\n<<>>\nendobj\ntrailer\n%%EOF\n".to_vec()
}

/// `AsyncRead` that yields at most `chunk` bytes per poll — forces the
/// ingestion feed loop through many small reads regardless of how the
/// runtime batches them.
struct ChunkedReader {
    data: Vec<u8>,
    pos: usize,
    chunk: usize,
}

impl ChunkedReader {
    fn new(data: Vec<u8>, chunk: usize) -> Self {
        Self {
            data,
            pos: 0,
            chunk: chunk.max(1),
        }
    }
}

impl AsyncRead for ChunkedReader {
    fn poll_read(
        mut self: Pin<&mut Self>,
        _cx: &mut Context<'_>,
        buf: &mut ReadBuf<'_>,
    ) -> Poll<io::Result<()>> {
        if self.pos >= self.data.len() {
            return Poll::Ready(Ok(()));
        }
        let n = (self.data.len() - self.pos)
            .min(self.chunk)
            .min(buf.remaining());
        buf.put_slice(&self.data[self.pos..self.pos + n]);
        self.pos += n;
        Poll::Ready(Ok(()))
    }
}

/// Paths of files currently sitting in `<root>/.staging/`.
fn staging_files(root: &TempRoot) -> Vec<PathBuf> {
    match std::fs::read_dir(root.path().join(STAGING_DIR_NAME)) {
        Ok(entries) => entries.flatten().map(|e| e.path()).collect(),
        Err(_) => Vec::new(),
    }
}

/// Polls until `predicate` holds or `timeout` elapses. Used to wait for
/// the `spawn_blocking` upload writer to finish its abort after a
/// cancelled ingest.
async fn wait_until(timeout: Duration, mut predicate: impl FnMut() -> bool) -> bool {
    let deadline = Instant::now() + timeout;
    while Instant::now() < deadline {
        if predicate() {
            return true;
        }
        tokio::time::sleep(Duration::from_millis(25)).await;
    }
    predicate()
}

/// Every regular file under `<root>/documents/`, as `documents/{id}/{name}`.
fn objects_under(root: &TempRoot) -> Vec<String> {
    let mut out = Vec::new();
    let docs = match std::fs::read_dir(root.path().join("documents")) {
        Ok(d) => d,
        Err(_) => return out,
    };
    for doc in docs.flatten() {
        if !doc.path().is_dir() {
            continue;
        }
        let doc_id = doc.file_name().to_string_lossy().into_owned();
        if let Ok(files) = std::fs::read_dir(doc.path()) {
            for f in files.flatten() {
                if f.path().is_file() {
                    out.push(format!(
                        "documents/{doc_id}/{}",
                        f.file_name().to_string_lossy()
                    ));
                }
            }
        }
    }
    out
}

/// Zero-grace recovery options: every grace period collapses so planted
/// fixtures are collected immediately. Still non-destructive toward
/// healthy, referenced objects.
fn zero_grace() -> RecoveryOptions {
    RecoveryOptions {
        staging_max_age: Duration::ZERO,
        incomplete_document_grace: Duration::ZERO,
        unreferenced_object_grace: Duration::ZERO,
    }
}

// ---------------------------------------------------------------------------
// 1. Large-file streaming
// ---------------------------------------------------------------------------

/// ~24 MiB uploaded through a real `tokio::io::duplex` stream (bounded
/// buffer → genuine backpressure between the async reader and the
/// blocking writer). Verifies the CommitReceipt path end to end:
/// `size_bytes`, SHA-256, artifact row, version 1, and the stored bytes.
#[tokio::test]
async fn large_streamed_upload_commits_verified_object() {
    const LEN: usize = 24 * 1024 * 1024 + 12345; // 24 MiB + odd tail

    let root = TempRoot::new();
    let pool = memory_pool().await;
    let svc = DocumentIngestionService::new(pool.clone(), root.config());

    let payload = binary_payload(LEN, 42);
    let expected_sha = hex_sha256(&payload);

    // Real duplex stream: the writer task feeds 128 KiB chunks and the
    // ingest future consumes them under bounded-channel backpressure.
    let (mut writer, reader) = duplex(256 * 1024);
    let writer_task = tokio::spawn(async move {
        for chunk in payload.chunks(128 * 1024) {
            writer.write_all(chunk).await.expect("duplex write");
        }
        // `writer` drops here → EOF on the read half.
    });

    let outcome = svc
        .ingest(reader, IngestRequest::new("user-1", "big.bin"))
        .await
        .expect("24 MiB ingest");
    writer_task.await.expect("writer task");

    assert_eq!(outcome.size_bytes, LEN as u64);
    assert_eq!(outcome.sha256, expected_sha);
    assert_eq!(outcome.document.document_type, DocumentType::Other);
    assert_eq!(outcome.detected_mime_type, "application/octet-stream");
    assert_eq!(outcome.artifact.size_bytes, LEN as u64);
    assert_eq!(
        outcome.artifact.checksum.as_ref().map(|c| &c.value),
        Some(&outcome.sha256)
    );
    assert_eq!(
        outcome.artifact.checksum.as_ref().map(|c| c.algorithm),
        Some(ChecksumAlgorithm::Sha256)
    );
    assert_eq!(outcome.version.version_number, 1);
    assert!(outcome.version.is_current);
    assert_eq!(outcome.version.created_by, "user-1");
    assert_eq!(
        outcome.storage_uri,
        format!("local://documents/{}/big.bin", outcome.document.id)
    );

    // The committed object on disk hashes to the same digest.
    let object = root
        .path()
        .join(format!("documents/{}/big.bin", outcome.document.id));
    let stored = std::fs::read(&object).expect("stored object readable");
    assert_eq!(stored.len(), LEN);
    assert_eq!(hex_sha256(&stored), expected_sha);

    assert!(staging_files(&root).is_empty());

    // And it is retrievable through the document facade.
    let docs = DocumentService::new(pool);
    let listed = docs
        .list_documents(Some("user-1"), None, false, 10)
        .await
        .unwrap();
    assert_eq!(listed.len(), 1);
    assert_eq!(listed[0].id, outcome.document.id);
}

// ---------------------------------------------------------------------------
// 2. Concurrent ingests
// ---------------------------------------------------------------------------

/// 12 parallel ingests of distinct documents share one service and one
/// file-backed pool (8 connections). Asserts every ingest commits its
/// own bytes — no cross-talk between document rows, objects, or
/// checksums — and that `list_documents` sees exactly the expected set.
#[tokio::test]
async fn concurrent_ingests_are_isolated() {
    const N: usize = 12;

    let root = TempRoot::new();
    let pool = file_pool(&root.path().join("docs.db"), 8).await;
    let svc = DocumentIngestionService::new(pool.clone(), root.config());
    let docs = DocumentService::new(pool);

    let mut handles = Vec::with_capacity(N);
    for i in 0..N {
        let svc = svc.clone();
        // Each document gets a distinct ~320 KiB payload and name.
        let payload = binary_payload(320 * 1024 + i, i as u64);
        let expected_sha = hex_sha256(&payload);
        handles.push(tokio::spawn(async move {
            let name = format!("concurrent-{i:02}.bin");
            let outcome = svc
                .ingest(
                    ChunkedReader::new(payload.clone(), 48 * 1024),
                    IngestRequest::new("concurrent-user", name.clone()),
                )
                .await
                .unwrap_or_else(|e| panic!("ingest {i} failed: {e}"));
            (i, name, payload, expected_sha, outcome)
        }));
    }

    let mut seen_ids = std::collections::HashSet::new();
    let mut seen_names = std::collections::HashSet::new();
    for handle in handles {
        let (i, name, payload, expected_sha, outcome) = handle.await.expect("join");
        assert!(seen_ids.insert(outcome.document.id.clone()), "dup doc id");
        assert!(seen_names.insert(name.clone()), "dup file name");
        assert_eq!(outcome.size_bytes, payload.len() as u64, "case {i}");
        assert_eq!(outcome.sha256, expected_sha, "case {i}");
        assert_eq!(outcome.document.name.as_str(), name, "case {i}");
        assert_eq!(
            outcome.storage_uri,
            format!("local://documents/{}/{}", outcome.document.id, name),
            "case {i}"
        );
        // Its own object exists with its own bytes.
        let object = root
            .path()
            .join(format!("documents/{}/{}", outcome.document.id, name));
        assert!(object.is_file(), "case {i}: object missing");
        assert_eq!(hex_sha256(&std::fs::read(&object).unwrap()), expected_sha);
    }

    // Exactly N documents for this owner, all active.
    let listed = docs
        .list_documents(Some("concurrent-user"), None, false, 100)
        .await
        .unwrap();
    assert_eq!(listed.len(), N);
    assert!(
        listed
            .iter()
            .all(|d| d.status == DocumentStatus::Active && d.owner_id == "concurrent-user")
    );
    assert_eq!(seen_ids.len(), N);
    assert!(staging_files(&root).is_empty());
}

// ---------------------------------------------------------------------------
// 3. Cancellation mid-stream
// ---------------------------------------------------------------------------

/// Drops the ingest future after the stream's first chunks. Documented
/// semantics: the staged `.part` file is aborted by the blocking writer,
/// no committed object is published, the artifact-less document row is
/// left for `recover()` to purge once it ages past the grace period.
#[tokio::test]
async fn cancelled_ingest_leaves_no_object_and_recover_purges_row() {
    let root = TempRoot::new();
    let pool = memory_pool().await;
    let svc = DocumentIngestionService::new(pool.clone(), root.config());
    let docs = DocumentService::new(pool);

    // Duplex stream gives us a live, back-pressured reader we can stall.
    let (mut writer, reader) = duplex(64 * 1024);
    let ingest = tokio::spawn({
        let svc = svc.clone();
        async move {
            svc.ingest(reader, IngestRequest::new("user-1", "cancel.bin"))
                .await
        }
    });

    // First chunk: > SNIFF_BYTES (512) with a PDF signature so the
    // document row is created and staging begins.
    let mut first = vec![b'z'; 8 * 1024];
    first[..5].copy_from_slice(b"%PDF-");
    writer.write_all(&first).await.expect("first chunk");

    // Let the pipeline consume the peek + chunks and reach the stalled
    // read; the document row must exist by now.
    let deadline = Instant::now() + Duration::from_secs(5);
    let doc_id = loop {
        let listed = docs
            .list_documents(Some("user-1"), None, true, 10)
            .await
            .unwrap();
        if let Some(d) = listed.into_iter().next() {
            break d.id;
        }
        assert!(Instant::now() < deadline, "document row never appeared");
        tokio::time::sleep(Duration::from_millis(20)).await;
    };

    // Simulate the caller going away mid-stream.
    ingest.abort();
    let _ = ingest.await;
    drop(writer);

    // The blocking writer sees the closed channel and aborts the staged
    // upload — wait for `.staging` to drain.
    assert!(
        wait_until(Duration::from_secs(5), || staging_files(&root).is_empty()).await,
        "staging residue never swept: {:?}",
        staging_files(&root)
    );

    // Documented intermediate state: the row exists, has no artifact,
    // and no committed object was ever published for it.
    let doc = docs.get_document(&doc_id).await.unwrap().expect("row");
    assert_eq!(doc.status, DocumentStatus::Active);
    assert!(docs.list_artifacts(&doc_id).await.unwrap().is_empty());
    assert!(
        objects_under(&root)
            .iter()
            .all(|o| !o.contains(doc_id.as_str())),
        "committed object leaked for cancelled ingest: {:?}",
        objects_under(&root)
    );

    // recover() collects the artifact-less row (zero grace = already
    // stale) and reports it.
    let report = svc.recover_with(&zero_grace()).await.unwrap();
    assert!(report.documents_purged >= 1, "report: {report:?}");
    assert_eq!(docs.get_document(&doc_id).await.unwrap(), None);
    assert!(staging_files(&root).is_empty());
    assert!(objects_under(&root).is_empty());
}

// ---------------------------------------------------------------------------
// 4. recover() against planted inconsistencies
// ---------------------------------------------------------------------------

/// Plants every inconsistency class `recover` documents and verifies
/// both the report counters and the resulting filesystem/DB state —
/// including the two-pass GC rule (objects of documents purged in pass
/// N are only deleted in pass N+1, once they are provably unreferenced).
#[tokio::test]
async fn recover_repairs_stale_staging_missing_objects_and_orphans() {
    let root = TempRoot::new();
    let pool = memory_pool().await;
    let svc = DocumentIngestionService::new(pool.clone(), root.config());
    let docs = DocumentService::new(pool);

    // -- Fixture 1: abandoned staged upload --------------------------
    let staging = root.path().join(STAGING_DIR_NAME);
    std::fs::create_dir_all(&staging).unwrap();
    std::fs::write(staging.join("crashed-upload.part"), b"partial bytes").unwrap();

    // -- Fixture 2: artifact-less document row (failed/cancelled
    //    ingest that never got past row creation) --------------------
    let shell = docs
        .create_document(&NewDocument {
            name: "shell.pdf".into(),
            document_type: DocumentType::Pdf,
            owner_id: "user-1".into(),
            source: DocumentSource::new(DocumentOrigin::Upload),
        })
        .await
        .unwrap();

    // -- Fixture 3: document whose artifact points at a MISSING object,
    //    plus a second artifact whose object DOES exist on disk. The
    //    purge must remove the rows but leave `surplus.bin` for the
    //    *next* GC pass (it was still referenced at snapshot time).
    let broken = docs
        .create_document(&NewDocument {
            name: "broken.pdf".into(),
            document_type: DocumentType::Pdf,
            owner_id: "user-1".into(),
            source: DocumentSource::new(DocumentOrigin::Import),
        })
        .await
        .unwrap();
    docs.upload_artifact(&NewArtifact {
        id: None,
        document_id: broken.id.clone(),
        storage_uri: format!("local://documents/{}/gone.pdf", broken.id),
        file_name: "gone.pdf".into(),
        mime_type: Some("application/pdf".into()),
        size_bytes: 100,
        checksum: None,
    })
    .await
    .unwrap();
    let surplus_name = format!("documents/{}/surplus.bin", broken.id);
    let surplus_path = root.path().join(&surplus_name);
    std::fs::create_dir_all(surplus_path.parent().unwrap()).unwrap();
    std::fs::write(&surplus_path, b"still referenced at snapshot").unwrap();
    docs.upload_artifact(&NewArtifact {
        id: None,
        document_id: broken.id.clone(),
        storage_uri: format!("local://{surplus_name}"),
        file_name: "surplus.bin".into(),
        mime_type: Some("application/octet-stream".into()),
        size_bytes: 27,
        checksum: None,
    })
    .await
    .unwrap();

    // -- Fixture 4: unreferenced object under documents/ -------------
    let orphan_name = "documents/ghost-id/orphan.bin".to_string();
    let orphan_path = root.path().join(&orphan_name);
    std::fs::create_dir_all(orphan_path.parent().unwrap()).unwrap();
    std::fs::write(&orphan_path, b"orphaned bytes").unwrap();

    // -- Fixture 5: a healthy ingested document that must survive ----
    let good = svc
        .ingest(
            ChunkedReader::new(pdf_bytes(), 64),
            IngestRequest::new("user-1", "good.pdf"),
        )
        .await
        .unwrap();
    let good_object = format!("documents/{}/good.pdf", good.document.id);

    // ================= Pass 1 =================
    let report = svc.recover_with(&zero_grace()).await.unwrap();

    assert_eq!(report.staging_entries_removed, 1, "report: {report:?}");
    assert_eq!(report.documents_checked, 3, "report: {report:?}");
    assert_eq!(report.artifacts_checked, 3, "report: {report:?}");
    assert_eq!(report.artifacts_missing_objects, 1, "report: {report:?}");
    assert_eq!(report.documents_purged, 2, "report: {report:?}"); // shell + broken
    assert_eq!(report.unreferenced_objects_removed, 1, "report: {report:?}");
    assert!(
        report.errors.iter().any(|e| e.contains("missing object")),
        "missing-object diagnostic absent: {:?}",
        report.errors
    );

    // Filesystem / DB state after pass 1.
    assert!(!staging.join("crashed-upload.part").exists());
    assert!(!orphan_path.exists(), "orphan object not GC'd");
    assert!(
        docs.get_document(&shell.id).await.unwrap().is_none(),
        "artifact-less stale doc must be purged"
    );
    assert!(
        docs.get_document(&broken.id).await.unwrap().is_none(),
        "doc with missing object must be purged"
    );
    assert!(
        docs.list_artifacts(&broken.id).await.unwrap().is_empty(),
        "purge must cascade artifact rows"
    );
    // Two-pass GC: surplus.bin was referenced when the snapshot was
    // taken, so it survives this pass even though its document is gone.
    assert!(
        surplus_path.exists(),
        "object of just-purged doc must survive until next pass"
    );
    // Healthy document + object untouched.
    assert!(
        docs.get_document(&good.document.id)
            .await
            .unwrap()
            .is_some()
    );
    assert!(root.path().join(&good_object).is_file());

    // ================= Pass 2 =================
    let report2 = svc.recover_with(&zero_grace()).await.unwrap();
    assert_eq!(report2.documents_purged, 0, "report2: {report2:?}");
    assert_eq!(report2.artifacts_missing_objects, 0, "report2: {report2:?}");
    assert_eq!(
        report2.unreferenced_objects_removed, 1,
        "surplus.bin must be collected once unreferenced: {report2:?}"
    );
    assert!(!surplus_path.exists(), "second-pass GC failed");
    assert!(
        docs.get_document(&good.document.id)
            .await
            .unwrap()
            .is_some(),
        "healthy doc must survive repeated recovery"
    );
    assert!(root.path().join(&good_object).is_file());
}

/// Default `recover()` uses hour-long grace periods: a just-created
/// artifact-less document (e.g. an ingest literally in flight) must NOT
/// be collected.
#[tokio::test]
async fn recover_with_default_grace_keeps_fresh_incomplete_documents() {
    let root = TempRoot::new();
    let pool = memory_pool().await;
    let svc = DocumentIngestionService::new(pool.clone(), root.config());
    let docs = DocumentService::new(pool);

    let inflight = docs
        .create_document(&NewDocument {
            name: "inflight.bin".into(),
            document_type: DocumentType::Other,
            owner_id: "user-1".into(),
            source: DocumentSource::new(DocumentOrigin::Upload),
        })
        .await
        .unwrap();

    let report = svc.recover().await.unwrap();
    assert_eq!(report.documents_purged, 0, "report: {report:?}");
    assert!(docs.get_document(&inflight.id).await.unwrap().is_some());
}

// ---------------------------------------------------------------------------
// 5. Format coverage matrix
// ---------------------------------------------------------------------------

/// Every supported magic signature lands on the documented
/// (DocumentType, mime) pair — detection is by content, not extension,
/// proven by a PNG payload named `disguised.pdf`.
#[tokio::test]
async fn format_detection_matrix_covers_all_signatures() {
    let root = TempRoot::new();
    let pool = memory_pool().await;
    let svc = DocumentIngestionService::new(pool, root.config());

    let cases: Vec<(Vec<u8>, &str, DocumentType, &str)> = vec![
        (pdf_bytes(), "doc.pdf", DocumentType::Pdf, "application/pdf"),
        (
            b"\x89PNG\r\n\x1a\n\x00\x00\x00\x0DIHDR....".to_vec(),
            "img.png",
            DocumentType::Image,
            "image/png",
        ),
        (
            vec![0xFF, 0xD8, 0xFF, 0xE0, 0x00, 0x10, b'J', b'F', b'I', b'F'],
            "photo.jpg",
            DocumentType::Image,
            "image/jpeg",
        ),
        (
            b"GIF89a\x01\x00\x01\x00\x80\x00\x00".to_vec(),
            "anim.gif",
            DocumentType::Image,
            "image/gif",
        ),
        (
            b"PK\x03\x04\x14\x00\x00\x00\x08\x00".to_vec(),
            "bundle.zip",
            DocumentType::Archive,
            "application/zip",
        ),
        (
            b"# Release notes\n\nPlain UTF-8 text body.\n".to_vec(),
            "notes.txt",
            DocumentType::Text,
            "text/plain",
        ),
        // Binary sniff: NUL + high bytes → not text → Other/octet-stream.
        (
            vec![0x00, 0x01, 0x02, 0xFE, 0xFF, 0x00, 0x7F],
            "firmware.bin",
            DocumentType::Other,
            "application/octet-stream",
        ),
        // Extension lies: PNG bytes under a .pdf name must still
        // detect as PNG.
        (
            b"\x89PNG\r\n\x1a\nrest-of-png".to_vec(),
            "disguised.pdf",
            DocumentType::Image,
            "image/png",
        ),
    ];

    for (i, (payload, name, expected_type, expected_mime)) in cases.into_iter().enumerate() {
        let outcome = svc
            .ingest(
                ChunkedReader::new(payload.clone(), 32),
                IngestRequest::new("matrix-user", name),
            )
            .await
            .unwrap_or_else(|e| panic!("case {i} ({name}) failed: {e}"));

        assert_eq!(outcome.document.document_type, expected_type, "case {i}");
        assert_eq!(outcome.detected_mime_type, expected_mime, "case {i}");
        // With no declared mime, the artifact records the detected one.
        assert_eq!(
            outcome.artifact.mime_type.as_deref(),
            Some(expected_mime),
            "case {i}"
        );
        assert_eq!(outcome.size_bytes, payload.len() as u64, "case {i}");
        assert_eq!(outcome.version.version_number, 1, "case {i}");
    }
}

// ---------------------------------------------------------------------------
// 6. IngestRequest options
// ---------------------------------------------------------------------------

/// Every `IngestRequest` field lands where documented: folder placement,
/// tags → keywords + tag rows, actor on version + audit, title override,
/// declared-mime override (detected mime still recorded separately).
#[tokio::test]
async fn ingest_request_options_are_applied() {
    let root = TempRoot::new();
    let pool = memory_pool().await;
    let svc = DocumentIngestionService::new(pool.clone(), root.config());
    let docs = DocumentService::new(pool);

    let folder = docs
        .create_folder("Invoices", None, "owner-1")
        .await
        .unwrap();

    let mut req = IngestRequest::new("owner-1", "inv-2026.pdf");
    req.folder_id = Some(folder.id.clone());
    req.tags = vec!["finance".into(), "2026".into(), "  ".into()]; // blank skipped
    req.actor_id = Some("admin-9".into());
    req.title = Some("Invoice #42 — Q3".into());
    req.mime_type = Some("application/x-declared".into());

    let outcome = svc
        .ingest(ChunkedReader::new(pdf_bytes(), 128), req)
        .await
        .unwrap();

    // folder_id placement (document re-read reflects the move).
    assert_eq!(outcome.document.folder_id, Some(folder.id.clone()));
    let in_folder = docs
        .list_documents(None, Some(&folder.id), false, 10)
        .await
        .unwrap();
    assert!(in_folder.iter().any(|d| d.id == outcome.document.id));

    // tags → keywords on metadata + tag assignment rows (blank skipped).
    let meta = docs
        .get_document_metadata(&outcome.document.id)
        .await
        .unwrap()
        .expect("metadata row");
    assert_eq!(meta.title.as_deref(), Some("Invoice #42 — Q3"));
    assert_eq!(
        meta.keywords,
        vec!["finance".to_string(), "2026".to_string()]
    );
    let mut tag_names: Vec<String> = docs
        .list_tags(&outcome.document.id)
        .await
        .unwrap()
        .into_iter()
        .map(|t| t.name)
        .collect();
    tag_names.sort();
    assert_eq!(tag_names, vec!["2026".to_string(), "finance".to_string()]);

    // actor_id on version + Imported audit event.
    assert_eq!(outcome.version.created_by, "admin-9");
    let history = docs
        .get_document_history(&outcome.document.id)
        .await
        .unwrap();
    let imported = history
        .iter()
        .find(|e| e.event_type == DocumentAuditEventType::Imported)
        .expect("Imported audit event");
    assert_eq!(imported.actor_id.as_deref(), Some("admin-9"));

    // mime override: artifact records the DECLARED type; detected mime
    // is still reported on the outcome and in metadata custom fields.
    assert_eq!(
        outcome.artifact.mime_type.as_deref(),
        Some("application/x-declared")
    );
    assert_eq!(outcome.detected_mime_type, "application/pdf");
    assert_eq!(meta.custom_fields["detected_mime_type"], "application/pdf");
    assert_eq!(
        meta.custom_fields["declared_mime_type"],
        "application/x-declared"
    );

    // Defaults: actor falls back to owner_id; title falls back to the
    // file-name stem.
    let plain = svc
        .ingest(
            ChunkedReader::new(pdf_bytes(), 64),
            IngestRequest::new("owner-1", "plain-report.pdf"),
        )
        .await
        .unwrap();
    assert_eq!(plain.version.created_by, "owner-1");
    let plain_meta = docs
        .get_document_metadata(&plain.document.id)
        .await
        .unwrap()
        .unwrap();
    assert_eq!(plain_meta.title.as_deref(), Some("plain-report"));
    assert!(plain.document.folder_id.is_none());
    assert!(plain_meta.keywords.is_empty());
}
