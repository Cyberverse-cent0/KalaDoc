//! Shared helpers for the external-tool backends: PATH probing,
//! timeout-bounded command execution, temp-dir management and input
//! validation.

use std::ffi::OsStr;
use std::fs;
use std::io::{self, Read};
use std::path::{Path, PathBuf};
use std::process::{Command, ExitStatus, Stdio};
use std::sync::atomic::{AtomicU64, Ordering};
use std::thread::{self, JoinHandle};
use std::time::{Duration, Instant};

use crate::crates::conversion_engine::{ConvertError, ConvertOutput, ConvertResult, Format};

/// Hard cap for a single external-tool invocation.
pub const COMMAND_TIMEOUT: Duration = Duration::from_secs(120);

/// Timeout used only for cheap `--version` probes during detection.
pub const PROBE_TIMEOUT: Duration = Duration::from_secs(10);

/// Inputs larger than this are rejected before touching disk.
pub const MAX_INPUT_BYTES: u64 = 256 * 1024 * 1024; // 256 MiB

/// stderr is trimmed to this many chars when embedded in errors.
const MAX_MESSAGE_CHARS: usize = 500;

/// How often [`run_capture`] polls `Child::try_wait`.
const POLL_INTERVAL: Duration = Duration::from_millis(20);

/// Result of a completed external command.
pub struct ProcOutput {
    pub status: ExitStatus,
    pub stdout: Vec<u8>,
    pub stderr: Vec<u8>,
}

/// Why [`run_capture`] failed.
pub enum RunFailure {
    Io(io::Error),
    Timeout,
}

impl From<io::Error> for RunFailure {
    fn from(e: io::Error) -> Self {
        Self::Io(e)
    }
}

/// An RAII temp directory removed on drop. Every external backend
/// funnels its scratch files through this so nothing leaks on error.
pub struct TempDir {
    path: PathBuf,
}

impl TempDir {
    /// Create a uniquely-named directory under the system temp dir.
    pub fn new(tag: &str) -> io::Result<Self> {
        static COUNTER: AtomicU64 = AtomicU64::new(0);
        let nanos = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|d| d.as_nanos())
            .unwrap_or(0);
        let seq = COUNTER.fetch_add(1, Ordering::Relaxed);
        let name = format!("kaladoc-ext-{tag}-{}-{nanos}-{seq}", std::process::id());
        let path = std::env::temp_dir().join(name);
        fs::create_dir_all(&path)?;
        Ok(Self { path })
    }

    pub fn path(&self) -> &Path {
        &self.path
    }

    /// `path()/name` convenience.
    pub fn file(&self, name: &str) -> PathBuf {
        self.path.join(name)
    }
}

impl Drop for TempDir {
    fn drop(&mut self) {
        let _ = fs::remove_dir_all(&self.path);
    }
}

/// Sanity-check request input: non-empty and within [`MAX_INPUT_BYTES`].
pub fn check_input(input: &[u8]) -> ConvertResult<()> {
    if input.is_empty() {
        return Err(ConvertError::InvalidInput {
            reason: "input is empty".to_string(),
        });
    }
    if input.len() as u64 > MAX_INPUT_BYTES {
        return Err(ConvertError::TooLarge {
            limit: format!("{} bytes", MAX_INPUT_BYTES),
            actual: input.len() as u64,
        });
    }
    Ok(())
}

/// File extension to use for a temp input file of `format`.
/// (Free function so every backend writes a sniffable extension.)
pub fn format_ext(format: Format) -> &'static str {
    match format {
        Format::Pdf => "pdf",
        Format::Txt => "txt",
        Format::Markdown => "md",
        Format::Html => "html",
        Format::Png => "png",
        Format::Jpeg => "jpg",
        Format::Docx => "docx",
        Format::Odt => "odt",
        Format::Xlsx => "xlsx",
        Format::Xls => "xls",
        Format::Pptx => "pptx",
        Format::Ppt => "ppt",
        Format::Epub => "epub",
        Format::Eml => "eml",
        Format::Cbz => "cbz",
        Format::Cbr => "cbr",
        // Url input is intercepted by the engine before backend
        // dispatch; this arm exists only for exhaustiveness.
        Format::Url => "url",
    }
}

/// Truncate arbitrary tool output for embedding in error messages.
pub fn trim_message(text: &str) -> String {
    text.trim()
        .chars()
        .take(MAX_MESSAGE_CHARS)
        .collect::<String>()
}

/// Best-effort human-readable message from a failed process:
/// stderr first, then stdout, then a fallback.
pub fn failure_message(output: &ProcOutput, fallback: &str) -> String {
    let stderr = String::from_utf8_lossy(&output.stderr);
    let trimmed = trim_message(stderr.trim());
    if !trimmed.is_empty() {
        return trimmed;
    }
    let stdout = String::from_utf8_lossy(&output.stdout);
    let trimmed = trim_message(stdout.trim());
    if !trimmed.is_empty() {
        return trimmed;
    }
    fallback.to_string()
}

