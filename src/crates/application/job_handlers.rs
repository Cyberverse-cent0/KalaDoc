//! Built-in job handlers registered on the engine at startup.
//!
//! Long-running entry points (e.g. `kaladoc jobs worker`) call
//! [`register_builtin`] before starting the run loop so every known
//! job `kind` has a handler. Add new handlers here as they come
//! online — job kinds are the contract between producers and
//! consumers.
//!
//! # Registered kinds
//!
//! | Kind                    | Payload contract                                              | Purpose |
//! |-------------------------|---------------------------------------------------------------|---------|
//! | `system.ping`           | any (ignored)                                                 | Health check / queue smoke test; always succeeds |
//! | `document.ingest`       | `{document_id, content_b64` or `content`, `file_name?`, `mime_type?}` | Write payload bytes into storage + register artifact (see [`DocumentIngestHandler`]) |
//! | `pdf.inspect`           | `{path, max_bytes?}`                                          | Validate + inspect a PDF under the storage root; writes `<path>.inspect.json` |
//! | `storage.gc_staging`    | `{retention_secs?}`                                           | Delete abandoned entries under `<storage_root>/staging/` older than the retention window |
//!
//! `pdf.inspect` payload details: `path` is a **relative** path under
//! the configured storage root (`[storage].root_dir` /
//! `KALADOC_STORAGE_DIR`); absolute paths and `..` components are
//! rejected. `max_bytes` defaults to 100 MiB and bounds both the
//! on-disk size check (before reading) and `security_engine`'s
//! validation. The inspection report (`PdfInfo` as pretty JSON) is
//! written atomically to `<root>/<path>.inspect.json` via a `*.tmp`
//! sibling + rename.
//!
//! `storage.gc_staging` payload details: `retention_secs` defaults to
//! 86 400 (24 h). Every entry directly under `staging/` whose
//! `modified()` time is older than the window is removed —
//! directories via `remove_dir_all`, everything else via
//! `remove_file`. Entries whose metadata cannot be read are skipped;
//! a missing `staging/` directory is a no-op success.

use std::path::{Component, Path, PathBuf};
use std::sync::Arc;
use std::time::{Duration, SystemTime};

use base64::{Engine as _, engine::general_purpose::STANDARD as B64};
use serde::Deserialize;
use serde_json::json;
use sha2::{Digest, Sha256};

use crate::config::AppConfig;
use crate::crates::document_engine::artifact::{ChecksumAlgorithm, DocumentChecksum};
use crate::crates::document_engine::audit::DocumentAuditEventType;
use crate::crates::document_engine::ids::DocumentId;
use crate::crates::document_engine::store::artifacts::{
    NewArtifact, list_artifacts, upload_artifact,
};
use crate::crates::document_engine::store::documents::get_document;
use crate::crates::document_engine::store::history::record_event;
use crate::crates::job_engine::{Job, JobContext, JobEngine, JobError, JobHandler};
use crate::crates::storage::local::config::StorageConfig;
use crate::crates::storage::local::debug_when_full::FaultInjectingFs;

/// Job kind for [`DocumentIngestHandler`].
pub const DOCUMENT_INGEST_KIND: &str = "document.ingest";

/// Job kind for [`SystemPingHandler`] — health check / smoke test.
pub const SYSTEM_PING_KIND: &str = "system.ping";

/// Job kind for [`PdfInspectHandler`].
pub const PDF_INSPECT_KIND: &str = "pdf.inspect";

/// Job kind for [`StorageGcStagingHandler`].
pub const STORAGE_GC_STAGING_KIND: &str = "storage.gc_staging";

/// Default `max_bytes` for `pdf.inspect` (100 MiB).
const DEFAULT_INSPECT_MAX_BYTES: u64 = 100 * 1024 * 1024;

/// Default `retention_secs` for `storage.gc_staging` (24 h).
const DEFAULT_GC_RETENTION_SECS: u64 = 86_400;

