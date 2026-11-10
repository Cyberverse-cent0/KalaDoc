//! Device/source/protocol enumeration. This engine does NOT perform
//! live capture in v1 — it reports what the ffmpeg build *could*
//! capture so callers and future versions can decide.
//!
//! Coverage notes (ffmpeg 7.1.5):
//!
//! - `ffmpeg -devices` prints one unified table: a 2-character flag
//!   column (`D` = demuxing/indev, `E` = muxing/outdev), the device
//!   name (aliases comma-separated), and a free-text description.
//!   Older builds split this into "Autodetected devices:",
//!   "Supported indevs:" and "Supported outdevs:" sections — the
//!   parser accepts both layouts. Devices flagged `DE` are emitted
//!   once as `kind="indev"` and once as `kind="outdev"`.
//! - There is no `-sources` option in 7.x; real capture sources are
//!   only enumerable through the device list above plus the platform
//!   demuxers `avfoundation` (macOS) and `dshow` (Windows), which
//!   enumerate their devices on stderr when invoked with
//!   `-list_devices`. Both are attempted unconditionally — on other
//!   platforms they fail instantly with "Unknown input format" and
//!   are skipped silently. Linux has no equivalent list mechanism
//!   (`x11grab`, `v4l2`, `alsa`, `pulse`, `kmsgrab` expose no device
//!   enumeration), so coverage there is limited to the compiled-in
//!   device names.
//! - `ffmpeg -protocols` lists protocol names under `Input:` and
//!   `Output:` headings with no description column; the direction
//!   is folded into `description` (e.g. `"input,output"`).
//! - `ffmpeg -hwaccels` is a bare name list under a
//!   `Hardware acceleration methods:` header.

use std::collections::BTreeMap;
use std::path::Path;
use std::sync::OnceLock;

use crate::crates::video_engine::error::{VideoError, VideoResult};
use crate::crates::video_engine::model::CaptureDevice;
use crate::crates::video_engine::ops::{FFMPEG, PROBE_TIMEOUT, find_tool, h264_encoder, run_tool};

/// Split a `-devices` data line into (demux_flag, mux_flag, name,
/// description). Returns `None` for headers, separators and blanks.
///
/// Handles both the unified 7.x table (`DE name  description`) and
/// the legacy sectioned layout where flag columns are absent and the
/// kind comes from the surrounding header instead.
fn parse_device_line(line: &str) -> Option<(bool, bool, &str, Option<&str>)> {
    let trimmed_end = line.trim_end();
    if trimmed_end.is_empty() || trimmed_end.ends_with(':') {
        return None; // blank lines and section headers ("Devices:", …)
    }
    let bytes = trimmed_end.as_bytes();
    // Flag columns occupy the first two bytes when present: a 2-char
    // field of 'D' (demuxing), 'E' (muxing), '.' (unsupported) or
    // ' ' padding, followed by a separating space — e.g. "DE ",
    // " D ", "  E ".
    let flag_char = |b: u8| matches!(b, b'D' | b'E' | b'.' | b' ');
    let has_flags = bytes.len() >= 3
        && flag_char(bytes[0])
        && flag_char(bytes[1])
        && bytes[2] == b' '
        && (bytes[0] == b'D' || bytes[0] == b'E' || bytes[1] == b'D' || bytes[1] == b'E');
    let (demux, mux, rest) = if has_flags {
        (
            bytes[0] == b'D' || bytes[1] == b'D',
            bytes[0] == b'E' || bytes[1] == b'E',
            trimmed_end[3..].trim_start(),
        )
    } else {
        (false, false, trimmed_end.trim_start())
    };
    // Device names never contain spaces; the description (when present)
    // follows the first whitespace run — a single space when the name
    // exceeds ffmpeg's name column width.
    let (name, description) = match rest.find(char::is_whitespace) {
        Some(idx) => {
            let name = rest[..idx].trim();
            let desc = rest[idx..].trim();
            (name, if desc.is_empty() { None } else { Some(desc) })
        }
        None => (rest.trim(), None),
    };
    if name.is_empty()
        || name.contains('=')
        || name.chars().all(|c| c == '-' || c == '=')
        || description.is_some_and(|d| d.starts_with('='))
    {
        return None; // legend lines like "D. = Demuxing supported", "---"
    }
    Some((demux, mux, name, description))
}

