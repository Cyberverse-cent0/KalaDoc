//! Global application configuration.
//!
//! Configuration is resolved in layers (later wins):
//!
//! 1. Built-in defaults
//! 2. Optional TOML file — the path comes from `KALADOC_CONFIG`, or
//!    `kaladoc.toml` in the current directory when present
//! 3. Environment variables (documented on [`AppConfig::apply_env`])
//!
//! Secrets (`APP_SECRET_KEY`, `APP_DB_USER`, `APP_DB_PASSWORD`) are
//! **environment-only** — they are never read from the config file, so
//! credentials stay out of version control.
//!
//! Some subsystems are configured outside [`AppConfig`] by design:
//!
//! - **Remote (S3) storage** — [`crate::crates::storage::remote::RemoteStorageConfig`]
//!   is populated exclusively from `KALADOC_S3_*` environment variables
//!   (`KALADOC_S3_ENDPOINT`, `KALADOC_S3_BUCKET`, `KALADOC_S3_REGION`,
//!   `KALADOC_S3_PREFIX`, `KALADOC_S3_ACCESS_KEY`, `KALADOC_S3_SECRET_KEY`,
//!   `KALADOC_S3_TIMEOUT_SECS`, `KALADOC_S3_MAX_ATTEMPTS`). Credentials
//!   never come from the config file.
//! - **Multimedia engine** — `ffmpeg`/`ffprobe` are discovered on `PATH`
//!   at first use (`MediaEngine::runtime`); there are no binary-path or
//!   limit keys in `AppConfig` yet. Per-op resource bounds live in
//!   `MediaLimits` (engine construction), not global config.
//! - **Scheduler** — `SchedulerConfig` (tick interval, tick priority,
//!   tick `max_attempts`) currently uses its built-in defaults; the job
//!   worker does not read it from `AppConfig` yet.
//! - **Pipeline engine** — `PipelineLimits` (step/input/total byte
//!   budgets) are carried per `PipelineDefinition`, not globally.
//!
//! # Example `kaladoc.toml`
//! ```toml
//! [database]
//! path = "data/kaladoc.db"
//! max_connections = 8
//! acquire_timeout_secs = 5
//!
//! [database.session]
//! ttl_secs = 86400
//! absolute_ttl_secs = 0        # 0 = no cap on total session age
//! renew_on_use = true
//! touch_interval_secs = 60     # min gap between last_seen_at writes
//! cleanup_interval_secs = 3600
//!
//! [storage]
//! root_dir = "./storage"
//! max_size_bytes = 10737418240   # optional quota
//! filesystem = "ext4"            # optional, for disk preparation
//!
//! [runtime]
//! log_level = "info"
//!
//! [api]
//! addr = "127.0.0.1:8080"   # `kaladoc serve` bind address
//! [jobs]
//! poll_interval_secs = 2
//! ```

use serde::Deserialize;
use std::path::{Path, PathBuf};
use std::time::Duration;

use crate::crates::application::Organization::init_db::{DbConfig, SessionConfig};
use crate::crates::storage::local::config::StorageConfig;

/// Fully resolved application configuration.
#[derive(Debug, Clone, Default)]
pub struct AppConfig {
    pub database: DatabaseSection,
    pub storage: StorageSection,
    pub runtime: RuntimeSection,
    pub jobs: JobsSection,
    pub api: ApiSection,
    /// From `APP_SECRET_KEY` — env only, never from the file.
    pub secret_key: String,
    /// From `APP_DB_USER` — env only.
    pub db_user: String,
    /// From `APP_DB_PASSWORD` — env only.
    pub db_password: String,
    /// The configuration file that was loaded, if any.
    pub config_path: Option<PathBuf>,
}

// ---------------------------------------------------------------------------
// Sections (deserialized from the TOML file)
// ---------------------------------------------------------------------------

#[derive(Debug, Clone, Deserialize)]
#[serde(default)]
pub struct DatabaseSection {
    /// SQLite database file path.
    pub path: PathBuf,
    pub max_connections: u32,
    pub acquire_timeout_secs: u64,
    pub session: SessionSection,
}

impl Default for DatabaseSection {
    fn default() -> Self {
        Self {
            path: PathBuf::from("db.sqlite"),
            max_connections: 8,
            acquire_timeout_secs: 5,
            session: SessionSection::default(),
        }
    }
}

#[derive(Debug, Clone, Deserialize)]
#[serde(default)]
pub struct SessionSection {
    /// Session lifetime in seconds.
    pub ttl_secs: u64,
    /// Hard cap on total session age in seconds; `0` disables it.
    pub absolute_ttl_secs: u64,
    /// Slide `expires_at` forward on each validated use.
    pub renew_on_use: bool,
    /// Minimum seconds between `last_seen_at` writes on validation.
    pub touch_interval_secs: u64,
    /// How often the background task deletes expired sessions.
    pub cleanup_interval_secs: u64,
}

impl Default for SessionSection {
    fn default() -> Self {
        Self {
            ttl_secs: 86_400,
            absolute_ttl_secs: 0,
            renew_on_use: true,
            touch_interval_secs: 60,
            cleanup_interval_secs: 3_600,
        }
    }
}

#[derive(Debug, Clone, Deserialize)]
#[serde(default)]
pub struct StorageSection {
    pub root_dir: PathBuf,
    /// Optional cap on total bytes stored under `root_dir`.
    pub max_size_bytes: Option<u64>,
    /// Filesystem for disk preparation (e.g. "ext4", "ntfs", "apfs").
    pub filesystem: Option<String>,
}