/// Registers all built-in job handlers on `engine`.
///
/// Handlers that need application config are constructed here; a
/// config that fails to load logs a warning and skips that handler so
/// the engine can still start (its jobs will fail with `NoHandler`).
/// Config-dependent handlers (`document.ingest`) resolve storage at
/// construction; `pdf.inspect` / `storage.gc_staging` resolve it per
/// job so a fixed storage root can be picked up without a restart.
pub fn register_builtin(engine: &mut JobEngine) {
    match DocumentIngestHandler::from_app_config() {
        Ok(handler) => engine.register(DOCUMENT_INGEST_KIND, Arc::new(handler)),
        Err(e) => tracing::warn!(
            error = %e,
            "skipping document.ingest handler: storage config unavailable"
        ),
    }
    engine.register(SYSTEM_PING_KIND, Arc::new(SystemPingHandler));
    engine.register(PDF_INSPECT_KIND, Arc::new(PdfInspectHandler));
    engine.register(STORAGE_GC_STAGING_KIND, Arc::new(StorageGcStagingHandler));
}

/// `document.ingest` — writes job payload bytes into the configured
/// storage backend and registers the result as a document artifact.
///
/// Payload contract (`jobs_tbl.payload` JSON):
///
/// | Field          | Required | Meaning                                            |
/// |----------------|----------|----------------------------------------------------|
/// | `document_id`  | yes      | Existing document the artifact belongs to          |
/// | `content_b64`  | one of   | Base64-encoded file bytes                          |
/// | `content`      | one of   | UTF-8 text stored as the file bytes                |
/// | `file_name`    | no       | Stored name, default `"upload.bin"`                |
/// | `mime_type`    | no       | Recorded on the artifact row                       |
///
/// Bytes land at `documents/{document_id}/{file_name}` under the
/// storage root; `storage_uri` on the artifact row is
/// `local://documents/{document_id}/{file_name}`.
///
/// Idempotent: the storage object name is deterministic (re-writing
/// replaces content) and an artifact row with the same `storage_uri`
/// is not duplicated on retry.
pub struct DocumentIngestHandler {
    storage: StorageConfig,
}

impl DocumentIngestHandler {
    /// Builds the handler from the application's storage configuration
    /// (`AppConfig::load_lenient`, so missing DB secrets don't matter —
    /// only the `[storage]` section and `KALADOC_STORAGE_*` env vars).
    pub fn from_app_config() -> Result<Self, crate::config::ConfigError> {
        Ok(Self::with_storage_config(
            AppConfig::load_lenient()?.to_storage_config(),
        ))
    }

    /// Builds the handler around an explicit storage config (tests).
    pub fn with_storage_config(storage: StorageConfig) -> Self {
        Self { storage }
    }
}

