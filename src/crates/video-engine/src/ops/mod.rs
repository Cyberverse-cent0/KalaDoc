//! ffmpeg/ffprobe-backed media operations.
//!
//! Every operation follows the same contract:
//!
//! - Input arrives as an in-memory `&[u8]` buffer and is written to a
//!   private scratch directory under the system temp dir (see
//!   [`TempDir`]); the directory is removed on drop, on success and on
//!   every error path.
//! - The external binary is resolved on `PATH` via [`find_tool`]
//!   before anything touches disk; absence yields
//!   [`VideoError::MissingTool`]. Results are never faked.
//! - Tool invocations go through [`run_tool`], which drains stdout and
//!   stderr on helper threads (a child cannot deadlock on a full pipe)
//!   and enforces a timeout by polling `try_wait` and killing the
//!   process — `std::process` has no built-in timeout.
//! - Non-zero exits are reported by callers as
//!   [`VideoError::ExternalTool`] with the tail of stderr; malformed
//!   media is [`VideoError::InvalidInput`], never a panic.

pub(crate) mod analysis;
pub(crate) mod audio;
pub(crate) mod audio_fx;
pub(crate) mod audio_mix;
pub mod av_visual;
pub mod capture;
pub(crate) mod concat;
pub(crate) mod contact_sheet;
pub(crate) mod encode;
pub(crate) mod extract;
pub(crate) mod extract_audio;
pub mod filters;
pub(crate) mod frames;
pub(crate) mod geometry;
pub(crate) mod geometry_adv;
pub(crate) mod loudness;
pub(crate) mod overlay;
pub(crate) mod packaging;
pub(crate) mod probe;
pub(crate) mod quality;
pub(crate) mod remux;
pub(crate) mod slideshow;
pub(crate) mod stream_ops;
pub(crate) mod subtitles;
pub(crate) mod tempo;
pub(crate) mod thumbnail;
pub(crate) mod transcode;
pub(crate) mod trim;

use std::fs;
use std::io::{self, Read};
use std::path::{Path, PathBuf};
use std::process::{Command, ExitStatus, Stdio};
use std::sync::atomic::{AtomicU64, Ordering};
use std::thread::{self, JoinHandle};
use std::time::{Duration, Instant};

use crate::crates::core::engine::EngineLimits;
use crate::crates::video_engine::error::{VideoError, VideoResult};
use crate::crates::video_engine::model::VideoInfo;

/// Binary used for all media processing.
pub(crate) const FFMPEG: &str = "ffmpeg";
/// Binary used for container/stream inspection.
pub(crate) const FFPROBE: &str = "ffprobe";

/// Hard cap on a single input buffer: 32 GiB. Inputs are already
/// in-memory `&[u8]` at this point, so this is a sanity bound against
/// absurd allocations rather than a disk-space guard.
pub(crate) const MAX_INPUT_BYTES: u64 = 32 * 1024 * 1024 * 1024;

/// Timeout for a single ffmpeg/ffprobe processing invocation.
/// Generous because transcodes of large files are legitimately slow.
pub(crate) const COMMAND_TIMEOUT: Duration = Duration::from_secs(600);

/// Timeout for `ffprobe` metadata reads and `--version`-style probes.
pub(crate) const PROBE_TIMEOUT: Duration = Duration::from_secs(60);

/// How often [`run_capture`] polls `Child::try_wait`.
const POLL_INTERVAL: Duration = Duration::from_millis(20);

/// Result of a completed external command.
pub(crate) struct ProcOutput {
    pub status: ExitStatus,
    pub stdout: Vec<u8>,
    pub stderr: Vec<u8>,
}

/// Why [`run_capture`] failed.
enum RunFailure {
    Io(io::Error),
    Timeout,
}

impl From<io::Error> for RunFailure {
    fn from(e: io::Error) -> Self {
        Self::Io(e)
    }
}

/// An RAII scratch directory removed on drop. Every operation funnels
/// its temp files through this so nothing leaks on error paths.
pub(crate) struct TempDir {
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
        let name = format!("kaladoc-video-{tag}-{}-{nanos}-{seq}", std::process::id());
        let path = std::env::temp_dir().join(name);
        fs::create_dir_all(&path)?;
        Ok(Self { path })
    }

    /// `name` convenience — `path()/name`.
    pub fn file(&self, name: &str) -> PathBuf {
        self.path.join(name)
    }
}

