//! Bearer-token authentication middleware.
//!
//! [`require_auth`] runs on every protected route: it extracts the
//! `Authorization: Bearer <token>` header and resolves it two ways —
//!
//! 1. **Static API keys** — if the token matches a configured
//!    [`ApiKey`](crate::config::ApiKey) (`api.api_keys` /
//!    `KALADOC_API_KEYS`, `name:key` pairs), the request is
//!    authenticated as `apikey:<name>` with no session.
//! 2. **Session tokens** — otherwise the token is validated against
//!    `sessions_tbl` via
//!    [`session::validate_session`](crate::crates::application::Organization::session::validate_session).
//!
//! The resolved [`AuthUser`] is stored in request extensions for
//! handlers. Missing/malformed/invalid credentials get a `401` in the
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
/// [`require_auth`].
///
/// `owner_id` is the default owner scope handlers use for
/// owner-scoped listings/uploads; `actor_id` is what audit and job
/// payloads record as the acting identity. For session tokens both are
/// the numeric user id; for static API keys both are `apikey:<name>`.
#[derive(Debug, Clone)]
pub struct AuthUser {
    /// Owner-scope id (session user id, or `apikey:<name>`).
    pub owner_id: String,
    /// Actor id recorded in audit/job payloads.
    pub actor_id: String,
    /// Session row id for session-token auth; `None` for API keys.
    pub session_id: Option<i64>,
}

impl AuthUser {
    /// Session-authenticated principal.
    fn session(user_id: i64, session_id: i64) -> Self {
        Self {
            owner_id: user_id.to_string(),
            actor_id: user_id.to_string(),
            session_id: Some(session_id),
        }
    }

    /// API-key-authenticated principal; `name` is the key's configured
    /// name (the `apikey:<name>` actor).
    fn api_key(name: &str) -> Self {
        let actor = format!("apikey:{name}");
        Self {
            owner_id: actor.clone(),
            actor_id: actor,
            session_id: None,
        }
    }
}

/// `Authorization: Bearer <token>` gate for protected routes.
///
/// Static API keys are checked first (constant-time compare, no I/O);
/// anything else falls through to session validation. An unknown key
/// is not distinguished from an unknown session token — both `401`.
pub async fn require_auth(
    State(state): State<Arc<ApiState>>,
    mut request: Request,
    next: Next,
) -> Result<Response, ApiError> {
    let token = bearer_token(request.headers()).ok_or_else(|| {
        ApiError::unauthorized("missing or malformed Authorization: Bearer <token> header")
            .with_request_id(request_id_from(request.headers()))
    })?;

    if let Some(name) = api_key_name(&state, token) {
        request.extensions_mut().insert(AuthUser::api_key(name));
        return Ok(next.run(request).await.into_response());
    }

    let session = session::validate_session(state.services.pool(), token, &state.session_config)
        .await
        .map_err(|e| ApiError::internal(format!("session validation failed: {e}")))?
        .ok_or_else(|| {
            ApiError::unauthorized("invalid or expired session token")
                .with_request_id(request_id_from(request.headers()))
        })?;

    request
        .extensions_mut()
        .insert(AuthUser::session(session.user_id, session.id));
    Ok(next.run(request).await.into_response())
}

/// Returns the configured key name when `token` matches a static API
/// key, else `None`. Comparisons are length-checked then constant-time
/// over the bytes so the key list isn't a timing oracle.
fn api_key_name<'a>(state: &'a ApiState, token: &str) -> Option<&'a str> {
    state
        .api_keys
        .iter()
        .find(|k| ct_eq(k.key.as_bytes(), token.as_bytes()))
        .map(|k| k.name.as_str())
}

/// Constant-time byte-string equality (no early exit on mismatch).
fn ct_eq(a: &[u8], b: &[u8]) -> bool {
    a.len() == b.len()
        && a.iter()
            .zip(b.iter())
            .fold(0u8, |acc, (x, y)| acc | (x ^ y))
            == 0
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