impl Default for StorageSection {
    fn default() -> Self {
        Self {
            root_dir: PathBuf::from("./storage"),
            max_size_bytes: None,
            filesystem: None,
        }
    }
}

#[derive(Debug, Clone, Deserialize)]
#[serde(default)]
pub struct JobsSection {
    /// How often the job engine polls `jobs_tbl` for due work.
    pub poll_interval_secs: u64,
    /// Default retry budget for jobs that don't set `max_attempts`.
    pub default_max_attempts: u32,
    /// Base seconds for exponential retry backoff.
    pub backoff_base_secs: u64,
    /// A `running` job older than this is assumed abandoned (crashed
    /// worker) and returned to `pending` by the engine's reaper. This
    /// is the crash-recovery timeout: at engine startup
    /// [`JobEngine::recover`](crate::crates::job_engine::JobEngine::recover)
    /// requeues all `running` rows immediately, and `reap_stale` then
    /// reclaims any job that stays `running` past this age (e.g. a job
    /// orphaned while this engine was down, or claimed by a dead peer).
    pub stale_after_secs: u64,
}

impl Default for JobsSection {
    fn default() -> Self {
        Self {
            poll_interval_secs: 2,
            default_max_attempts: 3,
            backoff_base_secs: 30,
            stale_after_secs: 600,
        }
    }
}

#[derive(Debug, Clone, Deserialize)]
#[serde(default)]
pub struct ApiSection {
    /// HTTP API bind address (`host:port`).
    pub addr: String,
    /// Static API keys accepted as `Authorization: Bearer <key>`
    /// alongside session tokens (see `crates::api::auth`). Each entry
    /// pairs a `name` — surfaced as the `apikey:<name>` actor — with
    /// the bearer `key` secret. TOML: `[[api.api_keys]]` tables; env:
    /// `KALADOC_API_KEYS` = comma-separated `name:key` pairs.
    pub api_keys: Vec<ApiKey>,
    /// Route groups disabled on the HTTP API (see
    /// `crates::api::flags` for group names: `documents`, `folders`,
    /// `jobs`, `pipelines`, `engines`, `storage`). Disabled groups
    /// answer `404`. TOML: `disabled_groups = [...]` under `[api]`;
    /// env: `KALADOC_API_DISABLE` = comma-separated names.
    pub disabled_groups: Vec<String>,
}

impl Default for ApiSection {
    fn default() -> Self {
        Self {
            addr: "127.0.0.1:8080".to_string(),
            api_keys: Vec::new(),
            disabled_groups: Vec::new(),
        }
    }
}

/// One static API key: `name` identifies the caller (`apikey:<name>`
/// in audit/job payloads); `key` is the bearer secret.
#[derive(Debug, Clone, Deserialize)]
pub struct ApiKey {
    pub name: String,
    pub key: String,
}

#[derive(Debug, Clone, Deserialize)]
#[serde(default)]
pub struct RuntimeSection {
    /// Default `tracing` filter level (also overridable via `RUST_LOG`).
    pub log_level: String,
}

impl Default for RuntimeSection {
    fn default() -> Self {
        Self {
            log_level: "info".to_string(),
        }
    }
}

/// The file-only subset of [`AppConfig`] — secrets are excluded on
/// purpose so a stray `secret_key = "..."` in TOML is ignored.
#[derive(Debug, Default, Deserialize)]
#[serde(default)]
struct FileConfig {
    database: DatabaseSection,
    storage: StorageSection,
    runtime: RuntimeSection,
    jobs: JobsSection,
    api: ApiSection,
}

// ---------------------------------------------------------------------------
// Loading
// ---------------------------------------------------------------------------

impl AppConfig {
    /// Loads configuration: defaults → optional file → environment.
    ///
    /// Returns `Err` when the config file is unreadable/malformed, an env
    /// var has an unparseable value, or a required secret is missing.
    pub fn load() -> Result<Self, ConfigError> {
        Self::load_inner(None, true)
    }

    /// Like [`AppConfig::load`], but reads the TOML file at `path`
    /// (the CLI's `--config` flag) instead of the default resolution.
    /// A missing file is an error.
    pub fn load_with(path: &Path) -> Result<Self, ConfigError> {
        Self::load_inner(Some(path), true)
    }

    /// Lenient variant for diagnostics (`config show`, `doctor`): missing
    /// secret env vars yield empty strings instead of errors, so display
    /// commands work before secrets are configured.
    pub fn load_lenient() -> Result<Self, ConfigError> {
        Self::load_inner(None, false)
    }

    /// [`AppConfig::load_lenient`] with an explicit config file path.
    pub fn load_lenient_with(path: &Path) -> Result<Self, ConfigError> {
        Self::load_inner(Some(path), false)
    }

    fn load_inner(config_path: Option<&Path>, require_secrets: bool) -> Result<Self, ConfigError> {
        // Best-effort .env loading; a missing file is fine.
        let _ = dotenvy::dotenv();

        let mut cfg = AppConfig::default();

        if let Some(path) = config_file_path(config_path)? {
            let text = std::fs::read_to_string(&path).map_err(|source| ConfigError::ReadFile {
                path: path.clone(),
                source,
            })?;
            let file: FileConfig =
                toml::from_str(&text).map_err(|source| ConfigError::ParseFile {
                    path: path.clone(),
                    source,
                })?;
            cfg.database = file.database;
            cfg.storage = file.storage;
            cfg.runtime = file.runtime;
            cfg.jobs = file.jobs;
            cfg.api = file.api;
            cfg.config_path = Some(path);
        }

        cfg.apply_env(require_secrets)?;
        cfg.validate()?;
        Ok(cfg)
    }

