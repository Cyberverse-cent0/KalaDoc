//! Bearer-token authentication middleware.
//!
//! [`require_auth`] runs on every protected route: it extracts the
//! `Authorization: Bearer <token>` header, validates the token against
//! `sessions_tbl` via
//! [`session::validate_session`](crate::crates::application::Organization::session::validate_session),
//! and stores the resolved [`AuthUser`] in request extensions for
//! handlers. Missing/malformed/invalid tokens get a `401` in the
//! standard error envelope; database failures get a `500`.

use std::sync::Arc;

use axum::extract::{Request, State};
use axum::http::header;
use axum::middleware::Next;
use axum::response::{IntoResponse, Response};

use crate::crates::application::Organization::session;
use crate::crates::observability::RequestId;

use super::ApiError;
use super::state::ApiState;

/// The authenticated principal, inserted into request extensions by
/// [`require_auth`]. `user_id` is the session's user id — the value
/// adapters use as `owner_id`/`actor_id` when registering documents.
#[derive(Debug, Clone, Copy)]
pub struct AuthUser {
    pub user_id: i64,
    pub session_id: i64,
}

/// `Authorization: Bearer <token>` gate for protected routes.
pub async fn require_auth(
    State(state): State<Arc<ApiState>>,
    mut request: Request,
    next: Next,
) -> Result<Response, ApiError> {
    let token = bearer_token(request.headers()).ok_or_else(|| {
        ApiError::unauthorized("missing or malformed Authorization: Bearer <token> header")
            .with_request_id(request_id_from(request.headers()))
    })?;

    let session = session::validate_session(state.services.pool(), token, &state.session_config)
        .await
        .map_err(|e| ApiError::internal(format!("session validation failed: {e}")))?
        .ok_or_else(|| {
            ApiError::unauthorized("invalid or expired session token")
                .with_request_id(request_id_from(request.headers()))
        })?;

    request.extensions_mut().insert(AuthUser {
        user_id: session.user_id,
        session_id: session.id,
    });
    Ok(next.run(request).await.into_response())
}

/// Extracts the token from `Authorization: Bearer …` (case-insensitive
/// scheme). Returns `None` when the header is absent or malformed.
fn bearer_token(headers: &axum::http::HeaderMap) -> Option<&str> {
    let value = headers.get(header::AUTHORIZATION)?.to_str().ok()?;
    let (scheme, token) = value.split_once(' ')?;
    if !scheme.eq_ignore_ascii_case("bearer") {
        return None;
    }
    let token = token.trim();
    (!token.is_empty()).then_some(token)
}

/// Propagates a well-formed `x-request-id` header into error bodies;
/// anything else yields a fresh id.
fn request_id_from(headers: &axum::http::HeaderMap) -> RequestId {
    headers
        .get("x-request-id")
        .and_then(|v| v.to_str().ok())
        .and_then(RequestId::parse)
        .unwrap_or_default()
}
