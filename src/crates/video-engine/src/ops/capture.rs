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

use crate::crates::video_engine::error::VideoResult;
use crate::crates::video_engine::model::CaptureDevice;
use crate::crates::video_engine::ops::{FFMPEG, PROBE_TIMEOUT, find_tool, run_tool};

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
}
