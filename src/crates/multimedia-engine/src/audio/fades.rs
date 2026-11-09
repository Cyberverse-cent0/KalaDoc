//! Audio fade-in / fade-out via ffmpeg's `afade` filter.
//!
//! [`apply_fades`] extracts the first audio stream and applies an
//! `afade=t=in` and/or `afade=t=out` filter chain. Output is
//! audio-only — same contract as `audio::volume`.
//!
//! # Curve
//!
//! `afade` supports `tri` (linear), `qsin`/`esin` (sinusoidal),
//! `log`, `ipar`, `qua`, `cub`, `square`, `cbr`, `par`, `exp`,
//! `iqsin`, `ihsin`, `desi`, `desq`, `losi`, `losq`, `qhsin`,
//! `whsin`, `hnsin`, `tanh`, `lfd`, `afd`. This op exposes the
//! common subset via [`FadeCurve`]; anything else is rejected before
//! reaching ffmpeg.

use std::ffi::OsString;

use serde::{Deserialize, Serialize};

use crate::crates::multimedia_engine::error::{MediaError, MediaResult};
use crate::crates::multimedia_engine::model::{MediaLimits, MediaOutput};
use crate::crates::multimedia_engine::runtime::discovery::Runtime;
use crate::crates::multimedia_engine::runtime::process;
use crate::crates::multimedia_engine::runtime::tempdir::TempDir;

/// Fade curve shape for `afade`.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
pub enum FadeCurve {
    /// Linear (`tri`) — constant gain slope. Default.
    #[default]
    Linear,
    /// Quarter-sine (`qsin`) — smooth, natural-sounding.
    QuarterSine,
    /// Exponential sine (`esin`).
    ExponentialSine,
    /// Logarithmic (`log`).
    Logarithmic,
    /// Parabolic (`qua`).
    Parabolic,
}

impl FadeCurve {
    fn ffmpeg_name(self) -> &'static str {
        match self {
            Self::Linear => "tri",
            Self::QuarterSine => "qsin",
            Self::ExponentialSine => "esin",
            Self::Logarithmic => "log",
            Self::Parabolic => "qua",
        }
    }
}

/// Options for [`apply_fades`].
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct FadeOptions {
    /// Fade-in duration in seconds. `0.0`/absent = no fade-in.
    /// Must be finite and `>= 0`.
    #[serde(default)]
    pub fade_in_secs: f64,
    /// Fade-out duration in seconds. `0.0`/absent = no fade-out.
    /// Must be finite and `>= 0`.
    #[serde(default)]
    pub fade_out_secs: f64,
    /// Curve shape applied to both fades.
    #[serde(default)]
    pub curve: FadeCurve,
    /// Fade-out start position: `"end"` (default — last
    /// `fade_out_secs` of the clip) or a time in seconds.
    #[serde(default = "default_end")]
    pub fade_out_start: String,
    /// Output container extension (no dot). `None` → `"wav"` for
    /// PCM codecs, `"m4a"` otherwise.
    pub output_format: Option<String>,
    /// Audio encoder. `None` → `pcm_s16le` (wav default).
    pub codec: Option<String>,
}

