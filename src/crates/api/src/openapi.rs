//! Hand-written OpenAPI 3.x description of the HTTP API.
//!
//! Served at `GET /openapi.json` (public, like `/health` — API
//! discovery shouldn't require a session). Written by hand with
//! `serde_json::json!` rather than generated: the surface is small
//! enough that a maintained document is clearer than a codegen step,
//! and the crate stays dependency-free. Keep this file in sync with
//! the route table in [`crate::crates::api`] docs and the router in
//! `lib.rs` — there is no drift check.
//!
//! Conventions:
//!
//! - Every route except `/health` and `/openapi.json` requires
//!   `Authorization: Bearer <credential>` — an Organization session
//!   token or a static API key (`KALADOC_API_KEYS`; see `auth.rs`).
//!   Route groups disabled via `KALADOC_API_DISABLE` answer `404`.
//! - Errors use the stable envelope
//!   `{"error": {"code", "message", "request_id", "retryable"}}`.
//! - Job payloads/statuses are described in `job_handlers.rs`; the
//!   `Job` schema below covers the shared fields.

use serde_json::{Value, json};

/// The OpenAPI 3.0.3 document for this API.
pub fn spec() -> Value {
    json!({
        "openapi": "3.0.3",
        "info": {
            "title": "KalaDoc API",
            "version": "0.1.0",
            "description": "Document ingest, storage, and processing API. Thin REST adapter over the application services and durable job queue.",
        },
        "servers": [{ "url": "/" }],
        "tags": [
            { "name": "system" },
            { "name": "documents" },
            { "name": "folders" },
            { "name": "jobs" },
            { "name": "pipelines" },
            { "name": "schedules" },
        ],
        "components": {
            "securitySchemes": {
                "bearerAuth": {
                    "type": "http",
                    "scheme": "bearer",
                    "description": "Either an Organization session token from `sessions_tbl` (validated on every protected request) or a static API key configured via `KALADOC_API_KEYS` / `api.api_keys` (`name:key` pairs). API-key callers act as `apikey:<name>`.",
                },
            },
            "schemas": {
                "Error": {
                    "type": "object",
                    "required": ["error"],
                    "properties": {
                        "error": {
                            "type": "object",
                            "required": ["code", "message", "request_id", "retryable"],
                            "properties": {
                                "code": { "type": "string", "example": "not_found" },
                                "message": { "type": "string" },
                                "request_id": { "type": "string" },
                                "retryable": { "type": "boolean" },
                            },
                        },
                    },
                },
                "Job": {
                    "type": "object",
                    "properties": {
                        "id": { "type": "integer", "format": "int64" },
                        "kind": { "type": "string", "example": "document.metadata_extract" },
                        "payload": { "type": "object" },
                        "status": {
                            "type": "string",
                            "enum": ["pending", "running", "succeeded", "failed", "cancelled", "skipped"],
                        },
                        "priority": { "type": "integer" },
                        "attempts": { "type": "integer" },
                        "max_attempts": { "type": "integer" },
                        "run_at": { "type": "integer", "description": "Unix seconds; earliest claimable time." },
                        "created_at": { "type": "integer" },
                        "started_at": { "type": ["integer", "null"] },
                        "finished_at": { "type": ["integer", "null"] },
                        "last_error": { "type": ["string", "null"] },
                        "worker_id": { "type": ["string", "null"] },
                        "batch_id": { "type": ["integer", "null"], "description": "Shared id of the batch this job belongs to (first job's id)." },
                        "progress": { "type": ["object", "null"], "description": "Latest handler-reported progress snapshot while running." },
                    },
                },
                "BatchProgress": {
                    "type": "object",
                    "properties": {
                        "items_total": { "type": "integer" },
                        "items_done": { "type": "integer" },
                        "items_pending": { "type": "integer" },
                        "items_succeeded": { "type": "integer" },
                        "items_failed": { "type": "integer" },
                        "items_skipped": { "type": "integer" },
                        "items_cancelled": { "type": "integer" },
                        "pct": { "type": "integer", "minimum": 0, "maximum": 100 },
                        "complete": { "type": "boolean" },
                    },
                },
                "PipelineStep": {
                    "type": "object",
                    "required": ["name", "engine", "operation"],
                    "properties": {
                        "name": { "type": "string" },
                        "engine": { "type": "string", "enum": ["pdf", "security"] },
                        "operation": { "type": "string", "description": "See pipeline-engine `operation` constants (~90 pdf ops + security gates)." },
                        "params": { "type": "object" },
                    },
                },
                "PipelineDefinition": {
                    "type": "object",
                    "required": ["name", "steps"],
                    "properties": {
                        "name": { "type": "string" },
                        "description": { "type": "string" },
                        "steps": { "type": "array", "items": { "$ref": "#/components/schemas/PipelineStep" } },
                    },
                },
            },
            "parameters": {
                "DocumentId": {
                    "name": "id",
                    "in": "path",
                    "required": true,
                    "schema": { "type": "string" },
                    "description": "Document id.",
                },
            },
            "responses": {
                "BadRequest": {
                    "description": "Malformed request.",
                    "content": { "application/json": { "schema": { "$ref": "#/components/schemas/Error" } } },
                },
                "Unauthorized": {
                    "description": "Missing or invalid Bearer token.",
                    "content": { "application/json": { "schema": { "$ref": "#/components/schemas/Error" } } },
                },
                "NotFound": {
                    "description": "Resource not found.",
                    "content": { "application/json": { "schema": { "$ref": "#/components/schemas/Error" } } },
                },
                "Forbidden": {
                    "description": "Authenticated but lacking document access (owner or permission grant required).",
                    "content": { "application/json": { "schema": { "$ref": "#/components/schemas/Error" } } },
                },
                "Conflict": {
                    "description": "State conflict (e.g. job already terminal).",
                    "content": { "application/json": { "schema": { "$ref": "#/components/schemas/Error" } } },
                },
            },
        },
        "paths": {
            "/health": {
                "get": {
                    "tags": ["system"],
                    "summary": "Aggregated engine health (public)",
                    "responses": {
                        "200": { "description": "Overall health plus per-component detail.", "content": { "application/json": { "schema": { "type": "object" } } } },
                        "503": { "description": "Overall status `unavailable`.", "content": { "application/json": { "schema": { "type": "object" } } } },
                    },
                },
            },
            "/openapi.json": {
                "get": {
                    "tags": ["system"],
                    "summary": "This OpenAPI document (public)",
                    "responses": {
                        "200": { "description": "OpenAPI 3.0.3 JSON.", "content": { "application/json": { "schema": { "type": "object" } } } },
                    },
                },
            },
            "/engines": {
                "get": {
                    "tags": ["system"],
                    "summary": "Engine registry + per-engine health",
                    "security": [{ "bearerAuth": [] }],
                    "responses": {
                        "200": { "description": "`{engines, health}`.", "content": { "application/json": { "schema": { "type": "object" } } } },
                        "401": { "$ref": "#/components/responses/Unauthorized" },
                    },
                },
            },
            "/storage": {
                "get": {
                    "tags": ["system"],
                    "summary": "Storage backend status",
                    "security": [{ "bearerAuth": [] }],
                    "responses": {
                        "200": { "description": "Backend kind, root configured, usage, quota.", "content": { "application/json": { "schema": { "type": "object" } } } },
                        "401": { "$ref": "#/components/responses/Unauthorized" },
                    },
                },
            },
            "/documents": {
                "get": {
                    "tags": ["documents"],
                    "summary": "List documents (`?q=` search, owner/folder filters)",
                    "security": [{ "bearerAuth": [] }],
                    "parameters": [
                        { "name": "q", "in": "query", "schema": { "type": "string" }, "description": "Case-insensitive name substring; switches to search mode." },
                        { "name": "owner_id", "in": "query", "schema": { "type": "string" }, "description": "Defaults to the caller." },
                        { "name": "folder_id", "in": "query", "schema": { "type": "string" } },
                        { "name": "include_deleted", "in": "query", "schema": { "type": "boolean" } },
                        { "name": "limit", "in": "query", "schema": { "type": "integer", "default": 100, "maximum": 1000 } },
                    ],
                    "responses": {
                        "200": { "description": "`{documents: [...]}`.", "content": { "application/json": { "schema": { "type": "object" } } } },
                        "401": { "$ref": "#/components/responses/Unauthorized" },
                    },
                },
                "post": {
                    "tags": ["documents"],
                    "summary": "Upload document bytes → validate → ingest",
                    "description": "Raw request body is the file content. Filename from `?filename=`, then `x-file-name` header, else `upload.bin`. Enqueues a `document.metadata_extract` job.",
                    "security": [{ "bearerAuth": [] }],
                    "parameters": [
                        { "name": "filename", "in": "query", "schema": { "type": "string" } },
                    ],
                    "requestBody": {
                        "required": true,
                        "content": {
                            "application/octet-stream": { "schema": { "type": "string", "format": "binary" } },
                            "*/*": { "schema": { "type": "string", "format": "binary" } },
                        },
                    },
                    "responses": {
                        "201": { "description": "`{document, artifact, version, storage_uri, size_bytes, sha256, detected_mime_type, processing_job}`.", "content": { "application/json": { "schema": { "type": "object" } } } },
                        "400": { "$ref": "#/components/responses/BadRequest" },
                        "401": { "$ref": "#/components/responses/Unauthorized" },
                        "413": { "description": "Upload exceeds the size limit.", "content": { "application/json": { "schema": { "$ref": "#/components/schemas/Error" } } } },
                    },
                },
            },
            "/documents/{id}": {
                "get": {
                    "tags": ["documents"],
                    "summary": "Document metadata + artifacts",
                    "security": [{ "bearerAuth": [] }],
                    "parameters": [{ "$ref": "#/components/parameters/DocumentId" }],
                    "responses": {
                        "200": { "description": "`{document, artifacts, metadata}`.", "content": { "application/json": { "schema": { "type": "object" } } } },
                        "401": { "$ref": "#/components/responses/Unauthorized" },
                        "404": { "$ref": "#/components/responses/NotFound" },
                    },
                },
                "patch": {
                    "tags": ["documents"],
                    "summary": "Rename / retype / move to folder",
                    "security": [{ "bearerAuth": [] }],
                    "parameters": [{ "$ref": "#/components/parameters/DocumentId" }],
                    "requestBody": {
                        "required": true,
                        "content": { "application/json": { "schema": {
                            "type": "object",
                            "properties": {
                                "name": { "type": "string" },
                                "document_type": { "type": "string" },
                                "folder_id": { "type": ["string", "null"], "description": "Absent = unchanged; null = move to root." },
                            },
                        } } },
                    },
                    "responses": {
                        "200": { "description": "`{document}`.", "content": { "application/json": { "schema": { "type": "object" } } } },
                        "400": { "$ref": "#/components/responses/BadRequest" },
                        "401": { "$ref": "#/components/responses/Unauthorized" },
                        "404": { "$ref": "#/components/responses/NotFound" },
                    },
                },
                "delete": {
                    "tags": ["documents"],
                    "summary": "Trash (soft delete)",
                    "security": [{ "bearerAuth": [] }],
                    "parameters": [{ "$ref": "#/components/parameters/DocumentId" }],
                    "responses": {
                        "200": { "description": "`{document}` (trashed).", "content": { "application/json": { "schema": { "type": "object" } } } },
                        "401": { "$ref": "#/components/responses/Unauthorized" },
                        "404": { "$ref": "#/components/responses/NotFound" },
                    },
                },
            },
            "/documents/{id}/restore": {
                "post": {
                    "tags": ["documents"],
                    "summary": "Restore a trashed document",
                    "security": [{ "bearerAuth": [] }],
                    "parameters": [{ "$ref": "#/components/parameters/DocumentId" }],
                    "responses": {
                        "200": { "description": "`{document}`.", "content": { "application/json": { "schema": { "type": "object" } } } },
                        "401": { "$ref": "#/components/responses/Unauthorized" },
                        "404": { "$ref": "#/components/responses/NotFound" },
                    },
                },
            },
            "/documents/{id}/versions": {
                "get": {
                    "tags": ["documents"],
                    "summary": "Version list",
                    "security": [{ "bearerAuth": [] }],
                    "parameters": [{ "$ref": "#/components/parameters/DocumentId" }],
                    "responses": {
                        "200": { "description": "`{versions: [...]}`.", "content": { "application/json": { "schema": { "type": "object" } } } },
                        "401": { "$ref": "#/components/responses/Unauthorized" },
                        "403": { "$ref": "#/components/responses/Forbidden" },
                        "404": { "$ref": "#/components/responses/NotFound" },
                    },
                },
                "post": {
                    "tags": ["documents"],
                    "summary": "Create a new version",
                    "description": "Requires `write` access. `artifact_id` absent stores a version with no artifact pointer.",
                    "security": [{ "bearerAuth": [] }],
                    "parameters": [{ "$ref": "#/components/parameters/DocumentId" }],
                    "requestBody": {
                        "required": false,
                        "content": { "application/json": { "schema": {
                            "type": "object",
                            "properties": {
                                "artifact_id": { "type": "string" },
                                "note": { "type": "string" },
                            },
                        } } },
                    },
                    "responses": {
                        "201": { "description": "`{version}`.", "content": { "application/json": { "schema": { "type": "object" } } } },
                        "400": { "$ref": "#/components/responses/BadRequest" },
                        "401": { "$ref": "#/components/responses/Unauthorized" },
                        "403": { "$ref": "#/components/responses/Forbidden" },
                        "404": { "$ref": "#/components/responses/NotFound" },
                    },
                },
            },
            "/documents/{id}/versions/{version}/restore": {
                "post": {
                    "tags": ["documents"],
                    "summary": "Restore version pointer",
                    "security": [{ "bearerAuth": [] }],
                    "parameters": [
                        { "$ref": "#/components/parameters/DocumentId" },
                        { "name": "version", "in": "path", "required": true, "schema": { "type": "integer" } },
                    ],
                    "responses": {
                        "200": { "description": "`{document}`.", "content": { "application/json": { "schema": { "type": "object" } } } },
                        "401": { "$ref": "#/components/responses/Unauthorized" },
                        "404": { "$ref": "#/components/responses/NotFound" },
                    },
                },
            },
            "/documents/{id}/tags": {
                "get": {
                    "tags": ["documents"],
                    "summary": "Assigned tags",
                    "security": [{ "bearerAuth": [] }],
                    "parameters": [{ "$ref": "#/components/parameters/DocumentId" }],
                    "responses": {
                        "200": { "description": "`{tags: [...]}`.", "content": { "application/json": { "schema": { "type": "object" } } } },
                        "401": { "$ref": "#/components/responses/Unauthorized" },
                        "404": { "$ref": "#/components/responses/NotFound" },
                    },
                },
                "put": {
                    "tags": ["documents"],
                    "summary": "Assign a tag",
                    "security": [{ "bearerAuth": [] }],
                    "parameters": [{ "$ref": "#/components/parameters/DocumentId" }],
                    "requestBody": {
                        "required": true,
                        "content": { "application/json": { "schema": { "type": "object", "required": ["name"], "properties": { "name": { "type": "string" } } } } },
                    },
                    "responses": {
                        "201": { "description": "`{tag}`.", "content": { "application/json": { "schema": { "type": "object" } } } },
                        "400": { "$ref": "#/components/responses/BadRequest" },
                        "401": { "$ref": "#/components/responses/Unauthorized" },
                        "404": { "$ref": "#/components/responses/NotFound" },
                    },
                },
            },
            "/documents/{id}/tags/{name}": {
                "delete": {
                    "tags": ["documents"],
                    "summary": "Remove a tag assignment",
                    "security": [{ "bearerAuth": [] }],
                    "parameters": [
                        { "$ref": "#/components/parameters/DocumentId" },
                        { "name": "name", "in": "path", "required": true, "schema": { "type": "string" } },
                    ],
                    "responses": {
                        "200": { "description": "`{removed: bool}`.", "content": { "application/json": { "schema": { "type": "object" } } } },
                        "401": { "$ref": "#/components/responses/Unauthorized" },
                    },
                },
            },
            "/documents/{id}/history": {
                "get": {
                    "tags": ["documents"],
                    "summary": "Audit event history",
                    "security": [{ "bearerAuth": [] }],
                    "parameters": [{ "$ref": "#/components/parameters/DocumentId" }],
                    "responses": {
                        "200": { "description": "`{events: [...]}` oldest first.", "content": { "application/json": { "schema": { "type": "object" } } } },
                        "401": { "$ref": "#/components/responses/Unauthorized" },
                        "404": { "$ref": "#/components/responses/NotFound" },
                    },
                },
            },
            "/documents/{id}/artifacts/{artifact_id}/download": {
                "get": {
                    "tags": ["documents"],
                    "summary": "Download artifact bytes",
                    "security": [{ "bearerAuth": [] }],
                    "parameters": [
                        { "$ref": "#/components/parameters/DocumentId" },
                        { "name": "artifact_id", "in": "path", "required": true, "schema": { "type": "string" } },
                    ],
                    "responses": {
                        "200": { "description": "Artifact bytes with Content-Type/Disposition and optional `x-checksum-*` header.", "content": { "application/octet-stream": { "schema": { "type": "string", "format": "binary" } } } },
                        "401": { "$ref": "#/components/responses/Unauthorized" },
                        "404": { "$ref": "#/components/responses/NotFound" },
                        "413": { "description": "Artifact exceeds the download cap (256 MiB).", "content": { "application/json": { "schema": { "$ref": "#/components/schemas/Error" } } } },
                    },
                },
            },
            "/documents/{id}/process": {
                "post": {
                    "tags": ["documents"],
                    "summary": "Enqueue a processing job",
                    "description": "`op` is `metadata_extract`, `index`, `thumbnail`, or any pdf-engine op (`page_count`, `extract_text`, `render`, `ocr`, `compress`, `rotate`, `split`, `watermark`, `encrypt`, ... — see `PDF_OPS` in job_handlers). `params` is forwarded verbatim.",
                    "security": [{ "bearerAuth": [] }],
                    "parameters": [{ "$ref": "#/components/parameters/DocumentId" }],
                    "requestBody": {
                        "required": true,
                        "content": { "application/json": { "schema": {
                            "type": "object",
                            "required": ["op"],
                            "properties": {
                                "op": { "type": "string" },
                                "artifact_id": { "type": "string", "description": "Absent = most recent artifact." },
                                "params": { "type": "object" },
                            },
                        } } },
                    },
                    "responses": {
                        "202": { "description": "`{job}` queued.", "content": { "application/json": { "schema": { "type": "object", "properties": { "job": { "$ref": "#/components/schemas/Job" } } } } } },
                        "400": { "$ref": "#/components/responses/BadRequest" },
                        "401": { "$ref": "#/components/responses/Unauthorized" },
                        "404": { "$ref": "#/components/responses/NotFound" },
                    },
                },
            },
            "/pipelines": {
                "get": {
                    "tags": ["pipelines"],
                    "summary": "List built-in pipeline definitions",
                    "description": "Returns every `pipeline_engine::builtin_definitions()` entry (`pdf.intake`, `pdf.extract-text`, …) as full PipelineDefinition JSON — the same shape `POST /pipelines/run` accepts.",
                    "security": [{ "bearerAuth": [] }],
                    "responses": {
                        "200": { "description": "`{pipelines: [PipelineDefinition]}`.", "content": { "application/json": { "schema": { "type": "object", "properties": { "pipelines": { "type": "array", "items": { "$ref": "#/components/schemas/PipelineDefinition" } } } } } } },
                        "401": { "$ref": "#/components/responses/Unauthorized" },
                    },
                },
            },
            "/pipelines/{name}": {
                "get": {
                    "tags": ["pipelines"],
                    "summary": "One built-in pipeline definition",
                    "security": [{ "bearerAuth": [] }],
                    "parameters": [{ "name": "name", "in": "path", "required": true, "schema": { "type": "string" }, "description": "Built-in pipeline name, e.g. `pdf.intake`." }],
                    "responses": {
                        "200": { "description": "`{pipeline: PipelineDefinition}`.", "content": { "application/json": { "schema": { "type": "object", "properties": { "pipeline": { "$ref": "#/components/schemas/PipelineDefinition" } } } } } },
                        "401": { "$ref": "#/components/responses/Unauthorized" },
                        "404": { "$ref": "#/components/responses/NotFound" },
                    },
                },
            },
            "/pipelines/run": {
                "post": {
                    "tags": ["pipelines"],
                    "summary": "Enqueue a pipeline.run job",
                    "description": "Runs a `pipeline_engine` definition over the document's artifact. `pipeline` is either `{\"name\": \"<builtin>\"}` (`pdf.intake`, `pdf.extract-text`) or a full PipelineDefinition (`name` + `steps`; a `steps` key selects definition mode). Byte outputs become artifacts under `documents/{id}/outputs/`; step reports land in document metadata.",
                    "security": [{ "bearerAuth": [] }],
                    "requestBody": {
                        "required": true,
                        "content": { "application/json": { "schema": {
                            "type": "object",
                            "required": ["document_id", "pipeline"],
                            "properties": {
                                "document_id": { "type": "string" },
                                "artifact_id": { "type": "string", "description": "Absent = most recent artifact." },
                                "pipeline": {
                                    "description": "`{\"name\": \"<builtin>\"}` or a full PipelineDefinition.",
                                    "oneOf": [
                                        { "type": "object", "required": ["name"], "properties": { "name": { "type": "string" } } },
                                        { "$ref": "#/components/schemas/PipelineDefinition" },
                                    ],
                                },
                            },
                        } } },
                    },
                    "responses": {
                        "202": { "description": "`{job}` queued (kind `pipeline.run`).", "content": { "application/json": { "schema": { "type": "object", "properties": { "job": { "$ref": "#/components/schemas/Job" } } } } } },
                        "400": { "$ref": "#/components/responses/BadRequest" },
                        "401": { "$ref": "#/components/responses/Unauthorized" },
                        "404": { "$ref": "#/components/responses/NotFound" },
                    },
                },
            },
            "/folders": {
                "get": {
                    "tags": ["folders"],
                    "summary": "List folders",
                    "security": [{ "bearerAuth": [] }],
                    "parameters": [
                        { "name": "owner_id", "in": "query", "schema": { "type": "string" }, "description": "Defaults to the caller." },
                        { "name": "parent_id", "in": "query", "schema": { "type": "string" }, "description": "Children of this folder; absent = root level." },
                    ],
                    "responses": {
                        "200": { "description": "`{folders: [...]}`.", "content": { "application/json": { "schema": { "type": "object" } } } },
                        "401": { "$ref": "#/components/responses/Unauthorized" },
                    },
                },
                "post": {
                    "tags": ["folders"],
                    "summary": "Create folder",
                    "security": [{ "bearerAuth": [] }],
                    "requestBody": {
                        "required": true,
                        "content": { "application/json": { "schema": {
                            "type": "object",
                            "required": ["name"],
                            "properties": {
                                "name": { "type": "string" },
                                "parent_id": { "type": "string" },
                            },
                        } } },
                    },
                    "responses": {
                        "201": { "description": "`{folder}`.", "content": { "application/json": { "schema": { "type": "object" } } } },
                        "400": { "$ref": "#/components/responses/BadRequest" },
                        "401": { "$ref": "#/components/responses/Unauthorized" },
                    },
                },
            },
            "/documents/{id}/copy": {
                "post": {
                    "tags": ["documents"],
                    "summary": "Duplicate a document",
                    "description": "Requires `read` on the source. The copy keeps the source's `owner_id` (store semantics) and shares storage bytes.",
                    "security": [{ "bearerAuth": [] }],
                    "parameters": [{ "$ref": "#/components/parameters/DocumentId" }],
                    "requestBody": {
                        "required": false,
                        "content": { "application/json": { "schema": {
                            "type": "object",
                            "properties": { "name": { "type": "string" } },
                        } } },
                    },
                    "responses": {
                        "201": { "description": "`{document}`.", "content": { "application/json": { "schema": { "type": "object" } } } },
                        "400": { "$ref": "#/components/responses/BadRequest" },
                        "401": { "$ref": "#/components/responses/Unauthorized" },
                        "403": { "$ref": "#/components/responses/Forbidden" },
                        "404": { "$ref": "#/components/responses/NotFound" },
                    },
                },
            },
            "/documents/{id}/archive": {
                "post": {
                    "tags": ["documents"],
                    "summary": "Archive a document",
                    "security": [{ "bearerAuth": [] }],
                    "parameters": [{ "$ref": "#/components/parameters/DocumentId" }],
                    "responses": {
                        "200": { "description": "`{document}` (archived).", "content": { "application/json": { "schema": { "type": "object" } } } },
                        "400": { "$ref": "#/components/responses/BadRequest" },
                        "401": { "$ref": "#/components/responses/Unauthorized" },
                        "403": { "$ref": "#/components/responses/Forbidden" },
                        "404": { "$ref": "#/components/responses/NotFound" },
                    },
                },
            },
            "/documents/{id}/unarchive": {
                "post": {
                    "tags": ["documents"],
                    "summary": "Unarchive a document",
                    "security": [{ "bearerAuth": [] }],
                    "parameters": [{ "$ref": "#/components/parameters/DocumentId" }],
                    "responses": {
                        "200": { "description": "`{document}` (active).", "content": { "application/json": { "schema": { "type": "object" } } } },
                        "400": { "$ref": "#/components/responses/BadRequest" },
                        "401": { "$ref": "#/components/responses/Unauthorized" },
                        "403": { "$ref": "#/components/responses/Forbidden" },
                        "404": { "$ref": "#/components/responses/NotFound" },
                    },
                },
            },
            "/documents/{id}/purge": {
                "post": {
                    "tags": ["documents"],
                    "summary": "Permanently delete a trashed document",
                    "description": "Irreversible. Requires `manage` access and a trashed document.",
                    "security": [{ "bearerAuth": [] }],
                    "parameters": [{ "$ref": "#/components/parameters/DocumentId" }],
                    "responses": {
                        "200": { "description": "`{purged: bool}`.", "content": { "application/json": { "schema": { "type": "object" } } } },
                        "400": { "$ref": "#/components/responses/BadRequest" },
                        "401": { "$ref": "#/components/responses/Unauthorized" },
                        "403": { "$ref": "#/components/responses/Forbidden" },
                        "404": { "$ref": "#/components/responses/NotFound" },
                    },
                },
            },
            "/documents/{id}/permissions": {
                "get": {
                    "tags": ["documents"],
                    "summary": "List permission grants on a document",
                    "security": [{ "bearerAuth": [] }],
                    "parameters": [{ "$ref": "#/components/parameters/DocumentId" }],
                    "responses": {
                        "200": { "description": "`{permissions: [...]}`.", "content": { "application/json": { "schema": { "type": "object" } } } },
                        "401": { "$ref": "#/components/responses/Unauthorized" },
                        "403": { "$ref": "#/components/responses/Forbidden" },
                        "404": { "$ref": "#/components/responses/NotFound" },
                    },
                },
                "post": {
                    "tags": ["documents"],
                    "summary": "Grant or update a subject's access level",
                    "description": "Requires `manage` access. `access_level` is `read`, `write`, or `manage`.",
                    "security": [{ "bearerAuth": [] }],
                    "parameters": [{ "$ref": "#/components/parameters/DocumentId" }],
                    "requestBody": {
                        "required": true,
                        "content": { "application/json": { "schema": {
                            "type": "object",
                            "required": ["subject_id", "access_level"],
                            "properties": {
                                "subject_id": { "type": "string", "description": "User id, `apikey:<name>`, group id, or role id." },
                                "access_level": { "type": "string", "enum": ["read", "write", "manage"] },
                            },
                        } } },
                    },
                    "responses": {
                        "201": { "description": "`{permission}`.", "content": { "application/json": { "schema": { "type": "object" } } } },
                        "400": { "$ref": "#/components/responses/BadRequest" },
                        "401": { "$ref": "#/components/responses/Unauthorized" },
                        "403": { "$ref": "#/components/responses/Forbidden" },
                        "404": { "$ref": "#/components/responses/NotFound" },
                    },
                },
            },
            "/documents/{id}/permissions/{subject}": {
                "delete": {
                    "tags": ["documents"],
                    "summary": "Revoke a subject's permission",
                    "security": [{ "bearerAuth": [] }],
                    "parameters": [
                        { "$ref": "#/components/parameters/DocumentId" },
                        { "name": "subject", "in": "path", "required": true, "schema": { "type": "string" } },
                    ],
                    "responses": {
                        "200": { "description": "`{revoked: bool}`.", "content": { "application/json": { "schema": { "type": "object" } } } },
                        "401": { "$ref": "#/components/responses/Unauthorized" },
                        "403": { "$ref": "#/components/responses/Forbidden" },
                        "404": { "$ref": "#/components/responses/NotFound" },
                    },
                },
            },
            "/documents/{id}/permissions/subject/{subject}": {
                "get": {
                    "tags": ["documents"],
                    "summary": "Grants held by a subject across all documents",
                    "description": "Self-service: any caller may query their own actor id; querying another subject requires `manage` on the path document.",
                    "security": [{ "bearerAuth": [] }],
                    "parameters": [
                        { "$ref": "#/components/parameters/DocumentId" },
                        { "name": "subject", "in": "path", "required": true, "schema": { "type": "string" } },
                    ],
                    "responses": {
                        "200": { "description": "`{subject_id, permissions: [...]}`.", "content": { "application/json": { "schema": { "type": "object" } } } },
                        "401": { "$ref": "#/components/responses/Unauthorized" },
                        "403": { "$ref": "#/components/responses/Forbidden" },
                        "404": { "$ref": "#/components/responses/NotFound" },
                    },
                },
            },
            "/documents/{id}/relationships": {
                "get": {
                    "tags": ["documents"],
                    "summary": "List relationships (outgoing + incoming)",
                    "security": [{ "bearerAuth": [] }],
                    "parameters": [{ "$ref": "#/components/parameters/DocumentId" }],
                    "responses": {
                        "200": { "description": "`{outgoing: [...], incoming: [...]}`.", "content": { "application/json": { "schema": { "type": "object" } } } },
                        "401": { "$ref": "#/components/responses/Unauthorized" },
                        "403": { "$ref": "#/components/responses/Forbidden" },
                        "404": { "$ref": "#/components/responses/NotFound" },
                    },
                },
                "post": {
                    "tags": ["documents"],
                    "summary": "Create a relationship to another document",
                    "description": "Requires `write` on the source and `read` on the target.",
                    "security": [{ "bearerAuth": [] }],
                    "parameters": [{ "$ref": "#/components/parameters/DocumentId" }],
                    "requestBody": {
                        "required": true,
                        "content": { "application/json": { "schema": {
                            "type": "object",
                            "required": ["target_document_id"],
                            "properties": {
                                "target_document_id": { "type": "string" },
                                "relationship_type": { "type": "string", "enum": ["parent", "child", "related", "attachment", "duplicate", "derived_from", "supersedes", "other"], "default": "related" },
                            },
                        } } },
                    },
                    "responses": {
                        "201": { "description": "`{relationship}`.", "content": { "application/json": { "schema": { "type": "object" } } } },
                        "400": { "$ref": "#/components/responses/BadRequest" },
                        "401": { "$ref": "#/components/responses/Unauthorized" },
                        "403": { "$ref": "#/components/responses/Forbidden" },
                        "404": { "$ref": "#/components/responses/NotFound" },
                    },
                },
            },
            "/documents/{id}/relationships/{rel_id}": {
                "delete": {
                    "tags": ["documents"],
                    "summary": "Delete a relationship touching this document",
                    "security": [{ "bearerAuth": [] }],
                    "parameters": [
                        { "$ref": "#/components/parameters/DocumentId" },
                        { "name": "rel_id", "in": "path", "required": true, "schema": { "type": "string" } },
                    ],
                    "responses": {
                        "200": { "description": "`{deleted: bool}`.", "content": { "application/json": { "schema": { "type": "object" } } } },
                        "401": { "$ref": "#/components/responses/Unauthorized" },
                        "403": { "$ref": "#/components/responses/Forbidden" },
                        "404": { "$ref": "#/components/responses/NotFound" },
                    },
                },
            },
            "/documents/{id}/related": {
                "get": {
                    "tags": ["documents"],
                    "summary": "Related documents (both directions, deduplicated)",
                    "security": [{ "bearerAuth": [] }],
                    "parameters": [{ "$ref": "#/components/parameters/DocumentId" }],
                    "responses": {
                        "200": { "description": "`{related: [{document, relationship_type, outgoing}]}`.", "content": { "application/json": { "schema": { "type": "object" } } } },
                        "401": { "$ref": "#/components/responses/Unauthorized" },
                        "403": { "$ref": "#/components/responses/Forbidden" },
                        "404": { "$ref": "#/components/responses/NotFound" },
                    },
                },
            },
            "/folders/{id}": {
                "patch": {
                    "tags": ["folders"],
                    "summary": "Rename / reparent a folder",
                    "description": "Owner-only; `parent_id` absent = unchanged, null = root.",
                    "security": [{ "bearerAuth": [] }],
                    "parameters": [{ "name": "id", "in": "path", "required": true, "schema": { "type": "string" }, "description": "Folder id." }],
                    "requestBody": {
                        "required": true,
                        "content": { "application/json": { "schema": {
                            "type": "object",
                            "properties": {
                                "name": { "type": "string" },
                                "parent_id": { "type": ["string", "null"] },
                            },
                        } } },
                    },
                    "responses": {
                        "200": { "description": "`{folder}`.", "content": { "application/json": { "schema": { "type": "object" } } } },
                        "400": { "$ref": "#/components/responses/BadRequest" },
                        "401": { "$ref": "#/components/responses/Unauthorized" },
                        "404": { "$ref": "#/components/responses/NotFound" },
                    },
                },
                "delete": {
                    "tags": ["folders"],
                    "summary": "Delete a folder (documents move to root)",
                    "security": [{ "bearerAuth": [] }],
                    "parameters": [{ "name": "id", "in": "path", "required": true, "schema": { "type": "string" } }],
                    "responses": {
                        "200": { "description": "`{deleted: bool}`.", "content": { "application/json": { "schema": { "type": "object" } } } },
                        "401": { "$ref": "#/components/responses/Unauthorized" },
                        "404": { "$ref": "#/components/responses/NotFound" },
                    },
                },
            },
            "/documents/{id}/settings": {
                "get": {
                    "tags": ["documents"],
                    "summary": "Per-document settings blob + is_locked",
                    "security": [{ "bearerAuth": [] }],
                    "parameters": [{ "$ref": "#/components/parameters/DocumentId" }],
                    "responses": {
                        "200": { "description": "`{settings}` (null when unset).", "content": { "application/json": { "schema": { "type": "object" } } } },
                        "401": { "$ref": "#/components/responses/Unauthorized" },
                        "403": { "$ref": "#/components/responses/Forbidden" },
                        "404": { "$ref": "#/components/responses/NotFound" },
                    },
                },
                "patch": {
                    "tags": ["documents"],
                    "summary": "Merge settings keys / set is_locked",
                    "security": [{ "bearerAuth": [] }],
                    "parameters": [{ "$ref": "#/components/parameters/DocumentId" }],
                    "requestBody": {
                        "required": true,
                        "content": { "application/json": { "schema": {
                            "type": "object",
                            "properties": {
                                "settings": { "type": "object" },
                                "is_locked": { "type": "boolean" },
                            },
                        } } },
                    },
                    "responses": {
                        "200": { "description": "`{settings}`.", "content": { "application/json": { "schema": { "type": "object" } } } },
                        "400": { "$ref": "#/components/responses/BadRequest" },
                        "401": { "$ref": "#/components/responses/Unauthorized" },
                        "403": { "$ref": "#/components/responses/Forbidden" },
                        "404": { "$ref": "#/components/responses/NotFound" },
                    },
                },
                "delete": {
                    "tags": ["documents"],
                    "summary": "Delete the settings row",
                    "security": [{ "bearerAuth": [] }],
                    "parameters": [{ "$ref": "#/components/parameters/DocumentId" }],
                    "responses": {
                        "200": { "description": "`{deleted: bool}`.", "content": { "application/json": { "schema": { "type": "object" } } } },
                        "401": { "$ref": "#/components/responses/Unauthorized" },
                        "403": { "$ref": "#/components/responses/Forbidden" },
                        "404": { "$ref": "#/components/responses/NotFound" },
                    },
                },
            },
            "/documents/{id}/metadata": {
                "get": {
                    "tags": ["documents"],
                    "summary": "Descriptive metadata row",
                    "security": [{ "bearerAuth": [] }],
                    "parameters": [{ "$ref": "#/components/parameters/DocumentId" }],
                    "responses": {
                        "200": { "description": "`{metadata}` (null when unset).", "content": { "application/json": { "schema": { "type": "object" } } } },
                        "401": { "$ref": "#/components/responses/Unauthorized" },
                        "403": { "$ref": "#/components/responses/Forbidden" },
                        "404": { "$ref": "#/components/responses/NotFound" },
                    },
                },
                "patch": {
                    "tags": ["documents"],
                    "summary": "Partial metadata update (tri-state fields)",
                    "security": [{ "bearerAuth": [] }],
                    "parameters": [{ "$ref": "#/components/parameters/DocumentId" }],
                    "requestBody": {
                        "required": true,
                        "content": { "application/json": { "schema": { "type": "object" } } },
                    },
                    "responses": {
                        "200": { "description": "`{metadata}`.", "content": { "application/json": { "schema": { "type": "object" } } } },
                        "400": { "$ref": "#/components/responses/BadRequest" },
                        "401": { "$ref": "#/components/responses/Unauthorized" },
                        "403": { "$ref": "#/components/responses/Forbidden" },
                        "404": { "$ref": "#/components/responses/NotFound" },
                    },
                },
                "delete": {
                    "tags": ["documents"],
                    "summary": "Delete the metadata row",
                    "security": [{ "bearerAuth": [] }],
                    "parameters": [{ "$ref": "#/components/parameters/DocumentId" }],
                    "responses": {
                        "200": { "description": "`{deleted: bool}`.", "content": { "application/json": { "schema": { "type": "object" } } } },
                        "401": { "$ref": "#/components/responses/Unauthorized" },
                        "403": { "$ref": "#/components/responses/Forbidden" },
                        "404": { "$ref": "#/components/responses/NotFound" },
                    },
                },
            },
            "/documents/{id}/metadata/fields/{key}": {
                "put": {
                    "tags": ["documents"],
                    "summary": "Set one metadata custom field",
                    "security": [{ "bearerAuth": [] }],
                    "parameters": [
                        { "$ref": "#/components/parameters/DocumentId" },
                        { "name": "key", "in": "path", "required": true, "schema": { "type": "string" } },
                    ],
                    "requestBody": {
                        "required": true,
                        "content": { "application/json": { "schema": {
                            "type": "object", "required": ["value"],
                            "properties": { "value": {} },
                        } } },
                    },
                    "responses": {
                        "200": { "description": "`{metadata}`.", "content": { "application/json": { "schema": { "type": "object" } } } },
                        "400": { "$ref": "#/components/responses/BadRequest" },
                        "401": { "$ref": "#/components/responses/Unauthorized" },
                        "403": { "$ref": "#/components/responses/Forbidden" },
                        "404": { "$ref": "#/components/responses/NotFound" },
                    },
                },
                "delete": {
                    "tags": ["documents"],
                    "summary": "Remove one metadata custom field",
                    "security": [{ "bearerAuth": [] }],
                    "parameters": [
                        { "$ref": "#/components/parameters/DocumentId" },
                        { "name": "key", "in": "path", "required": true, "schema": { "type": "string" } },
                    ],
                    "responses": {
                        "200": { "description": "`{removed: bool}`.", "content": { "application/json": { "schema": { "type": "object" } } } },
                        "401": { "$ref": "#/components/responses/Unauthorized" },
                        "403": { "$ref": "#/components/responses/Forbidden" },
                        "404": { "$ref": "#/components/responses/NotFound" },
                    },
                },
            },
            "/documents/{id}/ops/{op}": {
                "post": {
                    "tags": ["documents"],
                    "summary": "Run a generic engine op",
                    "description": "`image.*` runs synchronously (output → new artifact); `pdf.*`/bare pdf ops and `pipeline.run` enqueue jobs. Requires `write` access.",
                    "security": [{ "bearerAuth": [] }],
                    "parameters": [
                        { "$ref": "#/components/parameters/DocumentId" },
                        { "name": "op", "in": "path", "required": true, "schema": { "type": "string" } },
                    ],
                    "requestBody": {
                        "required": true,
                        "content": { "application/json": { "schema": {
                            "type": "object",
                            "properties": {
                                "artifact_id": { "type": "string" },
                                "params": { "type": "object" },
                            },
                        } } },
                    },
                    "responses": {
                        "200": { "description": "Op result or `{artifact}` for byte outputs.", "content": { "application/json": { "schema": { "type": "object" } } } },
                        "202": { "description": "`{job}` queued.", "content": { "application/json": { "schema": { "type": "object" } } } },
                        "400": { "$ref": "#/components/responses/BadRequest" },
                        "401": { "$ref": "#/components/responses/Unauthorized" },
                        "403": { "$ref": "#/components/responses/Forbidden" },
                        "404": { "$ref": "#/components/responses/NotFound" },
                    },
                },
            },
            "/schedules": {
                "get": {
                    "tags": ["schedules"],
                    "summary": "List recurring schedules",
                    "description": "Schedules are global (not owner-scoped); any authenticated caller may read them.",
                    "security": [{ "bearerAuth": [] }],
                    "responses": {
                        "200": { "description": "`{schedules: [...]}` ordered by name.", "content": { "application/json": { "schema": { "type": "object" } } } },
                        "401": { "$ref": "#/components/responses/Unauthorized" },
                    },
                },
                "post": {
                    "tags": ["schedules"],
                    "summary": "Create or replace a schedule (upsert by name)",
                    "security": [{ "bearerAuth": [] }],
                    "requestBody": {
                        "required": true,
                        "content": { "application/json": { "schema": {
                            "type": "object",
                            "required": ["name", "kind", "interval_secs"],
                            "properties": {
                                "name": { "type": "string" },
                                "kind": { "type": "string", "description": "Job kind enqueued per occurrence." },
                                "payload": { "type": "object", "default": {} },
                                "interval_secs": { "type": "integer", "minimum": 1 },
                                "priority": { "type": "integer", "default": 0 },
                                "next_run_at": { "type": "integer", "description": "Unix seconds; default now + interval." },
                                "enabled": { "type": "boolean", "default": true },
                            },
                        } } },
                    },
                    "responses": {
                        "200": { "description": "`{schedule}`.", "content": { "application/json": { "schema": { "type": "object" } } } },
                        "400": { "$ref": "#/components/responses/BadRequest" },
                        "401": { "$ref": "#/components/responses/Unauthorized" },
                    },
                },
            },
            "/schedules/{name}": {
                "delete": {
                    "tags": ["schedules"],
                    "summary": "Delete a schedule by name",
                    "security": [{ "bearerAuth": [] }],
                    "parameters": [{ "name": "name", "in": "path", "required": true, "schema": { "type": "string" } }],
                    "responses": {
                        "200": { "description": "`{deleted: true}`.", "content": { "application/json": { "schema": { "type": "object" } } } },
                        "401": { "$ref": "#/components/responses/Unauthorized" },
                        "404": { "$ref": "#/components/responses/NotFound" },
                    },
                },
            },
            "/schedules/tick": {
                "post": {
                    "tags": ["schedules"],
                    "summary": "Run one scheduler tick now",
                    "description": "Enqueues a job for every due schedule — same routine as the engine-driven `scheduler.tick` job.",
                    "security": [{ "bearerAuth": [] }],
                    "requestBody": {
                        "required": false,
                        "content": { "application/json": { "schema": {
                            "type": "object",
                            "properties": { "now": { "type": "integer", "description": "Unix seconds; default now." } },
                        } } },
                    },
                    "responses": {
                        "200": { "description": "`{enqueued}` — jobs enqueued this tick.", "content": { "application/json": { "schema": { "type": "object" } } } },
                        "400": { "$ref": "#/components/responses/BadRequest" },
                        "401": { "$ref": "#/components/responses/Unauthorized" },
                    },
                },
            },
            "/jobs": {
                "get": {
                    "tags": ["jobs"],
                    "summary": "List jobs (`?status=` filter)",
                    "security": [{ "bearerAuth": [] }],
                    "parameters": [
                        { "name": "status", "in": "query", "schema": { "type": "string", "enum": ["pending", "running", "succeeded", "failed", "cancelled", "skipped"] } },
                        { "name": "limit", "in": "query", "schema": { "type": "integer", "default": 100, "maximum": 1000 } },
                    ],
                    "responses": {
                        "200": { "description": "`{jobs: [Job]}` newest first.", "content": { "application/json": { "schema": { "type": "object", "properties": { "jobs": { "type": "array", "items": { "$ref": "#/components/schemas/Job" } } } } } } },
                        "400": { "$ref": "#/components/responses/BadRequest" },
                        "401": { "$ref": "#/components/responses/Unauthorized" },
                    },
                },
            },
            "/jobs/{id}": {
                "get": {
                    "tags": ["jobs"],
                    "summary": "Job status from the durable queue",
                    "security": [{ "bearerAuth": [] }],
                    "parameters": [{ "name": "id", "in": "path", "required": true, "schema": { "type": "integer", "format": "int64" } }],
                    "responses": {
                        "200": { "description": "Job object.", "content": { "application/json": { "schema": { "$ref": "#/components/schemas/Job" } } } },
                        "400": { "$ref": "#/components/responses/BadRequest" },
                        "401": { "$ref": "#/components/responses/Unauthorized" },
                        "404": { "$ref": "#/components/responses/NotFound" },
                    },
                },
            },
            "/jobs/{id}/cancel": {
                "post": {
                    "tags": ["jobs"],
                    "summary": "Cancel a pending/running job",
                    "security": [{ "bearerAuth": [] }],
                    "parameters": [{ "name": "id", "in": "path", "required": true, "schema": { "type": "integer", "format": "int64" } }],
                    "responses": {
                        "200": { "description": "`{id, status: \"cancelled\"}`.", "content": { "application/json": { "schema": { "type": "object" } } } },
                        "400": { "$ref": "#/components/responses/BadRequest" },
                        "401": { "$ref": "#/components/responses/Unauthorized" },
                        "409": { "$ref": "#/components/responses/Conflict" },
                    },
                },
            },
            "/jobs/{id}/retry": {
                "post": {
                    "tags": ["jobs"],
                    "summary": "Requeue a failed job",
                    "security": [{ "bearerAuth": [] }],
                    "parameters": [{ "name": "id", "in": "path", "required": true, "schema": { "type": "integer", "format": "int64" } }],
                    "responses": {
                        "200": { "description": "`{id, status: \"pending\"}`.", "content": { "application/json": { "schema": { "type": "object" } } } },
                        "400": { "$ref": "#/components/responses/BadRequest" },
                        "401": { "$ref": "#/components/responses/Unauthorized" },
                        "409": { "$ref": "#/components/responses/Conflict" },
                    },
                },
            },
            "/jobs/batch": {
                "post": {
                    "tags": ["jobs"],
                    "summary": "Enqueue a batch of jobs (max 200)",
                    "description": "Body is a JSON array of `{kind, payload, priority?, run_at?, max_attempts?}`. All items share a `batch_id` (the first job's id) for progress aggregation and batch cancel.",
                    "security": [{ "bearerAuth": [] }],
                    "requestBody": {
                        "required": true,
                        "content": { "application/json": { "schema": {
                            "type": "array",
                            "minItems": 1,
                            "maxItems": 200,
                            "items": {
                                "type": "object",
                                "required": ["kind"],
                                "properties": {
                                    "kind": { "type": "string" },
                                    "payload": { "type": "object", "default": {} },
                                    "priority": { "type": "integer", "default": 0 },
                                    "run_at": { "type": "integer", "description": "Unix seconds." },
                                    "max_attempts": { "type": "integer" },
                                },
                            },
                        } } },
                    },
                    "responses": {
                        "201": { "description": "`{batch_id, jobs: [Job]}`.", "content": { "application/json": { "schema": { "type": "object" } } } },
                        "400": { "$ref": "#/components/responses/BadRequest" },
                        "401": { "$ref": "#/components/responses/Unauthorized" },
                    },
                },
            },
            "/jobs/batch/{id}": {
                "get": {
                    "tags": ["jobs"],
                    "summary": "Batch progress + member jobs",
                    "security": [{ "bearerAuth": [] }],
                    "parameters": [{ "name": "id", "in": "path", "required": true, "schema": { "type": "integer", "format": "int64" }, "description": "batch_id (first job's id)." }],
                    "responses": {
                        "200": { "description": "`{batch_id, progress, jobs}`.", "content": { "application/json": { "schema": { "type": "object", "properties": {
                            "batch_id": { "type": "integer" },
                            "progress": { "$ref": "#/components/schemas/BatchProgress" },
                            "jobs": { "type": "array", "items": { "$ref": "#/components/schemas/Job" } },
                        } } } } },
                        "400": { "$ref": "#/components/responses/BadRequest" },
                        "401": { "$ref": "#/components/responses/Unauthorized" },
                        "404": { "$ref": "#/components/responses/NotFound" },
                    },
                },
            },
            "/jobs/batch/{id}/cancel": {
                "post": {
                    "tags": ["jobs"],
                    "summary": "Cancel all non-terminal jobs in a batch",
                    "security": [{ "bearerAuth": [] }],
                    "parameters": [{ "name": "id", "in": "path", "required": true, "schema": { "type": "integer", "format": "int64" } }],
                    "responses": {
                        "200": { "description": "`{batch_id, cancelled}` — number of jobs cancelled.", "content": { "application/json": { "schema": { "type": "object" } } } },
                        "400": { "$ref": "#/components/responses/BadRequest" },
                        "401": { "$ref": "#/components/responses/Unauthorized" },
                        "404": { "$ref": "#/components/responses/NotFound" },
                    },
                },
            },
        },
    })
}
