//! Dynamic-range compression via ffmpeg's `acompressor` filter.
//!
//! [`compress`] extracts the first audio stream and applies
//! `acompressor` with threshold/ratio/attack/release/makeup/knee
//! controls. Output is audio-only — same contract as
//! `audio::volume`.
//!
//! # Units
//!
//! The public API speaks decibels and milliseconds; ffmpeg's
//! `acompressor` takes `threshold` and `makeup` as **linear
//! amplitudes** and `attack`/`release` in milliseconds, so the op
//! converts:
//!
//! - `threshold_db` → `threshold = 10^(db/20)` (range `0.000976563..=1`)
//! - `makeup_db` → `makeup = 10^(db/20)` (range `1..=64`)
//! - `attack_ms`/`release_ms` are passed through unchanged.
//!
//! `knee` is passed through raw — ffmpeg's knee is a width-like
//! parameter in `1..=8` (default `2.82843`), not decibels.

use std::ffi::OsString;

use serde::{Deserialize, Serialize};

use crate::crates::multimedia_engine::error::{MediaError, MediaResult};
use crate::crates::multimedia_engine::model::{MediaLimits, MediaOutput};
use crate::crates::multimedia_engine::runtime::discovery::Runtime;
use crate::crates::multimedia_engine::runtime::process;
use crate::crates::multimedia_engine::runtime::tempdir::TempDir;

/// Level-detection mode for `acompressor`.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
pub enum CompressorDetection {
    /// Peak detection.
    Peak,
    /// RMS detection. Default — matches ffmpeg's default.
    #[default]
    Rms,
}

impl CompressorDetection {
    fn ffmpeg_name(self) -> &'static str {
        match self {
            Self::Peak => "peak",
            Self::Rms => "rms",
        }
    }
}

/// Options for [`compress`].
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct CompressorOptions {
    /// Threshold in dB. Converted to ffmpeg's linear `threshold`
    /// (`10^(db/20)`). Must be finite and in `-60..=0`. Default `-20`.
    #[serde(default = "default_threshold_db")]
    pub threshold_db: f64,
    /// Compression ratio (`ratio=`). Must be finite and in `1..=20`.
    /// `1.0` = no compression. Default `4.0`.
    #[serde(default = "default_ratio")]
    pub ratio: f64,
    /// Attack time in milliseconds (`attack=`). Must be finite and in
    /// `0.01..=2000`. Default `20.0`.
    #[serde(default = "default_attack_ms")]
    pub attack_ms: f64,
    /// Release time in milliseconds (`release=`). Must be finite and in
    /// `0.01..=9000`. Default `250.0`.
    #[serde(default = "default_release_ms")]
    pub release_ms: f64,
    /// Make-up gain in dB (`makeup=`). Converted to ffmpeg's linear
    /// `makeup` (`10^(db/20)`), clamped to ffmpeg's `1..=64`. Must be
    /// finite and in `-20..=20`. Default `0.0`.
    #[serde(default)]
    pub makeup_db: f64,
    /// Knee width, passed through raw to ffmpeg (`knee=`). Must be
    /// finite and in `1..=8` when set. `None` → ffmpeg default
    /// (`2.82843`).
    #[serde(default)]
    pub knee: Option<f64>,
    /// Level-detection mode (`detection=`). Default [`Rms`].
    #[serde(default)]
    pub detection: CompressorDetection,
    /// Wet/dry mix in `0..=1` (`mix=`). `1.0` = fully compressed.
    /// `None` → ffmpeg default (`1`).
    #[serde(default)]
    pub mix: Option<f64>,
    /// Output container extension (no dot). `None` → `"wav"` for PCM
    /// codecs, `"m4a"`/`"mp3"`/`"ogg"`/`"opus"`/`"flac"` matching
    /// `codec`.
    pub output_format: Option<String>,
    /// Audio encoder. `None` → `pcm_s16le` for `wav`, a sensible
    /// lossy default otherwise.
    pub codec: Option<String>,
    /// Audio bitrate for lossy codecs (`-b:a`).
    pub bitrate: Option<String>,
}

fn default_threshold_db() -> f64 {
    -20.0
}
fn default_ratio() -> f64 {
    4.0
}
fn default_attack_ms() -> f64 {
    20.0
}
fn default_release_ms() -> f64 {
    250.0
}

impl Default for CompressorOptions {
    fn default() -> Self {
        Self {
            threshold_db: default_threshold_db(),
            ratio: default_ratio(),
            attack_ms: default_attack_ms(),
            release_ms: default_release_ms(),
            makeup_db: 0.0,
            knee: None,
            detection: CompressorDetection::default(),
            mix: None,
            output_format: None,
            codec: None,
            bitrate: None,
        }
    }
}

