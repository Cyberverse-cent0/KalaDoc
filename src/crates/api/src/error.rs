//! HTTP error type — the single [`IntoResponse`] surface for the API.
//!
//! Handlers return `Result<_, ApiError>`; every variant maps to a
//! status code plus a small JSON body `{"error": "<message>"}`.
//! Messages are client-safe: they describe *what* was wrong with the
//! request, never internal state (SQL errors, paths, stack traces).

use axum::Json;
use axum::http::StatusCode;
use axum::response::{IntoResponse, Response};
use serde_json::json;

/// Errors produced by API handlers and extractors.
#[derive(Debug, thiserror::Error)]
pub enum ApiError {
    /// Missing, malformed, expired, or revoked bearer token.
    #[error("authentication required")]
    Unauthorized,

    /// The request was well-formed HTTP/JSON but semantically invalid
    /// (bad payload shape, unknown status filter, ...). The message
    /// is safe to show the client.
    #[error("{0}")]
    BadRequest(String),

    /// The addressed resource does not exist.
    #[error("{0}")]
    NotFound(String),

    /// An internal failure (database, store, engine). The detail is
    /// logged, not returned — clients get a generic message.
    #[error("{0}")]
    Internal(String),
}

impl IntoResponse for ApiError {
    fn into_response(self) -> Response {
        let (status, public_message) = match &self {
            Self::Unauthorized => (StatusCode::UNAUTHORIZED, self.to_string()),
            Self::BadRequest(msg) => (StatusCode::BAD_REQUEST, msg.clone()),
            Self::NotFound(msg) => (StatusCode::NOT_FOUND, msg.clone()),
            Self::Internal(msg) => {
                // Log the detail server-side; respond generically so
                // internals (SQL text, paths) never reach the client.
                tracing::error!(error = %msg, "api internal error");
                (
                    StatusCode::INTERNAL_SERVER_ERROR,
                    "internal server error".to_string(),
                )
            }
        };
        let mut response = (status, Json(json!({ "error": public_message }))).into_response();
        if matches!(self, Self::Unauthorized) {
            // RFC 7235 hint so clients know to retry with credentials.
            response.headers_mut().insert(
                axum::http::header::WWW_AUTHENTICATE,
                axum::http::HeaderValue::from_static("Bearer"),
            );
        }
        response
    }
}
