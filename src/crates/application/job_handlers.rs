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
//! | `document.metadata_extract` | `{document_id, artifact_id?}`                             | Sniff format + extract PDF metadata into document metadata (see [`DocumentMetadataExtractHandler`]) |
//! | `document.index`        | `{document_id, artifact_id?}`                                 | Extract searchable text into an `index` metadata field (see [`DocumentIndexHandler`]) |
//! | `thumbnail.generate`    | `{document_id, artifact_id?}`                                 | Extract an embedded PDF thumbnail image into storage (see [`ThumbnailGenerateHandler`]) |
//! | `pdf.inspect`           | `{path, max_bytes?}`                                          | Validate + inspect a PDF under the storage root; writes `<path>.inspect.json` |
//! | `storage.gc_staging`    | `{retention_secs?}`                                           | Delete abandoned entries under `<storage_root>/staging/` and `<storage_root>/.staging/` older than the retention window |
//!
//! `document.metadata_extract` / `document.index` / `thumbnail.generate`
//! payload details: `document_id` is required; `artifact_id` is
//! optional and selects a specific artifact — when omitted the
//! **most recently created** artifact of the document is used
//! (`list_artifacts` order, last entry). Artifact bytes are read
//! through the streaming backend with a hard cap of
//! [`MAX_PROCESSING_BYTES`] (64 MiB); larger objects are recorded as
//! `skipped_too_large` rather than read into memory. All three
//! handlers are idempotent: they overwrite deterministic metadata
//! fields and storage object names, so job retries and crash-recovery
//! replays are safe.
//!
//! # Processing status tracking
//!
//! These handlers report per-document progress through **document
//! metadata custom fields** (queryable via
//! `store::metadata::get_document_metadata`), backed by audit events:
//!
//! - `processing_status` — status of the *most recently run*
//!   processing job: `queued` (set by the `enqueue_*` helpers,
//!   best-effort), `running`, then a terminal value: `succeeded`,
//!   `failed`, `unsupported`, `unavailable`, or `skipped_too_large`.
//! - `{op}_status` — the same terminal vocabulary scoped to one
//!   operation: `metadata_extract_status`, `index_status`,
//!   `thumbnail_status`. This is the field to check for "did indexing
//!   work on this document".
//! - Audit events: [`DocumentAuditEventType::ProcessingStarted`] when
//!   a handler begins, `ProcessingCompleted` on success,
//!   `ProcessingFailed` only when the job exhausts `max_attempts`
//!   (intermediate retries stay `running` — the job row in
//!   `jobs_tbl` remains the authoritative retry state).
//!
//! The `jobs_tbl` row (`pending`/`running`/`succeeded`/`failed`) is
//! the source of truth for *execution*; the metadata fields above are
//! the source of truth for *document-level outcome* and survive job
//! row cleanup.
//!
//! Use the [`enqueue_ingest`], [`enqueue_metadata_extract`],
//! [`enqueue_index`], and [`enqueue_thumbnail`] helpers to produce
//! jobs — they build the documented payload contracts and stamp
//! `processing_status = "queued"` (best-effort) so producers never
//! hand-craft JSON.
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
//! 86 400 (24 h). Every entry directly under `staging/` **and** the
//! streaming backend's reserved `.staging/` directory whose
//! `modified()` time is older than the window is removed —
//! directories via `remove_dir_all`, everything else via
//! `remove_file`. Entries whose metadata cannot be read are skipped;
//! a missing `staging/` directory is a no-op success.

use std::io::Read;
use std::path::{Component, Path, PathBuf};
use std::sync::Arc;
use std::time::{Duration, SystemTime};

use base64::{Engine as _, engine::general_purpose::STANDARD as B64};
use serde::Deserialize;
use serde_json::json;
use sha2::{Digest, Sha256};

use crate::config::AppConfig;
use crate::crates::document_engine::artifact::{
    ChecksumAlgorithm, DocumentArtifact, DocumentChecksum,
};
use crate::crates::document_engine::audit::DocumentAuditEventType;
use crate::crates::document_engine::ids::DocumentId;
use crate::crates::document_engine::metadata::DocumentMetadata;
use crate::crates::document_engine::store::artifacts::{
    NewArtifact, download_artifact, list_artifacts, upload_artifact,
};
use crate::crates::document_engine::store::documents::get_document;
use crate::crates::document_engine::store::history::record_event;
use crate::crates::document_engine::store::metadata::{
    get_document_metadata, upsert_document_metadata,
};
use crate::crates::document_engine::store::StoreError;
use crate::crates::job_engine::{Job, JobContext, JobEngine, JobError, JobHandler, NewJob, enqueue};
use crate::crates::security_engine::{SniffedFormat, sniff_format};
use crate::crates::storage::local::config::StorageConfig;
use crate::crates::storage::local::debug_when_full::{FaultInjectingFs, StorageError};
use crate::crates::storage::stream::{StreamUpload, StreamingBackend};

/// Job kind for [`DocumentIngestHandler`].
pub const DOCUMENT_INGEST_KIND: &str = "document.ingest";

/// Job kind for [`SystemPingHandler`] — health check / smoke test.
pub const SYSTEM_PING_KIND: &str = "system.ping";

/// Job kind for [`PdfInspectHandler`].
pub const PDF_INSPECT_KIND: &str = "pdf.inspect";

/// Job kind for [`StorageGcStagingHandler`].
pub const STORAGE_GC_STAGING_KIND: &str = "storage.gc_staging";

/// Job kind for [`DocumentMetadataExtractHandler`].
pub const DOCUMENT_METADATA_EXTRACT_KIND: &str = "document.metadata_extract";

/// Job kind for [`DocumentIndexHandler`].
pub const DOCUMENT_INDEX_KIND: &str = "document.index";

/// Job kind for [`ThumbnailGenerateHandler`].
pub const THUMBNAIL_GENERATE_KIND: &str = "thumbnail.generate";

/// Default `max_bytes` for `pdf.inspect` (100 MiB).
const DEFAULT_INSPECT_MAX_BYTES: u64 = 100 * 1024 * 1024;

/// Hard cap on artifact bytes read for `document.metadata_extract`,
/// `document.index`, and `thumbnail.generate`. Larger objects are
/// recorded as `skipped_too_large` instead of being buffered.
const MAX_PROCESSING_BYTES: u64 = 64 * 1024 * 1024;

/// Bytes read first when sniffing an artifact's format.
const SNIFF_BYTES: usize = 4096;

/// Maximum characters of extracted text kept in the `index` metadata
/// field's `excerpt` member (the full-text hash/counts are always
/// recorded, so the excerpt is a preview, not the index itself).
const INDEX_EXCERPT_CHARS: usize = 50_000;

/// Chunk size for streaming uploads (`document.ingest`, thumbnails).
const UPLOAD_CHUNK_BYTES: usize = 64 * 1024;

/// Maximum decoded thumbnail raster (width × height pixels) — 16 MP
/// bounds decoded RGB samples at ~48 MiB.
const MAX_THUMBNAIL_PIXELS: u64 = 16_000_000;

/// Maximum decoded thumbnail payload accepted from a PDF image stream.
const MAX_THUMBNAIL_DECODED_BYTES: usize = 64 * 1024 * 1024;

