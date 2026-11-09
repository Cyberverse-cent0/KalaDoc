//! Objective quality metrics between two videos: PSNR and SSIM.
//!
//! Both are full-reference metrics — they need a reference (original)
//! and a distorted (processed) video of the same dimensions/frame
//! count. ffmpeg's `psnr`/`ssim` filters compare frame-by-frame and
//! report per-frame values; with `stats_file=` they append one line
//! per frame to a file we read back, keeping stderr clean.
//!
//! The returned [`QualityReport`] carries the **mean** score plus
//! min/max across frames:
//!
//! - **PSNR** (dB): higher is better; `inf` means identical. Values
//!   below ~30 dB are visibly degraded.
//! - **SSIM** (0..=1): higher is better; 1.0 is identical.

use serde::{Deserialize, Serialize};

use crate::crates::multimedia_engine::error::{MediaError, MediaResult};
use crate::crates::multimedia_engine::model::MediaLimits;
use crate::crates::multimedia_engine::runtime::discovery::Runtime;
use crate::crates::multimedia_engine::runtime::process;
use crate::crates::multimedia_engine::runtime::tempdir::TempDir;

/// Which metric to compute.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize, Deserialize)]
pub enum QualityMetric {
    /// Peak signal-to-noise ratio in dB (higher = better).
    #[default]
    Psnr,
    /// Structural similarity index (0..=1, higher = better).
    Ssim,
}

impl QualityMetric {
    fn filter_name(self) -> &'static str {
        match self {
            Self::Psnr => "psnr",
            Self::Ssim => "ssim",
        }
    }

    /// The per-frame average column name in the stats file
    /// (`psnr_avg` / `All` for ssim).
    fn avg_key(self) -> &'static str {
        match self {
            Self::Psnr => "psnr_avg",
            Self::Ssim => "All",
        }
    }
}

/// Aggregate quality result over all compared frames.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct QualityReport {
    /// Metric that was computed.
    pub metric: QualityMetric,
    /// Number of frame pairs compared.
    pub frames_compared: u32,
    /// Mean score across frames (`None` when ffmpeg reported no
    /// parseable values — e.g. all-identical PSNR writes `inf`).
    pub mean: Option<f64>,
    /// Minimum per-frame score.
    pub min: Option<f64>,
    /// Maximum per-frame score.
    pub max: Option<f64>,
}

/// Best-effort container sniff for temp input extensions.
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

fn check_size(input: &[u8], limits: &MediaLimits, which: &str) -> MediaResult<()> {
    if let Some(max) = limits.max_input_bytes
        && (input.len() as u64) > max
    {
        return Err(MediaError::LimitExceeded(format!(
            "{which} input is {} bytes, exceeding max_input_bytes {max}",
            input.len()
        )));
    }
    Ok(())
}

/// Parse one stats-file line into the per-frame average score.
///
/// psnr stats lines look like:
/// `n:1 mse_avg:123.4 mse_y:... psnr_avg:25.21 psnr_y:...`
/// ssim stats lines look like:
/// `n:1 Y:0.98 U:0.97 V:0.99 All:0.98 (average:...)`
fn score_from_line(line: &str, metric: QualityMetric) -> Option<f64> {
    let key = metric.avg_key();
    for token in line.split_whitespace() {
        let Some((k, v)) = token.split_once(':') else {
            continue;
        };
        if k == key {
            // `inf` for identical PSNR frames — skip (unbounded).
            return v.parse::<f64>().ok().filter(|f| f.is_finite());
        }
    }
    None
}

