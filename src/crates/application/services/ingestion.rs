//! Document ingestion service — the full upload pipeline in one place.
//!
//! [`DocumentIngestionService`] coordinates what used to be scattered
//! across adapters and job handlers: an async byte stream goes in,
//! bounded chunks are staged through the storage engine's
//! [`StreamingBackend`], the object is published atomically with a
//! SHA-256 receipt, and the document store is populated (document,
//! artifact, version 1, metadata, audit event) only *after* the bytes
//! are durable.
//!
//! # Pipeline
//!
//! ```text
//! AsyncRead ──peek 512B──▶ sniff format ──▶ create Document row
//!      │                                          │
//!      └──feed chunks──▶ mpsc ──▶ spawn_blocking ──▶ FaultInjectingFs
//!                          │            begin_upload → write_chunk…
//!                          │            → commit (rename + sha256)
//!                          ▼
//!              CommitReceipt { name, bytes, sha256 }
//!                          │
//!              upload_artifact → create_version(1) → metadata
//!              → Imported audit event → tags → folder
//! ```
//!
//! # Crash-safety ordering (why the document row comes first)
//!
//! The object name is `documents/{document_id}/{file_name}`, so the
//! [`DocumentId`] must exist before staging begins — hence
//! [`DocumentService::create_document`] runs *before* any bytes move.
//! The store functions used for registration do not accept a
//! transaction, so the phases are sequenced with explicit
//! compensation instead:
//!
//! - **Before commit**: any failure (bad input, oversize, IO, injected
//!   fault, reader error, caller cancellation) leaves only a staged
//!   `.part` file — removed by `abort`/`Drop` — plus the document row,
//!   which [`DocumentIngestionService::recover`] collects as an
//!   "incomplete document" once it ages past the grace period.
//! - **After commit, during registration**: the object is deleted via
//!   [`StorageBackend::delete`] and the partial document is trashed +
//!   purged (the store only allows purging trashed rows; the cascade
//!   removes artifacts, versions, metadata, tags and audit rows).
//!   Both steps are best-effort — [`recover`] is the backstop for
//!   anything compensation couldn't finish (e.g. the process died
//!   mid-rollback).
//!
//! # Cancellation
//!
//! The future returned by [`ingest`](DocumentIngestionService::ingest)
//! is cancel-safe: dropping it drops the chunk sender, the blocking
//! writer sees the channel close, aborts the staged upload, and removes
//! the `.part` file. The already-created document row remains until
//! [`recover`](DocumentIngestionService::recover) purges it — there is
//! no way to run async cleanup from a dropped future, so recovery is
//! deliberately deferred to the sweeper. A committed-but-unregistered
//! object is likewise left for `recover`'s unreferenced-object pass.
//!
//! # Blocking storage I/O
//!
//! [`StreamingBackend`] is a synchronous API (matching the rest of the
//! storage engine). The whole staged write — `begin_upload`,
//! `write_chunk` loop, `commit` — runs inside **one**
//! `tokio::task::spawn_blocking` call because the upload writer borrows
//! its backend mutably for its whole lifetime (the GAT makes that
//! borrow explicit). Chunks cross from the async reader to the blocking
//! writer through a bounded `tokio::sync::mpsc` channel, so the caller's
//! `AsyncRead` never blocks a runtime worker and memory stays bounded
//! (channel capacity × `max_chunk_bytes`).
//!
//! # Recovery semantics ([`recover`](DocumentIngestionService::recover))
//!
//! `recover` is conservative — it only repairs what it can *prove* is
//! inconsistent, and never deletes bytes that any artifact row still
//! references (copied documents share `storage_uri`s, so reference
//! counting is global, not per-document):
//!
//! 1. `sweep_staging(max_age)` removes abandoned `.part` files.
//! 2. Documents with **no artifacts** older than
//!    `incomplete_document_grace` are trashed + purged (failed or
//!    cancelled ingests).
//! 3. Documents with an artifact whose `local://` object is **missing**
//!    on disk are trashed + purged — the document is unusable and the
//!    purge cascades its rows. Objects are *not* deleted here; they
//!    become unreferenced and are handled by step 4 on a later pass.
//! 4. Files under `documents/` not referenced by *any* artifact row
//!    and older than `unreferenced_object_grace` are deleted (GC for
//!    committed-but-never-registered objects). The grace period keeps
//!    the commit→artifact-registration window safe.
//!
//! Everything is reported in a [`RecoveryReport`]; per-item failures
//! are collected in `errors` rather than aborting the sweep.

use std::collections::{HashMap, HashSet};
use std::path::Path;
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use serde_json::json;
use sqlx::SqlitePool;
use tokio::io::{AsyncRead, AsyncReadExt};
use tokio::sync::mpsc;

use crate::crates::document_engine::artifact::{
    ChecksumAlgorithm, DocumentArtifact, DocumentChecksum,
};
use crate::crates::document_engine::audit::DocumentAuditEventType;
use crate::crates::document_engine::document::{
    Document, DocumentOrigin, DocumentSource, DocumentStatus, DocumentType,
};
use crate::crates::document_engine::ids::{DocumentId, FolderId};
use crate::crates::document_engine::metadata::DocumentMetadata;
use crate::crates::document_engine::store::documents::NewDocument;
use crate::crates::document_engine::store::metadata::upsert_document_metadata;
use crate::crates::document_engine::store::{StoreError, now_rfc3339};
use crate::crates::document_engine::version::DocumentVersion;
use crate::crates::security_engine::{SniffedFormat, sniff_format};
use crate::crates::storage::local::config::StorageConfig;
use crate::crates::storage::local::debug_when_full::FaultInjectingFs;
use crate::crates::storage::{CommitReceipt, StreamLimits, StreamUpload, StreamingBackend};

use super::documents::{DocumentService, NewArtifact};
use super::error::ServiceError;

/// Bytes buffered up-front for format sniffing. Large enough for every
/// magic signature the sniffer knows (longest is the 8-byte PNG sig).
pub const SNIFF_BYTES: usize = 512;

/// Default per-object cap applied by [`DocumentIngestionService::new`]:
/// 512 MiB. The backend-wide `max_size_bytes` quota still applies on
/// top. Override via [`DocumentIngestionService::with_limits`].
pub const DEFAULT_MAX_OBJECT_BYTES: u64 = 512 * 1024 * 1024;

/// Channel capacity (in chunks) between the async reader and the
/// blocking upload writer. Bounds in-flight memory to
/// `capacity × max_chunk_bytes`.
const FEED_CHANNEL_CAPACITY: usize = 4;

/// Cap on the async read buffer. Never exceeds the configured
/// `max_chunk_bytes`, and never exceeds the storage engine's default
/// chunk cap so oversized `StreamLimits` can't force huge allocations.
const MAX_READ_BUFFER: usize = 8 * 1024 * 1024;

/// Input describing one ingestion. See [`DocumentIngestionService::ingest`].
#[derive(Debug, Clone)]
pub struct IngestRequest {
    /// Owning user/organization id. Must be non-empty.
    pub owner_id: String,
    /// Suggested file name. Sanitized with the same rules as the
    /// `document.ingest` job handler (no `/`, `\`, or control
    /// characters; not `.`/`..`; trimmed). Also becomes the document
    /// name and the final path component of the storage object.
    pub file_name: String,
    /// Caller-declared MIME type. Recorded on the artifact when set;
    /// the *detected* type from magic bytes is always stored in
    /// metadata regardless.
    pub mime_type: Option<String>,
    /// Folder to place the document in. Validated during registration
    /// (after commit) — an unknown folder fails the ingest and rolls
    /// back, matching the store's `move_document` semantics.
    pub folder_id: Option<FolderId>,
    /// Tags to assign to the document (created on first use).
    pub tags: Vec<String>,
    /// Actor recorded on audit events and used as version `created_by`.
    /// Defaults to `owner_id` when unset.
    pub actor_id: Option<String>,
    /// Metadata title override. Defaults to the file-name stem.
    pub title: Option<String>,
}