/// Default `max_attempts` used by the `enqueue_*` helpers.
const DEFAULT_MAX_ATTEMPTS: u32 = 3;

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
    match DocumentMetadataExtractHandler::from_app_config() {
        Ok(handler) => engine.register(
            DOCUMENT_METADATA_EXTRACT_KIND,
            Arc::new(handler),
        ),
        Err(e) => tracing::warn!(
            error = %e,
            "skipping document.metadata_extract handler: storage config unavailable"
        ),
    }
    match DocumentIndexHandler::from_app_config() {
        Ok(handler) => engine.register(DOCUMENT_INDEX_KIND, Arc::new(handler)),
        Err(e) => tracing::warn!(
            error = %e,
            "skipping document.index handler: storage config unavailable"
        ),
    }
    match ThumbnailGenerateHandler::from_app_config() {
        Ok(handler) => engine.register(THUMBNAIL_GENERATE_KIND, Arc::new(handler)),
        Err(e) => tracing::warn!(
            error = %e,
            "skipping thumbnail.generate handler: storage config unavailable"
        ),
    }
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
                // Stream the payload through the staging/commit path so
                // the SHA-256 and byte count come from the `CommitReceipt`
                // and a crash mid-upload never publishes a partial object.
                // The payload is already fully in memory (JSON), so we
                // chunk it rather than changing the wire contract.
                let mut fs = FaultInjectingFs::new(self.storage.clone())
                    .map_err(|e| JobError::Failed(format!("storage init failed: {e}")))?;
                let mut upload = fs
                    .begin_upload(&object_name)
                    .map_err(|e| JobError::Failed(format!("storage upload failed: {e}")))?;
                for chunk in payload.bytes.chunks(UPLOAD_CHUNK_BYTES) {
                    upload
                        .write_chunk(chunk)
                        .map_err(|e| JobError::Failed(format!("storage upload failed: {e}")))?;
                }
                let receipt = upload
                    .commit()
                    .map_err(|e| JobError::Failed(format!("storage commit failed: {e}")))?;

                let checksum = DocumentChecksum {
                    algorithm: ChecksumAlgorithm::Sha256,
                    value: receipt.sha256.clone(),
                };
                let artifact = upload_artifact(
                    &ctx.pool,
                    &NewArtifact {
                        id: None,
                        document_id: document_id.clone(),
                        storage_uri: storage_uri.clone(),
                        file_name: payload.file_name.clone(),
                        mime_type: payload.mime_type.clone(),
                        size_bytes: receipt.bytes,
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
            let retention = Duration::from_secs(payload.retention_secs);
            // Two staging areas exist: the legacy `staging/` directory
            // and the streaming backend's reserved `.staging/`
            // (`storage::stream::STAGING_DIR_NAME`), where crashed
            // uploads leave partial objects behind.
            let removed = gc_staging_dir(&root.join("staging"), retention)
                + gc_staging_dir(
                    &root.join(crate::crates::storage::stream::STAGING_DIR_NAME),
                    retention,
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

// ---------------------------------------------------------------------------
// document.metadata_extract / document.index / thumbnail.generate
// ---------------------------------------------------------------------------
//
// Shared machinery for the document-processing job kinds. See the
// module docs for the payload contracts and the processing-status
// scheme (`processing_status` + `{op}_status` metadata fields,
// ProcessingStarted/Completed/Failed audit events).

/// Payload shared by `document.metadata_extract`, `document.index`,
/// and `thumbnail.generate`.
#[derive(Debug)]
struct DocJobPayload {
    document_id: DocumentId,
    /// Optional artifact override; `None` selects the most recently
    /// created artifact.
    artifact_id: Option<String>,
}

impl DocJobPayload {
    fn parse(payload: &serde_json::Value) -> Result<Self, JobError> {
        let document_id = payload["document_id"]
            .as_str()
            .filter(|s| !s.is_empty())
            .ok_or_else(|| {
                JobError::Failed("payload.document_id must be a non-empty string".into())
            })?;
        let artifact_id = payload["artifact_id"]
            .as_str()
            .map(str::trim)
            .filter(|s| !s.is_empty())
            .map(str::to_string);
        Ok(Self {
            document_id: DocumentId::new(document_id),
            artifact_id,
        })
    }
}

/// Result of a bounded artifact read: at most `max_bytes` of content
/// plus a flag telling whether the object held more.
struct BoundedBytes {
    bytes: Vec<u8>,
    truncated: bool,
}

/// Converts a `local://` storage URI into the confined object name the
/// streaming backend expects. Other schemes are rejected — the
/// document store currently only produces `local://` URIs.
fn object_name_for_uri(uri: &str) -> Result<String, JobError> {
    uri.strip_prefix("local://")
        .map(str::to_string)
        .ok_or_else(|| JobError::Failed(format!("unsupported storage_uri: {uri}")))
}

/// Maps a storage error into a retryable job failure.
fn storage_err(e: StorageError) -> JobError {
    JobError::Failed(format!("storage error: {e}"))
}

/// Reads at most `max_bytes + 1` bytes from `name` so callers can tell
/// "exactly at the cap" apart from "there was more". Synchronous —
/// call inside `spawn_blocking`.
fn read_object_bounded(
    fs: &mut FaultInjectingFs,
    name: &str,
    max_bytes: u64,
) -> Result<BoundedBytes, JobError> {
    let reader = fs.open_reader(name).map_err(storage_err)?;
    let mut limited = reader.take(max_bytes.saturating_add(1));
    let mut buf = Vec::new();
    limited
        .read_to_end(&mut buf)
        .map_err(|e| JobError::Failed(format!("storage read failed for {name}: {e}")))?;
    let truncated = buf.len() as u64 > max_bytes;
    buf.truncate(max_bytes as usize);
    Ok(BoundedBytes { bytes: buf, truncated })
}

/// [`EngineLimits`] applied to pdf-engine calls made by job handlers:
/// the same 64 MiB input cap as the storage read, plus the default
/// page budget from `security_engine::ResourceLimits`.
fn processing_limits() -> crate::crates::core::engine::EngineLimits {
    crate::crates::core::engine::EngineLimits {
        max_input_bytes: Some(MAX_PROCESSING_BYTES),
        max_pages: Some(10_000),
        ..Default::default()
    }
}

/// Runs a synchronous, potentially CPU-bound closure on the blocking
/// thread pool so lopdf/pdf-engine work never occupies an async
/// executor thread.
async fn run_blocking<T, F>(what: &str, f: F) -> Result<T, JobError>
where
    F: FnOnce() -> Result<T, JobError> + Send + 'static,
    T: Send + 'static,
{
    tokio::task::spawn_blocking(f)
        .await
        .map_err(|e| JobError::Failed(format!("{what} blocking task failed: {e}")))?
}

/// Fetches the document's metadata row (or a default one) and applies
/// `f`, then upserts — one read-modify-write per call.
async fn mutate_metadata(
    pool: &sqlx::SqlitePool,
    document_id: &DocumentId,
    f: impl FnOnce(&mut DocumentMetadata),
) -> Result<(), JobError> {
    let mut meta = get_document_metadata(pool, document_id)
        .await
        .map_err(store_err)?
        .unwrap_or_else(|| DocumentMetadata {
            document_id: document_id.clone(),
            title: None,
            author: None,
            subject: None,
            language: None,
            keywords: Vec::new(),
            custom_fields: serde_json::Map::new(),
        });
    f(&mut meta);
    upsert_document_metadata(pool, &meta)
        .await
        .map_err(store_err)
}

/// Records a processing audit event; failures propagate so a missing
/// audit row can't silently pass.
async fn audit_event(
    pool: &sqlx::SqlitePool,
    document_id: &DocumentId,
    event_type: DocumentAuditEventType,
    details: serde_json::Value,
) -> Result<(), JobError> {
    record_event(pool, document_id, None, event_type, Some(&details))
        .await
        .map_err(store_err)
}

/// Maps an operation-level status to the shared `processing_status`
/// vocabulary. Operation statuses are more specific
/// (`extracted`/`indexed`/`generated`/`empty`); `processing_status`
/// collapses them to `succeeded`/`failed` while passing the
/// already-generic values (`queued`, `running`, `unsupported`,
/// `unavailable`, `skipped_too_large`) through unchanged.
fn normalized_processing_status(status: &str) -> &'static str {
    match status {
        "extracted" | "indexed" | "generated" | "empty" => "succeeded",
        "error" | "failed" => "failed",
        "queued" => "queued",
        "running" => "running",
        "succeeded" => "succeeded",
        "unsupported" => "unsupported",
        "unavailable" => "unavailable",
        "skipped_too_large" => "skipped_too_large",
        _ => "failed",
    }
}

/// Sets `processing_status` and the per-operation `{op}_status`
/// custom field to `status` in one metadata update.
async fn mark_op_status(
    pool: &sqlx::SqlitePool,
    document_id: &DocumentId,
    op_status_key: &str,
    status: &str,
) -> Result<(), JobError> {
    let key = op_status_key.to_string();
    let proc_status = normalized_processing_status(status);
    mutate_metadata(pool, document_id, move |m| {
        m.custom_fields
            .insert("processing_status".into(), json!(proc_status));
        m.custom_fields.insert(key, json!(status));
    })
    .await
}

/// Best-effort terminal `failed` status + audit event, used when a
/// handler is about to return `Err` on its final attempt. Errors are
/// swallowed: the job row already records the failure.
async fn mark_failed(
    pool: &sqlx::SqlitePool,
    document_id: &DocumentId,
    op_status_key: &str,
    job: &Job,
    error: &JobError,
) {
    let _ = mark_op_status(pool, document_id, op_status_key, "failed").await;
    let _ = audit_event(
        pool,
        document_id,
        DocumentAuditEventType::ProcessingFailed,
        json!({
            "job_id": job.id,
            "job_kind": job.kind,
            "attempts": job.attempts,
            "error": error.to_string(),
        }),
    )
    .await;
}

/// Resolves which artifact a processing job operates on: the explicit
/// `artifact_id` when given (verified to belong to the document), or
/// the most recently created artifact otherwise.
async fn resolve_artifact(
    pool: &sqlx::SqlitePool,
    document_id: &DocumentId,
    artifact_id: Option<&str>,
) -> Result<DocumentArtifact, JobError> {
    match artifact_id {
        Some(id) => {
            let artifact = download_artifact(pool, id)
                .await
                .map_err(store_err)?
                .ok_or_else(|| JobError::Failed(format!("artifact not found: {id}")))?;
            if artifact.document_id != *document_id {
                return Err(JobError::Failed(format!(
                    "artifact {id} does not belong to document {document_id}"
                )));
            }
            Ok(artifact)
        }
        None => {
            let artifacts = list_artifacts(pool, document_id)
                .await
                .map_err(store_err)?;
            artifacts.into_iter().last().ok_or_else(|| {
                JobError::Failed(format!("document {document_id} has no artifacts"))
            })
        }
    }
}

/// Verifies the document exists, resolves the target artifact, marks
/// the operation `running`, and emits `ProcessingStarted`. Shared
/// prologue for all three processing handlers.
async fn begin_processing(
    pool: &sqlx::SqlitePool,
    job: &Job,
    op_status_key: &str,
) -> Result<(DocumentId, DocumentArtifact), JobError> {
    let payload = DocJobPayload::parse(&job.payload)?;
    let document_id = payload.document_id;

    get_document(pool, &document_id)
        .await
        .map_err(store_err)?
        .ok_or_else(|| JobError::Failed(format!("document not found: {document_id}")))?;

    let artifact = resolve_artifact(pool, &document_id, payload.artifact_id.as_deref()).await?;

    mark_op_status(pool, &document_id, op_status_key, "running").await?;
    audit_event(
        pool,
        &document_id,
        DocumentAuditEventType::ProcessingStarted,
        json!({
            "job_id": job.id,
            "job_kind": job.kind,
            "artifact_id": artifact.id,
            "storage_uri": artifact.storage_uri,
        }),
    )
    .await?;

    Ok((document_id, artifact))
}

/// Stable lowercase name for a [`SniffedFormat`] (matches its serde
/// representation).
fn sniffed_name(format: SniffedFormat) -> &'static str {
    match format {
        SniffedFormat::Pdf => "pdf",
        SniffedFormat::Png => "png",
        SniffedFormat::Jpeg => "jpeg",
        SniffedFormat::Zip => "zip",
        SniffedFormat::Gif => "gif",
        SniffedFormat::Unknown => "unknown",
    }
}

/// Conventional MIME type for a sniffed format; `None` for unknown.
fn mime_for_format(format: SniffedFormat) -> Option<&'static str> {
    match format {
        SniffedFormat::Pdf => Some("application/pdf"),
        SniffedFormat::Png => Some("image/png"),
        SniffedFormat::Jpeg => Some("image/jpeg"),
        SniffedFormat::Zip => Some("application/zip"),
        SniffedFormat::Gif => Some("image/gif"),
        SniffedFormat::Unknown => None,
    }
}

/// `document.metadata_extract` — sniffs the artifact's real format and,
/// for PDFs, extracts [`PdfInfo`](crate::crates::pdf_engine::PdfInfo)
/// via `pdf_engine` inside `spawn_blocking`.
///
/// Persisted results (document metadata): `detected_format`,
/// `mime_type`, `metadata_extracted_at`, `metadata_extract_status`,
/// plus `page_count`/`pdf_creator`/`pdf_producer`/`pdf_encrypted` and
/// the `title`/`author` columns (filled only when currently unset —
/// extracted values never overwrite curated metadata). Non-PDF and
/// oversized artifacts get an honest `unsupported` /
/// `skipped_too_large` status and the job still succeeds. Re-running
/// overwrites everything, so retries are safe.
pub struct DocumentMetadataExtractHandler {
    storage: StorageConfig,
}

/// Metadata custom-field key holding the extract status.
const META_EXTRACT_STATUS_KEY: &str = "metadata_extract_status";

impl DocumentMetadataExtractHandler {
    /// Builds the handler from the application storage configuration.
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

/// Everything the blocking half of `metadata_extract` produces.
struct ExtractOutcome {
    format: SniffedFormat,
    truncated: bool,
    pdf_info: Option<crate::crates::pdf_engine::PdfInfo>,
    pdf_error: Option<String>,
}

impl JobHandler for DocumentMetadataExtractHandler {
    fn handle<'a>(
        &'a self,
        ctx: &'a JobContext,
        job: &'a Job,
    ) -> std::pin::Pin<Box<dyn std::future::Future<Output = Result<(), JobError>> + Send + 'a>>
    {
        Box::pin(async move {
            let result = self.run(ctx, job).await;
            if let Err(e) = &result
                && job.attempts >= job.max_attempts
                && let Ok(payload) = DocJobPayload::parse(&job.payload)
            {
                mark_failed(
                    &ctx.pool,
                    &payload.document_id,
                    META_EXTRACT_STATUS_KEY,
                    job,
                    e,
                )
                .await;
            }
            result
        })
    }
}

impl DocumentMetadataExtractHandler {
    async fn run(&self, ctx: &JobContext, job: &Job) -> Result<(), JobError> {
        let (document_id, artifact) =
            begin_processing(&ctx.pool, job, META_EXTRACT_STATUS_KEY).await?;

        let object_name = object_name_for_uri(&artifact.storage_uri)?;
        let storage = self.storage.clone();
        let read_name = object_name.clone();
        let outcome = run_blocking("metadata_extract", move || {
            let mut fs = FaultInjectingFs::new(storage)
                .map_err(|e| JobError::Failed(format!("storage init failed: {e}")))?;
            let read = read_object_bounded(&mut fs, &read_name, MAX_PROCESSING_BYTES)?;
            let head = &read.bytes[..read.bytes.len().min(SNIFF_BYTES)];
            let format = sniff_format(head);

            let (pdf_info, pdf_error) = if format == SniffedFormat::Pdf && !read.truncated {
                let engine = crate::crates::pdf_engine::PdfEngine::with_limits(
                    processing_limits(),
                );
                match engine.inspect(&read.bytes) {
                    Ok(info) => (Some(info), None),
                    Err(e) => (None, Some(e.to_string())),
                }
            } else {
                (None, None)
            };

            Ok(ExtractOutcome {
                format,
                truncated: read.truncated,
                pdf_info,
                pdf_error,
            })
        })
        .await?;

        // Terminal status: oversized input wins over everything; a PDF
        // parse failure is reported as `error`; unsupported formats
        // (including non-PDF) succeed with `unsupported`.
        let status = if outcome.truncated {
            "skipped_too_large"
        } else if outcome.pdf_error.is_some() {
            "error"
        } else if outcome.pdf_info.is_some() {
            "extracted"
        } else {
            "unsupported"
        };

        let mime_type = mime_for_format(outcome.format)
            .map(str::to_string)
            .or_else(|| artifact.mime_type.clone())
            .unwrap_or_else(|| "application/octet-stream".to_string());
        let extracted_at = crate::crates::document_engine::store::now_rfc3339();
        let info = outcome.pdf_info.clone();
        let pdf_error = outcome.pdf_error.clone();
        let format_name = sniffed_name(outcome.format);
        let artifact_id = artifact.id.clone();
        let storage_uri = artifact.storage_uri.clone();
        let artifact_size = artifact.size_bytes;

        mutate_metadata(&ctx.pool, &document_id, move |m| {
            m.custom_fields
                .insert("processing_status".into(), json!(status));
            m.custom_fields
                .insert(META_EXTRACT_STATUS_KEY.into(), json!(status));
            m.custom_fields
                .insert("detected_format".into(), json!(format_name));
            m.custom_fields.insert("mime_type".into(), json!(mime_type));
            m.custom_fields
                .insert("artifact_id".into(), json!(artifact_id));
            m.custom_fields
                .insert("storage_uri".into(), json!(storage_uri));
            m.custom_fields
                .insert("artifact_size_bytes".into(), json!(artifact_size));
            m.custom_fields
                .insert("metadata_extracted_at".into(), json!(extracted_at));
            if let Some(err) = &pdf_error {
                m.custom_fields
                    .insert("extract_error".into(), json!(err));
            }
            if let Some(info) = &info {
                m.custom_fields
                    .insert("page_count".into(), json!(info.page_count));
                m.custom_fields.insert(
                    "pdf_creator".into(),
                    json!(info.creator.clone().unwrap_or_default()),
                );
                m.custom_fields.insert(
                    "pdf_producer".into(),
                    json!(info.producer.clone().unwrap_or_default()),
                );
                m.custom_fields
                    .insert("pdf_encrypted".into(), json!(info.encrypted));
                // Extracted values fill empty columns only — curated
                // titles/authors are never overwritten.
                if m.title.is_none() {
                    m.title = info.title.clone();
                }
                if m.author.is_none() {
                    m.author = info.author.clone();
                }
                // Keep pdf_title/pdf_author as custom fields too so the
                // raw extracted values stay visible even when the
                // columns were already populated.
                m.custom_fields.insert(
                    "pdf_title".into(),
                    json!(info.title.clone().unwrap_or_default()),
                );
                m.custom_fields.insert(
                    "pdf_author".into(),
                    json!(info.author.clone().unwrap_or_default()),
                );
            }
        })
        .await?;

        audit_event(
            &ctx.pool,
            &document_id,
            DocumentAuditEventType::ProcessingCompleted,
            json!({
                "job_id": job.id,
                "job_kind": job.kind,
                "status": status,
                "format": format_name,
            }),
        )
        .await?;

        Ok(())
    }
}

