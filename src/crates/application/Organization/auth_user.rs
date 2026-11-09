//! User registration, lookup, credential validation, and removal.
//!
//! All functions take a `&SqlitePool` (see [`super::init_db`]) and return
//! [`AuthError`]. Passwords are hashed with Argon2id before they ever
//! touch the database; plaintext passwords are never stored or logged.

use argon2::Argon2;
use argon2::password_hash::{PasswordHasher, PasswordVerifier, SaltString, rand_core::OsRng};
use sqlx::sqlite::SqliteRow;
use sqlx::{Row, SqlitePool};

use crate::core::auth::AuthUser;

/// Credentials and profile fields required to register a new user.
#[derive(Debug, Clone)]
pub struct NewUser {
    pub first_name: String,
    pub last_name: String,
    pub user_name: String,
    pub email: String,
    /// Plaintext password — hashed internally, never stored as-is.
    pub password: String,
}

/// Errors from user-management operations.
#[derive(Debug, thiserror::Error)]
pub enum AuthError {
    #[error(transparent)]
    Db(#[from] sqlx::Error),
    #[error("user_name {0:?} is already taken")]
    DuplicateUserName(String),
    #[error("email {0:?} is already registered")]
    DuplicateEmail(String),
    #[error("invalid input: {0}")]
    Validation(String),
    #[error("password hashing failed: {0}")]
    Hashing(String),
}

// ---------------------------------------------------------------------------
// Public API
// ---------------------------------------------------------------------------

/// Validates `new`, hashes its password, and inserts a row into
/// `users_tbl`. Returns the new user's id.
pub async fn create_user(pool: &SqlitePool, new: &NewUser) -> Result<i64, AuthError> {
    validate_new_user(new)?;
    let password_hash = hash_password(&new.password)?;

    let done = sqlx::query(
        "INSERT INTO users_tbl (first_name, last_name, user_name, email, password_hash)
         VALUES (?, ?, ?, ?, ?)",
    )
    .bind(&new.first_name)
    .bind(&new.last_name)
    .bind(&new.user_name)
    .bind(&new.email)
    .bind(&password_hash)
    .execute(pool)
    .await
    .map_err(|e| classify_insert_error(e, new))?;

    Ok(done.last_insert_rowid())
}

/// Fetches a user by `user_name` (case-insensitive, per the column's
/// `COLLATE NOCASE`). Returns `None` when no such user exists.
pub async fn get_user(pool: &SqlitePool, user_name: &str) -> Result<Option<AuthUser>, AuthError> {
    let row = sqlx::query(
        "SELECT id, first_name, last_name, user_name, email, password_hash
         FROM users_tbl WHERE user_name = ?",
    )
    .bind(user_name)
    .fetch_optional(pool)
    .await?;

    Ok(row.map(user_from_row))
}

/// Fetches a user by email address (case-insensitive).
pub async fn get_user_by_email(
    pool: &SqlitePool,
    email: &str,
) -> Result<Option<AuthUser>, AuthError> {
    let row = sqlx::query(
        "SELECT id, first_name, last_name, user_name, email, password_hash
         FROM users_tbl WHERE email = ?",
    )
    .bind(email)
    .fetch_optional(pool)
    .await?;

    Ok(row.map(user_from_row))
}

/// Fetches a user by primary key. Returns `None` when no such user exists.
pub async fn get_user_by_id(pool: &SqlitePool, id: i64) -> Result<Option<AuthUser>, AuthError> {
    let row = sqlx::query(
        "SELECT id, first_name, last_name, user_name, email, password_hash
         FROM users_tbl WHERE id = ?",
    )
    .bind(id)
    .fetch_optional(pool)
    .await?;

    Ok(row.map(user_from_row))
}

/// Checks `user_name` + `password` against the stored hash.
///
/// Returns `Ok(Some(user))` on success, `Ok(None)` when the username
/// doesn't exist **or** the password is wrong — callers should not be
/// able to tell those cases apart (no user enumeration).
pub async fn validate_user(
    pool: &SqlitePool,
    user_name: &str,
    password: &str,
) -> Result<Option<AuthUser>, AuthError> {
    let Some(user) = get_user(pool, user_name).await? else {
        return Ok(None);
    };

    if verify_password(&user.password_hash, password) {
        Ok(Some(user))
    } else {
        Ok(None)
    }
}

/// Summary of a user account for listings — deliberately excludes
/// `password_hash`, which must never leave the application layer.
#[derive(Debug, Clone, serde::Serialize, sqlx::FromRow)]
pub struct UserSummary {
    pub id: i64,
    pub first_name: String,
    pub last_name: String,
    pub user_name: String,
    pub email: String,
}

/// Lists all user accounts ordered by `user_name`. Password hashes are
/// not selected.
pub async fn list_users(pool: &SqlitePool) -> Result<Vec<UserSummary>, AuthError> {
    let rows = sqlx::query_as::<_, UserSummary>(
        "SELECT id, first_name, last_name, user_name, email
         FROM users_tbl ORDER BY user_name",
    )
    .fetch_all(pool)
    .await?;
    Ok(rows)
}

/// Deletes the user with `user_name`. Returns `true` if a row was
/// removed, `false` if no such user existed.
pub async fn remove_user(pool: &SqlitePool, user_name: &str) -> Result<bool, AuthError> {
    let done = sqlx::query("DELETE FROM users_tbl WHERE user_name = ?")
        .bind(user_name)
        .execute(pool)
        .await?;

    Ok(done.rows_affected() > 0)
}

// ---------------------------------------------------------------------------
// Internals
// ---------------------------------------------------------------------------

fn user_from_row(row: SqliteRow) -> AuthUser {
    AuthUser {
        id: row.get("id"),
        first_name: row.get("first_name"),
        last_name: row.get("last_name"),
        user_name: row.get("user_name"),
        email: row.get("email"),
        password_hash: row.get("password_hash"),
    }
}

fn validate_new_user(new: &NewUser) -> Result<(), AuthError> {
    if new.first_name.trim().is_empty() {
        return Err(AuthError::Validation(
            "first_name must not be empty".to_string(),
        ));
    }
    if new.last_name.trim().is_empty() {
        return Err(AuthError::Validation(
            "last_name must not be empty".to_string(),
        ));
    }
    validate_user_name(&new.user_name)?;
    validate_email(&new.email)?;
    validate_password(&new.password)?;
    Ok(())
}

/// User names: 3–32 chars of ASCII letters, digits, `_`, `-`, `.`.
pub(crate) fn validate_user_name(name: &str) -> Result<(), AuthError> {
    let bad = |msg: &str| Err(AuthError::Validation(msg.to_string()));
    if name.len() < 3 || name.len() > 32 {
        return bad("user_name must be 3–32 characters");
    }
    if !name
        .chars()
        .all(|c| c.is_ascii_alphanumeric() || matches!(c, '_' | '-' | '.'))
    {
        return bad("user_name may only contain letters, digits, '_', '-' and '.'");
    }
    Ok(())
}

/// Email addresses: single `@` with non-empty local and domain parts,
/// no whitespace, bounded length.
pub(crate) fn validate_email(email: &str) -> Result<(), AuthError> {
    let bad = |msg: &str| Err(AuthError::Validation(msg.to_string()));
    if email.is_empty() || email.len() > 254 {
        return bad("email must be 1–254 characters");
    }
    if email.chars().any(char::is_whitespace) {
        return bad("email must not contain whitespace");
    }
    let Some((local, domain)) = email.rsplit_once('@') else {
        return bad("email is not a valid address");
    };
    if local.is_empty() || domain.is_empty() || !domain.contains('.') {
        return bad("email is not a valid address");
    }
    Ok(())
}

/// Passwords: 8–1024 chars. Upper bound prevents absurd allocations
/// being fed to Argon2.
pub(crate) fn validate_password(password: &str) -> Result<(), AuthError> {
    let bad = |msg: &str| Err(AuthError::Validation(msg.to_string()));
    if password.len() < 8 {
        return bad("password must be at least 8 characters");
    }
    if password.len() > 1024 {
        return bad("password must be at most 1024 characters");
    }
    Ok(())
}

fn hash_password(password: &str) -> Result<String, AuthError> {
    let salt = SaltString::generate(&mut OsRng);
    Argon2::default()
        .hash_password(password.as_bytes(), &salt)
        .map(|h| h.to_string())
        .map_err(|e| AuthError::Hashing(e.to_string()))
}

fn verify_password(hash: &str, password: &str) -> bool {
    let Ok(parsed) = argon2::password_hash::PasswordHash::new(hash) else {
        return false;
    };
    Argon2::default()
        .verify_password(password.as_bytes(), &parsed)
        .is_ok()
}

/// Maps a failed INSERT into a specific [`AuthError`]. UNIQUE violations
/// on `user_name`/`email` become dedicated variants; everything else is
/// passed through as [`AuthError::Db`].
fn classify_insert_error(e: sqlx::Error, new: &NewUser) -> AuthError {
    if let sqlx::Error::Database(db) = &e
        && db.is_unique_violation()
    {
        let msg = e.to_string().to_lowercase();
        if msg.contains("user_name") {
            return AuthError::DuplicateUserName(new.user_name.clone());
        }
        if msg.contains("email") {
            return AuthError::DuplicateEmail(new.email.clone());
        }
    }
    AuthError::Db(e)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn sample_user(name: &str) -> NewUser {
        NewUser {
            first_name: "Ada".into(),
            last_name: "Lovelace".into(),
            user_name: name.into(),
            email: format!("{name}@example.com"),
            password: "correct horse battery".into(),
        }
    }

    #[test]
    fn password_hash_roundtrip() {
        let hash = hash_password("hunter2 hunter2").unwrap();
        assert!(verify_password(&hash, "hunter2 hunter2"));
        assert!(!verify_password(&hash, "wrong"));
        assert!(!verify_password("not-a-phc-string", "hunter2 hunter2"));
    }

    #[test]
    fn validation_rejects_bad_input() {
        let mut u = sample_user("ok_user");
        assert!(validate_new_user(&u).is_ok());

        u.user_name = "ab".into();
        assert!(matches!(
            validate_new_user(&u),
            Err(AuthError::Validation(_))
        ));

        u.user_name = "bad name!".into();
        assert!(matches!(
            validate_new_user(&u),
            Err(AuthError::Validation(_))
        ));

        u.user_name = "ok_user".into();
        u.password = "short".into();
        assert!(matches!(
            validate_new_user(&u),
            Err(AuthError::Validation(_))
        ));
    }
}
