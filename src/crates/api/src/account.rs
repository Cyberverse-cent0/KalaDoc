//! Self-service account endpoints: register, login, logout, profile.
//!
//! `POST /auth/register` and `POST /auth/login` are **public** — they are
//! mounted on the outer router next to `/health`. Both issue a session
//! bearer token (the same kind `kaladoc auth login` stores) that clients
//! then send as `Authorization: Bearer <token>` on every protected route.
//!
//! `POST /auth/logout` and `GET /auth/me` sit behind the Bearer middleware
//! like every other protected route. API-key callers can use `/auth/me`
//! (they get their `apikey:<name>` actor back) but `/auth/logout` is a
//! no-op for them — there is no server-side session to revoke.
//!
//! Passwords are hashed with Argon2id by [`auth_user::create_user`]; they
//! are never logged, stored in plaintext, or echoed in responses. Login
//! failures return a generic `401` so usernames cannot be enumerated.

use axum::Json;
use axum::extract::{Extension, State};
use axum::http::HeaderMap;
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use std::sync::Arc;

use crate::crates::application::Organization::auth_user::{self, AuthError, NewUser};
use crate::crates::application::Organization::session;

use super::ApiError;
use super::auth::AuthUser;
use super::state::ApiState;

/// `POST /auth/register` request body.
#[derive(Debug, Deserialize)]
pub struct RegisterRequest {
    pub first_name: String,
    pub last_name: String,
    pub user_name: String,
    pub email: String,
    pub password: String,
}

/// `POST /auth/login` request body.
#[derive(Debug, Deserialize)]
pub struct LoginRequest {
    pub user_name: String,
    pub password: String,
}

/// Public profile shape returned to clients — never includes
/// `password_hash` or `token_hash`.
#[derive(Debug, Serialize)]
struct UserProfile {
    id: i64,
    first_name: String,
    last_name: String,
    user_name: String,
    email: String,
}

/// Login/register response: the raw session token (shown once) plus the
/// profile it belongs to.
#[derive(Debug, Serialize)]
struct SessionResponse {
    token: String,
    expires_at: i64,
    user: UserProfile,
}

/// `POST /auth/register` — create an account and open a session.
pub async fn register(
    State(state): State<Arc<ApiState>>,
    Json(req): Json<RegisterRequest>,
) -> Result<Json<Value>, ApiError> {
    let pool = state.services.pool();
    let new = NewUser {
        first_name: req.first_name.trim().to_string(),
        last_name: req.last_name.trim().to_string(),
        user_name: req.user_name.trim().to_string(),
        email: req.email.trim().to_string(),
        password: req.password,
    };

    let user_id = auth_user::create_user(pool, &new)
        .await
        .map_err(map_auth_error)?;

    Ok(Json(json!(issue_session(&state, user_id).await?)))
}

/// `POST /auth/login` — validate credentials and open a session.
pub async fn login(
    State(state): State<Arc<ApiState>>,
    Json(req): Json<LoginRequest>,
) -> Result<Json<Value>, ApiError> {
    let pool = state.services.pool();
    let user = auth_user::validate_user(pool, req.user_name.trim(), &req.password)
        .await
        .map_err(map_auth_error)?
        .ok_or_else(|| ApiError::unauthorized("invalid username or password"))?;

    Ok(Json(json!(issue_session(&state, user.id).await?)))
}

/// `POST /auth/logout` — revoke the caller's session token.
///
/// API-key callers have no session; they get `{ "logged_out": false }`.
pub async fn logout(
    State(state): State<Arc<ApiState>>,
    Extension(user): Extension<AuthUser>,
    headers: HeaderMap,
) -> Result<Json<Value>, ApiError> {
    let Some(session_id) = user.session_id else {
        return Ok(Json(json!({ "logged_out": false, "reason": "api_key" })));
    };
    let _ = session_id; // session row is revoked by token below
    let token = bearer_token(&headers)
        .ok_or_else(|| ApiError::unauthorized("missing bearer token"))?;
    session::revoke_session(state.services.pool(), token)
        .await
        .map_err(map_session_error)?;
    Ok(Json(json!({ "logged_out": true })))
}

/// `GET /auth/me` — the authenticated principal's profile.
///
/// Session users get their account profile; API-key callers get their
/// `apikey:<name>` actor identity.
pub async fn me(
    State(state): State<Arc<ApiState>>,
    Extension(user): Extension<AuthUser>,
) -> Result<Json<Value>, ApiError> {
    if user.session_id.is_none() {
        return Ok(Json(json!({
            "kind": "api_key",
            "actor_id": user.actor_id,
            "owner_id": user.owner_id,
        })));
    }
    let id: i64 = user
        .owner_id
        .parse()
        .map_err(|_| ApiError::internal("malformed session owner id"))?;
    let profile = auth_user::get_user_by_id(state.services.pool(), id)
        .await
        .map_err(map_auth_error)?
        .ok_or_else(|| ApiError::not_found("user", id))?;
    Ok(Json(json!({
        "kind": "session",
        "session_id": user.session_id,
        "user": profile_json(&profile),
    })))
}

// ---------------------------------------------------------------------------
// Internals
// ---------------------------------------------------------------------------

/// Creates a session for `user_id` and shapes the login response.
async fn issue_session(
    state: &ApiState,
    user_id: i64,
) -> Result<SessionResponse, ApiError> {
    let pool = state.services.pool();
    let user = auth_user::get_user_by_id(pool, user_id)
        .await
        .map_err(map_auth_error)?
        .ok_or_else(|| ApiError::not_found("user", user_id))?;
    let (sess, token) = session::create_session(pool, user_id, &state.session_config)
        .await
        .map_err(map_session_error)?;
    Ok(SessionResponse {
        token,
        expires_at: sess.expires_at,
        user: UserProfile {
            id: user.id,
            first_name: user.first_name,
            last_name: user.last_name,
            user_name: user.user_name,
            email: user.email,
        },
    })
}

fn profile_json(u: &crate::core::auth::AuthUser) -> Value {
    json!({
        "id": u.id,
        "first_name": u.first_name,
        "last_name": u.last_name,
        "user_name": u.user_name,
        "email": u.email,
    })
}

fn bearer_token(headers: &HeaderMap) -> Option<&str> {
    let value = headers.get(axum::http::header::AUTHORIZATION)?.to_str().ok()?;
    let (scheme, token) = value.split_once(' ')?;
    scheme
        .eq_ignore_ascii_case("bearer")
        .then(|| token.trim())
        .filter(|t| !t.is_empty())
}

/// `AuthError` → HTTP: caller-fixable input is `400`, uniqueness
/// conflicts are `409`, everything else is an internal failure.
fn map_auth_error(e: AuthError) -> ApiError {
    match e {
        AuthError::Validation(_) => ApiError::bad_request(e.to_string()),
        AuthError::DuplicateUserName(_) | AuthError::DuplicateEmail(_) => {
            ApiError::new(
                axum::http::StatusCode::CONFLICT,
                "conflict",
                e.to_string(),
                false,
            )
        }
        _ => ApiError::internal(e.to_string()),
    }
}

fn map_session_error(e: session::SessionError) -> ApiError {
    ApiError::internal(e.to_string())
}