impl JobHandler for DocumentIngestHandler {
    fn handle<'a>(
        &'a self,
        ctx: &'a JobContext,
        job: &'a Job,
    ) -> std::pin::Pin<Box<dyn std::future::Future<Output = Result<(), JobError>> + Send + 'a>>
    {
        Box::pin(async move {
            let payload = IngestPayload::parse(&job.payload)?;
            let document_id = DocumentId::new(payload.document_id.clone());

            // The document must already exist — ingest attaches bytes
            // to a registered document rather than creating rows
            // implicitly (document creation is a separate concern).
            get_document(&ctx.pool, &document_id)
                .await
                .map_err(store_err)?
                .ok_or_else(|| JobError::Failed(format!("document not found: {}", document_id)))?;

            // Deterministic object name → retries rewrite the same
            // bytes instead of accumulating orphans.
            let object_name = format!("documents/{}/{}", document_id, payload.file_name);
            let storage_uri = format!("local://{object_name}");

            // Skip the store write entirely when a previous attempt
            // already completed: the artifact row below is the source
            // of truth for "ingested".
            let already_ingested = list_artifacts(&ctx.pool, &document_id)
                .await
                .map_err(store_err)?
                .iter()
                .any(|a| a.storage_uri == storage_uri);

            if !already_ingested {
                let mut fs = FaultInjectingFs::new(self.storage.clone())
                    .map_err(|e| JobError::Failed(format!("storage init failed: {e}")))?;
                fs.write(&object_name, &payload.bytes)
                    .map_err(|e| JobError::Failed(format!("storage write failed: {e}")))?;

                let checksum = DocumentChecksum {
                    algorithm: ChecksumAlgorithm::Sha256,
                    value: hex_lower(&Sha256::digest(&payload.bytes)),
                };
                let artifact = upload_artifact(
                    &ctx.pool,
                    &NewArtifact {
                        id: None,
                        document_id: document_id.clone(),
                        storage_uri: storage_uri.clone(),
                        file_name: payload.file_name.clone(),
                        mime_type: payload.mime_type.clone(),
                        size_bytes: payload.bytes.len() as u64,
                        checksum: Some(checksum),
                    },
                )
                .await
                .map_err(store_err)?;

                record_event(
                    &ctx.pool,
                    &document_id,
                    None,
                    DocumentAuditEventType::Imported,
                    Some(&json!({
                        "artifact_id": artifact.id,
                        "storage_uri": storage_uri,
                        "size_bytes": artifact.size_bytes,
                        "job_id": job.id,
                    })),
                )
                .await
                .map_err(store_err)?;
            }

            Ok(())
        })
    }
}

/// Parsed and validated `document.ingest` payload.
struct IngestPayload {
    document_id: String,
    file_name: String,
    mime_type: Option<String>,
    bytes: Vec<u8>,
}

impl IngestPayload {
    fn parse(payload: &serde_json::Value) -> Result<Self, JobError> {
        let document_id = payload["document_id"]
            .as_str()
            .filter(|s| !s.is_empty())
            .ok_or_else(|| {
                JobError::Failed("payload.document_id must be a non-empty string".into())
            })?
            .to_string();

        let bytes = match (payload["content_b64"].as_str(), payload["content"].as_str()) {
            (Some(b64), None) => B64.decode(b64.trim()).map_err(|e| {
                JobError::Failed(format!("payload.content_b64 is not valid base64: {e}"))
            })?,
            (None, Some(text)) => text.as_bytes().to_vec(),
            _ => {
                return Err(JobError::Failed(
                    "payload must contain exactly one of 'content_b64' or 'content'".into(),
                ));
            }
        };

        let file_name = match payload["file_name"].as_str() {
            Some(name) => sanitize_file_name(name)?,
            None => "upload.bin".to_string(),
        };

        let mime_type = payload["mime_type"]
            .as_str()
            .map(str::trim)
            .filter(|s| !s.is_empty())
            .map(str::to_string);

        Ok(Self {
            document_id,
            file_name,
            mime_type,
            bytes,
        })
    }
}

/// Rejects names that could escape the `documents/{id}/` prefix or
/// break path confinement inside the storage backend.
fn sanitize_file_name(name: &str) -> Result<String, JobError> {
    let trimmed = name.trim();
    let bad = trimmed.is_empty()
        || trimmed == "."
        || trimmed == ".."
        || trimmed.contains('/')
        || trimmed.contains('\\')
        || trimmed.chars().any(|c| c.is_control());
    if bad {
        return Err(JobError::Failed(format!(
            "payload.file_name is not a safe file name: {name:?}"
        )));
    }
    Ok(trimmed.to_string())
}

/// Lowercase hex encoding without pulling in a `hex` crate.
fn hex_lower(bytes: &[u8]) -> String {
    const HEX: &[u8; 16] = b"0123456789abcdef";
    let mut out = String::with_capacity(bytes.len() * 2);
    for b in bytes {
        out.push(HEX[(b >> 4) as usize] as char);
        out.push(HEX[(b & 0x0f) as usize] as char);
    }
    out
}