/// Build a [`ConvertOutput`] for a successful conversion.
pub fn success(
    bytes: Vec<u8>,
    output_format: Format,
    backend: &'static str,
    warnings: Vec<String>,
) -> ConvertOutput {
    ConvertOutput {
        bytes,
        output_format,
        backend: backend.to_string(),
        warnings,
    }
}

/// Locate `name` on the given PATH-style search path. Respects the
/// PATH variable passed in (not a global), which keeps tests hermetic.
pub fn find_in_path(name: &str, path_env: &OsStr) -> Option<PathBuf> {
    for dir in std::env::split_paths(path_env) {
        let candidate = dir.join(name);
        if !candidate.is_file() {
            continue;
        }
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            match candidate.metadata() {
                Ok(meta) if meta.permissions().mode() & 0o111 != 0 => return Some(candidate),
                _ => continue,
            }
        }
        #[cfg(not(unix))]
        {
            return Some(candidate);
        }
    }
    None
}

/// Read a pipe to EOF on a helper thread so the child cannot deadlock
/// on a full stdout/stderr buffer while we poll for exit.
fn read_pipe<R>(mut reader: R) -> JoinHandle<io::Result<Vec<u8>>>
where
    R: Read + Send + 'static,
{
    thread::spawn(move || {
        let mut buf = Vec::new();
        reader.read_to_end(&mut buf)?;
        Ok(buf)
    })
}

/// Spawn `program` with `args`, drain stdout/stderr concurrently,
/// and wait up to `timeout`. On expiry the child is killed and
/// [`RunFailure::Timeout`] is returned.
pub fn run_capture(
    program: &Path,
    args: &[String],
    timeout: Duration,
) -> Result<ProcOutput, RunFailure> {
    let mut child = Command::new(program)
        .args(args)
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .map_err(RunFailure::Io)?;

    let stdout_pipe = child.stdout.take().expect("stdout was piped");
    let stderr_pipe = child.stderr.take().expect("stderr was piped");
    let out_handle = read_pipe(stdout_pipe);
    let err_handle = read_pipe(stderr_pipe);

    let deadline = Instant::now() + timeout;
    let status = loop {
        match child.try_wait() {
            Ok(Some(status)) => break status,
            Ok(None) => {
                if Instant::now() >= deadline {
                    let _ = child.kill();
                    let _ = child.wait();
                    let _ = out_handle.join();
                    let _ = err_handle.join();
                    return Err(RunFailure::Timeout);
                }
                thread::sleep(POLL_INTERVAL);
            }
            Err(e) => {
                let _ = child.kill();
                let _ = child.wait();
                let _ = out_handle.join();
                let _ = err_handle.join();
                return Err(RunFailure::Io(e));
            }
        }
    };

    let stdout = out_handle
        .join()
        .map_err(|_| io::Error::other("stdout reader thread panicked"))??;
    let stderr = err_handle
        .join()
        .map_err(|_| io::Error::other("stderr reader thread panicked"))??;

    Ok(ProcOutput {
        status,
        stdout,
        stderr,
    })
}

/// [`run_capture`] mapped onto [`ConvertError`]: spawn `NotFound`
/// becomes `BackendUnavailable`, timeout becomes `Timeout`, other
/// failures become `BackendFailed` with trimmed stderr.
pub fn run_tool(
    program: &Path,
    args: &[String],
    timeout: Duration,
    backend: &'static str,
) -> ConvertResult<ProcOutput> {
    match run_capture(program, args, timeout) {
        Ok(output) => Ok(output),
        Err(RunFailure::Timeout) => Err(ConvertError::Timeout {
            backend: backend.to_string(),
            seconds: timeout.as_secs(),
        }),
        Err(RunFailure::Io(e)) if e.kind() == io::ErrorKind::NotFound => {
            Err(ConvertError::BackendUnavailable {
                backend: backend.to_string(),
                reason: "executable not found".to_string(),
            })
        }
        Err(RunFailure::Io(e)) => Err(ConvertError::BackendFailed {
            backend: backend.to_string(),
            message: trim_message(&e.to_string()),
        }),
    }
}

/// Extract the first non-empty line of a `--version`-style output,
/// truncated to ~80 chars.
pub fn version_line(output: &ProcOutput) -> Option<String> {
    let stdout = String::from_utf8_lossy(&output.stdout);
    let stderr = String::from_utf8_lossy(&output.stderr);
    for text in [stdout.as_ref(), stderr.as_ref()] {
        for line in text.lines() {
            let line = line.trim();
            if !line.is_empty() {
                return Some(line.chars().take(80).collect());
            }
        }
    }
    None
}

/// Probe `program --version` (falling back to `-v`, which some
/// poppler tools use) and return the version line, if any.
pub fn probe_version(program: &Path) -> Option<String> {
    for flag in ["--version", "-v"] {
        let args = vec![flag.to_string()];
        if let Ok(output) = run_capture(program, &args, PROBE_TIMEOUT) {
            if let Some(v) = version_line(&output) {
                return Some(v);
            }
        }
    }
    None
}
