//! MCP `ServerHandler` implementation and the `kaladoc.*` tool set.
//!
//! Tools are read-only adapters over the same services the CLI commands
//! use ([`EngineRegistry`], [`VideoEngine`], [`ImageEngine`], config,
//! and the local database). Media tools accept either a local file path
//! or base64-encoded bytes and run the synchronous ffmpeg-backed engine
//! inside `tokio::task::spawn_blocking`, mirroring
//! `api::media`/`cli::command::video`.

use std::path::{Path, PathBuf};

use base64::Engine as _;
use rmcp::handler::server::wrapper::Parameters;
use rmcp::model::{CallToolResult, ContentBlock, ErrorData};
use rmcp::schemars::JsonSchema;
use rmcp::{ServerHandler, tool, tool_handler, tool_router};
use serde::Deserialize;
use serde_json::{Value, json};
use tokio::task::spawn_blocking;
use tracing::warn;

use crate::config::AppConfig;
use crate::crates::application::Organization::init_db;
use crate::crates::engine_registry::{EngineHealthEntry, EngineInfo, EngineRegistry};
use crate::crates::image_engine::{ImageEngine, ImageError};
use crate::crates::shared_public::platform::platform;
use crate::crates::video_engine::{VideoEngine, VideoError};
use crate::deployments::require;

/// Hard cap on a single media input accepted by the MCP media tools.
/// Deliberately far below the engine's own 32 GiB cap: tool calls are
/// synchronous and must stay bounded (spec §10.1).
const MAX_MEDIA_INPUT_BYTES: u64 = 256 * 1024 * 1024; // 256 MiB

/// Encoded-length bound matching [`MAX_MEDIA_INPUT_BYTES`] plus
/// base64 padding/whitespace headroom.
const MAX_MEDIA_BASE64_CHARS: usize = ((MAX_MEDIA_INPUT_BYTES as usize) / 3 + 1) * 4 + 64;

/// Cap on a generated thumbnail returned inline as base64.
const MAX_THUMBNAIL_BYTES: usize = 16 * 1024 * 1024; // 16 MiB

/// Cap on media duration the MCP tools will process.
const MAX_MEDIA_DURATION_SECS: u64 = 4 * 60 * 60; // 4 hours

/// The MCP server: owns the engine registry and stateless tool
/// handlers. Everything it needs beyond process state is the optional
/// `--config` path override.
pub struct McpServer {
    /// `--config` path override; `None` = default configuration search.
    config_path: Option<PathBuf>,
    /// Built-in engine registry (convert, image, pdf, pipeline,
    /// security, video) for discovery/health/capabilities tools.
    engines: EngineRegistry,
    /// ffmpeg-backed media engine for probe/thumbnail tools.
    video: VideoEngine,
    /// Image engine used to downscale thumbnails (`width` argument).
    images: ImageEngine,
}

impl McpServer {
    /// Build a server. `config_path` mirrors the CLI `--config` flag.
    pub fn new(config_path: Option<PathBuf>) -> Self {
        Self {
            config_path,
            engines: EngineRegistry::builtin(),
            video: VideoEngine::new(),
            images: ImageEngine::new(),
        }
    }

    // ------------------------------------------------------------------
    // Configuration access (mirrors `cli::context::Context` helpers)
    // ------------------------------------------------------------------

    /// Strict config load — fails when required secret env vars are
    /// missing. Used by checks that need real credentials.
    fn load_config(&self) -> Result<AppConfig, String> {
        match &self.config_path {
            Some(path) => AppConfig::load_with(path),
            None => AppConfig::load(),
        }
        .map_err(|e| e.to_string())
    }

    /// Lenient config load for diagnostics: missing secrets come back
    /// empty instead of failing.
    fn load_config_lenient(&self) -> Result<AppConfig, String> {
        match &self.config_path {
            Some(path) => AppConfig::load_lenient_with(path),
            None => AppConfig::load_lenient(),
        }
        .map_err(|e| e.to_string())
    }