impl IngestRequest {
    /// Minimal request: owner + file name; everything else defaulted.
    pub fn new(owner_id: impl Into<String>, file_name: impl Into<String>) -> Self {
        Self {
            owner_id: owner_id.into(),
            file_name: file_name.into(),
            mime_type: None,
            folder_id: None,
            tags: Vec::new(),
            actor_id: None,
            title: None,
        }
    }
}

/// Result of a successful [`DocumentIngestionService::ingest`].
#[derive(Debug)]
pub struct IngestOutcome {
    /// The registered document (status `Active`, detected type).
    pub document: Document,
    /// The artifact row pointing at the committed object.
    pub artifact: DocumentArtifact,
    /// Version 1, marked current, pointing at `artifact`.
    pub version: DocumentVersion,
    /// `local://documents/{document_id}/{file_name}` — the value stored
    /// on the artifact row.
    pub storage_uri: String,
    /// Committed object size in bytes (from the `CommitReceipt`).
    pub size_bytes: u64,
    /// Lowercase hex SHA-256 of the committed bytes.
    pub sha256: String,
    /// MIME type detected from magic bytes (before any caller-declared
    /// override). `"application/octet-stream"` when unrecognized.
    pub detected_mime_type: String,
}

/// Tunables for [`DocumentIngestionService::recover`].
#[derive(Debug, Clone, Copy)]
pub struct RecoveryOptions {
    /// Minimum age of a staged `.part` file before `sweep_staging`
    /// removes it. Protects uploads currently in flight.
    pub staging_max_age: Duration,
    /// Minimum age of an artifact-less document before it is purged.
    /// Must comfortably exceed the longest expected ingest so a
    /// concurrent upload is never collected mid-flight.
    pub incomplete_document_grace: Duration,
    /// Minimum age of an unreferenced object under `documents/` before
    /// it is deleted. Covers the commit→artifact-registration window.
    pub unreferenced_object_grace: Duration,
}

impl Default for RecoveryOptions {
    fn default() -> Self {
        let hour = Duration::from_secs(3600);
        Self {
            staging_max_age: hour,
            incomplete_document_grace: hour,
            unreferenced_object_grace: hour,
        }
    }
}

/// Summary of one [`DocumentIngestionService::recover`] pass.
#[derive(Debug, Clone, Default)]
pub struct RecoveryReport {
    /// Abandoned staged files removed by `sweep_staging`.
    pub staging_entries_removed: usize,
    /// Document rows examined.
    pub documents_checked: usize,
    /// Artifact rows examined.
    pub artifacts_checked: usize,
    /// Artifact rows whose `local://` object was missing on disk.
    pub artifacts_missing_objects: usize,
    /// Documents trashed + purged (artifact-less stale rows and rows
    /// with missing objects).
    pub documents_purged: usize,
    /// Unreferenced files under `documents/` deleted by the GC pass.
    pub unreferenced_objects_removed: usize,
    /// Non-fatal per-item failures encountered along the way.
    pub errors: Vec<String>,
}

/// Async facade coordinating staged storage uploads with document
/// registration. Cheap to clone; construct from a pool +
/// [`StorageConfig`].
#[derive(Clone)]
pub struct DocumentIngestionService {
    documents: DocumentService,
    storage_config: StorageConfig,
    limits: StreamLimits,
}

impl DocumentIngestionService {
    /// Builds the service with [`DEFAULT_MAX_OBJECT_BYTES`] as the
    /// per-object cap and the default chunk cap.
    pub fn new(pool: SqlitePool, storage_config: StorageConfig) -> Self {
        Self::with_limits(
            pool,
            storage_config,
            StreamLimits::new(Some(DEFAULT_MAX_OBJECT_BYTES)),
        )
    }

    /// Builds the service with explicit [`StreamLimits`]. Pass
    /// `max_object_bytes: None` for no per-object cap (the backend
    /// quota still applies).
    pub fn with_limits(
        pool: SqlitePool,
        storage_config: StorageConfig,
        limits: StreamLimits,
    ) -> Self {
        // A zero chunk cap would reject every chunk; floor it at 1.
        let limits = StreamLimits {
            max_chunk_bytes: limits.max_chunk_bytes.max(1),
            ..limits
        };
        Self {
            documents: DocumentService::new(pool),
            storage_config,
            limits,
        }
    }

    /// The document service used for registration (handy for callers
    /// that need to inspect the outcome afterwards).
    pub fn documents(&self) -> &DocumentService {
        &self.documents
    }

    /// The streaming limits enforced on every ingest.
    pub fn limits(&self) -> StreamLimits {
        self.limits
    }

    /// Ingests `reader` end-to-end. See the [module docs](self) for the
    /// pipeline, ordering, and cancellation guarantees.
    ///
    /// # Errors
    /// - [`ServiceError::Store`]`(Validation)` for bad input (empty
    ///   owner/file name, unsafe file name, empty upload, oversize is
    ///   [`ServiceError::Storage`]`(TooLarge)`), unknown folder, or
    ///   store failures — always after best-effort rollback.
    /// - [`ServiceError::Io`] for reader failures.
    /// - [`ServiceError::Storage`] for backend/quota/fault failures.
    pub async fn ingest<R>(
        &self,
        mut reader: R,
        request: IngestRequest,
    ) -> Result<IngestOutcome, ServiceError>
    where
        R: AsyncRead + Unpin,
    {
        // -- 1. Validate everything knowable before touching storage --
        let owner_id = request.owner_id.trim();
        if owner_id.is_empty() {
            return Err(validation("owner_id", "must not be empty"));
        }
        let file_name = sanitize_file_name(&request.file_name)?;
        let mime_type = request
            .mime_type
            .as_deref()
            .map(str::trim)
            .filter(|s| !s.is_empty())
            .map(str::to_string);

        // -- 2. Peek at the head for format detection --
        let mut prefix = vec![0u8; SNIFF_BYTES];
        let mut filled = 0usize;
        while filled < SNIFF_BYTES {
            match reader.read(&mut prefix[filled..]).await {
                Ok(0) => break,
                Ok(n) => filled += n,
                Err(e) => return Err(ServiceError::Io(e)),
            }
        }
        prefix.truncate(filled);
        if prefix.is_empty() {
            return Err(validation("content", "upload must not be empty"));
        }
        let (doc_type, detected_mime, sniffed) = detect_content(&prefix);

        // -- 3. Document row FIRST: the object name embeds its id ----
        let doc = self
            .documents
            .create_document(&NewDocument {
                name: file_name.clone(),
                document_type: doc_type,
                owner_id: owner_id.to_string(),
                source: DocumentSource {
                    origin: DocumentOrigin::Import,
                    uri: None,
                    external_reference: None,
                    imported_at: Some(now_rfc3339()),
                },
            })
            .await?;
        let object_name = format!("documents/{}/{}", doc.id, file_name);
        let storage_uri = format!("local://{object_name}");

        // From here on the document row exists: every failure must
        // roll back (purge rows; delete the object if it was published).
        let receipt = match self.stage_upload(&object_name, prefix, reader).await {
            Ok(receipt) => receipt,
            Err(e) => {
                // The writer guarantees nothing was published on Err,
                // but delete defensively — a previous same-name object
                // can't exist (id is fresh), so this is a no-op safety.
                self.compensate(&doc.id, &object_name).await;
                return Err(e);
            }
        };

        // -- 4. Register rows; any failure rolls back fully ----------
        let outcome = self
            .register(
                &doc,
                &request,
                owner_id,
                &file_name,
                &storage_uri,
                &receipt,
                detected_mime,
                sniffed,
                mime_type,
            )
            .await;
        match outcome {
            Ok(outcome) => Ok(outcome),
            Err(e) => {
                self.compensate(&doc.id, &object_name).await;
                Err(e)
            }
        }
    }

