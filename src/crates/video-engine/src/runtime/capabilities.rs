//! Capability discovery: what this particular ffmpeg build can
//! encode, decode, filter, mux, demux, and talk to.
//!
//! Results are cached in `OnceLock`s — the tool list cannot change
//! during a process lifetime in practice, and probing is several
//! subprocess spawns.

use std::sync::OnceLock;

use crate::crates::video_engine::error::VideoResult;
use crate::crates::video_engine::model::CaptureDevice;
use crate::crates::video_engine::ops::{FFMPEG, PROBE_TIMEOUT, find_tool, run_tool};

/// Everything one ffmpeg/ffprobe build advertises. Names are the
/// exact tokens ffmpeg prints (encoder `libx264`, filter `scale`,
/// muxer `mp4`, protocol `https`, ...).
#[derive(Debug, Clone, Default, serde::Serialize)]
pub struct MediaCapabilities {
    /// Video encoders (`ffmpeg -encoders`, video-only entries).
    pub video_encoders: Vec<String>,
    /// Audio encoders.
    pub audio_encoders: Vec<String>,
    /// Video decoders (`ffmpeg -decoders`).
    pub video_decoders: Vec<String>,
    /// Audio decoders.
    pub audio_decoders: Vec<String>,
    /// Video filters (`ffmpeg -filters`, video-capable entries).
    pub video_filters: Vec<String>,
    /// Audio filters.
    pub audio_filters: Vec<String>,
    /// Muxers (`ffmpeg -muxers`).
    pub muxers: Vec<String>,
    /// Demuxers (`ffmpeg -demuxers`).
    pub demuxers: Vec<String>,
    /// Protocols (`ffmpeg -protocols`).
    pub protocols: Vec<String>,
    /// Hardware accelerations (`ffmpeg -hwaccels`).
    pub hwaccels: Vec<String>,
}

impl MediaCapabilities {
    /// Whether a video encoder exists.
    pub fn has_video_encoder(&self, name: &str) -> bool {
        self.video_encoders.iter().any(|e| e == name)
    }
    /// Whether an audio encoder exists.
    pub fn has_audio_encoder(&self, name: &str) -> bool {
        self.audio_encoders.iter().any(|e| e == name)
    }
    /// Whether a video filter exists.
    pub fn has_video_filter(&self, name: &str) -> bool {
        self.video_filters.iter().any(|f| f == name)
    }
    /// Whether an audio filter exists.
    pub fn has_audio_filter(&self, name: &str) -> bool {
        self.audio_filters.iter().any(|f| f == name)
    }
    /// Whether a muxer exists.
    pub fn has_muxer(&self, name: &str) -> bool {
        self.muxers.iter().any(|m| m == name)
    }
}

/// Full capability snapshot of the installed ffmpeg build. Cached
/// after the first call; `MissingTool` propagates when ffmpeg is
/// absent.
pub fn capabilities() -> VideoResult<MediaCapabilities> {
    // Surface MissingTool eagerly so callers can distinguish "ffmpeg
    // absent" from "ffmpeg present but probing failed".
    find_tool(FFMPEG)?;
    static CAPS: OnceLock<MediaCapabilities> = OnceLock::new();
    Ok(CAPS.get_or_init(probe_all).clone())
}

fn probe_all() -> MediaCapabilities {
    let Ok(program) = find_tool(FFMPEG) else {
        return MediaCapabilities::default();
    };
    MediaCapabilities {
        video_encoders: list_kind(&program, "-encoders", "V"),
        audio_encoders: list_kind(&program, "-encoders", "A"),
        video_decoders: list_kind(&program, "-decoders", "V"),
        audio_decoders: list_kind(&program, "-decoders", "A"),
        video_filters: list_filters(&program, "V"),
        audio_filters: list_filters(&program, "A"),
        muxers: list_names(&program, "-muxers"),
        demuxers: list_names(&program, "-demuxers"),
        protocols: list_names(&program, "-protocols"),
        hwaccels: list_names(&program, "-hwaccels"),
    }
}

/// Run `ffmpeg <flag>` and collect names. For `-encoders`/`-decoders`
/// the second column is flags (V/A/...), the third the name —
/// `kind_filter` selects entries whose flag string contains it.
fn list_kind(program: &std::path::Path, flag: &str, kind_filter: &str) -> Vec<String> {
    let args = vec!["-hide_banner".to_string(), flag.to_string()];
    let Ok(out) = run_tool(FFMPEG, program, &args, PROBE_TIMEOUT) else {
        return Vec::new();
    };
    if !out.status.success() {
        return Vec::new();
    }
    String::from_utf8_lossy(&out.stdout)
        .lines()
        .skip_while(|l| !l.trim_start().starts_with("------"))
        .skip(1)
        .filter_map(|line| {
            let mut cols = line.split_whitespace();
            let flags = cols.next()?;
            let name = cols.next()?;
            if flags.contains(kind_filter) {
                Some(name.to_string())
            } else {
                None
            }
        })
        .collect()
}