/// Enumerate capture-capable devices and sources compiled into this
/// ffmpeg build: `ffmpeg -devices` (indevs/outdevs) plus platform
/// sources enumerable via `avfoundation -list_devices` (macOS) and
/// `dshow -list_devices` (Windows). See module docs for coverage.
pub(crate) fn list_devices() -> VideoResult<Vec<CaptureDevice>> {
    let ffmpeg = find_tool(FFMPEG)?;
    let output = run_tool(
        FFMPEG,
        &ffmpeg,
        &["-hide_banner".to_string(), "-devices".to_string()],
        PROBE_TIMEOUT,
    )?;
    if !output.status.success() {
        return Err(crate::crates::video_engine::ops::tool_failure(
            FFMPEG, &output,
        ));
    }

    let mut devices = Vec::new();
    // Section context for legacy "Supported indevs/outdevs" layouts.
    let mut section_indev = false;
    let mut section_outdev = false;
    for line in String::from_utf8_lossy(&output.stdout).lines() {
        let lower = line.to_lowercase();
        if lower.contains("indev") {
            section_indev = true;
            section_outdev = false;
            continue;
        }
        if lower.contains("outdev") {
            section_outdev = true;
            section_indev = false;
            continue;
        }
        if lower.contains("autodetected") {
            section_indev = false;
            section_outdev = false;
            continue;
        }
        let Some((demux, mux, name, description)) = parse_device_line(line) else {
            continue;
        };
        // Skip the flag legend lines (" D. = Demuxing supported").
        if name.starts_with('=') {
            continue;
        }
        let (is_indev, is_outdev) = if demux || mux {
            (demux, mux)
        } else {
            (section_indev, section_outdev)
        };
        for (kind, enabled) in [("indev", is_indev), ("outdev", is_outdev)] {
            if enabled {
                devices.push(CaptureDevice {
                    kind: kind.to_string(),
                    name: name.to_string(),
                    description: description.map(str::to_string),
                });
            }
        }
    }

    // Platform demuxers that self-enumerate on stderr. Absent on
    // this build ("Unknown input format") — failures are ignored.
    devices.extend(list_avfoundation(&ffmpeg));
    devices.extend(list_dshow(&ffmpeg));

    Ok(devices)
}

/// Parse `ffmpeg -f avfoundation -list_devices true -i ""` stderr:
/// lines like `[AVFoundation indev @ 0x…] [0] FaceTime HD Camera`
/// after the `AVFoundation video devices:` / `audio devices:` headers.
/// Returns an empty vec on any failure or on non-macOS builds.
fn list_avfoundation(ffmpeg: &Path) -> Vec<CaptureDevice> {
    let args = [
        "-hide_banner".to_string(),
        "-f".to_string(),
        "avfoundation".to_string(),
        "-list_devices".to_string(),
        "true".to_string(),
        "-i".to_string(),
        String::new(),
    ];
    let Ok(output) = run_tool(FFMPEG, ffmpeg, &args, PROBE_TIMEOUT) else {
        return Vec::new();
    };
    let stderr = String::from_utf8_lossy(&output.stderr);
    let mut devices = Vec::new();
    let mut in_list = false;
    let mut is_audio = false;
    for line in stderr.lines() {
        if line.contains("AVFoundation video devices:") {
            in_list = true;
            is_audio = false;
            continue;
        }
        if line.contains("AVFoundation audio devices:") {
            in_list = true;
            is_audio = true;
            continue;
        }
        if !in_list {
            continue;
        }
        // Entry lines look like `[AVFoundation indev @ 0x…] [3] Name`.
        if let Some(rest) = line.rsplit_once("] [") {
            if let Some((idx, name)) = rest.1.split_once("] ") {
                if !idx.is_empty() && idx.chars().all(|c| c.is_ascii_digit()) {
                    let name = name.trim();
                    if !name.is_empty() {
                        devices.push(CaptureDevice {
                            kind: "indev".to_string(),
                            name: name.to_string(),
                            description: Some(if is_audio {
                                "AVFoundation audio device".to_string()
                            } else {
                                "AVFoundation video device".to_string()
                            }),
                        });
                    }
                }
            }
        }
    }
    devices
}

/// Parse `ffmpeg -f dshow -list_devices true -i dummy` stderr: quoted
/// device names under the `DirectShow video devices` / `DirectShow
/// audio devices` headers. Returns an empty vec on failure.
fn list_dshow(ffmpeg: &Path) -> Vec<CaptureDevice> {
    let args = [
        "-hide_banner".to_string(),
        "-f".to_string(),
        "dshow".to_string(),
        "-list_devices".to_string(),
        "true".to_string(),
        "-i".to_string(),
        "dummy".to_string(),
    ];
    let Ok(output) = run_tool(FFMPEG, ffmpeg, &args, PROBE_TIMEOUT) else {
        return Vec::new();
    };
    let stderr = String::from_utf8_lossy(&output.stderr);
    let mut devices = Vec::new();
    let mut in_list = false;
    let mut is_audio = false;
    for line in stderr.lines() {
        if line.contains("DirectShow video devices") {
            in_list = true;
            is_audio = false;
            continue;
        }
        if line.contains("DirectShow audio devices") {
            in_list = true;
            is_audio = true;
            continue;
        }
        if !in_list {
            continue;
        }
        // Entry lines contain a quoted device name: `"Device Name"`.
        if let (Some(start), Some(end)) = (line.find('"'), line.rfind('"')) {
            if end > start + 1 {
                let name = line[start + 1..end].trim();
                if !name.is_empty() {
                    devices.push(CaptureDevice {
                        kind: "indev".to_string(),
                        name: name.to_string(),
                        description: Some(if is_audio {
                            "DirectShow audio device".to_string()
                        } else {
                            "DirectShow video device".to_string()
                        }),
                    });
                }
            }
        }
    }
    devices
}