    /// Resolved [`init_db::DbConfig`], preparing the storage root first
    /// — same contract as `Context::db_config`.
    fn db_config(&self) -> Result<init_db::DbConfig, String> {
        let app = self.load_config()?;
        crate::crates::storage::local::setup::setup(&app.to_storage_config())
            .map_err(|e| e.to_string())?;
        Ok(app.to_db_config())
    }
}

// ---------------------------------------------------------------------------
// Tool argument types (JSON Schema is derived from these via schemars)
// ---------------------------------------------------------------------------

/// Exactly-one-of `path`/`base64` media source shared by the media
/// tools. Kept as two flat optional fields so clients see a simple
/// schema; validation enforces the XOR.
#[derive(Debug, Deserialize, JsonSchema)]
#[schemars(crate = "rmcp::schemars")]
struct MediaProbeArgs {
    /// Path to a media file on this machine's filesystem, readable by
    /// the `kaladoc` process. Mutually exclusive with `base64`.
    #[serde(default)]
    path: Option<String>,
    /// Base64-encoded (standard alphabet) media file bytes.
    /// Mutually exclusive with `path`.
    #[serde(default)]
    base64: Option<String>,
}

#[derive(Debug, Deserialize, JsonSchema)]
#[schemars(crate = "rmcp::schemars")]
struct MediaThumbnailArgs {
    /// Path to a media file on this machine's filesystem. Mutually
    /// exclusive with `base64`.
    #[serde(default)]
    path: Option<String>,
    /// Base64-encoded (standard alphabet) media file bytes. Mutually
    /// exclusive with `path`.
    #[serde(default)]
    base64: Option<String>,
    /// Timestamp in seconds to grab the frame from. Omit for the
    /// engine default (~10% of duration, fallback 1s). Must be finite
    /// and non-negative.
    #[serde(default)]
    at: Option<f64>,
    /// Maximum output width in pixels; aspect ratio is preserved and
    /// the frame is downscaled via the image engine. Omit for native
    /// frame size.
    #[serde(default)]
    width: Option<u32>,
}

#[derive(Debug, Deserialize, JsonSchema)]
#[schemars(crate = "rmcp::schemars")]
struct EngineCapabilitiesArgs {
    /// Registered engine name, e.g. `convert`, `image`, `pdf`,
    /// `pipeline`, `security`, or `video` (see
    /// `kaladoc.engine_list`).
    name: String,
}

// ---------------------------------------------------------------------------
// Result helpers
// ---------------------------------------------------------------------------

/// Successful tool result: concise human-readable text plus the full
/// JSON payload in `structuredContent` (spec §10.1).
fn ok_result(value: Value, summary: impl Into<String>) -> CallToolResult {
    let mut result = CallToolResult::success(vec![ContentBlock::text(summary.into())]);
    result.structured_content = Some(value);
    result
}

/// Tool-level error (the request was valid; execution failed). The
/// caller sees `content`/`structuredContent`, unlike a protocol error.
fn tool_error(code: &str, message: impl Into<String>) -> CallToolResult {
    CallToolResult::structured_error(json!({
        "error": { "code": code, "message": message.into() }
    }))
}

/// Protocol-level error for infrastructure failures only (e.g. a
/// blocking task panicked). Callers render these opaquely.
fn internal_error(message: impl Into<String>) -> ErrorData {
    ErrorData::internal_error(message.into(), None)
}

/// Limits handed to media engines: bounded input size and duration on
/// top of the engine's own hard caps.
fn media_limits() -> crate::crates::core::engine::EngineLimits {
    crate::crates::core::engine::EngineLimits {
        max_input_bytes: Some(MAX_MEDIA_INPUT_BYTES),
        max_duration_secs: Some(MAX_MEDIA_DURATION_SECS),
        ..crate::crates::core::engine::EngineLimits::default()
    }
}

