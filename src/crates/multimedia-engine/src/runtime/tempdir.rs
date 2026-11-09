//! Temporary-directory helpers for op implementations.
//!
//! ffmpeg/ffprobe need real filesystem paths. Ops write their byte
//! inputs into a [`TempDir`], run the tool against files inside it,
//! then read outputs back under the configured byte limits. The
//! directory is removed on drop — including on error paths.

use std::path::{Path, PathBuf};

use crate::crates::multimedia_engine::error::{MediaError, MediaResult};

/// A unique temporary directory removed on drop.
#[derive(Debug)]
pub struct TempDir {
    path: PathBuf,
}

impl TempDir {
    /// Create a fresh temp dir with the given prefix.
    pub fn new(prefix: &str) -> MediaResult<Self> {
        let base = std::env::temp_dir();
        for attempt in 0..100u32 {
            let candidate = base.join(format!("kaladoc-{prefix}-{}-{attempt}", std::process::id()));
            match std::fs::create_dir(&candidate) {
                Ok(()) => return Ok(Self { path: candidate }),
                Err(e) if e.kind() == std::io::ErrorKind::AlreadyExists => continue,
                Err(e) => return Err(MediaError::Io(e)),
            }
        }
        Err(MediaError::Io(std::io::Error::new(
            std::io::ErrorKind::AlreadyExists,
            "could not allocate a unique temp directory",
        )))
    }

    /// Root path of the temp dir.
    pub fn path(&self) -> &Path {
        &self.path
    }

    /// Path for a file inside the dir (not created).
    pub fn file(&self, name: &str) -> PathBuf {
        self.path.join(name)
    }

    /// Write `data` to `name` inside the dir, returning its path.
    pub fn write(&self, name: &str, data: &[u8]) -> MediaResult<PathBuf> {
        let path = self.file(name);
        std::fs::write(&path, data)?;
        Ok(path)
    }

    /// Read a file inside the dir, enforcing `max_bytes`.
    pub fn read_limited(&self, name: &str, max_bytes: Option<u64>) -> MediaResult<Vec<u8>> {
        let path = self.file(name);
        let meta = std::fs::metadata(&path)?;
        if let Some(max) = max_bytes
            && meta.len() > max
        {
            return Err(MediaError::LimitExceeded(format!(
                "output {name} is {} bytes, exceeding max_output_bytes {max}",
                meta.len()
            )));
        }
        std::fs::read(&path).map_err(MediaError::Io)
    }
}

impl Drop for TempDir {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.path);
    }
}