/// List input/output protocols the build supports (`ffmpeg
/// -protocols`), folded into one entry per protocol name with the
/// direction(s) recorded in `description`. Callers filter as needed.
pub(crate) fn list_protocols() -> VideoResult<Vec<CaptureDevice>> {
    let ffmpeg = find_tool(FFMPEG)?;
    let output = run_tool(
        FFMPEG,
        &ffmpeg,
        &["-hide_banner".to_string(), "-protocols".to_string()],
        PROBE_TIMEOUT,
    )?;
    if !output.status.success() {
        return Err(crate::crates::video_engine::ops::tool_failure(
            FFMPEG, &output,
        ));
    }

    // name -> (seen_input, seen_output), BTreeMap for stable order.
    let mut protocols: BTreeMap<String, (bool, bool)> = BTreeMap::new();
    let mut direction = "";
    for line in String::from_utf8_lossy(&output.stdout).lines() {
        let trimmed = line.trim();
        if trimmed == "Input:" {
            direction = "input";
            continue;
        }
        if trimmed == "Output:" {
            direction = "output";
            continue;
        }
        // Protocol names are bare single tokens; skip headers, the
        // "Supported file protocols:" title, and blank lines.
        if trimmed.is_empty() || trimmed.contains(':') || trimmed.chars().any(char::is_whitespace) {
            continue;
        }
        if direction.is_empty() {
            continue;
        }
        let entry = protocols.entry(trimmed.to_string()).or_default();
        match direction {
            "input" => entry.0 = true,
            "output" => entry.1 = true,
            _ => {}
        }
    }

    Ok(protocols
        .into_iter()
        .map(|(name, (input, output))| {
            let description = match (input, output) {
                (true, true) => "input,output",
                (true, false) => "input",
                (false, true) => "output",
                (false, false) => "",
            };
            CaptureDevice {
                kind: "protocol".to_string(),
                name,
                description: if description.is_empty() {
                    None
                } else {
                    Some(description.to_string())
                },
            }
        })
        .collect())
}

/// List hardware accelerations (`ffmpeg -hwaccels`) as
/// [`CaptureDevice`] entries of kind `"hwaccel"`. An empty list is a
/// valid result for CPU-only builds.
pub(crate) fn list_hwaccels() -> VideoResult<Vec<CaptureDevice>> {
    let ffmpeg = find_tool(FFMPEG)?;
    let output = run_tool(
        FFMPEG,
        &ffmpeg,
        &["-hide_banner".to_string(), "-hwaccels".to_string()],
        PROBE_TIMEOUT,
    )?;
    if !output.status.success() {
        return Err(crate::crates::video_engine::ops::tool_failure(
            FFMPEG, &output,
        ));
    }
    Ok(String::from_utf8_lossy(&output.stdout)
        .lines()
        .map(str::trim)
        .filter(|line| !line.is_empty() && *line != "Hardware acceleration methods:")
        .map(|name| CaptureDevice {
            kind: "hwaccel".to_string(),
            name: name.to_string(),
            description: None,
        })
        .collect())
}

/// Preference order for [`detect_hwaccel`]: discrete GPU APIs first,
/// then integrated/other backends.
const HWACCEL_PREFERENCE: &[&str] = &[
    "cuda",
    "vaapi",
    "videotoolbox",
    "qsv",
    "amf",
    "dxva2",
    "d3d11va",
    "opencl",
];

/// Cached hwaccel names; `None` entries (ffmpeg missing/failed) are
/// cached as an empty vec so we only probe once per process.
static HWACCELS: OnceLock<Vec<String>> = OnceLock::new();

fn hwaccel_names() -> &'static [String] {
    HWACCELS.get_or_init(|| {
        list_hwaccels()
            .map(|devices| devices.into_iter().map(|d| d.name).collect())
            .unwrap_or_default()
    })
}

/// Pick the best available hardware acceleration for this host:
/// probe `ffmpeg -hwaccels` and prefer `cuda`/`vaapi`/`videotoolbox`/
/// `qsv`/`amf`/`dxva2`/`d3d11va`/`opencl` in that order. `None` when
/// nothing is usable — callers must then run software-only.
pub(crate) fn detect_hwaccel() -> Option<String> {
    let names = hwaccel_names();
    HWACCEL_PREFERENCE
        .iter()
        .find(|preferred| names.iter().any(|n| n == *preferred))
        .map(|s| (*s).to_string())
}

// ------------------------------------------------------------------
// Capture command construction (pure arg builders — no execution)
// ------------------------------------------------------------------
//
// Real device capture needs hardware this engine cannot assume, so
// these functions only *build and validate* ffmpeg argument vectors.
// The job/caller layer decides whether and where to execute them.
// Every builder re-validates its device strings so a crafted name
// cannot inject options or smuggle shell syntax (argv is passed
// without a shell, but ffmpeg itself parses some metacharacters in
// device specs — e.g. dshow's `audio=`/`video=` prefixes).

