//! Bearer-token authentication for protected routes.
//!
//! [`AuthenticatedSession`] is an axum [`FromRequestParts`]
//! extractor: it pulls the `Authorization: Bearer <token>` header,
//! hashes nothing itself, and delegates validation to
//! [`session::validate_session`](crate::crates::application::Organization::session::validate_session)
//! — the same code path `kaladoc auth` uses. Unknown, expired,
//! revoked, or absolutely-aged-out tokens are indistinguishable and
//! all produce [`ApiError::Unauthorized`] (`401`).
//!
//! On success the extractor yields the resolved [`Session`] (plus its
//! `user_id` for convenience) so handlers can attribute actions to a
//! user without re-querying.

use axum::extract::FromRequestParts;
use axum::http::request::Parts;
use axum::http::header::AUTHORIZATION;

use crate::crates::application::Organization::session::{
    Session, SessionError, validate_session,
};

use super::AppState;
use super::error::ApiError;

/// A request authenticated by a valid session token.
///
/// Insert this as a handler parameter (anywhere after `State` and
/// before the body extractor) to require authentication:
///
/// ```ignore
/// async fn handler(auth: AuthenticatedSession, State(state): State<AppState>) -> ...
/// ```
#[derive(Debug, Clone)]
pub struct AuthenticatedSession {
    /// The validated session row (`token_hash`, timestamps, ...).
    /// The raw token is never retained.
    pub session: Session,
    /// Convenience copy of `session.user_id`.
    pub user_id: i64,
}

impl FromRequestParts<AppState> for AuthenticatedSession {
    type Rejection = ApiError;

    async fn from_request_parts(
        parts: &mut Parts,
        state: &AppState,
    ) -> Result<Self, Self::Rejection> {
        let token = bearer_token(&parts.headers).ok_or(ApiError::Unauthorized)?;
        match validate_session(&state.pool, &token, &state.session_config).await {
            Ok(Some(session)) => Ok(Self {
                user_id: session.user_id,
                session,
            }),
            // Unknown / expired / revoked / absolutely aged out —
            // deliberately indistinguishable to the client.
            Ok(None) => Err(ApiError::Unauthorized),
            Err(e) => Err(ApiError::from_session(e)),
        }
    }
}

/// Extracts the token from `Authorization: Bearer <token>`.
///
/// Only the `Bearer` scheme is accepted (case-insensitive scheme,
/// per RFC 7235); extra whitespace around the token is trimmed.
/// Returns `None` for a missing header, a different scheme, or an
/// empty token.
fn bearer_token(headers: &axum::http::HeaderMap) -> Option<String> {
    let value = headers.get(AUTHORIZATION)?.to_str().ok()?;
    let (scheme, token) = value.split_once(' ')?;
    if !scheme.eq_ignore_ascii_case("bearer") {
        return None;
    }
    let token = token.trim();
    if token.is_empty() {
        return None;
    }
    Some(token.to_string())
}

impl ApiError {
    /// Maps a session-store failure to a 500 — DB outages are not the
    /// client's fault, but the detail stays generic (no internals leak).
    fn from_session(e: SessionError) -> Self {
        tracing::warn!(error = %e, "session validation failed");
        ApiError::Internal("session store unavailable".to_string())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use axum::http::HeaderValue;

    fn headers_with(value: &str) -> axum::http::HeaderMap {
        let mut h = axum::http::HeaderMap::new();
        h.insert(AUTHORIZATION, HeaderValue::from_str(value).unwrap());
        h
    }

    #[test]
    fn bearer_token_parsing() {
        assert_eq!(
            bearer_token(&headers_with("Bearer abc123")).as_deref(),
            Some("abc123")
        );
        // Scheme is case-insensitive; token whitespace is trimmed.
        assert_eq!(
            bearer_token(&headers_with("bearer   tok  ")).as_deref(),
            Some("tok")
        );
        // Wrong scheme / missing header / empty token → None.
        assert!(bearer_token(&headers_with("Basic abc")).is_none());
        assert!(bearer_token(&headers_with("Bearer")).is_none());
        assert!(bearer_token(&headers_with("Bearer   ")).is_none());
        assert!(bearer_token(&axum::http::HeaderMap::new()).is_none());
    }
}