    /// Rejects values that would panic or silently misbehave at runtime
    /// (e.g. `tokio::time::interval(Duration::ZERO)` panics).
    fn validate(&self) -> Result<(), ConfigError> {
        let bad = |field: &str, reason: &str| ConfigError::InvalidValue {
            field: field.to_string(),
            reason: reason.to_string(),
        };

        if self.database.max_connections == 0 {
            return Err(bad("database.max_connections", "must be >= 1"));
        }
        if self.database.acquire_timeout_secs == 0 {
            return Err(bad("database.acquire_timeout_secs", "must be >= 1 second"));
        }
        if self.database.session.ttl_secs == 0 {
            return Err(bad("database.session.ttl_secs", "must be >= 1 second"));
        }
        // `tokio::time::interval` panics on a zero period.
        if self.database.session.cleanup_interval_secs == 0 {
            return Err(bad(
                "database.session.cleanup_interval_secs",
                "must be >= 1 second",
            ));
        }
        if let Some(max) = self.storage.max_size_bytes
            && max == 0
        {
            return Err(bad(
                "storage.max_size_bytes",
                "0 would reject every write; omit it for no quota",
            ));
        }
        if let Some(fs) = &self.storage.filesystem
            && (fs.is_empty() || fs.contains('/') || fs.contains('\\'))
        {
            return Err(bad(
                "storage.filesystem",
                "must be a filesystem name, not a path",
            ));
        }
        // Catch typos in log_level here instead of at first log emit.
        if tracing_subscriber::filter::EnvFilter::try_new(&self.runtime.log_level).is_err() {
            return Err(bad(
                "runtime.log_level",
                "is not a valid tracing filter (e.g. \"info\", \"debug\")",
            ));
        }
        // `tokio::time::interval` panics on a zero period.
        if self.jobs.poll_interval_secs == 0 {
            return Err(bad("jobs.poll_interval_secs", "must be >= 1 second"));
        }
        if self.jobs.default_max_attempts == 0 {
            return Err(bad(
                "jobs.default_max_attempts",
                "must be >= 1 (0 would leave every job pending forever)",
            ));
        }
        // 0 would make every `running` job instantly reap-eligible,
        // requeueing live work on every poll.
        if self.jobs.stale_after_secs == 0 {
            return Err(bad(
                "jobs.stale_after_secs",
                "must be >= 1 second (0 would reap every running job immediately)",
            ));
        }
        if self.api.addr.parse::<std::net::SocketAddr>().is_err() {
            return Err(bad(
                "api.addr",
                "must be a valid socket address (e.g. \"127.0.0.1:8080\")",
            ));
        }
        // Static API keys come from env or `[[api.api_keys]]`; reject
        // entries that could never authenticate or would shadow each
        // other.
        {
            let mut seen_names = std::collections::HashSet::new();
            let mut seen_keys = std::collections::HashSet::new();
            for (i, k) in self.api.api_keys.iter().enumerate() {
                let field = format!("api.api_keys[{i}]");
                if k.name.trim().is_empty() {
                    return Err(bad(&field, "name must not be empty"));
                }
                if k.key.is_empty() {
                    return Err(bad(&field, "key must not be empty"));
                }
                if !seen_names.insert(k.name.as_str()) {
                    return Err(bad(&field, "duplicate api key name"));
                }
                if !seen_keys.insert(k.key.as_str()) {
                    return Err(bad(&field, "duplicate api key"));
                }
            }
        }
        // Lenient loads leave secrets empty; only validate when set.
        if !self.secret_key.is_empty() {
            validate_secret_key(&self.secret_key)?;
        }
        Ok(())
    }

