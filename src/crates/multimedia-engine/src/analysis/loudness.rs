//! Loudness measurement via ffmpeg's `loudnorm` filter in
//! `print_format=json` mode.
//!
//! `loudnorm` measures the input in one pass when only analysis is
//! needed: run the filter with output to `null` and it prints a JSON
//! block on **stderr** containing the integrated loudness (LUFS),
//! loudness range (LU), true peak (dBTP) and normalization-target
//! fields. This module parses that block into [`LoudnessReport`].
//!
//! This is *measurement*, not normalization — to actually normalize,
//! use the measured values in a second `loudnorm` pass (two-pass
//! loudness normalization) or the dynamic single-pass mode.

use serde::{Deserialize, Serialize};

use crate::crates::multimedia_engine::error::{MediaError, MediaResult};
use crate::crates::multimedia_engine::model::MediaLimits;
use crate::crates::multimedia_engine::runtime::discovery::Runtime;
use crate::crates::multimedia_engine::runtime::process;
use crate::crates::multimedia_engine::runtime::tempdir::TempDir;

/// Target parameters for the `loudnorm` measurement pass.
/// These do not change the measurement itself (loudnorm measures
/// the input regardless) but are echoed into the JSON output and
/// documented for callers that chain a normalization pass.
#[derive(Debug, Clone, Copy, PartialEq, Serialize, Deserialize)]
pub struct LoudnessOptions {
    /// Integrated loudness target in LUFS (`I=`). Default `-16`.
    pub target_i: f64,
    /// True-peak target in dBTP (`TP=`). Default `-1.5`.
    pub target_tp: f64,
    /// Loudness-range target in LU (`LRA=`). Default `11.0`.
    pub target_lra: f64,
}

impl Default for LoudnessOptions {
    fn default() -> Self {
        Self {
            target_i: -16.0,
            target_tp: -1.5,
            target_lra: 11.0,
        }
    }
}

/// Parsed `loudnorm` JSON measurement block.
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
pub struct LoudnessReport {
    /// Integrated loudness in LUFS (`input_i`).
    pub input_i: Option<f64>,
    /// Loudness range in LU (`input_lra`).
    pub input_lra: Option<f64>,
    /// Loudness range low in LU (`input_thresh` companion).
    pub input_lra_low: Option<f64>,
    /// Loudness range high in LU.
    pub input_lra_high: Option<f64>,
    /// True peak in dBTP (`input_tp`).
    pub input_tp: Option<f64>,
    /// True-peak low bound.
    pub input_tp_low: Option<f64>,
    /// True-peak high bound.
    pub input_tp_high: Option<f64>,
    /// Measured momentary loudness maximum (`input_momentary_max`).
    pub input_momentary_max: Option<f64>,
    /// Measured short-term loudness maximum (`input_shortterm_max`).
    pub input_shortterm_max: Option<f64>,
    // Target fields echoed by loudnorm (`target_offset` etc.) are
    // not carried — callers set their own targets.
}

/// Raw serde mirror of the loudnorm JSON block — ffprobe-style
/// tolerance: every field optional.
#[derive(Debug, Default, Deserialize)]
struct LoudnormJson {
    #[serde(default)]
    input_i: Option<String>,
    #[serde(default)]
    input_lra: Option<String>,
    #[serde(default)]
    input_lra_low: Option<String>,
    #[serde(default)]
    input_lra_high: Option<String>,
    #[serde(default)]
    input_tp: Option<String>,
    #[serde(default)]
    input_tp_low: Option<String>,
    #[serde(default)]
    input_tp_high: Option<String>,
    #[serde(default)]
    input_momentary_max: Option<String>,
    #[serde(default)]
    input_shortterm_max: Option<String>,
}

fn parse_num(s: Option<String>) -> Option<f64> {
    s.and_then(|v| v.trim().parse().ok())
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
    if input[0] == 0xFF && (input[1] & 0xE0) == 0xE0 {
        return "mp3";
    }
    "bin"
}

/// Extract the trailing `{...}` JSON block loudnorm prints on
/// stderr. The block is the last JSON object in the output.
fn extract_json_block(stderr: &str) -> Option<&str> {
    let start = stderr.rfind('{')?;
    let end = stderr.rfind('}')?;
    if end <= start {
        return None;
    }
    Some(&stderr[start..=end])
}

