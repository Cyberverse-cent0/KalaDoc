//! On-disk credential store for `kaladoc auth login`.
//!
//! The session bearer token is stored in
//! `<config-dir>/kaladoc/credentials.toml` with owner-only permissions
//! (0600 on Unix). The token is a secret: never log it, never print it,
//! never put it in command arguments or shell history.

use serde::{Deserialize, Serialize};
use std::path::PathBuf;
use std::time::{SystemTime, UNIX_EPOCH};

use super::error::CliError;

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Credentials {
    /// Raw bearer token. Secret — treat accordingly.
    pub token: String,
    pub user_name: String,
    /// Unix seconds when the session was created locally.
    pub created_at: i64,
}

impl Credentials {
    pub fn new(token: String, user_name: String) -> Self {
        Self {
            token,
            user_name,
            created_at: now_unix(),
        }
    }
}

/// Path of the credentials file (`~/.config/kaladoc/credentials.toml`
/// on Linux, platform equivalent elsewhere).
pub fn path() -> Result<PathBuf, CliError> {
    let dirs = directories::ProjectDirs::from("dev", "KalaDoc", "kaladoc")
        .ok_or_else(|| CliError::Usage("cannot determine user config directory".to_string()))?;
    Ok(dirs.config_dir().join("credentials.toml"))
}

/// Loads stored credentials, or `None` when the user is logged out.
pub fn load() -> Result<Option<Credentials>, CliError> {
    let path = path()?;
    match std::fs::read_to_string(&path) {
        Ok(text) => toml::from_str(&text)
            .map(Some)
            .map_err(|e| {
                CliError::Validation(format!(
                    "credentials file {} is corrupt: {e}",
                    path.display()
                ))
            }),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(None),
        Err(e) => Err(e.into()),
    }
}

/// Writes credentials with owner-only permissions, creating the config
/// directory when needed.
pub fn store(creds: &Credentials) -> Result<(), CliError> {
    let path = path()?;
    if let Some(dir) = path.parent() {
        std::fs::create_dir_all(dir)?;
    }
    let text = toml::to_string_pretty(creds)
        .map_err(|e| CliError::Validation(format!("failed to serialize credentials: {e}")))?;

    let mut opts = std::fs::OpenOptions::new();
    opts.write(true).create(true).truncate(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt;
        opts.mode(0o600);
    }
    use std::io::Write;
    opts.open(&path)?.write_all(text.as_bytes())?;
    Ok(())
}

/// Removes the credentials file. Returns `false` when none existed.
pub fn clear() -> Result<bool, CliError> {
    match std::fs::remove_file(path()?) {
        Ok(()) => Ok(true),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(false),
        Err(e) => Err(e.into()),
    }
}

fn now_unix() -> i64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_secs() as i64)
        .unwrap_or(0)
}