impl Drop for TempDir {
    fn drop(&mut self) {
        let _ = fs::remove_dir_all(&self.path);
    }
}

/// Locate `tool` on `PATH`. Returns the full path of the first
/// executable match, or [`VideoError::MissingTool`].
///
/// The search is a plain directory scan — no shell — so it is cheap
/// enough to call from `health()` probes.
pub(crate) fn find_tool(tool: &str) -> VideoResult<PathBuf> {
    let path_var = std::env::var_os("PATH").unwrap_or_default();
    for dir in std::env::split_paths(&path_var) {
        let candidate = dir.join(tool);
        if is_executable(&candidate) {
            return Ok(candidate);
        }
    }
    Err(VideoError::MissingTool {
        tool: tool.to_string(),
    })
}

/// Whether `tool` resolves on `PATH`. Never fails — absence is `false`.
pub(crate) fn tool_available(tool: &str) -> bool {
    find_tool(tool).is_ok()
}

#[cfg(unix)]
fn is_executable(path: &Path) -> bool {
    use std::os::unix::fs::PermissionsExt;
    path.is_file()
        && std::fs::metadata(path)
            .map(|m| m.permissions().mode() & 0o111 != 0)
            .unwrap_or(false)
}

#[cfg(not(unix))]
fn is_executable(path: &Path) -> bool {
    path.is_file()
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

/// Spawn `program` with `args`, drain stdout/stderr concurrently, and
/// wait up to `timeout`. On expiry the child is killed.
fn run_capture(
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

/// Last ~500 chars of `stderr`, for [`VideoError::ExternalTool`].
/// Trailing newline noise is trimmed first.
pub(crate) fn stderr_tail(stderr: &[u8]) -> String {
    const MAX_CHARS: usize = 500;
    let text = String::from_utf8_lossy(stderr);
    let trimmed = text.trim();
    if trimmed.chars().count() <= MAX_CHARS {
        return trimmed.to_string();
    }
    trimmed
        .chars()
        .rev()
        .take(MAX_CHARS)
        .collect::<Vec<char>>()
        .into_iter()
        .rev()
        .collect()
}

/// Human-readable detail for a failed [`ProcOutput`]: stderr tail,
/// falling back to stdout tail, then a status-only message.
pub(crate) fn failure_detail(output: &ProcOutput, fallback: &str) -> String {
    let tail = stderr_tail(&output.stderr);
    if !tail.is_empty() {
        return tail;
    }
    let tail = stderr_tail(&output.stdout);
    if !tail.is_empty() {
        return tail;
    }
    fallback.to_string()
}

/// [`run_capture`] mapped onto [`VideoError`]: spawn failures become
/// [`VideoError::ExternalTool`], timeouts likewise.
pub(crate) fn run_tool(
    tool: &str,
    program: &Path,
    args: &[String],
    timeout: Duration,
) -> VideoResult<ProcOutput> {
    match run_capture(program, args, timeout) {
        Ok(output) => Ok(output),
        Err(RunFailure::Timeout) => Err(VideoError::ExternalTool {
            tool: tool.to_string(),
            detail: format!("timed out after {}s", timeout.as_secs()),
        }),
        Err(RunFailure::Io(e)) => Err(VideoError::ExternalTool {
            tool: tool.to_string(),
            detail: format!("failed to spawn or wait: {e}"),
        }),
    }
}

/// Build an [`VideoError::ExternalTool`] for a non-zero exit.
pub(crate) fn tool_failure(tool: &str, output: &ProcOutput) -> VideoError {
    VideoError::ExternalTool {
        tool: tool.to_string(),
        detail: failure_detail(output, &format!("exited with {}", output.status)),
    }
}

/// Sanity-check an input buffer: non-empty and within the engine's
/// hard [`MAX_INPUT_BYTES`] cap. Returns the byte length for callers
/// that also enforce `limits.max_input_bytes`.
pub(crate) fn check_input(input: &[u8]) -> VideoResult<u64> {
    if input.is_empty() {
        return Err(VideoError::InvalidInput("input is empty".to_string()));
    }
    let len = input.len() as u64;
    if len > MAX_INPUT_BYTES {
        return Err(VideoError::LimitExceeded(format!(
            "input is {len} bytes, exceeding hard cap of {MAX_INPUT_BYTES} bytes (32 GiB)"
        )));
    }
    Ok(len)
}

/// Enforce the per-call [`EngineLimits`] that apply to timed media:
/// `max_input_bytes`, `max_duration_secs` (when the probe reported a
/// duration), and optionally `max_pixels` (thumbnail decompression-bomb
/// guard, width × height of the probed video stream).
pub(crate) fn check_limits(
    info: &VideoInfo,
    input_len: u64,
    limits: &EngineLimits,
    check_pixels: bool,
) -> VideoResult<()> {
    if let Some(max) = limits.max_input_bytes {
        if input_len > max {
            return Err(VideoError::LimitExceeded(format!(
                "input is {input_len} bytes, exceeding max_input_bytes {max}"
            )));
        }
    }
    if let (Some(max), Some(duration)) = (limits.max_duration_secs, info.duration_secs) {
        if duration > max as f64 {
            return Err(VideoError::LimitExceeded(format!(
                "duration {duration:.3}s exceeds max_duration_secs {max}"
            )));
        }
    }
    if check_pixels {
        if let (Some(max), Some(w), Some(h)) = (limits.max_pixels, info.width, info.height) {
            let pixels = w as u64 * h as u64;
            if pixels > max {
                return Err(VideoError::LimitExceeded(format!(
                    "frame is {w}x{h} = {pixels} pixels, exceeding max_pixels {max}"
                )));
            }
        }
    }
    Ok(())
}

/// Write `bytes` to a scratch file and return its path. The path lives
/// inside a [`TempDir`] the caller must keep alive for the duration of
/// the tool invocation.
pub(crate) fn write_input(dir: &TempDir, name: &str, bytes: &[u8]) -> VideoResult<PathBuf> {
    let path = dir.file(name);
    fs::write(&path, bytes).map_err(VideoError::Io)?;
    Ok(path)
}

/// Read a produced output file back into memory.
pub(crate) fn read_output(path: &Path) -> VideoResult<Vec<u8>> {
    fs::read(path).map_err(VideoError::Io)
}

/// Map a probed ffprobe `format_name` (e.g.
/// `"mov,mp4,m4a,3gp,3g2,mj2"`) to a file extension ffmpeg's muxer
/// selection understands. Returns `None` for containers this engine
/// cannot name — callers must not guess.
pub(crate) fn ext_for_format(format_name: &str) -> Option<&'static str> {
    let lower = format_name.to_ascii_lowercase();
    let tokens: Vec<&str> = lower.split(',').map(str::trim).collect();
    // Prefer the most common/compatible token when a demuxer lists
    // several aliases (mov,mp4,m4a,... all describe the ISO-BMFF
    // family; "mp4" is the safest output extension).
    for preferred in [
        "mp4", "matroska", "webm", "avi", "mpegts", "mpeg", "ogg", "flv", "asf",
    ] {
        if tokens.iter().any(|t| *t == preferred) {
            return Some(match preferred {
                "matroska" => "mkv",
                "mpegts" => "ts",
                "mpeg" => "mpg",
                "asf" => "wmv",
                other => other,
            });
        }
    }
    // Single-token names not in the preferred list (e.g. "mov").
    match tokens.as_slice() {
        ["mov"] => Some("mp4"),
        ["m4v"] => Some("mp4"),
        ["mp3"] => Some("mp3"),
        ["wav"] => Some("wav"),
        _ => None,
    }
}

/// Normalize a caller-supplied target extension: lowercase, trimmed,
/// leading dot stripped.
pub(crate) fn normalize_ext(ext: &str) -> String {
    ext.trim().trim_start_matches('.').to_ascii_lowercase()
}

/// Validate `ext` against `allowed` (already-normalized lowercase
/// names), returning the normalized form or [`VideoError::InvalidInput`].
pub(crate) fn checked_ext<'a>(ext: &str, allowed: &[&str], op: &str) -> VideoResult<String> {
    let norm = normalize_ext(ext);
    if allowed.contains(&norm.as_str()) {
        Ok(norm)
    } else {
        Err(VideoError::InvalidInput(format!(
            "{op} target format '{ext}' is not supported; allowed: {}",
            allowed.join(", ")
        )))
    }
}