    /// Applies environment overrides on top of file/default values.
    ///
    /// | Variable                        | Section            | Field                     |
    /// |---------------------------------|--------------------|---------------------------|
    /// | `DB_PATH`                       | `database`         | `path`                    |
    /// | `DB_MAX_CONNECTIONS`            | `database`         | `max_connections`         |
    /// | `DB_ACQUIRE_TIMEOUT_SECS`       | `database`         | `acquire_timeout_secs`    |
    /// | `SESSION_TTL_SECS`              | `database.session` | `ttl_secs`                |
    /// | `SESSION_ABSOLUTE_TTL_SECS`     | `database.session` | `absolute_ttl_secs`       |
    /// | `SESSION_RENEW_ON_USE`          | `database.session` | `renew_on_use`            |
    /// | `SESSION_TOUCH_INTERVAL_SECS`   | `database.session` | `touch_interval_secs`     |
    /// | `SESSION_CLEANUP_INTERVAL_SECS` | `database.session` | `cleanup_interval_secs`   |
    /// | `KALADOC_STORAGE_DIR`           | `storage`          | `root_dir`                |
    /// | `KALADOC_STORAGE_MAX_BYTES`     | `storage`          | `max_size_bytes`          |
    /// | `KALADOC_FS_TYPE`               | `storage`          | `filesystem`              |
    /// | `KALADOC_LOG_LEVEL`             | `runtime`          | `log_level`               |
    /// | `JOB_POLL_INTERVAL_SECS`        | `jobs`             | `poll_interval_secs`      |
    /// | `JOB_MAX_ATTEMPTS`              | `jobs`             | `default_max_attempts`    |
    /// | `JOB_BACKOFF_BASE_SECS`         | `jobs`             | `backoff_base_secs`       |
    /// | `JOB_STALE_AFTER_SECS`          | `jobs`             | `stale_after_secs`        |
    /// | `JOB_RECOVERY_TIMEOUT_SECS`      | `jobs`             | `stale_after_secs` (alias; applied after `JOB_STALE_AFTER_SECS`, so it wins when both are set) |
    /// | `KALADOC_API_ADDR`               | `api`              | `addr`                     |
    /// | `KALADOC_API_KEYS`               | `api`              | `api_keys` (`name:key,…`)  |
    /// | `KALADOC_API_DISABLE`            | `api`              | `disabled_groups` (CSV)    |
    /// | `APP_SECRET_KEY`                | secret             | `secret_key` (required)   |
    /// | `APP_DB_USER`                   | secret             | `db_user` (required)      |
    /// | `APP_DB_PASSWORD`               | secret             | `db_password` (required)  |
    fn apply_env(&mut self, require_secrets: bool) -> Result<(), ConfigError> {
        if let Some(v) = parse_env::<PathBuf>("DB_PATH")? {
            self.database.path = v;
        }
        if let Some(v) = parse_env::<u32>("DB_MAX_CONNECTIONS")? {
            self.database.max_connections = v;
        }
        if let Some(v) = parse_env::<u64>("DB_ACQUIRE_TIMEOUT_SECS")? {
            self.database.acquire_timeout_secs = v;
        }
        if let Some(v) = parse_env::<u64>("SESSION_TTL_SECS")? {
            self.database.session.ttl_secs = v;
        }
        if let Some(v) = parse_env::<u64>("SESSION_ABSOLUTE_TTL_SECS")? {
            self.database.session.absolute_ttl_secs = v;
        }
        if let Some(v) = parse_env_bool("SESSION_RENEW_ON_USE")? {
            self.database.session.renew_on_use = v;
        }
        if let Some(v) = parse_env::<u64>("SESSION_TOUCH_INTERVAL_SECS")? {
            self.database.session.touch_interval_secs = v;
        }
        if let Some(v) = parse_env::<u64>("SESSION_CLEANUP_INTERVAL_SECS")? {
            self.database.session.cleanup_interval_secs = v;
        }
        if let Some(v) = parse_env::<PathBuf>("KALADOC_STORAGE_DIR")? {
            self.storage.root_dir = v;
        }
        if let Some(v) = parse_env::<u64>("KALADOC_STORAGE_MAX_BYTES")? {
            self.storage.max_size_bytes = Some(v);
        }
        if let Some(v) = parse_env::<String>("KALADOC_FS_TYPE")? {
            let v = v.trim().to_lowercase();
            self.storage.filesystem = (!v.is_empty()).then_some(v);
        }
        if let Some(v) = parse_env::<String>("KALADOC_LOG_LEVEL")? {
            self.runtime.log_level = v;
        }
        if let Some(v) = parse_env::<u64>("JOB_POLL_INTERVAL_SECS")? {
            self.jobs.poll_interval_secs = v;
        }
        if let Some(v) = parse_env::<u32>("JOB_MAX_ATTEMPTS")? {
            self.jobs.default_max_attempts = v;
        }
        if let Some(v) = parse_env::<u64>("JOB_BACKOFF_BASE_SECS")? {
            self.jobs.backoff_base_secs = v;
        }
        if let Some(v) = parse_env::<u64>("JOB_STALE_AFTER_SECS")? {
            self.jobs.stale_after_secs = v;
        }
        // Alias for the crash-recovery timeout; applied second so it
        // takes precedence when both variables are set.
        if let Some(v) = parse_env::<u64>("JOB_RECOVERY_TIMEOUT_SECS")? {
            self.jobs.stale_after_secs = v;
        }
        if let Some(v) = parse_env::<String>("KALADOC_API_ADDR")? {
            self.api.addr = v;
        }
        if let Some(v) = parse_env::<String>("KALADOC_API_KEYS")? {
            self.api.api_keys = parse_api_keys(&v)?;
        }
        if let Some(v) = parse_env::<String>("KALADOC_API_DISABLE")? {
            self.api.disabled_groups = parse_csv(&v);
        }

        self.secret_key = env_secret("APP_SECRET_KEY", require_secrets)?;
        self.db_user = env_secret("APP_DB_USER", require_secrets)?;
        self.db_password = env_secret("APP_DB_PASSWORD", require_secrets)?;

        Ok(())
    }

    /// Converts the database-related sections into a [`DbConfig`].
    pub fn to_db_config(&self) -> DbConfig {
        DbConfig {
            db_path: self.database.path.clone(),
            secret_key: self.secret_key.clone(),
            user: self.db_user.clone(),
            password: self.db_password.clone(),
            max_connections: self.database.max_connections,
            acquire_timeout: Duration::from_secs(self.database.acquire_timeout_secs),
            session: SessionConfig {
                ttl: Duration::from_secs(self.database.session.ttl_secs),
                absolute_ttl: Duration::from_secs(self.database.session.absolute_ttl_secs),
                renew_on_use: self.database.session.renew_on_use,
                touch_interval: Duration::from_secs(self.database.session.touch_interval_secs),
                cleanup_interval: Duration::from_secs(self.database.session.cleanup_interval_secs),
            },
        }
    }

