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
use axum::extract::Query;
use axum::http::{StatusCode, header};
use axum::response::{IntoResponse, Response};
use serde::Deserialize;

use crate::crates::core::engine::EngineLimits;
use crate::crates::image_engine::{ImageEngine, ImageError};
use crate::crates::video_engine::{MediaCapabilities, VideoEngine, VideoError, VideoInfo};

use super::auth::AuthenticatedSession;
use super::error::ApiError;

/// `GET /media/capabilities` — what this ffmpeg build can do.
///
/// Returns [`VideoEngine::media_capabilities`]: encoder/decoder/
/// filter/muxer/demuxer/protocol/hwaccel name lists probed from the
/// installed `ffmpeg`/`ffprobe` binaries (cached process-wide by the
/// engine). Requires a valid bearer session like every other
/// non-`/health` route.
pub async fn capabilities(
    _auth: AuthenticatedSession,
) -> Result<Json<MediaCapabilities>, ApiError> {
    let caps = tokio::task::spawn_blocking(|| VideoEngine::new().media_capabilities())
        .await
        .map_err(|e| ApiError::Internal(format!("capabilities task failed: {e}")))?
        .map_err(map_video_error)?;
    Ok(Json(caps))
}

/// `POST /media/probe` — inspect uploaded media bytes via ffprobe.
///
/// The request body is the media file itself (any container ffprobe
/// reads). Returns the [`VideoInfo`] JSON: duration, dimensions,
/// codecs, frame rate, format, stream inventory, chapters, tags.
/// Malformed/non-media input is a `400` (`VideoError::InvalidInput`).
pub async fn probe(_auth: AuthenticatedSession, body: Bytes) -> Result<Json<VideoInfo>, ApiError> {
    if body.is_empty() {
        return Err(ApiError::BadRequest(
            "request body must not be empty".into(),
        ));
    }
    let input = body.to_vec();
    let info = tokio::task::spawn_blocking(move || VideoEngine::new().probe(&input))
        .await
        .map_err(|e| ApiError::Internal(format!("probe task failed: {e}")))?
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
    _auth: AuthenticatedSession,
    Query(query): Query<ThumbnailQuery>,
    body: Bytes,
) -> Result<Response, ApiError> {
    if body.is_empty() {
        return Err(ApiError::BadRequest(
            "request body must not be empty".into(),
        ));
    }
    if let Some(w) = query.width {
        if w == 0 {
            return Err(ApiError::BadRequest(
                "width must be greater than zero".into(),
            ));
        }
    }
    let ThumbnailQuery { time, width } = query;

    let input = body.to_vec();
    let mut png = tokio::task::spawn_blocking(move || {
        VideoEngine::new().thumbnail(&input, time, &EngineLimits::default())
    })
    .await
    .map_err(|e| ApiError::Internal(format!("thumbnail task failed: {e}")))?
    .map_err(map_video_error)?;

    if let Some(w) = width {
        png = tokio::task::spawn_blocking(move || {
            ImageEngine::new()
                .resize_to(&png, Some(w), None)
                .map_err(map_image_error)
        })
        .await
        .map_err(|e| ApiError::Internal(format!("resize task failed: {e}")))??;
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
        VideoError::InvalidInput(msg) | VideoError::LimitExceeded(msg) => ApiError::BadRequest(msg),
        other => {
            tracing::warn!(error = %other, "media engine failure");
            ApiError::Internal(format!("media engine error: {other}"))
        }
    }
}