/// Apply dynamic-range compression to the first audio stream of
/// `input`.
///
/// Runs `ffmpeg -i <in> -map 0:a:0 -vn -af acompressor=...
/// [-c:a codec] [-b:a bitrate] output.<format>`.
///
/// # Errors
///
/// - [`MediaError::InvalidInput`] — empty/oversized input, out-of-range
///   compressor parameters, empty `output_format`.
/// - [`MediaError::LimitExceeded`] — byte caps.
/// - [`MediaError::Unsupported`] — `acompressor` filter or chosen
///   encoder/muxer missing.
/// - [`MediaError::ToolFailed`] — undecodable input / no audio.
pub fn compress(
    input: &[u8],
    options: &CompressorOptions,
    limits: &MediaLimits,
    rt: &Runtime,
) -> MediaResult<MediaOutput> {
    check_size(input, limits)?;
    if input.is_empty() {
        return Err(MediaError::InvalidInput("empty input buffer".to_string()));
    }
    options.validate()?;

    let caps = rt.capabilities()?;
    if !caps.has_filter("acompressor") {
        return Err(MediaError::Unsupported(
            "filter \"acompressor\" is not available in this FFmpeg build".to_string(),
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
    if !caps.has_muxer(&out_ext) {
        return Err(MediaError::Unsupported(format!(
            "muxer {out_ext:?} is not available in this FFmpeg build"
        )));
    }

    let af = options.filter_string();

    let dir = TempDir::new("compressor")?;
    let in_ext = sniff_extension(input);
    let in_name = format!("input.{in_ext}");
    let in_path = dir.write(&in_name, input)?;

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
    if let Some(bitrate) = options.bitrate.as_deref() {
        argv.extend(["-b:a".into(), bitrate.into()]);
    }
    argv.push(dir.file(&out_name).into_os_string());

    let out = process::run_checked("ffmpeg", &rt.ffmpeg, &argv, None, limits)?;

    let bytes = dir.read_limited(&out_name, limits.max_output_bytes)?;
    if bytes.is_empty() {
        return Err(MediaError::ToolFailed {
            tool: "ffmpeg",
            status: "empty output".to_string(),
            detail: "compressor produced a zero-byte file".to_string(),
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

impl CompressorOptions {
    fn validate(&self) -> MediaResult<()> {
        if !self.threshold_db.is_finite() || !(-60.0..=0.0).contains(&self.threshold_db) {
            return Err(MediaError::InvalidInput(format!(
                "threshold_db must be finite and in -60..=0, got {}",
                self.threshold_db
            )));
        }
        if !self.ratio.is_finite() || !(1.0..=20.0).contains(&self.ratio) {
            return Err(MediaError::InvalidInput(format!(
                "ratio must be finite and in 1..=20, got {}",
                self.ratio
            )));
        }
        if !self.attack_ms.is_finite() || !(0.01..=2000.0).contains(&self.attack_ms) {
            return Err(MediaError::InvalidInput(format!(
                "attack_ms must be finite and in 0.01..=2000, got {}",
                self.attack_ms
            )));
        }
        if !self.release_ms.is_finite() || !(0.01..=9000.0).contains(&self.release_ms) {
            return Err(MediaError::InvalidInput(format!(
                "release_ms must be finite and in 0.01..=9000, got {}",
                self.release_ms
            )));
        }
        if !self.makeup_db.is_finite() || !(-20.0..=20.0).contains(&self.makeup_db) {
            return Err(MediaError::InvalidInput(format!(
                "makeup_db must be finite and in -20..=20, got {}",
                self.makeup_db
            )));
        }
        if let Some(k) = self.knee
            && (!k.is_finite() || !(1.0..=8.0).contains(&k))
        {
            return Err(MediaError::InvalidInput(format!(
                "knee must be finite and in 1..=8, got {k}"
            )));
        }
        if let Some(m) = self.mix
            && (!m.is_finite() || !(0.0..=1.0).contains(&m))
        {
            return Err(MediaError::InvalidInput(format!(
                "mix must be finite and in 0..=1, got {m}"
            )));
        }
        Ok(())
    }

    /// Render the `acompressor=...` filter string.
    fn filter_string(&self) -> String {
        // dB → linear amplitude conversions for ffmpeg's units.
        let threshold = db_to_linear(self.threshold_db).clamp(0.000976563, 1.0);
        let makeup = db_to_linear(self.makeup_db).clamp(1.0, 64.0);
        let mut s = format!(
            "acompressor=threshold={threshold:.6}:ratio={}:attack={}:release={}:makeup={makeup:.6}:detection={}",
            self.ratio,
            self.attack_ms,
            self.release_ms,
            self.detection.ffmpeg_name(),
        );
        if let Some(knee) = self.knee {
            s.push_str(&format!(":knee={knee}"));
        }
        if let Some(mix) = self.mix {
            s.push_str(&format!(":mix={mix}"));
        }
        s
    }
}

/// Convert dB to linear amplitude (`10^(db/20)`).
fn db_to_linear(db: f64) -> f64 {
    10f64.powf(db / 20.0)
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
    if input.len() >= 2 && input[0] == 0xFF && (input[1] & 0xE0) == 0xE0 {
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

#[cfg(test)]
mod tests {
    use super::*;
    use crate::crates::multimedia_engine::runtime::process;

    fn runtime() -> Option<Runtime> {
        Runtime::discover().ok()
    }

    fn limits() -> MediaLimits {
        MediaLimits::default()
    }

    /// Generate a 1 s 440 Hz stereo WAV via lavfi.
    fn sine_wav() -> Option<Vec<u8>> {
        let rt = runtime()?;
        let dir = TempDir::new("compressor-test-in").ok()?;
        let out_path = dir.file("sine.wav");
        let argv = process::args(&[
            "-hide_banner",
            "-loglevel",
            "error",
            "-nostdin",
            "-y",
            "-f",
            "lavfi",
            "-i",
            "sine=frequency=440:duration=1:sample_rate=44100",
            "-ac",
            "2",
            "-c:a",
            "pcm_s16le",
            out_path.to_str()?,
        ]);
        let out = process::run("ffmpeg", &rt.ffmpeg, &argv, None, &limits()).ok()?;
        if !out.success {
            return None;
        }
        std::fs::read(&out_path).ok()
    }

    #[test]
    fn filter_string_converts_units() {
        let opts = CompressorOptions {
            threshold_db: -20.0,
            ratio: 4.0,
            attack_ms: 10.0,
            release_ms: 100.0,
            makeup_db: 6.0,
            knee: Some(4.0),
            detection: CompressorDetection::Peak,
            mix: Some(0.5),
            ..Default::default()
        };
        let s = opts.filter_string();
        // -20 dB → 0.1 linear; +6 dB → ~1.995 linear.
        assert!(
            s.starts_with("acompressor=threshold=0.100000:ratio=4"),
            "{s}"
        );
        assert!(s.contains("attack=10"), "{s}");
        assert!(s.contains("release=100"), "{s}");
        assert!(s.contains("makeup=1.995"), "{s}");
        assert!(s.contains("detection=peak"), "{s}");
        assert!(s.contains("knee=4"), "{s}");
        assert!(s.contains("mix=0.5"), "{s}");

        // Defaults omit knee/mix.
        let s2 = CompressorOptions::default().filter_string();
        assert!(!s2.contains("knee="), "{s2}");
        assert!(!s2.contains("mix="), "{s2}");
        assert!(s2.contains("makeup=1.000000"), "{s2}");
    }

    #[test]
    fn rejects_invalid_params() {
        let Some(rt) = runtime() else {
            eprintln!("skipping: ffmpeg unavailable");
            return;
        };
        let input = sine_wav().unwrap_or_default();
        for (mut opts, what) in [
            (CompressorOptions::default(), "threshold"),
            (CompressorOptions::default(), "ratio"),
        ] {
            match what {
                "threshold" => opts.threshold_db = 5.0,
                _ => opts.ratio = 0.5,
            }
            let err = compress(&input, &opts, &limits(), &rt).expect_err("must fail");
            assert!(matches!(err, MediaError::InvalidInput(_)), "got {err:?}");
        }
        let bad_knee = CompressorOptions {
            knee: Some(42.0),
            ..Default::default()
        };
        let err = compress(&input, &bad_knee, &limits(), &rt).expect_err("must fail");
        assert!(matches!(err, MediaError::InvalidInput(_)));
    }

    #[test]
    fn compress_produces_audio_output() {
        let (Some(input), Some(rt)) = (sine_wav(), runtime()) else {
            eprintln!("skipping: ffmpeg lavfi unavailable");
            return;
        };
        let caps = match rt.capabilities() {
            Ok(c) => c,
            Err(_) => return,
        };
        if !caps.has_filter("acompressor") || !caps.has_encoder("pcm_s16le") {
            eprintln!("skipping: acompressor/pcm_s16le unavailable");
            return;
        }
        let opts = CompressorOptions {
            threshold_db: -30.0,
            ratio: 8.0,
            makeup_db: 4.0,
            ..Default::default()
        };
        let out = compress(&input, &opts, &limits(), &rt).expect("compress");
        let file = out.primary.expect("primary");
        assert!(file.name.ends_with(".wav"));
        assert!(!file.bytes.is_empty());
    }
}