/// Pick an H.264 encoder this ffmpeg build actually has, preferring
/// `libx264` then `libopenh264`. Returns `None` when neither exists —
/// callers must surface that as an honest error rather than emitting
/// `-c:v libx264` and failing mid-run.
pub(crate) fn h264_encoder() -> Option<&'static str> {
    for cand in ["libx264", "libopenh264"] {
        if encoder_available(cand) {
            return Some(cand);
        }
    }
    None
}

/// ffmpeg output arguments for a validated transcode target.
/// Returns the argument vector tail to append before the output path.
///
/// `mp4` re-encodes to H.264 (best available encoder) + AAC;
/// `webm` re-encodes to VP9 + Opus — callers must have verified those
/// encoders via [`encoder_available`] first; `mkv`/`avi` stream-copy
/// (`-c copy`) because both are general-purpose containers that accept
/// the common codec set — ffmpeg errors honestly if a codec cannot be
/// muxed there.
pub(crate) fn transcode_args(ext: &str) -> VideoResult<Vec<String>> {
    match ext {
        "mkv" | "avi" => Ok(vec!["-c".into(), "copy".into()]),
        "webm" => Ok(vec![
            "-c:v".into(),
            "libvpx-vp9".into(),
            "-deadline".into(),
            "good".into(),
            "-cpu-used".into(),
            "8".into(),
            "-c:a".into(),
            "libopus".into(),
        ]),
        _ => {
            let enc = h264_encoder().ok_or_else(|| VideoError::ExternalTool {
                tool: FFMPEG.to_string(),
                detail: "mp4 output requires an H.264 encoder (libx264 or libopenh264); \
                         this ffmpeg build provides neither"
                    .to_string(),
            })?;
            Ok(vec![
                "-c:v".into(),
                enc.to_string(),
                "-pix_fmt".into(),
                "yuv420p".into(),
                "-c:a".into(),
                "aac".into(),
            ])
        }
    }
}