    /// Converts the storage section into a [`StorageConfig`].
    pub fn to_storage_config(&self) -> StorageConfig {
        let mut cfg = StorageConfig::new(self.storage.root_dir.clone());
        if let Some(max) = self.storage.max_size_bytes {
            cfg = cfg.with_quota(max);
        }
        if let Some(fs) = &self.storage.filesystem {
            cfg = cfg.with_filesystem(fs.clone());
        }
        cfg
    }

    /// Converts the jobs section into a [`JobEngineConfig`].
    pub fn to_job_config(&self) -> crate::crates::job_engine::JobEngineConfig {
        crate::crates::job_engine::JobEngineConfig {
            poll_interval: Duration::from_secs(self.jobs.poll_interval_secs),
            default_max_attempts: self.jobs.default_max_attempts,
            backoff_base: Duration::from_secs(self.jobs.backoff_base_secs),
            stale_after: Duration::from_secs(self.jobs.stale_after_secs),
        }
    }
}

// ---------------------------------------------------------------------------
// Internals
// ---------------------------------------------------------------------------

/// Resolves which config file to read, if any.
///
/// An explicit `override_path` (CLI `--config`) wins; it must exist.
/// Otherwise `KALADOC_CONFIG` points at an explicit file (an error if
/// missing); failing that, `kaladoc.toml` in the working directory is
/// used when it exists, and no file is read when it doesn't.
fn config_file_path(override_path: Option<&Path>) -> Result<Option<PathBuf>, ConfigError> {
    if let Some(path) = override_path {
        if !path.is_file() {
            return Err(ConfigError::ReadFile {
                path: path.to_path_buf(),
                source: std::io::Error::new(
                    std::io::ErrorKind::NotFound,
                    "--config path does not exist",
                ),
            });
        }
        return Ok(Some(path.to_path_buf()));
    }

    if let Some(raw) = std::env::var_os("KALADOC_CONFIG") {
        let path = PathBuf::from(raw);
        if !path.is_file() {
            return Err(ConfigError::ReadFile {
                path,
                source: std::io::Error::new(
                    std::io::ErrorKind::NotFound,
                    "KALADOC_CONFIG points to a missing file",
                ),
            });
        }
        return Ok(Some(path));
    }

    let default = PathBuf::from("kaladoc.toml");
    Ok(default.is_file().then_some(default))
}

/// Validates `APP_SECRET_KEY` when one is configured: 32–1024 chars of
/// printable ASCII without whitespace. Short keys are rejected because
/// the key is destined for token signing/HMAC — brute-forceable
/// secrets defeat the purpose.
fn validate_secret_key(key: &str) -> Result<(), ConfigError> {
    let bad = |reason: &str| ConfigError::InvalidValue {
        field: "secret_key (APP_SECRET_KEY)".to_string(),
        reason: reason.to_string(),
    };
    if key.len() < 32 {
        return Err(bad("must be at least 32 characters"));
    }
    if key.len() > 1024 {
        return Err(bad("must be at most 1024 characters"));
    }
    if !key.chars().all(|c| c.is_ascii_graphic()) {
        return Err(bad("must contain only printable ASCII characters"));
    }
    if key.chars().any(char::is_whitespace) {
        return Err(bad("must not contain whitespace"));
    }
    Ok(())
}

/// Parses an optional env var. Empty/unset → `None`; a set-but-invalid
/// value is a hard error so typos don't silently fall back to defaults.
fn parse_env<T>(key: &str) -> Result<Option<T>, ConfigError>
where
    T: std::str::FromStr,
{
    match std::env::var(key) {
        Ok(v) if !v.trim().is_empty() => {
            v.trim()
                .parse()
                .map(Some)
                .map_err(|_| ConfigError::InvalidEnv {
                    key: key.to_string(),
                    value: v,
                })
        }
        _ => Ok(None),
    }
}

/// Like [`parse_env`] but for booleans, accepting the common spellings
/// `true/false/1/0/yes/no/on/off` (case-insensitive).
fn parse_env_bool(key: &str) -> Result<Option<bool>, ConfigError> {
    match std::env::var(key) {
        Ok(v) if !v.trim().is_empty() => {
            let b = match v.trim().to_ascii_lowercase().as_str() {
                "true" | "1" | "yes" | "on" => true,
                "false" | "0" | "no" | "off" => false,
                _ => {
                    return Err(ConfigError::InvalidEnv {
                        key: key.to_string(),
                        value: v,
                    });
                }
            };
            Ok(Some(b))
        }
        _ => Ok(None),
    }
}

/// Parses `KALADOC_API_KEYS`: a comma-separated list of `name:key`
/// pairs. Names/keys may not contain `,` or `:` respectively is NOT
/// enforced — the first `:` splits name from key, so keys may contain
/// colons but names may not. Empty segments are skipped.
fn parse_api_keys(raw: &str) -> Result<Vec<ApiKey>, ConfigError> {
    let mut keys = Vec::new();
    for (i, segment) in raw.split(',').enumerate() {
        let segment = segment.trim();
        if segment.is_empty() {
            continue;
        }
        let Some((name, key)) = segment.split_once(':') else {
            return Err(ConfigError::InvalidEnv {
                key: "KALADOC_API_KEYS".to_string(),
                value: format!("entry {i} ({segment:?}) is not a name:key pair"),
            });
        };
        let (name, key) = (name.trim(), key.trim());
        if name.is_empty() || key.is_empty() {
            return Err(ConfigError::InvalidEnv {
                key: "KALADOC_API_KEYS".to_string(),
                value: format!("entry {i} has an empty name or key"),
            });
        }
        keys.push(ApiKey {
            name: name.to_string(),
            key: key.to_string(),
        });
    }
    Ok(keys)
}