    /// Runs `recover` with default [`RecoveryOptions`].
    pub async fn recover(&self) -> Result<RecoveryReport, ServiceError> {
        self.recover_with(&RecoveryOptions::default()).await
    }

    /// Executes one recovery pass; see the [module docs](self) for the
    /// exact repair semantics.
    ///
    /// # Errors
    /// [`ServiceError::Storage`] if the backend can't be constructed or
    /// `sweep_staging` itself fails; DB listing failures surface as
    /// [`ServiceError::Store`]. Per-item problems are collected in
    /// [`RecoveryReport::errors`] instead of aborting the pass.
    pub async fn recover_with(
        &self,
        options: &RecoveryOptions,
    ) -> Result<RecoveryReport, ServiceError> {
        let mut report = RecoveryReport::default();

        // 1. Abandoned staged uploads.
        let cfg = self.storage_config.clone();
        let staging_max_age = options.staging_max_age;
        report.staging_entries_removed = spawn_blocking_joined(move || {
            let mut fs = FaultInjectingFs::new(cfg)?;
            Ok(fs.sweep_staging(staging_max_age)?)
        })
        .await?;

        // 2. Snapshot the document/artifact tables.
        let docs = self
            .documents
            .list_documents(None, None, true, i64::MAX)
            .await?;
        report.documents_checked = docs.len();

        let mut referenced: HashSet<String> = HashSet::new();
        // (doc index, artifact id, object name) needing an exists() check.
        let mut checks: Vec<(usize, String, String)> = Vec::new();
        let mut artifact_counts: Vec<usize> = Vec::with_capacity(docs.len());
        for (i, doc) in docs.iter().enumerate() {
            let artifacts = self.documents.list_artifacts(&doc.id).await?;
            artifact_counts.push(artifacts.len());
            report.artifacts_checked += artifacts.len();
            for art in artifacts {
                if let Some(object) = local_object_name(&art.storage_uri) {
                    referenced.insert(object.clone());
                    checks.push((i, art.id.clone(), object));
                }
            }
        }

        // 3. One blocking pass for all existence checks.
        let mut missing_by_doc: HashMap<usize, usize> = HashMap::new();
        if !checks.is_empty() {
            let cfg = self.storage_config.clone();
            let names: Vec<String> = checks.iter().map(|(_, _, n)| n.clone()).collect();
            let results = spawn_blocking_joined(move || {
                let mut fs = FaultInjectingFs::new(cfg)?;
                let mut out = Vec::with_capacity(names.len());
                for name in &names {
                    out.push(fs.exists(name)?);
                }
                Ok::<_, ServiceError>(out)
            })
            .await?;

            for ((doc_idx, artifact_id, object), exists) in checks.iter().zip(results) {
                if !exists {
                    report.artifacts_missing_objects += 1;
                    *missing_by_doc.entry(*doc_idx).or_default() += 1;
                    report.errors.push(format!(
                        "document {} artifact {artifact_id} points at missing object {object}",
                        docs[*doc_idx].id
                    ));
                }
            }
        }

        // 4. Purge inconsistent documents (rows only — their surviving
        //    objects become unreferenced and are GC'd by step 5 on a
        //    later pass once they age past grace).
        for (i, doc) in docs.iter().enumerate() {
            let has_missing = missing_by_doc.contains_key(&i);
            let stale_empty = artifact_counts[i] == 0
                && older_than(&doc.created_at, options.incomplete_document_grace);
            if !has_missing && !stale_empty {
                continue;
            }
            match self.purge_rows(&doc.id).await {
                Ok(true) => report.documents_purged += 1,
                Ok(false) => {}
                Err(e) => report
                    .errors
                    .push(format!("failed to purge document {}: {e}", doc.id)),
            }
        }

        // 5. GC unreferenced objects under documents/ (global reference
        //    set — shared storage_uris stay alive). `referenced` was
        //    computed pre-purge, so objects of just-purged documents
        //    survive this pass and are collected on the next one.
        let root = self.storage_config.root_dir.clone();
        let objects = spawn_blocking_joined(move || -> Result<Vec<DiskObject>, ServiceError> {
            Ok(walk_objects(&root))
        })
        .await?;
        if !objects.is_empty() {
            let cfg = self.storage_config.clone();
            let grace = options.unreferenced_object_grace;
            report.unreferenced_objects_removed = spawn_blocking_joined(move || {
                let mut fs = FaultInjectingFs::new(cfg)?;
                let mut removed = 0usize;
                for object in objects {
                    if referenced.contains(&object.name) {
                        continue;
                    }
                    let stale = object
                        .modified
                        .and_then(|t| t.elapsed().ok())
                        .is_some_and(|age| age >= grace);
                    if !stale {
                        continue;
                    }
                    match fs.delete(&object.name) {
                        Ok(()) => removed += 1,
                        Err(e) => {
                            tracing::warn!(object = %object.name, error = %e, "recover: object delete failed");
                        }
                    }
                }
                Ok::<_, ServiceError>(removed)
            })
            .await?;
        }

        Ok(report)
    }

    // ------------------------------------------------------------------
    // Internals
    // ------------------------------------------------------------------

    /// Streams `reader` into a staged upload inside one
    /// `spawn_blocking`, feeding chunks through a bounded channel.
    /// Returns the [`CommitReceipt`] on success.
    ///
    /// On any error the writer has already discarded the staged file;
    /// on channel close without a finish marker (caller dropped the
    /// future) the writer aborts itself.
    async fn stage_upload<R>(
        &self,
        object_name: &str,
        prefix: Vec<u8>,
        mut reader: R,
    ) -> Result<CommitReceipt, ServiceError>
    where
        R: AsyncRead + Unpin,
    {
        let (tx, rx) = mpsc::channel::<Option<Vec<u8>>>(FEED_CHANNEL_CAPACITY);

        let config = self.storage_config.clone();
        let limits = self.limits;
        let name = object_name.to_string();
        let writer = tokio::task::spawn_blocking(move || -> Result<CommitReceipt, ServiceError> {
            let mut rx = rx;
            let mut fs = FaultInjectingFs::new(config)?;
            fs = fs.with_stream_limits(limits);
            let mut upload = fs.begin_upload(&name)?;

            // The sniffed prefix is the first chunk; slice it to the
            // chunk cap so tiny `max_chunk_bytes` configs still work.
            let chunk_cap = limits.max_chunk_bytes.max(1);
            for piece in prefix.chunks(chunk_cap) {
                upload.write_chunk(piece)?;
            }

            let receipt = loop {
                match rx.blocking_recv() {
                    Some(Some(chunk)) => upload.write_chunk(&chunk)?,
                    Some(None) => break upload.commit()?,
                    // Sender dropped without a finish marker: caller
                    // went away — discard the staged bytes.
                    None => {
                        let _ = upload.abort();
                        return Err(validation("stream", "upload cancelled by caller"));
                    }
                }
            };
            Ok(receipt)
        });

        // Feed loop: read → send. A failed send means the writer died
        // (limit/fault); its error is authoritative and surfaces when
        // we await the handle.
        let read_cap = self.limits.max_chunk_bytes.clamp(1, MAX_READ_BUFFER);
        let mut buf = vec![0u8; read_cap];
        let mut stream_error = None;
        loop {
            match reader.read(&mut buf).await {
                Ok(0) => {
                    // End of stream: finish marker, then close.
                    let _ = tx.send(None).await;
                    break;
                }
                Ok(n) => {
                    if tx.send(Some(buf[..n].to_vec())).await.is_err() {
                        break;
                    }
                }
                Err(e) => {
                    stream_error = Some(e);
                    break;
                }
            }
        }
        drop(tx);

        if let Some(e) = stream_error {
            // Reader failed: the writer sees the closed channel and
            // aborts. Wait for it so staging is gone before we return.
            let _ = writer.await;
            return Err(ServiceError::Io(e));
        }

        writer
            .await
            .map_err(|e| ServiceError::Io(std::io::Error::other(e)))?
    }

