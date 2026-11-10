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
//! | `document.pdf_op`       | `{document_id, artifact_id?, op, params?}`                    | Run a pdf-engine operation; byte outputs become artifacts, reports land in metadata (see [`PdfOpHandler`]) |
//! | `pipeline.run`          | `{document_id, artifact_id?, pipeline}` or `{document_id, artifact_id?, dag}` | Run a `pipeline_engine` definition or DAG end-to-end; outputs/reports land like `document.pdf_op` (see [`PipelineRunHandler`]) |
//! | `pdf.inspect`           | `{path, max_bytes?}`                                          | Validate + inspect a PDF under the storage root; writes `<path>.inspect.json` |
//! | `storage.gc_staging`    | `{retention_secs?}`                                           | Delete abandoned entries under `<storage_root>/staging/` and `<storage_root>/.staging/` older than the retention window |
//! | `storage.scan_inbox`    | `{owner_id, recurse?, max_files?}`                            | Ingest files dropped in `<storage_root>/inbox/` and move them to `inbox/.processed/` (see [`StorageScanInboxHandler`]) |
//! | `document.auto_rename`  | `{document_id}`                                               | Rename a document to `{title}-{id8}.{ext}` from extracted PDF metadata, once (see [`DocumentAutoRenameHandler`]) |
//! | `media.op`              | `{document_id?, artifact_id?, op, params?, output_format?, input_b64?/content_b64?/content?}` | Run a multimedia-engine op (ffmpeg/ffprobe); byte outputs become artifacts (or `job-outputs/` objects + job progress when no document), reports land in metadata (see [`MediaOpHandler`]) |
//! | `convert.run`           | `{document_id?, artifact_id?, from_format, to_format, options?, input_b64?/content_b64?/content?}` | Convert bytes via `conversion_engine`; output becomes an artifact or job progress payload (see [`ConvertRunHandler`]) |
//! | `document.ingest_recover` | `{owner_id?, staging_max_age_secs?, incomplete_document_grace_secs?, unreferenced_object_grace_secs?}` | Run `DocumentIngestionService::recover_with` — purge stale artifact-less documents, sweep `.staging/`, GC unreferenced objects; counts land in job progress (see [`IngestRecoverHandler`]) |
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
//! [`enqueue_index`], [`enqueue_thumbnail`], and [`enqueue_pdf_op`]
//! helpers to produce jobs — they build the documented payload
//! contracts and stamp `processing_status = "queued"` (best-effort) so
//! producers never hand-craft JSON.
//!
//! `document.pdf_op` payload details: `document_id` is required,
//! `artifact_id` selects the input artifact (most recent when absent),
//! `op` must be one of [`PDF_OPS`], and `params` is an optional JSON
//! object of op-specific arguments (page ranges as `"pages"`,
//! passwords, watermark/stamp options, `mode` for `compare`, ...).
//! For `compare`, `params.other_artifact_id` (same document) or
//! `params.other_document_id` + `params.other_artifact_id` selects the
//! second input. Outcomes: ops returning PDF/image bytes store each
//! output as a new artifact under `documents/{id}/outputs/` (reported
//! in `pdf_ops.{op}.outputs`); ops returning structured data store a
//! JSON report inline in `pdf_ops.{op}.result` when it fits in
//! [`REPORT_INLINE_MAX_BYTES`], otherwise as a `*-result.json`
//! artifact. Terminal statuses use the same vocabulary as the other
//! processing handlers (`succeeded`, `error`, `unsupported`,
//! `unavailable`, `skipped_too_large`) under both `{op}_status` and
//! `processing_status`.
//!
//! `pipeline.run` payload details: `document_id` is required,
//! `artifact_id` selects the input artifact (most recent when absent,
//! same rules as the other document handlers), and exactly one of
//! `pipeline` (a full
//! [`PipelineDefinition`](crate::crates::pipeline_engine::PipelineDefinition)
//! JSON object — producers resolve built-in names (`pdf.intake`,
//! `pdf.extract-text`) to a definition before enqueueing) or `dag` (a
//! full [`DagDefinition`](crate::crates::pipeline_engine::DagDefinition)
//! JSON object run via [`run_dag`](crate::crates::pipeline_engine::run_dag)
//! — every node sees the same input bytes, waves execute sequentially,
//! and a failed node skips its dependents while siblings continue) is
//! required. The
//! pipeline's byte artifact(s) are stored under
//! `documents/{id}/outputs/` as `pipeline-{slug}-{artifact_id}.*`
//! files (DAG runs store each succeeded node's *changed* artifact as
//! `dag-{slug}-{artifact_id}-{node}.*` — pass-through report steps
//! produce no file), step/node reports land in the `report` field of
//! the `pipelines.{slug}` metadata entry (or a `*-report.json` artifact
//! when large), and statuses use the same vocabulary as
//! `document.pdf_op` under `pipeline_run_status` +
//! `processing_status`. A DAG run records `error` when any node
//! failed, `succeeded` otherwise.
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
//!
//! `storage.scan_inbox` payload details: `owner_id` is required and
//! becomes the owner of every created document. `recurse` (default
//! `false`) descends into subdirectories of `inbox/`; `max_files`
//! (default [`folder_watch::DEFAULT_MAX_FILES`], clamped to
//! [`folder_watch::MAX_MAX_FILES`]) caps how many files one run
//! handles. The scan lives in
//! [`crate::crates::application::services::folder_watch`]: only
//! regular, non-hidden files are collected (dot-prefixed names —
//! including `.processed` itself — and symlinks are always skipped,
//! so nothing outside `inbox/` is ever touched). Each file ≤
//! [`MAX_PROCESSING_BYTES`] (64 MiB, the base64 ingest cap) gets a
//! document row plus a `document.ingest` job; larger or otherwise
//! invalid files are recorded as per-file errors and left in place.
//! Successfully enqueued files are then moved to
//! `inbox/.processed/` preserving their relative path — that move is
//! the idempotency marker. The job itself is **at-least-once**: a
//! crash between enqueue and move re-scans the file and produces a
//! duplicate document/job pair, and per-file failures never fail the
//! whole scan.
//!
//! `document.auto_rename` payload details: `document_id` is required.
//! When the document's metadata carries a usable title (the
//! `pdf_title`/`title` custom fields written by
//! `document.metadata_extract`, falling back to the metadata `title`
//! column) the document is renamed to
//! `{sanitized-title}-{id8}.{ext}` via
//! [`store::documents::rename_document`](crate::crates::document_engine::store::documents::rename_document),
//! where `id8` is the last 8 characters of the document id and `ext`
//! is preserved from the latest artifact's file name (or the current
//! document name). The title is sanitized to [`DocumentName`] rules:
//! `/`, `\`, and control characters are replaced, the result is
//! whitespace-trimmed and truncated so the full name stays ≤ 255
//! chars. An `auto_renamed` custom field plus a
//! `ProcessingCompleted` audit event record the rename; when the flag
//! is already set, no title exists, or the name is already
//! content-derived the job succeeds without changes (idempotent).
//! A missing document fails the job.
//!
//! `media.op` payload details: input resolution prefers `artifact_id`
//! (verified against `document_id` when both are given; `document_id`
//! alone selects the most recent artifact, and an `artifact_id` alone
//! implies its owning document for output registration). With no
//! artifact fields, exactly one of `input_b64`, `content_b64`, or
//! `content` supplies inline bytes. `op` must be one of
//! [`media_ops::MEDIA_OPS`]; `params` is an opaque JSON object
//! forwarded to the op's options struct; top-level `output_format`
//! fills `params.output_format` when the op's params lack one.
//! `subtitles.burn` additionally accepts `params.subtitle_b64` or
//! `params.subtitle_artifact_id`/`params.subtitle_document_id` for
//! its second input. Artifact bytes are read under a
//! [`MEDIA_MAX_INPUT_BYTES`] (512 MiB) cap; larger inputs record
//! `skipped_too_large`. File outputs are stored under
//! `documents/{id}/outputs/` as `{op-slug}-{src}-{name}` artifacts
//! (cap [`MAX_OP_OUTPUTS`]); report ops store their JSON inline in
//! `media_ops.{op}.result` (or a `*-result.json` artifact when large).
//! With no document, outputs are stored under `job-outputs/{job_id}/`
//! (no artifact rows exist without a document) and the job's
//! `progress` snapshot carries the result. Statuses use the shared
//! vocabulary under `media_op_status` + `processing_status` and a
//! `media_ops.{op}` entry. `MediaError::Unsupported`/`MissingTool`
//! (capability-gated ops, absent ffmpeg) fail the *job* honestly —
//! they are not recorded as document outcomes.
//!
//! `convert.run` payload details: same input resolution as
//! `media.op` (`artifact_id`/`document_id`/inline bytes — inline
//! bytes are also accepted as `content_b64`/`content`).
//! `from_format`/`to_format` are [`ConvertFormat`]
//! names (aliases like `md`, `jpg`, `text` accepted); `options` is an
//! optional [`EngineConvertOptions`] object. Exactly
//! the format pairs the registered backends advertise are honored —
//! the engine itself reports `UnsupportedConversion` otherwise.
//! Outcomes mirror `media.op`: outputs land as
//! `documents/{id}/outputs/convert-{from}-{to}-{src}.{ext}`
//! artifacts with a `conversions.{pair}` metadata entry, or under
//! `job-outputs/{job_id}/` + job progress when no document is given.
//!
//! `document.ingest_recover` payload details: all fields optional.
//! `owner_id` is accepted for forward compatibility — recovery is a
//! global sweep and currently ignores it. The three `*_secs` fields
//! override [`RecoveryOptions`] grace periods (defaults
//! 3600 s each). There is no worker-startup hook — `register_builtin`
//! is synchronous and handlers own no lifecycle — so operators
//! schedule this job (e.g. periodically or at deploy time) to make
//! [`DocumentIngestionService::recover`] reachable; the report
//! (`staging_entries_removed`, `documents_purged`,
//! `unreferenced_objects_removed`, `errors`, ...) is stored on the
//! job's `progress` field.

use std::io::Read;
use std::path::{Component, Path, PathBuf};
use std::sync::Arc;
use std::time::{Duration, SystemTime};

use base64::{Engine as _, engine::general_purpose::STANDARD as B64};
use serde::Deserialize;
use serde_json::json;
use sha2::{Digest, Sha256};

use crate::config::AppConfig;
use crate::crates::application::services::folder_watch;
use crate::crates::application::services::ingestion::{DocumentIngestionService, RecoveryOptions};
use crate::crates::application::services::media_ops;
use crate::crates::conversion_engine::{
    ConversionEngine, ConvertError, ConvertOptions as EngineConvertOptions, ConvertOutput,
    ConvertRequest, Format as ConvertFormat,
};
use crate::crates::document_engine::artifact::{
    ChecksumAlgorithm, DocumentArtifact, DocumentChecksum,
};
use crate::crates::document_engine::audit::DocumentAuditEventType;
use crate::crates::document_engine::document::{
    DocumentName, DocumentOrigin, DocumentSource, DocumentType,
};
use crate::crates::document_engine::ids::DocumentId;
use crate::crates::document_engine::metadata::DocumentMetadata;
use crate::crates::document_engine::store::StoreError;
use crate::crates::document_engine::store::artifacts::{
    NewArtifact, download_artifact, list_artifacts, upload_artifact,
};
use crate::crates::document_engine::store::documents::{
    NewDocument, create_document, get_document, rename_document,
};
use crate::crates::document_engine::store::history::record_event;
use crate::crates::document_engine::store::metadata::{
    get_document_metadata, upsert_document_metadata,
};
use crate::crates::document_engine::store::now_rfc3339;
use crate::crates::job_engine::{
    Job, JobContext, JobEngine, JobError, JobHandler, NewJob, enqueue,
};
use crate::crates::multimedia_engine::error::MediaError;
use crate::crates::multimedia_engine::model::MediaLimits;
use crate::crates::multimedia_engine::runtime::discovery::Runtime;
use crate::crates::pdf_engine as pe;
use crate::crates::pdf_engine::PdfError;
use crate::crates::pipeline_engine::{
    DagDefinition, DagNodeStatus, DagReport, PipelineArtifact, PipelineDefinition, PipelineEngine,
    PipelineError, PipelineLimits, StepError, run_dag,
};
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

/// Job kind for [`PdfOpHandler`].
pub const PDF_OP_KIND: &str = "document.pdf_op";

/// Job kind for [`PipelineRunHandler`].
pub const PIPELINE_RUN_KIND: &str = "pipeline.run";

/// Job kind for [`StorageScanInboxHandler`].
pub const STORAGE_SCAN_INBOX_KIND: &str = "storage.scan_inbox";

/// Job kind for [`DocumentAutoRenameHandler`].
pub const DOCUMENT_AUTO_RENAME_KIND: &str = "document.auto_rename";

/// Job kind for [`MediaOpHandler`].
pub const MEDIA_OP_KIND: &str = "media.op";

/// Job kind for [`ConvertRunHandler`].
pub const CONVERT_RUN_KIND: &str = "convert.run";

/// Job kind for [`IngestRecoverHandler`].
pub const DOCUMENT_INGEST_RECOVER_KIND: &str = "document.ingest_recover";

/// Pdf-engine operations reachable through [`PDF_OP_KIND`] /
/// [`enqueue_pdf_op`]. Producers (the REST API) validate `op` against
/// this list before enqueueing; the handler re-checks it so hand-built
/// payloads fail fast too.
pub const PDF_OPS: &[&str] = &[
    // Inspection / extraction (structured reports).
    "page_count",
    "inspect",
    "validate",
    "repair",
    "verify_signatures",
    "extract_metadata",
    "extract_text",
    "extract_images",
    "ocr",
    "ocr_detailed",
    "compare",
    // Conversion / rendering (byte outputs).
    "convert",
    "render",
    "render_pages",
    // Page structure (byte outputs).
    "compress",
    "flatten",
    "sanitize",
    "rotate",
    "split",
    "extract_pages",
    "remove_pages",
    "reorder",
    "swap_pages",
    // Marks.
    "watermark",
    "overlay",
    "stamp",
    // Security.
    "encrypt",
    "decrypt",
    "redact",
    // Advanced editing / inspection ops (dispatch arms below).
    "add_attachment",
    "add_links",
    "add_named_destinations",
    "apply_trim_preset",
    "audit_content",
    "audit_fonts",
    "bates",
    "build_outline_from_text",
    "clean_scan",
    "clear_transitions",
    "dedup_images",
    "deskew",
    "detect_orientation",
    "edit_field_props",
    "extract_attachment",
    "get_document_dates",
    "get_page_boxes",
    "get_page_size",
    "get_trim_boxes",
    "inspect_fonts",
    "inspect_javascript",
    "inspect_structure",
    "list_attachments",
    "list_layers",
    "list_links",
    "list_named_destinations",
    "list_oc_marked_content",
    "list_transitions",
    "normalize_orientation",
    "object_report",
    "permissions_report",
    "recompress_streams",
    "redact_pages",
    "redact_regions",
    "remove_attachment",
    "remove_hidden_text",
    "remove_links",
    "remove_named_destination",
    "remove_oc_properties",
    "remove_page_box",
    "set_document_dates",
    "set_layer_visibility",
    "set_page_box",
    "set_page_size",
    "set_transition",
    // Backend-only ops (dispatch arms below).
    "add_comment",
    "add_freetext",
    "add_ink",
    "add_page_numbers",
    "add_print_marks",
    "booklet",
    "build_timestamp_request",
    "clear_crop",
    "clear_form",
    "clear_metadata",
    "clear_page_labels",
    "create_portfolio",
    "create_text_field",
    "crop",
    "delete_form_field",
    "document_digest",
    "draw_shapes",
    "draw_text",
    "duplicate_pages",
    "edit_text",
    "fill_form",
    "garbage_collect",
    "get_page_labels",
    "get_page_rotations",
    "get_xmp",
    "grayscale",
    "headers_footers",
    "has_timestamps",
    "insert_blank",
    "interleave",
    "invert_colors",
    "linearize",
    "list_annotations",
    "list_articles",
    "list_form_fields",
    "list_ink",
    "list_outline",
    "list_portfolio",
    "list_signature_timestamps",
    "lock_fields",
    "make_searchable",
    "merge",
    "merge_mode",
    "normalize_rotations",
    "nup",
    "poster",
    "remove_annotations",
    "remove_articles",
    "remove_blank_pages",
    "remove_images",
    "remove_ink",
    "remove_named_objects",
    "remove_outline",
    "replace_image",
    "request_external_timestamp",
    "resize",
    "scale_content",
    "set_metadata",
    "set_outline",
    "set_page_labels",
    "set_page_rotation",
    "set_view_crop",
    "set_xmp",
    "sign",
    "single_page",
    "split_at_outline",
    "split_mode",
    "text_markup",
    "timestamp_document",
    "timestamp_query",
    "to_pdfa",
    "to_pdfua",
    "to_pdfx",
    "unlock_fields",
];

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

/// Maximum output files a single `document.pdf_op` job may store as
/// artifacts (bounds `split` fan-out).
const MAX_OP_OUTPUTS: usize = 100;

/// Structured op reports up to this size are stored inline in the
/// `pdf_ops.{op}.result` metadata field; larger ones become a
/// `*-result.json` artifact instead.
const REPORT_INLINE_MAX_BYTES: usize = 64 * 1024;

/// Default `max_attempts` used by the `enqueue_*` helpers.
const DEFAULT_MAX_ATTEMPTS: u32 = 3;

/// Default `retention_secs` for `storage.gc_staging` (24 h).
const DEFAULT_GC_RETENTION_SECS: u64 = 86_400;

/// Maximum input bytes `media.op` / `convert.run` will buffer for a
/// stored artifact or secondary (subtitle) input — larger objects are
/// recorded as `skipped_too_large` instead of being read.
const MEDIA_MAX_INPUT_BYTES: u64 = 512 * 1024 * 1024;

/// Metadata custom-field key holding the `media.op` terminal status
/// (`media_op_status`), alongside the `media_ops.{op}` results map.
const MEDIA_OP_STATUS_KEY: &str = "media_op_status";

/// Metadata custom-field map holding per-op `media.op` results:
/// `media_ops.{op} = {status, finished_at, result?, outputs?, ...}`.
const MEDIA_OPS_FIELD: &str = "media_ops";

/// Metadata custom-field key holding the `convert.run` terminal
/// status (`convert_run_status`), alongside the `conversions.{pair}`
/// results map.
const CONVERT_RUN_STATUS_KEY: &str = "convert_run_status";

/// Metadata custom-field map holding per-pair `convert.run` results:
/// `conversions.{from}-{to} = {status, finished_at, ...}`.
const CONVERSIONS_FIELD: &str = "conversions";

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
        Ok(handler) => engine.register(DOCUMENT_METADATA_EXTRACT_KIND, Arc::new(handler)),
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
    match PdfOpHandler::from_app_config() {
        Ok(handler) => engine.register(PDF_OP_KIND, Arc::new(handler)),
        Err(e) => tracing::warn!(
            error = %e,
            "skipping document.pdf_op handler: storage config unavailable"
        ),
    }
    match PipelineRunHandler::from_app_config() {
        Ok(handler) => engine.register(PIPELINE_RUN_KIND, Arc::new(handler)),
        Err(e) => tracing::warn!(
            error = %e,
            "skipping pipeline.run handler: storage config unavailable"
        ),
    }
    match StorageScanInboxHandler::from_app_config() {
        Ok(handler) => engine.register(STORAGE_SCAN_INBOX_KIND, Arc::new(handler)),
        Err(e) => tracing::warn!(
            error = %e,
            "skipping storage.scan_inbox handler: storage config unavailable"
        ),
    }
    // document.auto_rename only touches the document store — no
    // storage config needed.
    engine.register(
        DOCUMENT_AUTO_RENAME_KIND,
        Arc::new(DocumentAutoRenameHandler),
    );
    match MediaOpHandler::from_app_config() {
        Ok(handler) => engine.register(MEDIA_OP_KIND, Arc::new(handler)),
        Err(e) => tracing::warn!(
            error = %e,
            "skipping media.op handler: storage config unavailable"
        ),
    }
    match ConvertRunHandler::from_app_config() {
        Ok(handler) => engine.register(CONVERT_RUN_KIND, Arc::new(handler)),
        Err(e) => tracing::warn!(
            error = %e,
            "skipping convert.run handler: storage config unavailable"
        ),
    }
    match IngestRecoverHandler::from_app_config() {
        Ok(handler) => engine.register(DOCUMENT_INGEST_RECOVER_KIND, Arc::new(handler)),
        Err(e) => tracing::warn!(
            error = %e,
            "skipping document.ingest_recover handler: storage config unavailable"
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
fn store_err(e: StoreError) -> JobError {
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
    Ok(BoundedBytes {
        bytes: buf,
        truncated,
    })
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
            let artifacts = list_artifacts(pool, document_id).await.map_err(store_err)?;
            artifacts
                .into_iter()
                .last()
                .ok_or_else(|| JobError::Failed(format!("document {document_id} has no artifacts")))
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
                let engine = crate::crates::pdf_engine::PdfEngine::with_limits(processing_limits());
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
            m.custom_fields.insert(
                "processing_status".into(),
                json!(normalized_processing_status(status)),
            );
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
                m.custom_fields.insert("extract_error".into(), json!(err));
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
    Failed {
        format: SniffedFormat,
        error: String,
    },
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
                mark_failed(&ctx.pool, &payload.document_id, INDEX_STATUS_KEY, job, e).await;
            }
            result
        })
    }
}