/// Resolve the `path`/`base64` pair into media bytes, enforcing size
/// limits. Returns a tool-error result string on failure — callers map
/// it with [`tool_error`].
fn load_media_input(path: Option<&str>, base64_data: Option<&str>) -> Result<Vec<u8>, String> {
    match (path, base64_data) {
        (Some(p), None) => read_media_path(p),
        (None, Some(b)) => decode_media_base64(b),
        (Some(_), Some(_)) => {
            Err("provide exactly one of `path` or `base64`, not both".to_string())
        }
        (None, None) => Err("provide exactly one of `path` or `base64`".to_string()),
    }
}

fn read_media_path(path: &str) -> Result<Vec<u8>, String> {
    if path.trim().is_empty() {
        return Err("`path` must not be empty".to_string());
    }
    let file = Path::new(path);
    let meta = std::fs::metadata(file).map_err(|e| format!("cannot stat {path:?}: {e}"))?;
    if !meta.is_file() {
        return Err(format!("{path:?} is not a regular file"));
    }
    if meta.len() > MAX_MEDIA_INPUT_BYTES {
        return Err(format!(
            "{path:?} is {} bytes, exceeding the {} byte MCP media limit",
            meta.len(),
            MAX_MEDIA_INPUT_BYTES
        ));
    }
    std::fs::read(file).map_err(|e| format!("cannot read {path:?}: {e}"))
}

fn decode_media_base64(data: &str) -> Result<Vec<u8>, String> {
    // Tolerate whitespace-wrapped base64 from clients; the encoded
    // length is checked before decoding so a huge payload never
    // allocates.
    let compact: String = data.chars().filter(|c| !c.is_whitespace()).collect();
    if compact.len() > MAX_MEDIA_BASE64_CHARS {
        return Err(format!(
            "base64 payload is {} chars, exceeding the {} char limit",
            compact.len(),
            MAX_MEDIA_BASE64_CHARS
        ));
    }
    let bytes = base64::engine::general_purpose::STANDARD
        .decode(&compact)
        .map_err(|e| format!("invalid base64 media input: {e}"))?;
    if bytes.len() as u64 > MAX_MEDIA_INPUT_BYTES {
        return Err(format!(
            "decoded media is {} bytes, exceeding the {} byte MCP media limit",
            bytes.len(),
            MAX_MEDIA_INPUT_BYTES
        ));
    }
    Ok(bytes)
}

/// Map a video-engine failure onto a caller-visible tool error,
/// mirroring `api::media::map_video_error`: client-correctable input
/// problems keep their message; internal failures are logged and
/// reported generically so paths/tool details do not leak (spec §12.11).
fn map_video_error(e: VideoError) -> CallToolResult {
    match e {
        VideoError::InvalidInput(msg) | VideoError::LimitExceeded(msg) => {
            tool_error("invalid_input", msg)
        }
        other => {
            warn!(error = %other, "MCP media engine failure");
            tool_error("engine_error", "media engine failure")
        }
    }
}

fn map_image_error(e: ImageError) -> CallToolResult {
    match e {
        ImageError::InvalidInput(msg) | ImageError::LimitExceeded(msg) => {
            tool_error("invalid_input", msg)
        }
        other => {
            warn!(error = %other, "MCP image engine failure");
            tool_error("engine_error", "image engine failure")
        }
    }
}

// ---------------------------------------------------------------------------
// Health checks (read-only; mirrors `cli::command::doctor` semantics)
// ---------------------------------------------------------------------------

#[derive(Debug, serde::Serialize)]
struct HealthCheck {
    name: &'static str,
    /// "ok" | "warn" | "fail"
    status: &'static str,
    detail: String,
}

impl McpServer {
    fn check_platform(&self) -> HealthCheck {
        HealthCheck {
            name: "platform",
            status: "ok",
            detail: platform().describe(),
        }
    }