    /// Registration phase after a successful commit. Order matters:
    /// artifact → version → metadata → audit → tags → folder. The
    /// store records its own audit events for artifact/version/
    /// metadata; `Imported` is recorded explicitly last among the
    /// row writes so it only lands on fully-registered documents.
    #[allow(clippy::too_many_arguments)]
    async fn register(
        &self,
        doc: &Document,
        request: &IngestRequest,
        owner_id: &str,
        file_name: &str,
        storage_uri: &str,
        receipt: &CommitReceipt,
        detected_mime: &'static str,
        sniffed: SniffedFormat,
        declared_mime: Option<String>,
    ) -> Result<IngestOutcome, ServiceError> {
        let actor = request.actor_id.as_deref().unwrap_or(owner_id);
        let artifact_mime = declared_mime
            .clone()
            .unwrap_or_else(|| detected_mime.to_string());

        let artifact = self
            .documents
            .upload_artifact(&NewArtifact {
                id: None,
                document_id: doc.id.clone(),
                storage_uri: storage_uri.to_string(),
                file_name: file_name.to_string(),
                mime_type: Some(artifact_mime.clone()),
                size_bytes: receipt.bytes,
                checksum: Some(DocumentChecksum {
                    algorithm: ChecksumAlgorithm::Sha256,
                    value: receipt.sha256.clone(),
                }),
            })
            .await?;

        let version = self
            .documents
            .create_version(&doc.id, Some(&artifact.id), actor, Some("initial import"))
            .await?;

        let title = request.title.clone().or_else(|| file_stem(file_name));
        let mut custom_fields = serde_json::Map::new();
        custom_fields.insert("detected_format".into(), json!(sniffed.label()));
        custom_fields.insert("detected_mime_type".into(), json!(detected_mime));
        custom_fields.insert("size_bytes".into(), json!(receipt.bytes));
        custom_fields.insert("sha256".into(), json!(receipt.sha256));
        if let Some(declared) = &declared_mime {
            custom_fields.insert("declared_mime_type".into(), json!(declared));
        }
        upsert_document_metadata(
            self.documents.pool(),
            &DocumentMetadata {
                document_id: doc.id.clone(),
                title,
                author: None,
                subject: None,
                language: None,
                keywords: request.tags.clone(),
                custom_fields,
            },
        )
        .await?;

        self.documents
            .record_event(
                &doc.id,
                request.actor_id.as_deref(),
                DocumentAuditEventType::Imported,
                Some(&json!({
                    "artifact_id": artifact.id,
                    "storage_uri": storage_uri,
                    "size_bytes": receipt.bytes,
                    "sha256": receipt.sha256,
                    "detected_format": sniffed.label(),
                    "detected_mime_type": detected_mime,
                })),
            )
            .await?;

        for tag in &request.tags {
            if tag.trim().is_empty() {
                continue;
            }
            self.documents
                .add_tag(&doc.id, tag, request.actor_id.as_deref())
                .await?;
        }

        if let Some(folder_id) = &request.folder_id {
            self.documents
                .move_document(&doc.id, Some(folder_id))
                .await?;
        }

        // Re-read so folder_id reflects the move.
        let document = self
            .documents
            .get_document(&doc.id)
            .await?
            .unwrap_or_else(|| doc.clone());

        Ok(IngestOutcome {
            document,
            artifact,
            version,
            storage_uri: storage_uri.to_string(),
            size_bytes: receipt.bytes,
            sha256: receipt.sha256.clone(),
            detected_mime_type: detected_mime.to_string(),
        })
    }

    /// Best-effort rollback after a failed ingest: delete the committed
    /// object (no-op when nothing was published) and purge the partial
    /// document. Errors are logged, never propagated — `recover` is the
    /// backstop for whatever this can't finish.
    async fn compensate(&self, document_id: &DocumentId, object_name: &str) {
        let config = self.storage_config.clone();
        let name = object_name.to_string();
        let doc_id_string = document_id.as_str().to_string();
        let object_result = tokio::task::spawn_blocking(move || -> Result<(), ServiceError> {
            let mut fs = FaultInjectingFs::new(config)?;
            fs.delete(&name)?;
            // Remove the per-document directory too, but only when
            // empty — `remove_dir` fails on non-empty dirs, which is
            // exactly the guard we want (other artifacts' objects, or
            // objects shared with copied documents, keep it alive).
            let _ = std::fs::remove_dir(fs.root_dir().join("documents").join(&doc_id_string));
            Ok(())
        })
        .await;
        match object_result {
            Ok(Ok(())) => {}
            Ok(Err(e)) => {
                tracing::warn!(document = %document_id, error = %e, "ingest rollback: object delete failed");
            }
            Err(e) => {
                tracing::warn!(document = %document_id, error = %e, "ingest rollback: object delete task failed");
            }
        }

        if let Err(e) = self.purge_rows(document_id).await {
            tracing::warn!(document = %document_id, error = %e, "ingest rollback: document purge failed");
        }
    }

    /// Trashes (when needed) and permanently purges a document.
    /// Returns `Ok(false)` when the document doesn't exist.
    async fn purge_rows(&self, id: &DocumentId) -> Result<bool, ServiceError> {
        match self.documents.get_document(id).await? {
            None => return Ok(false),
            Some(doc) if doc.status != DocumentStatus::Trashed => {
                self.documents.delete_document(id).await?;
            }
            Some(_) => {}
        }
        self.documents.purge_document(id).await
    }
}

// ---------------------------------------------------------------------------
// Free helpers
// ---------------------------------------------------------------------------

/// `spawn_blocking` + join-error mapping shared by `recover_with`.
async fn spawn_blocking_joined<T, F>(f: F) -> Result<T, ServiceError>
where
    F: FnOnce() -> Result<T, ServiceError> + Send + 'static,
    T: Send + 'static,
{
    tokio::task::spawn_blocking(f)
        .await
        .map_err(|e| ServiceError::Io(std::io::Error::other(e)))?
}

/// Store-validation error without echoing untrusted input.
fn validation(field: &'static str, reason: impl Into<String>) -> ServiceError {
    ServiceError::Store(StoreError::validation(field, reason))
}