/// `document.index` — extracts searchable text where feasible and
/// stores a bounded index payload in the `index` metadata custom
/// field.
///
/// Sources: PDFs go through `pdf_engine::extract_text` (all pages,
/// `spawn_blocking`); artifacts whose bytes are valid UTF-8 (or whose
/// MIME type is `text/*`) are indexed directly; everything else gets
/// `index_status: "unsupported"`. Oversized artifacts get
/// `skipped_too_large`. The `index` field holds `{status, format,
/// char_count, word_count, sha256, excerpt}` where `excerpt` is the
/// first [`INDEX_EXCERPT_CHARS`] characters — there is no separate
/// search table by design.
pub struct DocumentIndexHandler {
    storage: StorageConfig,
}

/// Metadata custom-field key holding the index status.
const INDEX_STATUS_KEY: &str = "index_status";

impl DocumentIndexHandler {
    /// Builds the handler from the application storage configuration.
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

/// Outcome of the blocking half of `document.index`.
enum IndexOutcome {
    /// Object exceeded [`MAX_PROCESSING_BYTES`].
    TooLarge,
    /// Format has no text-extraction path.
    Unsupported,
    /// Extraction ran but produced no text.
    Empty,
    /// Full extracted text (unbounded by the excerpt cap).
    Text { format: SniffedFormat, text: String },
    /// Permanent extraction failure (e.g. corrupt PDF).
    Failed { format: SniffedFormat, error: String },
    /// PDF is encrypted — cannot extract without a password.
    Encrypted,
}

impl JobHandler for DocumentIndexHandler {
    fn handle<'a>(
        &'a self,
        ctx: &'a JobContext,
        job: &'a Job,
    ) -> std::pin::Pin<Box<dyn std::future::Future<Output = Result<(), JobError>> + Send + 'a>>
    {
        Box::pin(async move {
            let result = self.run(ctx, job).await;
            if let Err(e) = &result
                && job.attempts >= job.max_attempts
                && let Ok(payload) = DocJobPayload::parse(&job.payload)
            {
                mark_failed(&ctx.pool, &payload.document_id, INDEX_STATUS_KEY, job, e)
                    .await;
            }
            result
        })
    }
}

impl DocumentIndexHandler {
    async fn run(&self, ctx: &JobContext, job: &Job) -> Result<(), JobError> {
        let (document_id, artifact) =
            begin_processing(&ctx.pool, job, INDEX_STATUS_KEY).await?;

        let object_name = object_name_for_uri(&artifact.storage_uri)?;
        let storage = self.storage.clone();
        let artifact_mime = artifact.mime_type.clone();
        let read_name = object_name.clone();
        let outcome = run_blocking("document.index", move || {
            let mut fs = FaultInjectingFs::new(storage)
                .map_err(|e| JobError::Failed(format!("storage init failed: {e}")))?;
            let read = read_object_bounded(&mut fs, &read_name, MAX_PROCESSING_BYTES)?;
            if read.truncated {
                return Ok(IndexOutcome::TooLarge);
            }
            let head = &read.bytes[..read.bytes.len().min(SNIFF_BYTES)];
            let format = sniff_format(head);

            match format {
                SniffedFormat::Pdf => {
                    let engine = crate::crates::pdf_engine::PdfEngine::with_limits(
                        processing_limits(),
                    );
                    match engine.extract_text(&read.bytes, None) {
                        Ok(pages) => {
                            let text = pages
                                .iter()
                                .map(|p| p.text.as_str())
                                .collect::<Vec<_>>()
                                .join("\n\n");
                            if text.trim().is_empty() {
                                Ok(IndexOutcome::Empty)
                            } else {
                                Ok(IndexOutcome::Text { format, text })
                            }
                        }
                        Err(crate::crates::pdf_engine::PdfError::Encrypted) => {
                            Ok(IndexOutcome::Encrypted)
                        }
                        Err(e) => Ok(IndexOutcome::Failed {
                            format,
                            error: e.to_string(),
                        }),
                    }
                }
                _ => {
                    // Text path: declared text/* MIME or content that
                    // is valid UTF-8 end-to-end.
                    let declared_text = artifact_mime
                        .as_deref()
                        .is_some_and(|m| m.trim_start().to_ascii_lowercase().starts_with("text/"));
                    match std::str::from_utf8(&read.bytes) {
                        Ok(text) if declared_text || !text.trim().is_empty() => {
                            if text.trim().is_empty() {
                                Ok(IndexOutcome::Empty)
                            } else {
                                Ok(IndexOutcome::Text {
                                    format,
                                    text: text.to_string(),
                                })
                            }
                        }
                        _ => Ok(IndexOutcome::Unsupported),
                    }
                }
            }
        })
        .await?;

        let (status, index_value) = match &outcome {
            IndexOutcome::TooLarge => ("skipped_too_large", None),
            IndexOutcome::Unsupported | IndexOutcome::Encrypted => ("unsupported", None),
            IndexOutcome::Empty => ("empty", None),
            IndexOutcome::Failed { error, format } => {
                let v = serde_json::json!({
                    "status": "error",
                    "format": sniffed_name(*format),
                    "error": error,
                });
                ("error", Some(v))
            }
            IndexOutcome::Text { format, text } => {
                let char_count = text.chars().count() as u64;
                let word_count = text.split_whitespace().count() as u64;
                let sha256 = hex_lower(&Sha256::digest(text.as_bytes()));
                let excerpt: String = text.chars().take(INDEX_EXCERPT_CHARS).collect();
                let value = serde_json::json!({
                    "status": "indexed",
                    "format": sniffed_name(*format),
                    "char_count": char_count,
                    "word_count": word_count,
                    "sha256": sha256,
                    "excerpt": excerpt,
                    "indexed_at": crate::crates::document_engine::store::now_rfc3339(),
                });
                ("indexed", Some(value))
            }
        };

        let artifact_id = artifact.id.clone();
        let format_label = match &outcome {
            IndexOutcome::Text { format, .. } | IndexOutcome::Failed { format, .. } => {
                Some(sniffed_name(*format))
            }
            _ => None,
        };
        let index_error = match &outcome {
            IndexOutcome::Failed { error, .. } => Some(error.clone()),
            IndexOutcome::Encrypted => Some("encrypted PDF".to_string()),
            _ => None,
        };

        mutate_metadata(&ctx.pool, &document_id, move |m| {
            m.custom_fields
                .insert("processing_status".into(), json!(status));
            m.custom_fields.insert(INDEX_STATUS_KEY.into(), json!(status));
            m.custom_fields
                .insert("index_artifact_id".into(), json!(artifact_id));
            if let Some(f) = format_label {
                m.custom_fields
                    .insert("index_format".into(), json!(f));
            }
            if let Some(e) = &index_error {
                m.custom_fields
                    .insert("index_error".into(), json!(e));
            }
            if let Some(v) = index_value {
                m.custom_fields.insert("index".into(), v);
            }
        })
        .await?;

        let event_type = if status == "indexed" {
            DocumentAuditEventType::Indexed
        } else {
            DocumentAuditEventType::ProcessingCompleted
        };
        audit_event(
            &ctx.pool,
            &document_id,
            event_type,
            json!({
                "job_id": job.id,
                "job_kind": job.kind,
                "status": status,
                "format": format_label,
            }),
        )
        .await?;

        Ok(())
    }
}

/// `thumbnail.generate` — extracts an embedded thumbnail/preview image
/// from a PDF (page `/Thumb` entry or an image `/XObject`) and stores
/// it under `documents/{document_id}/thumbnails/` via the streaming
/// upload API.
///
/// Supported encodings: `DCTDecode` (written verbatim as `.jpg`),
/// `JPXDecode` (`.jp2`), and unfiltered/`FlateDecode` `DeviceRGB` or
/// `DeviceGray` 8-bpc rasters (re-encoded as `.png` via the `image`
/// crate). PDFs without an extractable embedded image get
/// `thumbnail_status: "unavailable"`; non-PDFs get `"unsupported"`;
/// oversized inputs get `"skipped_too_large"`. All cases succeed —
/// only genuine I/O or store failures retry.
pub struct ThumbnailGenerateHandler {
    storage: StorageConfig,
}

/// Metadata custom-field key holding the thumbnail status.
const THUMBNAIL_STATUS_KEY: &str = "thumbnail_status";

impl ThumbnailGenerateHandler {
    /// Builds the handler from the application storage configuration.
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

/// Color space of a raw thumbnail raster.
#[derive(Clone, Copy)]
enum ThumbColor {
    Rgb,
    Gray,
}

/// A thumbnail image extracted from a PDF, before file encoding.
enum ThumbImage {
    /// Already-encoded image file bytes (JPEG/JP2) plus extension.
    Encoded { bytes: Vec<u8>, ext: &'static str },
    /// Raw samples to be PNG-encoded.
    Samples {
        width: u32,
        height: u32,
        color: ThumbColor,
        data: Vec<u8>,
    },
}

/// A located thumbnail: which page it came from and the image.
struct ThumbCandidate {
    page: u32,
    image: ThumbImage,
}

/// Reads an integer entry from a lopdf dictionary.
fn dict_int(dict: &lopdf::Dictionary, key: &[u8]) -> Option<i64> {
    match dict.get(key).ok()? {
        lopdf::Object::Integer(n) => Some(*n),
        _ => None,
    }
}

/// Dereferences `obj` (following `Reference`s) and returns it as a
/// dictionary when possible.
fn as_dict<'a>(
    doc: &'a lopdf::Document,
    obj: &'a lopdf::Object,
) -> Option<&'a lopdf::Dictionary> {
    match doc.dereference(obj).ok()?.1 {
        lopdf::Object::Dictionary(d) => Some(d),
        _ => None,
    }
}

/// Converts a PDF image [`lopdf::Stream`] into a [`ThumbImage`], or
/// `None` when the encoding/color space is not supported.
fn thumb_from_stream(stream: &lopdf::Stream) -> Option<ThumbImage> {
    // `filters()` errors when /Filter is absent — treat as unfiltered.
    let filters = stream.filters().unwrap_or_default();

    // Already-encoded formats: the stream content *is* the file.
    if filters.len() == 1 && filters[0] == b"DCTDecode" {
        return Some(ThumbImage::Encoded {
            bytes: stream.content.clone(),
            ext: "jpg",
        });
    }
    if filters.len() == 1 && filters[0] == b"JPXDecode" {
        return Some(ThumbImage::Encoded {
            bytes: stream.content.clone(),
            ext: "jp2",
        });
    }
    // Only unfiltered or FlateDecode streams are decodable to raw
    // samples; anything else (CCITT, JBIG2, multi-filter chains, ...)
    // is reported as unavailable rather than approximated.
    if !(filters.is_empty() || filters.iter().all(|f| *f == b"FlateDecode")) {
        return None;
    }

    let dict = &stream.dict;
    let width = u32::try_from(dict_int(dict, b"Width")?).ok()?;
    let height = u32::try_from(dict_int(dict, b"Height")?).ok()?;
    let bpc = dict_int(dict, b"BitsPerComponent")?;
    if bpc != 8 || width == 0 || height == 0 {
        return None;
    }
    if (width as u64) * (height as u64) > MAX_THUMBNAIL_PIXELS {
        return None;
    }

    let color = match dict.get(b"ColorSpace").ok().and_then(|o| o.as_name().ok()) {
        Some(b"DeviceRGB") => ThumbColor::Rgb,
        Some(b"DeviceGray") => ThumbColor::Gray,
        _ => return None,
    };

    let data = stream
        .decompressed_content_with_limit(MAX_THUMBNAIL_DECODED_BYTES)
        .ok()?;
    let expected = match color {
        ThumbColor::Rgb => width as usize * height as usize * 3,
        ThumbColor::Gray => width as usize * height as usize,
    };
    if data.len() != expected {
        return None;
    }
    Some(ThumbImage::Samples {
        width,
        height,
        color,
        data,
    })
}

