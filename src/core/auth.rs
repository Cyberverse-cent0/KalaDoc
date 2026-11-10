/// An authenticated application user, as stored in `users_tbl`.
#[derive(Debug, Clone, sqlx::FromRow)]
pub struct AuthUser {
    pub id: i64,
    pub first_name: String,
    pub last_name: String,
    pub user_name: String,
    pub email: String,
    /// Argon2id hash of the user's password. Never exposed to clients.
    pub password_hash: String,
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::crates::application::Organization::auth_user::{
        AuthError, NewUser, create_user, get_user, get_user_by_email, get_user_by_id, validate_user,
    };
    use crate::crates::application::Organization::init_db::{
        SessionConfig, create_sessions_table, create_users_table,
    };
    use crate::crates::application::Organization::session::{
        create_session, revoke_all_user_sessions, revoke_session, validate_session,
    };
    use sqlx::SqlitePool;
    use sqlx::sqlite::SqlitePoolOptions;
    use std::time::Duration;

    fn new_user(name: &str) -> NewUser {
        NewUser {
            first_name: "Ada".into(),
            last_name: "Lovelace".into(),
            user_name: name.into(),
            email: format!("{name}@example.com"),
            password: "correct horse battery".into(),
        }
    }

    fn test_config() -> SessionConfig {
        SessionConfig {
            ttl: Duration::from_secs(3600),
            absolute_ttl: Duration::ZERO,
            renew_on_use: true,
            touch_interval: Duration::ZERO,
            cleanup_interval: Duration::from_secs(60),
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

    #[test]
    fn auth_user_is_cloneable_and_debuggable() {
        let user = AuthUser {
            id: 1,
            first_name: "Ada".into(),
            last_name: "Lovelace".into(),
            user_name: "ada".into(),
            email: "ada@example.com".into(),
            password_hash: "$argon2id$v=19$...".into(),
        };
        let cloned = user.clone();
        assert_eq!(cloned.id, user.id);
        assert_eq!(cloned.user_name, user.user_name);
        assert!(format!("{user:?}").contains("ada"));
    }

    #[tokio::test]
    async fn user_roundtrip_never_exposes_plaintext_password() {
        let pool = test_pool().await;
        let id = create_user(&pool, &new_user("ada")).await.unwrap();

        let user = get_user(&pool, "ada").await.unwrap().unwrap();
        assert_eq!(user.id, id);
        assert_eq!(user.first_name, "Ada");
        assert_eq!(user.last_name, "Lovelace");
        assert_eq!(user.user_name, "ada");
        assert_eq!(user.email, "ada@example.com");
        // Argon2id PHC string — never the plaintext password.
        assert!(user.password_hash.starts_with("$argon2"));
        assert_ne!(user.password_hash, "correct horse battery");

        // Lookups by id and email (case-insensitive) resolve to the
        // same row.
        assert_eq!(
            get_user_by_id(&pool, id).await.unwrap().unwrap().user_name,
            "ada"
        );
        assert_eq!(
            get_user_by_email(&pool, "ADA@example.com")
                .await
                .unwrap()
                .unwrap()
                .id,
            id
        );
        // Username lookup is case-insensitive too.
        assert!(get_user(&pool, "ADA").await.unwrap().is_some());

        // Unknown identifiers → None.
        assert!(get_user(&pool, "nobody").await.unwrap().is_none());
        assert!(get_user_by_id(&pool, 999).await.unwrap().is_none());
        assert!(
            get_user_by_email(&pool, "nobody@example.com")
                .await
                .unwrap()
                .is_none()
        );
    }

    #[tokio::test]
    async fn credential_validation_and_duplicate_rules() {
        let pool = test_pool().await;
        let id = create_user(&pool, &new_user("grace")).await.unwrap();

        // Right password → user; wrong password and unknown user both
        // → None, so callers cannot enumerate accounts.
        let ok = validate_user(&pool, "grace", "correct horse battery")
            .await
            .unwrap();
        assert_eq!(ok.unwrap().id, id);
        assert!(
            validate_user(&pool, "grace", "wrong password")
                .await
                .unwrap()
                .is_none()
        );
        assert!(
            validate_user(&pool, "ghost", "correct horse battery")
                .await
                .unwrap()
                .is_none()
        );

        // Duplicate user_name / email surface as dedicated errors, not
        // generic Db failures. NOCASE collation makes "GRACE" clash.
        let dup_name = NewUser {
            user_name: "GRACE".into(),
            email: "other@example.com".into(),
            ..new_user("other")
        };
        assert!(matches!(
            create_user(&pool, &dup_name).await,
            Err(AuthError::DuplicateUserName(_))
        ));
        let dup_email = NewUser {
            user_name: "hopper".into(),
            email: "GRACE@example.com".into(),
            ..new_user("hopper")
        };
        assert!(matches!(
            create_user(&pool, &dup_email).await,
            Err(AuthError::DuplicateEmail(_))
        ));
    }

    #[tokio::test]
    async fn session_token_lifecycle() {
        let pool = test_pool().await;
        let cfg = test_config();
        let user_id = create_user(&pool, &new_user("session_user")).await.unwrap();

        let (session, token) = create_session(&pool, user_id, &cfg).await.unwrap();
        // Only the SHA-256 hash is persisted — the raw token is shown
        // once and never stored.
        assert_ne!(session.token_hash, token);
        assert_eq!(session.token_hash.len(), 64);
        assert_eq!(session.user_id, user_id);
        assert!(session.revoked_at.is_none());

        // A valid token resolves back to its owning user.
        let valid = validate_session(&pool, &token, &cfg)
            .await
            .unwrap()
            .unwrap();
        assert_eq!(valid.user_id, user_id);
        assert_eq!(valid.id, session.id);

        // A tampered token hashes to a different row lookup and is
        // rejected exactly like an unknown token; the original still
        // validates.
        let mut tampered = token.clone();
        let last = tampered.pop().unwrap();
        tampered.push(if last == 'a' { 'b' } else { 'a' });
        assert_ne!(tampered, token);
        assert!(
            validate_session(&pool, &tampered, &cfg)
                .await
                .unwrap()
                .is_none()
        );
        assert!(
            validate_session(&pool, &token, &cfg)
                .await
                .unwrap()
                .is_some()
        );

        // An expired session is rejected even though its row exists.
        sqlx::query("UPDATE sessions_tbl SET expires_at = 1")
            .execute(&pool)
            .await
            .unwrap();
        assert!(
            validate_session(&pool, &token, &cfg)
                .await
                .unwrap()
                .is_none()
        );
    }

    #[tokio::test]
    async fn revoked_sessions_are_rejected() {
        let pool = test_pool().await;
        let cfg = test_config();
        let user_id = create_user(&pool, &new_user("revoke_user")).await.unwrap();

        let (_, token) = create_session(&pool, user_id, &cfg).await.unwrap();
        assert!(revoke_session(&pool, &token).await.unwrap());
        assert!(
            validate_session(&pool, &token, &cfg)
                .await
                .unwrap()
                .is_none()
        );
        // Revoking an already-revoked token reports false.
        assert!(!revoke_session(&pool, &token).await.unwrap());

        // revoke_all covers every live session for the user.
        let (_, t1) = create_session(&pool, user_id, &cfg).await.unwrap();
        let (_, t2) = create_session(&pool, user_id, &cfg).await.unwrap();
        assert_eq!(revoke_all_user_sessions(&pool, user_id).await.unwrap(), 2);
        assert!(validate_session(&pool, &t1, &cfg).await.unwrap().is_none());
        assert!(validate_session(&pool, &t2, &cfg).await.unwrap().is_none());
    }
}
