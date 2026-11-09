# Implementation Report — Storage Streaming, Document Persistence, Ingestion Pipeline & Job Integration

Date: 2025-11-09 (verification pass by final agent)
Scope: storage streaming, document-engine persistence (metadata +
relationships), application services (documents/storage/ingestion),
job handlers, CLI `doc`/`engine` commands, plus the in-flight engine
expansion (pdf/image/conversion/pipeline ops) that shares the crate.

## Verification status (Definition of Done)

| Gate | Result |
|---|---|
| `cargo fmt --check` | clean |
| `cargo check --all-targets` | clean, 0 errors |
| `cargo clippy --all-targets` | 0 warnings in scope crates (document_engine, storage, application, api, job_engine, engine-registry); ~40 warnings remain in unrelated in-flight engines — see Known limitations |
| `cargo test` | **466 passed, 0 failed** (lib) + 3 doctests passed, 1 ignored |

## Files changed/added (by milestone area)

### Storage streaming
- `src/crates/storage/stream.rs` — `StreamingBackend` trait (GAT
  `Upload<'a>`), `StreamUpload` (`write_chunk`/`commit`/`abort`/
  `staged_bytes`), `StreamLimits`, `CommitReceipt{name,bytes,sha256}`,
  `LocalUploadWriter`, staging under `<root>/.staging/`, atomic
  rename publish, `sweep_staging`, `open_reader`, `StorageError::TooLarge`.
  Implemented for `FaultInjectingFs`.
- `src/crates/storage/remote/{mod,config,s3}.rs` — S3-compatible
  backend (SigV4, path-style, `ureq` transport, retry policy, quota
  accounting) — added as part of the same storage milestone.

### Document persistence (`document_engine`)
- `src/crates/document_engine/src/store/schema.rs` —
  `document_metadata_tbl`, `document_relationships_tbl` + indexes.
- `src/crates/document_engine/src/store/metadata.rs` —
  `upsert_document_metadata`, `get_document_metadata`,
  `set_custom_field`, `remove_custom_field`,
  `delete_document_metadata`.
- `src/crates/document_engine/src/store/relationships.rs` —
  `create_relationship`, `delete_relationship`,
  `list_outgoing_relationships`, `list_incoming_relationships`,
  `related_documents`.
- `src/crates/document_engine/src/audit.rs` — new event types:
  `MetadataUpdated`, `RelationshipCreated`, `RelationshipDeleted`,
  `ProcessingStarted`, `ProcessingCompleted`, `ProcessingFailed`,
  `Indexed`, `ThumbnailGenerated` (alongside existing `Imported`).

### Application services
- `src/crates/application/services/documents.rs` — `DocumentService`
  facade: documents, folders, artifacts, versions
  (`create_version`/`get_version`/`list_versions`/`restore_version`),
  tags, permissions, metadata, relationships, audit history.
- `src/crates/application/services/storage.rs` — `StorageService`
  over concrete `FaultInjectingFs`: `from_config`/`from_backend`,
  `put`/`get`/`delete`/`exists`/`used_bytes`/`status`, plus streaming
  surface `stream_limits`/`begin_upload`/`open_reader`/`sweep_staging`
  (staging is swept on construction).
- `src/crates/application/services/ingestion.rs` —
  `DocumentIngestionService`: `ingest(AsyncRead, IngestRequest) ->
  IngestOutcome`, `recover()`/`recover_with()`, `with_limits`.
  Format sniffing (512-byte peek), chunked staged upload inside one
  `spawn_blocking`, atomic commit → artifact + version 1 + metadata +
  `Imported` audit event, explicit compensation on post-commit
  failure, cancel-safe, conservative crash recovery.
- `src/crates/application/services/app.rs` — `AppServices` aggregate
  (`storage`, `documents`, `ingestion`, `pool`);
  `AppServices::new(storage, pool, &StorageConfig)` /
  `AppServices::init(&AppConfig)`.
- `src/crates/application/services/mod.rs` — module docs + re-exports.