impl DocumentIndexHandler {
    async fn run(&self, ctx: &JobContext, job: &Job) -> Result<(), JobError> {
        let (document_id, artifact) = begin_processing(&ctx.pool, job, INDEX_STATUS_KEY).await?;

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
                    let engine =
                        crate::crates::pdf_engine::PdfEngine::with_limits(processing_limits());
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
            m.custom_fields.insert(
                "processing_status".into(),
                json!(normalized_processing_status(status)),
            );
            m.custom_fields
                .insert(INDEX_STATUS_KEY.into(), json!(status));
            m.custom_fields
                .insert("index_artifact_id".into(), json!(artifact_id));
            if let Some(f) = format_label {
                m.custom_fields.insert("index_format".into(), json!(f));
            }
            if let Some(e) = &index_error {
                m.custom_fields.insert("index_error".into(), json!(e));
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
fn as_dict<'a>(doc: &'a lopdf::Document, obj: &'a lopdf::Object) -> Option<&'a lopdf::Dictionary> {
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
                let Some(xobjects) = resources.get(b"XObject").ok().and_then(|o| as_dict(doc, o))
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
                    m.custom_fields.insert(
                        "processing_status".into(),
                        json!(normalized_processing_status(status)),
                    );
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
                        .insert("processing_status".into(), json!("failed"));
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
                        .insert("processing_status".into(), json!("succeeded"));
                    m.custom_fields
                        .insert(THUMBNAIL_STATUS_KEY.into(), json!("generated"));
                    m.custom_fields
                        .insert("thumbnail_uri".into(), json!(storage_uri));
                    m.custom_fields
                        .insert("thumbnail_sha256".into(), json!(sha));
                    m.custom_fields
                        .insert("thumbnail_bytes".into(), json!(nbytes));
                    m.custom_fields.insert("thumbnail_page".into(), json!(page));
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
// document.pdf_op
// ---------------------------------------------------------------------------
//
// Generic pdf-engine operation runner: one job kind covers every op in
// [`PDF_OPS`] so the REST API (and any other producer) can reach the
// whole engine surface without a handler per op. See the module docs
// for the payload contract and result storage scheme.

/// `document.pdf_op` — runs a [`PdfEngine`](crate::crates::pdf_engine::PdfEngine)
/// operation named by `payload.op` over the resolved artifact's bytes.
///
/// Unlike the dedicated handlers, engine-level failures (bad params,
/// corrupt PDF, unsupported format) are **terminal job successes**
/// recorded as `{op}_status: "error"` (or `unsupported` /
/// `unavailable` / `skipped_too_large`) — they are deterministic, so
/// retrying cannot help. Only infrastructure failures (storage, store,
/// pool) produce `JobError` and consume retry attempts.
pub struct PdfOpHandler {
    storage: StorageConfig,
}

/// Metadata custom-field map holding per-op results:
/// `pdf_ops.{op} = {status, finished_at, result?, outputs?, error?}`.
const PDF_OPS_FIELD: &str = "pdf_ops";

impl PdfOpHandler {
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

/// Parsed `document.pdf_op` payload. `document_id`/`artifact_id` are
/// validated through [`DocJobPayload::parse`] but resolved by
/// [`begin_processing`], so only `op`/`params` are kept here.
struct PdfOpPayload {
    op: String,
    params: serde_json::Value,
}

impl PdfOpPayload {
    fn parse(payload: &serde_json::Value) -> Result<Self, JobError> {
        DocJobPayload::parse(payload)?;
        let op = payload["op"]
            .as_str()
            .map(str::trim)
            .filter(|s| !s.is_empty())
            .ok_or_else(|| JobError::Failed("payload.op must be a non-empty string".into()))?
            .to_string();
        if !PDF_OPS.contains(&op.as_str()) {
            return Err(JobError::Failed(format!("unknown pdf op: {op}")));
        }
        let params = match &payload["params"] {
            serde_json::Value::Null => json!({}),
            v @ serde_json::Value::Object(_) => v.clone(),
            _ => {
                return Err(JobError::Failed(
                    "payload.params must be a JSON object".into(),
                ));
            }
        };
        Ok(Self { op, params })
    }
}

/// Per-operation status field name (`{op}_status`), matching the
/// convention used by the dedicated processing handlers.
fn pdf_op_status_key(op: &str) -> String {
    format!("{op}_status")
}

/// One output file produced by an op, ready to be streamed into
/// storage and registered as an artifact.
struct OpFile {
    /// Safe file name (already validated); becomes the last path
    /// segment under `documents/{id}/outputs/`.
    file_name: String,
    bytes: Vec<u8>,
    /// File extension without dot, e.g. `"pdf"`, `"png"`, `"json"`.
    ext: &'static str,
    mime: &'static str,
}

/// Successful op result: an optional structured report plus any
/// binary output files.
struct OpSuccess {
    report: Option<serde_json::Value>,
    files: Vec<OpFile>,
}

/// Terminal op failure recorded on the document (the job itself still
/// succeeds — see [`PdfOpHandler`]).
struct OpFailure {
    /// Status vocabulary: `error`, `unsupported`, `unavailable`,
    /// `skipped_too_large`.
    status: &'static str,
    error: String,
}

enum OpOutcome {
    Done(OpSuccess),
    Failed(OpFailure),
}

/// Parameter/validation failure inside an op — deterministic, so it is
/// recorded as `error` rather than consuming job retries.
fn op_param_error(message: impl Into<String>) -> OpFailure {
    OpFailure {
        status: "error",
        error: message.into(),
    }
}

/// Maps a [`PdfError`](crate::crates::pdf_engine::PdfError) onto the
/// terminal-status vocabulary: missing external tools mean the op is
/// environmentally `unavailable`, encrypted/unsupported inputs are
/// `unsupported`, everything else is an honest `error`.
fn op_engine_error(e: crate::crates::pdf_engine::PdfError) -> OpFailure {
    use crate::crates::pdf_engine::PdfError as E;
    let status = match e {
        E::MissingTool(_) => "unavailable",
        E::Encrypted | E::UnsupportedOperation(_) => "unsupported",
        _ => "error",
    };
    OpFailure {
        status,
        error: e.to_string(),
    }
}

fn req_str(params: &serde_json::Value, key: &str) -> Result<String, OpFailure> {
    params
        .get(key)
        .and_then(|v| v.as_str())
        .map(str::trim)
        .filter(|s| !s.is_empty())
        .map(str::to_string)
        .ok_or_else(|| op_param_error(format!("params.{key} must be a non-empty string")))
}

fn opt_str(params: &serde_json::Value, key: &str) -> Option<String> {
    params
        .get(key)
        .and_then(|v| v.as_str())
        .map(str::trim)
        .filter(|s| !s.is_empty())
        .map(str::to_string)
}

fn opt_u32(params: &serde_json::Value, key: &str) -> Result<Option<u32>, OpFailure> {
    match params.get(key) {
        None | Some(serde_json::Value::Null) => Ok(None),
        Some(v) => v
            .as_u64()
            .and_then(|n| u32::try_from(n).ok())
            .map(Some)
            .ok_or_else(|| op_param_error(format!("params.{key} must be a non-negative integer"))),
    }
}

fn req_u32(params: &serde_json::Value, key: &str) -> Result<u32, OpFailure> {
    opt_u32(params, key)?.ok_or_else(|| op_param_error(format!("params.{key} is required")))
}

fn opt_f64(params: &serde_json::Value, key: &str) -> Result<Option<f64>, OpFailure> {
    match params.get(key) {
        None | Some(serde_json::Value::Null) => Ok(None),
        Some(v) => v
            .as_f64()
            .map(Some)
            .ok_or_else(|| op_param_error(format!("params.{key} must be a number"))),
    }
}

fn opt_bool(params: &serde_json::Value, key: &str, default: bool) -> Result<bool, OpFailure> {
    match params.get(key) {
        None | Some(serde_json::Value::Null) => Ok(default),
        Some(v) => v
            .as_bool()
            .ok_or_else(|| op_param_error(format!("params.{key} must be a boolean"))),
    }
}

/// Optional 1-based page-range spec shared by most ops.
fn pages_param(params: &serde_json::Value) -> Option<String> {
    opt_str(params, "pages")
}

/// Parses a serde unit-variant enum from a string param
/// (`"top_left"`, `"text"`, ...).
fn enum_param<T: serde::de::DeserializeOwned>(
    params: &serde_json::Value,
    key: &str,
) -> Result<Option<T>, OpFailure> {
    match opt_str(params, key) {
        None => Ok(None),
        Some(s) => serde_json::from_value(serde_json::Value::String(s))
            .map(Some)
            .map_err(|_| op_param_error(format!("params.{key} has an invalid value"))),
    }
}

/// Deserializes the first present of `keys` in `params` as `T` —
/// used for spec structs (`params.spec`) and arrays of spec structs
/// (`params.links`, `params.patches`, ...). `None`/absent keys are
/// skipped; a present-but-malformed value is a param error.
fn param_as<T: serde::de::DeserializeOwned>(
    params: &serde_json::Value,
    keys: &[&str],
) -> Result<Option<T>, OpFailure> {
    for key in keys {
        match params.get(*key) {
            None | Some(serde_json::Value::Null) => continue,
            Some(v) => {
                return serde_json::from_value(v.clone())
                    .map(Some)
                    .map_err(|e| op_param_error(format!("params.{key} is invalid: {e}")));
            }
        }
    }
    Ok(None)
}

/// Required variant of [`param_as`]; the error names the first key.
fn req_param_as<T: serde::de::DeserializeOwned>(
    params: &serde_json::Value,
    keys: &[&str],
) -> Result<T, OpFailure> {
    param_as(params, keys)?.ok_or_else(|| op_param_error(format!("params.{} is required", keys[0])))
}

fn req_f64(params: &serde_json::Value, key: &str) -> Result<f64, OpFailure> {
    opt_f64(params, key)?.ok_or_else(|| op_param_error(format!("params.{key} is required")))
}

/// Parses a required `[x0, y0, x1, y1]` rectangle param.
fn rect4_param(params: &serde_json::Value, key: &str) -> Result<[f64; 4], OpFailure> {
    match params.get(key) {
        Some(v) => serde_json::from_value::<[f64; 4]>(v.clone())
            .map_err(|_| op_param_error(format!("params.{key} must be [x0, y0, x1, y1]"))),
        None => Err(op_param_error(format!("params.{key} is required"))),
    }
}

/// Parses `params.box` (or `params.which`) into a [`pe::PageBox`].
/// The engine enum has no serde impl, so strings are matched here.
fn page_box_param(params: &serde_json::Value) -> Result<pe::PageBox, OpFailure> {
    let raw = opt_str(params, "box")
        .or_else(|| opt_str(params, "which"))
        .ok_or_else(|| op_param_error("params.box is required"))?;
    match raw.to_ascii_lowercase().as_str() {
        "media" | "mediabox" | "media_box" => Ok(pe::PageBox::MediaBox),
        "crop" | "cropbox" | "crop_box" => Ok(pe::PageBox::CropBox),
        "bleed" | "bleedbox" | "bleed_box" => Ok(pe::PageBox::BleedBox),
        "trim" | "trimbbox" | "trim_box" | "trimb_box" => Ok(pe::PageBox::TrimBox),
        "art" | "artbox" | "art_box" => Ok(pe::PageBox::ArtBox),
        other => Err(op_param_error(format!(
            "params.box must be media, crop, bleed, trim, or art, got {other:?}"
        ))),
    }
}

/// Parses `params.preset` into a [`pe::TrimPreset`].
fn trim_preset_param(params: &serde_json::Value) -> Result<pe::TrimPreset, OpFailure> {
    let raw =
        opt_str(params, "preset").ok_or_else(|| op_param_error("params.preset is required"))?;
    match raw.to_ascii_lowercase().as_str() {
        "a4" => Ok(pe::TrimPreset::A4),
        "a5" => Ok(pe::TrimPreset::A5),
        "letter" => Ok(pe::TrimPreset::Letter),
        "legal" => Ok(pe::TrimPreset::Legal),
        "a3" => Ok(pe::TrimPreset::A3),
        other => Err(op_param_error(format!(
            "params.preset must be a4, a5, letter, legal, or a3, got {other:?}"
        ))),
    }
}

/// Parses `params.unit` into a [`pe::SizeUnit`]; defaults to points.
fn size_unit_param(params: &serde_json::Value) -> Result<pe::SizeUnit, OpFailure> {
    match opt_str(params, "unit")
        .map(|u| u.to_ascii_lowercase())
        .as_deref()
    {
        None | Some("pt" | "point" | "points") => Ok(pe::SizeUnit::Pt),
        Some("mm" | "millimeter" | "millimetre") => Ok(pe::SizeUnit::Mm),
        Some("in" | "inch" | "inches") => Ok(pe::SizeUnit::In),
        Some("px" | "pixel" | "pixels") => Ok(pe::SizeUnit::Px),
        Some(other) => Err(op_param_error(format!(
            "params.unit must be pt, mm, in, or px, got {other:?}"
        ))),
    }
}

/// Parses `params.mode` into an [`pe::OrientMode`].
fn orient_mode_param(params: &serde_json::Value) -> Result<pe::OrientMode, OpFailure> {
    let raw = opt_str(params, "mode").ok_or_else(|| op_param_error("params.mode is required"))?;
    match raw.to_ascii_lowercase().as_str() {
        "portrait" => Ok(pe::OrientMode::Portrait),
        "landscape" => Ok(pe::OrientMode::Landscape),
        "as_is" | "asis" | "none" | "keep" => Ok(pe::OrientMode::AsIs),
        other => Err(op_param_error(format!(
            "params.mode must be portrait, landscape, or as_is, got {other:?}"
        ))),
    }
}

/// Parses the `redact_pages` replacement: `params.replacement` as the
/// serde form (`"blank"` or `{"text": "..."}`), falling back to a
/// plain `params.replacement_text`/`params.text` string. Defaults to
/// [`pe::RedactReplacement::Blank`].
fn redact_replacement_param(
    params: &serde_json::Value,
) -> Result<pe::RedactReplacement, OpFailure> {
    if let Some(v) = params.get("replacement").filter(|v| !v.is_null()) {
        return serde_json::from_value(v.clone()).map_err(|_| {
            op_param_error("params.replacement must be \"blank\" or {\"text\": \"...\"}")
        });
    }
    Ok(
        match opt_str(params, "replacement_text").or_else(|| opt_str(params, "text")) {
            Some(t) => pe::RedactReplacement::Text(t),
            None => pe::RedactReplacement::Blank,
        },
    )
}

/// Builds a [`pe::TransitionSpec`] from `params.spec`, or from the
/// top-level `kind`/`duration`/`direction`/`motion` params.
fn transition_spec_param(params: &serde_json::Value) -> Result<pe::TransitionSpec, OpFailure> {
    if let Some(spec) = param_as::<pe::TransitionSpec>(params, &["spec"])? {
        return Ok(spec);
    }
    let kind: pe::TransitionType =
        enum_param(params, "kind")?.ok_or_else(|| op_param_error("params.kind is required"))?;
    Ok(pe::TransitionSpec {
        kind,
        duration: req_f64(params, "duration")? as f32,
        direction: enum_param(params, "direction")?,
        motion: enum_param(params, "motion")?,
    })
}

/// Builds a [`pe::HeadingSpec`] from `params.spec`, or from the
/// top-level `min_font_size`/`max_font_size`/`max_depth` params.
fn heading_spec_param(params: &serde_json::Value) -> Result<pe::HeadingSpec, OpFailure> {
    if let Some(spec) = param_as::<pe::HeadingSpec>(params, &["spec"])? {
        return Ok(spec);
    }
    Ok(pe::HeadingSpec {
        min_font_size: req_f64(params, "min_font_size")? as f32,
        max_font_size: req_f64(params, "max_font_size")? as f32,
        max_depth: req_u32(params, "max_depth")?,
    })
}

/// Returns a params view with `spec`/`options` object keys merged
/// under the top-level keys (top-level wins on conflict). Lets the
/// newer ops accept either `{"spec": {...}}`/`{"options": {...}}` or
/// flat parameters while reusing the flat param helpers.
fn merged_params(params: &serde_json::Value) -> serde_json::Value {
    let mut merged = params.clone();
    for key in ["spec", "options"] {
        if let Some(obj) = params.get(key).and_then(|v| v.as_object())
            && let serde_json::Value::Object(map) = &mut merged
        {
            for (k, v) in obj {
                map.entry(k.clone()).or_insert_with(|| v.clone());
            }
        }
    }
    merged
}

/// First-present-key variant of [`enum_param`].
fn enum_param_keys<T: serde::de::DeserializeOwned>(
    params: &serde_json::Value,
    keys: &[&str],
) -> Result<Option<T>, OpFailure> {
    for key in keys {
        if let Some(v) = enum_param(params, key)? {
            return Ok(Some(v));
        }
    }
    Ok(None)
}

/// Optional `[f64; 4]` rectangle read from the first present of
/// `keys` (`params.rect`, `params.crop_box`, ...).
fn opt_rect4(params: &serde_json::Value, keys: &[&str]) -> Result<Option<[f64; 4]>, OpFailure> {
    for key in keys {
        if let Some(v) = params.get(*key).filter(|v| !v.is_null()) {
            return serde_json::from_value::<[f64; 4]>(v.clone())
                .map(Some)
                .map_err(|_| op_param_error(format!("params.{key} must be [x0, y0, x1, y1]")));
        }
    }
    Ok(None)
}

/// Required variant of [`opt_rect4`]; the error names the first key.
fn req_rect4(params: &serde_json::Value, keys: &[&str]) -> Result<[f64; 4], OpFailure> {
    opt_rect4(params, keys)?
        .ok_or_else(|| op_param_error(format!("params.{} is required", keys[0])))
}

/// Optional `[u8; 3]` RGB color param (`params.color` = `[r, g, b]`
/// with 0-255 components).
fn opt_color_u8(params: &serde_json::Value, key: &str) -> Result<Option<[u8; 3]>, OpFailure> {
    match params.get(key).filter(|v| !v.is_null()) {
        None => Ok(None),
        Some(v) => serde_json::from_value::<[u8; 3]>(v.clone())
            .map(Some)
            .map_err(|_| op_param_error(format!("params.{key} must be [r, g, b] (0-255)"))),
    }
}

/// Optional `[f64; 3]` color param (0.0-1.0 components).
fn opt_color_f64(params: &serde_json::Value, key: &str) -> Result<Option<[f64; 3]>, OpFailure> {
    match params.get(key).filter(|v| !v.is_null()) {
        None => Ok(None),
        Some(v) => serde_json::from_value::<[f64; 3]>(v.clone())
            .map(Some)
            .map_err(|_| op_param_error(format!("params.{key} must be [r, g, b] (0.0-1.0)"))),
    }
}

/// Optional `[f32; 3]` color param.
fn opt_color_f32(params: &serde_json::Value, key: &str) -> Result<Option<[f32; 3]>, OpFailure> {
    Ok(opt_color_f64(params, key)?.map(|c| [c[0] as f32, c[1] as f32, c[2] as f32]))
}

/// Optional array-of-strings param.
fn opt_str_vec(params: &serde_json::Value, key: &str) -> Result<Option<Vec<String>>, OpFailure> {
    match params.get(key).filter(|v| !v.is_null()) {
        None => Ok(None),
        Some(v) => {
            let arr = v.as_array().ok_or_else(|| {
                op_param_error(format!("params.{key} must be an array of strings"))
            })?;
            arr.iter()
                .map(|item| {
                    item.as_str().map(str::to_string).ok_or_else(|| {
                        op_param_error(format!("params.{key} must be an array of strings"))
                    })
                })
                .collect::<Result<Vec<_>, _>>()
                .map(Some)
        }
    }
}

/// Optional base64 param: the first present of `keys` is decoded.
fn b64_param(params: &serde_json::Value, keys: &[&str]) -> Result<Option<Vec<u8>>, OpFailure> {
    for key in keys {
        if let Some(s) = opt_str(params, key) {
            return B64
                .decode(s.trim())
                .map(Some)
                .map_err(|e| op_param_error(format!("params.{key} is not valid base64: {e}")));
        }
    }
    Ok(None)
}

/// Optional hex param: the first present of `keys` is decoded.
fn hex_param(params: &serde_json::Value, keys: &[&str]) -> Result<Option<Vec<u8>>, OpFailure> {
    for key in keys {
        if let Some(s) = opt_str(params, key) {
            return hex::decode(s.trim())
                .map(Some)
                .map_err(|e| op_param_error(format!("params.{key} is not valid hex: {e}")));
        }
    }
    Ok(None)
}

/// Decodes extra PDF inputs for the multi-input ops (`merge`,
/// `interleave`, `merge_mode`, `create_portfolio`, `overlay`).
///
/// A `document.pdf_op` job carries exactly one artifact (plus `other`
/// for `compare`), so additional inputs must be inlined in `params`:
/// `params.inputs` — an array of base64 strings or objects with a
/// `data_b64`/`content_b64`/`pdf_b64` field — and/or a single
/// `params.other_pdf_b64`/`params.second_pdf_b64`. `other` (the
/// resolved comparison artifact) is appended last when present.
fn extra_pdf_inputs(
    params: &serde_json::Value,
    other: Option<&[u8]>,
) -> Result<Vec<Vec<u8>>, OpFailure> {
    let mut out = Vec::new();
    if let Some(items) = params.get("inputs").and_then(|v| v.as_array()) {
        for (i, item) in items.iter().enumerate() {
            let bytes = match item {
                serde_json::Value::String(s) => B64.decode(s.trim()).map_err(|e| {
                    op_param_error(format!("params.inputs[{i}] is not valid base64: {e}"))
                })?,
                serde_json::Value::Object(_) => {
                    let b64 = item
                        .get("data_b64")
                        .or_else(|| item.get("content_b64"))
                        .or_else(|| item.get("pdf_b64"))
                        .and_then(|v| v.as_str())
                        .ok_or_else(|| {
                            op_param_error(format!(
                                "params.inputs[{i}] requires a data_b64/content_b64/pdf_b64 field"
                            ))
                        })?;
                    B64.decode(b64.trim()).map_err(|e| {
                        op_param_error(format!("params.inputs[{i}] is not valid base64: {e}"))
                    })?
                }
                _ => {
                    return Err(op_param_error(format!(
                        "params.inputs[{i}] must be a base64 string or an object"
                    )));
                }
            };
            out.push(bytes);
        }
    }
    for key in ["other_pdf_b64", "second_pdf_b64"] {
        if let Some(b) = b64_param(params, &[key])? {
            out.push(b);
            break;
        }
    }
    if let Some(b) = other {
        out.push(b.to_vec());
    }
    Ok(out)
}

/// Wraps multiple PDF outputs (`split`-style fan-out) into an
/// [`OpOutcome`].
fn multi_pdf_outcome(
    op: &str,
    artifact_id: &str,
    parts: Vec<Vec<u8>>,
) -> Result<OpOutcome, OpFailure> {
    let files = parts
        .into_iter()
        .enumerate()
        .map(|(i, bytes)| OpFile {
            file_name: format!("{op}-{artifact_id}-{}.pdf", i + 1),
            bytes,
            ext: "pdf",
            mime: "application/pdf",
        })
        .collect();
    Ok(OpOutcome::Done(OpSuccess {
        report: None,
        files,
    }))
}

/// Wraps arbitrary file bytes (not necessarily a PDF) into a
/// single-file [`OpOutcome`].
fn file_outcome(
    op: &str,
    artifact_id: &str,
    bytes: Vec<u8>,
    ext: &'static str,
    mime: &'static str,
) -> Result<OpOutcome, OpFailure> {
    Ok(OpOutcome::Done(OpSuccess {
        report: None,
        files: vec![OpFile {
            file_name: format!("{op}-{artifact_id}.{ext}"),
            bytes,
            ext,
            mime,
        }],
    }))
}

/// Serializes `report` to JSON with the embedded output-payload
/// fields (`bytes`, `pdf`) removed — those bytes are already stored
/// as output files, and embedding them would duplicate the document
/// inside the report.
fn report_sans_bytes<T: serde::Serialize>(report: &T) -> Result<serde_json::Value, OpFailure> {
    let mut value = serde_json::to_value(report)
        .map_err(|e| op_param_error(format!("failed to serialize op report: {e}")))?;
    if let Some(obj) = value.as_object_mut() {
        obj.remove("bytes");
        obj.remove("pdf");
    }
    Ok(value)
}

/// Builds an [`pe::AnnotFilter`] from `params` (merged): `pages`,
/// `subtypes` (array of `/Subtype` names) and `all`.
fn annot_filter_param(params: &serde_json::Value) -> Result<pe::AnnotFilter, OpFailure> {
    let p = merged_params(params);
    Ok(pe::AnnotFilter {
        pages: opt_str(&p, "pages"),
        subtypes: opt_str_vec(&p, "subtypes")?,
        all: opt_bool(&p, "all", false)?,
    })
}

/// Parses `params.values`/`params.fields` into `(name, value)` pairs
/// for [`PdfEngine::fill_form`]. Accepts an array of
/// `{"name"|"field", "value"}` objects or `[name, value]` pairs, or an
/// object map of `{"field": "value"}`.
fn form_values_param(params: &serde_json::Value) -> Result<Vec<(String, String)>, OpFailure> {
    let p = merged_params(params);
    let raw = p
        .get("values")
        .or_else(|| p.get("fields"))
        .or_else(|| p.get("form_values"))
        .filter(|v| !v.is_null())
        .ok_or_else(|| {
            op_param_error("params.values must be an array of {name, value} pairs or an object map")
        })?;
    let mut out = Vec::new();
    match raw {
        serde_json::Value::Object(map) => {
            for (name, value) in map {
                let value = value.as_str().ok_or_else(|| {
                    op_param_error(format!("form value for {name:?} must be a string"))
                })?;
                out.push((name.clone(), value.to_string()));
            }
        }
        serde_json::Value::Array(items) => {
            for (i, item) in items.iter().enumerate() {
                match item {
                    serde_json::Value::Object(obj) => {
                        let name = obj
                            .get("name")
                            .or_else(|| obj.get("field"))
                            .and_then(|v| v.as_str())
                            .ok_or_else(|| {
                                op_param_error(format!(
                                    "params.values[{i}] requires a name/field string"
                                ))
                            })?;
                        let value = obj.get("value").and_then(|v| v.as_str()).unwrap_or("");
                        out.push((name.to_string(), value.to_string()));
                    }
                    serde_json::Value::Array(pair) if pair.len() == 2 => {
                        let name = pair[0].as_str().ok_or_else(|| {
                            op_param_error(format!("params.values[{i}] name must be a string"))
                        })?;
                        let value = pair[1].as_str().ok_or_else(|| {
                            op_param_error(format!("params.values[{i}] value must be a string"))
                        })?;
                        out.push((name.to_string(), value.to_string()));
                    }
                    _ => {
                        return Err(op_param_error(format!(
                            "params.values[{i}] must be an object or a [name, value] pair"
                        )));
                    }
                }
            }
        }
        _ => {
            return Err(op_param_error(
                "params.values must be an array or an object map",
            ));
        }
    }
    Ok(out)
}

/// Parses `params.labels` into `(page, label)` pairs for
/// [`PdfEngine::set_page_labels`]. Accepts `[[page, label], ...]`
/// pairs or `[{"page": n, "label": "..."}]` objects (`text`/`name`
/// are accepted label aliases).
fn page_labels_param(params: &serde_json::Value) -> Result<Vec<(u32, String)>, OpFailure> {
    let p = merged_params(params);
    let raw = p
        .get("labels")
        .or_else(|| p.get("page_labels"))
        .filter(|v| !v.is_null())
        .ok_or_else(|| op_param_error("params.labels is required"))?;
    let items = raw
        .as_array()
        .ok_or_else(|| op_param_error("params.labels must be an array"))?;
    let mut out = Vec::with_capacity(items.len());
    for (i, item) in items.iter().enumerate() {
        if let Some(obj) = item.as_object() {
            let page = obj
                .get("page")
                .or_else(|| obj.get("page_number"))
                .and_then(|v| v.as_u64())
                .and_then(|n| u32::try_from(n).ok())
                .ok_or_else(|| {
                    op_param_error(format!(
                        "params.labels[{i}].page must be a positive integer"
                    ))
                })?;
            let label = obj
                .get("label")
                .or_else(|| obj.get("text"))
                .or_else(|| obj.get("name"))
                .and_then(|v| v.as_str())
                .ok_or_else(|| {
                    op_param_error(format!("params.labels[{i}] requires a label/text string"))
                })?;
            out.push((page, label.to_string()));
        } else {
            let pair: (u32, String) = serde_json::from_value(item.clone()).map_err(|_| {
                op_param_error(format!(
                    "params.labels[{i}] must be [page, label] or an object"
                ))
            })?;
            out.push(pair);
        }
    }
    Ok(out)
}

/// Serde helper for [`pe::OutlineItemSpec`] with `level`/`bold`/
/// `italic` defaulting — the engine type requires them, which is
/// awkward over JSON.
#[derive(serde::Deserialize)]
struct OutlineItemParam {
    title: String,
    page: u32,
    #[serde(default)]
    level: u32,
    #[serde(default)]
    bold: bool,
    #[serde(default)]
    italic: bool,
}

/// Parses `params.items`/`params.outline` into
/// [`pe::OutlineItemSpec`] rows for [`PdfEngine::set_outline`].
fn outline_items_param(params: &serde_json::Value) -> Result<Vec<pe::OutlineItemSpec>, OpFailure> {
    let p = merged_params(params);
    let items = param_as::<Vec<OutlineItemParam>>(&p, &["items", "outline", "outline_items"])?
        .ok_or_else(|| op_param_error("params.items is required"))?;
    Ok(items
        .into_iter()
        .map(|i| pe::OutlineItemSpec {
            title: i.title,
            page: i.page,
            level: i.level,
            bold: i.bold,
            italic: i.italic,
        })
        .collect())
}

/// Parses `params.position` (+ `params.at`/`params.anchor_page`/
/// `params.page` anchor) into an [`pe::InsertPos`] for
/// [`PdfEngine::insert_blank`]. Defaults to [`pe::InsertPos::End`].
fn insert_pos_param(params: &serde_json::Value) -> Result<pe::InsertPos, OpFailure> {
    let p = merged_params(params);
    let anchor = opt_u32(&p, "at")?
        .or(opt_u32(&p, "anchor_page")?)
        .or(opt_u32(&p, "page")?);
    match opt_str(&p, "position")
        .unwrap_or_else(|| "end".to_string())
        .to_ascii_lowercase()
        .as_str()
    {
        "end" | "append" | "after_last" => Ok(pe::InsertPos::End),
        "before" => anchor.map(pe::InsertPos::Before).ok_or_else(|| {
            op_param_error("insert_blank position=before requires params.at (1-based page)")
        }),
        "after" => anchor.map(pe::InsertPos::After).ok_or_else(|| {
            op_param_error("insert_blank position=after requires params.at (1-based page)")
        }),
        "at" => anchor.map(pe::InsertPos::At).ok_or_else(|| {
            op_param_error("insert_blank position=at requires params.at (1-based page)")
        }),
        other => Err(op_param_error(format!(
            "params.position must be before, after, at, or end, got {other:?}"
        ))),
    }
}

/// Parses `params.position` (+ anchor params) into a [`pe::DupPos`]
/// for [`PdfEngine::duplicate_pages`]. Defaults to
/// [`pe::DupPos::InPlace`].
fn dup_pos_param(params: &serde_json::Value) -> Result<pe::DupPos, OpFailure> {
    let p = merged_params(params);
    let anchor = opt_u32(&p, "at")?
        .or(opt_u32(&p, "anchor_page")?)
        .or(opt_u32(&p, "page")?);
    match opt_str(&p, "position")
        .unwrap_or_else(|| "in_place".to_string())
        .to_ascii_lowercase()
        .as_str()
    {
        "in_place" | "inplace" | "each" => Ok(pe::DupPos::InPlace),
        "end" | "append" | "after_last" => Ok(pe::DupPos::End),
        "after" => anchor.map(pe::DupPos::After).ok_or_else(|| {
            op_param_error("duplicate_pages position=after requires params.at (1-based page)")
        }),
        "before" => anchor.map(pe::DupPos::Before).ok_or_else(|| {
            op_param_error("duplicate_pages position=before requires params.at (1-based page)")
        }),
        other => Err(op_param_error(format!(
            "params.position must be in_place, before, after, or end, got {other:?}"
        ))),
    }
}

/// Parses `params.mode` into a [`pe::MergeMode`] for
/// [`PdfEngine::merge_mode`]. Defaults to
/// [`pe::MergeMode::Sequential`].
fn merge_mode_param(params: &serde_json::Value) -> Result<pe::MergeMode, OpFailure> {
    let p = merged_params(params);
    match opt_str(&p, "mode")
        .or_else(|| opt_str(&p, "merge_mode"))
        .unwrap_or_else(|| "sequential".to_string())
        .to_ascii_lowercase()
        .as_str()
    {
        "sequential" | "concat" | "append" | "merge" => Ok(pe::MergeMode::Sequential),
        "interleave" | "round_robin" | "roundrobin" => Ok(pe::MergeMode::Interleave),
        "reverse" | "reversed" => Ok(pe::MergeMode::Reverse),
        other => Err(op_param_error(format!(
            "params.mode must be sequential, interleave, or reverse, got {other:?}"
        ))),
    }
}

/// Parses `params.format` into an [`pe::ImageInputFormat`] for
/// [`PdfEngine::replace_image`]. Defaults to [`pe::ImageInputFormat::Auto`].
fn image_format_param(params: &serde_json::Value) -> Result<pe::ImageInputFormat, OpFailure> {
    match opt_str(params, "format")
        .unwrap_or_else(|| "auto".to_string())
        .to_ascii_lowercase()
        .as_str()
    {
        "auto" => Ok(pe::ImageInputFormat::Auto),
        "png" => Ok(pe::ImageInputFormat::Png),
        "jpeg" | "jpg" => Ok(pe::ImageInputFormat::Jpeg),
        other => Err(op_param_error(format!(
            "params.format must be auto, png, or jpeg, got {other:?}"
        ))),
    }
}

/// Parses `params.algorithm` (or `params.hash`) into a
/// [`pe::TimestampAlgorithm`]; defaults to SHA-256.
fn timestamp_algorithm_param(
    params: &serde_json::Value,
) -> Result<pe::TimestampAlgorithm, OpFailure> {
    Ok(enum_param_keys::<pe::TimestampAlgorithm>(
        params,
        &["algorithm", "hash", "digest_algorithm"],
    )?
    .unwrap_or(pe::TimestampAlgorithm::Sha256))
}

/// Parses `params.level` into a [`pe::PdfaLevel`]; defaults to
/// PDF/A-2b. Accepts `pdfa-1b`/`pdfa-2b`/`pdfa-3b` style names.
fn pdfa_level_param(params: &serde_json::Value) -> Result<pe::PdfaLevel, OpFailure> {
    let raw = opt_str(params, "level").unwrap_or_else(|| "pdfa-2b".to_string());
    match raw
        .to_ascii_lowercase()
        .replace(['-', '_', ' '], "")
        .as_str()
    {
        "1b" | "pdfa1b" | "pdfa1" => Ok(pe::PdfaLevel::PdfA1b),
        "2b" | "pdfa2b" | "pdfa2" => Ok(pe::PdfaLevel::PdfA2b),
        "3b" | "pdfa3b" | "pdfa3" => Ok(pe::PdfaLevel::PdfA3b),
        other => Err(op_param_error(format!(
            "params.level must be pdfa-1b, pdfa-2b, or pdfa-3b, got {other:?}"
        ))),
    }
}

/// Parses `params.level` into a [`pe::PdfuaLevel`]; defaults to
/// PDF/UA-1.
fn pdfua_level_param(params: &serde_json::Value) -> Result<pe::PdfuaLevel, OpFailure> {
    let raw = opt_str(params, "level").unwrap_or_else(|| "pdfua-1".to_string());
    match raw
        .to_ascii_lowercase()
        .replace(['-', '_', ' '], "")
        .as_str()
    {
        "1" | "pdfua1" => Ok(pe::PdfuaLevel::PdfUa1),
        other => Err(op_param_error(format!(
            "params.level must be pdfua-1, got {other:?}"
        ))),
    }
}

/// Parses `params.level` into a [`pe::PdfxLevel`]; defaults to
/// PDF/X-3.
fn pdfx_level_param(params: &serde_json::Value) -> Result<pe::PdfxLevel, OpFailure> {
    let raw = opt_str(params, "level").unwrap_or_else(|| "pdfx-3".to_string());
    match raw
        .to_ascii_lowercase()
        .replace(['-', '_', ' '], "")
        .as_str()
    {
        "1a" | "pdfx1a" | "pdfx1" => Ok(pe::PdfxLevel::Pdfx1a),
        "3" | "pdfx3" => Ok(pe::PdfxLevel::Pdfx3),
        other => Err(op_param_error(format!(
            "params.level must be pdfx-1a or pdfx-3, got {other:?}"
        ))),
    }
}

/// Builds [`pe::SignCredentials`] from `params`.
///
/// Path-based inputs (`cert_path`+`key_path`, or `p12_path`) are
/// passed through to `openssl`. Inline PEM (`cert_pem`+`key_pem`) is
/// staged into a temp dir whose guard is returned alongside so the
/// files outlive the `sign` call. Anything else is a param error —
/// signing is never faked.
fn sign_credentials_param(
    params: &serde_json::Value,
) -> Result<(pe::SignCredentials, Option<tempfile::TempDir>), OpFailure> {
    let p = merged_params(params);
    let key_password = opt_str(&p, "key_password").or_else(|| opt_str(&p, "private_key_password"));
    if let (Some(cert), Some(key)) = (opt_str(&p, "cert_path"), opt_str(&p, "key_path")) {
        return Ok((
            pe::SignCredentials::Pem {
                certificate: PathBuf::from(cert),
                private_key: PathBuf::from(key),
                private_key_password: key_password,
            },
            None,
        ));
    }
    if let Some(p12) = opt_str(&p, "p12_path")
        .or_else(|| opt_str(&p, "pkcs12_path"))
        .or_else(|| opt_str(&p, "pfx_path"))
    {
        return Ok((
            pe::SignCredentials::Pkcs12 {
                path: PathBuf::from(p12),
                password: opt_str(&p, "p12_password")
                    .or_else(|| opt_str(&p, "pkcs12_password"))
                    .or_else(|| opt_str(&p, "password")),
            },
            None,
        ));
    }
    if let (Some(cert_pem), Some(key_pem)) = (opt_str(&p, "cert_pem"), opt_str(&p, "key_pem")) {
        let dir = tempfile::tempdir()
            .map_err(|e| op_param_error(format!("failed to stage signing material: {e}")))?;
        let cert_path = dir.path().join("cert.pem");
        let key_path = dir.path().join("key.pem");
        std::fs::write(&cert_path, cert_pem.as_bytes())
            .map_err(|e| op_param_error(format!("failed to write certificate PEM: {e}")))?;
        std::fs::write(&key_path, key_pem.as_bytes())
            .map_err(|e| op_param_error(format!("failed to write private key PEM: {e}")))?;
        return Ok((
            pe::SignCredentials::Pem {
                certificate: cert_path,
                private_key: key_path,
                private_key_password: key_password,
            },
            Some(dir),
        ));
    }
    Err(op_param_error(
        "sign requires params.cert_path+params.key_path, params.p12_path, \
         or params.cert_pem+params.key_pem",
    ))
}

/// Reads a required f32 shape coordinate from a [`pe::Shape`] params
/// object.
fn shape_f32(item: &serde_json::Value, key: &str) -> Result<f32, OpFailure> {
    item.get(key)
        .and_then(|v| v.as_f64())
        .map(|v| v as f32)
        .ok_or_else(|| op_param_error(format!("shape param {key} must be a number")))
}

/// Reads an optional f32 shape coordinate.
fn shape_opt_f32(item: &serde_json::Value, key: &str) -> Result<Option<f32>, OpFailure> {
    match item.get(key).filter(|v| !v.is_null()) {
        None => Ok(None),
        Some(v) => v
            .as_f64()
            .map(|f| Some(f as f32))
            .ok_or_else(|| op_param_error(format!("shape param {key} must be a number"))),
    }
}

/// Parses one [`pe::ShapeStyle`] from a shape params object — a
/// `style` sub-object wins, otherwise inline
/// `stroke`/`fill`/`line_width`/`opacity` keys are read.
fn shape_style_param(item: &serde_json::Value) -> Result<pe::ShapeStyle, OpFailure> {
    let src = item.get("style").filter(|v| v.is_object()).unwrap_or(item);
    let stroke = match src.get("stroke").filter(|v| !v.is_null()) {
        None => [0, 0, 0],
        Some(v) => serde_json::from_value::<[u8; 3]>(v.clone())
            .map_err(|_| op_param_error("shape stroke must be [r, g, b] (0-255)"))?,
    };
    let fill = match src.get("fill").filter(|v| !v.is_null()) {
        None => None,
        Some(v) => Some(
            serde_json::from_value::<[u8; 3]>(v.clone())
                .map_err(|_| op_param_error("shape fill must be [r, g, b] (0-255)"))?,
        ),
    };
    let line_width = match src
        .get("line_width")
        .or_else(|| src.get("stroke_width"))
        .filter(|v| !v.is_null())
    {
        None => 1.0,
        Some(v) => v
            .as_f64()
            .map(|f| f as f32)
            .ok_or_else(|| op_param_error("shape line_width must be a number"))?,
    };
    let opacity = match src.get("opacity").filter(|v| !v.is_null()) {
        None => None,
        Some(v) => Some(
            v.as_f64()
                .map(|f| f as f32)
                .ok_or_else(|| op_param_error("shape opacity must be a number"))?,
        ),
    };
    Ok(pe::ShapeStyle {
        stroke,
        fill,
        line_width,
        opacity,
    })
}

/// Parses `params.shapes` (+ optional `pages`) into a
/// [`pe::DrawSpec`] for [`PdfEngine::draw_shapes`]. Each shape object
/// carries a `type`/`kind` (`rectangle`, `line`, `ellipse`, `arrow`)
/// plus its geometry fields and optional style keys.
fn draw_spec_param(params: &serde_json::Value) -> Result<pe::DrawSpec, OpFailure> {
    let p = merged_params(params);
    let items = p
        .get("shapes")
        .and_then(|v| v.as_array())
        .filter(|a| !a.is_empty())
        .ok_or_else(|| op_param_error("params.shapes must be a non-empty array"))?;
    let mut shapes = Vec::with_capacity(items.len());
    for (i, item) in items.iter().enumerate() {
        if !item.is_object() {
            return Err(op_param_error(format!(
                "params.shapes[{i}] must be an object"
            )));
        }
        let style = shape_style_param(item)?;
        let kind = item
            .get("type")
            .or_else(|| item.get("kind"))
            .and_then(|v| v.as_str())
            .unwrap_or("rectangle");
        let shape = match kind {
            "rectangle" | "rect" => pe::Shape::Rectangle {
                x: shape_f32(item, "x")?,
                y: shape_f32(item, "y")?,
                width: shape_f32(item, "width")?,
                height: shape_f32(item, "height")?,
                style,
            },
            "line" => pe::Shape::Line {
                x1: shape_f32(item, "x1")?,
                y1: shape_f32(item, "y1")?,
                x2: shape_f32(item, "x2")?,
                y2: shape_f32(item, "y2")?,
                style,
            },
            "ellipse" | "circle" => pe::Shape::Ellipse {
                cx: shape_f32(item, "cx")?,
                cy: shape_f32(item, "cy")?,
                rx: shape_f32(item, "rx")?,
                ry: shape_f32(item, "ry")?,
                style,
            },
            "arrow" => pe::Shape::Arrow {
                x1: shape_f32(item, "x1")?,
                y1: shape_f32(item, "y1")?,
                x2: shape_f32(item, "x2")?,
                y2: shape_f32(item, "y2")?,
                head_length: shape_opt_f32(item, "head_length")?,
                style,
            },
            other => {
                return Err(op_param_error(format!(
                    "params.shapes[{i}].type must be rectangle, line, ellipse, or arrow, \
                     got {other:?}"
                )));
            }
        };
        shapes.push(shape);
    }
    Ok(pe::DrawSpec {
        shapes,
        pages: opt_str(&p, "pages"),
    })
}

/// Wraps a serializable report into a successful [`OpOutcome`].
fn report_outcome<T: serde::Serialize>(value: &T) -> Result<OpOutcome, OpFailure> {
    let report = serde_json::to_value(value)
        .map_err(|e| op_param_error(format!("failed to serialize op report: {e}")))?;
    Ok(OpOutcome::Done(OpSuccess {
        report: Some(report),
        files: Vec::new(),
    }))
}

/// Wraps PDF output bytes into a successful single-file [`OpOutcome`].
fn pdf_outcome(op: &str, artifact_id: &str, bytes: Vec<u8>) -> Result<OpOutcome, OpFailure> {
    Ok(OpOutcome::Done(OpSuccess {
        report: None,
        files: vec![OpFile {
            file_name: format!("{op}-{artifact_id}.pdf"),
            bytes,
            ext: "pdf",
            mime: "application/pdf",
        }],
    }))
}

/// Wraps PDF output bytes plus a structured report into one
/// [`OpOutcome`] — used by ops that both rewrite the document and
/// report what they changed (deskew angles, dedup stats, ...).
fn pdf_report_outcome<T: serde::Serialize>(
    op: &str,
    artifact_id: &str,
    bytes: Vec<u8>,
    report: &T,
) -> Result<OpOutcome, OpFailure> {
    let report = serde_json::to_value(report)
        .map_err(|e| op_param_error(format!("failed to serialize op report: {e}")))?;
    Ok(OpOutcome::Done(OpSuccess {
        report: Some(report),
        files: vec![OpFile {
            file_name: format!("{op}-{artifact_id}.pdf"),
            bytes,
            ext: "pdf",
            mime: "application/pdf",
        }],
    }))
}

/// Dispatches `op` against `input` (and `other` for `compare`) and
/// returns the outcome. All failures — engine errors and bad params —
/// come back as [`OpOutcome::Failed`]; this function never performs
/// I/O beyond the engine calls themselves.
fn run_pdf_op(
    engine: &crate::crates::pdf_engine::PdfEngine,
    input: &[u8],
    other: Option<&[u8]>,
    op: &str,
    params: &serde_json::Value,
    artifact_id: &str,
) -> OpOutcome {
    match dispatch_pdf_op(engine, input, other, op, params, artifact_id) {
        Ok(outcome) => outcome,
        Err(f) => OpOutcome::Failed(f),
    }
}

fn dispatch_pdf_op(
    engine: &crate::crates::pdf_engine::PdfEngine,
    input: &[u8],
    other: Option<&[u8]>,
    op: &str,
    params: &serde_json::Value,
    artifact_id: &str,
) -> Result<OpOutcome, OpFailure> {
    use crate::crates::pdf_engine as pe;
    let pages = pages_param(params);
    let pages_ref = pages.as_deref();
    match op {
        // -- reports ----------------------------------------------------
        "page_count" => {
            let n = engine.page_count(input).map_err(op_engine_error)?;
            report_outcome(&n)
        }
        "inspect" => {
            let info = engine.inspect(input).map_err(op_engine_error)?;
            report_outcome(&info)
        }
        "validate" => {
            let report = engine.validate(input).map_err(op_engine_error)?;
            report_outcome(&report)
        }
        "repair" => {
            let report = engine.repair(input).map_err(op_engine_error)?;
            report_outcome(&report)
        }
        "verify_signatures" => {
            let report = engine.verify_signatures(input).map_err(op_engine_error)?;
            report_outcome(&report)
        }
        "extract_metadata" => {
            let meta = engine.extract_metadata(input).map_err(op_engine_error)?;
            report_outcome(&meta)
        }
        "extract_text" => {
            let pages = engine
                .extract_text(input, pages_ref)
                .map_err(op_engine_error)?;
            report_outcome(&pages)
        }
        "extract_images" => {
            let images = engine
                .extract_images(input, pages_ref)
                .map_err(op_engine_error)?;
            report_outcome(&images)
        }
        "ocr" => {
            let pages = engine.ocr(input, pages_ref).map_err(op_engine_error)?;
            report_outcome(&pages)
        }
        "ocr_detailed" => {
            let options = pe::OcrOptions {
                dpi: opt_u32(params, "dpi")?,
                lang: opt_str(params, "lang"),
                psm: opt_u32(params, "psm")?,
            };
            let detail = engine
                .ocr_detailed(input, pages_ref, &options)
                .map_err(op_engine_error)?;
            report_outcome(&detail)
        }
        "compare" => {
            let b = other.ok_or_else(|| {
                op_param_error(
                    "compare requires params.other_artifact_id or params.other_document_id",
                )
            })?;
            let mode: pe::CompareMode =
                enum_param(params, "mode")?.unwrap_or(pe::CompareMode::Text);
            let report = engine.compare(input, b, mode).map_err(op_engine_error)?;
            report_outcome(&report)
        }
        "list_links" => {
            let links = engine.list_links(input).map_err(op_engine_error)?;
            report_outcome(&links)
        }
        "list_attachments" => {
            let files = engine.list_attachments(input).map_err(op_engine_error)?;
            report_outcome(&files)
        }
        "list_transitions" => {
            let transitions = engine.list_transitions(input).map_err(op_engine_error)?;
            report_outcome(&transitions)
        }
        "detect_orientation" => {
            let orientations = engine.detect_orientation(input).map_err(op_engine_error)?;
            report_outcome(&orientations)
        }
        "inspect_fonts" => {
            let fonts = engine.inspect_fonts(input).map_err(op_engine_error)?;
            report_outcome(&fonts)
        }
        "inspect_javascript" => {
            let scripts = engine.inspect_javascript(input).map_err(op_engine_error)?;
            report_outcome(&scripts)
        }
        "get_document_dates" => {
            let dates = engine.get_document_dates(input).map_err(op_engine_error)?;
            // `DocDates` doesn't derive Serialize — emit its fields.
            report_outcome(&json!({
                "creation_date": dates.creation_date,
                "mod_date": dates.mod_date,
            }))
        }
        "get_page_size" => {
            let sizes = engine.get_page_sizes(input).map_err(op_engine_error)?;
            // `PageSizeInfo` doesn't derive Serialize — emit its fields.
            let sizes: Vec<serde_json::Value> = sizes
                .iter()
                .map(|s| {
                    json!({
                        "page_number": s.page_number,
                        "width_pt": s.width_pt,
                        "height_pt": s.height_pt,
                        "media_box": s.media_box,
                        "has_crop_box": s.has_crop_box,
                    })
                })
                .collect();
            report_outcome(&sizes)
        }
        "list_layers" => {
            let layers = engine.list_layers(input).map_err(op_engine_error)?;
            report_outcome(&layers)
        }
        "list_named_destinations" => {
            let dests = engine
                .list_named_destinations(input)
                .map_err(op_engine_error)?;
            report_outcome(&dests)
        }
        "inspect_structure" => {
            let report = engine.inspect_structure(input).map_err(op_engine_error)?;
            report_outcome(&report)
        }
        "object_report" => {
            let report = engine.object_report(input).map_err(op_engine_error)?;
            report_outcome(&report)
        }
        "audit_fonts" => {
            let report = engine.audit_fonts(input).map_err(op_engine_error)?;
            report_outcome(&report)
        }
        "audit_content" => {
            let report = engine.audit_content(input).map_err(op_engine_error)?;
            report_outcome(&report)
        }
        "permissions_report" => {
            let report = engine.permissions_report(input).map_err(op_engine_error)?;
            report_outcome(&report)
        }
        "get_trim_boxes" => {
            let boxes = engine.get_trim_boxes(input).map_err(op_engine_error)?;
            // `TrimPageBoxInfo` doesn't derive Serialize — emit its fields.
            let boxes: Vec<serde_json::Value> = boxes
                .iter()
                .map(|b| {
                    json!({
                        "page": b.page,
                        "media": b.media,
                        "crop": b.crop,
                        "trim": b.trim,
                        "bleed": b.bleed,
                    })
                })
                .collect();
            report_outcome(&boxes)
        }
        "get_page_boxes" => {
            let pages = req_str(params, "pages")?;
            let boxes = engine
                .get_page_boxes(input, &pages)
                .map_err(op_engine_error)?;
            // `PageBoxInfo` doesn't derive Serialize — emit its fields.
            let boxes: Vec<serde_json::Value> = boxes
                .iter()
                .map(|b| json!({ "page": b.page, "boxes": &b.boxes }))
                .collect();
            report_outcome(&boxes)
        }
        "list_oc_marked_content" => {
            let usage = engine
                .list_oc_marked_content(input)
                .map_err(op_engine_error)?;
            report_outcome(&usage)
        }
        // -- byte outputs -------------------------------------------------
        "convert" => {
            let target: pe::ConvertTarget =
                enum_param(params, "target")?.unwrap_or(pe::ConvertTarget::Text);
            let bytes = engine
                .convert(input, target, pages_ref)
                .map_err(op_engine_error)?;
            let (ext, mime) = match target {
                pe::ConvertTarget::Text => ("txt", "text/plain"),
                pe::ConvertTarget::Markdown => ("md", "text/markdown"),
            };
            Ok(OpOutcome::Done(OpSuccess {
                report: None,
                files: vec![OpFile {
                    file_name: format!("{op}-{artifact_id}.{ext}"),
                    bytes,
                    ext,
                    mime,
                }],
            }))
        }
        "render" => {
            let format: pe::RenderFormat = match opt_str(params, "format").as_deref() {
                None => pe::RenderFormat::Png,
                Some("png") => pe::RenderFormat::Png,
                Some("jpeg" | "jpg") => pe::RenderFormat::Jpeg,
                Some(other) => {
                    return Err(op_param_error(format!(
                        "params.format must be png or jpeg, got {other:?}"
                    )));
                }
            };
            let dpi = opt_u32(params, "dpi")?;
            let bytes = engine
                .render(input, pages_ref, dpi, format)
                .map_err(op_engine_error)?;
            let (ext, mime) = match format {
                pe::RenderFormat::Png => ("png", "image/png"),
                pe::RenderFormat::Jpeg => ("jpg", "image/jpeg"),
            };
            Ok(OpOutcome::Done(OpSuccess {
                report: None,
                files: vec![OpFile {
                    file_name: format!("{op}-{artifact_id}.{ext}"),
                    bytes,
                    ext,
                    mime,
                }],
            }))
        }
        "render_pages" => {
            let format: pe::RasterFormat = match opt_str(params, "format").as_deref() {
                None | Some("png") => pe::RasterFormat::Png,
                Some("jpeg" | "jpg") => pe::RasterFormat::Jpeg,
                Some("tiff" | "tif") => pe::RasterFormat::Tiff,
                Some(other) => {
                    return Err(op_param_error(format!(
                        "params.format must be png, jpeg or tiff, got {other:?}"
                    )));
                }
            };
            let jpeg_quality = match opt_u32(params, "jpeg_quality")? {
                None => None,
                Some(q) if (1..=100).contains(&q) => Some(q as u8),
                Some(q) => {
                    return Err(op_param_error(format!(
                        "params.jpeg_quality must be in 1..=100, got {q}"
                    )));
                }
            };
            let options = pe::RenderPageOptions {
                format,
                grayscale: opt_bool(params, "grayscale", false)?,
                monochrome: opt_bool(params, "monochrome", false)?,
                region: param_as::<pe::RenderRegion>(params, &["region"])?,
                scale_to: opt_u32(params, "scale_to")?,
                jpeg_quality,
            };
            let rendered = engine
                .render_pages(input, pages_ref, opt_u32(params, "dpi")?, &options)
                .map_err(op_engine_error)?;
            if rendered.len() > MAX_OP_OUTPUTS {
                return Err(op_param_error(format!(
                    "render produced {} pages, exceeding the limit of {MAX_OP_OUTPUTS}",
                    rendered.len()
                )));
            }
            let (ext, mime) = match format {
                pe::RasterFormat::Png => ("png", "image/png"),
                pe::RasterFormat::Jpeg => ("jpg", "image/jpeg"),
                pe::RasterFormat::Tiff => ("tif", "image/tiff"),
            };
            let page_list: Vec<serde_json::Value> = rendered
                .iter()
                .map(|p| {
                    json!({
                        "page": p.page,
                        "width_px": p.width_px,
                        "height_px": p.height_px,
                    })
                })
                .collect();
            let files = rendered
                .into_iter()
                .map(|p| OpFile {
                    file_name: format!("{op}-{artifact_id}-p{}.{}", p.page, ext),
                    bytes: p.data,
                    ext,
                    mime,
                })
                .collect();
            Ok(OpOutcome::Done(OpSuccess {
                report: Some(json!({ "pages": page_list })),
                files,
            }))
        }
        "compress" => pdf_outcome(
            op,
            artifact_id,
            engine.compress(input).map_err(op_engine_error)?,
        ),
        "flatten" => pdf_outcome(
            op,
            artifact_id,
            engine.flatten(input).map_err(op_engine_error)?,
        ),
        "sanitize" => pdf_outcome(
            op,
            artifact_id,
            engine.sanitize(input).map_err(op_engine_error)?,
        ),
        "rotate" => {
            let degrees = req_u32(params, "degrees")?;
            let degrees = u16::try_from(degrees)
                .map_err(|_| op_param_error("params.degrees must fit in u16"))?;
            let pages = pages_param(params).unwrap_or_else(|| "all".to_string());
            let bytes = engine
                .rotate_degrees(input, &pages, degrees)
                .map_err(op_engine_error)?;
            pdf_outcome(op, artifact_id, bytes)
        }
        "split" => {
            let per_file = req_u32(params, "pages_per_file")?;
            let parts = engine.split(input, per_file).map_err(op_engine_error)?;
            let files = parts
                .into_iter()
                .enumerate()
                .map(|(i, bytes)| OpFile {
                    file_name: format!("{op}-{artifact_id}-{}.pdf", i + 1),
                    bytes,
                    ext: "pdf",
                    mime: "application/pdf",
                })
                .collect();
            Ok(OpOutcome::Done(OpSuccess {
                report: None,
                files,
            }))
        }
        "extract_pages" => {
            let pages = req_str(params, "pages")?;
            let bytes = engine
                .extract_pages(input, &pages)
                .map_err(op_engine_error)?;
            pdf_outcome(op, artifact_id, bytes)
        }
        "remove_pages" => {
            let pages = req_str(params, "pages")?;
            let bytes = engine
                .remove_pages(input, &pages)
                .map_err(op_engine_error)?;
            pdf_outcome(op, artifact_id, bytes)
        }
        "reorder" => {
            let order = req_str(params, "order")?;
            let bytes = engine.reorder(input, &order).map_err(op_engine_error)?;
            pdf_outcome(op, artifact_id, bytes)
        }
        "swap_pages" => {
            let p = merged_params(params);
            let a = opt_u32(&p, "a")?
                .or(opt_u32(&p, "page_a")?)
                .ok_or_else(|| op_param_error("swap_pages requires params.a"))?;
            let b = opt_u32(&p, "b")?
                .or(opt_u32(&p, "page_b")?)
                .ok_or_else(|| op_param_error("swap_pages requires params.b"))?;
            let bytes = engine.swap_pages(input, a, b).map_err(op_engine_error)?;
            pdf_outcome(op, artifact_id, bytes)
        }
        "watermark" => {
            let mut opts = pe::WatermarkOptions {
                text: req_str(params, "text")?,
                ..Default::default()
            };
            if let Some(v) = opt_f64(params, "font_size")? {
                opts.font_size = v as f32;
            }
            if let Some(v) = opt_f64(params, "opacity")? {
                opts.opacity = v as f32;
            }
            if let Some(v) = opt_f64(params, "rotation_degrees")? {
                opts.rotation_degrees = v as f32;
            }
            if let Some(p) = enum_param::<pe::MarkPosition>(params, "position")? {
                opts.position = p;
            }
            opts.pages = pages_param(params);
            if let Some(c) = params.get("color") {
                opts.color = Some(
                    serde_json::from_value::<[u8; 3]>(c.clone())
                        .map_err(|_| op_param_error("params.color must be [r, g, b]"))?,
                );
            }
            let bytes = engine.watermark(input, &opts).map_err(op_engine_error)?;
            pdf_outcome(op, artifact_id, bytes)
        }
        "stamp" => {
            let mut opts = pe::StampOptions {
                text: opt_str(params, "text"),
                ..Default::default()
            };
            if let Some(b64) = opt_str(params, "image_png_b64") {
                opts.image_png = Some(B64.decode(b64.trim()).map_err(|e| {
                    op_param_error(format!("params.image_png_b64 is not valid base64: {e}"))
                })?);
            }
            if let Some(p) = enum_param::<pe::MarkPosition>(params, "position")? {
                opts.position = p;
            }
            if let Some(v) = opt_f64(params, "scale")? {
                opts.scale = v as f32;
            }
            if let Some(v) = opt_f64(params, "opacity")? {
                opts.opacity = v as f32;
            }
            if let Some(v) = opt_f64(params, "margin")? {
                opts.margin = v as f32;
            }
            opts.pages = pages_param(params);
            let bytes = engine.stamp(input, &opts).map_err(op_engine_error)?;
            pdf_outcome(op, artifact_id, bytes)
        }
        "encrypt" => {
            let user_password = req_str(params, "user_password")?;
            let owner_password = opt_str(params, "owner_password");
            let permissions = pe::PdfPermissions {
                allow_print: opt_bool(params, "allow_print", true)?,
                allow_copy: opt_bool(params, "allow_copy", true)?,
                allow_modify: opt_bool(params, "allow_modify", true)?,
                allow_annotations: opt_bool(params, "allow_annotations", true)?,
            };
            // Permission flags may also arrive nested under
            // `params.permissions` for callers that mirror the engine
            // type shape.
            let permissions = match params.get("permissions") {
                Some(pe_obj) if pe_obj.is_object() => pe::PdfPermissions {
                    allow_print: pe_obj
                        .get("allow_print")
                        .and_then(|v| v.as_bool())
                        .unwrap_or(permissions.allow_print),
                    allow_copy: pe_obj
                        .get("allow_copy")
                        .and_then(|v| v.as_bool())
                        .unwrap_or(permissions.allow_copy),
                    allow_modify: pe_obj
                        .get("allow_modify")
                        .and_then(|v| v.as_bool())
                        .unwrap_or(permissions.allow_modify),
                    allow_annotations: pe_obj
                        .get("allow_annotations")
                        .and_then(|v| v.as_bool())
                        .unwrap_or(permissions.allow_annotations),
                },
                _ => permissions,
            };
            let bytes = engine
                .encrypt(
                    input,
                    &user_password,
                    owner_password.as_deref(),
                    permissions,
                )
                .map_err(op_engine_error)?;
            pdf_outcome(op, artifact_id, bytes)
        }
        "decrypt" => {
            let password = req_str(params, "password")?;
            let bytes = engine.decrypt(input, &password).map_err(op_engine_error)?;
            pdf_outcome(op, artifact_id, bytes)
        }
        "redact" => {
            let terms: Vec<String> = params
                .get("terms")
                .and_then(|v| v.as_array())
                .map(|arr| {
                    arr.iter()
                        .filter_map(|v| v.as_str().map(str::to_string))
                        .collect()
                })
                .filter(|t: &Vec<String>| !t.is_empty())
                .ok_or_else(|| {
                    op_param_error("params.terms must be a non-empty array of strings")
                })?;
            let bytes = engine
                .redact(input, &terms, pages_ref)
                .map_err(op_engine_error)?;
            pdf_outcome(op, artifact_id, bytes)
        }
        // -- advanced ops: bytes (+ optional report) ----------------------
        "deskew" => {
            let max_angle = opt_f64(params, "max_angle")?.unwrap_or(5.0);
            let (bytes, report) = engine
                .deskew(input, pages_ref, max_angle)
                .map_err(op_engine_error)?;
            pdf_report_outcome(op, artifact_id, bytes, &report)
        }
        "clean_scan" => {
            let mode =
                enum_param::<pe::CleanMode>(params, "mode")?.unwrap_or(pe::CleanMode::Grayscale);
            let (bytes, report) = engine
                .clean_scan(input, pages_ref, mode)
                .map_err(op_engine_error)?;
            pdf_report_outcome(op, artifact_id, bytes, &report)
        }
        "bates" => {
            let mut spec = pe::BatesSpec {
                pages: pages.clone(),
                ..Default::default()
            };
            if let Some(v) = opt_str(params, "prefix") {
                spec.prefix = v;
            }
            if let Some(v) = opt_u32(params, "start")? {
                spec.start = u64::from(v);
            }
            if let Some(v) = opt_u32(params, "digits")? {
                spec.digits = v;
            }
            if let Some(p) = enum_param::<pe::MarkPosition>(params, "position")? {
                spec.position = p;
            }
            if let Some(v) = opt_f64(params, "font_size")? {
                spec.font_size = v as f32;
            }
            if let Some(c) = params.get("color") {
                spec.color = serde_json::from_value::<[u8; 3]>(c.clone())
                    .map_err(|_| op_param_error("params.color must be [r, g, b]"))?;
            }
            if let Some(v) = opt_f64(params, "opacity")? {
                spec.opacity = v as f32;
            }
            let bytes = engine.add_bates(input, &spec).map_err(op_engine_error)?;
            pdf_outcome(op, artifact_id, bytes)
        }
        "add_links" => {
            let links: Vec<pe::LinkSpec> = req_param_as(params, &["links", "spec"])?;
            let bytes = engine.add_links(input, &links).map_err(op_engine_error)?;
            pdf_outcome(op, artifact_id, bytes)
        }
        "remove_links" => {
            let bytes = engine
                .remove_links(input, pages_ref)
                .map_err(op_engine_error)?;
            pdf_outcome(op, artifact_id, bytes)
        }
        "extract_attachment" => {
            let name = req_str(params, "name")?;
            let bytes = engine
                .extract_attachment(input, &name)
                .map_err(op_engine_error)?;
            // The embedded payload is not necessarily a PDF — store it
            // as a generic binary blob named after the attachment.
            let stem: String = name
                .chars()
                .map(|c| {
                    if c.is_ascii_alphanumeric() || matches!(c, '.' | '-' | '_') {
                        c
                    } else {
                        '_'
                    }
                })
                .collect();
            Ok(OpOutcome::Done(OpSuccess {
                report: None,
                files: vec![OpFile {
                    file_name: format!("{op}-{artifact_id}-{stem}"),
                    bytes,
                    ext: "bin",
                    mime: "application/octet-stream",
                }],
            }))
        }
        "add_attachment" => {
            let mut spec = match param_as::<pe::AttachmentSpec>(params, &["spec"])? {
                Some(spec) => spec,
                None => pe::AttachmentSpec {
                    name: req_str(params, "name")?,
                    data: Vec::new(),
                    description: opt_str(params, "description"),
                },
            };
            if spec.name.trim().is_empty() {
                spec.name = req_str(params, "name")?;
            }
            // Raw bytes are more practical as base64 than a JSON array;
            // `params.spec.data` (array form) still works via serde.
            if let Some(b64) =
                opt_str(params, "data_b64").or_else(|| opt_str(params, "content_b64"))
            {
                spec.data = B64.decode(b64.trim()).map_err(|e| {
                    op_param_error(format!("attachment data is not valid base64: {e}"))
                })?;
            }
            if spec.data.is_empty() {
                return Err(op_param_error(
                    "add_attachment requires attachment bytes (params.data_b64 or params.spec.data)",
                ));
            }
            let bytes = engine
                .add_attachment(input, &spec)
                .map_err(op_engine_error)?;
            pdf_outcome(op, artifact_id, bytes)
        }
        "remove_attachment" => {
            let name = req_str(params, "name")?;
            let bytes = engine
                .remove_attachment(input, &name)
                .map_err(op_engine_error)?;
            pdf_outcome(op, artifact_id, bytes)
        }
        "dedup_images" => {
            let (bytes, report) = engine.dedup_images(input).map_err(op_engine_error)?;
            pdf_report_outcome(op, artifact_id, bytes, &report)
        }
        "recompress_streams" => {
            let level = opt_u32(params, "level")?.unwrap_or(9);
            let (bytes, report) = engine
                .recompress_streams(input, level)
                .map_err(op_engine_error)?;
            // `RecompressReport` doesn't derive Serialize — emit its fields.
            pdf_report_outcome(
                op,
                artifact_id,
                bytes,
                &json!({
                    "streams_seen": report.streams_seen,
                    "streams_recompressed": report.streams_recompressed,
                    "bytes_before": report.bytes_before,
                    "bytes_after": report.bytes_after,
                    "warnings": report.warnings,
                }),
            )
        }
        "remove_hidden_text" => {
            let (bytes, report) = engine
                .remove_hidden_text(input, pages_ref)
                .map_err(op_engine_error)?;
            pdf_report_outcome(op, artifact_id, bytes, &report)
        }
        "build_outline_from_text" => {
            let spec = heading_spec_param(params)?;
            let (bytes, report) = engine
                .build_outline_from_text(input, &spec)
                .map_err(op_engine_error)?;
            pdf_report_outcome(op, artifact_id, bytes, &report)
        }
        "apply_trim_preset" => {
            let pages = req_str(params, "pages")?;
            let preset = trim_preset_param(params)?;
            let bleed = opt_f64(params, "bleed")?.unwrap_or(pe::DEFAULT_BLEED_PT);
            let bytes = engine
                .apply_trim_preset(input, &pages, preset, bleed)
                .map_err(op_engine_error)?;
            pdf_outcome(op, artifact_id, bytes)
        }
        "set_page_box" => {
            let pages = req_str(params, "pages")?;
            let which = page_box_param(params)?;
            let rect = rect4_param(params, "rect")?;
            let bytes = engine
                .set_page_box(input, &pages, which, rect)
                .map_err(op_engine_error)?;
            pdf_outcome(op, artifact_id, bytes)
        }
        "remove_page_box" => {
            let pages = req_str(params, "pages")?;
            let which = page_box_param(params)?;
            let bytes = engine
                .remove_page_box(input, &pages, which)
                .map_err(op_engine_error)?;
            pdf_outcome(op, artifact_id, bytes)
        }
        "set_transition" => {
            let pages = req_str(params, "pages")?;
            let spec = transition_spec_param(params)?;
            let bytes = engine
                .set_transition(input, &pages, &spec)
                .map_err(op_engine_error)?;
            pdf_outcome(op, artifact_id, bytes)
        }
        "clear_transitions" => {
            let bytes = engine
                .clear_transitions(input, pages_ref)
                .map_err(op_engine_error)?;
            pdf_outcome(op, artifact_id, bytes)
        }
        "normalize_orientation" => {
            let mode = orient_mode_param(params)?;
            let bytes = engine
                .normalize_orientation(input, mode)
                .map_err(op_engine_error)?;
            pdf_outcome(op, artifact_id, bytes)
        }
        "redact_regions" => {
            let regions: Vec<pe::Redaction> = req_param_as(params, &["regions", "spec"])?;
            let label = opt_str(params, "label");
            let (bytes, report) = engine
                .redact_regions(input, &regions, label.as_deref())
                .map_err(op_engine_error)?;
            pdf_report_outcome(op, artifact_id, bytes, &report)
        }
        "redact_pages" => {
            let pages = req_str(params, "pages")?;
            let replacement = redact_replacement_param(params)?;
            let bytes = engine
                .redact_pages(input, &pages, replacement)
                .map_err(op_engine_error)?;
            pdf_outcome(op, artifact_id, bytes)
        }
        "set_document_dates" => {
            let creation = opt_str(params, "creation_date").or_else(|| opt_str(params, "creation"));
            let mod_date = opt_str(params, "mod_date")
                .or_else(|| opt_str(params, "modification_date"))
                .or_else(|| opt_str(params, "modified"));
            let bytes = engine
                .set_document_dates(input, creation.as_deref(), mod_date.as_deref())
                .map_err(op_engine_error)?;
            pdf_outcome(op, artifact_id, bytes)
        }
        "set_page_size" => {
            let pages = req_str(params, "pages")?;
            let spec = pe::PageSizeSpec {
                width: req_f64(params, "width")?,
                height: req_f64(params, "height")?,
                unit: size_unit_param(params)?,
                scale_content: opt_bool(params, "scale_content", false)?,
            };
            let bytes = engine
                .set_page_size(input, &pages, &spec)
                .map_err(op_engine_error)?;
            pdf_outcome(op, artifact_id, bytes)
        }
        "set_layer_visibility" => {
            let names: Vec<String> = req_param_as(params, &["names", "layers"])?;
            let visible = opt_bool(params, "visible", true)?;
            let bytes = engine
                .set_layer_visibility(input, &names, visible)
                .map_err(op_engine_error)?;
            pdf_outcome(op, artifact_id, bytes)
        }
        "remove_oc_properties" => {
            let bytes = engine
                .remove_oc_properties(input)
                .map_err(op_engine_error)?;
            pdf_outcome(op, artifact_id, bytes)
        }
        "add_named_destinations" => {
            let dests: Vec<pe::NamedDest> =
                req_param_as(params, &["destinations", "additions", "spec"])?;
            let bytes = engine
                .add_named_destinations(input, &dests)
                .map_err(op_engine_error)?;
            pdf_outcome(op, artifact_id, bytes)
        }
        "remove_named_destination" => {
            let name = req_str(params, "name")?;
            let bytes = engine
                .remove_named_destination(input, &name)
                .map_err(op_engine_error)?;
            pdf_outcome(op, artifact_id, bytes)
        }
        "edit_field_props" => {
            let patches: Vec<pe::FieldPatch> = req_param_as(params, &["patches", "spec"])?;
            let (bytes, report) = engine
                .edit_field_props(input, &patches)
                .map_err(op_engine_error)?;
            pdf_report_outcome(op, artifact_id, bytes, &report)
        }
        // -- backend-only ops: annotations / freehand / markup ---------
        "add_comment" => {
            let p = merged_params(params);
            let spec = match param_as::<pe::CommentSpec>(&p, &["spec"])? {
                Some(spec) => spec,
                None => pe::CommentSpec {
                    text: req_str(&p, "text")?,
                    pages: pages_param(&p),
                    rect: opt_rect4(&p, &["rect"])?,
                    author: opt_str(&p, "author"),
                    icon: enum_param(&p, "icon")?,
                },
            };
            let bytes = engine.add_comment(input, &spec).map_err(op_engine_error)?;
            pdf_outcome(op, artifact_id, bytes)
        }
        "add_freetext" => {
            let p = merged_params(params);
            let spec = match param_as::<pe::FreeTextSpec>(&p, &["spec"])? {
                Some(spec) => spec,
                None => pe::FreeTextSpec {
                    text: req_str(&p, "text")?,
                    pages: pages_param(&p),
                    rect: opt_rect4(&p, &["rect"])?,
                    author: opt_str(&p, "author"),
                    font_size: opt_f64(&p, "font_size")?,
                    color: opt_color_f64(&p, "color")?,
                    da: opt_str(&p, "da"),
                },
            };
            let bytes = engine.add_freetext(input, &spec).map_err(op_engine_error)?;
            pdf_outcome(op, artifact_id, bytes)
        }
        "list_annotations" => {
            let annotations = engine.list_annotations(input).map_err(op_engine_error)?;
            report_outcome(&annotations)
        }
        "remove_annotations" => {
            let filter = annot_filter_param(params)?;
            let bytes = engine
                .remove_annotations(input, &filter)
                .map_err(op_engine_error)?;
            pdf_outcome(op, artifact_id, bytes)
        }
        "list_articles" => {
            let articles = engine.list_articles(input).map_err(op_engine_error)?;
            report_outcome(&articles)
        }
        "remove_articles" => {
            let bytes = engine.remove_articles(input).map_err(op_engine_error)?;
            pdf_outcome(op, artifact_id, bytes)
        }
        "add_ink" => {
            let p = merged_params(params);
            let spec = pe::InkSpec {
                strokes: req_param_as(&p, &["strokes"])?,
                pages: opt_str(&p, "pages"),
                color: opt_color_u8(&p, "color")?.unwrap_or([0, 0, 0]),
                width: opt_f64(&p, "width")?.unwrap_or(1.0) as f32,
            };
            let bytes = engine.add_ink(input, &spec).map_err(op_engine_error)?;
            pdf_outcome(op, artifact_id, bytes)
        }
        "list_ink" => {
            let ink = engine.list_ink(input).map_err(op_engine_error)?;
            report_outcome(&ink)
        }
        "remove_ink" => {
            let filter = annot_filter_param(params)?;
            let bytes = engine.remove_ink(input, &filter).map_err(op_engine_error)?;
            pdf_outcome(op, artifact_id, bytes)
        }
        "text_markup" => {
            let p = merged_params(params);
            let pages_owned = pages_param(&p);
            let kind = enum_param_keys::<pe::MarkupKind>(&p, &["kind", "markup", "markup_kind"])?
                .ok_or_else(|| {
                op_param_error("params.kind is required (highlight, underline, or strike)")
            })?;
            let color = opt_color_u8(&p, "color")?;
            let bytes = engine
                .text_markup(input, kind, pages_owned.as_deref(), color)
                .map_err(op_engine_error)?;
            pdf_outcome(op, artifact_id, bytes)
        }
        "edit_text" => {
            let p = merged_params(params);
            let edit = match param_as::<pe::TextEdit>(&p, &["spec", "options", "edit"])? {
                Some(edit) => edit,
                None => {
                    // `replace` may legitimately be an empty string
                    // (deletion), so it is not read via `req_str`.
                    let replace = p
                        .get("replace")
                        .and_then(|v| v.as_str())
                        .map(str::to_string)
                        .ok_or_else(|| {
                            op_param_error("params.replace is required (may be empty)")
                        })?;
                    pe::TextEdit {
                        find: req_str(&p, "find")?,
                        replace,
                        pages: opt_str(&p, "pages"),
                        case_sensitive: opt_bool(&p, "case_sensitive", true)?,
                    }
                }
            };
            let outcome = engine.edit_text(input, &edit).map_err(op_engine_error)?;
            pdf_report_outcome(op, artifact_id, outcome.bytes, &outcome.report)
        }
        // -- backend-only ops: forms ------------------------------------
        "list_form_fields" => {
            let fields = engine.list_form_fields(input).map_err(op_engine_error)?;
            report_outcome(&fields)
        }
        "create_text_field" => {
            let p = merged_params(params);
            let spec = pe::TextFieldSpec {
                name: req_str(&p, "name")?,
                page: req_u32(&p, "page")?,
                rect: req_rect4(&p, &["rect", "widget_rect", "box"])?,
                value: opt_str(&p, "value"),
                read_only: opt_bool(&p, "read_only", false)?,
            };
            let bytes = engine
                .create_text_field(input, &spec)
                .map_err(op_engine_error)?;
            pdf_outcome(op, artifact_id, bytes)
        }
        "delete_form_field" => {
            let p = merged_params(params);
            let name = req_str(&p, "name")?;
            let bytes = engine
                .delete_form_field(input, &name)
                .map_err(op_engine_error)?;
            pdf_outcome(op, artifact_id, bytes)
        }
        "fill_form" => {
            let values = form_values_param(params)?;
            let bytes = engine.fill_form(input, &values).map_err(op_engine_error)?;
            pdf_outcome(op, artifact_id, bytes)
        }
        "clear_form" => {
            let bytes = engine.clear_form(input).map_err(op_engine_error)?;
            pdf_outcome(op, artifact_id, bytes)
        }
        "unlock_fields" => {
            let p = merged_params(params);
            let names = param_as::<Vec<String>>(&p, &["names", "fields"])?;
            let (bytes, report) = engine
                .unlock_fields(input, names.as_deref())
                .map_err(op_engine_error)?;
            pdf_report_outcome(op, artifact_id, bytes, &report)
        }
        "lock_fields" => {
            let p = merged_params(params);
            let names = param_as::<Vec<String>>(&p, &["names", "fields"])?;
            let (bytes, report) = engine
                .lock_fields(input, names.as_deref())
                .map_err(op_engine_error)?;
            pdf_report_outcome(op, artifact_id, bytes, &report)
        }
        // -- backend-only ops: headers/footers/marks ---------------------
        "headers_footers" => {
            let p = merged_params(params);
            let pages_owned = pages_param(&p);
            let spec = pe::HeaderFooterSpec {
                header: opt_str(&p, "header"),
                footer: opt_str(&p, "footer"),
                page_numbers: enum_param_keys::<pe::PageNumberStyle>(
                    &p,
                    &["page_numbers", "page_number_style", "numbers"],
                )?
                .unwrap_or(pe::PageNumberStyle::None),
                font_size: opt_f64(&p, "font_size")?.map(|v| v as f32),
                margin: opt_f64(&p, "margin")?.map(|v| v as f32),
            };
            let bytes = engine
                .headers_footers(input, &spec, pages_owned.as_deref())
                .map_err(op_engine_error)?;
            pdf_outcome(op, artifact_id, bytes)
        }
        "add_page_numbers" => {
            let p = merged_params(params);
            let style = enum_param_keys::<pe::PageNumberStyle>(
                &p,
                &["style", "page_numbers", "page_number_style", "numbers"],
            )?
            .ok_or_else(|| {
                op_param_error(
                    "params.style is required (none, bottom_center, bottom_right, bottom_left)",
                )
            })?;
            let bytes = engine
                .add_page_numbers(input, style)
                .map_err(op_engine_error)?;
            pdf_outcome(op, artifact_id, bytes)
        }
        "add_print_marks" => {
            let p = merged_params(params);
            let pages_owned = pages_param(&p);
            let spec = pe::PrintMarksSpec {
                crop_marks: opt_bool(&p, "crop_marks", true)?,
                registration: opt_bool(&p, "registration", false)?,
                offset: opt_f64(&p, "offset")?.unwrap_or(pe::DEFAULT_PRINT_MARK_OFFSET),
                color: opt_color_u8(&p, "color")?.unwrap_or([0, 0, 0]),
                pages: pages_owned.as_deref(),
            };
            let bytes = engine
                .add_print_marks(input, &spec)
                .map_err(op_engine_error)?;
            pdf_outcome(op, artifact_id, bytes)
        }
        // -- backend-only ops: metadata / conformance --------------------
        "set_metadata" => {
            let p = merged_params(params);
            // `MetadataPatch` is all-optional, so the merged view
            // deserializes whether the caller used `spec` or flat keys.
            let patch: pe::MetadataPatch = serde_json::from_value(p.clone())
                .map_err(|e| op_param_error(format!("params.spec is invalid: {e}")))?;
            if patch == pe::MetadataPatch::default() {
                return Err(op_param_error(
                    "set_metadata requires at least one metadata field \
                     (params.spec or top-level keys like title/author)",
                ));
            }
            let bytes = engine
                .set_metadata(input, &patch)
                .map_err(op_engine_error)?;
            pdf_outcome(op, artifact_id, bytes)
        }
        "clear_metadata" => {
            let bytes = engine.clear_metadata(input).map_err(op_engine_error)?;
            pdf_outcome(op, artifact_id, bytes)
        }
        "set_xmp" => {
            let p = merged_params(params);
            let packet = b64_param(&p, &["xmp_b64", "packet_b64"])?.or_else(|| {
                opt_str(&p, "xmp")
                    .or_else(|| opt_str(&p, "xmp_xml"))
                    .map(|s| s.into_bytes())
            });
            let packet = packet.ok_or_else(|| {
                op_param_error(
                    "set_xmp requires params.xmp_b64 (base64) or params.xmp (XML string)",
                )
            })?;
            let bytes = engine.set_xmp(input, &packet).map_err(op_engine_error)?;
            pdf_outcome(op, artifact_id, bytes)
        }
        "get_xmp" => {
            let bytes = engine.get_xmp(input).map_err(op_engine_error)?;
            file_outcome(op, artifact_id, bytes, "xml", "application/xml")
        }
        "to_pdfa" => {
            let p = merged_params(params);
            let level = pdfa_level_param(&p)?;
            let report = engine.to_pdfa(input, level).map_err(op_engine_error)?;
            let bytes = report.bytes.clone();
            pdf_report_outcome(op, artifact_id, bytes, &report_sans_bytes(&report)?)
        }
        "to_pdfua" => {
            let p = merged_params(params);
            let level = pdfua_level_param(&p)?;
            let report = engine.to_pdfua(input, level).map_err(op_engine_error)?;
            let bytes = report.bytes.clone();
            pdf_report_outcome(op, artifact_id, bytes, &report_sans_bytes(&report)?)
        }
        "to_pdfx" => {
            let p = merged_params(params);
            let level = pdfx_level_param(&p)?;
            let report = engine.to_pdfx(input, level).map_err(op_engine_error)?;
            let bytes = report.bytes.clone();
            pdf_report_outcome(op, artifact_id, bytes, &report_sans_bytes(&report)?)
        }
        // -- backend-only ops: page labels / outline ---------------------
        "set_page_labels" => {
            let labels = page_labels_param(params)?;
            let bytes = engine
                .set_page_labels(input, &labels)
                .map_err(op_engine_error)?;
            pdf_outcome(op, artifact_id, bytes)
        }
        "get_page_labels" => {
            let labels = engine.get_page_labels(input).map_err(op_engine_error)?;
            report_outcome(&labels)
        }
        "clear_page_labels" => {
            let bytes = engine.clear_page_labels(input).map_err(op_engine_error)?;
            pdf_outcome(op, artifact_id, bytes)
        }
        "set_outline" => {
            let items = outline_items_param(params)?;
            let bytes = engine.set_outline(input, &items).map_err(op_engine_error)?;
            pdf_outcome(op, artifact_id, bytes)
        }
        "remove_outline" => {
            let bytes = engine.remove_outline(input).map_err(op_engine_error)?;
            pdf_outcome(op, artifact_id, bytes)
        }
        "list_outline" => {
            let outline = engine.list_outline(input).map_err(op_engine_error)?;
            report_outcome(&outline)
        }
        // -- backend-only ops: rotation / view / page ops ----------------
        "set_page_rotation" => {
            let p = merged_params(params);
            let pages_req = req_str(&p, "pages")?;
            let degrees = opt_u32(&p, "degrees")?
                .or(opt_u32(&p, "rotation")?)
                .ok_or_else(|| op_param_error("params.degrees is required"))?;
            let degrees = u16::try_from(degrees)
                .map_err(|_| op_param_error("params.degrees must fit in u16"))?;
            let bytes = engine
                .set_page_rotation(input, &pages_req, degrees)
                .map_err(op_engine_error)?;
            pdf_outcome(op, artifact_id, bytes)
        }
        "get_page_rotations" => {
            let rotations = engine.get_page_rotations(input).map_err(op_engine_error)?;
            report_outcome(&rotations)
        }
        "normalize_rotations" => {
            let bytes = engine.normalize_rotations(input).map_err(op_engine_error)?;
            pdf_outcome(op, artifact_id, bytes)
        }
        "crop" => {
            let p = merged_params(params);
            let pages_req = req_str(&p, "pages")?;
            let rect = req_rect4(&p, &["crop_box", "rect", "box"])?;
            let crop_box = [
                rect[0] as f32,
                rect[1] as f32,
                rect[2] as f32,
                rect[3] as f32,
            ];
            let bytes = engine
                .crop(input, &pages_req, crop_box)
                .map_err(op_engine_error)?;
            pdf_outcome(op, artifact_id, bytes)
        }
        "set_view_crop" => {
            let p = merged_params(params);
            let pages_req = req_str(&p, "pages")?;
            let rect = req_rect4(&p, &["rect", "crop_box", "box"])?;
            let bytes = engine
                .set_view_crop(input, &pages_req, rect)
                .map_err(op_engine_error)?;
            pdf_outcome(op, artifact_id, bytes)
        }
        "clear_crop" => {
            let p = merged_params(params);
            let pages_req = req_str(&p, "pages")?;
            let bytes = engine
                .clear_crop(input, &pages_req)
                .map_err(op_engine_error)?;
            pdf_outcome(op, artifact_id, bytes)
        }
        "resize" => {
            let p = merged_params(params);
            let pages_req = req_str(&p, "pages")?;
            let spec = match (
                opt_f64(&p, "width_pt")?.or(opt_f64(&p, "width")?),
                opt_f64(&p, "height_pt")?.or(opt_f64(&p, "height")?),
            ) {
                (Some(width_pt), Some(height_pt)) => pe::ResizeSpec::Size {
                    width_pt,
                    height_pt,
                },
                _ => pe::ResizeSpec::Scale(
                    opt_f64(&p, "scale")?
                        .or(opt_f64(&p, "factor")?)
                        .ok_or_else(|| {
                            op_param_error(
                                "params.scale or params.width_pt+params.height_pt is required",
                            )
                        })?,
                ),
            };
            let bytes = engine
                .resize(input, &pages_req, &spec)
                .map_err(op_engine_error)?;
            pdf_outcome(op, artifact_id, bytes)
        }
        "scale_content" => {
            let p = merged_params(params);
            let pages_req = req_str(&p, "pages")?;
            let scale_x = opt_f64(&p, "scale_x")?
                .or(opt_f64(&p, "scale")?)
                .ok_or_else(|| op_param_error("params.scale_x is required"))?;
            let scale_y = opt_f64(&p, "scale_y")?;
            let bytes = engine
                .scale_content(input, &pages_req, scale_x, scale_y)
                .map_err(op_engine_error)?;
            pdf_outcome(op, artifact_id, bytes)
        }
        "draw_text" => {
            let p = merged_params(params);
            let spec = pe::DrawTextSpec {
                text: req_str(&p, "text")?,
                x: req_f64(&p, "x")? as f32,
                y: req_f64(&p, "y")? as f32,
                font_size: req_f64(&p, "font_size")? as f32,
                color: opt_color_f32(&p, "color")?,
                opacity: opt_f64(&p, "opacity")?.map(|v| v as f32),
                rotation: opt_f64(&p, "rotation")?.map(|v| v as f32),
                pages: opt_str(&p, "pages"),
            };
            let bytes = engine.draw_text(input, &spec).map_err(op_engine_error)?;
            pdf_outcome(op, artifact_id, bytes)
        }
        "draw_shapes" => {
            let spec = draw_spec_param(params)?;
            let bytes = engine.draw_shapes(input, &spec).map_err(op_engine_error)?;
            pdf_outcome(op, artifact_id, bytes)
        }
        // -- backend-only ops: page manipulation -------------------------
        "duplicate_pages" => {
            let p = merged_params(params);
            let pages_req = req_str(&p, "pages")?;
            let copies = req_u32(&p, "copies")?;
            let position = dup_pos_param(&p)?;
            let (bytes, copied) = engine
                .duplicate_pages(input, &pages_req, copies, position)
                .map_err(op_engine_error)?;
            pdf_report_outcome(op, artifact_id, bytes, &json!({ "copied_pages": copied }))
        }
        "insert_blank" => {
            let p = merged_params(params);
            let position = insert_pos_param(&p)?;
            let count = req_u32(&p, "count")?;
            let size = match opt_str(&p, "size")
                .unwrap_or_else(|| "match".to_string())
                .to_ascii_lowercase()
                .as_str()
            {
                // `None` behaves like `PageSize::Match` per the op docs.
                "match" | "same" | "same_as_input" | "neighbor" | "neighbour" => None,
                "a4" => Some(pe::PageSize::A4),
                "letter" => Some(pe::PageSize::Letter),
                other => {
                    return Err(op_param_error(format!(
                        "params.size must be match, a4, or letter, got {other:?}"
                    )));
                }
            };
            let (bytes, inserted) = engine
                .insert_blank(input, position, count, size)
                .map_err(op_engine_error)?;
            pdf_report_outcome(
                op,
                artifact_id,
                bytes,
                &json!({ "inserted_pages": inserted }),
            )
        }
        "remove_blank_pages" => {
            let report = engine.remove_blank_pages(input).map_err(op_engine_error)?;
            pdf_report_outcome(
                op,
                artifact_id,
                report.pdf.clone(),
                &json!({
                    "removed": report.removed,
                    "kept": report.kept,
                    "total_before": report.total_before,
                    "total_after": report.total_after,
                }),
            )
        }
        "interleave" => {
            let extra = extra_pdf_inputs(params, other)?;
            if extra.is_empty() {
                return Err(op_param_error(
                    "interleave requires at least one additional input \
                     (params.inputs or params.other_pdf_b64)",
                ));
            }
            let mut inputs: Vec<&[u8]> = Vec::with_capacity(extra.len() + 1);
            inputs.push(input);
            inputs.extend(extra.iter().map(Vec::as_slice));
            let bytes = engine.interleave(&inputs).map_err(op_engine_error)?;
            pdf_outcome(op, artifact_id, bytes)
        }
        "merge" => {
            let extra = extra_pdf_inputs(params, other)?;
            if extra.is_empty() {
                return Err(op_param_error(
                    "merge requires at least one additional input \
                     (params.inputs or params.other_pdf_b64)",
                ));
            }
            let mut inputs: Vec<&[u8]> = Vec::with_capacity(extra.len() + 1);
            inputs.push(input);
            inputs.extend(extra.iter().map(Vec::as_slice));
            let bytes = engine.merge(&inputs).map_err(op_engine_error)?;
            pdf_outcome(op, artifact_id, bytes)
        }
        "merge_mode" => {
            let p = merged_params(params);
            let mode = merge_mode_param(&p)?;
            let extra = extra_pdf_inputs(params, other)?;
            if extra.is_empty() {
                return Err(op_param_error(
                    "merge_mode requires at least one additional input \
                     (params.inputs or params.other_pdf_b64)",
                ));
            }
            let mut inputs: Vec<&[u8]> = Vec::with_capacity(extra.len() + 1);
            inputs.push(input);
            inputs.extend(extra.iter().map(Vec::as_slice));
            let bytes = engine.merge_mode(&inputs, mode).map_err(op_engine_error)?;
            pdf_outcome(op, artifact_id, bytes)
        }
        "nup" => {
            let p = merged_params(params);
            let cols = req_u32(&p, "cols")?;
            let rows = req_u32(&p, "rows")?;
            let pages_owned = pages_param(&p);
            let bytes = engine
                .nup(input, cols, rows, pages_owned.as_deref())
                .map_err(op_engine_error)?;
            pdf_outcome(op, artifact_id, bytes)
        }
        "single_page" => {
            let p = merged_params(params);
            let pages_owned = pages_param(&p);
            let cols = opt_u32(&p, "cols")?;
            let bytes = engine
                .single_page(input, pages_owned.as_deref(), cols)
                .map_err(op_engine_error)?;
            pdf_outcome(op, artifact_id, bytes)
        }
        "poster" => {
            let p = merged_params(params);
            let pages_owned = pages_param(&p);
            let tile_cols = req_u32(&p, "tile_cols")?;
            let tile_rows = req_u32(&p, "tile_rows")?;
            let bytes = engine
                .poster(input, pages_owned.as_deref(), tile_cols, tile_rows)
                .map_err(op_engine_error)?;
            pdf_outcome(op, artifact_id, bytes)
        }
        "booklet" => {
            let p = merged_params(params);
            let sheet_size = match opt_str(&p, "sheet_size")
                .or_else(|| opt_str(&p, "size"))
                .unwrap_or_else(|| "same_as_input".to_string())
                .to_ascii_lowercase()
                .as_str()
            {
                "same_as_input" | "input" | "same" => pe::BookletSize::SameAsInput,
                "a4" => pe::BookletSize::A4,
                "letter" => pe::BookletSize::Letter,
                other => {
                    return Err(op_param_error(format!(
                        "params.sheet_size must be same_as_input, a4, or letter, got {other:?}"
                    )));
                }
            };
            let imposition = match opt_str(&p, "imposition")
                .or_else(|| opt_str(&p, "order"))
                .unwrap_or_else(|| "left_right".to_string())
                .to_ascii_lowercase()
                .as_str()
            {
                "left_right" | "leftright" | "ltr" => pe::BookletMode::LeftRight,
                "right_left" | "rightleft" | "rtl" => pe::BookletMode::RightLeft,
                other => {
                    return Err(op_param_error(format!(
                        "params.imposition must be left_right or right_left, got {other:?}"
                    )));
                }
            };
            let bytes = engine
                .booklet(
                    input,
                    pe::BookletSpec {
                        sheet_size,
                        imposition,
                    },
                )
                .map_err(op_engine_error)?;
            pdf_outcome(op, artifact_id, bytes)
        }
        "split_mode" => {
            let p = merged_params(params);
            let mode_name = opt_str(&p, "mode").unwrap_or_else(|| "by_count".to_string());
            // `ByRange`/`OnTextMatch` borrow their spec — keep the
            // Strings alive for the whole arm.
            let by_range_spec;
            let on_match_query;
            let mode = match mode_name.to_ascii_lowercase().as_str() {
                "by_count" | "count" => pe::SplitMode::ByCount {
                    pages_per_file: opt_u32(&p, "pages_per_file")?
                        .or(opt_u32(&p, "per_file")?)
                        .ok_or_else(|| {
                            op_param_error("split_mode by_count requires params.pages_per_file")
                        })?,
                },
                "by_range" | "range" | "ranges" => {
                    by_range_spec = opt_str(&p, "spec")
                        .or_else(|| opt_str(&p, "ranges"))
                        .or_else(|| opt_str(&p, "pages"))
                        .ok_or_else(|| {
                            op_param_error(
                                "split_mode by_range requires params.spec or params.pages",
                            )
                        })?;
                    pe::SplitMode::ByRange {
                        spec: by_range_spec.as_str(),
                    }
                }
                "every_nth" | "nth" | "stride" => pe::SplitMode::EveryNth {
                    n: opt_u32(&p, "n")?
                        .or(opt_u32(&p, "stride")?)
                        .ok_or_else(|| op_param_error("split_mode every_nth requires params.n"))?,
                },
                "at_bookmarks" | "bookmarks" => pe::SplitMode::AtBookmarks {
                    level: opt_u32(&p, "level")?.unwrap_or(1),
                },
                "by_file_size" | "file_size" | "size" => pe::SplitMode::ByFileSize {
                    max_bytes: match opt_f64(&p, "max_bytes")? {
                        Some(v) => Some(v),
                        None => opt_f64(&p, "max_size_mb")?.map(|mb| mb * 1_048_576.0),
                    }
                    .map(|v| v as u64)
                    .filter(|v| *v > 0)
                    .ok_or_else(|| {
                        op_param_error("split_mode by_file_size requires params.max_bytes")
                    })?,
                },
                "at_blank_pages" | "blank_pages" => pe::SplitMode::AtBlankPages,
                "on_text_match" | "text_match" => {
                    on_match_query = opt_str(&p, "query")
                        .or_else(|| opt_str(&p, "text"))
                        .ok_or_else(|| {
                            op_param_error("split_mode on_text_match requires params.query")
                        })?;
                    pe::SplitMode::OnTextMatch {
                        query: on_match_query.as_str(),
                    }
                }
                other => {
                    return Err(op_param_error(format!(
                        "params.mode must be by_count, by_range, every_nth, at_bookmarks, \
                         by_file_size, at_blank_pages, or on_text_match, got {other:?}"
                    )));
                }
            };
            let parts = engine.split_mode(input, mode).map_err(op_engine_error)?;
            multi_pdf_outcome(op, artifact_id, parts)
        }
        "split_at_outline" => {
            let p = merged_params(params);
            let level = opt_u32(&p, "level")?.unwrap_or(0);
            let parts = engine
                .split_at_outline(input, level)
                .map_err(op_engine_error)?;
            multi_pdf_outcome(op, artifact_id, parts)
        }
        // -- backend-only ops: color / content / objects ---------------
        "grayscale" => {
            let pages_owned = pages_param(params);
            let (bytes, report) = engine
                .grayscale(input, pages_owned.as_deref())
                .map_err(op_engine_error)?;
            pdf_report_outcome(op, artifact_id, bytes, &report)
        }
        "invert_colors" => {
            let pages_owned = pages_param(params);
            let (bytes, report) = engine
                .invert_colors(input, pages_owned.as_deref())
                .map_err(op_engine_error)?;
            pdf_report_outcome(op, artifact_id, bytes, &report)
        }
        "overlay" => {
            let p = merged_params(params);
            let overlay_bytes = match b64_param(&p, &["overlay_b64", "overlay_pdf_b64", "pdf_b64"])?
            {
                Some(b) => b,
                None => extra_pdf_inputs(params, other)?
                    .into_iter()
                    .next()
                    .ok_or_else(|| {
                        op_param_error(
                            "overlay requires params.overlay_b64 (base64 PDF) or an extra input",
                        )
                    })?,
            };
            let pages_owned = pages_param(&p);
            let bytes = engine
                .overlay(input, &overlay_bytes, pages_owned.as_deref())
                .map_err(op_engine_error)?;
            pdf_outcome(op, artifact_id, bytes)
        }
        "remove_images" => {
            let pages_owned = pages_param(params);
            let report = engine
                .remove_images(input, pages_owned.as_deref())
                .map_err(op_engine_error)?;
            let bytes = report.bytes.clone();
            pdf_report_outcome(op, artifact_id, bytes, &report_sans_bytes(&report)?)
        }
        "remove_named_objects" => {
            let p = merged_params(params);
            let names: Vec<String> = req_param_as(&p, &["names", "categories", "objects"])?;
            // The engine op only reports what it would remove — it
            // does not emit rewritten bytes (see `ops::remove_objects`
            // docs); the outcome is report-only by design.
            let report = engine
                .remove_named_objects(input, &names)
                .map_err(op_engine_error)?;
            let counts: Vec<serde_json::Value> = report
                .counts
                .iter()
                .map(|(category, count)| json!({ "category": category, "count": count }))
                .collect();
            report_outcome(&json!({
                "removed_categories": report.removed_categories,
                "counts": counts,
            }))
        }
        "replace_image" => {
            let p = merged_params(params);
            let page = req_u32(&p, "page")?;
            let name = ["name", "xobject", "xobject_name", "image_name"]
                .iter()
                .find_map(|k| opt_str(&p, k))
                .ok_or_else(|| op_param_error("params.name is required"))?;
            let image = b64_param(
                &p,
                &["image_b64", "new_image_b64", "content_b64", "data_b64"],
            )?
            .ok_or_else(|| op_param_error("params.image_b64 is required (base64 PNG/JPEG)"))?;
            let format = image_format_param(&p)?;
            let bytes = engine
                .replace_image(input, page, &name, &image, format)
                .map_err(op_engine_error)?;
            pdf_outcome(op, artifact_id, bytes)
        }
        "linearize" => {
            let report = engine.linearize(input).map_err(op_engine_error)?;
            pdf_report_outcome(
                op,
                artifact_id,
                report.bytes.clone(),
                &json!({
                    "linearized": report.linearized,
                    "hint_stream": report.hint_stream,
                    "notes": report.notes,
                }),
            )
        }
        "garbage_collect" => {
            let report = engine.garbage_collect(input).map_err(op_engine_error)?;
            pdf_report_outcome(
                op,
                artifact_id,
                report.bytes.clone(),
                &json!({
                    "objects_before": report.objects_before,
                    "objects_after": report.objects_after,
                    "objects_pruned": report.objects_pruned,
                    "bytes_before": report.bytes_before,
                    "bytes_after": report.bytes_after,
                    "techniques": report.techniques,
                }),
            )
        }
        "make_searchable" => {
            let pages_owned = pages_param(params);
            // The engine op discards its rewritten bytes and returns
            // only a report (see `ops::searchable_pdf` docs) — the
            // outcome is honestly report-only.
            let report = engine
                .make_searchable(input, pages_owned.as_deref())
                .map_err(op_engine_error)?;
            report_outcome(&report)
        }
        // -- backend-only ops: portfolio ---------------------------------
        "list_portfolio" => {
            let entries = engine.list_portfolio(input).map_err(op_engine_error)?;
            let entries: Vec<serde_json::Value> = entries
                .iter()
                .map(|e| json!({ "name": e.name, "page": e.page }))
                .collect();
            report_outcome(&entries)
        }
        "create_portfolio" => {
            let p = merged_params(params);
            let extra = extra_pdf_inputs(params, other)?;
            if extra.is_empty() {
                return Err(op_param_error(
                    "create_portfolio requires at least one additional input \
                     (params.inputs or params.other_pdf_b64)",
                ));
            }
            let names = opt_str_vec(&p, "names")?.unwrap_or_default();
            let mut inputs: Vec<&[u8]> = Vec::with_capacity(extra.len() + 1);
            inputs.push(input);
            inputs.extend(extra.iter().map(Vec::as_slice));
            let bytes = engine
                .create_portfolio(&inputs, &names)
                .map_err(op_engine_error)?;
            pdf_outcome(op, artifact_id, bytes)
        }
        // -- backend-only ops: signatures / timestamps -------------------
        "sign" => {
            let p = merged_params(params);
            let (credentials, _guard) = sign_credentials_param(&p)?;
            let options = param_as::<pe::SignOptions>(&p, &["options", "sign_options", "spec"])?
                .unwrap_or_else(|| pe::SignOptions {
                    reason: opt_str(&p, "reason"),
                    location: opt_str(&p, "location"),
                    contact_info: opt_str(&p, "contact_info"),
                    signer_name: opt_str(&p, "signer_name"),
                    // `timestamp_authority` is unsupported by the engine
                    // — mapping `tsa_url` here makes it fail honestly
                    // with UnsupportedOperation instead of being ignored.
                    timestamp_authority: opt_str(&p, "timestamp_authority")
                        .or_else(|| opt_str(&p, "tsa_url")),
                });
            let bytes = engine
                .sign(input, &credentials, &options)
                .map_err(op_engine_error)?;
            pdf_outcome(op, artifact_id, bytes)
        }
        "list_signature_timestamps" => {
            let timestamps = engine
                .list_signature_timestamps(input)
                .map_err(op_engine_error)?;
            report_outcome(&timestamps)
        }
        "has_timestamps" => {
            let has = engine.has_timestamps(input).map_err(op_engine_error)?;
            report_outcome(&has)
        }
        "document_digest" => {
            let p = merged_params(params);
            let algorithm = timestamp_algorithm_param(&p)?;
            let report = engine
                .document_digest(input, algorithm)
                .map_err(op_engine_error)?;
            report_outcome(&report)
        }
        "timestamp_query" => {
            let p = merged_params(params);
            let algorithm = timestamp_algorithm_param(&p)?;
            let bytes = engine
                .timestamp_query(input, algorithm)
                .map_err(op_engine_error)?;
            file_outcome(op, artifact_id, bytes, "tsq", "application/octet-stream")
        }
        "build_timestamp_request" => {
            let p = merged_params(params);
            let algorithm = timestamp_algorithm_param(&p)?;
            let digest = match b64_param(&p, &["digest_b64", "digest"])? {
                Some(d) => d,
                None => hex_param(&p, &["digest_hex"])?.ok_or_else(|| {
                    op_param_error("params.digest_b64 or params.digest_hex is required")
                })?,
            };
            let bytes = engine
                .build_timestamp_request(&digest, algorithm)
                .map_err(op_engine_error)?;
            file_outcome(op, artifact_id, bytes, "tsq", "application/octet-stream")
        }
        "request_external_timestamp" => {
            let p = merged_params(params);
            let tsq = b64_param(&p, &["tsq_b64", "request_b64", "timestamp_query_b64"])?
                .ok_or_else(|| {
                    op_param_error("params.tsq_b64 is required (base64 DER TimeStampReq)")
                })?;
            let url = ["tsa_url", "url", "timestamp_authority"]
                .iter()
                .find_map(|k| opt_str(&p, k))
                .ok_or_else(|| op_param_error("params.tsa_url is required"))?;
            let token = engine
                .request_external_timestamp(&tsq, &url)
                .map_err(op_engine_error)?;
            report_outcome(&token)
        }
        "timestamp_document" => {
            let p = merged_params(params);
            let algorithm = timestamp_algorithm_param(&p)?;
            let url = ["tsa_url", "url", "timestamp_authority"]
                .iter()
                .find_map(|k| opt_str(&p, k))
                .ok_or_else(|| op_param_error("params.tsa_url is required"))?;
            let report = engine
                .timestamp_document(input, algorithm, &url)
                .map_err(op_engine_error)?;
            report_outcome(&report)
        }
        other => Err(op_param_error(format!("unsupported pdf op: {other}"))),
    }
}

/// Uploads one [`OpFile`] through the streaming staging/commit path
/// and registers (or reuses) its artifact row. Object names are
/// deterministic — `documents/{doc}/outputs/{file_name}` — so job
/// retries rewrite the same bytes instead of accumulating orphans,
/// and an existing artifact row for the same `storage_uri` is reused
/// rather than duplicated.
async fn store_op_output(
    ctx: &JobContext,
    storage: &StorageConfig,
    document_id: &DocumentId,
    file: &OpFile,
) -> Result<(String, DocumentArtifact), JobError> {
    store_output_object(
        ctx,
        storage,
        document_id,
        &file.file_name,
        &file.bytes,
        Some(file.mime),
    )
    .await
}

/// Uploads `bytes` through the streaming staging/commit path to
/// `documents/{document_id}/outputs/{file_name}` and registers (or
/// reuses) the artifact row. Shared by [`store_op_output`] and the
/// `media.op` / `convert.run` output paths, whose file names and
/// MIME types are computed at runtime.
async fn store_output_object(
    ctx: &JobContext,
    storage: &StorageConfig,
    document_id: &DocumentId,
    file_name: &str,
    bytes: &[u8],
    mime: Option<&str>,
) -> Result<(String, DocumentArtifact), JobError> {
    let object_name = format!("documents/{document_id}/outputs/{file_name}");
    let storage_uri = format!("local://{object_name}");

    // Reuse a previously registered artifact for this deterministic
    // object name (retry / replay path).
    let existing = list_artifacts(&ctx.pool, document_id)
        .await
        .map_err(store_err)?
        .into_iter()
        .find(|a| a.storage_uri == storage_uri);
    if let Some(artifact) = existing {
        return Ok((storage_uri, artifact));
    }

    let receipt = {
        let storage = storage.clone();
        let name = object_name.clone();
        let bytes = bytes.to_vec();
        run_blocking("op output upload", move || {
            let mut fs = FaultInjectingFs::new(storage)
                .map_err(|e| JobError::Failed(format!("storage init failed: {e}")))?;
            let mut upload = fs.begin_upload(&name).map_err(storage_err)?;
            for chunk in bytes.chunks(UPLOAD_CHUNK_BYTES) {
                upload.write_chunk(chunk).map_err(storage_err)?;
            }
            upload.commit().map_err(storage_err)
        })
        .await?
    };

    let artifact = upload_artifact(
        &ctx.pool,
        &NewArtifact {
            id: None,
            document_id: document_id.clone(),
            storage_uri: storage_uri.clone(),
            file_name: file_name.to_string(),
            mime_type: mime.map(str::to_string),
            size_bytes: receipt.bytes,
            checksum: Some(DocumentChecksum {
                algorithm: ChecksumAlgorithm::Sha256,
                value: receipt.sha256,
            }),
        },
    )
    .await
    .map_err(store_err)?;
    Ok((storage_uri, artifact))
}

impl JobHandler for PdfOpHandler {
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
                let status_key = PdfOpPayload::parse(&job.payload)
                    .map(|p| pdf_op_status_key(&p.op))
                    .unwrap_or_else(|_| "pdf_op_status".to_string());
                mark_failed(&ctx.pool, &payload.document_id, &status_key, job, e).await;
            }
            result
        })
    }
}

impl PdfOpHandler {
    async fn run(&self, ctx: &JobContext, job: &Job) -> Result<(), JobError> {
        let payload = PdfOpPayload::parse(&job.payload)?;
        let op = payload.op.clone();
        let status_key = pdf_op_status_key(&op);
        let (document_id, artifact) = begin_processing(&ctx.pool, job, &status_key).await?;

        // `compare` needs a second input artifact, resolved up front
        // (same rules as the primary: explicit id or most recent).
        let other_artifact = match op.as_str() {
            "compare" => {
                let other_doc = opt_str(&payload.params, "other_document_id").map(DocumentId::new);
                let other_art = opt_str(&payload.params, "other_artifact_id");
                match (other_doc, other_art) {
                    (None, None) => {
                        return Err(JobError::Failed(
                            "compare requires params.other_artifact_id or params.other_document_id"
                                .into(),
                        ));
                    }
                    (doc, art) => {
                        let doc_id = doc.unwrap_or_else(|| document_id.clone());
                        Some(resolve_artifact(&ctx.pool, &doc_id, art.as_deref()).await?)
                    }
                }
            }
            _ => None,
        };

        let object_name = object_name_for_uri(&artifact.storage_uri)?;
        let other_name = other_artifact
            .as_ref()
            .map(|a| object_name_for_uri(&a.storage_uri))
            .transpose()?;
        let storage = self.storage.clone();
        let read_name = object_name.clone();
        let other_read_name = other_name.clone();
        let artifact_id = artifact.id.clone();
        let artifact_id_for_blocking = artifact_id.clone();
        let op_for_blocking = op.clone();
        let params_for_blocking = payload.params.clone();
        let outcome = run_blocking("document.pdf_op", move || {
            let engine = crate::crates::pdf_engine::PdfEngine::with_limits(processing_limits());
            let mut fs = FaultInjectingFs::new(storage)
                .map_err(|e| JobError::Failed(format!("storage init failed: {e}")))?;
            let read = read_object_bounded(&mut fs, &read_name, MAX_PROCESSING_BYTES)?;
            if read.truncated {
                return Ok(OpOutcome::Failed(OpFailure {
                    status: "skipped_too_large",
                    error: format!("artifact exceeds {MAX_PROCESSING_BYTES} bytes"),
                }));
            }
            let other_bytes = match other_read_name {
                Some(name) => {
                    let read = read_object_bounded(&mut fs, &name, MAX_PROCESSING_BYTES)?;
                    if read.truncated {
                        return Ok(OpOutcome::Failed(OpFailure {
                            status: "skipped_too_large",
                            error: format!(
                                "comparison artifact exceeds {MAX_PROCESSING_BYTES} bytes"
                            ),
                        }));
                    }
                    Some(read.bytes)
                }
                None => None,
            };
            Ok(run_pdf_op(
                &engine,
                &read.bytes,
                other_bytes.as_deref(),
                &op_for_blocking,
                &params_for_blocking,
                &artifact_id_for_blocking,
            ))
        })
        .await?;

        // Turn the outcome into (status, inline report, output files).
        let (status, inline_result, files, error) = match outcome {
            OpOutcome::Failed(f) => (f.status, serde_json::Value::Null, Vec::new(), Some(f.error)),
            OpOutcome::Done(success) => {
                let mut files = success.files;
                let mut inline = serde_json::Value::Null;
                if let Some(report) = success.report {
                    let bytes = serde_json::to_vec(&report).map_err(|e| {
                        JobError::Failed(format!("failed to serialize op report: {e}"))
                    })?;
                    if bytes.len() <= REPORT_INLINE_MAX_BYTES {
                        inline = report;
                    } else {
                        files.push(OpFile {
                            file_name: format!("{op}-{artifact_id}-result.json"),
                            bytes,
                            ext: "json",
                            mime: "application/json",
                        });
                    }
                }
                if files.len() > MAX_OP_OUTPUTS {
                    (
                        "error",
                        serde_json::Value::Null,
                        Vec::new(),
                        Some(format!(
                            "op produced {} outputs, exceeding the limit of {MAX_OP_OUTPUTS}",
                            files.len()
                        )),
                    )
                } else {
                    ("succeeded", inline, files, None)
                }
            }
        };
        // Upload outputs and register artifact rows.
        let mut outputs = Vec::new();
        for file in &files {
            let (_uri, stored) = store_op_output(ctx, &self.storage, &document_id, file).await?;
            outputs.push(json!({
                "artifact_id": stored.id,
                "storage_uri": stored.storage_uri,
                "file_name": stored.file_name,
                "bytes": stored.size_bytes,
                "format": file.ext,
            }));
        }
        let output_count = outputs.len();

        let finished_at = crate::crates::document_engine::store::now_rfc3339();
        let op_key = op.clone();
        let status_string = status.to_string();
        let entry = json!({
            "op": op,
            "status": status,
            "finished_at": finished_at,
            "source_artifact_id": artifact.id,
            "result": inline_result,
            "outputs": outputs,
            "error": error,
        });
        let proc_status = normalized_processing_status(status).to_string();
        mutate_metadata(&ctx.pool, &document_id, move |m| {
            m.custom_fields.insert(status_key, json!(status_string));
            m.custom_fields
                .insert("processing_status".into(), json!(proc_status));
            let mut ops_map = m
                .custom_fields
                .get(PDF_OPS_FIELD)
                .cloned()
                .unwrap_or_else(|| json!({}));
            if !ops_map.is_object() {
                ops_map = json!({});
            }
            ops_map[op_key.as_str()] = entry;
            m.custom_fields.insert(PDF_OPS_FIELD.into(), ops_map);
        })
        .await?;

        audit_event(
            &ctx.pool,
            &document_id,
            DocumentAuditEventType::ProcessingCompleted,
            json!({
                "job_id": job.id,
                "job_kind": job.kind,
                "op": op,
                "status": status,
                "outputs": output_count,
            }),
        )
        .await?;

        Ok(())
    }
}

// ---------------------------------------------------------------------------
// pipeline.run
// ---------------------------------------------------------------------------
//
// Runs a `pipeline_engine::PipelineDefinition` over a document
// artifact: the resolved artifact bytes seed the pipeline, step
// reports and byte outputs are persisted like `document.pdf_op`
// results. See the module docs for the payload contract.

/// `pipeline.run` — executes a [`PipelineDefinition`] against the
/// resolved artifact's bytes via [`PipelineEngine::run_pipeline`].
///
/// Outcome handling mirrors [`PdfOpHandler`]: deterministic pipeline
/// failures (bad definition, step errors, limit violations) are
/// terminal job *successes* recorded as `pipeline_run_status:
/// "error"` / `"unsupported"` / `"unavailable"` /
/// `"skipped_too_large"`; only infrastructure failures (storage,
/// store, pool) produce `JobError` and consume retry attempts.
pub struct PipelineRunHandler {
    storage: StorageConfig,
}

/// Metadata custom-field map holding per-pipeline results:
/// `pipelines.{slug} = {pipeline, status, finished_at,
/// source_artifact_id, report?, outputs, error?}`.
const PIPELINES_FIELD: &str = "pipelines";

/// Metadata custom-field key holding the run status.
const PIPELINE_RUN_STATUS_KEY: &str = "pipeline_run_status";

impl PipelineRunHandler {
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

/// What a `pipeline.run` payload asks the handler to execute: either
/// a linear [`PipelineDefinition`] or a [`DagDefinition`] graph.
enum PipelineRunTarget {
    /// `payload.pipeline` — a full definition JSON (producers resolve
    /// built-in names to a definition before enqueueing, so retries
    /// replay identically).
    Pipeline(PipelineDefinition),
    /// `payload.dag` — a full DAG definition JSON executed via
    /// [`run_dag`]. Same self-contained/replayable contract.
    Dag(DagDefinition),
}

impl PipelineRunTarget {
    /// Stable name used for the metadata slug and report field.
    fn name(&self) -> &str {
        match self {
            Self::Pipeline(def) => &def.name,
            Self::Dag(def) => &def.name,
        }
    }
}

/// Parsed `pipeline.run` payload. `document_id`/`artifact_id` are
/// validated through [`DocJobPayload::parse`] but resolved by
/// [`begin_processing`]; `target` is the full definition JSON.
struct PipelineRunPayload {
    target: PipelineRunTarget,
}

impl PipelineRunPayload {
    fn parse(payload: &serde_json::Value) -> Result<Self, JobError> {
        DocJobPayload::parse(payload)?;
        // `dag` takes precedence when both keys are present; a payload
        // with neither is the same definition error as before.
        let target = if payload["dag"].is_object() {
            let dag = payload["dag"].clone();
            let definition: DagDefinition = serde_json::from_value(dag).map_err(|e| {
                JobError::Failed(format!("payload.dag is not a valid dag definition: {e}"))
            })?;
            PipelineRunTarget::Dag(definition)
        } else {
            let pipeline = payload["pipeline"].clone();
            let definition: PipelineDefinition = serde_json::from_value(pipeline).map_err(|e| {
                JobError::Failed(format!(
                    "payload.pipeline is not a valid pipeline definition: {e}"
                ))
            })?;
            PipelineRunTarget::Pipeline(definition)
        };
        Ok(Self { target })
    }
}

/// Maps a [`PipelineError`] onto the terminal-status vocabulary:
/// resource-limit rejections are `skipped_too_large`, missing external
/// tools surface as `unavailable`, everything else is an honest
/// `error`.
fn pipeline_error_status(e: &PipelineError) -> &'static str {
    match e {
        PipelineError::LimitExceeded(_) => "skipped_too_large",
        PipelineError::StepFailed {
            source: StepError::Pdf(PdfError::MissingTool(_)),
            ..
        } => "unavailable",
        _ => "error",
    }
}

/// Lowercase slug for a pipeline name, safe as a metadata map key and
/// file-name segment (keeps `[a-zA-Z0-9._-]`, everything else → `_`).
fn pipeline_slug(name: &str) -> String {
    let slug: String = name
        .chars()
        .map(|c| {
            if c.is_ascii_alphanumeric() || matches!(c, '.' | '-' | '_') {
                c
            } else {
                '_'
            }
        })
        .collect();
    if slug.is_empty() {
        "pipeline".to_string()
    } else {
        slug
    }
}

/// Detects a stored byte stream's format for output file naming —
/// same sniffing used by the other processing handlers. Returns
/// `(extension, mime)`; both are `'static` because [`OpFile`] stores
/// them as such.
fn ext_mime_for_bytes(bytes: &[u8], artifact_mime: Option<&str>) -> (&'static str, &'static str) {
    let format = sniff_format(&bytes[..bytes.len().min(SNIFF_BYTES)]);
    match format {
        SniffedFormat::Pdf => ("pdf", "application/pdf"),
        SniffedFormat::Png => ("png", "image/png"),
        SniffedFormat::Jpeg => ("jpg", "image/jpeg"),
        SniffedFormat::Zip => ("zip", "application/zip"),
        SniffedFormat::Gif => ("gif", "image/gif"),
        SniffedFormat::Unknown => {
            if let Some(mime) = artifact_mime
                && mime.trim_start().to_ascii_lowercase().starts_with("text/")
            {
                if mime.contains("markdown") {
                    ("md", "text/markdown")
                } else {
                    ("txt", "text/plain")
                }
            } else {
                ("bin", "application/octet-stream")
            }
        }
    }
}

impl JobHandler for PipelineRunHandler {
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
                    PIPELINE_RUN_STATUS_KEY,
                    job,
                    e,
                )
                .await;
            }
            result
        })
    }
}