/// Finds the first usable embedded thumbnail in `doc`: prefers each
/// page's `/Thumb` entry, then falls back to the first `/Subtype
/// /Image` XObject in the page's (inherited) resources.
fn find_pdf_thumbnail(doc: &lopdf::Document) -> Option<ThumbCandidate> {
    for (page_num, page_id) in doc.get_pages() {
        // 1) Explicit /Thumb entry on the page dictionary.
        if let Ok(page) = doc.get_dictionary(page_id)
            && let Ok(thumb) = page.get(b"Thumb")
            && let Ok((_, resolved)) = doc.dereference(thumb)
            && let lopdf::Object::Stream(stream) = resolved
            && let Some(image) = thumb_from_stream(stream)
        {
            return Some(ThumbCandidate {
                page: page_num,
                image,
            });
        }

        // 2) Image XObjects in the page's resource dictionaries.
        if let Ok((direct, referenced)) = doc.get_page_resources(page_id) {
            let mut resource_dicts: Vec<&lopdf::Dictionary> = Vec::new();
            if let Some(d) = direct {
                resource_dicts.push(d);
            }
            for id in referenced {
                if let Ok(d) = doc.get_dictionary(id) {
                    resource_dicts.push(d);
                }
            }
            for resources in resource_dicts {
                let Some(xobjects) = resources
                    .get(b"XObject")
                    .ok()
                    .and_then(|o| as_dict(doc, o))
                else {
                    continue;
                };
                for (_, xobj) in xobjects.iter() {
                    let Ok((_, resolved)) = doc.dereference(xobj) else {
                        continue;
                    };
                    let lopdf::Object::Stream(stream) = resolved else {
                        continue;
                    };
                    if stream
                        .dict
                        .get(b"Subtype")
                        .ok()
                        .and_then(|o| o.as_name().ok())
                        != Some(b"Image")
                    {
                        continue;
                    }
                    if let Some(image) = thumb_from_stream(stream) {
                        return Some(ThumbCandidate {
                            page: page_num,
                            image,
                        });
                    }
                }
            }
        }
    }
    None
}

/// Encodes a [`ThumbImage`] into a file-ready byte vector. Returns the
/// bytes and the file extension to use.
fn encode_thumbnail(image: &ThumbImage) -> Option<(Vec<u8>, &'static str)> {
    match image {
        ThumbImage::Encoded { bytes, ext } => Some((bytes.clone(), ext)),
        ThumbImage::Samples {
            width,
            height,
            color,
            data,
        } => {
            use image::ImageEncoder as _;
            let mut out = Vec::new();
            let result = match color {
                ThumbColor::Rgb => image::codecs::png::PngEncoder::new(&mut out).write_image(
                    data,
                    *width,
                    *height,
                    image::ExtendedColorType::Rgb8,
                ),
                ThumbColor::Gray => image::codecs::png::PngEncoder::new(&mut out).write_image(
                    data,
                    *width,
                    *height,
                    image::ExtendedColorType::L8,
                ),
            };
            result.ok()?;
            Some((out, "png"))
        }
    }
}

/// Outcome of the blocking half of `thumbnail.generate`.
enum ThumbOutcome {
    /// Object exceeded [`MAX_PROCESSING_BYTES`].
    TooLarge,
    /// Not a PDF — nothing to extract.
    Unsupported,
    /// PDF loaded but has no usable embedded thumbnail.
    Unavailable,
    /// Permanent failure (unparseable PDF, oversized raster, ...).
    Failed { error: String },
    /// Thumbnail located and encoded, ready to upload.
    Ready {
        page: u32,
        bytes: Vec<u8>,
        ext: &'static str,
    },
}

impl JobHandler for ThumbnailGenerateHandler {
    fn handle<'a>(
        &'a self,
        ctx: &'a JobContext,
        job: &'a Job,
    ) -> std::pin::Pin<Box<dyn std::future::Future<Output = Result<(), JobError>> + Send + 'a>>
    {
        Box::pin(async move {
            let result = self.run(ctx, job).await;
            if let Err(e) = &result
                && job.attempts >= job.max_attempts
                && let Ok(payload) = DocJobPayload::parse(&job.payload)
            {
                mark_failed(
                    &ctx.pool,
                    &payload.document_id,
                    THUMBNAIL_STATUS_KEY,
                    job,
                    e,
                )
                .await;
            }
            result
        })
    }
}

impl ThumbnailGenerateHandler {
    async fn run(&self, ctx: &JobContext, job: &Job) -> Result<(), JobError> {
        let (document_id, artifact) =
            begin_processing(&ctx.pool, job, THUMBNAIL_STATUS_KEY).await?;

        let object_name = object_name_for_uri(&artifact.storage_uri)?;
        let storage = self.storage.clone();
        let read_name = object_name.clone();
        let outcome = run_blocking("thumbnail.generate", move || {
            let mut fs = FaultInjectingFs::new(storage)
                .map_err(|e| JobError::Failed(format!("storage init failed: {e}")))?;
            let read = read_object_bounded(&mut fs, &read_name, MAX_PROCESSING_BYTES)?;
            if read.truncated {
                return Ok(ThumbOutcome::TooLarge);
            }
            let head = &read.bytes[..read.bytes.len().min(SNIFF_BYTES)];
            if sniff_format(head) != SniffedFormat::Pdf {
                return Ok(ThumbOutcome::Unsupported);
            }

            // lopdf can panic on hostile input — same guard the
            // pdf-engine applies at load time.
            let loaded = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
                lopdf::Document::load_mem(&read.bytes)
            }));
            let doc = match loaded {
                Ok(Ok(doc)) => doc,
                Ok(Err(e)) => {
                    return Ok(ThumbOutcome::Failed {
                        error: format!("pdf load failed: {e}"),
                    });
                }
                Err(_) => {
                    return Ok(ThumbOutcome::Failed {
                        error: "pdf parser crashed".to_string(),
                    });
                }
            };

            match find_pdf_thumbnail(&doc).and_then(|c| {
                encode_thumbnail(&c.image).map(|(bytes, ext)| ThumbOutcome::Ready {
                    page: c.page,
                    bytes,
                    ext,
                })
            }) {
                Some(ready) => Ok(ready),
                None => Ok(ThumbOutcome::Unavailable),
            }
        })
        .await?;

        match outcome {
            ThumbOutcome::TooLarge | ThumbOutcome::Unsupported | ThumbOutcome::Unavailable => {
                let status = match outcome {
                    ThumbOutcome::TooLarge => "skipped_too_large",
                    ThumbOutcome::Unsupported => "unsupported",
                    _ => "unavailable",
                };
                mutate_metadata(&ctx.pool, &document_id, move |m| {
                    m.custom_fields
                        .insert("processing_status".into(), json!(status));
                    m.custom_fields
                        .insert(THUMBNAIL_STATUS_KEY.into(), json!(status));
                })
                .await?;
                audit_event(
                    &ctx.pool,
                    &document_id,
                    DocumentAuditEventType::ProcessingCompleted,
                    json!({
                        "job_id": job.id,
                        "job_kind": job.kind,
                        "status": status,
                    }),
                )
                .await?;
                Ok(())
            }
            ThumbOutcome::Failed { error } => {
                mutate_metadata(&ctx.pool, &document_id, move |m| {
                    m.custom_fields
                        .insert("processing_status".into(), json!("error"));
                    m.custom_fields
                        .insert(THUMBNAIL_STATUS_KEY.into(), json!("error"));
                    m.custom_fields
                        .insert("thumbnail_error".into(), json!(error));
                })
                .await?;
                audit_event(
                    &ctx.pool,
                    &document_id,
                    DocumentAuditEventType::ProcessingCompleted,
                    json!({
                        "job_id": job.id,
                        "job_kind": job.kind,
                        "status": "error",
                    }),
                )
                .await?;
                Ok(())
            }
            ThumbOutcome::Ready { page, bytes, ext } => {
                // Deterministic object name → replays rewrite the same
                // thumbnail instead of accumulating versions.
                let thumb_name = format!(
                    "documents/{}/thumbnails/{}-p{}.{}",
                    document_id, artifact.id, page, ext
                );
                let storage_uri = format!("local://{thumb_name}");

                // Upload inside the blocking pool too: storage I/O is
                // synchronous and chunking keeps memory bounded.
                let storage = self.storage.clone();
                let upload_name = thumb_name.clone();
                let receipt = run_blocking("thumbnail upload", move || {
                    let mut fs = FaultInjectingFs::new(storage)
                        .map_err(|e| JobError::Failed(format!("storage init failed: {e}")))?;
                    let mut upload = fs.begin_upload(&upload_name).map_err(storage_err)?;
                    for chunk in bytes.chunks(UPLOAD_CHUNK_BYTES) {
                        upload.write_chunk(chunk).map_err(storage_err)?;
                    }
                    upload.commit().map_err(storage_err)
                })
                .await?;

                let sha = receipt.sha256.clone();
                let nbytes = receipt.bytes;
                let artifact_id = artifact.id.clone();
                mutate_metadata(&ctx.pool, &document_id, move |m| {
                    m.custom_fields
                        .insert("processing_status".into(), json!("generated"));
                    m.custom_fields
                        .insert(THUMBNAIL_STATUS_KEY.into(), json!("generated"));
                    m.custom_fields
                        .insert("thumbnail_uri".into(), json!(storage_uri));
                    m.custom_fields
                        .insert("thumbnail_sha256".into(), json!(sha));
                    m.custom_fields
                        .insert("thumbnail_bytes".into(), json!(nbytes));
                    m.custom_fields
                        .insert("thumbnail_page".into(), json!(page));
                    m.custom_fields
                        .insert("thumbnail_format".into(), json!(ext));
                    m.custom_fields
                        .insert("thumbnail_artifact_id".into(), json!(artifact_id));
                })
                .await?;

                audit_event(
                    &ctx.pool,
                    &document_id,
                    DocumentAuditEventType::ThumbnailGenerated,
                    json!({
                        "job_id": job.id,
                        "job_kind": job.kind,
                        "page": page,
                        "format": ext,
                        "storage_uri": format!("local://{thumb_name}"),
                        "bytes": nbytes,
                        "sha256": receipt.sha256,
                    }),
                )
                .await?;
                Ok(())
            }
        }
    }
}

// ---------------------------------------------------------------------------
// Enqueue helpers — producers should use these instead of hand-crafting
// job payloads. Each returns the enqueued [`Job`] and best-effort stamps
// `processing_status = "queued"` on the document's metadata.
// ---------------------------------------------------------------------------

/// Builds the shared document-processing payload.
fn doc_job_payload(document_id: &DocumentId, artifact_id: Option<&str>) -> serde_json::Value {
    let mut p = serde_json::json!({ "document_id": document_id.as_str() });
    if let Some(id) = artifact_id {
        p["artifact_id"] = json!(id);
    }
    p
}

/// Enqueues a `document.ingest` job for `bytes`.
///
/// Payload contract: `{document_id, content_b64, file_name?,
/// mime_type?}` — see [`DocumentIngestHandler`]. `file_name` defaults
/// to `"upload.bin"`.
pub async fn enqueue_ingest(
    pool: &sqlx::SqlitePool,
    document_id: &DocumentId,
    file_name: Option<&str>,
    mime_type: Option<&str>,
    bytes: &[u8],
) -> Result<Job, JobError> {
    let mut payload = serde_json::json!({
        "document_id": document_id.as_str(),
        "content_b64": B64.encode(bytes),
    });
    if let Some(name) = file_name {
        payload["file_name"] = json!(name);
    }
    if let Some(mime) = mime_type {
        payload["mime_type"] = json!(mime);
    }
    enqueue(
        pool,
        &NewJob {
            kind: DOCUMENT_INGEST_KIND.to_string(),
            payload,
            priority: 0,
            run_at: None,
            max_attempts: None,
        },
        DEFAULT_MAX_ATTEMPTS,
    )
    .await
}

/// Enqueues a `document.metadata_extract` job. Payload:
/// `{document_id, artifact_id?}` — see [`DocumentMetadataExtractHandler`].
pub async fn enqueue_metadata_extract(
    pool: &sqlx::SqlitePool,
    document_id: &DocumentId,
    artifact_id: Option<&str>,
) -> Result<Job, JobError> {
    let job = enqueue(
        pool,
        &NewJob {
            kind: DOCUMENT_METADATA_EXTRACT_KIND.to_string(),
            payload: doc_job_payload(document_id, artifact_id),
            priority: 0,
            run_at: None,
            max_attempts: None,
        },
        DEFAULT_MAX_ATTEMPTS,
    )
    .await?;
    // Best-effort: the job itself is the source of truth; a missing
    // document simply means the queued stamp has nowhere to go.
    let _ = mark_op_status(pool, document_id, META_EXTRACT_STATUS_KEY, "queued").await;
    Ok(job)
}

/// Enqueues a `document.index` job. Payload: `{document_id,
/// artifact_id?}` — see [`DocumentIndexHandler`].
pub async fn enqueue_index(
    pool: &sqlx::SqlitePool,
    document_id: &DocumentId,
    artifact_id: Option<&str>,
) -> Result<Job, JobError> {
    let job = enqueue(
        pool,
        &NewJob {
            kind: DOCUMENT_INDEX_KIND.to_string(),
            payload: doc_job_payload(document_id, artifact_id),
            priority: 0,
            run_at: None,
            max_attempts: None,
        },
        DEFAULT_MAX_ATTEMPTS,
    )
    .await?;
    let _ = mark_op_status(pool, document_id, INDEX_STATUS_KEY, "queued").await;
    Ok(job)
}