/// Capture backend/demuxer family for [`CaptureSource`]. Maps to the
/// ffmpeg `-f <name>` input format.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[allow(dead_code)]
pub(crate) enum CaptureFormat {
    /// Linux V4L2 video device (`/dev/videoN`).
    V4l2,
    /// Windows DirectShow (`dshow`); device is a friendly name.
    Dshow,
    /// macOS AVFoundation; device is `"<video_idx>:<audio_idx>"`.
    AvFoundation,
    /// ALSA audio device (`hw:0,0`, `default`, ...).
    Alsa,
    /// PulseAudio/PipeWire sink/source.
    Pulse,
    /// X11 screen grab (`:0.0`).
    X11Grab,
    /// KMS/DRM framebuffer (`/dev/dri/cardN`).
    KmsGrab,
}

#[allow(dead_code)]
impl CaptureFormat {
    /// ffmpeg `-f` name.
    fn as_str(self) -> &'static str {
        match self {
            Self::V4l2 => "v4l2",
            Self::Dshow => "dshow",
            Self::AvFoundation => "avfoundation",
            Self::Alsa => "alsa",
            Self::Pulse => "pulse",
            Self::X11Grab => "x11grab",
            Self::KmsGrab => "kmsgrab",
        }
    }

    /// Sensible default capture backend for this host OS.
    pub(crate) fn for_host() -> Self {
        if cfg!(windows) {
            Self::Dshow
        } else if cfg!(target_os = "macos") {
            Self::AvFoundation
        } else {
            Self::V4l2
        }
    }

    /// Whether this format captures video (vs. audio-only).
    fn is_video(self) -> bool {
        !matches!(self, Self::Alsa | Self::Pulse)
    }
}

/// A capture input: which demuxer family, which device, and an
/// optional second (audio) input.
#[derive(Debug, Clone)]
#[allow(dead_code)]
pub(crate) struct CaptureSource {
    /// Primary input format/device kind.
    pub format: CaptureFormat,
    /// Device specifier — see [`validate_capture_device`] for the
    /// per-format grammar.
    pub device: String,
    /// Optional audio input device, used as a second `-i`.
    pub audio_device: Option<String>,
    /// Format of `audio_device` when it differs from a same-family
    /// pairing (e.g. v4l2 video + alsa audio). `None` reuses `format`.
    pub audio_format: Option<CaptureFormat>,
}

/// Input-side capture options (resolution/framerate/sample-rate are
/// ffmpeg *input* options — they must precede `-i`).
#[derive(Debug, Clone, Default)]
#[allow(dead_code)]
pub(crate) struct CaptureInputOpts {
    /// Frame width in pixels (`-video_size`).
    pub width: Option<u32>,
    /// Frame height in pixels (`-video_size`).
    pub height: Option<u32>,
    /// Capture framerate (`-framerate`).
    pub fps: Option<u32>,
    /// Audio sample rate for audio inputs (`-ar`).
    pub sample_rate: Option<u32>,
}

/// Output-side capture options (duration limit, scaling, codec).
#[derive(Debug, Clone)]
#[allow(dead_code)]
pub(crate) struct CaptureOutputOpts {
    /// Stop after N seconds (`-t`).
    pub duration_secs: Option<u32>,
    /// Output frame width (`-s` scale). Both `out_width`/`out_height`
    /// must be set together or neither.
    pub out_width: Option<u32>,
    /// Output frame height (`-s` scale).
    pub out_height: Option<u32>,
    /// Output framerate (`-r`).
    pub out_fps: Option<u32>,
    /// Video codec name; `None` → best available H.264 encoder
    /// ([`h264_encoder`]); `"copy"` stream-copies.
    pub vcodec: Option<String>,
    /// Audio codec name; `None` → `"aac"` (or `"copy"` when the video
    /// codec is `"copy"` so nothing is re-encoded).
    pub acodec: Option<String>,
    /// Audio bitrate (`-b:a`), e.g. `"128k"`.
    pub audio_bitrate: Option<String>,
    /// Whether to include audio at all (`false` → `-an`).
    pub include_audio: bool,
}

impl Default for CaptureOutputOpts {
    fn default() -> Self {
        Self {
            duration_secs: None,
            out_width: None,
            out_height: None,
            out_fps: None,
            vcodec: None,
            acodec: None,
            audio_bitrate: None,
            include_audio: true,
        }
    }
}

/// Characters never allowed in a capture device spec: ASCII
/// control/whitespace plus shell/ffmpeg metacharacters. Device names
/// legitimately contain spaces (dshow friendly names) so space is
/// handled per-format instead of here.
#[allow(dead_code)]
fn device_has_dangerous_chars(device: &str) -> bool {
    device.chars().any(|c| {
        c.is_control()
            || matches!(
                c,
                '\'' | '"'
                    | '\\'
                    | ';'
                    | '|'
                    | '<'
                    | '>'
                    | '`'
                    | '$'
                    | '{'
                    | '}'
                    | '['
                    | ']'
                    | '*'
                    | '!'
            )
    })
}