/// Outcome of the blocking `pipeline.run` execution: either the
/// shared linear-pipeline [`OpOutcome`] accounting or a completed
/// [`DagReport`] with its per-node changed artifacts.
enum PipelineExecOutcome {
    /// `payload.pipeline` — a `PipelineDefinition` run.
    Linear(OpOutcome),
    /// `payload.dag` — a `DagDefinition` run via [`run_dag`]. Node
    /// failures are data inside `report`, not a job error; the
    /// top-level status is derived from `report.failed`.
    Dag {
        /// Complete per-node report (always produced for a valid
        /// graph — see `pipeline_engine::dag`).
        report: DagReport,
        /// `OpFile`s for each succeeded node's *changed* artifact.
        files: Vec<OpFile>,
    },
}

impl PipelineRunHandler {
    async fn run(&self, ctx: &JobContext, job: &Job) -> Result<(), JobError> {
        let payload = PipelineRunPayload::parse(&job.payload)?;
        let target_name = payload.target.name().to_string();
        let slug = pipeline_slug(&target_name);
        let (document_id, artifact) =
            begin_processing(&ctx.pool, job, PIPELINE_RUN_STATUS_KEY).await?;

        let object_name = object_name_for_uri(&artifact.storage_uri)?;
        let storage = self.storage.clone();
        let read_name = object_name.clone();
        let artifact_mime = artifact.mime_type.clone();
        let artifact_id = artifact.id.clone();
        let artifact_id_for_blocking = artifact_id.clone();
        let target_for_blocking = payload.target;
        let outcome = run_blocking("pipeline.run", move || {
            let mut fs = FaultInjectingFs::new(storage)
                .map_err(|e| JobError::Failed(format!("storage init failed: {e}")))?;
            let read = read_object_bounded(&mut fs, &read_name, MAX_PROCESSING_BYTES)?;
            if read.truncated {
                return Ok(PipelineExecOutcome::Linear(OpOutcome::Failed(OpFailure {
                    status: "skipped_too_large",
                    error: format!("artifact exceeds {MAX_PROCESSING_BYTES} bytes"),
                })));
            }
            let engine = PipelineEngine::with_limits(
                PipelineLimits::default(),
                crate::crates::security_engine::ResourceLimits::default(),
            );
            match target_for_blocking {
                PipelineRunTarget::Pipeline(def) => match engine.run_pipeline(&def, &read.bytes) {
                    Ok(output) => Ok(PipelineExecOutcome::Linear(pipeline_output_to_op(
                        &def,
                        output,
                        &artifact_id_for_blocking,
                        artifact_mime.as_deref(),
                    ))),
                    Err(e) => Ok(PipelineExecOutcome::Linear(OpOutcome::Failed(OpFailure {
                        status: pipeline_error_status(&e),
                        error: e.to_string(),
                    }))),
                },
                PipelineRunTarget::Dag(dag) => {
                    match run_dag(&engine, &dag, &read.bytes) {
                        Ok(report) => {
                            let files = dag_report_files(
                                &report,
                                &read.bytes,
                                &artifact_id_for_blocking,
                                artifact_mime.as_deref(),
                            );
                            Ok(PipelineExecOutcome::Dag { report, files })
                        }
                        // Graph-shape/limit violations are deterministic
                        // definition errors — same terminal-status
                        // treatment as a pipeline failure.
                        Err(e) => Ok(PipelineExecOutcome::Linear(OpOutcome::Failed(OpFailure {
                            status: "error",
                            error: e.to_string(),
                        }))),
                    }
                }
            }
        })
        .await?;

        // Same result accounting as document.pdf_op: byte outputs and
        // an oversized report become artifacts under outputs/; a small
        // report is stored inline in the metadata entry.
        let (status, inline_result, files, error) = match outcome {
            PipelineExecOutcome::Linear(OpOutcome::Failed(f)) => {
                (f.status, serde_json::Value::Null, Vec::new(), Some(f.error))
            }
            PipelineExecOutcome::Linear(OpOutcome::Done(success)) => {
                let mut files = success.files;
                let mut inline = serde_json::Value::Null;
                if let Some(report) = success.report {
                    let bytes = serde_json::to_vec(&report).map_err(|e| {
                        JobError::Failed(format!("failed to serialize pipeline report: {e}"))
                    })?;
                    if bytes.len() <= REPORT_INLINE_MAX_BYTES {
                        inline = report;
                    } else {
                        files.push(OpFile {
                            file_name: format!("pipeline-{slug}-{artifact_id}-report.json"),
                            bytes,
                            ext: "json",
                            mime: "application/json",
                        });
                    }
                }
                if files.len() > MAX_OP_OUTPUTS {
                    (
                        "error",
                        serde_json::Value::Null,
                        Vec::new(),
                        Some(format!(
                            "pipeline produced {} outputs, exceeding the limit of {MAX_OP_OUTPUTS}",
                            files.len()
                        )),
                    )
                } else {
                    ("succeeded", inline, files, None)
                }
            }
            PipelineExecOutcome::Dag { report, files } => {
                // The DagReport *is* the pipeline result: per-node
                // statuses, errors, and structured step outputs.
                let report_json = serde_json::to_value(&report).map_err(|e| {
                    JobError::Failed(format!("failed to serialize dag report: {e}"))
                })?;
                let mut files = files;
                let mut inline = serde_json::Value::Null;
                let bytes = serde_json::to_vec(&report_json).map_err(|e| {
                    JobError::Failed(format!("failed to serialize dag report: {e}"))
                })?;
                if bytes.len() <= REPORT_INLINE_MAX_BYTES {
                    inline = report_json;
                } else {
                    files.push(OpFile {
                        file_name: format!("pipeline-{slug}-{artifact_id}-report.json"),
                        bytes,
                        ext: "json",
                        mime: "application/json",
                    });
                }
                // Node failures are data, but the run as a whole is
                // only "succeeded" when every node succeeded; skipped
                // nodes only occur behind a failure, so `failed > 0`
                // is the honest top-level signal.
                let (dag_status, dag_error) = if report.failed > 0 {
                    (
                        "error",
                        Some(format!(
                            "dag `{}`: {} node(s) failed, {} skipped",
                            report.dag, report.failed, report.skipped
                        )),
                    )
                } else {
                    ("succeeded", None)
                };
                if files.len() > MAX_OP_OUTPUTS {
                    (
                        "error",
                        serde_json::Value::Null,
                        Vec::new(),
                        Some(format!(
                            "dag produced {} outputs, exceeding the limit of {MAX_OP_OUTPUTS}",
                            files.len()
                        )),
                    )
                } else {
                    (dag_status, inline, files, dag_error)
                }
            }
        };

        let mut outputs = Vec::new();
        for file in &files {
            let (_uri, stored) = store_op_output(ctx, &self.storage, &document_id, file).await?;
            outputs.push(json!({
                "artifact_id": stored.id,
                "storage_uri": stored.storage_uri,
                "file_name": stored.file_name,
                "bytes": stored.size_bytes,
                "format": file.ext,
            }));
        }
        let output_count = outputs.len();

        let finished_at = crate::crates::document_engine::store::now_rfc3339();
        let pipeline_name = target_name.clone();
        let status_string = status.to_string();
        let entry = json!({
            "pipeline": pipeline_name.clone(),
            "status": status,
            "finished_at": finished_at,
            "source_artifact_id": artifact.id,
            "report": inline_result,
            "outputs": outputs,
            "error": error,
        });
        let proc_status = normalized_processing_status(status).to_string();
        let pipelines_key = slug.clone();
        mutate_metadata(&ctx.pool, &document_id, move |m| {
            m.custom_fields
                .insert(PIPELINE_RUN_STATUS_KEY.into(), json!(status_string));
            m.custom_fields
                .insert("processing_status".into(), json!(proc_status));
            let mut map = m
                .custom_fields
                .get(PIPELINES_FIELD)
                .cloned()
                .unwrap_or_else(|| json!({}));
            if !map.is_object() {
                map = json!({});
            }
            map[pipelines_key.as_str()] = entry;
            m.custom_fields.insert(PIPELINES_FIELD.into(), map);
        })
        .await?;

        audit_event(
            &ctx.pool,
            &document_id,
            DocumentAuditEventType::ProcessingCompleted,
            json!({
                "job_id": job.id,
                "job_kind": job.kind,
                "pipeline": pipeline_name,
                "status": status,
                "outputs": output_count,
            }),
        )
        .await?;

        Ok(())
    }
}