/// Store errors become retryable job failures with context attached.
fn store_err(e: crate::crates::document_engine::store::StoreError) -> JobError {
    JobError::Failed(format!("document store error: {e}"))
}

// ---------------------------------------------------------------------------
// system.ping
// ---------------------------------------------------------------------------

/// `system.ping` — trivial handler that always succeeds.
///
/// Exists so operators and tests can verify the queue end-to-end
/// (enqueue → claim → dispatch → `succeeded`) without touching real
/// storage or document state.
struct SystemPingHandler;

impl JobHandler for SystemPingHandler {
    fn handle<'a>(
        &'a self,
        _ctx: &'a JobContext,
        _job: &'a Job,
    ) -> std::pin::Pin<Box<dyn std::future::Future<Output = Result<(), JobError>> + Send + 'a>>
    {
        Box::pin(async move { Ok(()) })
    }
}

// ---------------------------------------------------------------------------
// pdf.inspect
// ---------------------------------------------------------------------------

/// `pdf.inspect` — validates a PDF stored under the storage root and
/// writes the inspection report next to it.
///
/// See the module docs for the payload contract. All failures are
/// `JobError::Failed` so the engine's retry policy applies uniformly.
struct PdfInspectHandler;

/// Parsed `pdf.inspect` payload.
#[derive(Debug, Deserialize)]
struct InspectPayload {
    /// Path relative to the storage root. Must not be absolute and
    /// must not contain `..` components.
    path: String,
    /// Size cap applied before reading (bytes). Defaults to 100 MiB.
    #[serde(default = "default_inspect_max_bytes")]
    max_bytes: u64,
}

fn default_inspect_max_bytes() -> u64 {
    DEFAULT_INSPECT_MAX_BYTES
}

/// Parsed `storage.gc_staging` payload.
#[derive(Debug, Deserialize)]
struct GcStagingPayload {
    /// Entries older than this many seconds are deleted. Defaults to
    /// 24 h.
    #[serde(default = "default_gc_retention_secs")]
    retention_secs: u64,
}

fn default_gc_retention_secs() -> u64 {
    DEFAULT_GC_RETENTION_SECS
}

impl JobHandler for PdfInspectHandler {
    fn handle<'a>(
        &'a self,
        _ctx: &'a JobContext,
        job: &'a Job,
    ) -> std::pin::Pin<Box<dyn std::future::Future<Output = Result<(), JobError>> + Send + 'a>>
    {
        Box::pin(async move {
            let payload: InspectPayload = serde_json::from_value(job.payload.clone())?;
            let root = storage_root()?;
            inspect_pdf_at(&root, &payload.path, payload.max_bytes)
        })
    }
}

/// Resolves the configured storage root for handlers that need it.
fn storage_root() -> Result<PathBuf, JobError> {
    AppConfig::load_lenient()
        .map(|cfg| cfg.to_storage_config().root_dir)
        .map_err(|e| JobError::Failed(format!("cannot load storage config: {e}")))
}

/// Confines `rel_path` to `root`: rejects empty, absolute, and any
/// path containing `..` (or other non-normal components such as a
/// Windows prefix). Returns the joined path on success.
fn resolve_under_root(root: &Path, rel_path: &str) -> Result<PathBuf, JobError> {
    let rel = Path::new(rel_path);
    let confined = !rel_path.trim().is_empty()
        && !rel.is_absolute()
        && rel
            .components()
            .all(|c| matches!(c, Component::Normal(_) | Component::CurDir));
    if !confined {
        return Err(JobError::Failed(format!(
            "payload.path must be a relative path inside the storage root: {rel_path:?}"
        )));
    }
    Ok(root.join(rel))
}