/// ffmpeg `-c:v`/`-c:a` argument pairs for a *re-encode* (used by trim
/// with `reencode=true` and concat across mismatched containers when
/// the target is not webm/mkv-copy). Returns an honest error when no
/// H.264 encoder exists.
pub(crate) fn h264_aac_args() -> VideoResult<Vec<String>> {
    let enc = h264_encoder().ok_or_else(|| VideoError::ExternalTool {
        tool: FFMPEG.to_string(),
        detail: "re-encode requires an H.264 encoder (libx264 or libopenh264); \
                 this ffmpeg build provides neither"
            .to_string(),
    })?;
    Ok(vec![
        "-c:v".into(),
        enc.to_string(),
        "-pix_fmt".into(),
        "yuv420p".into(),
        "-c:a".into(),
        "aac".into(),
    ])
}

/// Whether `ffmpeg` was built with the named encoder. Used so `webm`
/// targets fail with an honest message instead of a mid-run ffmpeg
/// error. The encoder list is fetched once and cached; if the probe
/// itself fails we return `false` (callers then surface the ffmpeg
/// error honestly).
pub(crate) fn encoder_available(name: &str) -> bool {
    use std::sync::OnceLock;
    static ENCODERS: OnceLock<Vec<String>> = OnceLock::new();
    let list = ENCODERS.get_or_init(|| {
        let Ok(program) = find_tool(FFMPEG) else {
            return Vec::new();
        };
        let args = vec!["-hide_banner".to_string(), "-encoders".to_string()];
        match run_capture(&program, &args, PROBE_TIMEOUT) {
            Ok(output) if output.status.success() => String::from_utf8_lossy(&output.stdout)
                .lines()
                .filter_map(|line| line.split_whitespace().nth(1))
                .map(str::to_string)
                .collect(),
            _ => Vec::new(),
        }
    });
    list.iter().any(|e| e == name)
}

/// Re-export for ops modules: run `ffmpeg`/`ffprobe` and return the
/// parsed JSON body, or the mapped error. `ffprobe` non-zero exits on
/// non-media input map to [`VideoError::InvalidInput`].
pub(crate) fn probe_json(input_path: &Path) -> VideoResult<serde_json::Value> {
    let program = find_tool(FFPROBE)?;
    let args = vec![
        "-v".to_string(),
        "quiet".to_string(),
        "-print_format".to_string(),
        "json".to_string(),
        "-show_format".to_string(),
        "-show_streams".to_string(),
        "-show_chapters".to_string(),
        input_path.to_string_lossy().into_owned(),
    ];
    let output = run_tool(FFPROBE, &program, &args, PROBE_TIMEOUT)?;
    if !output.status.success() {
        return Err(VideoError::InvalidInput(failure_detail(
            &output,
            "ffprobe could not read the input as media",
        )));
    }
    Ok(serde_json::from_slice(&output.stdout)?)
}