/// Converts a successful [`PipelineOutput`] into stored artifacts +
/// an optional inline report — the pipeline analogue of
/// [`OpSuccess`]. Byte artifacts become one `OpFile` each (detected
/// extension/MIME); the step reports become a JSON report that the
/// caller either inlines or stores as a `*-report.json` artifact.
fn pipeline_output_to_op(
    def: &PipelineDefinition,
    output: crate::crates::pipeline_engine::PipelineOutput,
    artifact_id: &str,
    source_mime: Option<&str>,
) -> OpOutcome {
    let slug = pipeline_slug(&def.name);
    let mut files = Vec::new();
    match &output.artifact {
        PipelineArtifact::Bytes(bytes) => {
            let (ext, mime) = ext_mime_for_bytes(bytes, source_mime);
            files.push(OpFile {
                file_name: format!("pipeline-{slug}-{artifact_id}.{ext}"),
                bytes: bytes.clone(),
                ext,
                mime,
            });
        }
        PipelineArtifact::Multiple(parts) => {
            for (i, bytes) in parts.iter().enumerate() {
                let (ext, mime) = ext_mime_for_bytes(bytes, source_mime);
                files.push(OpFile {
                    file_name: format!("pipeline-{slug}-{artifact_id}-{}.{}", i + 1, ext),
                    bytes: bytes.clone(),
                    ext,
                    mime,
                });
            }
        }
    }
    // The step reports are the pipeline's structured output; keep them
    // out of the byte-artifact file names.
    let report = json!({
        "pipeline": output.pipeline,
        "steps": output.steps,
        "artifact_kind": match &output.artifact {
            PipelineArtifact::Bytes(_) => "bytes",
            PipelineArtifact::Multiple(_) => "multiple",
        },
        "output_count": match &output.artifact {
            PipelineArtifact::Bytes(_) => 1,
            PipelineArtifact::Multiple(v) => v.len(),
        },
    });
    OpOutcome::Done(OpSuccess {
        report: Some(report),
        files,
    })
}