/// Validates `rel_path`'s file under `root`, inspects it, and writes
/// `<root>/<rel_path>.inspect.json` atomically (tmp + rename).
fn inspect_pdf_at(root: &Path, rel_path: &str, max_bytes: u64) -> Result<(), JobError> {
    let file = resolve_under_root(root, rel_path)?;

    // Bound memory before reading: reject on metadata size first, then
    // re-check the actual bytes read (TOCTOU guard).
    let meta = std::fs::metadata(&file)
        .map_err(|e| JobError::Failed(format!("cannot stat {}: {e}", file.display())))?;
    if !meta.is_file() {
        return Err(JobError::Failed(format!(
            "payload.path is not a regular file: {rel_path:?}"
        )));
    }
    if meta.len() > max_bytes {
        return Err(JobError::Failed(format!(
            "file {} is {} bytes, exceeding max_bytes {max_bytes}",
            rel_path,
            meta.len()
        )));
    }
    let data = std::fs::read(&file)
        .map_err(|e| JobError::Failed(format!("cannot read {}: {e}", file.display())))?;
    if data.len() as u64 > max_bytes {
        return Err(JobError::Failed(format!(
            "file {} grew past max_bytes {max_bytes} while reading",
            rel_path
        )));
    }

    crate::crates::security_engine::validate_pdf(&data, max_bytes)
        .map_err(|e| JobError::Failed(format!("pdf validation failed for {rel_path:?}: {e}")))?;

    let info = crate::crates::pdf_engine::PdfEngine::new()
        .inspect(&data)
        .map_err(|e| JobError::Failed(format!("pdf inspect failed for {rel_path:?}: {e}")))?;

    // Report lands at `<root>/<path>.inspect.json` — a sibling of the
    // input, never overwriting it.
    let mut out_os = file.clone().into_os_string();
    out_os.push(".inspect.json");
    let out = PathBuf::from(out_os);
    let json = serde_json::to_string_pretty(&info)?;

    // Write-then-rename so a crash mid-write can't leave a truncated
    // report that looks authoritative.
    let mut tmp_os = out.clone().into_os_string();
    tmp_os.push(".tmp");
    let tmp = PathBuf::from(tmp_os);
    if let Some(parent) = tmp.parent() {
        std::fs::create_dir_all(parent)
            .map_err(|e| JobError::Failed(format!("cannot create {}: {e}", parent.display())))?;
    }
    std::fs::write(&tmp, json.as_bytes())
        .map_err(|e| JobError::Failed(format!("cannot write {}: {e}", tmp.display())))?;
    std::fs::rename(&tmp, &out).map_err(|e| {
        JobError::Failed(format!(
            "cannot rename {} -> {}: {e}",
            tmp.display(),
            out.display()
        ))
    })?;
    Ok(())
}

// ---------------------------------------------------------------------------
// storage.gc_staging
// ---------------------------------------------------------------------------

/// `storage.gc_staging` — removes abandoned staging objects.
///
/// Implements the crash-recovery requirement: uploads that were staged
/// but never committed (e.g. the app died mid-operation) are deleted
/// once they are older than `retention_secs`. See module docs for the
/// payload contract.
struct StorageGcStagingHandler;

impl JobHandler for StorageGcStagingHandler {
    fn handle<'a>(
        &'a self,
        _ctx: &'a JobContext,
        job: &'a Job,
    ) -> std::pin::Pin<Box<dyn std::future::Future<Output = Result<(), JobError>> + Send + 'a>>
    {
        Box::pin(async move {
            let payload: GcStagingPayload = serde_json::from_value(job.payload.clone())?;
            let root = storage_root()?;
            let removed = gc_staging_dir(
                &root.join("staging"),
                Duration::from_secs(payload.retention_secs),
            );
            if removed > 0 {
                tracing::info!(removed, "storage.gc_staging deleted abandoned entries");
            }
            Ok(())
        })
    }
}