/// Compare `distorted` against `reference` with the given metric.
///
/// Both inputs must contain a video stream of identical dimensions —
/// mismatched sizes make ffmpeg fail honestly.
///
/// # Errors
///
/// - [`MediaError::LimitExceeded`] when either input exceeds
///   `limits.max_input_bytes`.
/// - [`MediaError::Unsupported`] when the metric's filter is absent.
/// - [`MediaError::ToolFailed`] when ffmpeg cannot decode either
///   input or the streams differ in size.
pub fn compare_quality(
    reference: &[u8],
    distorted: &[u8],
    metric: QualityMetric,
    limits: &MediaLimits,
    rt: &Runtime,
) -> MediaResult<QualityReport> {
    check_size(reference, limits, "reference")?;
    check_size(distorted, limits, "distorted")?;

    let caps = rt.capabilities()?;
    let filter = metric.filter_name();
    if !caps.has_filter(filter) {
        return Err(MediaError::Unsupported(format!(
            "filter {filter:?} is not available in this FFmpeg build"
        )));
    }

    let dir = TempDir::new("quality")?;
    let ref_name = format!("reference.{}", sniff_extension(reference));
    let dist_name = format!("distorted.{}", sniff_extension(distorted));
    dir.write(&ref_name, reference)?;
    dir.write(&dist_name, distorted)?;

    let stats_name = "stats.log";
    let stats_path = dir.file(stats_name);

    let filter_complex = format!("[0:v][1:v]{filter}=stats_file={}", stats_path.display());
    let argv = process::args(&[
        "-hide_banner",
        "-nostdin",
        "-i",
        dir.file(&ref_name).to_str().unwrap_or("reference"),
        "-i",
        dir.file(&dist_name).to_str().unwrap_or("distorted"),
        "-filter_complex",
        &filter_complex,
        "-an",
        "-f",
        "null",
        "-",
    ]);
    let out = process::run_checked("ffmpeg", &rt.ffmpeg, &argv, None, limits)?;
    let _ = out; // stats live in the file; stderr is diagnostic only

    let stats = std::fs::read_to_string(&stats_path).unwrap_or_default();
    let mut scores = Vec::new();
    for line in stats.lines() {
        if let Some(s) = score_from_line(line, metric) {
            scores.push(s);
        }
    }

    if scores.is_empty() {
        return Err(MediaError::InvalidInput(format!(
            "{filter} produced no comparable frames (video streams missing or size mismatch?)"
        )));
    }

    let n = scores.len() as u32;
    let mean = scores.iter().sum::<f64>() / n as f64;
    let min = scores.iter().copied().fold(f64::INFINITY, f64::min);
    let max = scores.iter().copied().fold(f64::NEG_INFINITY, f64::max);

    Ok(QualityReport {
        metric,
        frames_compared: n,
        mean: Some(mean),
        min: Some(min),
        max: Some(max),
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::crates::multimedia_engine::runtime::process;

    fn testsrc_mp4(secs: u32) -> Option<Vec<u8>> {
        let rt = Runtime::discover().ok()?;
        let dir = TempDir::new("qual-test").ok()?;
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
    fn identical_inputs_score_perfect() {
        let Some(input) = testsrc_mp4(1) else {
            eprintln!("skipping: ffmpeg lavfi/libx264 unavailable");
            return;
        };
        let rt = Runtime::discover().unwrap();
        let report = compare_quality(
            &input,
            &input,
            QualityMetric::Ssim,
            &MediaLimits::default(),
            &rt,
        )
        .expect("ssim compare failed");
        assert_eq!(report.metric, QualityMetric::Ssim);
        assert!(report.frames_compared >= 1);
        let mean = report.mean.expect("mean");
        assert!(
            mean > 0.99,
            "identical inputs should score ~1.0, got {mean}"
        );
    }

    #[test]
    fn compare_quality_rejects_empty_input() {
        let rt = match Runtime::discover() {
            Ok(rt) => rt,
            Err(_) => return,
        };
        let input = testsrc_mp4(1).unwrap_or_default();
        match compare_quality(
            &input,
            &[],
            QualityMetric::Psnr,
            &MediaLimits::default(),
            &rt,
        ) {
            Err(MediaError::LimitExceeded(_))
            | Err(MediaError::InvalidInput(_))
            | Err(MediaError::ToolFailed { .. }) => {}
            other => panic!(
                "expected failure for empty distorted input, got {:?}",
                other.map(|_| ())
            ),
        }
    }
}