/// Collects the [`OpFile`]s for a completed [`DagReport`]: one file
/// per succeeded node whose artifact *differs* from the DAG input.
///
/// DAG data flow is fan-out — every node sees the same input bytes —
/// so a node artifact byte-identical to `input` is a pass-through
/// report step and produces no file (its structured output is already
/// in the report). Transform nodes produce
/// `dag-{slug}-{artifact_id}-{node}.{ext}` (or `...-{node}-{i}.{ext}`
/// for multi-document artifacts). Node ids are slugged for safe file
/// names.
fn dag_report_files(
    report: &DagReport,
    input: &[u8],
    artifact_id: &str,
    source_mime: Option<&str>,
) -> Vec<OpFile> {
    let slug = pipeline_slug(&report.dag);
    let mut files = Vec::new();
    for node in &report.nodes {
        if node.status != DagNodeStatus::Succeeded {
            continue;
        }
        let Some(artifact) = &node.artifact else {
            continue;
        };
        let node_slug = pipeline_slug(&node.id);
        match artifact {
            PipelineArtifact::Bytes(bytes) => {
                if bytes.as_slice() == input {
                    continue;
                }
                let (ext, mime) = ext_mime_for_bytes(bytes, source_mime);
                files.push(OpFile {
                    file_name: format!("dag-{slug}-{artifact_id}-{node_slug}.{ext}"),
                    bytes: bytes.clone(),
                    ext,
                    mime,
                });
            }
            PipelineArtifact::Multiple(parts) => {
                for (i, bytes) in parts.iter().enumerate() {
                    let (ext, mime) = ext_mime_for_bytes(bytes, source_mime);
                    files.push(OpFile {
                        file_name: format!(
                            "dag-{slug}-{artifact_id}-{node_slug}-{}.{}",
                            i + 1,
                            ext
                        ),
                        bytes: bytes.clone(),
                        ext,
                        mime,
                    });
                }
            }
        }
    }
    files
}