/// Same rules as the `document.ingest` job handler: trimmed, non-empty,
/// not `.`/`..`, no `/` or `\`, no control characters.
fn sanitize_file_name(name: &str) -> Result<String, ServiceError> {
    let trimmed = name.trim();
    let bad = trimmed.is_empty()
        || trimmed == "."
        || trimmed == ".."
        || trimmed.contains('/')
        || trimmed.contains('\\')
        || trimmed.chars().any(|c| c.is_control());
    if bad {
        return Err(validation(
            "file_name",
            "must be a non-empty bare file name (no path separators or control characters)",
        ));
    }
    Ok(trimmed.to_string())
}

/// Maps a magic-byte sniff to `(DocumentType, mime, SniffedFormat)`.
/// Unknown binary → `Other`/`application/octet-stream`; unknown but
/// plausible UTF-8 text → `Text`/`text/plain`.
fn detect_content(head: &[u8]) -> (DocumentType, &'static str, SniffedFormat) {
    match sniff_format(head) {
        SniffedFormat::Pdf => (DocumentType::Pdf, "application/pdf", SniffedFormat::Pdf),
        SniffedFormat::Png => (DocumentType::Image, "image/png", SniffedFormat::Png),
        SniffedFormat::Jpeg => (DocumentType::Image, "image/jpeg", SniffedFormat::Jpeg),
        SniffedFormat::Gif => (DocumentType::Image, "image/gif", SniffedFormat::Gif),
        SniffedFormat::Zip => (DocumentType::Archive, "application/zip", SniffedFormat::Zip),
        SniffedFormat::Unknown => {
            if looks_like_text(head) {
                (DocumentType::Text, "text/plain", SniffedFormat::Unknown)
            } else {
                (
                    DocumentType::Other,
                    "application/octet-stream",
                    SniffedFormat::Unknown,
                )
            }
        }
    }
}

/// Conservative text check: no NUL and no C0 controls other than
/// tab/LF/CR; bytes ≥ 0x80 are allowed (UTF-8 continuation/lead).
fn looks_like_text(bytes: &[u8]) -> bool {
    bytes
        .iter()
        .all(|&b| b == b'\t' || b == b'\n' || b == b'\r' || (0x20..=0x7e).contains(&b) || b >= 0x80)
}

/// `local://documents/x/y` → `documents/x/y`; `None` for other schemes.
fn local_object_name(storage_uri: &str) -> Option<String> {
    storage_uri
        .strip_prefix("local://")
        .filter(|s| !s.is_empty())
        .map(str::to_string)
}

/// File-name stem for the default metadata title (`report.pdf` →
/// `report`). Falls back to the full name when there's no usable stem.
fn file_stem(file_name: &str) -> Option<String> {
    let stem = file_name
        .rsplit_once('.')
        .map(|(s, _)| s)
        .unwrap_or(file_name);
    let stem = stem.trim();
    (!stem.is_empty()).then(|| stem.to_string())
}

/// One regular file found under `<root>/documents/`.
struct DiskObject {
    /// Confined object name, e.g. `documents/{id}/{file}`.
    name: String,
    /// File mtime, used for the unreferenced-object grace check.
    modified: Option<SystemTime>,
}

/// Walks `<root>/documents/` collecting regular files as object names.
/// Blocking; run inside `spawn_blocking`.
fn walk_objects(root: &Path) -> Vec<DiskObject> {
    let mut out = Vec::new();
    let Ok(docs) = std::fs::read_dir(root.join("documents")) else {
        return out;
    };
    for doc_entry in docs.flatten() {
        let doc_path = doc_entry.path();
        if !doc_path.is_dir() {
            continue;
        }
        let Some(doc_id) = doc_entry.file_name().to_str().map(str::to_string) else {
            continue;
        };
        let Ok(files) = std::fs::read_dir(&doc_path) else {
            continue;
        };
        for file in files.flatten() {
            let file_path = file.path();
            if !file_path.is_file() {
                continue;
            }
            let file_name_os = file.file_name();
            let Some(file_name) = file_name_os.to_str() else {
                continue;
            };
            out.push(DiskObject {
                name: format!("documents/{doc_id}/{file_name}"),
                modified: file.metadata().ok().and_then(|m| m.modified().ok()),
            });
        }
    }
    out
}

/// Whether an RFC 3339 timestamp (the store's fixed
/// `YYYY-MM-DDTHH:MM:SSZ` format) is older than `grace`. Unparseable or
/// future timestamps are treated as *not* older — conservative: never
/// purge what we can't age.
fn older_than(timestamp: &str, grace: Duration) -> bool {
    match rfc3339_epoch_secs(timestamp) {
        Some(secs) => {
            let now = SystemTime::now()
                .duration_since(UNIX_EPOCH)
                .map(|d| d.as_secs())
                .unwrap_or(0);
            now.saturating_sub(secs) >= grace.as_secs()
        }
        None => false,
    }
}

/// Parses the store's `YYYY-MM-DDTHH:MM:SSZ` timestamps to epoch secs.
fn rfc3339_epoch_secs(s: &str) -> Option<u64> {
    let b = s.as_bytes();
    if b.len() != 20
        || b[4] != b'-'
        || b[7] != b'-'
        || b[10] != b'T'
        || b[13] != b':'
        || b[16] != b':'
        || b[19] != b'Z'
    {
        return None;
    }
    let num = |lo: usize, hi: usize| -> Option<i64> { s.get(lo..hi)?.parse::<i64>().ok() };
    let (y, mo, d) = (num(0, 4)?, num(5, 7)?, num(8, 10)?);
    let (hh, mm, ss) = (num(11, 13)?, num(14, 16)?, num(17, 19)?);
    if !(1..=12).contains(&mo) || !(1..=31).contains(&d) || hh > 23 || mm > 59 || ss > 60 {
        return None;
    }
    let days = days_from_civil(y, mo as u32, d as u32);
    Some((days * 86_400 + hh * 3600 + mm * 60 + ss) as u64)
}

