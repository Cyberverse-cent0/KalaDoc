//! Media endpoints — a thin `axum` adapter over the `video_engine`
//! (ffmpeg/ffprobe) and `image_engine` crates.
//!
//! Like [`super::handlers`], every handler here owns **no** media
//! logic: it extracts the request, calls one engine method inside
//! `tokio::task::spawn_blocking` (the engines are synchronous,
//! subprocess-bound functions — see `video_engine::engine` docs), and
//! maps the result onto a status code + body.
//!
//! # Endpoints
//!
//! | Method & path              | Auth | Purpose                                  |
//! |----------------------------|------|------------------------------------------|
//! | `GET /media/capabilities`  | yes  | ffmpeg build capability snapshot        |
//! | `POST /media/probe`        | yes  | ffprobe inspection of uploaded bytes    |
//! | `POST /media/thumbnail`    | yes  | PNG frame at `?time=`/`?width=`         |
//!
//! Uploads are raw request bodies (`Bytes`), not JSON — media bytes
//! are binary and would not survive a JSON envelope. Errors reuse
//! [`ApiError`]: engine `InvalidInput`/`LimitExceeded` failures are
//! client errors (`400`); tool/IO failures stay generic `500`s.

use axum::Json;
use axum::body::Bytes;
use axum::extract::{Extension, Query};
use axum::http::{StatusCode, header};
use axum::response::{IntoResponse, Response};
use serde::Deserialize;

use crate::crates::core::engine::EngineLimits;
use crate::crates::image_engine::{ImageEngine, ImageError};
use crate::crates::video_engine::{MediaCapabilities, VideoEngine, VideoError, VideoInfo};

use super::auth::AuthUser;
use super::error::ApiError;

/// `GET /media/capabilities` — what this ffmpeg build can do.
///
/// Returns [`VideoEngine::media_capabilities`]: encoder/decoder/
/// filter/muxer/demuxer/protocol/hwaccel name lists probed from the
/// installed `ffmpeg`/`ffprobe` binaries (cached process-wide by the
/// engine). Requires a valid bearer session like every other
/// non-`/health` route.
pub async fn capabilities(
    _auth: Extension<AuthUser>,
) -> Result<Json<MediaCapabilities>, ApiError> {
    let caps = tokio::task::spawn_blocking(|| VideoEngine::new().media_capabilities())
        .await
        .map_err(|e| ApiError::internal(format!("capabilities task failed: {e}")))?
        .map_err(map_video_error)?;
    Ok(Json(caps))
}

/// `POST /media/probe` — inspect uploaded media bytes via ffprobe.
///
/// The request body is the media file itself (any container ffprobe
/// reads). Returns the [`VideoInfo`] JSON: duration, dimensions,
/// codecs, frame rate, format, stream inventory, chapters, tags.
/// Malformed/non-media input is a `400` (`VideoError::InvalidInput`).
pub async fn probe(_auth: Extension<AuthUser>, body: Bytes) -> Result<Json<VideoInfo>, ApiError> {
    if body.is_empty() {
        return Err(ApiError::bad_request(
            "request body must not be empty",
        ));
    }
    let input = body.to_vec();
    let info = tokio::task::spawn_blocking(move || VideoEngine::new().probe(&input))
        .await
        .map_err(|e| ApiError::internal(format!("probe task failed: {e}")))?
        .map_err(map_video_error)?;
    Ok(Json(info))
}

/// `POST /media/thumbnail?time=&width=` — PNG frame of uploaded media.
///
/// `time` (seconds, optional) selects the seek point — `None` uses the
/// engine default (~10% of duration, 1s fallback). `width` (pixels,
/// optional) bounds the output width preserving aspect ratio via
/// [`ImageEngine::resize_to`]; absent → native frame size.
///
/// Responds `200` with `Content-Type: image/png` and the PNG bytes.
pub async fn thumbnail(
    _auth: Extension<AuthUser>,
    Query(query): Query<ThumbnailQuery>,
    body: Bytes,
) -> Result<Response, ApiError> {
    if body.is_empty() {
        return Err(ApiError::bad_request(
            "request body must not be empty",
        ));
    }
    if let Some(w) = query.width {
        if w == 0 {
            return Err(ApiError::bad_request(
                "width must be greater than zero",
            ));
        }
    }
    let ThumbnailQuery { time, width } = query;

    let input = body.to_vec();
    let mut png = tokio::task::spawn_blocking(move || {
        VideoEngine::new().thumbnail(&input, time, &EngineLimits::default())
    })
    .await
    .map_err(|e| ApiError::internal(format!("thumbnail task failed: {e}")))?
    .map_err(map_video_error)?;

    if let Some(w) = width {
        png = tokio::task::spawn_blocking(move || {
            ImageEngine::new()
                .resize_to(&png, Some(w), None)
                .map_err(map_image_error)
        })
        .await
        .map_err(|e| ApiError::internal(format!("resize task failed: {e}")))??;
    }

    Ok((StatusCode::OK, [(header::CONTENT_TYPE, "image/png")], png).into_response())
}

/// Query parameters for `POST /media/thumbnail`.
#[derive(Debug, Clone, Default, Deserialize)]
pub struct ThumbnailQuery {
    /// Seek point in seconds (`None` → engine default ~10% of
    /// duration). Must be finite and non-negative — enforced by the
    /// engine, surfaced here as a `400`.
    #[serde(default)]
    pub time: Option<f64>,
    /// Maximum output width in pixels, aspect preserved (`None` →
    /// native frame size). Must be `> 0`.
    #[serde(default)]
    pub width: Option<u32>,
}

/// Maps engine failures onto the API error surface.
///
/// `InvalidInput`/`LimitExceeded` describe problems with *the
/// client's bytes* (not media, over a bound) → `400` with the
/// engine's client-safe message. Tool absence, subprocess failures,
/// and I/O problems are server-side → generic `500` (detail logged).
fn map_video_error(e: VideoError) -> ApiError {
    match e {
        VideoError::InvalidInput(msg) | VideoError::LimitExceeded(msg) => ApiError::bad_request(msg),
        other => {
            tracing::warn!(error = %other, "media engine failure");
            ApiError::internal(format!("media engine error: {other}"))
        }
    }
}

/// Same mapping for the image engine used by the thumbnail resize.
fn map_image_error(e: ImageError) -> ApiError {
    match e {
        ImageError::InvalidInput(msg) | ImageError::LimitExceeded(msg) => ApiError::bad_request(msg),
        other => {
            tracing::warn!(error = %other, "image engine failure");
            ApiError::internal(format!("image engine error: {other}"))
        }
    }
}

