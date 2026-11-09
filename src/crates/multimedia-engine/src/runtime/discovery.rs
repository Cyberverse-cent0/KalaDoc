//! FFmpeg/ffprobe discovery and capability reporting.
//!
//! The engine never assumes a codec, muxer, demuxer, filter or
//! hardware accelerator exists — FFmpeg features depend on the build.
//! [`discover`] probes the binaries once and caches the result;
//! [`Runtime::capabilities`] exposes what this build actually
//! supports so `MediaEngine::capabilities` can advertise honestly.

use std::collections::BTreeSet;
use std::ffi::OsString;
use std::path::{Path, PathBuf};
use std::sync::OnceLock;

use crate::crates::multimedia_engine::error::{MediaError, MediaResult};
use crate::crates::multimedia_engine::model::MediaLimits;
use crate::crates::multimedia_engine::runtime::process;

/// What this machine's FFmpeg installation can do.
#[derive(Debug, Clone, Default)]
pub struct RuntimeCapabilities {
    /// ffmpeg version string (first line of `ffmpeg -version`).
    pub ffmpeg_version: Option<String>,
    /// ffprobe version string.
    pub ffprobe_version: Option<String>,
    /// Encoders (`ffmpeg -encoders` names).
    pub encoders: BTreeSet<String>,
    /// Decoders.
    pub decoders: BTreeSet<String>,
    /// Muxers (output containers).
    pub muxers: BTreeSet<String>,
    /// Demuxers (input containers).
    pub demuxers: BTreeSet<String>,
    /// Filters (`ffmpeg -filters` names, `..` entries removed).
    pub filters: BTreeSet<String>,
    /// Hardware accelerators (`ffmpeg -hwaccels`).
    pub hwaccels: BTreeSet<String>,
    /// Protocols (`ffmpeg -protocols`, excluding `..` entries).
    pub protocols: BTreeSet<String>,
}

impl RuntimeCapabilities {
    /// Whether `name` is a known encoder.
    pub fn has_encoder(&self, name: &str) -> bool {
        self.encoders.contains(name)
    }
    /// Whether `name` is a known decoder.
    pub fn has_decoder(&self, name: &str) -> bool {
        self.decoders.contains(name)
    }
    /// Whether `name` is a known muxer.
    pub fn has_muxer(&self, name: &str) -> bool {
        self.muxers.contains(name)
    }
    /// Whether `name` is a known demuxer.
    pub fn has_demuxer(&self, name: &str) -> bool {
        self.demuxers.contains(name)
    }
    /// Whether `name` is a known filter.
    pub fn has_filter(&self, name: &str) -> bool {
        self.filters.contains(name)
    }
    /// Whether `name` is a known hwaccel.
    pub fn has_hwaccel(&self, name: &str) -> bool {
        self.hwaccels.contains(name)
    }
    /// Whether `name` is a known protocol.
    pub fn has_protocol(&self, name: &str) -> bool {
        self.protocols.contains(name)
    }
}

/// Resolved locations of the FFmpeg tools plus cached capabilities.
#[derive(Debug, Clone)]
pub struct Runtime {
    /// Path to the `ffmpeg` binary.
    pub ffmpeg: PathBuf,
    /// Path to the `ffprobe` binary.
    pub ffprobe: PathBuf,
    /// Cached capability sets (lazily populated by [`Runtime::capabilities`]).
    caps: std::sync::Arc<OnceLock<MediaResult<RuntimeCapabilities>>>,
}

impl Runtime {
    /// Probe `PATH` for `ffmpeg` and `ffprobe`. Fails with
    /// [`MediaError::MissingTool`] when either is absent — the engine
    /// cannot function without both.
    pub fn discover() -> MediaResult<Self> {
        let ffmpeg =
            find_tool("ffmpeg").ok_or_else(|| MediaError::MissingTool("ffmpeg".to_string()))?;
        let ffprobe =
            find_tool("ffprobe").ok_or_else(|| MediaError::MissingTool("ffprobe".to_string()))?;
        Ok(Self {
            ffmpeg,
            ffprobe,
            caps: std::sync::Arc::new(OnceLock::new()),
        })
    }