/// Deletes entries directly under `staging_dir` whose `modified()`
/// time is older than `retention`. Files and directories are both
/// removed; entries whose metadata can't be read are skipped, as are
/// individual delete failures (logged, not fatal — GC is best-effort
/// and the job stays idempotent). A missing `staging_dir` is a no-op.
/// Returns the number of entries removed.
fn gc_staging_dir(staging_dir: &Path, retention: Duration) -> u64 {
    let Ok(entries) = std::fs::read_dir(staging_dir) else {
        return 0;
    };
    let cutoff = SystemTime::now()
        .checked_sub(retention)
        .unwrap_or(SystemTime::UNIX_EPOCH);
    let mut removed = 0u64;
    for entry in entries.flatten() {
        let path = entry.path();
        let Ok(meta) = entry.metadata() else {
            continue;
        };
        let Ok(modified) = meta.modified() else {
            continue;
        };
        if modified > cutoff {
            continue; // still within the retention window
        }
        let result = if meta.is_dir() {
            std::fs::remove_dir_all(&path)
        } else {
            std::fs::remove_file(&path)
        };
        match result {
            Ok(()) => {
                removed += 1;
                tracing::info!(path = %path.display(), "storage.gc_staging removed abandoned entry");
            }
            Err(e) => {
                tracing::warn!(path = %path.display(), error = %e, "storage.gc_staging delete failed");
            }
        }
    }
    removed
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::crates::document_engine::document::{DocumentOrigin, DocumentSource, DocumentType};
    use crate::crates::document_engine::store::artifacts::list_artifacts;
    use crate::crates::document_engine::store::documents::{NewDocument, create_document};
    use crate::crates::document_engine::store::history::get_document_history;
    use crate::crates::document_engine::store::schema::init_document_store;
    use crate::crates::job_engine::create_jobs_table;
    use crate::crates::job_engine::{JobEngineConfig, JobStatus, NewJob, list_jobs};
    use base64::engine::general_purpose::STANDARD as B64;
    use sqlx::SqlitePool;
    use sqlx::sqlite::SqlitePoolOptions;
    use std::path::PathBuf;
    use std::sync::atomic::{AtomicU64, Ordering};
    use std::time::Duration;

    static NEXT_DIR: AtomicU64 = AtomicU64::new(0);

    async fn test_pool() -> SqlitePool {
        let pool = SqlitePoolOptions::new()
            .max_connections(1)
            .connect("sqlite::memory:")
            .await
            .unwrap();
        create_jobs_table(&pool).await.unwrap();
        init_document_store(&pool).await.unwrap();
        pool
    }

    fn temp_root() -> PathBuf {
        let dir = std::env::temp_dir().join(format!(
            "kaladoc-ingest-test-{}-{}",
            std::process::id(),
            NEXT_DIR.fetch_add(1, Ordering::Relaxed)
        ));
        std::fs::create_dir_all(&dir).unwrap();
        dir
    }

    fn engine(pool: SqlitePool, root: &std::path::Path) -> JobEngine {
        let mut eng = JobEngine::new(
            pool,
            JobEngineConfig {
                poll_interval: Duration::from_millis(10),
                default_max_attempts: 3,
                backoff_base: Duration::from_secs(0),
                stale_after: Duration::from_secs(60),
            },
        );
        eng.register(
            DOCUMENT_INGEST_KIND,
            Arc::new(DocumentIngestHandler::with_storage_config(
                StorageConfig::new(root),
            )),
        );
        eng
    }

    async fn make_document(pool: &SqlitePool) -> DocumentId {
        let doc = create_document(
            pool,
            &NewDocument {
                name: "report".into(),
                document_type: DocumentType::Pdf,
                owner_id: "user-1".into(),
                source: DocumentSource::new(DocumentOrigin::Upload),
            },
        )
        .await
        .unwrap();
        doc.id
    }

    fn ingest_payload(document_id: &DocumentId, bytes: &[u8]) -> serde_json::Value {
        serde_json::json!({
            "document_id": document_id.as_str(),
            "file_name": "report.pdf",
            "mime_type": "application/pdf",
            "content_b64": B64.encode(bytes),
        })
    }

    #[tokio::test]
    async fn ingest_writes_bytes_and_artifact() {
        let pool = test_pool().await;
        let root = temp_root();
        let doc_id = make_document(&pool).await;
        let eng = engine(pool.clone(), &root);

        let bytes = b"%PDF-1.4 fake pdf bytes";
        eng.enqueue(&NewJob {
            kind: DOCUMENT_INGEST_KIND.into(),
            payload: ingest_payload(&doc_id, bytes),
            priority: 0,
            run_at: None,
            max_attempts: None,
        })
        .await
        .unwrap();
        assert!(eng.run_once().await.unwrap());

        // Bytes landed under the deterministic object name.
        let on_disk = root
            .join("documents")
            .join(doc_id.as_str())
            .join("report.pdf");
        assert_eq!(std::fs::read(&on_disk).unwrap(), bytes);

        // Artifact row recorded with checksum + storage_uri.
        let arts = list_artifacts(&pool, &doc_id).await.unwrap();
        assert_eq!(arts.len(), 1);
        assert_eq!(
            arts[0].storage_uri,
            format!("local://documents/{}/report.pdf", doc_id)
        );
        assert_eq!(arts[0].size_bytes, bytes.len() as u64);
        let checksum = arts[0].checksum.as_ref().unwrap();
        assert_eq!(checksum.algorithm, ChecksumAlgorithm::Sha256);
        assert_eq!(checksum.value.len(), 64);

        // Audit trail got an `imported` event.
        let history = get_document_history(&pool, &doc_id).await.unwrap();
        assert!(
            history
                .iter()
                .any(|e| e.event_type == DocumentAuditEventType::Imported)
        );

        // Job marked succeeded.
        let jobs = list_jobs(&pool, Some(JobStatus::Succeeded), 10)
            .await
            .unwrap();
        assert_eq!(jobs.len(), 1);
        std::fs::remove_dir_all(&root).ok();
    }

    #[tokio::test]
    async fn ingest_is_idempotent_across_retries() {
        let pool = test_pool().await;
        let root = temp_root();
        let doc_id = make_document(&pool).await;
        let eng = engine(pool.clone(), &root);
        let payload = ingest_payload(&doc_id, b"same bytes");

        // Two jobs with identical payloads — e.g. a producer retry —
        // must not create duplicate artifact rows.
        for _ in 0..2 {
            eng.enqueue(&NewJob {
                kind: DOCUMENT_INGEST_KIND.into(),
                payload: payload.clone(),
                priority: 0,
                run_at: None,
                max_attempts: None,
            })
            .await
            .unwrap();
            assert!(eng.run_once().await.unwrap());
        }
        assert_eq!(list_artifacts(&pool, &doc_id).await.unwrap().len(), 1);
        std::fs::remove_dir_all(&root).ok();
    }

    #[tokio::test]
    async fn ingest_rejects_missing_document() {
        let pool = test_pool().await;
        let root = temp_root();
        let eng = engine(pool.clone(), &root);
        let ghost = DocumentId::new("doc-does-not-exist");

        eng.enqueue(&NewJob {
            kind: DOCUMENT_INGEST_KIND.into(),
            payload: ingest_payload(&ghost, b"x"),
            priority: 0,
            run_at: None,
            max_attempts: Some(1),
        })
        .await
        .unwrap();
        assert!(eng.run_once().await.unwrap());

        // max_attempts=1 → straight to failed with the not-found message.
        let jobs = list_jobs(&pool, Some(JobStatus::Failed), 10).await.unwrap();
        assert_eq!(jobs.len(), 1);
        assert!(
            jobs[0]
                .last_error
                .as_deref()
                .unwrap_or("")
                .contains("document not found")
        );
        std::fs::remove_dir_all(&root).ok();
    }

    #[tokio::test]
    async fn ingest_rejects_bad_payload() {
        let pool = test_pool().await;
        let root = temp_root();
        let eng = engine(pool.clone(), &root);

        // Neither content field present.
        eng.enqueue(&NewJob {
            kind: DOCUMENT_INGEST_KIND.into(),
            payload: serde_json::json!({ "document_id": "doc-x" }),
            priority: 0,
            run_at: None,
            max_attempts: Some(1),
        })
        .await
        .unwrap();
        // Unsafe file name.
        let doc_id = make_document(&pool).await;
        eng.enqueue(&NewJob {
            kind: DOCUMENT_INGEST_KIND.into(),
            payload: serde_json::json!({
                "document_id": doc_id.as_str(),
                "content": "hi",
                "file_name": "../escape.txt",
            }),
            priority: 0,
            run_at: None,
            max_attempts: Some(1),
        })
        .await
        .unwrap();

        assert!(eng.run_once().await.unwrap());
        assert!(eng.run_once().await.unwrap());
        let failed = list_jobs(&pool, Some(JobStatus::Failed), 10).await.unwrap();
        assert_eq!(failed.len(), 2);
        std::fs::remove_dir_all(&root).ok();
    }

    // -----------------------------------------------------------------
    // register_builtin / system.ping / storage.gc_staging
    // -----------------------------------------------------------------

    #[tokio::test]
    async fn register_builtin_handles_system_ping() {
        let pool = test_pool().await;
        let mut eng = JobEngine::new(
            pool.clone(),
            JobEngineConfig {
                poll_interval: Duration::from_millis(10),
                default_max_attempts: 3,
                backoff_base: Duration::from_secs(0),
                stale_after: Duration::from_secs(60),
            },
        );
        register_builtin(&mut eng);

        eng.enqueue(&NewJob {
            kind: SYSTEM_PING_KIND.into(),
            payload: serde_json::json!({}),
            priority: 0,
            run_at: None,
            max_attempts: None,
        })
        .await
        .unwrap();
        assert!(eng.run_once().await.unwrap());

        let jobs = list_jobs(&pool, Some(JobStatus::Succeeded), 10)
            .await
            .unwrap();
        assert_eq!(jobs.len(), 1);
        assert_eq!(jobs[0].kind, SYSTEM_PING_KIND);
    }

    #[test]
    fn resolve_under_root_rejects_escapes() {
        let root = PathBuf::from("/storage");
        assert!(resolve_under_root(&root, "docs/a.pdf").is_ok());
        assert!(resolve_under_root(&root, "./docs/a.pdf").is_ok());
        for bad in [
            "",
            "   ",
            "/etc/passwd",
            "../escape.pdf",
            "docs/../../escape.pdf",
            "docs/../a.pdf",
        ] {
            assert!(
                resolve_under_root(&root, bad).is_err(),
                "expected {bad:?} to be rejected"
            );
        }
    }

    #[test]
    fn gc_staging_dir_removes_only_expired_entries() {
        let root = temp_root();
        let staging = root.join("staging");
        std::fs::create_dir_all(&staging).unwrap();

        // Fresh file (modified now) — must survive any positive retention.
        let fresh = staging.join("fresh.bin");
        std::fs::write(&fresh, b"new").unwrap();
        // A directory full of files — also fresh.
        let dir = staging.join("dir");
        std::fs::create_dir_all(dir.join("nested")).unwrap();
        std::fs::write(dir.join("nested/f.bin"), b"x").unwrap();

        // retention = 0 → cutoff is ~now, everything created before the
        // call is "expired".
        let removed = gc_staging_dir(&staging, Duration::ZERO);
        assert_eq!(removed, 2);
        assert!(!fresh.exists());
        assert!(!dir.exists());

        // Missing staging dir is a no-op.
        assert_eq!(gc_staging_dir(&root.join("nope"), Duration::ZERO), 0);

        // Huge retention keeps everything.
        std::fs::write(&fresh, b"new").unwrap();
        let kept = gc_staging_dir(&staging, Duration::from_secs(u64::MAX));
        assert_eq!(kept, 0);
        assert!(fresh.exists());

        std::fs::remove_dir_all(&root).ok();
    }
}
