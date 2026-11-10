//! HTTP error type — the stable JSON error envelope.
//!
//! Every API error serializes as
//! `{"error": {"code", "message", "request_id", "retryable"}}`,
//! matching the CLI's spec §5.3 shape (`cli::report_error`). `code` is
//! a stable machine-readable string; `retryable` tells clients whether
//! repeating the request may succeed.

use axum::Json;
use axum::http::StatusCode;
use axum::response::{IntoResponse, Response};
use serde_json::json;

use crate::crates::application::services::ServiceError;
use crate::crates::document_engine::store::StoreError;
use crate::crates::image_engine::ImageError;
use crate::crates::job_engine::JobError;
use crate::crates::observability::RequestId;
use crate::crates::security_engine::SecurityError;
use crate::crates::storage::local::debug_when_full::StorageError;

/// An API error ready to become a response.
#[derive(Debug)]
pub struct ApiError {
    status: StatusCode,
    code: String,
    message: String,
    retryable: bool,
    request_id: RequestId,
}

impl ApiError {
    /// Generic constructor. `retryable` should be `true` only when the
    /// client may succeed by retrying the identical request.
    pub fn new(
        status: StatusCode,
        code: impl Into<String>,
        message: impl Into<String>,
        retryable: bool,
    ) -> Self {
        Self {
            status,
            code: code.into(),
            message: message.into(),
            retryable,
            request_id: RequestId::new(),
        }
    }

    /// `401` — missing, malformed, or invalid/expired Bearer token.
    pub fn unauthorized(message: impl Into<String>) -> Self {
        Self::new(StatusCode::UNAUTHORIZED, "unauthorized", message, false)
    }

    /// `400` — malformed request (bad id, invalid input).
    pub fn bad_request(message: impl Into<String>) -> Self {
        Self::new(StatusCode::BAD_REQUEST, "bad_request", message, false)
    }

    /// `403` — authenticated, but the caller lacks access to the
    /// addressed document/folder (see `services::access`).
    pub fn forbidden(message: impl Into<String>) -> Self {
        Self::new(StatusCode::FORBIDDEN, "forbidden", message, false)
    }

    /// `404` — the addressed resource does not exist.
    pub fn not_found(entity: &str, id: impl ToString) -> Self {
        Self::new(
            StatusCode::NOT_FOUND,
            "not_found",
            format!("{entity} not found: {}", id.to_string()),
            false,
        )
    }

    /// `413` — request body or upload exceeds a size limit.
    pub fn payload_too_large(message: impl Into<String>) -> Self {
        Self::new(
            StatusCode::PAYLOAD_TOO_LARGE,
            "payload_too_large",
            message,
            false,
        )
    }

    /// `500` — unexpected internal failure.
    pub fn internal(message: impl Into<String>) -> Self {
        Self::new(StatusCode::INTERNAL_SERVER_ERROR, "internal", message, true)
    }

    /// Replaces the generated request id (e.g. propagated from an
    /// incoming `x-request-id` header).
    pub fn with_request_id(mut self, id: RequestId) -> Self {
        self.request_id = id;
        self
    }
}

impl IntoResponse for ApiError {
    fn into_response(self) -> Response {
        (
            self.status,
            Json(json!({
                "error": {
                    "code": self.code,
                    "message": self.message,
                    "request_id": self.request_id,
                    "retryable": self.retryable,
                }
            })),
        )
            .into_response()
    }
}

impl From<ServiceError> for ApiError {
    fn from(e: ServiceError) -> Self {
        match e {
            ServiceError::Store(StoreError::NotFound { entity, id }) => Self::not_found(entity, id),
            ServiceError::Store(StoreError::Validation { field, reason }) => Self::new(
                StatusCode::BAD_REQUEST,
                "validation_failed",
                format!("invalid {field}: {reason}"),
                false,
            ),
            ServiceError::Store(_) => Self::internal(e.to_string()),
            ServiceError::Storage(StorageError::TooLarge { .. })
            | ServiceError::Storage(StorageError::QuotaExceeded { .. }) => {
                Self::payload_too_large(e.to_string())
            }
            ServiceError::Storage(StorageError::NotFound { .. }) => {
                Self::not_found("storage object", "unknown")
            }
            ServiceError::Storage(StorageError::PathOutsideRoot { .. }) => {
                Self::bad_request(e.to_string())
            }
            ServiceError::Storage(_) | ServiceError::Io(_) => Self::internal(e.to_string()),
            ServiceError::Config(_) | ServiceError::Db(_) => Self::internal(e.to_string()),
        }
    }
}

impl From<SecurityError> for ApiError {
    fn from(e: SecurityError) -> Self {
        use SecurityError as S;
        match e {
            S::FileTooLarge { .. } | S::OperationTooLarge { .. } => {
                Self::payload_too_large(e.to_string())
            }
            S::TooManyPages { .. } | S::TooManyOutputs { .. } => Self::new(
                StatusCode::BAD_REQUEST,
                "limit_exceeded",
                e.to_string(),
                false,
            ),
            _ => Self::new(
                StatusCode::BAD_REQUEST,
                "validation_failed",
                e.to_string(),
                false,
            ),
        }
    }
}

impl From<ImageError> for ApiError {
    fn from(e: ImageError) -> Self {
        match e {
            // Resource budgets map to 413 like storage/upload limits;
            // everything else is caller-fixable input → 400.
            ImageError::LimitExceeded(_) => Self::payload_too_large(e.to_string()),
            _ => Self::new(
                StatusCode::BAD_REQUEST,
                "validation_failed",
                e.to_string(),
                false,
            ),
        }
    }
}

impl From<crate::crates::scheduler::SchedulerError> for ApiError {
    fn from(e: crate::crates::scheduler::SchedulerError) -> Self {
        use crate::crates::scheduler::SchedulerError as S;
        match e {
            // Caller-fixable schedule input (blank name/kind, zero
            // interval) → 400; storage/job failures → 500.
            S::Invalid(_) => Self::bad_request(e.to_string()),
            _ => Self::internal(e.to_string()),
        }
    }
}

impl From<JobError> for ApiError {
    fn from(e: JobError) -> Self {
        match e {
            JobError::Payload(_) => Self::bad_request(e.to_string()),
            _ => Self::internal(e.to_string()),
        }
    }
}
