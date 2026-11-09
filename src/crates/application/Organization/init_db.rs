use sqlx::sqlite::{SqliteConnectOptions, SqliteJournalMode, SqlitePoolOptions, SqliteSynchronous};
use sqlx::{ConnectOptions, Executor, SqlitePool};
use std::path::PathBuf;
use std::time::Duration;

// ---------------------------------------------------------------------------
// Config
// ---------------------------------------------------------------------------

#[derive(Debug, Clone)]
pub struct DbConfig {
    pub db_path: PathBuf,
    pub secret_key: String,
    pub user: String,
    pub password: String,
    pub max_connections: u32,
    pub acquire_timeout: Duration,
    pub session: SessionConfig,
}

/// Session lifecycle settings, loaded from the environment alongside
/// [`DbConfig`].
///
/// | Variable                        | Default | Meaning                                            |
/// |---------------------------------|---------|----------------------------------------------------|
/// | `SESSION_TTL_SECS`              | `86400` | Session lifetime in seconds (24 h).                |
/// | `SESSION_ABSOLUTE_TTL_SECS`     | `0`     | Max total session age; `0` disables the cap.       |
/// | `SESSION_RENEW_ON_USE`          | `true`  | Slide `expires_at` forward on each use.            |
/// | `SESSION_TOUCH_INTERVAL_SECS`   | `60`    | Min seconds between `last_seen_at` DB writes.      |
/// | `SESSION_CLEANUP_INTERVAL_SECS` | `3600`  | How often expired sessions are deleted.            |
#[derive(Debug, Clone)]
pub struct SessionConfig {
    /// How long a session stays valid after creation (or last use, when
    /// `renew_on_use` is enabled).
    pub ttl: Duration,
    /// Hard cap on total session age measured from `created_at`, enforced
    /// even when `renew_on_use` slides `expires_at`. `Duration::ZERO`
    /// disables the cap.
    pub absolute_ttl: Duration,
    /// When `true`, every successful authentication pushes `expires_at`
    /// forward by `ttl` (sliding expiration). When `false`, sessions
    /// expire at a fixed absolute time.
    pub renew_on_use: bool,
    /// Minimum interval between `last_seen_at` writes during validation.
    /// Validation is read-only when neither this interval elapsed nor the
    /// expiry needs extending — this keeps the hot path off SQLite's
    /// single writer. `Duration::ZERO` writes on every validation.
    pub touch_interval: Duration,
    /// Interval for the background task that deletes expired sessions.
    pub cleanup_interval: Duration,
}

/// Loads application configuration (file + environment, see
/// [`crate::config::AppConfig`]) and returns the database portion.
pub fn load_config() -> Result<DbConfig, crate::config::ConfigError> {
    crate::config::AppConfig::load().map(|c| c.to_db_config())
}

// ---------------------------------------------------------------------------
// Pool init
// ---------------------------------------------------------------------------

/// Opens (and creates if missing) the SQLite database, runs migrations,
/// and returns a configured pool. Loads configuration via
/// [`load_config`].
pub async fn init_db() -> Result<SqlitePool, DbError> {
    let config = load_config()?;
    init_db_with(&config).await
}

/// Same as [`init_db`] but takes an explicit [`DbConfig`] — used when the
/// caller already resolved configuration (e.g. via
/// [`crate::config::AppConfig`]).
pub async fn init_db_with(config: &DbConfig) -> Result<SqlitePool, DbError> {
    let pool = open_db(config).await?;
    create_users_table(&pool).await?;
    create_sessions_table(&pool).await?;
    crate::crates::job_engine::create_jobs_table(&pool).await?;
    crate::crates::document_engine::store::schema::init_document_store(&pool).await?;
    Ok(pool)
}