/// Measure loudness of `input` with `loudnorm`.
///
/// Runs `ffmpeg -i input -af "loudnorm=I=..:TP=..:LRA=..:print_format=json"
/// -f null -` and parses the JSON block loudnorm prints on stderr.
/// Audio is required — video-only inputs fail via ffmpeg.
///
/// # Errors
///
/// - [`MediaError::LimitExceeded`] when `input` exceeds
///   `limits.max_input_bytes`.
/// - [`MediaError::Unsupported`] when `loudnorm` is absent from this
///   FFmpeg build.
/// - [`MediaError::InvalidInput`] when loudnorm produced no JSON
///   block (typically a silent or undecodable input).
pub fn measure_loudness(
    input: &[u8],
    options: &LoudnessOptions,
    limits: &MediaLimits,
    rt: &Runtime,
) -> MediaResult<LoudnessReport> {
    for (name, v) in [
        ("target_i", options.target_i),
        ("target_tp", options.target_tp),
        ("target_lra", options.target_lra),
    ] {
        if !v.is_finite() {
            return Err(MediaError::InvalidInput(format!(
                "{name} must be finite, got {v}"
            )));
        }
    }

    let caps = rt.capabilities()?;
    if !caps.has_filter("loudnorm") {
        return Err(MediaError::Unsupported(
            "filter \"loudnorm\" is not available in this FFmpeg build".to_string(),
        ));
    }

    if let Some(max) = limits.max_input_bytes
        && (input.len() as u64) > max
    {
        return Err(MediaError::LimitExceeded(format!(
            "input is {} bytes, exceeding max_input_bytes {max}",
            input.len()
        )));
    }

    let dir = TempDir::new("loudness")?;
    let in_name = format!("input.{}", sniff_extension(input));
    let in_path = dir.write(&in_name, input)?;

    let filter = format!(
        "loudnorm=I={}:TP={}:LRA={}:print_format=json",
        options.target_i, options.target_tp, options.target_lra
    );
    let argv = process::args(&[
        "-hide_banner",
        "-nostdin",
        "-i",
        in_path.to_str().unwrap_or("input"),
        "-af",
        &filter,
        "-f",
        "null",
        "-",
    ]);
    let out = process::run_checked("ffmpeg", &rt.ffmpeg, &argv, None, limits)?;

    let block = extract_json_block(&out.stderr).ok_or_else(|| {
        MediaError::InvalidInput(
            "loudnorm produced no JSON measurement block (silent or undecodable input?)"
                .to_string(),
        )
    })?;
    let parsed: LoudnormJson = serde_json::from_str(block)
        .map_err(|e| MediaError::InvalidInput(format!("loudnorm JSON parse failed: {e}")))?;

    Ok(LoudnessReport {
        input_i: parse_num(parsed.input_i),
        input_lra: parse_num(parsed.input_lra),
        input_lra_low: parse_num(parsed.input_lra_low),
        input_lra_high: parse_num(parsed.input_lra_high),
        input_tp: parse_num(parsed.input_tp),
        input_tp_low: parse_num(parsed.input_tp_low),
        input_tp_high: parse_num(parsed.input_tp_high),
        input_momentary_max: parse_num(parsed.input_momentary_max),
        input_shortterm_max: parse_num(parsed.input_shortterm_max),
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::crates::multimedia_engine::runtime::process;

    fn sine_wav(secs: u32) -> Option<Vec<u8>> {
        let rt = Runtime::discover().ok()?;
        let dir = TempDir::new("loud-test").ok()?;
        let out = dir.file("tone.wav");
        let args = process::args(&[
            "-hide_banner",
            "-loglevel",
            "error",
            "-nostdin",
            "-y",
            "-f",
            "lavfi",
            "-i",
            &format!("sine=frequency=440:duration={secs}"),
            "-c:a",
            "pcm_s16le",
            out.to_str()?,
        ]);
        let res = process::run("ffmpeg", &rt.ffmpeg, &args, None, &MediaLimits::default()).ok()?;
        if !res.success {
            return None;
        }
        std::fs::read(&out).ok()
    }

    #[test]
    fn measure_loudness_reports_i_and_tp() {
        let Some(input) = sine_wav(2) else {
            eprintln!("skipping: ffmpeg lavfi unavailable");
            return;
        };
        let rt = Runtime::discover().unwrap();
        let report = measure_loudness(
            &input,
            &LoudnessOptions::default(),
            &MediaLimits::default(),
            &rt,
        )
        .expect("measure_loudness failed");
        assert!(report.input_i.is_some(), "expected input_i, got {report:?}");
        assert!(
            report.input_tp.is_some(),
            "expected input_tp, got {report:?}"
        );
    }

    #[test]
    fn measure_loudness_rejects_bad_target() {
        let rt = match Runtime::discover() {
            Ok(rt) => rt,
            Err(_) => return,
        };
        let input = b"junk".to_vec();
        let opts = LoudnessOptions {
            target_i: f64::NAN,
            ..Default::default()
        };
        assert!(measure_loudness(&input, &opts, &MediaLimits::default(), &rt).is_err());
    }
}
