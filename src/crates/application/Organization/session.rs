//! Session lifecycle: creation, validation, renewal, revocation, cleanup.
//!
//! Sessions are bearer tokens. The raw token is returned to the caller
//! **once** at creation; only `SHA-256(token)` (hex) is stored in
//! `sessions_tbl`, so a database leak does not expose usable credentials.
//!
//! All functions take `&SqlitePool` (see [`super::init_db`]) and return
//! [`SessionError`]. Expiry is evaluated at validation time against
//! `SystemTime::now()`; the [`spawn_cleanup_task`] background job deletes
//! expired rows so the table doesn't grow unboundedly.

use base64::{Engine as _, engine::general_purpose::URL_SAFE_NO_PAD};
use sha2::{Digest, Sha256};
use sqlx::sqlite::SqliteRow;
use sqlx::{Row, SqlitePool};
use std::time::{SystemTime, UNIX_EPOCH};

use super::init_db::SessionConfig;

/// Length of the random bearer token in bytes (256 bits of entropy).
const TOKEN_LEN: usize = 32;

/// A row from `sessions_tbl`.
#[derive(Debug, Clone)]
pub struct Session {
    pub id: i64,
    pub user_id: i64,
    /// `SHA-256` hex of the bearer token. Internal — never send to clients.
    pub token_hash: String,
    /// Unix seconds.
    pub created_at: i64,
    /// Unix seconds.
    pub expires_at: i64,
    /// Unix seconds of the most recent successful validation.
    pub last_seen_at: i64,
    /// Unix seconds when revoked, if ever.
    pub revoked_at: Option<i64>,
}