// ---------------------------------------------------------------------------
// storage.scan_inbox / document.auto_rename
// ---------------------------------------------------------------------------

/// `storage.scan_inbox` — ingests files dropped into
/// `<storage_root>/inbox/` and moves them to `inbox/.processed/`.
///
/// See the module docs for the payload contract and the
/// at-least-once/processed-marker semantics. The filesystem walk and
/// the marker move live in
/// [`crate::crates::application::services::folder_watch`]; this handler
/// owns validation, document creation, and the `document.ingest`
/// enqueue per file. Per-file failures are collected and logged —
/// they never fail the job — while payload-level problems (missing
/// `owner_id`, unreadable inbox) do.
pub struct StorageScanInboxHandler {
    storage: StorageConfig,
}

/// Parsed `storage.scan_inbox` payload.
#[derive(Debug, Deserialize)]
struct ScanInboxPayload {
    /// Owner recorded on every created document. Required.
    owner_id: String,
    /// Descend into inbox subdirectories. Default `false`.
    #[serde(default)]
    recurse: bool,
    /// Cap on files handled this run. Default
    /// [`folder_watch::DEFAULT_MAX_FILES`], clamped to
    /// [`folder_watch::MAX_MAX_FILES`].
    #[serde(default = "default_scan_max_files")]
    max_files: usize,
}

fn default_scan_max_files() -> usize {
    folder_watch::DEFAULT_MAX_FILES
}

impl StorageScanInboxHandler {
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

    /// Processes one inbox file: validate → read → create document →
    /// enqueue `document.ingest` → move to `.processed`. Any failure
    /// leaves the file in place (or already-moved state consistent)
    /// and is reported to the caller as a per-file error.
    async fn ingest_inbox_file(
        &self,
        ctx: &JobContext,
        owner_id: &str,
        inbox_root: &Path,
        file: &folder_watch::InboxFile,
    ) -> Result<(), JobError> {
        if file.size_bytes == 0 {
            return Err(JobError::Failed("empty file".into()));
        }
        // Base64 inflates ~33%; the 64 MiB cap matches the processing
        // handlers' read bound and keeps job payloads reasonable.
        // Larger files are skipped with a recorded error rather than
        // routed through the streaming ingestion service — producers
        // that need big-file inbox support should call
        // `DocumentIngestionService` directly.
        if file.size_bytes > MAX_PROCESSING_BYTES {
            return Err(JobError::Failed(format!(
                "{} bytes exceeds the {MAX_PROCESSING_BYTES}-byte inbox ingest cap",
                file.size_bytes
            )));
        }
        let file_name = sanitize_file_name(&file.file_name)?;
        let doc_name = document_display_name(&file_name)?;

        let read_path = file.path.clone();
        let bytes = run_blocking("scan_inbox read", move || {
            std::fs::read(&read_path)
                .map_err(|e| JobError::Failed(format!("cannot read {}: {e}", read_path.display())))
        })
        .await?;
        // Re-check after read: the file may have grown past the cap
        // between scan and read (TOCTOU guard, same as pdf.inspect).
        if bytes.len() as u64 > MAX_PROCESSING_BYTES {
            return Err(JobError::Failed(format!(
                "file grew past the {MAX_PROCESSING_BYTES}-byte inbox ingest cap while reading"
            )));
        }

        let format = sniff_format(&bytes[..bytes.len().min(SNIFF_BYTES)]);
        let doc = create_document(
            &ctx.pool,
            &NewDocument {
                name: doc_name,
                document_type: doc_type_for_format(format),
                owner_id: owner_id.to_string(),
                source: DocumentSource {
                    origin: DocumentOrigin::Import,
                    uri: Some(format!("inbox:{}", file.relative.display())),
                    external_reference: None,
                    imported_at: Some(now_rfc3339()),
                },
            },
        )
        .await
        .map_err(store_err)?;

        // Enqueue BEFORE the marker move: if the move fails the file
        // is re-scanned and a duplicate document/job pair may result
        // — the documented at-least-once semantics.
        enqueue_ingest(
            &ctx.pool,
            &doc.id,
            Some(&file_name),
            mime_for_format(format),
            &bytes,
        )
        .await?;

        let rel = file.relative.clone();
        let inbox = inbox_root.to_path_buf();
        run_blocking("scan_inbox move", move || {
            folder_watch::move_to_processed(&inbox, &rel)
                .map(|_| ())
                .map_err(|e| {
                    JobError::Failed(format!(
                        "cannot move {} into .processed: {e}",
                        rel.display()
                    ))
                })
        })
        .await?;

        Ok(())
    }
}

impl JobHandler for StorageScanInboxHandler {
    fn handle<'a>(
        &'a self,
        ctx: &'a JobContext,
        job: &'a Job,
    ) -> std::pin::Pin<Box<dyn std::future::Future<Output = Result<(), JobError>> + Send + 'a>>
    {
        Box::pin(async move {
            let payload: ScanInboxPayload = serde_json::from_value(job.payload.clone())?;
            let owner_id = payload.owner_id.trim().to_string();
            if owner_id.is_empty() {
                return Err(JobError::Failed(
                    "payload.owner_id must be a non-empty string".into(),
                ));
            }
            let recurse = payload.recurse;
            let max_files = payload.max_files.clamp(1, folder_watch::MAX_MAX_FILES);
            let inbox_root = folder_watch::inbox_dir(&self.storage.root_dir);
            let root_for_scan = inbox_root.clone();
            let files = run_blocking("scan_inbox", move || {
                folder_watch::scan_inbox(&root_for_scan, recurse, max_files).map_err(|e| {
                    JobError::Failed(format!(
                        "cannot scan inbox {}: {e}",
                        root_for_scan.display()
                    ))
                })
            })
            .await?;

            let mut processed = 0usize;
            let mut failures: Vec<String> = Vec::new();
            for file in &files {
                match self
                    .ingest_inbox_file(ctx, &owner_id, &inbox_root, file)
                    .await
                {
                    Ok(()) => processed += 1,
                    Err(e) => {
                        tracing::warn!(
                            file = %file.path.display(),
                            error = %e,
                            "storage.scan_inbox: file skipped"
                        );
                        failures.push(format!("{}: {e}", file.file_name));
                    }
                }
            }
            if failures.is_empty() {
                tracing::info!(
                    scanned = files.len(),
                    processed,
                    "storage.scan_inbox finished"
                );
            } else {
                tracing::warn!(
                    scanned = files.len(),
                    processed,
                    failed = failures.len(),
                    errors = ?failures,
                    "storage.scan_inbox finished with per-file errors"
                );
            }
            Ok(())
        })
    }
}

/// Maps a sniffed format onto the coarse [`DocumentType`] used for
/// inbox-created documents (mirrors `ingestion::detect_content`,
/// minus the text heuristic — unknown stays `Other`).
fn doc_type_for_format(format: SniffedFormat) -> DocumentType {
    match format {
        SniffedFormat::Pdf => DocumentType::Pdf,
        SniffedFormat::Png | SniffedFormat::Jpeg | SniffedFormat::Gif => DocumentType::Image,
        SniffedFormat::Zip => DocumentType::Archive,
        SniffedFormat::Unknown => DocumentType::Other,
    }
}

/// Document display name for an inbox file: the sanitized file name
/// truncated to [`DocumentName::MAX_LEN`] characters so it always
/// passes store validation.
fn document_display_name(file_name: &str) -> Result<String, JobError> {
    let truncated: String = file_name.chars().take(DocumentName::MAX_LEN).collect();
    DocumentName::new(truncated)
        .map(|n| n.as_str().to_string())
        .map_err(|e| JobError::Failed(format!("file name is not a valid document name: {e}")))
}

/// Trims a JSON string field, yielding `None` when missing, not a
/// string, or empty after trimming. Used by `document.auto_rename`'s
/// title precedence chain.
fn usable_json_str<'a>(v: Option<&'a serde_json::Value>) -> Option<&'a str> {
    v.and_then(serde_json::Value::as_str)
        .map(str::trim)
        .filter(|s| !s.is_empty())
}

/// `document.auto_rename` — renames a document to
/// `{sanitized-title}-{id8}.{ext}` once extracted metadata provides a
/// usable title.
///
/// Payload: `{document_id}` (validated through [`DocJobPayload`]).
/// Title sources, in order: the `pdf_title` custom field, the `title`
/// custom field, then the metadata `title` column — all written by
/// `document.metadata_extract` or curators. The `auto_renamed`
/// custom field is the idempotency marker: once set (or when no
/// usable title exists, or the current name already equals the
/// computed target) the job succeeds without changes. A missing
/// document fails the job so retries surface the problem.
pub struct DocumentAutoRenameHandler;

/// Custom-field key marking that auto-rename already ran.
const AUTO_RENAMED_KEY: &str = "auto_renamed";

/// Last-8-characters suffix making auto-renamed names unique-ish and
/// recognizable (`{title}-{id8}.{ext}`).
fn document_id_suffix(document_id: &DocumentId) -> String {
    document_id
        .as_str()
        .chars()
        .rev()
        .take(8)
        .collect::<Vec<_>>()
        .into_iter()
        .rev()
        .collect()
}

/// Replaces characters [`DocumentName`] forbids (`/`, `\`, control
/// chars) with spaces and collapses whitespace — titles like
/// `Q1/2024: Report` become `Q1 2024 Report`.
fn sanitize_rename_title(title: &str) -> String {
    title
        .chars()
        .map(|c| {
            if c.is_control() || matches!(c, '/' | '\\') {
                ' '
            } else {
                c
            }
        })
        .collect::<String>()
        .split_whitespace()
        .collect::<Vec<_>>()
        .join(" ")
}

/// Computes the target name `{sanitized-title}-{id8}.{ext}` within
/// [`DocumentName`] limits. `ext` is the preserved original file
/// extension *without* the dot; `None`/empty yields no suffix.
fn auto_rename_name(
    document_id: &DocumentId,
    title: &str,
    ext: Option<&str>,
) -> Result<String, JobError> {
    let id8 = document_id_suffix(document_id);
    let ext = ext
        .map(str::trim)
        .filter(|e| {
            !e.is_empty()
                && !e
                    .chars()
                    .any(|c| c.is_control() || matches!(c, '/' | '\\' | '.'))
        })
        .map(|e| format!(".{e}"))
        .unwrap_or_default();
    let suffix = format!("-{id8}{ext}");
    let budget = DocumentName::MAX_LEN.saturating_sub(suffix.chars().count());
    let base: String = sanitize_rename_title(title)
        .chars()
        .take(budget)
        .collect::<String>()
        .trim_end()
        .to_string();
    if base.is_empty() {
        return Err(JobError::Failed(
            "document title sanitizes to an empty name".into(),
        ));
    }
    let candidate = format!("{base}{suffix}");
    DocumentName::new(candidate.clone())
        .map(|n| n.as_str().to_string())
        .map_err(|e| JobError::Failed(format!("auto-rename produced invalid name: {e}")))
}

/// Extension (no dot) to preserve: the most recent artifact's file
/// name first, then the current document name.
async fn rename_extension(
    pool: &sqlx::SqlitePool,
    document_id: &DocumentId,
    current_name: &str,
) -> Option<String> {
    let from_artifact = list_artifacts(pool, document_id)
        .await
        .ok()
        .and_then(|arts| arts.last().map(|a| a.file_name.clone()))
        .and_then(|name| {
            Path::new(&name)
                .extension()
                .and_then(|e| e.to_str())
                .map(str::to_string)
        });
    from_artifact.or_else(|| {
        Path::new(current_name)
            .extension()
            .and_then(|e| e.to_str())
            .map(str::to_string)
    })
}

impl JobHandler for DocumentAutoRenameHandler {
    fn handle<'a>(
        &'a self,
        ctx: &'a JobContext,
        job: &'a Job,
    ) -> std::pin::Pin<Box<dyn std::future::Future<Output = Result<(), JobError>> + Send + 'a>>
    {
        Box::pin(async move {
            let payload = DocJobPayload::parse(&job.payload)?;
            let document_id = payload.document_id;

            let doc = get_document(&ctx.pool, &document_id)
                .await
                .map_err(store_err)?
                .ok_or_else(|| JobError::Failed(format!("document not found: {document_id}")))?;

            let meta = get_document_metadata(&ctx.pool, &document_id)
                .await
                .map_err(store_err)?;
            let custom = meta.as_ref().map(|m| &m.custom_fields);
            if custom
                .and_then(|c| c.get(AUTO_RENAMED_KEY))
                .and_then(|v| v.as_bool())
                .unwrap_or(false)
            {
                return Ok(()); // already renamed — idempotent no-op
            }

            // Title precedence: pdf_title → title custom field →
            // metadata title column.
            let title = custom
                .and_then(|c| usable_json_str(c.get("pdf_title")))
                .or_else(|| custom.and_then(|c| usable_json_str(c.get("title"))))
                .or_else(|| {
                    meta.as_ref()
                        .and_then(|m| m.title.as_deref())
                        .map(str::trim)
                        .filter(|s| !s.is_empty())
                });
            let Some(title) = title else {
                return Ok(()); // nothing to name it after — success no-op
            };

            let ext = rename_extension(&ctx.pool, &document_id, doc.name.as_str()).await;
            let new_name = auto_rename_name(&document_id, title, ext.as_deref())?;
            let old_name = doc.name.as_str().to_string();
            let renamed = old_name != new_name;
            if renamed {
                rename_document(&ctx.pool, &document_id, &new_name)
                    .await
                    .map_err(store_err)?;
            }

            let finished_at = now_rfc3339();
            let new_name_for_meta = new_name.clone();
            let old_name_for_meta = old_name.clone();
            mutate_metadata(&ctx.pool, &document_id, move |m| {
                m.custom_fields.insert(AUTO_RENAMED_KEY.into(), json!(true));
                m.custom_fields
                    .insert("auto_renamed_at".into(), json!(finished_at));
                m.custom_fields
                    .insert("auto_rename_from".into(), json!(old_name_for_meta));
                m.custom_fields
                    .insert("auto_rename_to".into(), json!(new_name_for_meta));
            })
            .await?;

            audit_event(
                &ctx.pool,
                &document_id,
                DocumentAuditEventType::ProcessingCompleted,
                json!({
                    "job_id": job.id,
                    "job_kind": job.kind,
                    "auto_rename": true,
                    "renamed": renamed,
                    "old_name": old_name,
                    "new_name": new_name,
                }),
            )
            .await?;

            Ok(())
        })
    }
}

// ---------------------------------------------------------------------------
// media.op / convert.run / document.ingest_recover
// ---------------------------------------------------------------------------
//
// `media.op` dispatches a `services::media_ops` op (ffmpeg/ffprobe)
// over a stored artifact or inline bytes; `convert.run` runs the
// `conversion_engine` over the same input shapes;
// `document.ingest_recover` drives
// `DocumentIngestionService::recover_with`. See the module docs for
// the payload contracts and result storage scheme.

/// Optional document/artifact selectors plus inline bytes shared by
/// the `media.op` and `convert.run` payloads.
struct StoredOrInlineInput {
    document_id: Option<String>,
    artifact_id: Option<String>,
    /// `(display name, bytes)` decoded from `input_b64`,
    /// `content_b64`, or `content` — present only when no stored
    /// artifact is referenced (stored inputs take precedence).
    inline: Option<(String, Vec<u8>)>,
}

impl StoredOrInlineInput {
    fn parse(payload: &serde_json::Value) -> Result<Self, JobError> {
        let opt_str = |key: &str| {
            payload[key]
                .as_str()
                .map(str::trim)
                .filter(|s| !s.is_empty())
                .map(str::to_string)
        };
        let document_id = opt_str("document_id");
        let artifact_id = opt_str("artifact_id");

        let inline = if document_id.is_none() && artifact_id.is_none() {
            let b64 = payload["input_b64"]
                .as_str()
                .or_else(|| payload["content_b64"].as_str());
            let text = payload["content"].as_str();
            let sources = usize::from(b64.is_some()) + usize::from(text.is_some());
            if sources == 0 {
                return Err(JobError::Failed(
                    "payload requires document_id/artifact_id or one of \
                     input_b64/content_b64/content"
                        .into(),
                ));
            }
            if sources > 1 {
                return Err(JobError::Failed(
                    "payload accepts at most one of input_b64/content_b64/content".into(),
                ));
            }
            if let Some(encoded) = b64 {
                let bytes = B64.decode(encoded).map_err(|e| {
                    JobError::Failed(format!("payload input is not valid base64: {e}"))
                })?;
                Some(("input.bin".to_string(), bytes))
            } else {
                // `content` is UTF-8 text stored as the file bytes.
                Some(("input.txt".to_string(), text.unwrap().as_bytes().to_vec()))
            }
        } else {
            None
        };

        Ok(Self {
            document_id,
            artifact_id,
            inline,
        })
    }
}

/// Resolved primary input for `media.op` / `convert.run`.
enum ByteInput {
    /// Bytes come from a stored artifact; `document_id` is the owning
    /// document outputs register against.
    Stored {
        document_id: DocumentId,
        artifact: DocumentArtifact,
    },
    /// Caller-supplied bytes — outputs have no document to attach to
    /// and land under `job-outputs/{job_id}/` instead.
    Inline { name: String, bytes: Vec<u8> },
}