    fn check_requirements(&self) -> HealthCheck {
        // Spawns `<tool> --version` subprocesses — run off the async
        // executor's critical path is unnecessary here (tool calls are
        // already serialized per client in practice), but keep it cheap.
        let report = require::check_requirements();
        if report.missing.is_empty() {
            HealthCheck {
                name: "dependencies",
                status: "ok",
                detail: format!("{} checked, all present", report.dependencies.len()),
            }
        } else {
            HealthCheck {
                name: "dependencies",
                status: "fail",
                detail: format!("missing: {}", report.missing.join(", ")),
            }
        }
    }

    fn check_config(&self) -> HealthCheck {
        match self.load_config_lenient() {
            Ok(cfg) => {
                let source = self
                    .config_path
                    .as_ref()
                    .map(|p| p.display().to_string())
                    .unwrap_or_else(|| "defaults + env".to_string());
                HealthCheck {
                    name: "config",
                    status: "ok",
                    detail: format!("loaded ({source}); db path {}", cfg.database.path.display()),
                }
            }
            Err(e) => HealthCheck {
                name: "config",
                status: "fail",
                detail: e,
            },
        }
    }

    fn check_secrets(&self) -> HealthCheck {
        match self.load_config_lenient() {
            Ok(cfg) => {
                let mut missing = Vec::new();
                if cfg.secret_key.is_empty() {
                    missing.push("APP_SECRET_KEY");
                }
                if cfg.db_user.is_empty() {
                    missing.push("APP_DB_USER");
                }
                if cfg.db_password.is_empty() {
                    missing.push("APP_DB_PASSWORD");
                }
                if missing.is_empty() {
                    HealthCheck {
                        name: "secrets",
                        status: "ok",
                        detail: "required env vars present".to_string(),
                    }
                } else {
                    HealthCheck {
                        name: "secrets",
                        status: "fail",
                        detail: format!("missing: {}", missing.join(", ")),
                    }
                }
            }
            Err(e) => HealthCheck {
                name: "secrets",
                status: "fail",
                detail: e,
            },
        }
    }

    async fn check_database(&self) -> HealthCheck {
        let db = match self.db_config() {
            Ok(db) => db,
            Err(e) => {
                return HealthCheck {
                    name: "database",
                    status: "fail",
                    detail: e,
                };
            }
        };

        // open_db only — health must not create/migrate schema.
        match init_db::open_db(&db).await {
            Ok(pool) => {
                let tables_ok = sqlx::query_scalar::<_, i64>(
                    "SELECT COUNT(*) FROM sqlite_master
                     WHERE type='table' AND name IN ('users_tbl','sessions_tbl')",
                )
                .fetch_one(&pool)
                .await
                .map(|n| n == 2)
                .unwrap_or(false);
                pool.close().await;

                if tables_ok {
                    HealthCheck {
                        name: "database",
                        status: "ok",
                        detail: format!("{} reachable, schema present", db.db_path.display()),
                    }
                } else {
                    HealthCheck {
                        name: "database",
                        status: "warn",
                        detail: format!(
                            "{} reachable but users_tbl/sessions_tbl missing — run `kaladoc init`",
                            db.db_path.display()
                        ),
                    }
                }
            }
            Err(e) => HealthCheck {
                name: "database",
                status: "fail",
                detail: format!("{}: {e}", db.db_path.display()),
            },
        }
    }

    fn check_storage(&self) -> HealthCheck {
        let cfg = match self.load_config_lenient() {
            Ok(c) => c.to_storage_config(),
            Err(e) => {
                return HealthCheck {
                    name: "storage",
                    status: "fail",
                    detail: e,
                };
            }
        };

        let root = &cfg.root_dir;
        if let Err(e) = std::fs::create_dir_all(root) {
            return HealthCheck {
                name: "storage",
                status: "fail",
                detail: format!("cannot create {}: {e}", root.display()),
            };
        }

        // Write-probe: create and remove a dotfile inside the root —
        // same contract as `kaladoc doctor`.
        let probe = root.join(".kaladoc_mcp_health_probe");
        match std::fs::write(&probe, b"") {
            Ok(()) => {
                let _ = std::fs::remove_file(&probe);
                HealthCheck {
                    name: "storage",
                    status: "ok",
                    detail: format!("{} writable", root.display()),
                }
            }
            Err(e) => HealthCheck {
                name: "storage",
                status: "fail",
                detail: format!("{} not writable: {e}", root.display()),
            },
        }
    }
}