/// Enqueues a `thumbnail.generate` job. Payload: `{document_id,
/// artifact_id?}` — see [`ThumbnailGenerateHandler`].
pub async fn enqueue_thumbnail(
    pool: &sqlx::SqlitePool,
    document_id: &DocumentId,
    artifact_id: Option<&str>,
) -> Result<Job, JobError> {
    let job = enqueue(
        pool,
        &NewJob {
            kind: THUMBNAIL_GENERATE_KIND.to_string(),
            payload: doc_job_payload(document_id, artifact_id),
            priority: 0,
            run_at: None,
            max_attempts: None,
        },
        DEFAULT_MAX_ATTEMPTS,
    )
    .await?;
    let _ = mark_op_status(pool, document_id, THUMBNAIL_STATUS_KEY, "queued").await;
    Ok(job)
}

// ---------------------------------------------------------------------------
// document.metadata_extract / document.index / thumbnail.generate
// ---------------------------------------------------------------------------

/// Shared payload for the document-processing handlers:
/// `{document_id, artifact_id?}`. When `artifact_id` is omitted the
/// most recently created artifact of the document is used.
#[derive(Debug, Deserialize)]
struct DocumentJobPayload {
    document_id: String,
    #[serde(default)]
    artifact_id: Option<String>,
}

impl DocumentJobPayload {
    fn parse(payload: &serde_json::Value) -> Result<(Self, DocumentId), JobError> {
        let parsed: Self = serde_json::from_value(payload.clone())?;
        if parsed.document_id.trim().is_empty() {
            return Err(JobError::Failed(
                "payload.document_id must be a non-empty string".into(),
            ));
        }
        let id = DocumentId::new(parsed.document_id.clone());
        Ok((parsed, id))
    }
}

/// Resolves the artifact a document job operates on: the explicitly
/// named artifact, or the most recently created one. The artifact must
/// belong to the payload's document.
async fn resolve_job_artifact(
    pool: &sqlx::SqlitePool,
    document_id: &DocumentId,
    artifact_id: Option<&str>,
) -> Result<DocumentArtifact, JobError> {
    let artifact = match artifact_id {
        Some(id) => download_artifact(pool, id)
            .await
            .map_err(store_err)?
            .ok_or_else(|| JobError::Failed(format!("artifact not found: {id}")))?,
        None => list_artifacts(pool, document_id)
            .await
            .map_err(store_err)?
            .into_iter()
            .next_back()
            .ok_or_else(|| JobError::Failed("document has no artifacts".into()))?,
    };
    if artifact.document_id != *document_id {
        return Err(JobError::Failed(format!(
            "artifact {} does not belong to document {document_id}",
            artifact.id
        )));
    }
    Ok(artifact)
}

/// Maps a `local://` storage URI to the backend object name. Other
/// schemes are rejected — these handlers only read local storage.
fn object_name_from_uri(uri: &str) -> Result<String, JobError> {
    uri.strip_prefix("local://")
        .filter(|name| !name.is_empty())
        .map(str::to_string)
        .ok_or_else(|| {
            JobError::Failed(format!("unsupported storage_uri scheme: {uri:?}"))
        })
}

fn storage_fs(config: &StorageConfig) -> Result<FaultInjectingFs, JobError> {
    FaultInjectingFs::new(config.clone())
        .map_err(|e| JobError::Failed(format!("storage init failed: {e}")))
}

/// Reads at most `cap` bytes of object `name` through the streaming
/// backend. Returns `Ok(None)` when the object exceeds `cap` — the
/// caller records `skipped_too_large` rather than buffering it.
fn read_object_capped(
    fs: &mut FaultInjectingFs,
    name: &str,
    cap: u64,
) -> Result<Option<Vec<u8>>, JobError> {
    let mut reader = fs
        .open_reader(name)
        .map_err(|e| JobError::Failed(format!("storage read failed for {name:?}: {e}")))?;
    let mut buf = Vec::new();
    (&mut reader)
        .take(cap.saturating_add(1))
        .read_to_end(&mut buf)
        .map_err(|e| JobError::Failed(format!("storage read failed for {name:?}: {e}")))?;
    if buf.len() as u64 > cap {
        return Ok(None);
    }
    Ok(Some(buf))
}

/// Reads the artifact's bytes, enforcing [`MAX_PROCESSING_BYTES`].
/// `Ok(None)` means "too large — record skipped_too_large".
fn read_artifact_bytes(
    fs: &mut FaultInjectingFs,
    artifact: &DocumentArtifact,
) -> Result<Option<Vec<u8>>, JobError> {
    if artifact.size_bytes > MAX_PROCESSING_BYTES {
        return Ok(None);
    }
    let name = object_name_from_uri(&artifact.storage_uri)?;
    read_object_capped(fs, &name, MAX_PROCESSING_BYTES)
}

/// Marks the start of a processing op: `processing_status = "running"`
/// plus a [`DocumentAuditEventType::ProcessingStarted`] event.
async fn begin_processing(
    pool: &sqlx::SqlitePool,
    document_id: &DocumentId,
) -> Result<(), JobError> {
    set_custom_field(pool, document_id, "processing_status", &json!("running"))
        .await
        .map_err(store_err)?;
    record_event(
        pool,
        document_id,
        None,
        DocumentAuditEventType::ProcessingStarted,
        None,
    )
    .await
    .map_err(store_err)
}

/// Records a terminal processing outcome: `processing_status` plus the
/// per-op `{op}_status` custom field, and the matching audit event.
async fn finish_processing(
    pool: &sqlx::SqlitePool,
    document_id: &DocumentId,
    op: &str,
    status: &str,
    audit: DocumentAuditEventType,
) -> Result<(), JobError> {
    set_custom_field(pool, document_id, "processing_status", &json!(status))
        .await
        .map_err(store_err)?;
    set_custom_field(
        pool,
        document_id,
        &format!("{op}_status"),
        &json!(status),
    )
    .await
    .map_err(store_err)?;
    record_event(
        pool,
        document_id,
        None,
        audit,
        Some(&json!({ "operation": op, "status": status })),
    )
    .await
    .map_err(store_err)
}

/// Terminal statuses shared by all document-processing handlers.
mod op_status {
    pub const SUCCEEDED: &str = "succeeded";
    pub const FAILED: &str = "failed";
    pub const UNSUPPORTED: &str = "unsupported";
    pub const UNAVAILABLE: &str = "unavailable";
    pub const SKIPPED_TOO_LARGE: &str = "skipped_too_large";
}

/// Runs the shared preamble for a document job: parse payload, verify
/// the document exists, mark processing as running, resolve + size-check
/// the target artifact. Returns the artifact bytes, or `None` when the
/// artifact exceeded [`MAX_PROCESSING_BYTES`] (caller records
/// `skipped_too_large`).
async fn load_job_input(
    ctx: &JobContext,
    fs: &mut FaultInjectingFs,
    job: &Job,
) -> Result<Option<(DocumentId, DocumentArtifact, Vec<u8>)>, JobError> {
    let (payload, document_id) = DocumentJobPayload::parse(&job.payload)?;
    get_document(&ctx.pool, &document_id)
        .await
        .map_err(store_err)?
        .ok_or_else(|| JobError::Failed(format!("document not found: {document_id}")))?;
    begin_processing(&ctx.pool, &document_id).await?;
    let artifact = resolve_job_artifact(&ctx.pool, &document_id, payload.artifact_id.as_deref())
        .await?;
    match read_artifact_bytes(fs, &artifact)? {
        Some(bytes) => Ok(Some((document_id, artifact, bytes))),
        None => Ok(None),
    }
}

/// Records the failure-side bookkeeping when a handler returns `Err`:
/// on the final attempt (`attempts + 1 >= max_attempts`) the document
/// gets `processing_status = "failed"` and a `ProcessingFailed` audit
/// event; intermediate retries stay `running` — the job row remains
/// the authoritative retry state.
async fn note_failure(ctx: &JobContext, job: &Job, document_id: Option<&DocumentId>) {
    let Some(document_id) = document_id else { return };
    if job.attempts.saturating_add(1) < job.max_attempts {
        return;
    }
    let _ = finish_processing(
        &ctx.pool,
        document_id,
        "processing",
        op_status::FAILED,
        DocumentAuditEventType::ProcessingFailed,
    )
    .await;
}

/// `document.metadata_extract` — sniffs the target artifact and, for
/// PDFs, writes title/author/producer/page-count into document
/// metadata. Non-PDF artifacts are recorded `unsupported`.
pub struct DocumentMetadataExtractHandler {
    storage: StorageConfig,
}

impl DocumentMetadataExtractHandler {
    /// Builds the handler from the application's storage configuration.
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

impl JobHandler for DocumentMetadataExtractHandler {
    fn handle<'a>(
        &'a self,
        ctx: &'a JobContext,
        job: &'a Job,
    ) -> std::pin::Pin<Box<dyn std::future::Future<Output = Result<(), JobError>> + Send + 'a>>
    {
        Box::pin(async move {
            let mut fs = storage_fs(&self.storage)?;
            let input = load_job_input(ctx, &mut fs, job).await;
            let (document_id, _artifact, bytes) = match input {
                Ok(Some(input)) => input,
                Ok(None) => {
                    // Too-large artifact: still need the document id for
                    // status bookkeeping — re-parse cheaply.
                    let (_, document_id) =
                        DocumentJobPayload::parse(&job.payload)?;
                    let id = DocumentId::new(document_id.document_id.clone());
                    finish_processing(
                        &ctx.pool,
                        &id,
                        "metadata_extract",
                        op_status::SKIPPED_TOO_LARGE,
                        DocumentAuditEventType::ProcessingCompleted,
                    )
                    .await?;
                    return Ok(());
                }
                Err(e) => {
                    note_failure(ctx, job, None).await;
                    return Err(e);
                }
            };

            let result = extract_metadata_into_store(ctx, &document_id, &bytes).await;
            match result {
                Ok(()) => {
                    finish_processing(
                        &ctx.pool,
                        &document_id,
                        "metadata_extract",
                        op_status::SUCCEEDED,
                        DocumentAuditEventType::ProcessingCompleted,
                    )
                    .await
                }
                Err(e) if is_unsupported_format(&e) => {
                    finish_processing(
                        &ctx.pool,
                        &document_id,
                        "metadata_extract",
                        op_status::UNSUPPORTED,
                        DocumentAuditEventType::ProcessingCompleted,
                    )
                    .await
                }
                Err(e) => {
                    note_failure(ctx, job, Some(&document_id)).await;
                    Err(e)
                }
            }
        })
    }
}

/// Runs the PDF metadata extraction and merges the result into the
/// document's stored metadata (existing custom fields are preserved).
async fn extract_metadata_into_store(
    ctx: &JobContext,
    document_id: &DocumentId,
    bytes: &[u8],
) -> Result<(), JobError> {
    if sniff_format(bytes) != SniffedFormat::Pdf {
        return Err(unsupported_format());
    }

    let info = crate::crates::pdf_engine::PdfEngine::new()
        .inspect(bytes)
        .map_err(|e| JobError::Failed(format!("pdf inspect failed: {e}")))?;

    let mut metadata = get_document_metadata(&ctx.pool, document_id)
        .await
        .map_err(store_err)?
        .unwrap_or_else(|| DocumentMetadata {
            document_id: document_id.clone(),
            title: None,
            author: None,
            subject: None,
            language: None,
            keywords: Vec::new(),
            custom_fields: serde_json::Map::new(),
        });

    // PDF Info-dict fields win when present; existing values are kept
    // otherwise so re-runs never erase metadata set by other tools.
    if info.title.is_some() {
        metadata.title = info.title.clone();
    }
    if info.author.is_some() {
        metadata.author = info.author.clone();
    }
    metadata.custom_fields.insert(
        "format".to_string(),
        json!("pdf"),
    );
    metadata.custom_fields.insert(
        "page_count".to_string(),
        json!(info.page_count),
    );
    metadata.custom_fields.insert(
        "encrypted".to_string(),
        json!(info.encrypted),
    );
    metadata.custom_fields.insert(
        "file_size_bytes".to_string(),
        json!(info.file_size_bytes),
    );
    if let Some(v) = &info.creator {
        metadata.custom_fields.insert("creator".to_string(), json!(v));
    }
    if let Some(v) = &info.producer {
        metadata.custom_fields.insert("producer".to_string(), json!(v));
    }

    upsert_document_metadata(&ctx.pool, &metadata)
        .await
        .map_err(store_err)
}

/// `document.index` — extracts the PDF text layer into the `index`
/// metadata custom field (excerpt + sha256 + counts). Non-PDF
/// artifacts are recorded `unsupported`.
pub struct DocumentIndexHandler {
    storage: StorageConfig,
}