/// Resolves [`StoredOrInlineInput`] to concrete bytes-or-artifact.
/// Stored resolution verifies the document exists; the most recent
/// artifact is used when `artifact_id` is absent.
async fn resolve_byte_input(
    ctx: &JobContext,
    input: &StoredOrInlineInput,
) -> Result<ByteInput, JobError> {
    if input.document_id.is_none() && input.artifact_id.is_none() {
        let (name, bytes) = input
            .inline
            .clone()
            .ok_or_else(|| JobError::Failed("missing inline input bytes".into()))?;
        return Ok(ByteInput::Inline { name, bytes });
    }

    let artifact = match &input.artifact_id {
        Some(id) => {
            let artifact = download_artifact(&ctx.pool, id)
                .await
                .map_err(store_err)?
                .ok_or_else(|| JobError::Failed(format!("artifact not found: {id}")))?;
            if let Some(doc) = &input.document_id
                && artifact.document_id.as_str() != doc.as_str()
            {
                return Err(JobError::Failed(format!(
                    "artifact {id} does not belong to document {doc}"
                )));
            }
            artifact
        }
        None => {
            let document_id = DocumentId::new(input.document_id.clone().unwrap_or_default());
            resolve_artifact(&ctx.pool, &document_id, None).await?
        }
    };
    let document_id = artifact.document_id.clone();
    get_document(&ctx.pool, &document_id)
        .await
        .map_err(store_err)?
        .ok_or_else(|| JobError::Failed(format!("document not found: {document_id}")))?;
    Ok(ByteInput::Stored {
        document_id,
        artifact,
    })
}

/// Marks a document-backed `media.op`/`convert.run` job `running` and
/// emits `ProcessingStarted` (document existence was verified during
/// input resolution).
async fn begin_media_job(
    ctx: &JobContext,
    job: &Job,
    status_key: &str,
    document_id: &DocumentId,
    artifact_id: &str,
) -> Result<(), JobError> {
    mark_op_status(&ctx.pool, document_id, status_key, "running").await?;
    audit_event(
        &ctx.pool,
        document_id,
        DocumentAuditEventType::ProcessingStarted,
        json!({
            "job_id": job.id,
            "job_kind": job.kind,
            "artifact_id": artifact_id,
        }),
    )
    .await
}

/// A file produced by `media.op`/`convert.run`, ready to stream into
/// storage. Unlike [`OpFile`], the extension and MIME type are
/// runtime-computed (ffmpeg outputs and conversion formats vary).
struct JobOutputFile {
    /// Safe single-segment file name.
    file_name: String,
    bytes: Vec<u8>,
    /// Extension without dot, recorded in result metadata.
    ext: String,
    mime: Option<String>,
}

/// Extension of `name` without the dot (`"bin"` fallback).
fn output_ext(name: &str) -> String {
    Path::new(name)
        .extension()
        .and_then(|e| e.to_str())
        .filter(|e| !e.is_empty())
        .unwrap_or("bin")
        .to_string()
}

/// Builds a stored output name `{op-slug}-{src}-{name}` per the
/// documented scheme; unsafe engine-suggested names fall back to
/// `output-{index}.bin`.
fn prefixed_output_name(op: &str, src: &str, index: usize, raw: &str) -> String {
    let slug = op.replace('.', "-");
    let safe_src = sanitize_file_name(src).unwrap_or_else(|_| "input".to_string());
    let safe_raw = sanitize_file_name(raw).unwrap_or_else(|_| format!("output-{index}.bin"));
    format!("{slug}-{safe_src}-{safe_raw}")
}

/// Stores `files` for a document-backed job under
/// `documents/{id}/outputs/` and returns the `outputs` metadata array.
async fn store_document_outputs(
    ctx: &JobContext,
    storage: &StorageConfig,
    document_id: &DocumentId,
    files: &[JobOutputFile],
) -> Result<Vec<serde_json::Value>, JobError> {
    let mut outputs = Vec::with_capacity(files.len());
    for file in files {
        let (uri, stored) = store_output_object(
            ctx,
            storage,
            document_id,
            &file.file_name,
            &file.bytes,
            file.mime.as_deref(),
        )
        .await?;
        outputs.push(json!({
            "artifact_id": stored.id,
            "storage_uri": uri,
            "file_name": stored.file_name,
            "bytes": stored.size_bytes,
            "format": file.ext,
        }));
    }
    Ok(outputs)
}

/// Stores one output that has no document to attach to under
/// `job-outputs/{job_id}/` (no artifact rows exist without a
/// document). Deterministic names make retries rewrite the same
/// object; the returned `local://` URI goes into job progress.
async fn store_job_scoped_output(
    _ctx: &JobContext,
    storage: &StorageConfig,
    job_id: i64,
    file: &JobOutputFile,
) -> Result<String, JobError> {
    let object_name = format!("job-outputs/{job_id}/{}", file.file_name);
    let storage_uri = format!("local://{object_name}");
    let receipt = {
        let storage = storage.clone();
        let name = object_name.clone();
        let bytes = file.bytes.clone();
        run_blocking("job output upload", move || {
            let mut fs = FaultInjectingFs::new(storage)
                .map_err(|e| JobError::Failed(format!("storage init failed: {e}")))?;
            let mut upload = fs.begin_upload(&name).map_err(storage_err)?;
            for chunk in bytes.chunks(UPLOAD_CHUNK_BYTES) {
                upload.write_chunk(chunk).map_err(storage_err)?;
            }
            upload.commit().map_err(storage_err)
        })
        .await?
    };
    debug_assert!(receipt.bytes == file.bytes.len() as u64);
    Ok(storage_uri)
}

/// Stores `files` under `job-outputs/{job_id}/` and returns the
/// progress `outputs` array (no artifact rows — there is no document).
async fn store_scoped_outputs(
    ctx: &JobContext,
    storage: &StorageConfig,
    job_id: i64,
    files: &[JobOutputFile],
) -> Result<Vec<serde_json::Value>, JobError> {
    let mut outputs = Vec::with_capacity(files.len());
    for file in files {
        let uri = store_job_scoped_output(ctx, storage, job_id, file).await?;
        outputs.push(json!({
            "storage_uri": uri,
            "file_name": file.file_name,
            "bytes": file.bytes.len(),
            "format": file.ext,
        }));
    }
    Ok(outputs)
}

/// Records a terminal `media.op`/`convert.run` outcome: with a
/// document, outputs become artifacts and a `{ops_field}.{entry_name}`
/// metadata entry plus `{status_key}`/`processing_status` are written;
/// without one, outputs land under `job-outputs/{job_id}/` and the
/// result is reported through job progress. `label_key` is `"op"` for
/// `media.op` and `"pair"` for `convert.run` entries; `extra` is
/// merged into the metadata entry / progress payload (e.g. convert
/// backend + warnings).
#[allow(clippy::too_many_arguments)]
async fn finish_media_job(
    ctx: &JobContext,
    storage: &StorageConfig,
    job: &Job,
    document_id: Option<&DocumentId>,
    source_artifact_id: Option<&str>,
    status_key: &str,
    ops_field: &str,
    label_key: &str,
    entry_name: &str,
    mut status: &str,
    report: Option<serde_json::Value>,
    mut files: Vec<JobOutputFile>,
    warnings: Vec<String>,
    error: Option<String>,
    extra: serde_json::Value,
) -> Result<(), JobError> {
    // Structured reports too large for inline metadata become a
    // `*-result.json` output file (same rule as `document.pdf_op`).
    let mut inline_result = serde_json::Value::Null;
    if let Some(report) = report {
        let bytes = serde_json::to_vec(&report)
            .map_err(|e| JobError::Failed(format!("failed to serialize op report: {e}")))?;
        if bytes.len() <= REPORT_INLINE_MAX_BYTES {
            inline_result = report;
        } else {
            let src = source_artifact_id.unwrap_or("inline");
            let slug = entry_name.replace('.', "-");
            files.push(JobOutputFile {
                file_name: format!(
                    "{}-{}-result.json",
                    slug,
                    sanitize_file_name(src).unwrap_or_else(|_| "input".to_string())
                ),
                bytes,
                ext: "json".to_string(),
                mime: Some("application/json".to_string()),
            });
        }
    }
    let mut error = error;
    if files.len() > MAX_OP_OUTPUTS {
        status = "error";
        inline_result = serde_json::Value::Null;
        files.clear();
        error = Some(format!("op produced more than {MAX_OP_OUTPUTS} outputs"));
    }

    let finished_at = crate::crates::document_engine::store::now_rfc3339();
    match document_id {
        Some(document_id) => {
            let outputs = store_document_outputs(ctx, storage, document_id, &files).await?;
            let output_count = outputs.len();
            let mut entry = json!({
                "status": status,
                "finished_at": finished_at,
                "source_artifact_id": source_artifact_id,
                "result": inline_result,
                "outputs": outputs,
                "warnings": warnings,
                "error": error,
            });
            entry[label_key] = json!(entry_name);
            if let serde_json::Value::Object(extra) = extra {
                if let serde_json::Value::Object(map) = &mut entry {
                    for (k, v) in extra {
                        map.insert(k, v);
                    }
                }
            }
            let status_key = status_key.to_string();
            let ops_field = ops_field.to_string();
            let entry_name = entry_name.to_string();
            let status_string = status.to_string();
            let proc_status = normalized_processing_status(status).to_string();
            let entry_name_key = entry_name.clone();
            mutate_metadata(&ctx.pool, document_id, move |m| {
                m.custom_fields.insert(status_key, json!(status_string));
                m.custom_fields
                    .insert("processing_status".into(), json!(proc_status));
                let mut ops_map = m
                    .custom_fields
                    .get(ops_field.as_str())
                    .cloned()
                    .unwrap_or_else(|| json!({}));
                if !ops_map.is_object() {
                    ops_map = json!({});
                }
                ops_map[entry_name_key.as_str()] = entry;
                m.custom_fields.insert(ops_field, ops_map);
            })
            .await?;
            audit_event(
                &ctx.pool,
                document_id,
                DocumentAuditEventType::ProcessingCompleted,
                json!({
                    "job_id": job.id,
                    "job_kind": job.kind,
                    "op": entry_name,
                    "status": status,
                    "outputs": output_count,
                }),
            )
            .await?;
        }
        None => {
            let outputs = store_scoped_outputs(ctx, storage, job.id, &files).await?;
            let mut progress = json!({
                "status": status,
                "result": inline_result,
                "outputs": outputs,
                "warnings": warnings,
                "error": error,
            });
            if let serde_json::Value::Object(extra) = extra
                && let serde_json::Value::Object(map) = &mut progress
            {
                for (k, v) in extra {
                    map.insert(k, v);
                }
            }
            let _ = ctx.report_progress(job.id, &progress).await;
        }
    }
    Ok(())
}

// ---------------------------------------------------------------------------
// media.op
// ---------------------------------------------------------------------------

/// `media.op` — runs a [`media_ops::run_media_op`] dispatch (ffmpeg /
/// ffprobe via the multimedia engine) over the resolved input.
///
/// Outcome handling mirrors [`PdfOpHandler`]: deterministic engine
/// failures are terminal job *successes* recorded as
/// `media_op_status: "error"` (or `skipped_too_large`); only
/// infrastructure failures produce `JobError` and consume retries.
/// `MediaError::Unsupported` / `MissingTool` are the exception — they
/// fail the *job* honestly without recording a document outcome.
pub struct MediaOpHandler {
    storage: StorageConfig,
    runtime: std::sync::OnceLock<Result<Runtime, String>>,
}

/// Parsed `media.op` payload.
struct MediaOpPayload {
    sel: StoredOrInlineInput,
    op: String,
    /// Op params with handler-level `subtitle_*` selectors removed.
    params: serde_json::Value,
    /// Resolved secondary input for `subtitles.burn`.
    secondary: Option<SecondaryInput>,
}

/// Handler-level second input for `subtitles.burn`, taken from
/// `params.subtitle_b64` / `params.subtitle_artifact_id` (+
/// `params.subtitle_document_id`) and stripped before `params` reach
/// the op's options struct.
enum SecondaryInput {
    Bytes(Vec<u8>),
    Artifact {
        document_id: Option<String>,
        artifact_id: String,
    },
}

impl MediaOpPayload {
    fn parse(payload: &serde_json::Value) -> Result<Self, JobError> {
        let sel = StoredOrInlineInput::parse(payload)?;
        let op = payload["op"]
            .as_str()
            .map(str::trim)
            .filter(|s| !s.is_empty())
            .ok_or_else(|| JobError::Failed("payload.op must be a non-empty string".into()))?
            .to_string();
        if !media_ops::MEDIA_OPS.contains(&op.as_str()) {
            return Err(JobError::Failed(format!("unknown media op: {op}")));
        }
        let mut params = match &payload["params"] {
            serde_json::Value::Null => json!({}),
            v @ serde_json::Value::Object(_) => v.clone(),
            _ => {
                return Err(JobError::Failed(
                    "payload.params must be a JSON object".into(),
                ));
            }
        };
        // Top-level `output_format` fills `params.output_format` when
        // the op's params lack one (same rule as `kaladoc media op`).
        if let Some(fmt) = payload["output_format"]
            .as_str()
            .map(str::trim)
            .filter(|s| !s.is_empty())
            && params
                .get("output_format")
                .is_none_or(serde_json::Value::is_null)
        {
            params["output_format"] = json!(fmt);
        }

        // `subtitles.burn`'s second input is resolved by the handler,
        // not the op — extract it, then strip the selectors so the
        // op's serde options struct never sees them.
        let mut secondary = None;
        if op == "subtitles.burn"
            && let serde_json::Value::Object(map) = &mut params
        {
            if let Some(encoded) = map
                .remove("subtitle_b64")
                .and_then(|v| v.as_str().map(str::to_string))
            {
                let bytes = B64.decode(&encoded).map_err(|e| {
                    JobError::Failed(format!("params.subtitle_b64 is not valid base64: {e}"))
                })?;
                secondary = Some(SecondaryInput::Bytes(bytes));
            } else if let Some(artifact_id) = map
                .remove("subtitle_artifact_id")
                .and_then(|v| v.as_str().map(str::to_string))
            {
                let document_id = map
                    .remove("subtitle_document_id")
                    .and_then(|v| v.as_str().map(str::to_string));
                secondary = Some(SecondaryInput::Artifact {
                    document_id,
                    artifact_id,
                });
            }
        }

        Ok(Self {
            sel,
            op,
            params,
            secondary,
        })
    }
}

/// What the blocking half of `media.op` produced.
enum MediaBlockOutcome {
    /// `run_media_op` returned an outcome.
    Done(media_ops::MediaOpOutcome),
    /// `run_media_op` returned an engine error.
    Engine(MediaError),
    /// Primary or secondary input exceeded [`MEDIA_MAX_INPUT_BYTES`].
    SkippedTooLarge(String),
}

/// Secondary input resolved to something the blocking closure can
/// read: inline bytes, or a storage object name.
enum SecondarySource {
    Bytes(Vec<u8>),
    Object(String),
}

/// Failure channel for [`MediaOpHandler::execute`]: `Honest`
/// failures (`MediaError::Unsupported` / `MissingTool` — capability-
/// gated ops, absent ffmpeg) fail the job without recording a
/// document outcome; `Infra` failures are retryable and mirrored as
/// `failed` on the final attempt.
enum MediaRunError {
    Honest(JobError),
    Infra(JobError),
}

impl From<JobError> for MediaRunError {
    fn from(e: JobError) -> Self {
        Self::Infra(e)
    }
}

impl MediaOpHandler {
    /// Builds the handler from the application storage configuration.
    pub fn from_app_config() -> Result<Self, crate::config::ConfigError> {
        Ok(Self::with_storage_config(
            AppConfig::load_lenient()?.to_storage_config(),
        ))
    }

    /// Builds the handler around an explicit storage config (tests).
    pub fn with_storage_config(storage: StorageConfig) -> Self {
        Self {
            storage,
            runtime: std::sync::OnceLock::new(),
        }
    }

    /// ffmpeg/ffprobe runtime, probed once. A missing tool is an
    /// honest job failure — the op cannot run without both binaries.
    fn runtime(&self) -> Result<Runtime, JobError> {
        self.runtime
            .get_or_init(|| Runtime::discover().map_err(|e| e.to_string()))
            .clone()
            .map_err(|e| JobError::Failed(format!("multimedia runtime unavailable: {e}")))
    }
}

impl JobHandler for MediaOpHandler {
    fn handle<'a>(
        &'a self,
        ctx: &'a JobContext,
        job: &'a Job,
    ) -> std::pin::Pin<Box<dyn std::future::Future<Output = Result<(), JobError>> + Send + 'a>>
    {
        Box::pin(async move {
            match self.run(ctx, job).await {
                Ok(()) => Ok(()),
                // Capability failures fail the job honestly and are
                // never mirrored into document metadata.
                Err(MediaRunError::Honest(e)) => Err(e),
                Err(MediaRunError::Infra(e)) => Err(e),
            }
        })
    }
}

impl MediaOpHandler {
    async fn run(&self, ctx: &JobContext, job: &Job) -> Result<(), MediaRunError> {
        let payload = MediaOpPayload::parse(&job.payload)?;
        // Probing the runtime is cheap and happens before any document
        // status is touched, so a missing ffmpeg fails cleanly.
        let rt = self.runtime().map_err(|e| MediaRunError::Honest(e))?;
        let input = resolve_byte_input(ctx, &payload.sel).await?;
        let doc_id = match &input {
            ByteInput::Stored { document_id, .. } => Some(document_id.clone()),
            ByteInput::Inline { .. } => None,
        };
        let payload_doc = payload.sel.document_id.as_deref().map(DocumentId::new);
        let result = self.execute(ctx, job, &payload, input, rt).await;
        if let Err(MediaRunError::Infra(e)) = &result
            && job.attempts >= job.max_attempts
        {
            let doc = doc_id.as_ref().or(payload_doc.as_ref());
            if let Some(doc) = doc {
                mark_failed(&ctx.pool, doc, MEDIA_OP_STATUS_KEY, job, e).await;
            }
        }
        result
    }

    async fn execute(
        &self,
        ctx: &JobContext,
        job: &Job,
        payload: &MediaOpPayload,
        input: ByteInput,
        rt: Runtime,
    ) -> Result<(), MediaRunError> {
        let (document_id, artifact_id, input_name, object_name, inline_bytes) = match input {
            ByteInput::Stored {
                document_id,
                artifact,
            } => {
                let name = object_name_for_uri(&artifact.storage_uri)?;
                (
                    Some(document_id),
                    Some(artifact.id.clone()),
                    artifact.file_name.clone(),
                    Some(name),
                    None,
                )
            }
            ByteInput::Inline { name, bytes } => (None, None, name, None, Some(bytes)),
        };
        if let (Some(doc), Some(artifact)) = (document_id.as_ref(), artifact_id.as_deref()) {
            begin_media_job(ctx, job, MEDIA_OP_STATUS_KEY, doc, artifact).await?;
        }

        // `subtitles.burn`'s second input: inline bytes or another
        // artifact's object (read inside the same blocking pass).
        let secondary_source = match &payload.secondary {
            Some(SecondaryInput::Bytes(bytes)) => Some(SecondarySource::Bytes(bytes.clone())),
            Some(SecondaryInput::Artifact {
                document_id,
                artifact_id,
            }) => {
                let artifact = download_artifact(&ctx.pool, artifact_id)
                    .await
                    .map_err(store_err)?
                    .ok_or_else(|| {
                        JobError::Failed(format!("subtitle artifact not found: {artifact_id}"))
                    })?;
                if let Some(doc) = document_id
                    && artifact.document_id.as_str() != doc.as_str()
                {
                    return Err(JobError::Failed(format!(
                        "subtitle artifact {artifact_id} does not belong to document {doc}"
                    ))
                    .into());
                }
                Some(SecondarySource::Object(object_name_for_uri(
                    &artifact.storage_uri,
                )?))
            }
            None => None,
        };

        let op = payload.op.clone();
        let params = payload.params.clone();
        let storage = self.storage.clone();
        let limits = MediaLimits {
            max_input_bytes: Some(MEDIA_MAX_INPUT_BYTES),
            ..MediaLimits::default()
        };
        let outcome = run_blocking("media.op", move || {
            // The fs is only needed when a stored object must be read.
            let needs_fs = object_name.is_some()
                || matches!(secondary_source, Some(SecondarySource::Object(_)));
            let mut fs = if needs_fs {
                Some(
                    FaultInjectingFs::new(storage)
                        .map_err(|e| JobError::Failed(format!("storage init failed: {e}")))?,
                )
            } else {
                None
            };
            let (name, bytes) = match object_name {
                Some(object) => {
                    let read = read_object_bounded(
                        fs.as_mut().expect("fs for stored input"),
                        &object,
                        MEDIA_MAX_INPUT_BYTES,
                    )?;
                    if read.truncated {
                        return Ok(MediaBlockOutcome::SkippedTooLarge(format!(
                            "input exceeds {MEDIA_MAX_INPUT_BYTES} bytes"
                        )));
                    }
                    (input_name, read.bytes)
                }
                None => (input_name, inline_bytes.unwrap_or_default()),
            };
            let secondary = match secondary_source {
                Some(SecondarySource::Bytes(b)) => Some(b),
                Some(SecondarySource::Object(object)) => {
                    let read = read_object_bounded(
                        fs.as_mut().expect("fs for subtitle input"),
                        &object,
                        MEDIA_MAX_INPUT_BYTES,
                    )?;
                    if read.truncated {
                        return Ok(MediaBlockOutcome::SkippedTooLarge(format!(
                            "subtitle input exceeds {MEDIA_MAX_INPUT_BYTES} bytes"
                        )));
                    }
                    Some(read.bytes)
                }
                None => None,
            };
            match media_ops::run_media_op(
                &op,
                &name,
                &bytes,
                &params,
                secondary.as_deref(),
                &limits,
                &rt,
            ) {
                Ok(outcome) => Ok(MediaBlockOutcome::Done(outcome)),
                Err(e) => Ok(MediaBlockOutcome::Engine(e)),
            }
        })
        .await?;

        let src_tag = artifact_id.as_deref().unwrap_or("inline");
        match outcome {
            MediaBlockOutcome::SkippedTooLarge(message) => {
                finish_media_job(
                    ctx,
                    &self.storage,
                    job,
                    document_id.as_ref(),
                    artifact_id.as_deref(),
                    MEDIA_OP_STATUS_KEY,
                    MEDIA_OPS_FIELD,
                    "op",
                    &payload.op,
                    "skipped_too_large",
                    None,
                    Vec::new(),
                    Vec::new(),
                    Some(message),
                    json!({}),
                )
                .await?;
                Ok(())
            }
            MediaBlockOutcome::Engine(
                MediaError::Unsupported(detail) | MediaError::MissingTool(detail),
            ) => Err(MediaRunError::Honest(JobError::Failed(format!(
                "media op {:?} cannot run: {detail}",
                payload.op
            )))),
            MediaBlockOutcome::Engine(e) => {
                finish_media_job(
                    ctx,
                    &self.storage,
                    job,
                    document_id.as_ref(),
                    artifact_id.as_deref(),
                    MEDIA_OP_STATUS_KEY,
                    MEDIA_OPS_FIELD,
                    "op",
                    &payload.op,
                    "error",
                    None,
                    Vec::new(),
                    Vec::new(),
                    Some(e.to_string()),
                    json!({}),
                )
                .await?;
                Ok(())
            }
            MediaBlockOutcome::Done(done) => {
                let (report, files, warnings) = match done {
                    media_ops::MediaOpOutcome::Report(value) => {
                        (Some(value), Vec::new(), Vec::new())
                    }
                    media_ops::MediaOpOutcome::Files(output) => {
                        let mut used = std::collections::HashSet::new();
                        let files = output
                            .files()
                            .enumerate()
                            .map(|(i, f)| {
                                let mut name =
                                    prefixed_output_name(&payload.op, src_tag, i, &f.name);
                                // Same engine-suggested name twice in
                                // one batch would dedup to a single
                                // artifact — disambiguate with the index.
                                if !used.insert(name.clone()) {
                                    name = format!(
                                        "{}-{i}-{}",
                                        name,
                                        sanitize_file_name(&f.name)
                                            .unwrap_or_else(|_| format!("output-{i}.bin"))
                                    );
                                }
                                JobOutputFile {
                                    file_name: name,
                                    bytes: f.bytes.clone(),
                                    ext: output_ext(&f.name),
                                    mime: f.content_type.clone(),
                                }
                            })
                            .collect();
                        (None, files, output.warnings.clone())
                    }
                };
                finish_media_job(
                    ctx,
                    &self.storage,
                    job,
                    document_id.as_ref(),
                    artifact_id.as_deref(),
                    MEDIA_OP_STATUS_KEY,
                    MEDIA_OPS_FIELD,
                    "op",
                    &payload.op,
                    "succeeded",
                    report,
                    files,
                    warnings,
                    None,
                    json!({}),
                )
                .await?;
                Ok(())
            }
        }
    }
}

// ---------------------------------------------------------------------------
// convert.run
// ---------------------------------------------------------------------------

/// `convert.run` — converts the resolved input via
/// [`ConversionEngine::convert`] and stores the output like a
/// `media.op` file result.
///
/// Outcome handling mirrors [`MediaOpHandler`]: engine-reported
/// failures are terminal job *successes* recorded under
/// `convert_run_status` + `processing_status` and a
/// `conversions.{from}-{to}` metadata entry
/// (`unsupported`/`unavailable`/`skipped_too_large`/`error`);
/// infrastructure failures produce `JobError` and consume retries.
pub struct ConvertRunHandler {
    storage: StorageConfig,
    engine: std::sync::OnceLock<std::sync::Arc<ConversionEngine>>,
}

/// Parsed `convert.run` payload.
struct ConvertPayload {
    sel: StoredOrInlineInput,
    from: ConvertFormat,
    to: ConvertFormat,
    options: EngineConvertOptions,
}

impl ConvertPayload {
    fn parse(payload: &serde_json::Value) -> Result<Self, JobError> {
        let sel = StoredOrInlineInput::parse(payload)?;
        let req_format = |key: &str| -> Result<ConvertFormat, JobError> {
            let raw = payload[key]
                .as_str()
                .map(str::trim)
                .filter(|s| !s.is_empty())
                .ok_or_else(|| {
                    JobError::Failed(format!("payload.{key} must be a non-empty string"))
                })?;
            ConvertFormat::parse(raw)
                .ok_or_else(|| JobError::Failed(format!("unknown {key}: {raw}")))
        };
        let from = req_format("from_format")?;
        let to = req_format("to_format")?;
        let options = match &payload["options"] {
            serde_json::Value::Null => EngineConvertOptions::default(),
            v => serde_json::from_value(v.clone()).map_err(|e| {
                JobError::Failed(format!(
                    "payload.options is not a valid ConvertOptions: {e}"
                ))
            })?,
        };
        Ok(Self {
            sel,
            from,
            to,
            options,
        })
    }
}

/// What the blocking half of `convert.run` produced.
enum ConvertBlockOutcome {
    Done(ConvertOutput),
    Failed(ConvertError),
    SkippedTooLarge(String),
}

/// Conventional MIME type for a [`ConvertFormat`].
fn mime_for_convert_format(format: ConvertFormat) -> &'static str {
    match format {
        ConvertFormat::Pdf => "application/pdf",
        ConvertFormat::Txt => "text/plain",
        ConvertFormat::Markdown => "text/markdown",
        ConvertFormat::Html => "text/html",
        ConvertFormat::Png => "image/png",
        ConvertFormat::Jpeg => "image/jpeg",
        ConvertFormat::Docx => {
            "application/vnd.openxmlformats-officedocument.wordprocessingml.document"
        }
        ConvertFormat::Odt => "application/vnd.oasis.opendocument.text",
        ConvertFormat::Xlsx => "application/vnd.openxmlformats-officedocument.spreadsheetml.sheet",
        ConvertFormat::Xls => "application/vnd.ms-excel",
        ConvertFormat::Pptx => {
            "application/vnd.openxmlformats-officedocument.presentationml.presentation"
        }
        ConvertFormat::Ppt => "application/vnd.ms-powerpoint",
        ConvertFormat::Epub => "application/epub+zip",
        ConvertFormat::Eml => "message/rfc822",
        ConvertFormat::Url => "text/uri-list",
        ConvertFormat::Cbz => "application/vnd.comicbook+zip",
        ConvertFormat::Cbr => "application/x-cbr",
    }
}

/// Maps a [`ConvertError`] onto the shared terminal-status
/// vocabulary.
fn convert_error_status(e: &ConvertError) -> &'static str {
    match e {
        ConvertError::UnsupportedConversion { .. } => "unsupported",
        ConvertError::BackendUnavailable { .. } => "unavailable",
        ConvertError::TooLarge { .. } => "skipped_too_large",
        _ => "error",
    }
}