/// Validate a capture device string for `format`. Returns
/// [`VideoError::InvalidInput`] on anything that does not match the
/// format's expected grammar.
#[allow(dead_code)]
fn validate_capture_device(format: CaptureFormat, device: &str) -> VideoResult<()> {
    if device.is_empty() {
        return Err(VideoError::InvalidInput(format!(
            "{:?} capture device is empty",
            format
        )));
    }
    let bad =
        || VideoError::InvalidInput(format!("invalid {:?} device specifier: {device}", format));
    match format {
        // /dev/video0, /dev/dri/card0, ... — absolute device path.
        CaptureFormat::V4l2 | CaptureFormat::KmsGrab => {
            if device_has_dangerous_chars(device)
                || !device.starts_with("/dev/")
                || device.len() <= "/dev/".len()
            {
                return Err(bad());
            }
        }
        // ALSA/Pulse names: "default", "hw:0,0", "plughw:1,0",
        // "alsa_output.*". No whitespace or metacharacters.
        CaptureFormat::Alsa | CaptureFormat::Pulse => {
            if device_has_dangerous_chars(device)
                || device.chars().any(char::is_whitespace)
                || !device
                    .chars()
                    .all(|c| c.is_ascii_alphanumeric() || matches!(c, ':' | ',' | '.' | '_' | '-'))
            {
                return Err(bad());
            }
        }
        // X11 display: ":0", ":0.0", "hostname:0.0" — an optional
        // host prefix, then display[.screen] as digits.
        CaptureFormat::X11Grab => {
            let ok = !device_has_dangerous_chars(device)
                && device
                    .chars()
                    .all(|c| c.is_ascii_alphanumeric() || matches!(c, ':' | '.' | '-' | '_'))
                && device.contains(':')
                && device.split(':').count() <= 2
                && device.split(':').enumerate().all(|(i, part)| {
                    if part.is_empty() {
                        return i == 0; // empty host before ':' is fine (":0")
                    }
                    if i == 0 {
                        // host part: no ':' left, alnum/.-_ only
                        return true;
                    }
                    // display[.screen]: digits with at most one dot
                    let mut dots = 0;
                    part.chars().all(|c| {
                        if c == '.' {
                            dots += 1;
                            dots <= 1
                        } else {
                            c.is_ascii_digit()
                        }
                    }) && part.chars().any(|c| c.is_ascii_digit())
                });
            if !ok {
                return Err(bad());
            }
        }
        // AVFoundation: "<video_idx>:<audio_idx>" where each side is a
        // decimal index or "none" (e.g. "0:none", "1:2").
        CaptureFormat::AvFoundation => {
            let ok = !device_has_dangerous_chars(device)
                && device.split(':').count() == 2
                && device.split(':').all(|part| {
                    part == "none" || (!part.is_empty() && part.chars().all(|c| c.is_ascii_digit()))
                });
            if !ok {
                return Err(bad());
            }
        }
        // DirectShow friendly names may contain spaces and most
        // punctuation; quotes/backslashes would break ffmpeg's own
        // `video=`/`audio=` option parsing, so they stay banned.
        CaptureFormat::Dshow => {
            if device_has_dangerous_chars(device)
                || device.chars().any(|c| c.is_whitespace() && c != ' ')
            {
                return Err(bad());
            }
        }
    }
    Ok(())
}

/// Build the input-side ffmpeg arguments for `source` + `opts`:
/// `["-f", <fmt>, <input options>, "-i", <device>, ...]`. Pure — no
/// process is spawned. Validation failures are `InvalidInput`.
///
/// For video formats the input options are `-video_size WxH` and
/// `-framerate N` (both or neither for size; they precede `-i`). For
/// audio formats (`alsa`, `pulse`) `-ar` sets the sample rate. When
/// `source.audio_device` is set, a second input block is appended
/// with its own `-f`/options/`-i`.
#[allow(dead_code)]
pub(crate) fn capture_args(
    source: &CaptureSource,
    opts: &CaptureInputOpts,
) -> VideoResult<Vec<String>> {
    validate_capture_device(source.format, &source.device)?;
    match (opts.width, opts.height) {
        (Some(w), Some(h)) => {
            if !(1..=16384).contains(&w) {
                return Err(VideoError::InvalidInput(format!(
                    "capture width {w} out of range 1..=16384"
                )));
            }
            if !(1..=16384).contains(&h) {
                return Err(VideoError::InvalidInput(format!(
                    "capture height {h} out of range 1..=16384"
                )));
            }
        }
        (None, None) => {}
        _ => {
            return Err(VideoError::InvalidInput(
                "capture width and height must be set together".to_string(),
            ));
        }
    }
    if let Some(fps) = opts.fps {
        if !(1..=240).contains(&fps) {
            return Err(VideoError::InvalidInput(format!(
                "capture fps {fps} out of range 1..=240"
            )));
        }
    }
    if let Some(ar) = opts.sample_rate {
        if !(1000..=384000).contains(&ar) {
            return Err(VideoError::InvalidInput(format!(
                "capture sample_rate {ar} out of range 1000..=384000"
            )));
        }
    }

    let mut args = Vec::new();
    push_input_block(
        &mut args,
        source.format,
        &source.device,
        opts.width.unwrap_or(0),
        opts.height.unwrap_or(0),
        opts.fps,
        opts.sample_rate,
    );

    if let Some(audio_dev) = &source.audio_device {
        let audio_format = source.audio_format.unwrap_or(source.format);
        validate_capture_device(audio_format, audio_dev)?;
        push_input_block(
            &mut args,
            audio_format,
            audio_dev,
            // Video size/framerate apply to the primary video input
            // only; audio inputs get the sample-rate option.
            0,
            0,
            None,
            opts.sample_rate,
        );
    }
    Ok(args)
}

