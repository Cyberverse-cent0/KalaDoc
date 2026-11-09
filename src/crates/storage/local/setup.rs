//! One-time local storage preparation.

use std::io;

use super::config::StorageConfig;

/// Prepares the local storage root described by `config`.
///
/// Creates `root_dir` (and any missing parents) if it does not exist.
/// Idempotent — calling it on an already-prepared root is a no-op.
pub fn setup(config: &StorageConfig) -> io::Result<()> {
    std::fs::create_dir_all(&config.root_dir)
}