/// Opens a pool against `config.db_path`, creating the file if absent.
///
/// Uses tuned connect options:
/// - WAL journal (concurrent readers + one writer)
/// - NORMAL synchronous (safe with WAL, much faster than FULL)
/// - foreign keys ON
/// - 5 s busy timeout (instead of immediate `SQLITE_BUSY`)
/// - statement cache
pub async fn open_db(config: &DbConfig) -> Result<SqlitePool, DbError> {
    // Build a proper URL. `SqliteConnectOptions::from_str` accepts
    // `sqlite://<path>`; for absolute paths we can pass the filename
    // form `sqlite:///abs/path` via `new()` instead.
    let options = SqliteConnectOptions::new()
        .filename(&config.db_path)
        .create_if_missing(true)
        .journal_mode(SqliteJournalMode::Wal)
        .synchronous(SqliteSynchronous::Normal)
        .foreign_keys(true)
        .busy_timeout(Duration::from_secs(5))
        .statement_cache_capacity(64)
        .optimize_on_close(true, None);

    // Log slow queries at debug level; remove for production if noisy.
    let options = options.log_statements(tracing::log::LevelFilter::Debug);

    let pool = SqlitePoolOptions::new()
        .max_connections(config.max_connections)
        .acquire_timeout(config.acquire_timeout)
        .min_connections(1)
        .idle_timeout(Some(Duration::from_secs(300)))
        .connect_with(options)
        .await?;

    // Sanity probe so callers fail fast if the file is locked/invalid.
    sqlx::query("SELECT 1").execute(&pool).await?;

    Ok(pool)
}

// ---------------------------------------------------------------------------
// Schema
// ---------------------------------------------------------------------------

/// Creates the users table and its indexes. Idempotent.
pub async fn create_users_table(pool: &SqlitePool) -> Result<(), DbError> {
    // `IF NOT EXISTS` makes this safe to call on every startup.
    // `COLLATE NOCASE` on email/user_name gives case-insensitive uniqueness.
    pool.execute(
        r#"
        CREATE TABLE IF NOT EXISTS users_tbl (
            id          INTEGER PRIMARY KEY AUTOINCREMENT,
            first_name  TEXT NOT NULL,
            last_name   TEXT NOT NULL,
            user_name   TEXT NOT NULL UNIQUE COLLATE NOCASE,
            email       TEXT NOT NULL UNIQUE COLLATE NOCASE,
            password_hash TEXT NOT NULL
        );
        "#,
    )
    .await?;

    // Indexes for lookup-by-email / username (UNIQUE already creates one,
    // but keeping this explicit for clarity if constraints change).
    pool.execute("CREATE INDEX IF NOT EXISTS idx_users_email ON users_tbl(email);")
        .await?;

    Ok(())
}

/// Creates the sessions table and its indexes. Idempotent.
///
/// `token_hash` stores `SHA-256(token)` — never the raw token — so a
/// database leak does not hand out usable session credentials. Timestamps
/// are Unix seconds (`INTEGER`). `ON DELETE CASCADE` removes a user's
/// sessions when the user row is deleted (requires `PRAGMA foreign_keys`,
/// which [`open_db`] enables).
pub async fn create_sessions_table(pool: &SqlitePool) -> Result<(), DbError> {
    pool.execute(
        r#"
        CREATE TABLE IF NOT EXISTS sessions_tbl (
            id            INTEGER PRIMARY KEY AUTOINCREMENT,
            user_id       INTEGER NOT NULL REFERENCES users_tbl(id) ON DELETE CASCADE,
            token_hash    TEXT NOT NULL UNIQUE,
            created_at    INTEGER NOT NULL,
            expires_at    INTEGER NOT NULL,
            last_seen_at  INTEGER NOT NULL,
            revoked_at    INTEGER
        );
        "#,
    )
    .await?;

    pool.execute("CREATE INDEX IF NOT EXISTS idx_sessions_user ON sessions_tbl(user_id);")
        .await?;
    pool.execute("CREATE INDEX IF NOT EXISTS idx_sessions_expires ON sessions_tbl(expires_at);")
        .await?;

    Ok(())
}

// ---------------------------------------------------------------------------
// Errors
// ---------------------------------------------------------------------------

#[derive(Debug, thiserror::Error)]
pub enum DbError {
    #[error(transparent)]
    Config(#[from] crate::config::ConfigError),
    #[error(transparent)]
    Sqlx(#[from] sqlx::Error),
    #[error(transparent)]
    Jobs(#[from] crate::crates::job_engine::JobError),
}