impl ConvertRunHandler {
    /// Builds the handler from the application storage configuration.
    pub fn from_app_config() -> Result<Self, crate::config::ConfigError> {
        Ok(Self::with_storage_config(
            AppConfig::load_lenient()?.to_storage_config(),
        ))
    }

    /// Builds the handler around an explicit storage config (tests).
    pub fn with_storage_config(storage: StorageConfig) -> Self {
        Self {
            storage,
            engine: std::sync::OnceLock::new(),
        }
    }

    /// The conversion engine, built once (backend detection probes
    /// for external tools).
    fn engine(&self) -> std::sync::Arc<ConversionEngine> {
        self.engine
            .get_or_init(|| std::sync::Arc::new(ConversionEngine::new()))
            .clone()
    }
}

impl JobHandler for ConvertRunHandler {
    fn handle<'a>(
        &'a self,
        ctx: &'a JobContext,
        job: &'a Job,
    ) -> std::pin::Pin<Box<dyn std::future::Future<Output = Result<(), JobError>> + Send + 'a>>
    {
        Box::pin(async move { self.run(ctx, job).await })
    }
}

impl ConvertRunHandler {
    async fn run(&self, ctx: &JobContext, job: &Job) -> Result<(), JobError> {
        let payload = ConvertPayload::parse(&job.payload)?;
        let input = resolve_byte_input(ctx, &payload.sel).await?;
        let doc_id = match &input {
            ByteInput::Stored { document_id, .. } => Some(document_id.clone()),
            ByteInput::Inline { .. } => None,
        };
        let payload_doc = payload.sel.document_id.as_deref().map(DocumentId::new);
        let result = self.execute(ctx, job, &payload, input).await;
        if let Err(e) = &result
            && job.attempts >= job.max_attempts
        {
            let doc = doc_id.as_ref().or(payload_doc.as_ref());
            if let Some(doc) = doc {
                mark_failed(&ctx.pool, doc, CONVERT_RUN_STATUS_KEY, job, e).await;
            }
        }
        result
    }

    async fn execute(
        &self,
        ctx: &JobContext,
        job: &Job,
        payload: &ConvertPayload,
        input: ByteInput,
    ) -> Result<(), JobError> {
        let (document_id, artifact_id, input_name, object_name, inline_bytes) = match input {
            ByteInput::Stored {
                document_id,
                artifact,
            } => {
                let name = object_name_for_uri(&artifact.storage_uri)?;
                (
                    Some(document_id),
                    Some(artifact.id.clone()),
                    artifact.file_name.clone(),
                    Some(name),
                    None,
                )
            }
            ByteInput::Inline { name, bytes } => (None, None, name, None, Some(bytes)),
        };
        let _ = input_name; // conversion formats are declared, not sniffed
        if let (Some(doc), Some(artifact)) = (document_id.as_ref(), artifact_id.as_deref()) {
            begin_media_job(ctx, job, CONVERT_RUN_STATUS_KEY, doc, artifact).await?;
        }

        let from = payload.from;
        let to = payload.to;
        let options = payload.options.clone();
        let engine = self.engine();
        let storage = self.storage.clone();
        let outcome = run_blocking("convert.run", move || {
            let bytes = match object_name {
                Some(object) => {
                    let mut fs = FaultInjectingFs::new(storage)
                        .map_err(|e| JobError::Failed(format!("storage init failed: {e}")))?;
                    let read = read_object_bounded(&mut fs, &object, MEDIA_MAX_INPUT_BYTES)?;
                    if read.truncated {
                        return Ok(ConvertBlockOutcome::SkippedTooLarge(format!(
                            "input exceeds {MEDIA_MAX_INPUT_BYTES} bytes"
                        )));
                    }
                    read.bytes
                }
                None => inline_bytes.unwrap_or_default(),
            };
            let request = ConvertRequest {
                input_format: from,
                output_format: to,
                input: bytes,
                options,
            };
            match engine.convert(&request) {
                Ok(out) => Ok(ConvertBlockOutcome::Done(out)),
                Err(e) => Ok(ConvertBlockOutcome::Failed(e)),
            }
        })
        .await?;

        let pair = format!("{}-{}", from.as_str(), to.as_str());
        let src_tag = artifact_id.as_deref().unwrap_or("inline");
        let extra = |out: Option<&ConvertOutput>| {
            let mut e = json!({
                "from": from.as_str(),
                "to": to.as_str(),
                "pair": pair,
            });
            if let Some(o) = out {
                e["backend"] = json!(o.backend);
                e["warnings"] = json!(o.warnings);
            }
            e
        };

        match outcome {
            ConvertBlockOutcome::SkippedTooLarge(message) => {
                finish_media_job(
                    ctx,
                    &self.storage,
                    job,
                    document_id.as_ref(),
                    artifact_id.as_deref(),
                    CONVERT_RUN_STATUS_KEY,
                    CONVERSIONS_FIELD,
                    "pair",
                    &pair,
                    "skipped_too_large",
                    None,
                    Vec::new(),
                    Vec::new(),
                    Some(message),
                    extra(None),
                )
                .await
            }
            ConvertBlockOutcome::Failed(e) => {
                let status = convert_error_status(&e);
                finish_media_job(
                    ctx,
                    &self.storage,
                    job,
                    document_id.as_ref(),
                    artifact_id.as_deref(),
                    CONVERT_RUN_STATUS_KEY,
                    CONVERSIONS_FIELD,
                    "pair",
                    &pair,
                    status,
                    None,
                    Vec::new(),
                    Vec::new(),
                    Some(e.to_string()),
                    extra(None),
                )
                .await
            }
            ConvertBlockOutcome::Done(out) => {
                let ext = to.extension().unwrap_or("bin").to_string();
                let file_name = format!(
                    "convert-{}-{}-{}.{}",
                    from.as_str(),
                    to.as_str(),
                    sanitize_file_name(src_tag).unwrap_or_else(|_| "input".to_string()),
                    ext
                );
                let warnings = out.warnings.clone();
                let file = JobOutputFile {
                    file_name,
                    bytes: out.bytes.clone(),
                    ext,
                    mime: Some(mime_for_convert_format(to).to_string()),
                };
                finish_media_job(
                    ctx,
                    &self.storage,
                    job,
                    document_id.as_ref(),
                    artifact_id.as_deref(),
                    CONVERT_RUN_STATUS_KEY,
                    CONVERSIONS_FIELD,
                    "pair",
                    &pair,
                    "succeeded",
                    None,
                    vec![file],
                    warnings,
                    None,
                    extra(Some(&out)),
                )
                .await
            }
        }
    }
}

// ---------------------------------------------------------------------------
// document.ingest_recover
// ---------------------------------------------------------------------------

/// `document.ingest_recover` — runs
/// [`DocumentIngestionService::recover_with`]: purges stale
/// artifact-less documents, sweeps abandoned `.staging/` uploads, and
/// GCs unreferenced objects under `documents/`.
///
/// All payload fields are optional; the three `*_secs` fields override
/// [`RecoveryOptions`] grace periods (1 h defaults). `owner_id` is
/// accepted for forward compatibility — recovery is a global sweep.
/// The [`RecoveryReport`] counts land on the job's `progress` field.
pub struct IngestRecoverHandler {
    storage: StorageConfig,
}

impl IngestRecoverHandler {
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

impl JobHandler for IngestRecoverHandler {
    fn handle<'a>(
        &'a self,
        ctx: &'a JobContext,
        job: &'a Job,
    ) -> std::pin::Pin<Box<dyn std::future::Future<Output = Result<(), JobError>> + Send + 'a>>
    {
        Box::pin(async move {
            let secs = |key: &str| -> Result<Option<u64>, JobError> {
                match &job.payload[key] {
                    serde_json::Value::Null => Ok(None),
                    v => v.as_u64().map(Some).ok_or_else(|| {
                        JobError::Failed(format!("payload.{key} must be a non-negative integer"))
                    }),
                }
            };
            let mut options = RecoveryOptions::default();
            if let Some(s) = secs("staging_max_age_secs")? {
                options.staging_max_age = Duration::from_secs(s);
            }
            if let Some(s) = secs("incomplete_document_grace_secs")? {
                options.incomplete_document_grace = Duration::from_secs(s);
            }
            if let Some(s) = secs("unreferenced_object_grace_secs")? {
                options.unreferenced_object_grace = Duration::from_secs(s);
            }

            let service = DocumentIngestionService::new(ctx.pool.clone(), self.storage.clone());
            let report = service
                .recover_with(&options)
                .await
                .map_err(|e| JobError::Failed(format!("ingest recovery failed: {e}")))?;

            let progress = json!({
                "status": "succeeded",
                "staging_entries_removed": report.staging_entries_removed,
                "documents_checked": report.documents_checked,
                "artifacts_checked": report.artifacts_checked,
                "artifacts_missing_objects": report.artifacts_missing_objects,
                "documents_purged": report.documents_purged,
                "unreferenced_objects_removed": report.unreferenced_objects_removed,
                "errors": report.errors,
            });
            let _ = ctx.report_progress(job.id, &progress).await;
            Ok(())
        })
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

/// Enqueues a `document.pdf_op` job. Payload: `{document_id,
/// artifact_id?, op, params?}` — see [`PdfOpHandler`]. `op` must be
/// one of [`PDF_OPS`]; `params` is an opaque JSON object forwarded to
/// the op (use `serde_json::json!({})` or `Value::Null` for none).
pub async fn enqueue_pdf_op(
    pool: &sqlx::SqlitePool,
    document_id: &DocumentId,
    artifact_id: Option<&str>,
    op: &str,
    params: serde_json::Value,
) -> Result<Job, JobError> {
    if !PDF_OPS.contains(&op) {
        return Err(JobError::Failed(format!("unknown pdf op: {op}")));
    }
    let mut payload = doc_job_payload(document_id, artifact_id);
    payload["op"] = json!(op);
    payload["params"] = params;
    let job = enqueue(
        pool,
        &NewJob {
            kind: PDF_OP_KIND.to_string(),
            payload,
            priority: 0,
            run_at: None,
            max_attempts: None,
        },
        DEFAULT_MAX_ATTEMPTS,
    )
    .await?;
    let _ = mark_op_status(pool, document_id, &pdf_op_status_key(op), "queued").await;
    Ok(job)
}

/// Enqueues a `pipeline.run` job. Payload: `{document_id,
/// artifact_id?, pipeline}` where `pipeline` is a full
/// [`PipelineDefinition`] JSON — see [`PipelineRunHandler`]. Callers
/// resolve built-in names to a definition before calling so the
/// payload is self-contained and retries replay identically.
pub async fn enqueue_pipeline_run(
    pool: &sqlx::SqlitePool,
    document_id: &DocumentId,
    artifact_id: Option<&str>,
    definition: &PipelineDefinition,
) -> Result<Job, JobError> {
    let mut payload = doc_job_payload(document_id, artifact_id);
    payload["pipeline"] = serde_json::to_value(definition)?;
    let job = enqueue(
        pool,
        &NewJob {
            kind: PIPELINE_RUN_KIND.to_string(),
            payload,
            priority: 0,
            run_at: None,
            max_attempts: None,
        },
        DEFAULT_MAX_ATTEMPTS,
    )
    .await?;
    let _ = mark_op_status(pool, document_id, PIPELINE_RUN_STATUS_KEY, "queued").await;
    Ok(job)
}

/// Enqueues a `pipeline.run` job carrying a DAG definition. Payload:
/// `{document_id, artifact_id?, dag}` where `dag` is a full
/// [`DagDefinition`] JSON — see [`PipelineRunHandler`]. The same job
/// kind as [`enqueue_pipeline_run`]; the handler dispatches on the
/// payload shape (`dag` key → [`run_dag`]).
pub async fn enqueue_pipeline_dag(
    pool: &sqlx::SqlitePool,
    document_id: &DocumentId,
    artifact_id: Option<&str>,
    dag: &DagDefinition,
) -> Result<Job, JobError> {
    let mut payload = doc_job_payload(document_id, artifact_id);
    payload["dag"] = serde_json::to_value(dag)?;
    let job = enqueue(
        pool,
        &NewJob {
            kind: PIPELINE_RUN_KIND.to_string(),
            payload,
            priority: 0,
            run_at: None,
            max_attempts: None,
        },
        DEFAULT_MAX_ATTEMPTS,
    )
    .await?;
    let _ = mark_op_status(pool, document_id, PIPELINE_RUN_STATUS_KEY, "queued").await;
    Ok(job)
}

/// Enqueues a `storage.scan_inbox` job. Payload: `{owner_id}` plus
/// optional `recurse`/`max_files` — see [`StorageScanInboxHandler`].
/// `owner_id` becomes the owner of every document the scan creates.
pub async fn enqueue_scan_inbox(pool: &sqlx::SqlitePool, owner_id: &str) -> Result<Job, JobError> {
    enqueue(
        pool,
        &NewJob {
            kind: STORAGE_SCAN_INBOX_KIND.to_string(),
            payload: json!({ "owner_id": owner_id }),
            priority: 0,
            run_at: None,
            max_attempts: None,
        },
        DEFAULT_MAX_ATTEMPTS,
    )
    .await
}

/// Enqueues a `document.auto_rename` job. Payload: `{document_id}` —
/// see [`DocumentAutoRenameHandler`]. Typically chained after
/// `document.metadata_extract` once a title is available.
pub async fn enqueue_auto_rename(
    pool: &sqlx::SqlitePool,
    document_id: &DocumentId,
) -> Result<Job, JobError> {
    enqueue(
        pool,
        &NewJob {
            kind: DOCUMENT_AUTO_RENAME_KIND.to_string(),
            payload: doc_job_payload(document_id, None),
            priority: 0,
            run_at: None,
            max_attempts: None,
        },
        DEFAULT_MAX_ATTEMPTS,
    )
    .await
}

/// Enqueues a `media.op` job. Payload: `{document_id?, artifact_id?,
/// op, params?, output_format?, input_b64?}` — see
/// [`MediaOpHandler`]. Exactly one input source is required: a stored
/// artifact (`document_id` and/or `artifact_id`) or `input` inline
/// bytes. `op` must be one of [`media_ops::MEDIA_OPS`]; `params` is an
/// opaque JSON object forwarded to the op.
pub async fn enqueue_media_op(
    pool: &sqlx::SqlitePool,
    document_id: Option<&DocumentId>,
    artifact_id: Option<&str>,
    op: &str,
    params: serde_json::Value,
    input: Option<&[u8]>,
) -> Result<Job, JobError> {
    if !media_ops::MEDIA_OPS.contains(&op) {
        return Err(JobError::Failed(format!("unknown media op: {op}")));
    }
    if document_id.is_none() && artifact_id.is_none() && input.is_none() {
        return Err(JobError::Failed(
            "media.op requires a document/artifact or inline input".into(),
        ));
    }
    let mut payload = serde_json::json!({});
    if let Some(d) = document_id {
        payload["document_id"] = json!(d.as_str());
    }
    if let Some(a) = artifact_id {
        payload["artifact_id"] = json!(a);
    }
    payload["op"] = json!(op);
    payload["params"] = params;
    if let Some(bytes) = input {
        payload["input_b64"] = json!(B64.encode(bytes));
    }
    let job = enqueue(
        pool,
        &NewJob {
            kind: MEDIA_OP_KIND.to_string(),
            payload,
            priority: 0,
            run_at: None,
            max_attempts: None,
        },
        DEFAULT_MAX_ATTEMPTS,
    )
    .await?;
    if let Some(d) = document_id {
        let _ = mark_op_status(pool, d, MEDIA_OP_STATUS_KEY, "queued").await;
    }
    Ok(job)
}

/// Enqueues a `convert.run` job. Payload: `{document_id?,
/// artifact_id?, from_format, to_format, options?, input_b64?}` — see
/// [`ConvertRunHandler`]. Same input-source rules as
/// [`enqueue_media_op`]; `from_format`/`to_format` are
/// [`ConvertFormat`] names (aliases like `md`, `jpg`, `text`
/// accepted), `options` an optional [`EngineConvertOptions`] object.
pub async fn enqueue_convert(
    pool: &sqlx::SqlitePool,
    document_id: Option<&DocumentId>,
    artifact_id: Option<&str>,
    from_format: &str,
    to_format: &str,
    options: Option<serde_json::Value>,
    input: Option<&[u8]>,
) -> Result<Job, JobError> {
    let from = ConvertFormat::parse(from_format)
        .ok_or_else(|| JobError::Failed(format!("unknown from_format: {from_format}")))?;
    let to = ConvertFormat::parse(to_format)
        .ok_or_else(|| JobError::Failed(format!("unknown to_format: {to_format}")))?;
    if document_id.is_none() && artifact_id.is_none() && input.is_none() {
        return Err(JobError::Failed(
            "convert.run requires a document/artifact or inline input".into(),
        ));
    }
    let mut payload = serde_json::json!({
        "from_format": from.as_str(),
        "to_format": to.as_str(),
    });
    if let Some(d) = document_id {
        payload["document_id"] = json!(d.as_str());
    }
    if let Some(a) = artifact_id {
        payload["artifact_id"] = json!(a);
    }
    if let Some(o) = options {
        payload["options"] = o;
    }
    if let Some(bytes) = input {
        payload["input_b64"] = json!(B64.encode(bytes));
    }
    let job = enqueue(
        pool,
        &NewJob {
            kind: CONVERT_RUN_KIND.to_string(),
            payload,
            priority: 0,
            run_at: None,
            max_attempts: None,
        },
        DEFAULT_MAX_ATTEMPTS,
    )
    .await?;
    if let Some(d) = document_id {
        let _ = mark_op_status(pool, d, CONVERT_RUN_STATUS_KEY, "queued").await;
    }
    Ok(job)
}

/// Enqueues a `document.ingest_recover` job. Payload: all fields
/// optional — `owner_id` (accepted for forward compatibility), plus
/// `staging_max_age_secs` / `incomplete_document_grace_secs` /
/// `unreferenced_object_grace_secs` overriding the
/// [`RecoveryOptions`] grace periods. See [`IngestRecoverHandler`].
pub async fn enqueue_ingest_recover(
    pool: &sqlx::SqlitePool,
    owner_id: Option<&str>,
    staging_max_age_secs: Option<u64>,
    incomplete_document_grace_secs: Option<u64>,
    unreferenced_object_grace_secs: Option<u64>,
) -> Result<Job, JobError> {
    let mut payload = serde_json::json!({});
    if let Some(o) = owner_id {
        payload["owner_id"] = json!(o);
    }
    if let Some(s) = staging_max_age_secs {
        payload["staging_max_age_secs"] = json!(s);
    }
    if let Some(s) = incomplete_document_grace_secs {
        payload["incomplete_document_grace_secs"] = json!(s);
    }
    if let Some(s) = unreferenced_object_grace_secs {
        payload["unreferenced_object_grace_secs"] = json!(s);
    }
    enqueue(
        pool,
        &NewJob {
            kind: DOCUMENT_INGEST_RECOVER_KIND.to_string(),
            payload,
            priority: 0,
            run_at: None,
            max_attempts: None,
        },
        DEFAULT_MAX_ATTEMPTS,
    )
    .await
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
            Arc::new(DocumentMetadataExtractHandler::with_storage_config(
                storage(),
            )),
        );
        eng.register(
            DOCUMENT_INDEX_KIND,
            Arc::new(DocumentIndexHandler::with_storage_config(storage())),
        );
        eng.register(
            THUMBNAIL_GENERATE_KIND,
            Arc::new(ThumbnailGenerateHandler::with_storage_config(storage())),
        );
        eng.register(
            PDF_OP_KIND,
            Arc::new(PdfOpHandler::with_storage_config(storage())),
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
        ingest_doc(
            &eng,
            &pool,
            &doc_id,
            &test_pdf_bytes(),
            "report.pdf",
            "application/pdf",
        )
        .await;

        enqueue_metadata_extract(&pool, &doc_id, None)
            .await
            .unwrap();
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
        let jobs = list_jobs(&pool, Some(JobStatus::Succeeded), 10)
            .await
            .unwrap();
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

        enqueue_metadata_extract(&pool, &doc_id, None)
            .await
            .unwrap();
        assert!(eng.run_once().await.unwrap());

        let meta = metadata_of(&pool, &doc_id).await;
        assert_eq!(
            meta.custom_fields["metadata_extract_status"],
            json!("unsupported")
        );
        assert_eq!(
            meta.custom_fields["processing_status"],
            json!("unsupported")
        );
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
        ingest_doc(
            &eng,
            &pool,
            &doc_id,
            &test_pdf_bytes(),
            "report.pdf",
            "application/pdf",
        )
        .await;

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
        assert!(
            index["excerpt"]
                .as_str()
                .unwrap()
                .contains("quick brown fox")
        );
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
        assert_eq!(
            meta.custom_fields["processing_status"],
            json!("unsupported")
        );
        assert!(meta.custom_fields.get("index").is_none());
        // Job itself succeeded.
        let jobs = list_jobs(&pool, Some(JobStatus::Succeeded), 10)
            .await
            .unwrap();
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
        let uri = meta.custom_fields["thumbnail_uri"]
            .as_str()
            .unwrap()
            .to_string();
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
        ingest_doc(
            &eng,
            &pool,
            &doc_id,
            &test_pdf_bytes(),
            "plain.pdf",
            "application/pdf",
        )
        .await;

        enqueue_thumbnail(&pool, &doc_id, None).await.unwrap();
        assert!(eng.run_once().await.unwrap());

        let meta = metadata_of(&pool, &doc_id).await;
        assert_eq!(meta.custom_fields["thumbnail_status"], json!("unavailable"));
        assert_eq!(
            meta.custom_fields["processing_status"],
            json!("unavailable")
        );
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
        ingest_doc(
            &eng,
            &pool,
            &doc_id,
            &test_pdf_bytes(),
            "r.pdf",
            "application/pdf",
        )
        .await;

        // Enqueue helper stamps "queued".
        enqueue_metadata_extract(&pool, &doc_id, None)
            .await
            .unwrap();
        let meta = metadata_of(&pool, &doc_id).await;
        assert_eq!(meta.custom_fields["processing_status"], json!("queued"));
        assert_eq!(
            meta.custom_fields["metadata_extract_status"],
            json!("queued")
        );

        assert!(eng.run_once().await.unwrap());
        let meta = metadata_of(&pool, &doc_id).await;
        assert_eq!(meta.custom_fields["processing_status"], json!("succeeded"));
        assert_eq!(
            meta.custom_fields["metadata_extract_status"],
            json!("extracted")
        );

        // Audit ordering: started before completed.
        let history = get_document_history(&pool, &doc_id).await.unwrap();
        let pos = |t: DocumentAuditEventType| history.iter().position(|e| e.event_type == t);
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
        ingest_doc(
            &eng,
            &pool,
            &doc_id,
            &test_pdf_with_image_bytes(),
            "i.pdf",
            "application/pdf",
        )
        .await;

        // Run each processing job twice — second run must succeed and
        // leave a single consistent state (no duplicate artifacts,
        // deterministic thumbnail path).
        for kind_payload in [
            (
                DOCUMENT_METADATA_EXTRACT_KIND,
                json!({"document_id": doc_id.as_str()}),
            ),
            (DOCUMENT_INDEX_KIND, json!({"document_id": doc_id.as_str()})),
            (
                THUMBNAIL_GENERATE_KIND,
                json!({"document_id": doc_id.as_str()}),
            ),
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
        assert_eq!(
            meta.custom_fields["metadata_extract_status"],
            json!("extracted")
        );
        assert_eq!(meta.custom_fields["index_status"], json!("indexed"));
        assert_eq!(meta.custom_fields["thumbnail_status"], json!("generated"));
        // Exactly one thumbnail object on disk.
        let thumbs = root
            .join("documents")
            .join(doc_id.as_str())
            .join("thumbnails");
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
        ingest_doc(
            &eng,
            &pool,
            &doc_id,
            &test_pdf_bytes(),
            "c.pdf",
            "application/pdf",
        )
        .await;

        enqueue_index(&pool, &doc_id, None).await.unwrap();
        let job = list_jobs(&pool, None, 10)
            .await
            .unwrap()
            .into_iter()
            .find(|j| j.kind == DOCUMENT_INDEX_KIND)
            .unwrap();

        // Simulate a worker that claimed the job then died.
        sqlx::query(
            "UPDATE jobs_tbl SET status = 'running', worker_id = 'dead-worker',
             started_at = ?, attempts = 1 WHERE id = ?",
        )
        .bind(
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_secs() as i64,
        )
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

    // -----------------------------------------------------------------
    // document.pdf_op
    // -----------------------------------------------------------------

    #[tokio::test]
    async fn pdf_op_page_count_reports_inline() {
        let pool = test_pool().await;
        let root = temp_root();
        let doc_id = make_document(&pool).await;
        let eng = full_engine(pool.clone(), &root);
        ingest_doc(
            &eng,
            &pool,
            &doc_id,
            &test_pdf_bytes(),
            "report.pdf",
            "application/pdf",
        )
        .await;

        enqueue_pdf_op(&pool, &doc_id, None, "page_count", json!({}))
            .await
            .unwrap();
        assert!(eng.run_once().await.unwrap());

        let meta = metadata_of(&pool, &doc_id).await;
        assert_eq!(meta.custom_fields["page_count_status"], json!("succeeded"));
        assert_eq!(meta.custom_fields["processing_status"], json!("succeeded"));
        let entry = &meta.custom_fields["pdf_ops"]["page_count"];
        assert_eq!(entry["status"], json!("succeeded"));
        assert_eq!(entry["result"], json!(1));
        assert_eq!(entry["outputs"], json!([]));
        std::fs::remove_dir_all(&root).ok();
    }

    #[tokio::test]
    async fn pdf_op_rotate_writes_output_artifact() {
        let pool = test_pool().await;
        let root = temp_root();
        let doc_id = make_document(&pool).await;
        let eng = full_engine(pool.clone(), &root);
        ingest_doc(
            &eng,
            &pool,
            &doc_id,
            &test_pdf_bytes(),
            "report.pdf",
            "application/pdf",
        )
        .await;

        enqueue_pdf_op(
            &pool,
            &doc_id,
            None,
            "rotate",
            json!({ "degrees": 90, "pages": "all" }),
        )
        .await
        .unwrap();
        assert!(eng.run_once().await.unwrap());

        // The rotated PDF landed as a second artifact under outputs/.
        let arts = list_artifacts(&pool, &doc_id).await.unwrap();
        assert_eq!(arts.len(), 2);
        let out = arts
            .iter()
            .find(|a| a.storage_uri.contains("/outputs/"))
            .expect("output artifact");
        assert!(out.storage_uri.ends_with(".pdf"));
        assert_eq!(out.mime_type.as_deref(), Some("application/pdf"));
        let object_name = out.storage_uri.strip_prefix("local://").unwrap();
        assert!(root.join(object_name).is_file());
        // Output bytes really are a PDF.
        let bytes = std::fs::read(root.join(object_name)).unwrap();
        assert!(bytes.starts_with(b"%PDF"));

        let meta = metadata_of(&pool, &doc_id).await;
        assert_eq!(meta.custom_fields["rotate_status"], json!("succeeded"));
        let entry = &meta.custom_fields["pdf_ops"]["rotate"];
        assert_eq!(entry["status"], json!("succeeded"));
        assert_eq!(entry["outputs"][0]["artifact_id"], json!(out.id));
        std::fs::remove_dir_all(&root).ok();
    }

    #[tokio::test]
    async fn pdf_op_bad_params_record_error_not_job_failure() {
        let pool = test_pool().await;
        let root = temp_root();
        let doc_id = make_document(&pool).await;
        let eng = full_engine(pool.clone(), &root);
        ingest_doc(
            &eng,
            &pool,
            &doc_id,
            &test_pdf_bytes(),
            "report.pdf",
            "application/pdf",
        )
        .await;

        // rotate without degrees → deterministic param error; the job
        // still succeeds and the document records the failure.
        enqueue_pdf_op(&pool, &doc_id, None, "rotate", json!({}))
            .await
            .unwrap();
        assert!(eng.run_once().await.unwrap());

        let meta = metadata_of(&pool, &doc_id).await;
        assert_eq!(meta.custom_fields["rotate_status"], json!("error"));
        assert_eq!(meta.custom_fields["processing_status"], json!("failed"));
        assert_eq!(
            meta.custom_fields["pdf_ops"]["rotate"]["status"],
            json!("error")
        );
        let jobs = list_jobs(&pool, Some(JobStatus::Succeeded), 10)
            .await
            .unwrap();
        assert_eq!(jobs.len(), 2); // ingest + pdf_op
        std::fs::remove_dir_all(&root).ok();
    }

    #[tokio::test]
    async fn pdf_op_rejects_unknown_op_at_enqueue() {
        let pool = test_pool().await;
        let doc_id = make_document(&pool).await;
        assert!(
            enqueue_pdf_op(&pool, &doc_id, None, "explode", json!({}))
                .await
                .is_err()
        );
    }
}