/// Splits a comma-separated env var into trimmed, non-empty entries.
fn parse_csv(raw: &str) -> Vec<String> {
    raw.split(',')
        .map(str::trim)
        .filter(|s| !s.is_empty())
        .map(str::to_string)
        .collect()
}

/// Reads a secret env var. When `required` is false a missing/empty
/// value yields an empty string (diagnostics only — never persisted).
fn env_secret(key: &str, required: bool) -> Result<String, ConfigError> {
    match std::env::var(key) {
        Ok(v) if !v.trim().is_empty() => Ok(v),
        _ if required => Err(ConfigError::MissingEnv(key.to_string())),
        _ => Ok(String::new()),
    }
}

// ---------------------------------------------------------------------------
// Errors
// ---------------------------------------------------------------------------

#[derive(Debug, thiserror::Error)]
pub enum ConfigError {
    #[error("missing required environment variable: {0}")]
    MissingEnv(String),
    #[error("environment variable {key} has invalid value {value:?}")]
    InvalidEnv { key: String, value: String },
    #[error("invalid configuration value for {field}: {reason}")]
    InvalidValue { field: String, reason: String },
    #[error("failed to read config file {}: {source}", path.display())]
    ReadFile {
        path: PathBuf,
        source: std::io::Error,
    },
    #[error("failed to parse config file {}: {source}", path.display())]
    ParseFile {
        path: PathBuf,
        source: toml::de::Error,
    },
}

#[cfg(test)]
mod tests {
    use super::*;

    // ------------------------------------------------------------------
    // Defaults
    // ------------------------------------------------------------------

    #[test]
    fn appconfig_defaults_are_sane() {
        let cfg = AppConfig::default();
        assert_eq!(cfg.database.path, PathBuf::from("db.sqlite"));
        assert_eq!(cfg.database.max_connections, 8);
        assert_eq!(cfg.database.acquire_timeout_secs, 5);
        assert_eq!(cfg.database.session.ttl_secs, 86_400);
        assert_eq!(cfg.database.session.absolute_ttl_secs, 0);
        assert!(cfg.database.session.renew_on_use);
        assert_eq!(cfg.database.session.touch_interval_secs, 60);
        assert_eq!(cfg.database.session.cleanup_interval_secs, 3_600);

        assert_eq!(cfg.storage.root_dir, PathBuf::from("./storage"));
        assert_eq!(cfg.storage.max_size_bytes, None);
        assert_eq!(cfg.storage.filesystem, None);

        assert_eq!(cfg.runtime.log_level, "info");

        assert_eq!(cfg.jobs.poll_interval_secs, 2);
        assert_eq!(cfg.jobs.default_max_attempts, 3);
        assert_eq!(cfg.jobs.backoff_base_secs, 30);
        assert_eq!(cfg.jobs.stale_after_secs, 600);

        assert_eq!(cfg.api.addr, "127.0.0.1:8080");
        assert!(cfg.api.api_keys.is_empty());
        assert!(cfg.api.disabled_groups.is_empty());

        // Secrets are env-only → empty under Default.
        assert_eq!(cfg.secret_key, "");
        assert_eq!(cfg.db_user, "");
        assert_eq!(cfg.db_password, "");
        assert_eq!(cfg.config_path, None);
    }

    #[test]
    fn default_config_passes_validation() {
        AppConfig::default().validate().unwrap();
    }

    // ------------------------------------------------------------------
    // TOML file parsing (serde)
    // ------------------------------------------------------------------

    #[test]
    fn fileconfig_toml_parses_sections_and_api_keys() {
        let text = r#"
[database]
path = "/tmp/kala-test.db"
max_connections = 4

[database.session]
ttl_secs = 3600
renew_on_use = false

[storage]
root_dir = "/tmp/kala-storage"
max_size_bytes = 1024
filesystem = "ext4"

[runtime]
log_level = "debug"

[jobs]
poll_interval_secs = 5
default_max_attempts = 7
backoff_base_secs = 10
stale_after_secs = 120

[api]
addr = "0.0.0.0:9999"
disabled_groups = ["jobs", "pipelines"]

[[api.api_keys]]
name = "ci"
key = "secret-token-1"

[[api.api_keys]]
name = "deploy"
key = "secret-token-2"
"#;
        let file: FileConfig = toml::from_str(text).unwrap();

        assert_eq!(file.database.path, PathBuf::from("/tmp/kala-test.db"));
        assert_eq!(file.database.max_connections, 4);
        assert_eq!(file.database.session.ttl_secs, 3600);
        assert!(!file.database.session.renew_on_use);
        // Unset session fields fall back to serde defaults.
        assert_eq!(file.database.session.cleanup_interval_secs, 3_600);

        assert_eq!(file.storage.max_size_bytes, Some(1024));
        assert_eq!(file.storage.filesystem.as_deref(), Some("ext4"));
        assert_eq!(file.runtime.log_level, "debug");

        assert_eq!(file.jobs.poll_interval_secs, 5);
        assert_eq!(file.jobs.default_max_attempts, 7);
        assert_eq!(file.jobs.stale_after_secs, 120);

        assert_eq!(file.api.addr, "0.0.0.0:9999");
        assert_eq!(file.api.disabled_groups, vec!["jobs", "pipelines"]);
        assert_eq!(file.api.api_keys.len(), 2);
        assert_eq!(file.api.api_keys[0].name, "ci");
        assert_eq!(file.api.api_keys[0].key, "secret-token-1");
        assert_eq!(file.api.api_keys[1].name, "deploy");
    }