### Job integration
- `src/crates/application/job_handlers.rs` — kinds `document.ingest`
  (streaming upgrade), `document.metadata_extract`, `document.index`,
  `thumbnail.generate`; `register_builtin` registers all plus
  `system.ping`, `pdf.inspect`, `storage.gc_staging`; enqueue helpers
  `enqueue_ingest`/`enqueue_metadata_extract`/`enqueue_index`/
  `enqueue_thumbnail`; per-document progress via metadata custom
  fields (`processing_status`, `{op}_status`) + audit events.

### CLI / API
- `src/crates/application/cli_mcp_server/cli/command/{doc,engine,serve}.rs`,
  `envfile.rs`, `args.rs`, `command/mod.rs`, `cli/{mod,error}.rs` —
  new `kaladoc doc …` and `kaladoc engine …` commands.
- `src/crates/api/` (new module, `#[path]`-mounted in
  `src/crates/mod.rs`) — axum HTTP layer: `GET /health`,
  `POST /documents`, `GET /documents/{id}`, versions/artifacts/
  folders/tags/jobs endpoints, Bearer session auth, stable JSON error
  envelope. Deps added: `axum`, `hmac`, `hex`, `tempfile`, `tower`,
  `ureq` (dev), tokio `net`/`signal` features.

## Database schema additions

```sql
CREATE TABLE IF NOT EXISTS document_metadata_tbl (
    document_id   TEXT PRIMARY KEY REFERENCES documents_tbl(id) ON DELETE CASCADE,
    title         TEXT,
    author        TEXT,
    subject       TEXT,
    language      TEXT,
    keywords      TEXT NOT NULL DEFAULT '[]',
    custom_fields TEXT NOT NULL DEFAULT '{}',
    created_at    TEXT NOT NULL,
    updated_at    TEXT NOT NULL
);

CREATE TABLE IF NOT EXISTS document_relationships_tbl (
    id                  TEXT PRIMARY KEY,
    source_document_id  TEXT NOT NULL REFERENCES documents_tbl(id) ON DELETE CASCADE,
    target_document_id  TEXT NOT NULL REFERENCES documents_tbl(id) ON DELETE CASCADE,
    relationship_type   TEXT NOT NULL,
    created_by          TEXT,
    created_at          TEXT NOT NULL,
    UNIQUE (source_document_id, target_document_id, relationship_type)
);
CREATE INDEX IF NOT EXISTS idx_doc_relationships_source ON document_relationships_tbl(source_document_id);
CREATE INDEX IF NOT EXISTS idx_doc_relationships_target ON document_relationships_tbl(target_document_id);
```

`init_document_store` is idempotent (`IF NOT EXISTS` everywhere) —
verified by every service test that calls it on a fresh in-memory
pool, and `test_pool()` helpers run it per test.

## Public API surface (key items)

- `storage::stream`: `StreamingBackend`, `StreamUpload`,
  `StreamLimits`, `CommitReceipt`, `LocalUploadWriter`,
  `sweep_staging`, `open_reader`, `StorageError::TooLarge`.
- `document_engine::store::metadata`: `DocumentMetadata`,
  `upsert/get/set_custom_field/remove_custom_field/delete` fns.
- `document_engine::store::relationships`:
  `DocumentRelationship(Type)`, `RelatedDocument`, CRUD/list fns.
- `application::services`: `DocumentService` (full facade),
  `StorageService` (+ streaming methods), `DocumentIngestionService`
  (`ingest`, `recover`, `with_limits`), `IngestRequest`,
  `IngestOutcome`, `RecoveryReport`, `RecoveryOptions`,
  `AppServices` (`new`/`init`), `StorageStatus`.
- `application::job_handlers`: `register_builtin`, kind constants,
  `enqueue_*` helpers.
- `api`: `ApiState::init`, `serve`, `router`, `ApiError`.

## Test coverage vs. Definition of Done

Covered by tests (all green):

- **E2E flow** — `ingestion.rs::ingest_retrieve_version_list_and_restore`
  (added in this pass): ingest → retrieve document/artifact/metadata →
  create version 2 → list versions → restore version 1 → artifacts
  retained.
