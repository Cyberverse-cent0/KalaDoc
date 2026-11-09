//! HTTP request/response DTOs.
//!
//! These types are the API's wire contract — serde shapes only, no
//! behavior beyond input validation that exists purely to produce a
//! well-formed [`NewJob`](crate::crates::job_engine::NewJob) payload.
//! Domain rules (is this document real? is this file name safe?)
//! stay in the services and job handlers.

use base64::{Engine as _, engine::general_purpose::STANDARD as B64};
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};

use crate::crates::job_engine::{Job, JobStatus};

use super::error::ApiError;

/// `POST /documents` request body.
///
/// Mirrors the `document.ingest` job payload contract documented in
/// `application::job_handlers`: `document_id` is required and must
/// reference an existing document; **exactly one** of
/// `content_b64` (base64-encoded bytes) or `content` (UTF-8 text)
/// carries the file bytes. `file_name`/`mime_type` are optional and
/// forwarded verbatim — the job handler applies its own sanitizing
/// and defaults (`upload.bin` when absent).
#[derive(Debug, Clone, Deserialize)]
pub struct CreateDocumentRequest {
    /// Existing document the ingested bytes attach to. `Option` so a
    /// missing field surfaces as our 400 rather than the extractor's
    /// generic 422.
    #[serde(default)]
    pub document_id: Option<String>,
    /// Base64-encoded file bytes. Mutually exclusive with `content`.
    #[serde(default)]
    pub content_b64: Option<String>,
    /// UTF-8 text stored as the file bytes. Mutually exclusive with
    /// `content_b64`.
    #[serde(default)]
    pub content: Option<String>,
    /// Suggested file name (handler default: `upload.bin`).
    #[serde(default)]
    pub file_name: Option<String>,
    /// Caller-declared MIME type, recorded on the artifact.
    #[serde(default)]
    pub mime_type: Option<String>,
}

impl CreateDocumentRequest {
    /// Validates the request and returns `(document_id, job_payload)`.
    ///
    /// # Errors
    /// [`ApiError::BadRequest`] when `document_id` is empty, neither
    /// or both content fields are set, `content`/`content_b64` are
    /// empty, or `content_b64` is not valid base64.
    pub fn into_job_payload(self) -> Result<(String, Value), ApiError> {
        let document_id = self.document_id.as_deref().unwrap_or("").trim();
        if document_id.is_empty() {
            return Err(ApiError::BadRequest(
                "document_id must be a non-empty string".into(),
            ));
        }

        // Exactly one content channel — same rule the job handler
        // enforces, checked here so the client gets a 400 instead of
        // a job that fails asynchronously.
        match (&self.content_b64, &self.content) {
            (Some(b64), None) => {
                let b64 = b64.trim();
                if b64.is_empty() {
                    return Err(ApiError::BadRequest("content_b64 must not be empty".into()));
                }
                // Decode now so malformed base64 is a 400, not a
                // failed job. The handler re-decodes the canonical
                // re-encoding, which is cheap insurance against
                // non-canonical-but-valid input (whitespace etc.).
                B64.decode(b64).map_err(|e| {
                    ApiError::BadRequest(format!("content_b64 is not valid base64: {e}"))
                })?;
            }
            (None, Some(text)) => {
                if text.is_empty() {
                    return Err(ApiError::BadRequest("content must not be empty".into()));
                }
            }
            _ => {
                return Err(ApiError::BadRequest(
                    "exactly one of 'content_b64' or 'content' is required".into(),
                ));
            }
        }

        let mut payload = json!({ "document_id": document_id });
        if let Some(b64) = self.content_b64 {
            payload["content_b64"] = json!(b64.trim());
        }
        if let Some(text) = self.content {
            payload["content"] = json!(text);
        }
        if let Some(name) = self
            .file_name
            .map(|s| s.trim().to_string())
            .filter(|s| !s.is_empty())
        {
            payload["file_name"] = json!(name);
        }
        if let Some(mime) = self
            .mime_type
            .map(|s| s.trim().to_string())
            .filter(|s| !s.is_empty())
        {
            payload["mime_type"] = json!(mime);
        }

        Ok((document_id.to_string(), payload))
    }
}

/// `GET /jobs` query string.
#[derive(Debug, Clone, Default, Deserialize)]
pub struct JobsQuery {
    /// Filter by lifecycle status (`pending`, `running`,
    /// `succeeded`, `failed`, `cancelled`).
    #[serde(default)]
    pub status: Option<String>,
    /// Page size; defaults to 100, clamped to `[1, 1000]`.
    #[serde(default)]
    pub limit: Option<i64>,
}

/// Default page size for `GET /jobs`.
pub const DEFAULT_JOBS_LIMIT: i64 = 100;
/// Hard cap for `GET /jobs` page size.
pub const MAX_JOBS_LIMIT: i64 = 1000;

impl JobsQuery {
    /// Parses `status` into a [`JobStatus`] and normalizes `limit`.
    ///
    /// # Errors
    /// [`ApiError::BadRequest`] for an unrecognized status string.
    pub fn into_parts(self) -> Result<(Option<JobStatus>, i64), ApiError> {
        let status = match self.status.as_deref().map(str::trim) {
            Some(s) if !s.is_empty() => Some(JobStatus::parse(s).ok_or_else(|| {
                ApiError::BadRequest(format!(
                    "unknown job status {s:?}; expected one of: \
                     pending, running, succeeded, failed, cancelled"
                ))
            })?),
            _ => None,
        };
        let limit = self
            .limit
            .unwrap_or(DEFAULT_JOBS_LIMIT)
            .clamp(1, MAX_JOBS_LIMIT);
        Ok((status, limit))
    }
}

/// JSON projection of a [`Job`] row — the stable wire shape for
/// `POST /documents` (202 body) and the `GET /jobs*` endpoints.
///
/// Field names mirror `jobs_tbl` columns; `status` serializes as
/// snake_case (`pending`, `running`, ...) via [`JobStatus`]'s serde
/// impl, and `payload` is the raw handler JSON.
#[derive(Debug, Clone, Serialize)]
pub struct JobResponse {
    pub id: i64,
    /// Handler registry key, e.g. `"document.ingest"`.
    pub kind: String,
    /// The job payload exactly as stored.
    pub payload: Value,
    pub status: JobStatus,
    pub priority: i64,
    pub attempts: u32,
    pub max_attempts: u32,
    /// Unix seconds — earliest claimable time.
    pub run_at: i64,
    /// Unix seconds.
    pub created_at: i64,
    pub started_at: Option<i64>,
    pub finished_at: Option<i64>,
    pub last_error: Option<String>,
    pub worker_id: Option<String>,
}

impl From<Job> for JobResponse {
    fn from(job: Job) -> Self {
        Self {
            id: job.id,
            kind: job.kind,
            payload: job.payload,
            status: job.status,
            priority: job.priority,
            attempts: job.attempts,
            max_attempts: job.max_attempts,
            run_at: job.run_at,
            created_at: job.created_at,
            started_at: job.started_at,
            finished_at: job.finished_at,
            last_error: job.last_error,
            worker_id: job.worker_id,
        }
    }
}