    #[test]
    fn fileconfig_empty_toml_uses_defaults() {
        let file: FileConfig = toml::from_str("").unwrap();
        assert_eq!(file.database.max_connections, 8);
        assert_eq!(file.api.addr, "127.0.0.1:8080");
        assert!(file.api.api_keys.is_empty());
    }

    #[test]
    fn fileconfig_rejects_malformed_toml() {
        assert!(toml::from_str::<FileConfig>("[[[not toml").is_err());
        // Wrong type for a known field.
        assert!(toml::from_str::<FileConfig>("database.max_connections = \"lots\"").is_err());
    }

    #[test]
    fn secrets_in_toml_are_ignored() {
        // FileConfig has no secret fields — a stray key must not error
        // (serde ignores unknown fields) nor populate secrets.
        let file: FileConfig =
            toml::from_str("secret_key = \"hunter2\"\napi.addr = \"127.0.0.1:1\"").unwrap();
        assert_eq!(file.api.addr, "127.0.0.1:1");
    }

    // ------------------------------------------------------------------
    // Env-var parsing helpers (pure fns; unique var names avoid races)
    // ------------------------------------------------------------------

    #[test]
    fn parse_env_reads_typed_values() {
        let key = "KALADOC_CFG_TEST_PARSE_NUM_a1b2";
        unsafe { std::env::set_var(key, "42") };
        assert_eq!(parse_env::<u64>(key).unwrap(), Some(42));
        unsafe { std::env::remove_var(key) };
        assert_eq!(parse_env::<u64>(key).unwrap(), None);
    }

    #[test]
    fn parse_env_empty_and_unset_yield_none() {
        let key = "KALADOC_CFG_TEST_PARSE_EMPTY_c3d4";
        unsafe { std::env::set_var(key, "   ") };
        assert_eq!(parse_env::<u64>(key).unwrap(), None);
        unsafe { std::env::remove_var(key) };
    }

    #[test]
    fn parse_env_invalid_value_is_error() {
        let key = "KALADOC_CFG_TEST_PARSE_BAD_e5f6";
        unsafe { std::env::set_var(key, "not-a-number") };
        let err = parse_env::<u64>(key).unwrap_err();
        assert!(matches!(err, ConfigError::InvalidEnv { .. }));
        unsafe { std::env::remove_var(key) };
    }

    #[test]
    fn parse_env_bool_accepts_common_spellings() {
        let key = "KALADOC_CFG_TEST_PARSE_BOOL_g7h8";
        for (raw, want) in [
            ("true", true),
            ("TRUE", true),
            ("1", true),
            ("yes", true),
            ("on", true),
            ("false", false),
            ("0", false),
            ("no", false),
            ("off", false),
        ] {
            unsafe { std::env::set_var(key, raw) };
            assert_eq!(parse_env_bool(key).unwrap(), Some(want), "input {raw:?}");
        }
        unsafe { std::env::set_var(key, "maybe") };
        assert!(matches!(
            parse_env_bool(key).unwrap_err(),
            ConfigError::InvalidEnv { .. }
        ));
        unsafe { std::env::remove_var(key) };
    }

    #[test]
    fn parse_api_keys_valid_formats() {
        let keys = parse_api_keys("ci:tok-1, deploy:tok-2 ,, backup:key:with:colons").unwrap();
        assert_eq!(keys.len(), 3);
        assert_eq!(keys[0].name, "ci");
        assert_eq!(keys[0].key, "tok-1");
        assert_eq!(keys[1].name, "deploy");
        // First ':' splits; the key may itself contain colons.
        assert_eq!(keys[2].name, "backup");
        assert_eq!(keys[2].key, "key:with:colons");

        // Empty input / only separators → empty list.
        assert!(parse_api_keys("").unwrap().is_empty());
        assert!(parse_api_keys(" , ,").unwrap().is_empty());
    }

    #[test]
    fn parse_api_keys_rejects_malformed_entries() {
        // No colon at all.
        assert!(matches!(
            parse_api_keys("just-a-name"),
            Err(ConfigError::InvalidEnv { .. })
        ));
        // Empty name.
        assert!(parse_api_keys(":key").is_err());
        // Empty key.
        assert!(parse_api_keys("name:").is_err());
        // Whitespace-only segments inside a list still error when
        // they carry content but no colon.
        assert!(parse_api_keys("ok:k, badentry").is_err());
    }

    #[test]
    fn parse_csv_trims_and_drops_empties() {
        assert_eq!(
            parse_csv(" jobs , pipelines ,, storage "),
            vec!["jobs", "pipelines", "storage"]
        );
        assert!(parse_csv("").is_empty());
        assert!(parse_csv(" , , ").is_empty());
    }

    // ------------------------------------------------------------------
    // Validation
    // ------------------------------------------------------------------

    #[test]
    fn validate_rejects_zero_intervals() {
        let mut cfg = AppConfig::default();
        cfg.database.max_connections = 0;
        assert!(matches!(
            cfg.validate(),
            Err(ConfigError::InvalidValue { .. })
        ));

        let mut cfg = AppConfig::default();
        cfg.jobs.poll_interval_secs = 0;
        assert!(cfg.validate().is_err());

        let mut cfg = AppConfig::default();
        cfg.jobs.stale_after_secs = 0;
        assert!(cfg.validate().is_err());

        let mut cfg = AppConfig::default();
        cfg.database.session.cleanup_interval_secs = 0;
        assert!(cfg.validate().is_err());
    }