/// Days-from-civil (Howard Hinnant's algorithm), inverse of the civil
/// conversion used by `store::now_rfc3339`.
fn days_from_civil(y: i64, m: u32, d: u32) -> i64 {
    let y = if m <= 2 { y - 1 } else { y };
    let era = if y >= 0 { y } else { y - 399 } / 400;
    let yoe = y - era * 400;
    let mp = (m as i64 + 9) % 12;
    let doy = (153 * mp + 2) / 5 + d as i64 - 1;
    let doe = yoe * 365 + yoe / 4 - yoe / 100 + doy;
    era * 146_097 + doe - 719_468
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;
    use crate::crates::document_engine::document::DocumentType as DT;
    use crate::crates::document_engine::store::documents::NewDocument as StoreNewDocument;
    use crate::crates::storage::STAGING_DIR_NAME;
    use crate::crates::storage::local::config::{FaultInjectionConfig, FaultKind};
    use sha2::{Digest, Sha256};
    use std::io;
    use std::path::PathBuf;
    use std::pin::Pin;
    use std::sync::atomic::{AtomicU64, Ordering};
    use std::task::{Context, Poll};
    use tokio::io::ReadBuf;
    use tokio::sync::mpsc as tokio_mpsc;

    static NEXT_DIR: AtomicU64 = AtomicU64::new(0);

    struct TempRoot(PathBuf);

    impl TempRoot {
        fn new() -> Self {
            let dir = std::env::temp_dir().join(format!(
                "kaladoc-ingest-test-{}-{}",
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

    async fn test_pool() -> SqlitePool {
        let pool = sqlx::sqlite::SqlitePoolOptions::new()
            .max_connections(1)
            .connect("sqlite::memory:")
            .await
            .expect("in-memory sqlite pool");
        crate::crates::document_engine::store::schema::init_document_store(&pool)
            .await
            .expect("init document store");
        pool
    }

    /// In-memory `AsyncRead` over a byte slice (ready immediately).
    struct SliceReader {
        data: Vec<u8>,
        pos: usize,
    }

    impl SliceReader {
        fn new(data: &[u8]) -> Self {
            Self {
                data: data.to_vec(),
                pos: 0,
            }
        }
    }

    impl AsyncRead for SliceReader {
        fn poll_read(
            mut self: Pin<&mut Self>,
            _cx: &mut Context<'_>,
            buf: &mut ReadBuf<'_>,
        ) -> Poll<io::Result<()>> {
            let remaining = &self.data[self.pos..];
            let n = remaining.len().min(buf.remaining());
            buf.put_slice(&remaining[..n]);
            self.pos += n;
            Poll::Ready(Ok(()))
        }
    }

    /// `AsyncRead` fed by an mpsc channel of chunks — lets tests control
    /// pacing and then drop the sender to simulate a stalled stream.
    struct ChanReader {
        rx: tokio_mpsc::Receiver<Vec<u8>>,
        cur: Vec<u8>,
        pos: usize,
    }

    impl AsyncRead for ChanReader {
        fn poll_read(
            mut self: Pin<&mut Self>,
            cx: &mut Context<'_>,
            buf: &mut ReadBuf<'_>,
        ) -> Poll<io::Result<()>> {
            loop {
                if self.pos < self.cur.len() {
                    let n = (self.cur.len() - self.pos).min(buf.remaining());
                    buf.put_slice(&self.cur[self.pos..self.pos + n]);
                    self.pos += n;
                    return Poll::Ready(Ok(()));
                }
                match self.rx.poll_recv(cx) {
                    Poll::Ready(Some(chunk)) => {
                        self.cur = chunk;
                        self.pos = 0;
                    }
                    Poll::Ready(None) => return Poll::Ready(Ok(())),
                    Poll::Pending => return Poll::Pending,
                }
            }
        }
    }

    fn pdf_bytes() -> Vec<u8> {
        b"%PDF-1.4\n1 0 obj\n<<>>\nendobj\ntrailer\n%%EOF\n".to_vec()
    }

    fn hex_sha256(data: &[u8]) -> String {
        Sha256::digest(data)
            .iter()
            .map(|b| format!("{b:02x}"))
            .collect()
    }

    fn staging_files(root: &TempRoot) -> Vec<PathBuf> {
        let dir = root.0.join(STAGING_DIR_NAME);
        match std::fs::read_dir(&dir) {
            Ok(entries) => entries.flatten().map(|e| e.path()).collect(),
            Err(_) => Vec::new(),
        }
    }

    fn object_path(root: &TempRoot, object_name: &str) -> PathBuf {
        root.0.join(object_name)
    }

    #[tokio::test]
    async fn happy_path_pdf_ingest() {
        let root = TempRoot::new();
        let pool = test_pool().await;
        let svc = DocumentIngestionService::new(pool.clone(), root.config());
        let payload = pdf_bytes();

        let outcome = svc
            .ingest(
                SliceReader::new(&payload),
                IngestRequest::new("user-1", "report.pdf"),
            )
            .await
            .expect("ingest");

        assert_eq!(outcome.document.document_type, DT::Pdf);
        assert_eq!(outcome.document.status, DocumentStatus::Active);
        assert_eq!(outcome.detected_mime_type, "application/pdf");
        assert_eq!(outcome.size_bytes, payload.len() as u64);
        assert_eq!(outcome.sha256, hex_sha256(&payload));
        assert_eq!(
            outcome.storage_uri,
            format!("local://documents/{}/report.pdf", outcome.document.id)
        );
        assert_eq!(outcome.artifact.file_name, "report.pdf");
        assert_eq!(
            outcome.artifact.mime_type.as_deref(),
            Some("application/pdf")
        );
        assert_eq!(outcome.artifact.size_bytes, payload.len() as u64);
        assert_eq!(
            outcome.artifact.checksum.as_ref().unwrap().value,
            outcome.sha256
        );
        assert_eq!(outcome.version.version_number, 1);
        assert!(outcome.version.is_current);
        assert_eq!(
            outcome.version.artifact_id.as_deref(),
            Some(outcome.artifact.id.as_str())
        );

        // Bytes are actually stored.
        let mut fs = FaultInjectingFs::new(root.config()).unwrap();
        let object = format!("documents/{}/report.pdf", outcome.document.id);
        assert_eq!(fs.read(&object).unwrap(), payload);

        // Metadata row exists with detected fields.
        let meta = crate::crates::document_engine::store::metadata::get_document_metadata(
            &pool,
            &outcome.document.id,
        )
        .await
        .unwrap()
        .expect("metadata");
        assert_eq!(meta.title.as_deref(), Some("report"));
        assert_eq!(meta.custom_fields["detected_format"], "PDF");
        assert_eq!(meta.custom_fields["detected_mime_type"], "application/pdf");

        // Imported audit event recorded.
        let svc_docs = DocumentService::new(pool);
        let history = svc_docs
            .get_document_history(&outcome.document.id)
            .await
            .unwrap();
        assert!(
            history
                .iter()
                .any(|e| e.event_type == DocumentAuditEventType::Imported)
        );

        assert!(staging_files(&root).is_empty());
    }

    #[tokio::test]
    async fn format_detection_table() {
        let cases: Vec<(Vec<u8>, DT, &str)> = vec![
            (pdf_bytes(), DT::Pdf, "application/pdf"),
            (b"\x89PNG\r\n\x1a\nrest".to_vec(), DT::Image, "image/png"),
            (
                vec![0xFF, 0xD8, 0xFF, 0xE0, 1, 2, 3],
                DT::Image,
                "image/jpeg",
            ),
            (b"GIF89a....".to_vec(), DT::Image, "image/gif"),
            (
                b"PK\x03\x04\x14\x00".to_vec(),
                DT::Archive,
                "application/zip",
            ),
            (
                b"hello world\nplain text\n".to_vec(),
                DT::Text,
                "text/plain",
            ),
            (vec![0u8, 1, 2, 3, 4], DT::Other, "application/octet-stream"),
        ];
        for (i, (payload, expected_type, expected_mime)) in cases.into_iter().enumerate() {
            let root = TempRoot::new();
            let pool = test_pool().await;
            let svc = DocumentIngestionService::new(pool, root.config());
            let outcome = svc
                .ingest(
                    SliceReader::new(&payload),
                    IngestRequest::new("user-1", format!("f{i}.bin")),
                )
                .await
                .unwrap_or_else(|e| panic!("case {i} failed: {e}"));
            assert_eq!(outcome.document.document_type, expected_type, "case {i}");
            assert_eq!(outcome.detected_mime_type, expected_mime, "case {i}");
        }
    }

    #[tokio::test]
    async fn oversize_upload_rolls_back_everything() {
        let root = TempRoot::new();
        let pool = test_pool().await;
        let svc = DocumentIngestionService::with_limits(
            pool.clone(),
            root.config(),
            StreamLimits::new(Some(100)),
        );
        let payload = vec![b'x'; 500];

        let err = svc
            .ingest(
                SliceReader::new(&payload),
                IngestRequest::new("user-1", "big.bin"),
            )
            .await
            .expect_err("oversize must fail");
        assert!(matches!(
            err,
            ServiceError::Storage(
                crate::crates::storage::local::debug_when_full::StorageError::TooLarge { .. }
            )
        ));

        // No document row, no object, no staging residue.
        let docs = DocumentService::new(pool)
            .list_documents(None, None, true, 10)
            .await
            .unwrap();
        assert!(docs.is_empty());
        assert!(staging_files(&root).is_empty());
        let objects_dir = root.0.join("documents");
        let count = std::fs::read_dir(&objects_dir)
            .map(|d| d.flatten().count())
            .unwrap_or(0);
        assert_eq!(count, 0);
    }

    #[tokio::test]
    async fn empty_upload_rejected_before_doc_creation() {
        let root = TempRoot::new();
        let pool = test_pool().await;
        let svc = DocumentIngestionService::new(pool.clone(), root.config());
        let err = svc
            .ingest(SliceReader::new(b""), IngestRequest::new("user-1", "e.bin"))
            .await
            .expect_err("empty upload");
        assert!(matches!(
            err,
            ServiceError::Store(StoreError::Validation { .. })
        ));
        assert!(
            DocumentService::new(pool)
                .list_documents(None, None, true, 10)
                .await
                .unwrap()
                .is_empty()
        );
    }

    #[tokio::test]
    async fn unsafe_file_name_and_owner_rejected() {
        let root = TempRoot::new();
        let pool = test_pool().await;
        let svc = DocumentIngestionService::new(pool, root.config());
        for name in ["../evil", "a/b", "a\\b", ".", "..", "x\x00y"] {
            assert!(
                svc.ingest(SliceReader::new(b"data"), IngestRequest::new("u", name))
                    .await
                    .is_err()
            );
        }
        assert!(
            svc.ingest(
                SliceReader::new(b"data"),
                IngestRequest::new("  ", "ok.bin")
            )
            .await
            .is_err()
        );
    }

    #[tokio::test]
    async fn post_commit_failure_rolls_back_object_and_rows() {
        // Unknown folder_id fails move_document AFTER the object is
        // committed — the harshest rollback path.
        let root = TempRoot::new();
        let pool = test_pool().await;
        let svc = DocumentIngestionService::new(pool.clone(), root.config());
        let mut req = IngestRequest::new("user-1", "doc.pdf");
        req.folder_id = Some(FolderId::new("folder-does-not-exist"));
        let payload = pdf_bytes();

        let err = svc
            .ingest(SliceReader::new(&payload), req)
            .await
            .expect_err("must fail");
        assert!(matches!(
            err,
            ServiceError::Store(StoreError::NotFound { .. })
                | ServiceError::Store(StoreError::Validation { .. })
        ));

        // Document purged, committed object deleted, staging clean.
        assert!(
            DocumentService::new(pool)
                .list_documents(None, None, true, 10)
                .await
                .unwrap()
                .is_empty()
        );
        let objects_dir = root.0.join("documents");
        let remaining: Vec<_> = std::fs::read_dir(&objects_dir)
            .map(|d| d.flatten().collect())
            .unwrap_or_default();
        assert!(remaining.is_empty(), "leftover objects: {remaining:?}");
        assert!(staging_files(&root).is_empty());
    }

    #[tokio::test]
    async fn injected_fault_at_commit_rolls_back() {
        // every_n_ops=3 → begin_upload(1), write_chunk(2), commit(3) →
        // the commit injects a fault deterministically for a ≤chunk-cap
        // payload (prefix write is the only chunk).
        let root = TempRoot::new();
        let pool = test_pool().await;
        let config = root.config().with_faults(FaultInjectionConfig {
            enabled: true,
            every_n_ops: 3,
            allowed_kinds: vec![FaultKind::IoError],
        });
        let svc = DocumentIngestionService::new(pool.clone(), config);
        let payload = pdf_bytes(); // < 512B → single prefix chunk

        let err = svc
            .ingest(SliceReader::new(&payload), IngestRequest::new("u", "f.pdf"))
            .await
            .expect_err("commit fault must fail ingest");
        assert!(matches!(err, ServiceError::Storage(_)));

        assert!(
            DocumentService::new(pool)
                .list_documents(None, None, true, 10)
                .await
                .unwrap()
                .is_empty()
        );
        assert!(staging_files(&root).is_empty());
    }

    #[tokio::test]
    async fn cancellation_leaves_no_doc_after_recover() {
        let root = TempRoot::new();
        let pool = test_pool().await;
        let svc = DocumentIngestionService::new(pool.clone(), root.config());

        let (tx, rx) = tokio_mpsc::channel::<Vec<u8>>(16);
        let reader = ChanReader {
            rx,
            cur: Vec::new(),
            pos: 0,
        };
        let handle = tokio::spawn({
            let svc = svc.clone();
            async move {
                svc.ingest(reader, IngestRequest::new("user-1", "slow.bin"))
                    .await
            }
        });

        // Feed enough bytes to clear the 512-byte sniff peek so the
        // document row gets created, then stall — simulates a caller
        // dropping the ingest mid-stream.
        let mut first = vec![b'a'; SNIFF_BYTES + 100];
        first[..5].copy_from_slice(b"%PDF-");
        tx.send(first).await.unwrap();
        tokio::time::sleep(Duration::from_millis(100)).await;
        tx.send(b"chunk-two".to_vec()).await.unwrap();
        tokio::time::sleep(Duration::from_millis(100)).await;
        handle.abort();
        let _ = handle.await;

        // The document row exists (created before streaming) but has no
        // artifact; staging may still be settling.
        tokio::time::sleep(Duration::from_millis(100)).await;

        let report = svc
            .recover_with(&RecoveryOptions {
                staging_max_age: Duration::ZERO,
                incomplete_document_grace: Duration::ZERO,
                unreferenced_object_grace: Duration::ZERO,
            })
            .await
            .unwrap();
        assert!(report.documents_purged >= 1, "report: {report:?}");

        assert!(
            DocumentService::new(pool)
                .list_documents(None, None, true, 10)
                .await
                .unwrap()
                .is_empty()
        );
        assert!(staging_files(&root).is_empty());
    }

    #[tokio::test]
    async fn recover_sweeps_staging_purges_incomplete_and_gcs_orphans() {
        let root = TempRoot::new();
        let pool = test_pool().await;
        let svc = DocumentIngestionService::new(pool.clone(), root.config());
        let docs = DocumentService::new(pool.clone());

        // 1. Plant a stale staged file.
        let staging = root.0.join(STAGING_DIR_NAME);
        std::fs::create_dir_all(&staging).unwrap();
        std::fs::write(staging.join("crashed.part"), b"partial").unwrap();

        // 2. Incomplete document (no artifacts), old enough via grace 0.
        docs.create_document(&StoreNewDocument {
            name: "shell.pdf".into(),
            document_type: DT::Pdf,
            owner_id: "user-1".into(),
            source: DocumentSource::new(DocumentOrigin::Upload),
        })
        .await
        .unwrap();

        // 3. A healthy ingested document — must survive.
        let payload = pdf_bytes();
        let good = svc
            .ingest(
                SliceReader::new(&payload),
                IngestRequest::new("user-1", "good.pdf"),
            )
            .await
            .unwrap();

        // 4. Document whose artifact's object goes missing.
        let broken = svc
            .ingest(
                SliceReader::new(&payload),
                IngestRequest::new("user-1", "broken.pdf"),
            )
            .await
            .unwrap();
        let broken_object = format!("documents/{}/broken.pdf", broken.document.id);
        std::fs::remove_file(object_path(&root, &broken_object)).unwrap();

        // 5. Unreferenced object older than grace (grace 0 → immediate).
        let ghost = root.0.join("documents/ghost-id/orphan.bin");
        std::fs::create_dir_all(ghost.parent().unwrap()).unwrap();
        std::fs::write(&ghost, b"orphan").unwrap();

        let report = svc
            .recover_with(&RecoveryOptions {
                staging_max_age: Duration::ZERO,
                incomplete_document_grace: Duration::ZERO,
                unreferenced_object_grace: Duration::ZERO,
            })
            .await
            .unwrap();

        assert_eq!(report.staging_entries_removed, 1);
        assert!(report.documents_purged >= 2, "report: {report:?}"); // shell + broken
        assert!(report.artifacts_missing_objects >= 1);
        assert!(report.unreferenced_objects_removed >= 1);

        // Healthy doc + its object survive; broken doc is gone.
        assert!(
            docs.get_document(&good.document.id)
                .await
                .unwrap()
                .is_some()
        );
        assert!(
            docs.get_document(&broken.document.id)
                .await
                .unwrap()
                .is_none()
        );
        assert!(object_path(&root, &format!("documents/{}/good.pdf", good.document.id)).exists());
        assert!(!ghost.exists());
    }

    #[tokio::test]
    async fn recover_keeps_fresh_incomplete_docs() {
        let root = TempRoot::new();
        let pool = test_pool().await;
        let svc = DocumentIngestionService::new(pool.clone(), root.config());
        let docs = DocumentService::new(pool);
        let doc = docs
            .create_document(&StoreNewDocument {
                name: "inflight.pdf".into(),
                document_type: DT::Pdf,
                owner_id: "user-1".into(),
                source: DocumentSource::new(DocumentOrigin::Upload),
            })
            .await
            .unwrap();

        // Default grace (1h) → the fresh shell survives.
        let report = svc.recover().await.unwrap();
        assert_eq!(report.documents_purged, 0);
        assert!(docs.get_document(&doc.id).await.unwrap().is_some());
    }

    #[tokio::test]
    async fn concurrent_ingests_of_distinct_docs() {
        let root = TempRoot::new();
        let pool = test_pool().await;
        let svc = DocumentIngestionService::new(pool.clone(), root.config());
        let payload = pdf_bytes();

        let (a, b) = tokio::join!(
            svc.ingest(SliceReader::new(&payload), IngestRequest::new("u", "a.pdf")),
            svc.ingest(
                SliceReader::new(b"\x89PNG\r\n\x1a\nfake"),
                IngestRequest::new("u", "b.png")
            ),
        );
        let a = a.expect("ingest a");
        let b = b.expect("ingest b");
        assert_ne!(a.document.id, b.document.id);
        assert_eq!(a.document.document_type, DT::Pdf);
        assert_eq!(b.document.document_type, DT::Image);
    }

    #[tokio::test]
    async fn tags_folder_and_actor_metadata() {
        let root = TempRoot::new();
        let pool = test_pool().await;
        let svc = DocumentIngestionService::new(pool.clone(), root.config());
        let docs = DocumentService::new(pool);

        let folder = docs
            .create_folder("Invoices", None, "user-1")
            .await
            .unwrap();
        let mut req = IngestRequest::new("user-1", "inv.pdf");
        req.folder_id = Some(folder.id.clone());
        req.tags = vec!["finance".into(), "2026".into()];
        req.actor_id = Some("admin-1".into());
        req.title = Some("Invoice #42".into());
        req.mime_type = Some("application/pdf".into());

        let outcome = svc
            .ingest(SliceReader::new(&pdf_bytes()), req)
            .await
            .unwrap();
        assert_eq!(outcome.document.folder_id, Some(folder.id));

        let tags = docs.list_tags(&outcome.document.id).await.unwrap();
        assert_eq!(tags.len(), 2);

        let meta = crate::crates::document_engine::store::metadata::get_document_metadata(
            docs.pool(),
            &outcome.document.id,
        )
        .await
        .unwrap()
        .unwrap();
        assert_eq!(meta.title.as_deref(), Some("Invoice #42"));
        assert_eq!(
            meta.keywords,
            vec!["finance".to_string(), "2026".to_string()]
        );
    }

    /// Definition-of-done flow end to end: ingest a document, retrieve
    /// it and its artifact/metadata through the service facade, create
    /// a new version, list versions, then restore the earlier one.
    #[tokio::test]
    async fn ingest_retrieve_version_list_and_restore() {
        let root = TempRoot::new();
        let pool = test_pool().await;
        let svc = DocumentIngestionService::new(pool.clone(), root.config());
        let docs = DocumentService::new(pool);

        // 1. Ingest registers the document, artifact, version 1 and
        //    metadata in one pipeline.
        let outcome = svc
            .ingest(
                SliceReader::new(&pdf_bytes()),
                IngestRequest::new("user-1", "e2e.pdf"),
            )
            .await
            .expect("ingest");
        assert_eq!(outcome.version.version_number, 1);
        assert!(outcome.version.is_current);

        // 2. Retrieve through the facade.
        let doc = docs
            .get_document(&outcome.document.id)
            .await
            .unwrap()
            .expect("document row");
        assert_eq!(doc.id, outcome.document.id);

        let artifacts = docs.list_artifacts(&doc.id).await.unwrap();
        assert_eq!(artifacts.len(), 1);
        assert_eq!(artifacts[0].id, outcome.artifact.id);
        let resolved = docs
            .download_artifact(&artifacts[0].id)
            .await
            .unwrap()
            .expect("artifact row");
        assert_eq!(resolved.storage_uri, outcome.storage_uri);

        let meta = docs
            .get_document_metadata(&doc.id)
            .await
            .unwrap()
            .expect("metadata row");
        assert_eq!(meta.custom_fields["detected_format"], "PDF");

        // 3. A second artifact becomes version 2.
        let art2 = docs
            .upload_artifact(&NewArtifact {
                id: None,
                document_id: doc.id.clone(),
                storage_uri: format!("local://documents/{}/e2e-v2.pdf", doc.id),
                file_name: "e2e-v2.pdf".to_string(),
                mime_type: Some("application/pdf".to_string()),
                size_bytes: pdf_bytes().len() as u64,
                checksum: None,
            })
            .await
            .unwrap();
        let v2 = docs
            .create_version(&doc.id, Some(&art2.id), "user-1", Some("revision 2"))
            .await
            .unwrap();
        assert_eq!(v2.version_number, 2);
        assert!(v2.is_current);

        // 4. Versions list newest-first with the current pointer on v2.
        let versions = docs.list_versions(&doc.id).await.unwrap();
        assert_eq!(versions.len(), 2);
        assert_eq!(versions[0].version_number, 2);
        assert!(versions[0].is_current);
        assert_eq!(versions[1].version_number, 1);
        assert!(!versions[1].is_current);

        // 5. Restore version 1: the current pointer moves back.
        let restored = docs.restore_version(&doc.id, 1).await.unwrap();
        assert_eq!(restored.id, doc.id);
        let versions = docs.list_versions(&doc.id).await.unwrap();
        let v1 = versions
            .iter()
            .find(|v| v.version_number == 1)
            .expect("v1 listed");
        let v2 = versions
            .iter()
            .find(|v| v.version_number == 2)
            .expect("v2 listed");
        assert!(v1.is_current);
        assert!(!v2.is_current);
        // Both artifacts remain listed — restore moves a pointer, it
        // does not delete bytes.
        assert_eq!(docs.list_artifacts(&doc.id).await.unwrap().len(), 2);
    }
}