// ---------------------------------------------------------------------------
// Tools
// ---------------------------------------------------------------------------

#[tool_router]
impl McpServer {
    /// Product version, build, transport, and the advertised tool and
    /// engine lists. Safe to call before `notifications/initialized`.
    #[tool(
        name = "kaladoc.system_info",
        annotations(read_only_hint = true, open_world_hint = false)
    )]
    async fn system_info(&self) -> Result<CallToolResult, ErrorData> {
        let tool_names: Vec<String> = Self::tool_router()
            .list_all()
            .iter()
            .map(|t| t.name.to_string())
            .collect();
        let engines: Vec<String> = self.engines.list().iter().map(|e| e.name.clone()).collect();
        Ok(ok_result(
            json!({
                "name": "kaladoc",
                "version": env!("CARGO_PKG_VERSION"),
                "build": if cfg!(debug_assertions) { "debug" } else { "release" },
                "protocol": "MCP",
                "transport": "stdio",
                "tools": tool_names,
                "engines": engines,
            }),
            format!("KalaDoc {} (MCP over stdio)", env!("CARGO_PKG_VERSION")),
        ))
    }

    /// Read-only health summary: platform, dependencies, configuration,
    /// required secrets (names only — never values), database
    /// reachability, storage writability, and per-engine health.
    #[tool(
        name = "kaladoc.system_health",
        annotations(read_only_hint = true, open_world_hint = false)
    )]
    async fn system_health(&self) -> Result<CallToolResult, ErrorData> {
        let mut checks = vec![
            self.check_platform(),
            self.check_requirements(),
            self.check_config(),
            self.check_secrets(),
        ];
        checks.push(self.check_database().await);
        checks.push(self.check_storage());

        let engine_health: Vec<EngineHealthEntry> = self.engines.health_all().await;

        let overall = if checks.iter().any(|c| c.status == "fail") {
            "fail"
        } else if checks.iter().any(|c| c.status == "warn")
            || engine_health.iter().any(|e| {
                !matches!(
                    e.status,
                    crate::crates::core::engine::EngineHealthStatus::Healthy
                )
            })
        {
            "warn"
        } else {
            "ok"
        };

        Ok(ok_result(
            json!({
                "overall": overall,
                "checks": checks,
                "engines": engine_health,
            }),
            format!("system health: {overall}"),
        ))
    }

    /// List the built-in processing engines with advertised operations
    /// and formats.
    #[tool(
        name = "kaladoc.engine_list",
        annotations(read_only_hint = true, open_world_hint = false)
    )]
    async fn engine_list(&self) -> Result<CallToolResult, ErrorData> {
        let engines: Vec<EngineInfo> = self.engines.list();
        let count = engines.len();
        Ok(ok_result(
            json!({ "engines": engines }),
            format!("{count} engine(s) registered"),
        ))
    }

    /// Detailed capabilities for one engine: operations, input/output
    /// formats, and resource limits.
    #[tool(
        name = "kaladoc.engine_capabilities",
        annotations(read_only_hint = true, open_world_hint = false)
    )]
    async fn engine_capabilities(
        &self,
        params: Parameters<EngineCapabilitiesArgs>,
    ) -> Result<CallToolResult, ErrorData> {
        let name = params.0.name.trim();
        if name.is_empty() {
            return Ok(tool_error("invalid_input", "`name` must not be empty"));
        }
        let engine = match self.engines.get(name) {
            Some(engine) => engine,
            None => {
                let available: Vec<String> =
                    self.engines.list().iter().map(|e| e.name.clone()).collect();
                return Ok(tool_error(
                    "not_found",
                    format!(
                        "engine {name:?} is not registered; available: {}",
                        available.join(", ")
                    ),
                ));
            }
        };
        let caps = engine.capabilities();
        let value = json!({
            "name": engine.name(),
            "version": engine.version(),
            "kind": engine.kind(),
            "operations": caps.operations,
            "input_formats": caps.input_formats,
            "output_formats": caps.output_formats,
            "limits": {
                "max_input_bytes": caps.limits.max_input_bytes,
                "max_pages": caps.limits.max_pages,
                "max_pixels": caps.limits.max_pixels,
                "max_outputs": caps.limits.max_outputs,
                "max_duration_secs": caps.limits.max_duration_secs,
            },
        });
        Ok(ok_result(
            value,
            format!("capabilities for engine {name:?}"),
        ))
    }

    /// Probe a media file (video/audio container) with ffprobe:
    /// duration, dimensions, codecs, frame rate, format, size, stream
    /// inventory, and chapters. Input is a local `path` or base64
    /// `base64`, max 256 MiB.
    #[tool(
        name = "kaladoc.media_probe",
        annotations(read_only_hint = true, open_world_hint = true)
    )]
    async fn media_probe(
        &self,
        params: Parameters<MediaProbeArgs>,
    ) -> Result<CallToolResult, ErrorData> {
        let args = params.0;
        let input = match load_media_input(args.path.as_deref(), args.base64.as_deref()) {
            Ok(bytes) => bytes,
            Err(msg) => return Ok(tool_error("invalid_input", msg)),
        };
        let video = self.video;
        let info = match spawn_blocking(move || video.probe(&input)).await {
            Err(e) => return Err(internal_error(format!("probe task failed: {e}"))),
            Ok(Err(e)) => return Ok(map_video_error(e)),
            Ok(Ok(info)) => info,
        };
        let value = serde_json::to_value(&info)
            .map_err(|e| internal_error(format!("probe result serialization failed: {e}")))?;
        let summary = match (info.duration_secs, info.width, info.height) {
            (Some(d), Some(w), Some(h)) => {
                format!("probed media: {w}x{h}, {d:.2}s")
            }
            _ => "probed media".to_string(),
        };
        Ok(ok_result(value, summary))
    }

    /// Extract a PNG thumbnail frame from a media file at `at`
    /// seconds (engine default ~10% of duration). Optionally
    /// downscales to `width` pixels. Returns the PNG inline as
    /// base64 in `structuredContent.png_base64`.
    #[tool(
        name = "kaladoc.media_thumbnail",
        annotations(read_only_hint = true, open_world_hint = true)
    )]
    async fn media_thumbnail(
        &self,
        params: Parameters<MediaThumbnailArgs>,
    ) -> Result<CallToolResult, ErrorData> {
        let args = params.0;
        if let Some(at) = args.at
            && (!at.is_finite() || at < 0.0)
        {
            return Ok(tool_error(
                "invalid_input",
                "`at` must be a finite, non-negative number of seconds",
            ));
        }
        if let Some(w) = args.width
            && w == 0
        {
            return Ok(tool_error("invalid_input", "`width` must be > 0"));
        }
        let input = match load_media_input(args.path.as_deref(), args.base64.as_deref()) {
            Ok(bytes) => bytes,
            Err(msg) => return Ok(tool_error("invalid_input", msg)),
        };

        let video = self.video;
        let limits = media_limits();
        let at = args.at;
        let mut png = match spawn_blocking(move || video.thumbnail(&input, at, &limits)).await {
            Err(e) => return Err(internal_error(format!("thumbnail task failed: {e}"))),
            Ok(Err(e)) => return Ok(map_video_error(e)),
            Ok(Ok(png)) => png,
        };

        // Optional downscale, same as `POST /media/thumbnail?width=`.
        if let Some(width) = args.width {
            let images = self.images.clone();
            png = match spawn_blocking(move || images.resize_to(&png, Some(width), None)).await {
                Err(e) => return Err(internal_error(format!("resize task failed: {e}"))),
                Ok(Err(e)) => return Ok(map_image_error(e)),
                Ok(Ok(png)) => png,
            };
        }

        if png.len() > MAX_THUMBNAIL_BYTES {
            return Ok(tool_error(
                "limit_exceeded",
                format!(
                    "thumbnail is {} bytes, exceeding the {} byte inline limit",
                    png.len(),
                    MAX_THUMBNAIL_BYTES
                ),
            ));
        }

        // Best-effort dimensions for the response; inspection failure
        // must not fail the tool — the PNG bytes are still valid.
        let (out_w, out_h) = {
            let images = self.images.clone();
            let probe = png.clone();
            spawn_blocking(move || images.inspect(&probe).ok())
                .await
                .ok()
                .flatten()
                .map(|info| (info.width, info.height))
                .unwrap_or((0, 0))
        };

        let value = json!({
            "format": "png",
            "width": out_w,
            "height": out_h,
            "size_bytes": png.len(),
            "png_base64": base64::engine::general_purpose::STANDARD.encode(&png),
        });
        Ok(ok_result(
            value,
            format!("thumbnail: {}x{}, {} bytes PNG", out_w, out_h, png.len()),
        ))
    }
}

