//! Per-frame luma/chroma histograms via `signalstats` +
//! `metadata=print`, sampled at a bounded rate.
//!
//! Like [`super::signalstats`], this op drives ffmpeg's `signalstats`
//! filter, which computes per-frame Y/U/V statistics (average, min,
//! max, and `YDIF` — the difference between luma and chroma
//! averages). The values only exist as frame metadata, so
//! `metadata=mode=print` dumps every frame's tags on **stderr**
//! while the video is discarded to the `null` muxer. The returned
//! [`HistogramReport`] aggregates those per-frame values into
//! per-channel min/max/mean plus a coarse luma histogram of the
//! sampled frames' `yavg`.
//!
//! To keep stderr (and therefore runtime) bounded, the stream is
//! downscaled to 64x64 and optionally decimated with
//! `fps=<fps_sample>`; `signalstats` averages are resolution
//! independent for a given content, so downscaling is cheap
//! insurance rather than a correctness compromise.

use std::collections::BTreeMap;

use serde::{Deserialize, Serialize};

use crate::crates::multimedia_engine::error::{MediaError, MediaResult};
use crate::crates::multimedia_engine::model::MediaLimits;
use crate::crates::multimedia_engine::runtime::discovery::Runtime;
use crate::crates::multimedia_engine::runtime::process;
use crate::crates::multimedia_engine::runtime::tempdir::TempDir;

/// Options for [`histogram`].
#[derive(Debug, Clone, Copy, PartialEq, Serialize, Deserialize)]
pub struct HistogramOptions {
    /// Analysis rate in frames per second. `None` (default) analyzes
    /// every decoded frame; `Some(fps)` inserts an `fps=` decimator
    /// before `signalstats` so at most `duration * fps` stat lines
    /// are produced.
    pub fps_sample: Option<f64>,
    /// Which planes the report aggregates. Default [`HistogramPlanes::All`].
    pub planes: HistogramPlanes,
}

impl Default for HistogramOptions {
    fn default() -> Self {
        Self {
            fps_sample: None,
            planes: HistogramPlanes::All,
        }
    }
}

/// Plane selection for [`histogram`].
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
pub enum HistogramPlanes {
    /// Luma (`Y*`) statistics only — fastest to aggregate, useful
    /// for exposure/brightness checks.
    Luma,
    /// Luma plus chroma (`Y`, `U`, `V`) statistics.
    #[default]
    All,
}

/// Min/max/mean of one signalstats channel over the sampled frames.
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
pub struct ChannelStats {
    /// Number of sampled frames that contributed a value.
    pub samples: u32,
    /// Minimum value seen.
    pub min: Option<f64>,
    /// Maximum value seen.
    pub max: Option<f64>,
    /// Arithmetic mean across sampled frames.
    pub mean: Option<f64>,
}

impl ChannelStats {
    fn observe(&mut self, v: f64) {
        self.samples += 1;
        self.min = Some(self.min.map_or(v, |m| m.min(v)));
        self.max = Some(self.max.map_or(v, |m| m.max(v)));
        // Running mean; `samples` was already incremented.
        let n = f64::from(self.samples);
        match self.mean {
            Some(m) => self.mean = Some(m + (v - m) / n),
            None => self.mean = Some(v),
        }
    }
}

/// Number of luma-histogram buckets in [`HistogramReport`].
/// Covers the nominal 0–255 luma range at 1-unit resolution.
pub const LUMA_HISTOGRAM_BUCKETS: usize = 256;

