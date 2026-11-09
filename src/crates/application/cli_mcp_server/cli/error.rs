//! CLI error type and the stable exit-code contract (spec §5.2/§5.3).

use crate::config::ConfigError;
use crate::crates::application::Organization::auth_user::AuthError;
use crate::crates::application::Organization::init_db::DbError;
use crate::crates::application::Organization::session::SessionError;

/// Every failure a command can produce. `exit_code()`/`code()` implement
/// the documented contract; do not add variants without assigning both.
#[derive(Debug, thiserror::Error)]
pub enum CliError {
    /// Invalid arguments or configuration. Exit 2.
    #[error("{0}")]
    Usage(String),
    /// Configuration file/env problem. Exit 2.
    #[error(transparent)]
    Config(#[from] ConfigError),
    /// No valid session/token for an authenticated command. Exit 3.
    #[error("authentication required: run `kaladoc auth login`")]
    AuthRequired,
    /// Credentials rejected. Exit 3.
    #[error("authentication failed")]
    AuthFailed,
    /// Authenticated but not allowed. Exit 4.
    #[error("permission denied")]
    PermissionDenied,
    /// Referenced resource does not exist. Exit 5.
    #[error("not found: {0}")]
    NotFound(String),
    /// State conflict (e.g. unsafe overwrite, duplicate). Exit 6.
    #[error("conflict: {0}")]
    Conflict(String),
    /// Input failed validation. Exit 7.
    #[error("validation failed: {0}")]
    Validation(String),
    /// Operation/format this build does not support. Exit 13.
    #[error("unsupported: {0}")]
    Unsupported(String),
    /// User-management service error.
    #[error(transparent)]
    Auth(#[from] AuthError),
    /// Database/service error.
    #[error(transparent)]
    Db(#[from] DbError),
    /// Session service error.
    #[error(transparent)]
    Session(#[from] SessionError),
    /// Local I/O (credentials file, prompts, artifacts).
    #[error(transparent)]
    Io(#[from] std::io::Error),
    /// JSON serialization failure.
    #[error(transparent)]
    Json(#[from] serde_json::Error),
    /// YAML serialization failure.
    #[error(transparent)]
    Yaml(#[from] serde_yaml::Error),
    /// MCP server/runtime failure.
    #[error("MCP server error: {0}")]
    Mcp(String),
    /// Unexpected internal failure.
    #[error("internal error: {0}")]
    Internal(String),
}

impl CliError {
    /// Stable process exit code per spec §5.2.
    pub fn exit_code(&self) -> i32 {
        match self {
            Self::Usage(_) | Self::Config(_) => 2,
            Self::AuthRequired | Self::AuthFailed => 3,
            Self::PermissionDenied => 4,
            Self::NotFound(_) => 5,
            Self::Conflict(_) => 6,
            Self::Validation(_) => 7,
            Self::Unsupported(_) => 13,
            Self::Auth(AuthError::Validation(_)) => 7,
            Self::Auth(
                AuthError::DuplicateUserName(_) | AuthError::DuplicateEmail(_),
            ) => 6,
            _ => 1,
        }
    }

    /// Stable machine-readable error code per spec §5.3.
    pub fn code(&self) -> &'static str {
        match self {
            Self::Usage(_) => "invalid_args",
            Self::Config(_) => "invalid_config",
            Self::AuthRequired => "auth_required",
            Self::AuthFailed => "auth_failed",
            Self::PermissionDenied => "permission_denied",
            Self::NotFound(_) => "not_found",
            Self::Conflict(_) => "conflict",
            Self::Validation(_) => "validation_failed",
            Self::Unsupported(_) => "unsupported",
            Self::Auth(AuthError::Validation(_)) => "validation_failed",
            Self::Auth(
                AuthError::DuplicateUserName(_) | AuthError::DuplicateEmail(_),
            ) => "conflict",
            Self::Auth(_) => "internal",
            Self::Db(_) | Self::Session(_) => "internal",
            Self::Io(_) => "io_error",
            Self::Json(_) | Self::Yaml(_) | Self::Mcp(_) | Self::Internal(_) => "internal",
        }
    }

    /// Whether a retry could plausibly succeed (spec §5.3 `retryable`).
    pub fn retryable(&self) -> bool {
        matches!(self, Self::Db(_) | Self::Session(_) | Self::Io(_))
    }
}