/// Append `["-f", fmt, <opts>, "-i", device]` for one input.
#[allow(dead_code)]
fn push_input_block(
    args: &mut Vec<String>,
    format: CaptureFormat,
    device: &str,
    width: u32,
    height: u32,
    fps: Option<u32>,
    sample_rate: Option<u32>,
) {
    args.push("-f".to_string());
    args.push(format.as_str().to_string());
    if format.is_video() {
        if width > 0 && height > 0 {
            args.extend(["-video_size".to_string(), format!("{width}x{height}")]);
        }
        if let Some(fps) = fps {
            args.extend(["-framerate".to_string(), fps.to_string()]);
        }
    } else if let Some(ar) = sample_rate {
        args.extend(["-ar".to_string(), ar.to_string()]);
    }
    args.push("-i".to_string());
    args.push(device.to_string());
}

/// Build the output-side ffmpeg arguments for `opts`:
/// `[-t secs] [-s WxH] [-r fps] <-c:v ...> <-c:a ...> [-b:a ...] [-an]`.
/// Pure — no process is spawned.
///
/// Codec defaults: video → best available H.264 encoder
/// ([`h264_encoder`]) with `yuv420p`; `"copy"` passes through
/// untouched. Audio → `"aac"`, or `"copy"` when video is `"copy"` so a
/// stream-copy capture never silently re-encodes. `include_audio:
/// false` emits `-an` and suppresses audio codec arguments.
#[allow(dead_code)]
pub(crate) fn capture_output_args(opts: &CaptureOutputOpts) -> VideoResult<Vec<String>> {
    let mut args = Vec::new();

    if let Some(secs) = opts.duration_secs {
        if !(1..=86400).contains(&secs) {
            return Err(VideoError::InvalidInput(format!(
                "capture duration {secs}s out of range 1..=86400"
            )));
        }
        args.extend(["-t".to_string(), secs.to_string()]);
    }
    match (opts.out_width, opts.out_height) {
        (Some(w), Some(h)) => {
            if !(1..=16384).contains(&w) || !(1..=16384).contains(&h) {
                return Err(VideoError::InvalidInput(format!(
                    "capture output size {w}x{h} out of range"
                )));
            }
            args.extend(["-s".to_string(), format!("{w}x{h}")]);
        }
        (None, None) => {}
        _ => {
            return Err(VideoError::InvalidInput(
                "capture output width and height must be set together".to_string(),
            ));
        }
    }
    if let Some(fps) = opts.out_fps {
        if !(1..=240).contains(&fps) {
            return Err(VideoError::InvalidInput(format!(
                "capture output fps {fps} out of range 1..=240"
            )));
        }
        args.extend(["-r".to_string(), fps.to_string()]);
    }

    let video_copy = opts.vcodec.as_deref() == Some("copy");
    match opts.vcodec.as_deref() {
        Some("copy") => {
            args.extend(["-c:v".to_string(), "copy".to_string()]);
        }
        Some(name) => {
            args.extend(["-c:v".to_string(), name.to_string()]);
        }
        None => {
            let enc = h264_encoder().ok_or_else(|| VideoError::ExternalTool {
                tool: FFMPEG.to_string(),
                detail: "capture output requires an H.264 encoder (libx264 or \
                         libopenh264); this ffmpeg build provides neither"
                    .to_string(),
            })?;
            args.extend([
                "-c:v".to_string(),
                enc.to_string(),
                "-pix_fmt".to_string(),
                "yuv420p".to_string(),
            ]);
        }
    }

    if !opts.include_audio {
        args.push("-an".to_string());
    } else {
        let acodec = opts.acodec.clone().unwrap_or_else(|| {
            if video_copy {
                "copy".to_string()
            } else {
                "aac".to_string()
            }
        });
        args.extend(["-c:a".to_string(), acodec]);
        if let Some(b) = &opts.audio_bitrate {
            args.extend(["-b:a".to_string(), b.clone()]);
        }
    }
    Ok(args)
}