    #[test]
    fn validate_rejects_bad_addr_and_storage() {
        let mut cfg = AppConfig::default();
        cfg.api.addr = "not-an-address".into();
        assert!(cfg.validate().is_err());

        let mut cfg = AppConfig::default();
        cfg.storage.max_size_bytes = Some(0);
        assert!(cfg.validate().is_err());

        let mut cfg = AppConfig::default();
        cfg.storage.filesystem = Some("/etc/passwd".into());
        assert!(cfg.validate().is_err());
    }

    #[test]
    fn validate_rejects_bad_log_level() {
        let mut cfg = AppConfig::default();
        cfg.runtime.log_level = "definitely not a filter {{{".into();
        assert!(cfg.validate().is_err());
    }

    #[test]
    fn validate_rejects_duplicate_or_empty_api_keys() {
        let mut cfg = AppConfig::default();
        cfg.api.api_keys = vec![
            ApiKey {
                name: "a".into(),
                key: "k1".into(),
            },
            ApiKey {
                name: "a".into(),
                key: "k2".into(),
            }, // dup name
        ];
        assert!(cfg.validate().is_err());

        let mut cfg = AppConfig::default();
        cfg.api.api_keys = vec![
            ApiKey {
                name: "a".into(),
                key: "same".into(),
            },
            ApiKey {
                name: "b".into(),
                key: "same".into(),
            }, // dup key
        ];
        assert!(cfg.validate().is_err());

        let mut cfg = AppConfig::default();
        cfg.api.api_keys = vec![ApiKey {
            name: " ".into(),
            key: "k".into(),
        }];
        assert!(cfg.validate().is_err());

        let mut cfg = AppConfig::default();
        cfg.api.api_keys = vec![ApiKey {
            name: "n".into(),
            key: String::new(),
        }];
        assert!(cfg.validate().is_err());
    }

    #[test]
    fn validate_secret_key_rules() {
        // Too short.
        assert!(validate_secret_key(&"x".repeat(31)).is_err());
        // Minimum length, printable ASCII, no whitespace → ok.
        validate_secret_key(&"a".repeat(32)).unwrap();
        // Whitespace rejected even at valid length.
        assert!(validate_secret_key(&format!("{} ", "a".repeat(40))).is_err());
        // Non-ASCII rejected.
        assert!(validate_secret_key(&"é".repeat(40)).is_err());
        // Too long.
        assert!(validate_secret_key(&"a".repeat(1025)).is_err());
        validate_secret_key(&"a".repeat(1024)).unwrap();
    }

    // ------------------------------------------------------------------
    // Config file path resolution
    // ------------------------------------------------------------------

    #[test]
    fn config_file_path_explicit_missing_is_error() {
        let missing = PathBuf::from("/nonexistent/kala-doc-test.toml");
        let err = config_file_path(Some(&missing)).unwrap_err();
        assert!(matches!(err, ConfigError::ReadFile { .. }));
    }

    #[test]
    fn config_file_path_explicit_existing_is_used() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("cfg.toml");
        std::fs::write(&path, "[jobs]\npoll_interval_secs = 9\n").unwrap();
        let resolved = config_file_path(Some(&path)).unwrap().unwrap();
        assert_eq!(resolved, path);
    }

    // ------------------------------------------------------------------
    // Section → subsystem config conversions
    // ------------------------------------------------------------------

    #[test]
    fn to_db_config_maps_sections() {
        let cfg = AppConfig::default();
        let db = cfg.to_db_config();
        assert_eq!(db.db_path, PathBuf::from("db.sqlite"));
        assert_eq!(db.max_connections, 8);
        assert_eq!(db.acquire_timeout, Duration::from_secs(5));
        assert_eq!(db.session.ttl, Duration::from_secs(86_400));
        assert_eq!(db.session.absolute_ttl, Duration::ZERO);
        assert!(db.session.renew_on_use);
        assert_eq!(db.session.touch_interval, Duration::from_secs(60));
        assert_eq!(db.session.cleanup_interval, Duration::from_secs(3_600));
    }

    #[test]
    fn to_job_config_maps_sections() {
        let cfg = AppConfig::default();
        let job = cfg.to_job_config();
        assert_eq!(job.poll_interval, Duration::from_secs(2));
        assert_eq!(job.default_max_attempts, 3);
        assert_eq!(job.backoff_base, Duration::from_secs(30));
        assert_eq!(job.stale_after, Duration::from_secs(600));
    }

    #[test]
    fn to_storage_config_maps_sections() {
        let mut cfg = AppConfig::default();
        cfg.storage.max_size_bytes = Some(4096);
        cfg.storage.filesystem = Some("ext4".into());
        let storage = cfg.to_storage_config();
        assert_eq!(storage.root_dir, PathBuf::from("./storage"));
        assert_eq!(storage.max_size_bytes, Some(4096));
        assert_eq!(storage.filesystem.as_deref(), Some("ext4"));
    }

    #[test]
    fn config_error_display_messages() {
        let err = ConfigError::MissingEnv("APP_SECRET_KEY".into());
        assert!(err.to_string().contains("APP_SECRET_KEY"));
        let err = ConfigError::InvalidEnv {
            key: "K".into(),
            value: "v".into(),
        };
        assert!(err.to_string().contains("K"));
    }
}