/// Errors from session operations. Currently only database failures —
/// invalid/expired/revoked tokens are reported as `Ok(None)`, not errors.
#[derive(Debug, thiserror::Error)]
pub enum SessionError {
    #[error(transparent)]
    Db(#[from] sqlx::Error),
}

// ---------------------------------------------------------------------------
// Public API
// ---------------------------------------------------------------------------

/// Creates a session for `user_id` and returns it together with the raw
/// bearer token. The token is shown **only here** — store it hashed, hand
/// the raw value to the client once.
pub async fn create_session(
    pool: &SqlitePool,
    user_id: i64,
    config: &SessionConfig,
) -> Result<(Session, String), SessionError> {
    let token = generate_token();
    let token_hash = hash_token(&token);
    let now = now_unix();
    let expires_at = now.saturating_add(config.ttl.as_secs() as i64);

    let id = sqlx::query(
        "INSERT INTO sessions_tbl (user_id, token_hash, created_at, expires_at, last_seen_at)
         VALUES (?, ?, ?, ?, ?)",
    )
    .bind(user_id)
    .bind(&token_hash)
    .bind(now)
    .bind(expires_at)
    .bind(now)
    .execute(pool)
    .await?
    .last_insert_rowid();

    let session = Session {
        id,
        user_id,
        token_hash,
        created_at: now,
        expires_at,
        last_seen_at: now,
        revoked_at: None,
    };
    Ok((session, token))
}

/// Validates a bearer token.
///
/// Returns `Ok(Some(session))` when the token exists, is unrevoked, has
/// not expired, and has not passed `config.absolute_ttl` (when set).
/// Returns `Ok(None)` for unknown, revoked, expired, or absolutely-aged
/// out tokens — callers cannot distinguish them.
///
/// This is the hot path, so it is read-only whenever possible: the
/// `UPDATE` only runs when `last_seen_at` is older than
/// `config.touch_interval` **or** sliding renewal would actually extend
/// `expires_at`. With default settings most validations cost a single
/// indexed `SELECT`.
pub async fn validate_session(
    pool: &SqlitePool,
    token: &str,
    config: &SessionConfig,
) -> Result<Option<Session>, SessionError> {
    let token_hash = hash_token(token);

    let row = sqlx::query(
        "SELECT id, user_id, token_hash, created_at, expires_at, last_seen_at, revoked_at
         FROM sessions_tbl WHERE token_hash = ? AND revoked_at IS NULL",
    )
    .bind(&token_hash)
    .fetch_optional(pool)
    .await?;

    let Some(row) = row else {
        return Ok(None);
    };
    let mut session = session_from_row(row);

    let now = now_unix();
    if session.expires_at <= now {
        return Ok(None);
    }

    let absolute_ttl = config.absolute_ttl.as_secs() as i64;
    if absolute_ttl > 0 && now - session.created_at >= absolute_ttl {
        return Ok(None);
    }

    // Sliding renewal never shortens an existing expiry.
    let new_expiry = if config.renew_on_use {
        (now.saturating_add(config.ttl.as_secs() as i64)).max(session.expires_at)
    } else {
        session.expires_at
    };
    let expiry_changed = new_expiry != session.expires_at;
    let touch_due = now - session.last_seen_at >= config.touch_interval.as_secs() as i64;

    if expiry_changed || touch_due {
        sqlx::query("UPDATE sessions_tbl SET expires_at = ?, last_seen_at = ? WHERE id = ?")
            .bind(new_expiry)
            .bind(now)
            .bind(session.id)
            .execute(pool)
            .await?;
        session.expires_at = new_expiry;
        session.last_seen_at = now;
    }

    Ok(Some(session))
}

/// Revokes the session identified by `token`. Returns `true` when an
/// active session was revoked, `false` for unknown/already-revoked tokens.
pub async fn revoke_session(pool: &SqlitePool, token: &str) -> Result<bool, SessionError> {
    let done = sqlx::query(
        "UPDATE sessions_tbl SET revoked_at = ?
         WHERE token_hash = ? AND revoked_at IS NULL",
    )
    .bind(now_unix())
    .bind(hash_token(token))
    .execute(pool)
    .await?;

    Ok(done.rows_affected() > 0)
}

/// Revokes every active session belonging to `user_id` (e.g. on logout-
/// everywhere or password change). Returns the number revoked.
pub async fn revoke_all_user_sessions(
    pool: &SqlitePool,
    user_id: i64,
) -> Result<u64, SessionError> {
    let done = sqlx::query(
        "UPDATE sessions_tbl SET revoked_at = ?
         WHERE user_id = ? AND revoked_at IS NULL",
    )
    .bind(now_unix())
    .bind(user_id)
    .execute(pool)
    .await?;

    Ok(done.rows_affected())
}

/// Revokes the session with primary key `session_id`. Returns `true`
/// when an active session was revoked.
pub async fn revoke_session_by_id(
    pool: &SqlitePool,
    session_id: i64,
) -> Result<bool, SessionError> {
    let done = sqlx::query(
        "UPDATE sessions_tbl SET revoked_at = ?
         WHERE id = ? AND revoked_at IS NULL",
    )
    .bind(now_unix())
    .bind(session_id)
    .execute(pool)
    .await?;

    Ok(done.rows_affected() > 0)
}

/// Lists a user's sessions that are currently usable: unrevoked and
/// unexpired. Ordered newest first. `token_hash` is included for
/// internal bookkeeping — never expose it to clients.
pub async fn list_active_sessions(
    pool: &SqlitePool,
    user_id: i64,
) -> Result<Vec<Session>, SessionError> {
    let rows = sqlx::query(
        "SELECT id, user_id, token_hash, created_at, expires_at, last_seen_at, revoked_at
         FROM sessions_tbl
         WHERE user_id = ? AND revoked_at IS NULL AND expires_at > ?
         ORDER BY created_at DESC, id DESC",
    )
    .bind(user_id)
    .bind(now_unix())
    .fetch_all(pool)
    .await?;

    Ok(rows.into_iter().map(session_from_row).collect())
}

/// Deletes all sessions whose `expires_at` has passed (revoked sessions
/// are removed here too once they expire). Returns rows deleted.
pub async fn cleanup_expired_sessions(pool: &SqlitePool) -> Result<u64, SessionError> {
    let done = sqlx::query("DELETE FROM sessions_tbl WHERE expires_at <= ?")
        .bind(now_unix())
        .execute(pool)
        .await?;

    Ok(done.rows_affected())
}

/// Spawns a background task that runs [`cleanup_expired_sessions`] every
/// `config.cleanup_interval`. Cleanup failures are logged via `tracing`
/// and retried on the next tick — they never crash the task.
///
/// The returned [`tokio::task::JoinHandle`] can be aborted on shutdown.
pub fn spawn_cleanup_task(pool: SqlitePool, config: SessionConfig) -> tokio::task::JoinHandle<()> {
    tokio::spawn(async move {
        let mut ticker = tokio::time::interval(config.cleanup_interval);
        ticker.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);
        loop {
            ticker.tick().await;
            match cleanup_expired_sessions(&pool).await {
                Ok(0) => {}
                Ok(n) => tracing::info!(removed = n, "cleaned expired sessions"),
                Err(e) => tracing::warn!(error = %e, "session cleanup failed; will retry"),
            }
        }
    })
}

