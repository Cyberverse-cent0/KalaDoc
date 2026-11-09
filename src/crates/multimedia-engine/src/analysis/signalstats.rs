//! Per-frame video signal statistics via `signalstats` +
//! `metadata=print`, sampled at a bounded rate.
//!
//! `signalstats` computes per-frame statistics (luma min/max/average,
//! saturation, hue, etc.) but only exposes them as frame metadata —
//! `metadata=mode=print` dumps every frame's tags, which for a long
//! video would be unbounded. To keep output bounded this op first
//! reduces the stream with `fps=<sample_fps>` and `scale=64:64`,
//! so at most `duration * sample_fps` stat lines are produced.
//!
//! The returned [`SignalStatsReport`] carries the *last* sampled
//! frame's values plus min/max across all sampled frames — enough
//! for "is this clip dark/saturated" style checks without holding
//! the whole histogram in memory.

use std::collections::BTreeMap;

use serde::{Deserialize, Serialize};

use crate::crates::multimedia_engine::error::{MediaError, MediaResult};
use crate::crates::multimedia_engine::model::MediaLimits;
use crate::crates::multimedia_engine::runtime::discovery::Runtime;
use crate::crates::multimedia_engine::runtime::process;
use crate::crates::multimedia_engine::runtime::tempdir::TempDir;

/// Options for [`signal_stats`].
#[derive(Debug, Clone, Copy, PartialEq, Serialize, Deserialize)]
pub struct SignalStatsOptions {
    /// Sample rate in frames per second fed to `signalstats`.
    /// Default `1.0` (one stat line per second of video).
    pub sample_fps: f64,
}

impl Default for SignalStatsOptions {
    fn default() -> Self {
        Self { sample_fps: 1.0 }
    }
}

/// Aggregate statistics over the sampled frames.
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
pub struct SignalStatsReport {
    /// Number of frames actually analyzed.
    pub frames_sampled: u32,
    /// Y (luma) average of the last sampled frame.
    pub yavg: Option<f64>,
    /// Minimum Y across all sampled frames.
    pub ymin: Option<f64>,
    /// Maximum Y across all sampled frames.
    pub ymax: Option<f64>,
    /// U (chroma) average of the last sampled frame.
    pub uavg: Option<f64>,
    /// V (chroma) average of the last sampled frame.
    pub vavg: Option<f64>,
    /// Saturation average of the last sampled frame.
    pub satavg: Option<f64>,
    /// Maximum saturation across sampled frames.
    pub satmax: Option<f64>,
    /// Hue average of the last sampled frame.
    pub hueavg: Option<f64>,
    /// All parsed `lavfi.signalstats.*` keys from the last frame,
    /// for callers that need values this struct doesn't name.
    pub last_frame: BTreeMap<String, f64>,
}

/// Best-effort container sniff for the temp input extension.
fn sniff_extension(input: &[u8]) -> &'static str {
    if input.len() >= 4 {
        match &input[..4] {
            [0x1A, 0x45, 0xDF, 0xA3] => return "mkv",
            [b'O', b'g', b'g', b'S'] => return "ogg",
            [b'f', b'L', b'a', b'C'] => return "flac",
            [b'R', b'I', b'F', b'F'] => {
                if input.len() >= 12 && &input[8..12] == b"WAVE" {
                    return "wav";
                }
                if input.len() >= 12 && &input[8..12] == b"AVI " {
                    return "avi";
                }
                return "bin";
            }
            [b'I', b'D', b'3', _] => return "mp3",
            _ => {}
        }
        if input.len() >= 8 && &input[4..8] == b"ftyp" {
            return "mp4";
        }
    }
    "bin"
}

/// Parse `key=value` tokens containing `signalstats` from one
/// `metadata=print` frame block. Values may be `N/A` — skipped.
fn parse_frame_stats(block: &str) -> BTreeMap<String, f64> {
    let mut map = BTreeMap::new();
    for token in block.split_whitespace() {
        if let Some((k, v)) = token.split_once('=')
            && let Some(name) = k.strip_prefix("lavfi.signalstats.")
            && let Ok(f) = v.parse::<f64>()
        {
            map.insert(name.to_string(), f);
        }
    }
    map
}

/// Split stderr into per-frame stat blocks. `metadata=print` emits
/// lines like `frame:123 pts:...` followed by `lavfi.signalstats.yavg=...`
/// on subsequent lines; a new `frame:` line starts a new block.
fn frame_blocks(stderr: &str) -> Vec<BTreeMap<String, f64>> {
    let mut blocks = Vec::new();
    let mut current = String::new();
    let mut in_frame = false;
    for line in stderr.lines() {
        if line.starts_with("frame:") {
            if in_frame {
                let parsed = parse_frame_stats(&current);
                if !parsed.is_empty() {
                    blocks.push(parsed);
                }
            }
            current.clear();
            in_frame = true;
            current.push_str(line);
            current.push('\n');
        } else if in_frame {
            current.push_str(line);
            current.push('\n');
        }
    }
    if in_frame {
        let parsed = parse_frame_stats(&current);
        if !parsed.is_empty() {
            blocks.push(parsed);
        }
    }
    blocks
}