/// `ffmpeg -filters` rows are ` T.. name` style — the name is the
/// second whitespace-separated token after the flag letters; kind
/// filtering uses the trailing `(V..)`/`(A..)` capability markers in
/// the description column.
fn list_filters(program: &std::path::Path, kind: &str) -> Vec<String> {
    let args = vec!["-hide_banner".to_string(), "-filters".to_string()];
    let Ok(out) = run_tool(FFMPEG, program, &args, PROBE_TIMEOUT) else {
        return Vec::new();
    };
    if !out.status.success() {
        return Vec::new();
    }
    String::from_utf8_lossy(&out.stdout)
        .lines()
        .filter_map(|line| {
            // Format: " ..C name  description..." — second token is
            // the filter name; capability letters appear later as
            // (V…) / (A…) groups in newer builds or in a flags column.
            let mut cols = line.split_whitespace();
            let _flags = cols.next()?;
            let name = cols.next()?;
            if name.starts_with('-') || name.is_empty() {
                return None;
            }
            let rest = line;
            // Older ffmpeg prints capabilities inline; accept the
            // name when the line mentions the kind marker.
            let has_marker = rest.contains(&format!("({kind}"))
                || rest.contains(&format!(" {kind}.."))
                || rest.contains(&format!("{kind}."));
            // Fallback: when markers are absent, include the name and
            // let callers verify — filters are cheap to attempt.
            if has_marker || !rest.contains('(') {
                Some(name.to_string())
            } else {
                None
            }
        })
        .collect()
}

/// `ffmpeg -<flag>` name lists. `-muxers`/`-demuxers` print a
/// `D`/`E` flag column before the name under a short `---` rule;
/// `-protocols` and `-hwaccels` are plain name lists under section
/// headers — each gets its own parser.
fn list_names(program: &std::path::Path, flag: &str) -> Vec<String> {
    let args = vec!["-hide_banner".to_string(), flag.to_string()];
    let Ok(out) = run_tool(FFMPEG, program, &args, PROBE_TIMEOUT) else {
        return Vec::new();
    };
    if !out.status.success() {
        return Vec::new();
    }
    let text = String::from_utf8_lossy(&out.stdout);
    match flag {
        "-protocols" => parse_protocols(&text),
        "-hwaccels" => parse_hwaccels(&text),
        _ => parse_flag_table(&text),
    }
}

/// True for the `---`/`------` rule line that separates ffmpeg's
/// legend block from the entry rows.
fn is_rule_line(line: &str) -> bool {
    let t = line.trim();
    t.len() >= 3 && t.chars().all(|c| c == '-')
}

/// `-muxers`/`-demuxers`: legend lines, a dashes rule, then rows of
/// `<flags> <name> <description>` — the name is the second token.
fn parse_flag_table(text: &str) -> Vec<String> {
    text.lines()
        .skip_while(|l| !is_rule_line(l))
        .skip(1)
        .take_while(|l| !l.trim().is_empty())
        .filter_map(|line| line.split_whitespace().nth(1).map(str::to_string))
        .collect()
}

/// `-protocols`: `Input:`/`Output:` section headers followed by
/// one-name-per-line entries.
fn parse_protocols(text: &str) -> Vec<String> {
    text.lines()
        .filter_map(|line| {
            let t = line.trim();
            // Single-token lines that aren't section labels.
            if t.is_empty() || t.contains(char::is_whitespace) || t.ends_with(':') {
                return None;
            }
            Some(t.to_string())
        })
        .filter(|name| {
            // Drop header words that are single tokens ("Formats:",
            // "Supported", ...) — protocol names never contain ':'.
            !name.contains(':')
        })
        .collect()
}

/// `-hwaccels`: one header line, then one accelerator name per line.
fn parse_hwaccels(text: &str) -> Vec<String> {
    text.lines()
        .skip(1)
        .map(str::trim)
        .filter(|l| !l.is_empty())
        .map(str::to_string)
        .collect()
}

/// Enumerate capture devices/sources for [`super::hardware`]/
/// [`super::capabilities`] consumers. Kept here so `ffmpeg -devices`
/// parsing lives next to the other list probes.
pub(crate) fn devices() -> Vec<CaptureDevice> {
    let Ok(program) = find_tool(FFMPEG) else {
        return Vec::new();
    };
    let args = vec!["-hide_banner".to_string(), "-devices".to_string()];
    let Ok(out) = run_tool(FFMPEG, &program, &args, PROBE_TIMEOUT) else {
        return Vec::new();
    };
    if !out.status.success() {
        return Vec::new();
    }
    String::from_utf8_lossy(&out.stdout)
        .lines()
        .skip_while(|l| !l.trim_start().starts_with("Devices:"))
        .skip(1)
        .skip_while(|l| l.trim().is_empty())
        .take_while(|l| !l.trim().is_empty())
        .filter_map(|line| {
            let t = line.trim();
            let (kind, rest) = t.split_once(' ')?;
            let name = rest.split_whitespace().next()?.to_string();
            let desc = rest
                .split_once(char::is_whitespace)
                .map(|(_, d)| d.trim().to_string());
            Some(CaptureDevice {
                kind: "device".to_string(),
                name: format!("{kind} {name}"),
                description: desc.filter(|d| !d.is_empty()),
            })
        })
        .collect()
}