/// Aggregate luma/chroma statistics over sampled frames of a video.
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
pub struct HistogramReport {
    /// Number of frames actually analyzed.
    pub frame_count: u32,
    /// Luma (`Y*`) channel statistics across sampled frames.
    pub y: ChannelStats,
    /// Chroma (`U*`) channel statistics. Empty when `planes` was
    /// [`HistogramPlanes::Luma`].
    pub u: ChannelStats,
    /// Chroma (`V*`) channel statistics. Empty when `planes` was
    /// [`HistogramPlanes::Luma`].
    pub v: ChannelStats,
    /// Mean of `yavg`, `uavg`, `vavg` across all sampled frames —
    /// a single-number "overall brightness" figure. `None` when no
    /// channel produced values.
    pub overall_mean: Option<f64>,
    /// Histogram of each sampled frame's `yavg`, one count per
    /// [`LUMA_HISTOGRAM_BUCKETS`] bucket. Bucket `i` counts frames
    /// whose yavg falls in `[i, i+1)`; values outside 0–255 clamp to
    /// the edge buckets.
    pub yavg_histogram: Vec<u32>,
    /// All parsed `lavfi.signalstats.*` keys from the last sampled
    /// frame, for callers that need values this struct doesn't name.
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

/// Aggregate `histogram` statistics over sampled frames of `input`.
///
/// # Errors
///
/// - [`MediaError::InvalidInput`] when `fps_sample` is non-finite or
///   `<= 0`, or the input has no video stream.
/// - [`MediaError::LimitExceeded`] when `input` exceeds
///   `limits.max_input_bytes`.
/// - [`MediaError::Unsupported`] when `signalstats` or `metadata`
///   filters are absent.
/// - [`MediaError::ToolFailed`] when ffmpeg cannot decode the input.
pub fn histogram(
    input: &[u8],
    options: &HistogramOptions,
    limits: &MediaLimits,
    rt: &Runtime,
) -> MediaResult<HistogramReport> {
    if let Some(fps) = options.fps_sample
        && (!fps.is_finite() || fps <= 0.0)
    {
        return Err(MediaError::InvalidInput(format!(
            "fps_sample must be finite and > 0, got {fps}"
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

    let dir = TempDir::new("hist")?;
    let in_name = format!("input.{}", sniff_extension(input));
    let in_path = dir.write(&in_name, input)?;

    // Downscale to 64x64 (stats are scale-invariant enough for
    // analysis) and optionally decimate to bound the number of
    // metadata lines.
    let filter = match options.fps_sample {
        Some(fps) => format!("fps={fps},scale=64:64,signalstats,metadata=mode=print"),
        None => "scale=64:64,signalstats,metadata=mode=print".to_string(),
    };
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

    let chroma = options.planes == HistogramPlanes::All;
    let mut y = ChannelStats::default();
    let mut u = ChannelStats::default();
    let mut v = ChannelStats::default();
    let mut hist = vec![0u32; LUMA_HISTOGRAM_BUCKETS];
    let mut overall_sum = 0.0;
    let mut overall_n = 0u32;

    for b in &blocks {
        let mut frame_sum = 0.0;
        let mut frame_n = 0u32;
        for (key, chan) in [("yavg", &mut y), ("uavg", &mut u), ("vavg", &mut v)] {
            // Chroma channels skipped entirely in Luma-only mode so
            // their `samples` stays 0.
            let is_chroma = key.starts_with('u') || key.starts_with('v');
            if is_chroma && !chroma {
                continue;
            }
            if let Some(&val) = b.get(key) {
                chan.observe(val);
                frame_sum += val;
                frame_n += 1;
            }
        }
        if let Some(&yavg) = b.get("yavg") {
            let idx = (yavg.clamp(0.0, 255.0) as usize).min(LUMA_HISTOGRAM_BUCKETS - 1);
            hist[idx] += 1;
        }
        overall_sum += frame_sum;
        overall_n += frame_n;
    }

    let overall_mean = if overall_n > 0 {
        Some(overall_sum / f64::from(overall_n))
    } else {
        None
    };

    Ok(HistogramReport {
        frame_count: blocks.len() as u32,
        y,
        u,
        v,
        overall_mean,
        yavg_histogram: hist,
        last_frame: blocks.last().cloned().unwrap_or_default(),
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::crates::multimedia_engine::runtime::process;

    fn testsrc_mp4(secs: u32) -> Option<Vec<u8>> {
        let rt = Runtime::discover().ok()?;
        let dir = TempDir::new("hist-test").ok()?;
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
    fn histogram_aggregates_testsrc() {
        let Some(input) = testsrc_mp4(2) else {
            eprintln!("skipping: ffmpeg lavfi/libx264 unavailable");
            return;
        };
        let rt = Runtime::discover().unwrap();
        let report = histogram(
            &input,
            &HistogramOptions::default(),
            &MediaLimits::default(),
            &rt,
        )
        .expect("histogram failed");
        assert!(report.frame_count > 0, "no frames analyzed");
        // testsrc is mid-range content; yavg should be well inside
        // the nominal 16–235 luma range.
        let mean = report.y.mean.expect("no yavg samples");
        assert!((0.0..=255.0).contains(&mean), "y mean out of range: {mean}");
        assert!(report.u.samples > 0 && report.v.samples > 0);
        assert_eq!(
            report.yavg_histogram.len(),
            LUMA_HISTOGRAM_BUCKETS,
            "histogram must be fixed-size"
        );
        assert_eq!(
            report.yavg_histogram.iter().sum::<u32>(),
            report.frame_count,
            "every analyzed frame should land in exactly one bucket"
        );
    }

    #[test]
    fn histogram_luma_only_skips_chroma() {
        let Some(input) = testsrc_mp4(1) else {
            eprintln!("skipping: ffmpeg lavfi/libx264 unavailable");
            return;
        };
        let rt = Runtime::discover().unwrap();
        let opts = HistogramOptions {
            planes: HistogramPlanes::Luma,
            ..HistogramOptions::default()
        };
        let report =
            histogram(&input, &opts, &MediaLimits::default(), &rt).expect("histogram failed");
        assert!(report.y.samples > 0);
        assert_eq!(report.u.samples, 0);
        assert_eq!(report.v.samples, 0);
    }

    #[test]
    fn histogram_rejects_bad_fps() {
        let rt = match Runtime::discover() {
            Ok(rt) => rt,
            Err(_) => return,
        };
        let input = b"junk".to_vec();
        let opts = HistogramOptions {
            fps_sample: Some(0.0),
            ..HistogramOptions::default()
        };
        assert!(histogram(&input, &opts, &MediaLimits::default(), &rt).is_err());
    }
}