/// Collect signal statistics over sampled frames of `input`.
///
/// # Errors
///
/// - [`MediaError::LimitExceeded`] when `input` exceeds
///   `limits.max_input_bytes`.
/// - [`MediaError::Unsupported`] when `signalstats` or `metadata`
///   filters are absent.
/// - [`MediaError::InvalidInput`] when `sample_fps` is non-finite or
///   `<= 0`, or the input has no video stream.
pub fn signal_stats(
    input: &[u8],
    options: &SignalStatsOptions,
    limits: &MediaLimits,
    rt: &Runtime,
) -> MediaResult<SignalStatsReport> {
    if !options.sample_fps.is_finite() || options.sample_fps <= 0.0 {
        return Err(MediaError::InvalidInput(format!(
            "sample_fps must be finite and > 0, got {}",
            options.sample_fps
        )));
    }
    let caps = rt.capabilities()?;
    for f in ["signalstats", "metadata", "fps", "scale"] {
        if !caps.has_filter(f) {
            return Err(MediaError::Unsupported(format!(
                "filter {f:?} is not available in this FFmpeg build"
            )));
        }
    }

    if let Some(max) = limits.max_input_bytes
        && (input.len() as u64) > max
    {
        return Err(MediaError::LimitExceeded(format!(
            "input is {} bytes, exceeding max_input_bytes {max}",
            input.len()
        )));
    }

    let dir = TempDir::new("sigstats")?;
    let in_name = format!("input.{}", sniff_extension(input));
    let in_path = dir.write(&in_name, input)?;

    // Bound the work: downscale to 64x64 and sample at sample_fps so
    // a 2-hour video yields ~7200 stat lines instead of ~200k.
    let filter = format!(
        "fps={},scale=64:64,signalstats,metadata=mode=print",
        options.sample_fps
    );
    let argv = process::args(&[
        "-hide_banner",
        "-nostdin",
        "-i",
        in_path.to_str().unwrap_or("input"),
        "-vf",
        &filter,
        "-an",
        "-f",
        "null",
        "-",
    ]);
    let out = process::run_checked("ffmpeg", &rt.ffmpeg, &argv, None, limits)?;

    let blocks = frame_blocks(&out.stderr);
    if blocks.is_empty() {
        return Err(MediaError::InvalidInput(
            "signalstats produced no frame statistics (video stream missing?)".to_string(),
        ));
    }

    let last = blocks.last().cloned().unwrap_or_default();
    let mut ymin = f64::INFINITY;
    let mut ymax = f64::NEG_INFINITY;
    let mut satmax = f64::NEG_INFINITY;
    let mut saw_y = false;
    let mut saw_sat = false;
    for b in &blocks {
        if let Some(y) = b.get("ymin") {
            ymin = ymin.min(*y);
            saw_y = true;
        }
        if let Some(y) = b.get("ymax") {
            ymax = ymax.max(*y);
            saw_y = true;
        }
        if let Some(s) = b.get("satmax") {
            satmax = satmax.max(*s);
            saw_sat = true;
        }
    }

    Ok(SignalStatsReport {
        frames_sampled: blocks.len() as u32,
        yavg: last.get("yavg").copied(),
        ymin: if saw_y { Some(ymin) } else { None },
        ymax: if saw_y { Some(ymax) } else { None },
        uavg: last.get("uavg").copied(),
        vavg: last.get("vavg").copied(),
        satavg: last.get("satavg").copied(),
        satmax: if saw_sat { Some(satmax) } else { None },
        hueavg: last.get("hueavg").copied(),
        last_frame: last,
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::crates::multimedia_engine::runtime::process;

    fn testsrc_mp4(secs: u32) -> Option<Vec<u8>> {
        let rt = Runtime::discover().ok()?;
        let dir = TempDir::new("sig-test").ok()?;
        let out = dir.file("clip.mp4");
        let args = process::args(&[
            "-hide_banner",
            "-loglevel",
            "error",
            "-nostdin",
            "-y",
            "-f",
            "lavfi",
            "-i",
            &format!("testsrc=duration={secs}:size=64x64:rate=10"),
            "-c:v",
            "libx264",
            "-pix_fmt",
            "yuv420p",
            out.to_str()?,
        ]);
        let res = process::run("ffmpeg", &rt.ffmpeg, &args, None, &MediaLimits::default()).ok()?;
        if !res.success {
            return None;
        }
        std::fs::read(&out).ok()
    }

    #[test]
    fn signal_stats_samples_frames() {
        let Some(input) = testsrc_mp4(2) else {
            eprintln!("skipping: ffmpeg lavfi/libx264 unavailable");
            return;
        };
        let rt = Runtime::discover().unwrap();
        let report = signal_stats(
            &input,
            &SignalStatsOptions::default(),
            &MediaLimits::default(),
            &rt,
        )
        .expect("signal_stats failed");
        assert!(report.frames_sampled >= 1, "no frames sampled");
        assert!(report.yavg.is_some() || report.ymin.is_some());
    }

    #[test]
    fn signal_stats_rejects_bad_fps() {
        let rt = match Runtime::discover() {
            Ok(rt) => rt,
            Err(_) => return,
        };
        let input = b"junk".to_vec();
        let opts = SignalStatsOptions { sample_fps: 0.0 };
        assert!(signal_stats(&input, &opts, &MediaLimits::default(), &rt).is_err());
    }
}
