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