fn default_end() -> String {
    "end".to_string()
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

fn content_type_for(ext: &str) -> Option<String> {
    let mime = match ext {
        "wav" => "audio/wav",
        "mp3" => "audio/mpeg",
        "m4a" | "mp4" | "mov" | "mkv" => "audio/mp4",
        "ogg" | "oga" | "opus" => "audio/ogg",
        "flac" => "audio/flac",
        "aac" | "adts" => "audio/aac",
        _ => return None,
    };
    Some(mime.to_string())
}

fn check_size(input: &[u8], limits: &MediaLimits) -> MediaResult<()> {
    if let Some(max) = limits.max_input_bytes
        && (input.len() as u64) > max
    {
        return Err(MediaError::LimitExceeded(format!(
            "input is {} bytes, exceeding max_input_bytes {max}",
            input.len()
        )));
    }
    Ok(())
}

fn validate_options(options: &FadeOptions) -> MediaResult<()> {
    for (name, v) in [
        ("fade_in_secs", options.fade_in_secs),
        ("fade_out_secs", options.fade_out_secs),
    ] {
        if !v.is_finite() || v < 0.0 {
            return Err(MediaError::InvalidInput(format!(
                "{name} must be finite and >= 0, got {v}"
            )));
        }
    }
    if options.fade_in_secs == 0.0 && options.fade_out_secs == 0.0 {
        return Err(MediaError::InvalidInput(
            "at least one of fade_in_secs/fade_out_secs must be > 0".to_string(),
        ));
    }
    // fade_out_start: "end" or a non-negative finite number.
    if options.fade_out_start != "end" {
        let v: f64 = options.fade_out_start.parse().map_err(|_| {
            MediaError::InvalidInput(format!(
                "fade_out_start must be \"end\" or seconds, got {:?}",
                options.fade_out_start
            ))
        })?;
        if !v.is_finite() || v < 0.0 {
            return Err(MediaError::InvalidInput(format!(
                "fade_out_start must be >= 0, got {v}"
            )));
        }
    }
    Ok(())
}

/// Apply fade-in/fade-out to the first audio stream of `input`.
///
/// # Errors
///
/// - [`MediaError::InvalidInput`] — empty input, both fades zero,
///   negative/non-finite durations, bad `fade_out_start`.
/// - [`MediaError::LimitExceeded`] — byte caps.
/// - [`MediaError::Unsupported`] — `afade` filter or chosen codec
///   missing.
/// - [`MediaError::ToolFailed`] — undecodable input / no audio.
pub fn apply_fades(
    input: &[u8],
    options: &FadeOptions,
    limits: &MediaLimits,
    rt: &Runtime,
) -> MediaResult<MediaOutput> {
    check_size(input, limits)?;
    if input.is_empty() {
        return Err(MediaError::InvalidInput("empty input buffer".to_string()));
    }
    validate_options(options)?;

    let caps = rt.capabilities()?;
    if !caps.has_filter("afade") {
        return Err(MediaError::Unsupported(
            "filter \"afade\" is not available in this FFmpeg build".to_string(),
        ));
    }

    let out_ext = options
        .output_format
        .as_deref()
        .map(|s| s.trim().trim_start_matches('.').to_ascii_lowercase())
        .filter(|s| !s.is_empty())
        .unwrap_or_else(|| "wav".to_string());
    let codec = options.codec.as_deref().unwrap_or(match out_ext.as_str() {
        "mp3" => "libmp3lame",
        "m4a" | "mp4" | "mov" | "mkv" => "aac",
        "ogg" | "oga" => "libvorbis",
        "opus" => "libopus",
        "flac" => "flac",
        _ => "pcm_s16le",
    });
    if !caps.has_encoder(codec) {
        return Err(MediaError::Unsupported(format!(
            "audio encoder {codec:?} is not available in this FFmpeg build"
        )));
    }

    let dir = TempDir::new("fades")?;
    let in_ext = sniff_extension(input);
    let in_name = format!("input.{in_ext}");
    let in_path = dir.write(&in_name, input)?;

    // Build the afade chain. fade-out start accepts "end" or a
    // literal seconds value; ffmpeg's afade `start_time` also accepts
    // negative offsets from EOF only via `start_sample` — we keep it
    // simple and document "end" semantics by using a very large
    // start? No — afade does NOT support "end". Instead we measure
    // the duration via ffprobe when start == "end".
    let fade_out_start = if options.fade_out_start == "end" {
        probe_duration_secs(&in_path, limits, rt)?.unwrap_or(0.0)
    } else {
        options.fade_out_start.parse::<f64>().unwrap_or(0.0)
    };

    let mut chain: Vec<String> = Vec::new();
    let curve = options.curve.ffmpeg_name();
    if options.fade_in_secs > 0.0 {
        chain.push(format!(
            "afade=t=in:st=0:d={}:curve={curve}",
            options.fade_in_secs
        ));
    }
    if options.fade_out_secs > 0.0 {
        let start = (fade_out_start - options.fade_out_secs).max(0.0);
        chain.push(format!(
            "afade=t=out:st={start}:d={}:curve={curve}",
            options.fade_out_secs
        ));
    }
    let af = chain.join(",");

    let out_name = format!("output.{out_ext}");
    let mut argv: Vec<OsString> = vec![
        "-hide_banner".into(),
        "-loglevel".into(),
        "error".into(),
        "-nostdin".into(),
        "-y".into(),
        "-i".into(),
        in_path.into_os_string(),
        "-map".into(),
        "0:a:0".into(),
        "-vn".into(),
        "-af".into(),
        af.into(),
        "-c:a".into(),
        codec.into(),
    ];
    match codec {
        "aac" => argv.extend(["-b:a".into(), "128k".into()]),
        "libmp3lame" => argv.extend(["-b:a".into(), "192k".into()]),
        "libopus" => argv.extend(["-b:a".into(), "128k".into()]),
        "libvorbis" => argv.extend(["-b:a".into(), "160k".into()]),
        _ => {}
    }
    argv.push(dir.file(&out_name).into_os_string());

    let out = process::run_checked("ffmpeg", &rt.ffmpeg, &argv, None, limits)?;

    let bytes = dir.read_limited(&out_name, limits.max_output_bytes)?;
    if bytes.is_empty() {
        return Err(MediaError::ToolFailed {
            tool: "ffmpeg",
            status: "empty output".to_string(),
            detail: "fade produced a zero-byte file".to_string(),
        });
    }

    let mut result = MediaOutput {
        primary: Some(crate::crates::multimedia_engine::model::MediaFile {
            name: out_name,
            bytes,
            content_type: content_type_for(&out_ext),
        }),
        extra: Vec::new(),
        warnings: Vec::new(),
    };
    if !out.stderr.is_empty() {
        result.warnings.push(out.stderr);
    }
    Ok(result)
}

/// Probe the media duration via ffprobe (needed to position a
/// fade-out at EOF). Returns `None` when duration is unknown —
/// caller falls back to start=0 which ffmpeg clamps.
fn probe_duration_secs(
    path: &std::path::Path,
    limits: &MediaLimits,
    rt: &Runtime,
) -> MediaResult<Option<f64>> {
    let args = process::args(&[
        "-v",
        "error",
        "-print_format",
        "json",
        "-show_format",
        path.to_str().unwrap_or("input"),
    ]);
    let out = process::run("ffprobe", &rt.ffprobe, &args, None, limits)?;
    if !out.success || out.stdout.is_empty() {
        return Ok(None);
    }
    #[derive(serde::Deserialize)]
    struct Fmt {
        format: Option<crate::crates::multimedia_engine::probe::types::FormatInfo>,
    }
    let parsed: Fmt = serde_json::from_slice(&out.stdout).unwrap_or(Fmt { format: None });
    Ok(parsed.format.and_then(|f| f.duration_secs()))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::crates::multimedia_engine::runtime::process;

    fn sine_wav(secs: u32) -> Option<Vec<u8>> {
        let rt = Runtime::discover().ok()?;
        let dir = TempDir::new("fade-test").ok()?;
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
    fn apply_fades_produces_audio_output() {
        let Some(input) = sine_wav(2) else {
            eprintln!("skipping: ffmpeg lavfi unavailable");
            return;
        };
        let rt = Runtime::discover().unwrap();
        let opts = FadeOptions {
            fade_in_secs: 0.2,
            fade_out_secs: 0.2,
            ..Default::default()
        };
        let out =
            apply_fades(&input, &opts, &MediaLimits::default(), &rt).expect("apply_fades failed");
        let file = out.primary.expect("primary");
        assert!(file.name.ends_with(".wav"));
        assert!(!file.bytes.is_empty());
    }

    #[test]
    fn apply_fades_rejects_zero_fades() {
        let rt = match Runtime::discover() {
            Ok(rt) => rt,
            Err(_) => return,
        };
        let input = sine_wav(1).unwrap_or_default();
        let err = apply_fades(
            &input,
            &FadeOptions::default(),
            &MediaLimits::default(),
            &rt,
        )
        .expect_err("must fail");
        assert!(matches!(err, MediaError::InvalidInput(_)));
    }
}
