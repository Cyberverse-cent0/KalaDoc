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
//! ```

use serde::Deserialize;
use std::path::{Path, PathBuf};
use std::time::Duration;

use crate::crates::application::Organization::init_db::{DbConfig, SessionConfig};
use crate::crates::storage::local::config::StorageConfig;

/// Fully resolved application configuration.
#[derive(Debug, Clone)]
pub struct AppConfig {
    pub database: DatabaseSection,
    pub storage: StorageSection,
    pub runtime: RuntimeSection,
    pub jobs: JobsSection,
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
    /// A `running` job older than this is reclaimed as `pending`.
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
}

impl Default for AppConfig {
    fn default() -> Self {
        Self {
            database: DatabaseSection::default(),
            storage: StorageSection::default(),
            runtime: RuntimeSection::default(),
            jobs: JobsSection::default(),
            secret_key: String::new(),
            db_user: String::new(),
            db_password: String::new(),
            config_path: None,
        }
    }
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
            return Err(bad(
                "database.acquire_timeout_secs",
                "must be >= 1 second",
            ));
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
        if let Some(max) = self.storage.max_size_bytes {
            if max == 0 {
                return Err(bad(
                    "storage.max_size_bytes",
                    "0 would reject every write; omit it for no quota",
                ));
            }
        }
        if let Some(fs) = &self.storage.filesystem {
            if fs.is_empty() || fs.contains('/') || fs.contains('\\') {
                return Err(bad(
                    "storage.filesystem",
                    "must be a filesystem name, not a path",
                ));
            }
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