#[tool_handler(
    name = "kaladoc",
    instructions = "KalaDoc document platform tools: system discovery, engine capabilities, and media inspection. All tools are read-only."
)]
impl ServerHandler for McpServer {}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::Value;

    fn server() -> McpServer {
        McpServer::new(None)
    }

    /// `tools/list` equivalent: the router must advertise exactly the
    /// documented starter tool set with JSON-Schema inputs.
    #[test]
    fn tool_router_lists_expected_tools() {
        let router = McpServer::tool_router();
        let names: Vec<String> = router
            .list_all()
            .iter()
            .map(|t| t.name.to_string())
            .collect();
        assert_eq!(
            names,
            vec![
                "kaladoc.engine_capabilities",
                "kaladoc.engine_list",
                "kaladoc.media_probe",
                "kaladoc.media_thumbnail",
                "kaladoc.system_health",
                "kaladoc.system_info",
            ]
        );
        for tool in router.list_all() {
            assert!(
                !tool.input_schema.is_empty(),
                "tool {} must carry a JSON Schema input",
                tool.name
            );
        }
    }

    #[tokio::test]
    async fn system_info_returns_structured_content() {
        let result = server().system_info().await.unwrap();
        assert_eq!(result.is_error, Some(false));
        let structured = result.structured_content.expect("structuredContent");
        assert_eq!(structured["name"], "kaladoc");
        assert_eq!(structured["transport"], "stdio");
        assert_eq!(structured["tools"].as_array().unwrap().len(), 6);
        assert!(!result.content.is_empty(), "text content required");
    }

    #[tokio::test]
    async fn engine_capabilities_rejects_unknown_engine() {
        let result = server()
            .engine_capabilities(Parameters(EngineCapabilitiesArgs {
                name: "no-such-engine".to_string(),
            }))
            .await
            .unwrap();
        assert_eq!(result.is_error, Some(true));
        assert_eq!(
            result.structured_content.unwrap()["error"]["code"],
            "not_found"
        );
    }

    #[tokio::test]
    async fn engine_capabilities_returns_video_caps() {
        let result = server()
            .engine_capabilities(Parameters(EngineCapabilitiesArgs {
                name: "video".to_string(),
            }))
            .await
            .unwrap();
        assert_eq!(result.is_error, Some(false));
        let structured = result.structured_content.unwrap();
        assert_eq!(structured["name"], "video");
        assert!(structured["operations"].as_array().unwrap().len() > 0);
    }

    #[test]
    fn media_input_requires_exactly_one_source() {
        assert!(load_media_input(None, None).is_err());
        assert!(load_media_input(Some("/tmp/x"), Some("aGk=")).is_err());
        assert!(load_media_input(Some("   "), None).is_err());
    }

    #[test]
    fn media_base64_roundtrip_and_garbage() {
        let bytes = load_media_input(None, Some("aGVsbG8=")).unwrap();
        assert_eq!(bytes, b"hello");
        // Whitespace-wrapped payloads are tolerated.
        let bytes = load_media_input(None, Some("aGVs\nbG8=")).unwrap();
        assert_eq!(bytes, b"hello");
        assert!(load_media_input(None, Some("!!!not-base64!!!")).is_err());
    }

    /// End-to-end JSON-RPC smoke test over an in-memory duplex
    /// transport: `initialize`, `notifications/initialized`,
    /// `tools/list`, `ping`, and one `tools/call`.
    #[tokio::test]
    async fn jsonrpc_handshake_and_tool_call_over_stdio_transport() {
        use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader, duplex};

        let (client_io, server_io) = duplex(256 * 1024);
        let (reader, writer) = tokio::io::split(server_io);
        let server = McpServer::new(None);
        let running = tokio::spawn(async move {
            rmcp::serve_server(server, (reader, writer))
                .await
                .expect("serve_server")
                .waiting()
                .await
        });

        let (client_read, mut client_write) = tokio::io::split(client_io);
        let mut lines = BufReader::new(client_read).lines();

        async fn send(write: &mut tokio::io::WriteHalf<tokio::io::DuplexStream>, msg: Value) {
            let mut line = serde_json::to_vec(&msg).unwrap();
            line.push(b'\n');
            write.write_all(&line).await.unwrap();
            write.flush().await.unwrap();
        }
        async fn recv(
            lines: &mut tokio::io::Lines<BufReader<tokio::io::ReadHalf<tokio::io::DuplexStream>>>,
        ) -> Value {
            let line = lines.next_line().await.unwrap().expect("response line");
            serde_json::from_str(&line).expect("json-rpc response")
        }

        send(
            &mut client_write,
            json!({
                "jsonrpc": "2.0", "id": 1, "method": "initialize",
                "params": {
                    "protocolVersion": "2025-06-18",
                    "capabilities": {},
                    "clientInfo": {"name": "test", "version": "0"}
                }
            }),
        )
        .await;
        let init = recv(&mut lines).await;
        assert_eq!(init["id"], 1);
        assert!(init["result"]["serverInfo"]["name"].is_string());
        assert!(init["result"]["capabilities"]["tools"].is_object());

        send(
            &mut client_write,
            json!({"jsonrpc": "2.0", "method": "notifications/initialized"}),
        )
        .await;

        send(
            &mut client_write,
            json!({"jsonrpc": "2.0", "id": 2, "method": "tools/list", "params": {}}),
        )
        .await;
        let list = recv(&mut lines).await;
        let tools = list["result"]["tools"].as_array().unwrap();
        assert_eq!(tools.len(), 6);
        assert!(tools.iter().any(|t| t["name"] == "kaladoc.system_info"));

        send(
            &mut client_write,
            json!({"jsonrpc": "2.0", "id": 3, "method": "ping"}),
        )
        .await;
        let pong = recv(&mut lines).await;
        assert_eq!(pong["id"], 3);
        assert!(pong["result"].is_object());

        send(
            &mut client_write,
            json!({
                "jsonrpc": "2.0", "id": 4, "method": "tools/call",
                "params": {
                    "name": "kaladoc.engine_list",
                    "arguments": {}
                }
            }),
        )
        .await;
        let call = recv(&mut lines).await;
        assert_eq!(call["id"], 4);
        let result = &call["result"];
        assert_eq!(result["isError"], false);
        assert!(
            result["structuredContent"]["engines"]
                .as_array()
                .unwrap()
                .len()
                >= 6
        );

        drop(client_write);
        drop(lines);
        let _ = tokio::time::timeout(std::time::Duration::from_secs(5), running).await;
    }
}
