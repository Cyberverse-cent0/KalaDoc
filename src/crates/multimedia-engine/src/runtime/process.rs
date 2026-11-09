//! Managed external process execution for `ffmpeg`/`ffprobe`.
//!
//! Every op shells out through [`run`]: arguments are passed as an
//! argv vector (never a shell string), the child gets the configured
//! timeout, and stderr is captured because FFmpeg reports all
//! diagnostics there. stdout is captured for tools that emit data on
//! it (`ffprobe -of json`, `ffmpeg -f hash`).

use std::ffi::OsString;
use std::io::Write;
use std::path::Path;
use std::process::{Command, Stdio};
use std::time::{Duration, Instant};

use crate::crates::multimedia_engine::error::{MediaError, MediaResult};
use crate::crates::multimedia_engine::model::MediaLimits;

/// Tail of stderr kept in error messages — enough for FFmpeg's
/// "matches no format/codec/filter" diagnostics without flooding logs.
const STDERR_TAIL: usize = 4096;

/// Result of a completed tool invocation.
#[derive(Debug)]
pub struct ProcOutput {
    /// Whether the process exited with status 0.
    pub success: bool,
    /// Raw stdout bytes.
    pub stdout: Vec<u8>,
    /// stderr as UTF-8-lossy string, tail-trimmed.
    pub stderr: String,
}

/// Run `program` with `args`, feeding `stdin` if given, bounded by
/// `limits.timeout`. Kills the child on timeout.
///
/// Does not interpret the exit status — callers decide whether
/// non-zero is fatal via [`require_success`].
pub fn run(
    tool: &'static str,
    program: &Path,
    args: &[OsString],
    stdin: Option<&[u8]>,
    limits: &MediaLimits,
) -> MediaResult<ProcOutput> {
    let mut cmd = Command::new(program);
    cmd.args(args)
        .stdin(if stdin.is_some() {
            Stdio::piped()
        } else {
            Stdio::null()
        })
        .stdout(Stdio::piped())
        .stderr(Stdio::piped());
    // Children must not inherit our environment secrets into error
    // paths; keep the default env (PATH resolution already happened)
    // but never leak it into messages.
    let mut child = cmd.spawn().map_err(|e| {
        if e.kind() == std::io::ErrorKind::NotFound {
            MediaError::MissingTool(format!("{tool} ({})", program.display()))
        } else {
            MediaError::Io(e)
        }
    })?;

    if let Some(data) = stdin {
        // Write stdin on a thread so a child that stops reading cannot
        // deadlock us against a full pipe while we wait.
        let mut pipe = child.stdin.take().expect("stdin was piped");
        let data = data.to_vec();
        std::thread::spawn(move || {
            let _ = pipe.write_all(&data);
        });
    }

    let deadline = Instant::now() + limits.timeout;
    let status = loop {
        match child.try_wait()? {
            Some(status) => break status,
            None if Instant::now() >= deadline => {
                let _ = child.kill();
                let _ = child.wait();
                return Err(MediaError::Timeout {
                    tool,
                    timeout: limits.timeout,
                });
            }
            None => std::thread::sleep(Duration::from_millis(25)),
        }
    };

    let mut stdout = Vec::new();
    let mut stderr = Vec::new();
    if let Some(mut out) = child.stdout.take() {
        use std::io::Read;
        let _ = out.read_to_end(&mut stdout);
    }
    if let Some(mut err) = child.stderr.take() {
        use std::io::Read;
        let _ = err.read_to_end(&mut stderr);
    }

    let stderr_text = String::from_utf8_lossy(&stderr).into_owned();
    let stderr_text = if stderr_text.len() > STDERR_TAIL {
        let start = stderr_text.len() - STDERR_TAIL;
        format!("…{}", &stderr_text[start..])
    } else {
        stderr_text
    };

    Ok(ProcOutput {
        success: status.success(),
        stdout,
        stderr: stderr_text.trim().to_string(),
    })
}

/// Convenience: run and require exit status 0, mapping failure to
/// [`MediaError::ToolFailed`].
pub fn run_checked(
    tool: &'static str,
    program: &Path,
    args: &[OsString],
    stdin: Option<&[u8]>,
    limits: &MediaLimits,
) -> MediaResult<ProcOutput> {
    let out = run(tool, program, args, stdin, limits)?;
    require_success(tool, out)
}

/// Require `output.success`, else [`MediaError::ToolFailed`].
pub fn require_success(tool: &'static str, output: ProcOutput) -> MediaResult<ProcOutput> {
    if output.success {
        return Ok(output);
    }
    let detail = if output.stderr.is_empty() {
        String::from_utf8_lossy(&output.stdout)
            .chars()
            .rev()
            .take(500)
            .collect::<String>()
            .chars()
            .rev()
            .collect()
    } else {
        output.stderr.clone()
    };
    Err(MediaError::ToolFailed {
        tool,
        status: "non-zero exit".to_string(),
        detail,
    })
}

/// Convert a string slice list to the argv vector [`run`] expects.
pub fn args(items: &[&str]) -> Vec<OsString> {
    items.iter().map(OsString::from).collect()
}