// ---------------------------------------------------------------------------
// Internals
// ---------------------------------------------------------------------------

fn session_from_row(row: SqliteRow) -> Session {
    Session {
        id: row.get("id"),
        user_id: row.get("user_id"),
        token_hash: row.get("token_hash"),
        created_at: row.get("created_at"),
        expires_at: row.get("expires_at"),
        last_seen_at: row.get("last_seen_at"),
        revoked_at: row.get("revoked_at"),
    }
}

/// 32 cryptographically random bytes, base64url-encoded (no padding).
fn generate_token() -> String {
    let mut bytes = [0u8; TOKEN_LEN];
    use rand_core::RngCore;
    rand_core::OsRng.fill_bytes(&mut bytes);
    URL_SAFE_NO_PAD.encode(bytes)
}

/// `SHA-256(token)` as lowercase hex — the only form stored in the DB.
fn hash_token(token: &str) -> String {
    const HEX: &[u8; 16] = b"0123456789abcdef";
    let digest = Sha256::digest(token.as_bytes());
    let mut hex = String::with_capacity(digest.len() * 2);
    for b in digest {
        hex.push(HEX[(b >> 4) as usize] as char);
        hex.push(HEX[(b & 0x0f) as usize] as char);
    }
    hex
}

fn now_unix() -> i64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_secs() as i64)
        .unwrap_or(0)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::crates::application::Organization::auth_user::{NewUser, create_user};
    use crate::crates::application::Organization::init_db::{
        create_sessions_table, create_users_table,
    };
    use sqlx::sqlite::SqlitePoolOptions;

    fn test_config() -> SessionConfig {
        SessionConfig {
            ttl: std::time::Duration::from_secs(3600),
            absolute_ttl: std::time::Duration::ZERO,
            renew_on_use: true,
            touch_interval: std::time::Duration::ZERO,
            cleanup_interval: std::time::Duration::from_secs(60),
        }
    }

    async fn test_pool() -> SqlitePool {
        // Single connection so the in-memory DB is shared.
        let pool = SqlitePoolOptions::new()
            .max_connections(1)
            .connect("sqlite::memory:")
            .await
            .unwrap();
        create_users_table(&pool).await.unwrap();
        create_sessions_table(&pool).await.unwrap();
        pool
    }

    async fn test_user(pool: &SqlitePool) -> i64 {
        create_user(
            pool,
            &NewUser {
                first_name: "Test".into(),
                last_name: "User".into(),
                user_name: "session_tester".into(),
                email: "session@example.com".into(),
                password: "correct horse battery".into(),
            },
        )
        .await
        .unwrap()
    }

    #[test]
    fn tokens_are_unique_and_hash_stable() {
        let a = generate_token();
        let b = generate_token();
        assert_ne!(a, b);
        assert_eq!(hash_token(&a), hash_token(&a));
        assert_ne!(hash_token(&a), hash_token(&b));
        assert_eq!(hash_token(&a).len(), 64); // SHA-256 hex
    }

    #[tokio::test]
    async fn session_lifecycle() {
        let pool = test_pool().await;
        let cfg = test_config();
        let user_id = test_user(&pool).await;

        // create + validate
        let (session, token) = create_session(&pool, user_id, &cfg).await.unwrap();
        assert!(
            validate_session(&pool, &token, &cfg)
                .await
                .unwrap()
                .is_some()
        );
        // token hash stored, raw token is not
        assert_ne!(session.token_hash, token);

        // unknown token → None
        assert!(
            validate_session(&pool, "bogus-token", &cfg)
                .await
                .unwrap()
                .is_none()
        );

        // revoke → validate fails
        assert!(revoke_session(&pool, &token).await.unwrap());
        assert!(
            validate_session(&pool, &token, &cfg)
                .await
                .unwrap()
                .is_none()
        );
        // second revoke is a no-op returning false
        assert!(!revoke_session(&pool, &token).await.unwrap());

        // revoke_all covers multiple sessions
        let (_, t1) = create_session(&pool, user_id, &cfg).await.unwrap();
        let (_, t2) = create_session(&pool, user_id, &cfg).await.unwrap();
        assert_eq!(revoke_all_user_sessions(&pool, user_id).await.unwrap(), 2);
        assert!(validate_session(&pool, &t1, &cfg).await.unwrap().is_none());
        assert!(validate_session(&pool, &t2, &cfg).await.unwrap().is_none());
    }

    #[tokio::test]
    async fn expired_sessions_fail_validation_and_cleanup() {
        let pool = test_pool().await;
        let user_id = test_user(&pool).await;

        // ttl of 1 second, already-expired row inserted directly
        let (_, token) = create_session(&pool, user_id, &test_config())
            .await
            .unwrap();
        sqlx::query("UPDATE sessions_tbl SET expires_at = 1")
            .execute(&pool)
            .await
            .unwrap();

        assert!(
            validate_session(&pool, &token, &test_config())
                .await
                .unwrap()
                .is_none()
        );

        assert!(cleanup_expired_sessions(&pool).await.unwrap() >= 1);
        let remaining: i64 = sqlx::query_scalar("SELECT COUNT(*) FROM sessions_tbl")
            .fetch_one(&pool)
            .await
            .unwrap();
        assert_eq!(remaining, 0);
    }

    #[tokio::test]
    async fn absolute_ttl_caps_session_age() {
        let pool = test_pool().await;
        let user_id = test_user(&pool).await;

        let cfg = SessionConfig {
            absolute_ttl: std::time::Duration::from_secs(3600),
            ..test_config()
        };
        let (_, token) = create_session(&pool, user_id, &cfg).await.unwrap();

        // Fresh session validates.
        assert!(
            validate_session(&pool, &token, &cfg)
                .await
                .unwrap()
                .is_some()
        );

        // Age the session past the absolute cap; sliding renewal must not
        // rescue it even though expires_at is still in the future.
        sqlx::query("UPDATE sessions_tbl SET created_at = created_at - 7200")
            .execute(&pool)
            .await
            .unwrap();
        assert!(
            validate_session(&pool, &token, &cfg)
                .await
                .unwrap()
                .is_none()
        );

        // With the cap disabled the same aged session is valid again.
        let uncapped = test_config();
        assert!(
            validate_session(&pool, &token, &uncapped)
                .await
                .unwrap()
                .is_some()
        );
    }

    #[tokio::test]
    async fn validation_is_read_only_within_touch_interval() {
        let pool = test_pool().await;
        let user_id = test_user(&pool).await;

        let cfg = SessionConfig {
            touch_interval: std::time::Duration::from_secs(3600),
            ..test_config()
        };
        let (_, token) = create_session(&pool, user_id, &cfg).await.unwrap();

        // Backdate last_seen_at; a validation inside the touch interval
        // must not write it back.
        sqlx::query("UPDATE sessions_tbl SET last_seen_at = last_seen_at - 10")
            .execute(&pool)
            .await
            .unwrap();
        let before: i64 = sqlx::query_scalar("SELECT last_seen_at FROM sessions_tbl")
            .fetch_one(&pool)
            .await
            .unwrap();

        assert!(
            validate_session(&pool, &token, &cfg)
                .await
                .unwrap()
                .is_some()
        );

        let after: i64 = sqlx::query_scalar("SELECT last_seen_at FROM sessions_tbl")
            .fetch_one(&pool)
            .await
            .unwrap();
        assert_eq!(
            before, after,
            "last_seen_at must not be written within interval"
        );

        // touch_interval = 0 restores write-on-every-validation.
        let eager = test_config();
        validate_session(&pool, &token, &eager).await.unwrap();
        let written: i64 = sqlx::query_scalar("SELECT last_seen_at FROM sessions_tbl")
            .fetch_one(&pool)
            .await
            .unwrap();
        assert!(written > before);
    }

    #[tokio::test]
    async fn list_and_revoke_by_id() {
        let pool = test_pool().await;
        let user_id = test_user(&pool).await;
        let cfg = test_config();

        let (s1, t1) = create_session(&pool, user_id, &cfg).await.unwrap();
        let (s2, t2) = create_session(&pool, user_id, &cfg).await.unwrap();

        let active = list_active_sessions(&pool, user_id).await.unwrap();
        assert_eq!(active.len(), 2);
        // Newest first.
        assert_eq!(active[0].id, s2.id);
        assert_eq!(active[1].id, s1.id);

        assert!(revoke_session_by_id(&pool, s1.id).await.unwrap());
        assert!(!revoke_session_by_id(&pool, s1.id).await.unwrap());
        assert!(validate_session(&pool, &t1, &cfg).await.unwrap().is_none());
        assert!(validate_session(&pool, &t2, &cfg).await.unwrap().is_some());

        let remaining = list_active_sessions(&pool, user_id).await.unwrap();
        assert_eq!(remaining.len(), 1);
    }
}