impl DocumentIndexHandler {
    /// Builds the handler from the application's storage configuration.
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

impl JobHandler for DocumentIndexHandler {
    fn handle<'a>(
        &'a self,
        ctx: &'a JobContext,
        job: &'a Job,
    ) -> std::pin::Pin<Box<dyn std::future::Future<Output = Result<(), JobError>> + Send + 'a>>
    {
        Box::pin(async move {
            let mut fs = storage_fs(&self.storage)?;
            let input = load_job_input(ctx, &mut fs, job).await;
            let (document_id, _artifact, bytes) = match input {
                Ok(Some(input)) => input,
                Ok(None) => {
                    let (_, document_id) = DocumentJobPayload::parse(&job.payload)?;
                    let id = DocumentId::new(document_id.document_id.clone());
                    finish_processing(
                        &ctx.pool,
                        &id,
                        "index",
                        op_status::SKIPPED_TOO_LARGE,
                        DocumentAuditEventType::ProcessingCompleted,
                    )
                    .await?;
                    return Ok(());
                }
                Err(e) => {
                    note_failure(ctx, job, None).await;
                    return Err(e);
                }
            };

            let result = index_document_text(ctx, &document_id, &bytes).await;
            match result {
                Ok(()) => {
                    finish_processing(
                        &ctx.pool,
                        &document_id,
                        "index",
                        op_status::SUCCEEDED,
                        DocumentAuditEventType::ProcessingCompleted,
                    )
                    .await
                }
                Err(e) => {
                    if is_unsupported_format(&e) {
                        finish_processing(
                            &ctx.pool,
                            &document_id,
                            "index",
                            op_status::UNSUPPORTED,
                            DocumentAuditEventType::ProcessingCompleted,
                        )
                        .await
                    } else {
                        note_failure(ctx, job, Some(&document_id)).await;
                        Err(e)
                    }
                }
            }
        })
    }
}

/// Marker prefix for the "format not supported" job error.
const UNSUPPORTED_FORMAT: &str = "unsupported document format";

fn unsupported_format() -> JobError {
    JobError::Failed(UNSUPPORTED_FORMAT.to_string())
}

fn is_unsupported_format(e: &JobError) -> bool {
    matches!(e, JobError::Failed(msg) if msg == UNSUPPORTED_FORMAT)
}

/// Extracts the PDF text layer and stores it under the `index` custom
/// field: `{excerpt, sha256, characters, words}`. The excerpt is capped
/// at [`INDEX_EXCERPT_CHARS`]; counts/hash always cover the full text.
async fn index_document_text(
    ctx: &JobContext,
    document_id: &DocumentId,
    bytes: &[u8],
) -> Result<(), JobError> {
    if sniff_format(bytes) != SniffedFormat::Pdf {
        return Err(unsupported_format());
    }

    let pages = crate::crates::pdf_engine::PdfEngine::new()
        .extract_text(bytes, None)
        .map_err(|e| JobError::Failed(format!("pdf text extraction failed: {e}")))?;

    let text = pages
        .iter()
        .map(|p| p.text.as_str())
        .collect::<Vec<_>>()
        .join("\n\n");
    let words = text.split_whitespace().count() as u64;
    let characters = text.chars().count() as u64;
    let sha256 = hex_lower(&Sha256::digest(text.as_bytes()));
    let excerpt: String = text.chars().take(INDEX_EXCERPT_CHARS).collect();

    set_custom_field(
        &ctx.pool,
        document_id,
        "index",
        &json!({
            "excerpt": excerpt,
            "sha256": sha256,
            "characters": characters,
            "words": words,
        }),
    )
    .await
    .map_err(store_err)?;
    Ok(())
}

/// `thumbnail.generate` — extracts the document's embedded thumbnail
/// (page `/Thumb` or first image XObject) and stores it under
/// `thumbnails/{document_id}/thumbnail.{ext}`.
pub struct ThumbnailGenerateHandler {
    storage: StorageConfig,
}

impl ThumbnailGenerateHandler {
    /// Builds the handler from the application's storage configuration.
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

impl JobHandler for ThumbnailGenerateHandler {
    fn handle<'a>(
        &'a self,
        ctx: &'a JobContext,
        job: &'a Job,
    ) -> std::pin::Pin<Box<dyn std::future::Future<Output = Result<(), JobError>> + Send + 'a>>
    {
        Box::pin(async move {
            let mut fs = storage_fs(&self.storage)?;
            let input = load_job_input(ctx, &mut fs, job).await;
            let (document_id, _artifact, bytes) = match input {
                Ok(Some(input)) => input,
                Ok(None) => {
                    let (_, document_id) = DocumentJobPayload::parse(&job.payload)?;
                    let id = DocumentId::new(document_id.document_id.clone());
                    finish_processing(
                        &ctx.pool,
                        &id,
                        "thumbnail",
                        op_status::SKIPPED_TOO_LARGE,
                        DocumentAuditEventType::ProcessingCompleted,
                    )
                    .await?;
                    return Ok(());
                }
                Err(e) => {
                    note_failure(ctx, job, None).await;
                    return Err(e);
                }
            };

            match generate_thumbnail(ctx, &mut fs, &document_id, &bytes).await {
                Ok(()) => {
                    finish_processing(
                        &ctx.pool,
                        &document_id,
                        "thumbnail",
                        op_status::SUCCEEDED,
                        DocumentAuditEventType::ProcessingCompleted,
                    )
                    .await
                }
                Err(ThumbnailOutcome::Unavailable) => {
                    finish_processing(
                        &ctx.pool,
                        &document_id,
                        "thumbnail",
                        op_status::UNAVAILABLE,
                        DocumentAuditEventType::ProcessingCompleted,
                    )
                    .await
                }
                Err(ThumbnailOutcome::Unsupported) => {
                    finish_processing(
                        &ctx.pool,
                        &document_id,
                        "thumbnail",
                        op_status::UNSUPPORTED,
                        DocumentAuditEventType::ProcessingCompleted,
                    )
                    .await
                }
                Err(ThumbnailOutcome::Failed(e)) => {
                    note_failure(ctx, job, Some(&document_id)).await;
                    Err(e)
                }
            }
        })
    }
}

/// Outcome of a thumbnail attempt that isn't a plain success/failure.
enum ThumbnailOutcome {
    /// No embedded thumbnail could be found or decoded.
    Unavailable,
    /// The artifact isn't a PDF.
    Unsupported,
    /// A real error (storage, parse, encode).
    Failed(JobError),
}

impl From<JobError> for ThumbnailOutcome {
    fn from(e: JobError) -> Self {
        Self::Failed(e)
    }
}

/// Extracts and stores the document thumbnail. Deterministic object
/// name → retries overwrite the same bytes (idempotent).
async fn generate_thumbnail(
    ctx: &JobContext,
    fs: &mut FaultInjectingFs,
    document_id: &DocumentId,
    bytes: &[u8],
) -> Result<(), ThumbnailOutcome> {
    if sniff_format(bytes) != SniffedFormat::Pdf {
        return Err(ThumbnailOutcome::Unsupported);
    }
    let (image, ext) =
        extract_pdf_thumbnail(bytes).map_err(ThumbnailOutcome::Failed)?
            .ok_or(ThumbnailOutcome::Unavailable)?;

    let name = format!("thumbnails/{document_id}/thumbnail{ext}");
    let mut writer = fs
        .begin_upload(&name)
        .map_err(|e| ThumbnailOutcome::Failed(JobError::Failed(format!("storage init failed: {e}"))))?;
    for chunk in image.chunks(UPLOAD_CHUNK_BYTES) {
        writer
            .write_chunk(chunk)
            .map_err(|e| ThumbnailOutcome::Failed(JobError::Failed(format!("thumbnail upload failed: {e}"))))?;
    }
    writer
        .commit()
        .map_err(|e| ThumbnailOutcome::Failed(JobError::Failed(format!("thumbnail commit failed: {e}"))))?;

    set_custom_field(
        &ctx.pool,
        document_id,
        "thumbnail_uri",
        &json!(format!("local://{name}")),
    )
    .await
    .map_err(|e| ThumbnailOutcome::Failed(store_err(e)))?;
    Ok(())
}

/// Finds and decodes the best embedded thumbnail in a PDF: the first
/// page's `/Thumb` entry, falling back to the first image XObject in
/// that page's resources. Returns `(encoded_bytes, extension)`.
fn extract_pdf_thumbnail(pdf: &[u8]) -> Result<Option<(Vec<u8>, &'static str)>, JobError> {
    let doc = lopdf::Document::load_mem(pdf)
        .map_err(|e| JobError::Failed(format!("pdf parse failed: {e}")))?;
    if doc.trailer.has(b"Encrypt") {
        return Ok(None);
    }
    let pages = doc.get_pages();
    let Some(first_page_id) = pages.values().next().copied() else {
        return Ok(None);
    };
    let Ok(page) = doc.get_dictionary(first_page_id) else {
        return Ok(None);
    };

    // 1) Explicit /Thumb entry on the page.
    if let Ok(thumb) = page.get(b"Thumb")
        && let Ok((_, obj)) = doc.dereference(thumb)
        && let Ok(stream) = obj.as_stream()
        && let Some(found) = image_stream_to_file(stream)?
    {
        return Ok(Some(found));
    }

    // 2) First image XObject in the page's /Resources.
    if let Ok(resources) = page.get(b"Resources")
        && let Ok((_, res_obj)) = doc.dereference(resources)
        && let Ok(res_dict) = res_obj.as_dict()
        && let Ok(xobjects) = res_dict.get(b"XObject")
        && let Ok((_, xo_obj)) = doc.dereference(xobjects)
        && let Ok(xo_dict) = xo_obj.as_dict()
    {
        for (_, xobj) in xo_dict.iter() {
            if let Ok((_, obj)) = doc.dereference(xobj)
                && let Ok(stream) = obj.as_stream()
                && stream
                    .dict
                    .get(b"Subtype")
                    .is_ok_and(|s| s.as_name() == Ok(b"Image"))
                && let Some(found) = image_stream_to_file(stream)?
            {
                return Ok(Some(found));
            }
        }
    }
    Ok(None)
}

/// Converts one PDF image stream into a storable file: already-encoded
/// formats (JPEG/JPX/CCITT) pass through with their extension; raw
/// sample data (FlateDecode) is decoded to PNG via the `image` crate.
fn image_stream_to_file(
    stream: &lopdf::Stream,
) -> Result<Option<(Vec<u8>, &'static str)>, JobError> {
    // Filter may be a name or an array of names; anything more complex
    // (multiple filters, decode parms we don't model) is declined.
    let filter = match stream.dict.get(b"Filter") {
        Ok(lopdf::Object::Name(n)) => Some(n.as_slice().to_vec()),
        Ok(lopdf::Object::Array(items)) if items.len() == 1 => items[0]
            .as_name()
            .ok()
            .map(|n| n.as_slice().to_vec()),
        Ok(_) => return Ok(None),
        Err(_) => None,
    };

    // Already-file formats: the (still-filtered) stream content is the
    // file itself.
    if let Some(name) = filter.as_deref() {
        let ext = match name {
            b"DCTDecode" => Some(".jpg"),
            b"JPXDecode" => Some(".jp2"),
            b"CCITTFaxDecode" => Some(".ccitt"),
            b"JBIG2Decode" => Some(".jbig2"),
            _ => None,
        };
        if let Some(ext) = ext {
            if stream.content.is_empty() || stream.content.len() > MAX_THUMBNAIL_DECODED_BYTES {
                return Ok(None);
            }
            return Ok(Some((stream.content.clone(), ext)));
        }
    }

    // Raw sample path: only plain FlateDecode (or unfiltered) streams
    // with trivial decode parms are supported.
    let flate_only = filter.is_none() || filter.as_deref() == Some(b"FlateDecode");
    if !flate_only {
        return Ok(None);
    }
    if stream
        .dict
        .get(b"DecodeParms")
        .is_ok_and(|dp| dp.as_dict().is_ok_and(|d| d.get(b"Predictor").is_ok()))
    {
        // Row predictors change the sample layout — decoding them is
        // out of scope; report "no usable thumbnail" honestly.
        return Ok(None);
    }

    let width = stream.dict.get(b"Width").ok().and_then(|v| v.as_i64().ok())
        .filter(|&w| w > 0)
        .map(|w| w as u32)
        .ok_or_else(|| JobError::Failed("thumbnail stream missing /Width".into()))?;
    let height = stream.dict.get(b"Height").ok().and_then(|v| v.as_i64().ok())
        .filter(|&h| h > 0)
        .map(|h| h as u32)
        .ok_or_else(|| JobError::Failed("thumbnail stream missing /Height".into()))?;
    let bpc = stream
        .dict
        .get(b"BitsPerComponent")
        .ok()
        .and_then(|v| v.as_i64().ok())
        .unwrap_or(8);
    if bpc != 8 {
        return Ok(None);
    }
    let components = color_space_components(stream)?;
    let Some(components) = components else {
        return Ok(None);
    };
    if u64::from(width) * u64::from(height) > MAX_THUMBNAIL_PIXELS {
        return Ok(None);
    }

    let samples = stream
        .decompressed_content_with_limit(MAX_THUMBNAIL_DECODED_BYTES)
        .map_err(|e| JobError::Failed(format!("thumbnail decode failed: {e}")))?;
    let expected = width as usize * height as usize * components;
    if samples.len() < expected {
        return Ok(None);
    }
    let samples = &samples[..expected];

    let mut png = std::io::Cursor::new(Vec::new());
    match components {
        1 => {
            let img = image::GrayImage::from_raw(width, height, samples.to_vec())
                .ok_or_else(|| JobError::Failed("thumbnail buffer size mismatch".into()))?;
            image::DynamicImage::ImageLuma8(img)
                .write_to(&mut png, image::ImageFormat::Png)
                .map_err(|e| JobError::Failed(format!("thumbnail png encode failed: {e}")))?;
        }
        3 => {
            let img = image::RgbImage::from_raw(width, height, samples.to_vec())
                .ok_or_else(|| JobError::Failed("thumbnail buffer size mismatch".into()))?;
            image::DynamicImage::ImageRgb8(img)
                .write_to(&mut png, image::ImageFormat::Png)
                .map_err(|e| JobError::Failed(format!("thumbnail png encode failed: {e}")))?;
        }
        _ => return Ok(None),
    }
    Ok(Some((png.into_inner(), ".png")))
}