/// Full ffmpeg argv for a capture session:
/// `["-hide_banner", "-y", <capture_args>, <capture_output_args>,
/// <output_path>]`. Pure builder — callers execute (or don't) at
/// their own risk layer. `output_path` must be non-empty and free of
/// control characters.
#[allow(dead_code)]
pub(crate) fn capture_argv(
    source: &CaptureSource,
    in_opts: &CaptureInputOpts,
    out_opts: &CaptureOutputOpts,
    output_path: &str,
) -> VideoResult<Vec<String>> {
    if output_path.is_empty() || output_path.chars().any(char::is_control) {
        return Err(VideoError::InvalidInput(
            "capture output path is empty or contains control characters".to_string(),
        ));
    }
    let mut args = vec!["-hide_banner".to_string(), "-y".to_string()];
    args.extend(capture_args(source, in_opts)?);
    args.extend(capture_output_args(out_opts)?);
    args.push(output_path.to_string());
    Ok(args)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_unified_device_table() {
        let line = "DE video4linux2,v4l2 Video4Linux2 output device";
        let (d, m, name, desc) = parse_device_line(line).unwrap();
        assert!(d && m);
        assert_eq!(name, "video4linux2,v4l2");
        assert_eq!(desc, Some("Video4Linux2 output device"));
    }

    #[test]
    fn parses_flags_and_empty_description() {
        let line = " D  libcdio          ";
        let (d, m, name, desc) = parse_device_line(line).unwrap();
        assert!(d && !m);
        assert_eq!(name, "libcdio");
        assert_eq!(desc, None);
    }

    #[test]
    fn skips_headers_and_legend() {
        assert!(parse_device_line("Devices:").is_none());
        assert!(parse_device_line(" D. = Demuxing supported").is_none());
        assert!(parse_device_line("---").is_none());
        assert!(parse_device_line("").is_none());
    }

    #[test]
    fn ffmpeg_enumeration_smoke() {
        if !crate::crates::video_engine::ops::tool_available(FFMPEG) {
            return; // ffmpeg absent: MissingTool paths can't be tested here.
        }

        let devices = list_devices().expect("list_devices");
        assert!(!devices.is_empty());
        assert!(
            devices
                .iter()
                .any(|d| d.kind == "indev" && d.name == "lavfi"),
            "expected lavfi indev, got {devices:?}"
        );

        let protocols = list_protocols().expect("list_protocols");
        let names: Vec<&str> = protocols.iter().map(|p| p.name.as_str()).collect();
        for expected in ["file", "http", "https"] {
            assert!(names.contains(&expected), "missing protocol {expected}");
        }
        assert!(protocols.iter().all(|p| p.kind == "protocol"));

        let hwaccels = list_hwaccels().expect("list_hwaccels");
        assert!(hwaccels.iter().all(|h| h.kind == "hwaccel"));

        // Option only — value depends on host hardware.
        let _ = detect_hwaccel();
    }

    // ---- capture arg builders (pure, no ffmpeg needed) ----

    fn v4l2_source() -> CaptureSource {
        CaptureSource {
            format: CaptureFormat::V4l2,
            device: "/dev/video0".to_string(),
            audio_device: None,
            audio_format: None,
        }
    }

    #[test]
    fn capture_args_v4l2_basic() {
        let args = capture_args(&v4l2_source(), &CaptureInputOpts::default()).unwrap();
        assert_eq!(
            args,
            vec!["-f", "v4l2", "-i", "/dev/video0"]
                .into_iter()
                .map(String::from)
                .collect::<Vec<_>>()
        );
    }

    #[test]
    fn capture_args_v4l2_with_size_fps_and_audio() {
        let source = CaptureSource {
            audio_device: Some("default".to_string()),
            audio_format: Some(CaptureFormat::Alsa),
            ..v4l2_source()
        };
        let opts = CaptureInputOpts {
            width: Some(1280),
            height: Some(720),
            fps: Some(30),
            sample_rate: Some(48000),
        };
        let args = capture_args(&source, &opts).unwrap();
        assert_eq!(
            args,
            vec![
                "-f",
                "v4l2",
                "-video_size",
                "1280x720",
                "-framerate",
                "30",
                "-i",
                "/dev/video0",
                "-f",
                "alsa",
                "-ar",
                "48000",
                "-i",
                "default",
            ]
            .into_iter()
            .map(String::from)
            .collect::<Vec<_>>()
        );
    }

    #[test]
    fn capture_args_per_format() {
        let cases: &[(CaptureFormat, &str)] = &[
            (CaptureFormat::Dshow, "USB Camera"),
            (CaptureFormat::AvFoundation, "0:none"),
            (CaptureFormat::Alsa, "hw:0,0"),
            (CaptureFormat::Pulse, "default"),
            (CaptureFormat::X11Grab, ":0.0"),
            (CaptureFormat::KmsGrab, "/dev/dri/card0"),
        ];
        for (format, device) in cases {
            let source = CaptureSource {
                format: *format,
                device: (*device).to_string(),
                audio_device: None,
                audio_format: None,
            };
            let args = capture_args(&source, &CaptureInputOpts::default())
                .unwrap_or_else(|e| panic!("{format:?} {device}: {e}"));
            assert_eq!(args[0], "-f");
            assert_eq!(args[args.len() - 1], *device);
        }
    }

    #[test]
    fn capture_args_rejects_bad_devices() {
        for (format, device) in [
            (CaptureFormat::V4l2, "video0"),         // not /dev/*
            (CaptureFormat::V4l2, "/dev/video0;rm"), // metachar
            (CaptureFormat::V4l2, "/dev/$(x)"),      // substitution
            (CaptureFormat::Alsa, "default card"),   // whitespace
            (CaptureFormat::Alsa, ""),               // empty
            (CaptureFormat::X11Grab, "0.0"),         // no display colon
            (CaptureFormat::AvFoundation, "0"),      // missing :idx
            (CaptureFormat::AvFoundation, "a:b"),    // non-numeric
            (CaptureFormat::Dshow, "cam\"quote"),    // quote breaks dshow opts
        ] {
            let source = CaptureSource {
                format,
                device: device.to_string(),
                audio_device: None,
                audio_format: None,
            };
            assert!(
                capture_args(&source, &CaptureInputOpts::default()).is_err(),
                "{format:?} device {device:?} should be rejected"
            );
        }
    }

    #[test]
    fn capture_args_rejects_bad_opts() {
        let source = v4l2_source();
        for opts in [
            CaptureInputOpts {
                width: Some(640),
                height: None,
                ..Default::default()
            },
            CaptureInputOpts {
                width: Some(0),
                height: Some(480),
                ..Default::default()
            },
            CaptureInputOpts {
                fps: Some(0),
                ..Default::default()
            },
            CaptureInputOpts {
                fps: Some(1000),
                ..Default::default()
            },
            CaptureInputOpts {
                sample_rate: Some(100),
                ..Default::default()
            },
        ] {
            assert!(capture_args(&source, &opts).is_err());
        }
    }

    #[test]
    fn capture_output_args_defaults_to_h264() {
        match h264_encoder() {
            Some(enc) => {
                let args = capture_output_args(&CaptureOutputOpts::default()).unwrap();
                assert_eq!(args[0], "-c:v");
                assert_eq!(args[1], enc);
                assert!(args.windows(2).any(|w| w == ["-pix_fmt", "yuv420p"]));
                // video re-encode → audio defaults to aac
                assert!(args.windows(2).any(|w| w == ["-c:a", "aac"]));
            }
            None => {
                assert!(capture_output_args(&CaptureOutputOpts::default()).is_err());
            }
        }
    }

    #[test]
    fn capture_output_args_copy_and_limits() {
        let opts = CaptureOutputOpts {
            duration_secs: Some(30),
            out_width: Some(640),
            out_height: Some(360),
            out_fps: Some(25),
            vcodec: Some("copy".to_string()),
            acodec: None,
            audio_bitrate: Some("128k".to_string()),
            include_audio: true,
        };
        let args = capture_output_args(&opts).unwrap();
        assert!(args.windows(2).any(|w| w == ["-t", "30"]));
        assert!(args.windows(2).any(|w| w == ["-s", "640x360"]));
        assert!(args.windows(2).any(|w| w == ["-r", "25"]));
        assert!(args.windows(2).any(|w| w == ["-c:v", "copy"]));
        // video copy → audio defaults to copy too
        assert!(args.windows(2).any(|w| w == ["-c:a", "copy"]));
        assert!(args.windows(2).any(|w| w == ["-b:a", "128k"]));

        // -an path
        let opts = CaptureOutputOpts {
            include_audio: false,
            ..CaptureOutputOpts::default()
        };
        let args = capture_output_args(&opts).unwrap();
        assert!(args.contains(&"-an".to_string()));
        assert!(!args.iter().any(|a| a == "-c:a"));

        // mismatched output size rejected
        let opts = CaptureOutputOpts {
            out_width: Some(640),
            out_height: None,
            ..CaptureOutputOpts::default()
        };
        assert!(capture_output_args(&opts).is_err());
    }

    #[test]
    fn capture_argv_combines_everything() {
        let source = CaptureSource {
            audio_device: Some("hw:0,0".to_string()),
            audio_format: Some(CaptureFormat::Alsa),
            ..v4l2_source()
        };
        let in_opts = CaptureInputOpts {
            width: Some(1920),
            height: Some(1080),
            fps: Some(30),
            sample_rate: Some(48000),
        };
        let out_opts = CaptureOutputOpts {
            duration_secs: Some(60),
            ..CaptureOutputOpts::default()
        };
        let argv = capture_argv(&source, &in_opts, &out_opts, "/tmp/cap.mp4").unwrap();
        assert_eq!(argv[0], "-hide_banner");
        assert_eq!(argv[1], "-y");
        assert_eq!(argv.last().unwrap(), "/tmp/cap.mp4");
        // input block precedes output block precedes output path
        let i_pos = argv.iter().position(|a| a == "/dev/video0").unwrap();
        let t_pos = argv.iter().position(|a| a == "-t").unwrap();
        assert!(i_pos < t_pos);
        assert!(capture_argv(&source, &in_opts, &out_opts, "").is_err());
    }

    #[test]
    fn capture_format_for_host_is_video_kind() {
        let f = CaptureFormat::for_host();
        assert!(f.is_video());
    }
}