/// Same mapping for the image engine used by the thumbnail resize.
fn map_image_error(e: ImageError) -> ApiError {
    match e {
        ImageError::InvalidInput(msg) | ImageError::LimitExceeded(msg) => ApiError::BadRequest(msg),
        other => {
            tracing::warn!(error = %other, "image engine failure");
            ApiError::Internal(format!("image engine error: {other}"))
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::crates::api::{AppState, router};
    use crate::crates::application::Organization::auth_user::{NewUser, create_user};
    use crate::crates::application::Organization::init_db::{
        create_sessions_table, create_users_table,
    };
    use crate::crates::application::Organization::session::create_session;
    use crate::crates::job_engine::{JobEngineConfig, create_jobs_table};
    use axum::body::Body;
    use axum::http::{Request, header};
    use serde_json::Value;
    use sqlx::sqlite::SqlitePoolOptions;
    use tower::ServiceExt;

    fn test_session_config() -> crate::crates::application::Organization::init_db::SessionConfig {
        crate::crates::application::Organization::init_db::SessionConfig {
            ttl: std::time::Duration::from_secs(3600),
            absolute_ttl: std::time::Duration::ZERO,
            renew_on_use: true,
            touch_interval: std::time::Duration::ZERO,
            cleanup_interval: std::time::Duration::from_secs(60),
        }
    }

    /// In-memory pool with users, sessions, and jobs tables — mirrors
    /// the helpers in `lib.rs` tests (kept local so this module stays
    /// self-contained).
    async fn test_state() -> AppState {
        let pool = SqlitePoolOptions::new()
            .max_connections(1)
            .connect("sqlite::memory:")
            .await
            .unwrap();
        create_users_table(&pool).await.unwrap();
        create_sessions_table(&pool).await.unwrap();
        create_jobs_table(&pool).await.unwrap();
        AppState::new(pool, test_session_config(), JobEngineConfig::default())
    }

    /// Creates a user + session; returns the raw bearer token.
    async fn seed_session(state: &AppState) -> String {
        let user_id = create_user(
            &state.pool,
            &NewUser {
                first_name: "Api".into(),
                last_name: "Tester".into(),
                user_name: "api_tester".into(),
                email: "api@example.com".into(),
                password: "correct horse battery".into(),
            },
        )
        .await
        .unwrap();
        let (_session, token) = create_session(&state.pool, user_id, &state.session_config)
            .await
            .unwrap();
        token
    }

    /// Sends one request with an optional raw byte body; returns
    /// (status, content-type, body bytes).
    async fn send_bytes(
        app: &axum::Router,
        method: &str,
        uri: &str,
        token: Option<&str>,
        body: Option<&[u8]>,
    ) -> (StatusCode, Option<String>, Vec<u8>) {
        let mut builder = Request::builder().method(method).uri(uri);
        if let Some(t) = token {
            builder = builder.header(header::AUTHORIZATION, format!("Bearer {t}"));
        }
        let req = match body {
            Some(b) => builder
                .header(header::CONTENT_TYPE, "application/octet-stream")
                .body(Body::from(b.to_vec()))
                .unwrap(),
            None => builder.body(Body::empty()).unwrap(),
        };
        let resp = app.clone().oneshot(req).await.unwrap();
        let status = resp.status();
        let content_type = resp
            .headers()
            .get(header::CONTENT_TYPE)
            .and_then(|v| v.to_str().ok())
            .map(str::to_string);
        let bytes = axum::body::to_bytes(resp.into_body(), usize::MAX)
            .await
            .unwrap()
            .to_vec();
        (status, content_type, bytes)
    }

    /// JSON convenience wrapper around [`send_bytes`].
    async fn send_json(
        app: &axum::Router,
        method: &str,
        uri: &str,
        token: Option<&str>,
        body: Option<&[u8]>,
    ) -> (StatusCode, Value) {
        let (status, _, bytes) = send_bytes(app, method, uri, token, body).await;
        let value = if bytes.is_empty() {
            Value::Null
        } else {
            serde_json::from_slice(&bytes).unwrap_or(Value::Null)
        };
        (status, value)
    }

    #[tokio::test]
    async fn capabilities_returns_engine_capability_snapshot() {
        let state = test_state().await;
        let token = seed_session(&state).await;
        let app = router(state);

        // Unauthenticated → 401.
        let (status, _, _) = send_bytes(&app, "GET", "/media/capabilities", None, None).await;
        assert_eq!(status, StatusCode::UNAUTHORIZED);

        let (status, body) =
            send_json(&app, "GET", "/media/capabilities", Some(&token), None).await;
        assert_eq!(status, StatusCode::OK);
        // MediaCapabilities serializes its probed name lists.
        assert!(body["video_encoders"].is_array());
        assert!(body["audio_encoders"].is_array());
        assert!(body["muxers"].is_array());
        assert!(body["demuxers"].is_array());
    }

    #[tokio::test]
    async fn probe_rejects_garbage_and_requires_auth() {
        let state = test_state().await;
        let token = seed_session(&state).await;
        let app = router(state);

        // No token → 401.
        let (status, _, _) = send_bytes(&app, "POST", "/media/probe", None, Some(b"garbage")).await;
        assert_eq!(status, StatusCode::UNAUTHORIZED);

        // Garbage bytes are not a media container → 400.
        let (status, body) = send_json(
            &app,
            "POST",
            "/media/probe",
            Some(&token),
            Some(b"this is definitely not media"),
        )
        .await;
        assert_eq!(status, StatusCode::BAD_REQUEST);
        assert!(body["error"].is_string());

        // Empty body → 400.
        let (status, _, _) =
            send_bytes(&app, "POST", "/media/probe", Some(&token), Some(b"")).await;
        assert_eq!(status, StatusCode::BAD_REQUEST);
    }

    #[tokio::test]
    async fn thumbnail_rejects_garbage_and_requires_auth() {
        let state = test_state().await;
        let token = seed_session(&state).await;
        let app = router(state);

        // No token → 401.
        let (status, _, _) = send_bytes(
            &app,
            "POST",
            "/media/thumbnail?time=1.0&width=320",
            None,
            Some(b"garbage"),
        )
        .await;
        assert_eq!(status, StatusCode::UNAUTHORIZED);

        // Garbage bytes → engine InvalidInput → 400.
        let (status, _, _) = send_bytes(
            &app,
            "POST",
            "/media/thumbnail?time=1.0&width=320",
            Some(&token),
            Some(b"\x00\x01\x02not-a-video"),
        )
        .await;
        assert!(status.is_client_error() || status.is_server_error());
        assert_ne!(status, StatusCode::OK);

        // Degenerate width → 400 before touching ffmpeg.
        let (status, _, _) = send_bytes(
            &app,
            "POST",
            "/media/thumbnail?width=0",
            Some(&token),
            Some(b"whatever"),
        )
        .await;
        assert_eq!(status, StatusCode::BAD_REQUEST);
    }

    #[tokio::test]
    async fn thumbnail_produces_png_for_real_media() {
        let state = test_state().await;
        let token = seed_session(&state).await;
        let app = router(state);

        // Generate a tiny real MP4 with ffmpeg itself (testsrc).
        let media = tokio::task::spawn_blocking(|| -> Vec<u8> {
            let dir = std::env::temp_dir().join(format!("kaladoc-api-test-{}", std::process::id()));
            std::fs::create_dir_all(&dir).unwrap();
            let out = dir.join("clip.mp4");
            let encoder = ["libx264", "libopenh264", "mpeg4"]
                .iter()
                .find(|e| {
                    std::process::Command::new("ffmpeg")
                        .args(["-hide_banner", "-encoders"])
                        .output()
                        .map(|o| String::from_utf8_lossy(&o.stdout).contains(&format!(" {e} ")))
                        .unwrap_or(false)
                })
                .copied()
                .expect("no usable h264 encoder in test ffmpeg build");
            let status = std::process::Command::new("ffmpeg")
                .args([
                    "-y",
                    "-v",
                    "error",
                    "-f",
                    "lavfi",
                    "-i",
                    "testsrc=duration=1:size=64x64:rate=5",
                    "-f",
                    "lavfi",
                    "-i",
                    "sine=frequency=440:duration=1",
                    "-c:v",
                    encoder,
                    "-pix_fmt",
                    "yuv420p",
                    "-c:a",
                    "aac",
                    "-shortest",
                ])
                .arg(&out)
                .status()
                .unwrap();
            assert!(status.success());
            let bytes = std::fs::read(&out).unwrap();
            let _ = std::fs::remove_dir_all(&dir);
            bytes
        })
        .await
        .unwrap();

        let (status, content_type, png) = send_bytes(
            &app,
            "POST",
            "/media/thumbnail?time=0.5&width=32",
            Some(&token),
            Some(&media),
        )
        .await;
        assert_eq!(status, StatusCode::OK);
        assert_eq!(content_type.as_deref(), Some("image/png"));
        assert_eq!(&png[..8], b"\x89PNG\r\n\x1a\n");

        // Probe the same bytes → 200 JSON with stream info.
        let (status, info) =
            send_json(&app, "POST", "/media/probe", Some(&token), Some(&media)).await;
        assert_eq!(status, StatusCode::OK);
        assert_eq!(info["has_video"], true);
        assert_eq!(info["has_audio"], true);
        assert!(info["streams"].is_array());
    }
}