/// Maps a PDF `/ColorSpace` entry to a component count for 8-bpc
/// device spaces. Returns `None` for anything else (ICCBased,
/// Indexed, Separation, Pattern, ...).
fn color_space_components(stream: &lopdf::Stream) -> Result<Option<usize>, JobError> {
    let cs = match stream.dict.get(b"ColorSpace") {
        Ok(v) => v,
        Err(_) => return Ok(Some(1)), // image XObjects default to DeviceGray
    };
    let name = match cs {
        lopdf::Object::Name(n) => Some(n.as_slice()),
        lopdf::Object::Array(items) => items
            .first()
            .and_then(|o| o.as_name().ok())
            .map(|n| n.as_slice()),
        _ => None,
    };
    Ok(match name {
        Some(b"DeviceGray") | Some(b"G") => Some(1),
        Some(b"DeviceRGB") | Some(b"RGB") => Some(3),
        _ => None,
    })
}

// ---------------------------------------------------------------------------
// enqueue_* producer helpers
// ---------------------------------------------------------------------------

/// Builds the `document.ingest` payload, enqueues the job, and stamps
/// `processing_status = "queued"` (best-effort). Bytes are base64'd
/// into `content_b64`.
pub async fn enqueue_ingest(
    pool: &sqlx::SqlitePool,
    document_id: &DocumentId,
    file_name: &str,
    mime_type: Option<&str>,
    bytes: &[u8],
    default_max_attempts: u32,
) -> Result<Job, JobError> {
    let job = enqueue(
        pool,
        &NewJob {
            kind: DOCUMENT_INGEST_KIND.to_string(),
            payload: json!({
                "document_id": document_id.as_str(),
                "content_b64": B64.encode(bytes),
                "file_name": file_name,
                "mime_type": mime_type,
            }),
            priority: 0,
            run_at: None,
            max_attempts: None,
        },
        default_max_attempts,
    )
    .await?;
    stamp_queued(pool, document_id).await;
    Ok(job)
}

/// Enqueues `document.metadata_extract` for `document_id`
/// (`artifact_id` optional — most recent artifact when `None`).
pub async fn enqueue_metadata_extract(
    pool: &sqlx::SqlitePool,
    document_id: &DocumentId,
    artifact_id: Option<&str>,
    default_max_attempts: u32,
) -> Result<Job, JobError> {
    enqueue_document_op(
        pool,
        DOCUMENT_METADATA_EXTRACT_KIND,
        document_id,
        artifact_id,
        default_max_attempts,
    )
    .await
}

/// Enqueues `document.index` for `document_id`.
pub async fn enqueue_index(
    pool: &sqlx::SqlitePool,
    document_id: &DocumentId,
    artifact_id: Option<&str>,
    default_max_attempts: u32,
) -> Result<Job, JobError> {
    enqueue_document_op(
        pool,
        DOCUMENT_INDEX_KIND,
        document_id,
        artifact_id,
        default_max_attempts,
    )
    .await
}

/// Enqueues `thumbnail.generate` for `document_id`.
pub async fn enqueue_thumbnail(
    pool: &sqlx::SqlitePool,
    document_id: &DocumentId,
    artifact_id: Option<&str>,
    default_max_attempts: u32,
) -> Result<Job, JobError> {
    enqueue_document_op(
        pool,
        THUMBNAIL_GENERATE_KIND,
        document_id,
        artifact_id,
        default_max_attempts,
    )
    .await
}

async fn enqueue_document_op(
    pool: &sqlx::SqlitePool,
    kind: &str,
    document_id: &DocumentId,
    artifact_id: Option<&str>,
    default_max_attempts: u32,
) -> Result<Job, JobError> {
    let mut payload = json!({ "document_id": document_id.as_str() });
    if let Some(id) = artifact_id {
        payload["artifact_id"] = json!(id);
    }
    let job = enqueue(
        pool,
        &NewJob {
            kind: kind.to_string(),
            payload,
            priority: 0,
            run_at: None,
            max_attempts: None,
        },
        default_max_attempts,
    )
    .await?;
    stamp_queued(pool, document_id).await;
    Ok(job)
}