- Ingestion unit coverage: happy-path PDF ingest (bytes on disk,
  metadata row, `Imported` audit, empty staging), format-detection
  table, oversize rollback, empty upload, unsafe names, post-commit
  failure rollback, injected commit fault, cancellation + recover,
  staging sweep / incomplete-doc purge / orphan GC, fresh-doc grace,
  concurrent ingests, tags/folder/actor metadata.
- Store coverage: metadata roundtrip/custom fields, relationships
  (dup/self-link rejection, directional listing, `related_documents`),
  document CRUD/trash/restore/purge, folder+artifact flow.
- Storage streaming: chunked-vs-write checksum, atomic commit,
  abort/oversize cleanup, quota accounting, path traversal, reader
  streaming, staging sweep, S3 SigV4/retry/quota/path-confinement via
  mock transport.
- Jobs: ingest idempotency/rejection, metadata/index/thumbnail
  handlers, crash recovery re-execution, GC staging.

## Fixes made during verification (beyond formatting)

- `pdf-engine/ops/compress.rs` — `save_with_options` returns
  `io::Result`; mapped via `PdfError::Encode` (was `lopdf_err`).
- `pdf-engine/ops/{flatten,sanitize,repair,resize}.rs` — lopdf 0.47
  API drift (`add_object` infallible, unused imports, test-module
  fixes, borrow/lifetime fixes).
- `image-engine/ops/rotate.rs` — stale `load_image`/`encode` arity.
- `image-engine/ops/watermark.rs` — `mut` closure binding.
- `storage/remote/s3.rs` — grouped 8-arg SigV4 helpers into
  `SignatureParams`; redundant closure → `TransportError::Io`.
- `storage/remote/config.rs` — test helper lifetime (`+ use<>`).
- `storage/stream.rs` — `io::Error::other` instead of
  `ErrorKind::Other` construction.
- `api/handlers.rs`, `job_engine/store.rs` — clippy
  (`&sum.algorithm`, needless `Ok(..?)`).
- `cli/command/{doc,engine}.rs` — `clippy::print_literal` header
  literals inlined.
- `pdf-engine/src/tests.rs` + `engine-registry/src/registry.rs` —
  tests updated for the expanded advertised capabilities and
  environment-dependent health (pdf engine honestly reports
  `Degraded` when `tesseract` is absent; `pdftoppm` present).
- `pdf-engine/src/lib.rs`/`ops/mod.rs` — re-exported the new ops'
  types/functions so they are reachable public API.
- Added the missing E2E version/restore test described above.

## Known limitations

- `LocalUploadWriter` is single-flight per backend instance (the GAT
  borrows the backend mutably for the writer's lifetime); concurrent
  uploads need separate backend handles.
- `StreamingBackend` is synchronous; async callers must use
  `spawn_blocking` (ingestion does exactly that).
- `thumbnail.generate` only extracts images embedded in the PDF;
  it does not rasterize pages.
- `document.index` stores extracted text in metadata `custom_fields`,
  not a dedicated search table — no full-text search yet.
- CLI has document/engine commands but no dedicated `upload`
  subcommand yet (ingest is driven via jobs/API).
- pdf `sign` op exists but is not advertised in engine capabilities.
- Remaining clippy warnings (~40) are confined to the unrelated
  in-flight engines: dead/duplicate helper code in
  `pdf-engine/ops/{overlay,watermark,stamp,...}.rs`, style lints in
  `conversion-engine`, `image-engine`, `pipeline-engine`,
  `security-engine`. Left untouched per scope; they do not affect
  correctness or the test suite.

## Remaining work

- Wire `sign` into `PdfEngine` capabilities/operations if intended.
- Deduplicate the content/resource helpers that currently exist in
  both `pdf-engine/ops/overlay.rs` and `ops/watermark.rs`.
- Optional: FTS/search table for `document.index` output; CLI upload
  command; thumbnail fallback via `pdftoppm` rasterization.
- Remote (S3) streaming backend: `StreamingBackend` is currently
  implemented for the local `FaultInjectingFs` only.