    /// Capability sets, probed once and cached. Uses a minimal timeout
    /// — listing codecs is fast even on slow machines.
    pub fn capabilities(&self) -> MediaResult<&RuntimeCapabilities> {
        match self.caps.get_or_init(|| self.probe_capabilities()) {
            Ok(caps) => Ok(caps),
            Err(e) => Err(MediaError::Probe(e.to_string())),
        }
    }

    fn probe_capabilities(&self) -> MediaResult<RuntimeCapabilities> {
        let limits = MediaLimits {
            timeout: std::time::Duration::from_secs(30),
            ..MediaLimits::default()
        };
        let mut caps = RuntimeCapabilities::default();

        caps.ffmpeg_version = first_line(&self.ffmpeg, &[OsString::from("-version")], &limits)?;
        caps.ffprobe_version = first_line(&self.ffprobe, &[OsString::from("-version")], &limits)?;

        caps.encoders = list_names(&self.ffmpeg, &[OsString::from("-encoders")], &limits)?;
        caps.decoders = list_names(&self.ffmpeg, &[OsString::from("-decoders")], &limits)?;
        caps.muxers = list_names(&self.ffmpeg, &[OsString::from("-muxers")], &limits)?;
        caps.demuxers = list_names(&self.ffmpeg, &[OsString::from("-demuxers")], &limits)?;
        caps.filters = list_names(&self.ffmpeg, &[OsString::from("-filters")], &limits)?;
        caps.hwaccels = list_names(&self.ffmpeg, &[OsString::from("-hwaccels")], &limits)?;
        caps.protocols = list_names(&self.ffmpeg, &[OsString::from("-protocols")], &limits)?;

        // `..` marks "decoding/encoding only" pseudo-entries — not real
        // names callers can pass.
        for set in [
            &mut caps.encoders,
            &mut caps.decoders,
            &mut caps.muxers,
            &mut caps.demuxers,
            &mut caps.filters,
            &mut caps.hwaccels,
            &mut caps.protocols,
        ] {
            set.remove("..");
        }
        Ok(caps)
    }
}

/// Locate `name` on `PATH`.
pub fn find_tool(name: &str) -> Option<PathBuf> {
    let path_env = std::env::var_os("PATH")?;
    for dir in std::env::split_paths(&path_env) {
        let candidate = dir.join(name);
        if candidate.is_file() {
            return Some(candidate);
        }
    }
    None
}

/// First line of `<program> <args>` stdout/stderr (version banners go
/// to stdout for `-version`).
fn first_line(
    program: &Path,
    args: &[OsString],
    limits: &MediaLimits,
) -> MediaResult<Option<String>> {
    let out = process::run("ffmpeg", program, args, None, limits)?;
    let text = if out.stdout.is_empty() {
        out.stderr
    } else {
        String::from_utf8_lossy(&out.stdout).into_owned()
    };
    Ok(text.lines().next().map(|l| l.trim().to_string()))
}

/// Parse `ffmpeg -<kind>s` listing output: lines like
/// ` V....D aac                  AAC (Advanced Audio Coding)`.
/// We keep the second whitespace-separated token as the name.
fn list_names(
    program: &Path,
    args: &[OsString],
    limits: &MediaLimits,
) -> MediaResult<BTreeSet<String>> {
    let out = process::run("ffmpeg", program, args, None, limits)?;
    let text = String::from_utf8_lossy(&out.stdout);
    let mut names = BTreeSet::new();
    for line in text.lines() {
        let mut parts = line.split_whitespace();
        let _flags = parts.next();
        if let Some(name) = parts.next()
            && !name.starts_with('-')
            && name != "(a)"
        {
            names.insert(name.to_string());
        }
    }
    Ok(names)
}