/// Best-effort `processing_status = "queued"` stamp — producers must
/// never fail just because the status field couldn't be written.
async fn stamp_queued(pool: &sqlx::SqlitePool, document_id: &DocumentId) {
    if let Err(e) = set_custom_field(pool, document_id, "processing_status", &json!("queued"))
        .await
    {
        tracing::warn!(error = %e, "failed to stamp processing_status=queued");
    }
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

    // -----------------------------------------------------------------
    // document.metadata_extract / document.index / thumbnail.generate
    // -----------------------------------------------------------------

    use crate::crates::document_engine::store::metadata::get_document_metadata;

    /// Engine with every built-in document handler registered against
    /// an explicit storage root (no app config involved).
    fn full_engine(pool: SqlitePool, root: &std::path::Path) -> JobEngine {
        let storage = || StorageConfig::new(root);
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
            Arc::new(DocumentIngestHandler::with_storage_config(storage())),
        );
        eng.register(
            DOCUMENT_METADATA_EXTRACT_KIND,
            Arc::new(DocumentMetadataExtractHandler::with_storage_config(storage())),
        );
        eng.register(
            DOCUMENT_INDEX_KIND,
            Arc::new(DocumentIndexHandler::with_storage_config(storage())),
        );
        eng.register(
            THUMBNAIL_GENERATE_KIND,
            Arc::new(ThumbnailGenerateHandler::with_storage_config(storage())),
        );
        eng
    }

    /// Creates a document, ingests `bytes` as `name`, and returns its id.
    async fn ingest_doc(
        eng: &JobEngine,
        pool: &SqlitePool,
        doc_id: &DocumentId,
        bytes: &[u8],
        name: &str,
        mime: &str,
    ) {
        enqueue_ingest(pool, doc_id, Some(name), Some(mime), bytes)
            .await
            .unwrap();
        assert!(eng.run_once().await.unwrap());
    }

    /// Builds a real one-page PDF with extractable text and an Info
    /// dict, using lopdf (same approach as pdf-engine's test_utils).
    fn test_pdf_bytes() -> Vec<u8> {
        use lopdf::content::{Content, Operation};
        use lopdf::{Object, Stream, dictionary};

        let mut doc = lopdf::Document::with_version("1.5");
        let pages_id = doc.new_object_id();
        let font_id = doc.add_object(dictionary! {
            "Type" => "Font",
            "Subtype" => "Type1",
            "BaseFont" => "Courier",
        });
        let content = Content {
            operations: vec![
                Operation::new("BT", vec![]),
                Operation::new("Tf", vec!["F1".into(), 12.into()]),
                Operation::new("Td", vec![100.into(), 700.into()]),
                Operation::new("Tj", vec![Object::string_literal("Hello KalaDoc")]),
                Operation::new("ET", vec![]),
            ],
        };
        let content_id = doc.add_object(Stream::new(dictionary! {}, content.encode().unwrap()));
        let resources_id = doc.add_object(dictionary! {
            "Font" => dictionary! { "F1" => font_id },
        });
        let page_id = doc.add_object(dictionary! {
            "Type" => "Page",
            "Parent" => pages_id,
            "Contents" => content_id,
            "Resources" => resources_id,
            "MediaBox" => vec![0.into(), 0.into(), 595.into(), 842.into()],
        });
        doc.objects.insert(
            pages_id,
            Object::Dictionary(dictionary! {
                "Type" => "Pages",
                "Kids" => vec![Object::Reference(page_id)],
                "Count" => 1,
            }),
        );
        let catalog_id = doc.add_object(dictionary! {
            "Type" => "Catalog",
            "Pages" => pages_id,
        });
        let info_id = doc.add_object(dictionary! {
            "Title" => Object::string_literal("Job Handler Test"),
            "Author" => Object::string_literal("KalaDoc Tests"),
        });
        doc.trailer.set("Root", catalog_id);
        doc.trailer.set("Info", info_id);
        let mut buf = Vec::new();
        doc.save_to(&mut buf).unwrap();
        buf
    }

    /// Like [`test_pdf_bytes`] but adds a 2x2 DeviceRGB image XObject —
    /// used to exercise real thumbnail extraction.
    fn test_pdf_with_image_bytes() -> Vec<u8> {
        use lopdf::content::{Content, Operation};
        use lopdf::{Object, Stream, dictionary};

        let mut doc = lopdf::Document::with_version("1.5");
        let pages_id = doc.new_object_id();
        let font_id = doc.add_object(dictionary! {
            "Type" => "Font",
            "Subtype" => "Type1",
            "BaseFont" => "Courier",
        });
        // 2x2 RGB pixels, uncompressed.
        let img = Stream::new(
            dictionary! {
                "Type" => "XObject",
                "Subtype" => "Image",
                "Width" => 2,
                "Height" => 2,
                "ColorSpace" => "DeviceRGB",
                "BitsPerComponent" => 8,
            },
            vec![
                255, 0, 0, 0, 255, 0, //
                0, 0, 255, 255, 255, 0,
            ],
        );
        let img_id = doc.add_object(img);
        let content = Content {
            operations: vec![
                Operation::new("BT", vec![]),
                Operation::new("Tf", vec!["F1".into(), 12.into()]),
                Operation::new("Td", vec![100.into(), 700.into()]),
                Operation::new("Tj", vec![Object::string_literal("doc with image")]),
                Operation::new("ET", vec![]),
            ],
        };
        let content_id = doc.add_object(Stream::new(dictionary! {}, content.encode().unwrap()));
        let resources_id = doc.add_object(dictionary! {
            "Font" => dictionary! { "F1" => font_id },
            "XObject" => dictionary! { "Im1" => img_id },
        });
        let page_id = doc.add_object(dictionary! {
            "Type" => "Page",
            "Parent" => pages_id,
            "Contents" => content_id,
            "Resources" => resources_id,
            "MediaBox" => vec![0.into(), 0.into(), 595.into(), 842.into()],
        });
        doc.objects.insert(
            pages_id,
            Object::Dictionary(dictionary! {
                "Type" => "Pages",
                "Kids" => vec![Object::Reference(page_id)],
                "Count" => 1,
            }),
        );
        let catalog_id = doc.add_object(dictionary! {
            "Type" => "Catalog",
            "Pages" => pages_id,
        });
        doc.trailer.set("Root", catalog_id);
        let mut buf = Vec::new();
        doc.save_to(&mut buf).unwrap();
        buf
    }

    async fn metadata_of(pool: &SqlitePool, doc_id: &DocumentId) -> DocumentMetadata {
        get_document_metadata(pool, doc_id)
            .await
            .unwrap()
            .expect("metadata row")
    }

    #[tokio::test]
    async fn metadata_extract_pdf_records_info() {
        let pool = test_pool().await;
        let root = temp_root();
        let doc_id = make_document(&pool).await;
        let eng = full_engine(pool.clone(), &root);
        ingest_doc(&eng, &pool, &doc_id, &test_pdf_bytes(), "report.pdf", "application/pdf").await;

        enqueue_metadata_extract(&pool, &doc_id, None).await.unwrap();
        assert!(eng.run_once().await.unwrap());

        let meta = metadata_of(&pool, &doc_id).await;
        let cf = &meta.custom_fields;
        assert_eq!(cf["metadata_extract_status"], json!("extracted"));
        assert_eq!(cf["processing_status"], json!("succeeded"));
        assert_eq!(cf["detected_format"], json!("pdf"));
        assert_eq!(cf["mime_type"], json!("application/pdf"));
        assert_eq!(cf["page_count"], json!(1));
        assert_eq!(cf["pdf_title"], json!("Job Handler Test"));
        assert!(cf["metadata_extracted_at"].is_string());
        // Title/author columns filled from the Info dict (were unset).
        assert_eq!(meta.title.as_deref(), Some("Job Handler Test"));
        assert_eq!(meta.author.as_deref(), Some("KalaDoc Tests"));

        // Audit: started + completed events.
        let history = get_document_history(&pool, &doc_id).await.unwrap();
        let types: Vec<_> = history.iter().map(|e| e.event_type).collect();
        assert!(types.contains(&DocumentAuditEventType::ProcessingStarted));
        assert!(types.contains(&DocumentAuditEventType::ProcessingCompleted));

        // Job succeeded.
        let jobs = list_jobs(&pool, Some(JobStatus::Succeeded), 10).await.unwrap();
        assert_eq!(jobs.len(), 2); // ingest + extract
        std::fs::remove_dir_all(&root).ok();
    }

    #[tokio::test]
    async fn metadata_extract_unsupported_format_is_honest() {
        let pool = test_pool().await;
        let root = temp_root();
        let doc_id = make_document(&pool).await;
        let eng = full_engine(pool.clone(), &root);
        // PNG signature bytes — recognized but not PDF-extractable.
        let png = b"\x89PNG\r\n\x1a\nrest-of-file";
        ingest_doc(&eng, &pool, &doc_id, png, "img.png", "image/png").await;

        enqueue_metadata_extract(&pool, &doc_id, None).await.unwrap();
        assert!(eng.run_once().await.unwrap());

        let meta = metadata_of(&pool, &doc_id).await;
        assert_eq!(meta.custom_fields["metadata_extract_status"], json!("unsupported"));
        assert_eq!(meta.custom_fields["processing_status"], json!("unsupported"));
        assert_eq!(meta.custom_fields["detected_format"], json!("png"));
        assert_eq!(meta.custom_fields["mime_type"], json!("image/png"));
        std::fs::remove_dir_all(&root).ok();
    }

    #[tokio::test]
    async fn metadata_extract_missing_document_fails_job() {
        let pool = test_pool().await;
        let root = temp_root();
        let eng = full_engine(pool.clone(), &root);
        let ghost = DocumentId::new("ghost-doc");

        enqueue_metadata_extract(&pool, &ghost, None).await.unwrap();
        // max_attempts default is 3 — run all attempts.
        for _ in 0..3 {
            assert!(eng.run_once().await.unwrap());
        }
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
    async fn index_pdf_extracts_text() {
        let pool = test_pool().await;
        let root = temp_root();
        let doc_id = make_document(&pool).await;
        let eng = full_engine(pool.clone(), &root);
        ingest_doc(&eng, &pool, &doc_id, &test_pdf_bytes(), "report.pdf", "application/pdf").await;

        enqueue_index(&pool, &doc_id, None).await.unwrap();
        assert!(eng.run_once().await.unwrap());

        let meta = metadata_of(&pool, &doc_id).await;
        assert_eq!(meta.custom_fields["index_status"], json!("indexed"));
        assert_eq!(meta.custom_fields["processing_status"], json!("succeeded"));
        let index = &meta.custom_fields["index"];
        assert_eq!(index["status"], json!("indexed"));
        assert_eq!(index["format"], json!("pdf"));
        assert!(index["char_count"].as_u64().unwrap() > 0);
        assert!(index["word_count"].as_u64().unwrap() > 0);
        assert_eq!(index["sha256"].as_str().unwrap().len(), 64);
        assert!(index["excerpt"].as_str().unwrap().contains("Hello KalaDoc"));

        let history = get_document_history(&pool, &doc_id).await.unwrap();
        assert!(
            history
                .iter()
                .any(|e| e.event_type == DocumentAuditEventType::Indexed)
        );
        std::fs::remove_dir_all(&root).ok();
    }

    #[tokio::test]
    async fn index_plain_text_document() {
        let pool = test_pool().await;
        let root = temp_root();
        let doc_id = make_document(&pool).await;
        let eng = full_engine(pool.clone(), &root);
        ingest_doc(
            &eng,
            &pool,
            &doc_id,
            b"the quick brown fox jumps over the lazy dog",
            "notes.txt",
            "text/plain",
        )
        .await;

        enqueue_index(&pool, &doc_id, None).await.unwrap();
        assert!(eng.run_once().await.unwrap());

        let meta = metadata_of(&pool, &doc_id).await;
        assert_eq!(meta.custom_fields["index_status"], json!("indexed"));
        let index = &meta.custom_fields["index"];
        assert_eq!(index["char_count"], json!(43));
        assert_eq!(index["word_count"], json!(9));
        assert!(index["excerpt"].as_str().unwrap().contains("quick brown fox"));
        std::fs::remove_dir_all(&root).ok();
    }

    #[tokio::test]
    async fn index_unsupported_format_is_honest() {
        let pool = test_pool().await;
        let root = temp_root();
        let doc_id = make_document(&pool).await;
        let eng = full_engine(pool.clone(), &root);
        // Valid PNG signature + non-UTF8 payload → not indexable.
        let bytes: &[u8] = b"\x89PNG\r\n\x1a\n\xff\xfe\x00\x01binary";
        ingest_doc(&eng, &pool, &doc_id, bytes, "img.png", "image/png").await;

        enqueue_index(&pool, &doc_id, None).await.unwrap();
        assert!(eng.run_once().await.unwrap());

        let meta = metadata_of(&pool, &doc_id).await;
        assert_eq!(meta.custom_fields["index_status"], json!("unsupported"));
        assert_eq!(meta.custom_fields["processing_status"], json!("unsupported"));
        assert!(meta.custom_fields.get("index").is_none());
        // Job itself succeeded.
        let jobs = list_jobs(&pool, Some(JobStatus::Succeeded), 10).await.unwrap();
        assert_eq!(jobs.len(), 2);
        std::fs::remove_dir_all(&root).ok();
    }

    #[tokio::test]
    async fn thumbnail_generated_from_image_xobject() {
        let pool = test_pool().await;
        let root = temp_root();
        let doc_id = make_document(&pool).await;
        let eng = full_engine(pool.clone(), &root);
        ingest_doc(
            &eng,
            &pool,
            &doc_id,
            &test_pdf_with_image_bytes(),
            "withimg.pdf",
            "application/pdf",
        )
        .await;

        enqueue_thumbnail(&pool, &doc_id, None).await.unwrap();
        assert!(eng.run_once().await.unwrap());

        let meta = metadata_of(&pool, &doc_id).await;
        assert_eq!(meta.custom_fields["thumbnail_status"], json!("generated"));
        assert_eq!(meta.custom_fields["processing_status"], json!("succeeded"));
        assert_eq!(meta.custom_fields["thumbnail_format"], json!("png"));
        assert_eq!(meta.custom_fields["thumbnail_page"], json!(1));

        // Thumbnail bytes really landed in storage as a PNG.
        let arts = list_artifacts(&pool, &doc_id).await.unwrap();
        let uri = meta.custom_fields["thumbnail_uri"].as_str().unwrap().to_string();
        assert!(uri.starts_with("local://documents/"));
        assert!(uri.contains("/thumbnails/"));
        assert!(uri.ends_with(".png"));
        let object_name = uri.trim_start_matches("local://");
        let on_disk = root.join(object_name);
        let bytes = std::fs::read(&on_disk).unwrap();
        assert_eq!(&bytes[..8], b"\x89PNG\r\n\x1a\n");
        assert_eq!(
            meta.custom_fields["thumbnail_bytes"].as_u64().unwrap(),
            bytes.len() as u64
        );
        assert_eq!(arts.len(), 1); // thumbnail is not an artifact row

        let history = get_document_history(&pool, &doc_id).await.unwrap();
        assert!(
            history
                .iter()
                .any(|e| e.event_type == DocumentAuditEventType::ThumbnailGenerated)
        );
        std::fs::remove_dir_all(&root).ok();
    }

    #[tokio::test]
    async fn thumbnail_unavailable_without_embedded_image() {
        let pool = test_pool().await;
        let root = temp_root();
        let doc_id = make_document(&pool).await;
        let eng = full_engine(pool.clone(), &root);
        ingest_doc(&eng, &pool, &doc_id, &test_pdf_bytes(), "plain.pdf", "application/pdf").await;

        enqueue_thumbnail(&pool, &doc_id, None).await.unwrap();
        assert!(eng.run_once().await.unwrap());

        let meta = metadata_of(&pool, &doc_id).await;
        assert_eq!(meta.custom_fields["thumbnail_status"], json!("unavailable"));
        assert_eq!(meta.custom_fields["processing_status"], json!("unavailable"));
        assert!(meta.custom_fields.get("thumbnail_uri").is_none());
        std::fs::remove_dir_all(&root).ok();
    }

    #[tokio::test]
    async fn thumbnail_unsupported_for_non_pdf() {
        let pool = test_pool().await;
        let root = temp_root();
        let doc_id = make_document(&pool).await;
        let eng = full_engine(pool.clone(), &root);
        ingest_doc(&eng, &pool, &doc_id, b"plain text", "a.txt", "text/plain").await;

        enqueue_thumbnail(&pool, &doc_id, None).await.unwrap();
        assert!(eng.run_once().await.unwrap());

        let meta = metadata_of(&pool, &doc_id).await;
        assert_eq!(meta.custom_fields["thumbnail_status"], json!("unsupported"));
        std::fs::remove_dir_all(&root).ok();
    }

    #[tokio::test]
    async fn processing_status_transitions_queued_running_succeeded() {
        let pool = test_pool().await;
        let root = temp_root();
        let doc_id = make_document(&pool).await;
        let eng = full_engine(pool.clone(), &root);
        ingest_doc(&eng, &pool, &doc_id, &test_pdf_bytes(), "r.pdf", "application/pdf").await;

        // Enqueue helper stamps "queued".
        enqueue_metadata_extract(&pool, &doc_id, None).await.unwrap();
        let meta = metadata_of(&pool, &doc_id).await;
        assert_eq!(meta.custom_fields["processing_status"], json!("queued"));
        assert_eq!(
            meta.custom_fields["metadata_extract_status"],
            json!("queued")
        );

        assert!(eng.run_once().await.unwrap());
        let meta = metadata_of(&pool, &doc_id).await;
        assert_eq!(meta.custom_fields["processing_status"], json!("succeeded"));
        assert_eq!(meta.custom_fields["metadata_extract_status"], json!("extracted"));

        // Audit ordering: started before completed.
        let history = get_document_history(&pool, &doc_id).await.unwrap();
        let pos = |t: DocumentAuditEventType| {
            history.iter().position(|e| e.event_type == t)
        };
        let started = pos(DocumentAuditEventType::ProcessingStarted).unwrap();
        let completed = pos(DocumentAuditEventType::ProcessingCompleted).unwrap();
        assert!(started < completed);
        std::fs::remove_dir_all(&root).ok();
    }

    #[tokio::test]
    async fn processing_jobs_are_idempotent_on_rerun() {
        let pool = test_pool().await;
        let root = temp_root();
        let doc_id = make_document(&pool).await;
        let eng = full_engine(pool.clone(), &root);
        ingest_doc(&eng, &pool, &doc_id, &test_pdf_with_image_bytes(), "i.pdf", "application/pdf").await;

        // Run each processing job twice — second run must succeed and
        // leave a single consistent state (no duplicate artifacts,
        // deterministic thumbnail path).
        for kind_payload in [
            (DOCUMENT_METADATA_EXTRACT_KIND, json!({"document_id": doc_id.as_str()})),
            (DOCUMENT_INDEX_KIND, json!({"document_id": doc_id.as_str()})),
            (THUMBNAIL_GENERATE_KIND, json!({"document_id": doc_id.as_str()})),
        ] {
            for _ in 0..2 {
                eng.enqueue(&NewJob {
                    kind: kind_payload.0.into(),
                    payload: kind_payload.1.clone(),
                    priority: 0,
                    run_at: None,
                    max_attempts: None,
                })
                .await
                .unwrap();
                assert!(eng.run_once().await.unwrap());
            }
        }

        assert_eq!(list_artifacts(&pool, &doc_id).await.unwrap().len(), 1);
        let meta = metadata_of(&pool, &doc_id).await;
        assert_eq!(meta.custom_fields["metadata_extract_status"], json!("extracted"));
        assert_eq!(meta.custom_fields["index_status"], json!("indexed"));
        assert_eq!(meta.custom_fields["thumbnail_status"], json!("generated"));
        // Exactly one thumbnail object on disk.
        let thumbs = root.join("documents").join(doc_id.as_str()).join("thumbnails");
        let entries: Vec<_> = std::fs::read_dir(&thumbs).unwrap().collect();
        assert_eq!(entries.len(), 1);
        std::fs::remove_dir_all(&root).ok();
    }

    #[tokio::test]
    async fn crash_recovery_reexecutes_processing_job() {
        let pool = test_pool().await;
        let root = temp_root();
        let doc_id = make_document(&pool).await;
        let eng = full_engine(pool.clone(), &root);
        ingest_doc(&eng, &pool, &doc_id, &test_pdf_bytes(), "c.pdf", "application/pdf").await;

        enqueue_index(&pool, &doc_id, None).await.unwrap();
        let job = list_jobs(&pool, None, 10).await.unwrap()
            .into_iter()
            .find(|j| j.kind == DOCUMENT_INDEX_KIND)
            .unwrap();

        // Simulate a worker that claimed the job then died.
        sqlx::query(
            "UPDATE jobs_tbl SET status = 'running', worker_id = 'dead-worker',
             started_at = ?, attempts = 1 WHERE id = ?",
        )
        .bind(std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_secs() as i64)
        .bind(job.id)
        .execute(&pool)
        .await
        .unwrap();

        assert_eq!(eng.recover().await.unwrap(), 1);
        assert!(eng.run_once().await.unwrap());

        let meta = metadata_of(&pool, &doc_id).await;
        assert_eq!(meta.custom_fields["index_status"], json!("indexed"));
        std::fs::remove_dir_all(&root).ok();
    }
}
