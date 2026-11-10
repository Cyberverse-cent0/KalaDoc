//! Endpoint feature flags — per-group kill switches.
//!
//! [`require_enabled`] runs on the protected router *before*
//! authentication: when a request's route group is listed in
//! `ApiState::disabled_groups` (config `api.disabled_groups` /
//! `KALADOC_API_DISABLE`, comma-separated) it answers `404` with a
//! clear error, as if the routes did not exist. `/health` and
//! `/openapi.json` are public and never gated.
//!
//! Group names map to the first path segment of the route:
//!
//! | Group        | Routes                                             |
//! |--------------|----------------------------------------------------|
//! | `documents`  | `/documents`, `/documents/{id}/…`                  |
//! | `folders`    | `/folders`                                         |
//! | `jobs`       | `/jobs`, `/jobs/{id}/…`, `/jobs/batch/…`           |
//! | `pipelines`  | `/pipelines`, `/pipelines/{name}`, `/pipelines/run`|
//! | `engines`    | `/engines`                                         |
//! | `storage`    | `/storage`                                         |
//! | `schedules`  | `/schedules`, `/schedules/{name}`, `/schedules/tick` |
//!
//! Unknown names in the disable list are ignored (a typo disables
//! nothing rather than everything); the effective set is whatever
//! [`route_group`] recognizes.

use std::sync::Arc;

use axum::extract::{Request, State};
use axum::middleware::Next;
use axum::response::{IntoResponse, Response};

use super::ApiError;
use super::state::ApiState;
use crate::crates::observability::RequestId;

/// Route groups that can be disabled via `KALADOC_API_DISABLE`.
pub const GROUPS: &[&str] = &[
    "documents",
    "folders",
    "jobs",
    "pipelines",
    "engines",
    "storage",
    "schedules",
];

/// Maps a request path to its route group (`None` for paths outside
/// the protected router — those are never gated).
fn route_group(path: &str) -> Option<&'static str> {
    let segment = path.trim_start_matches('/').split('/').next()?;
    GROUPS.iter().copied().find(|g| *g == segment)
}

/// `404`s requests whose route group is disabled; passes everything
/// else through. Runs outermost on the protected router so a disabled
/// group is invisible even to unauthenticated callers.
pub async fn require_enabled(
    State(state): State<Arc<ApiState>>,
    request: Request,
    next: Next,
) -> Result<Response, ApiError> {
    if let Some(group) = route_group(request.uri().path())
        && state.disabled_groups.iter().any(|g| g.as_str() == group)
    {
        return Err(ApiError::new(
            axum::http::StatusCode::NOT_FOUND,
            "not_found",
            format!(
                "endpoint group {group:?} is disabled (see KALADOC_API_DISABLE / api.disabled_groups)"
            ),
            false,
        )
        .with_request_id(request_id_from(request.headers())));
    }
    Ok(next.run(request).await.into_response())
}

/// Propagates a well-formed `x-request-id` header into error bodies.
fn request_id_from(headers: &axum::http::HeaderMap) -> RequestId {
    headers
        .get("x-request-id")
        .and_then(|v| v.to_str().ok())
        .and_then(RequestId::parse)
        .unwrap_or_default()
}
