//! Silence removal via ffmpeg's `silenceremove` audio filter.
//!
//! `silenceremove` counts runs of **non-silence** ("periods") separated
//! by silence and drops the silent gaps that match the configured
//! duration/threshold. It can trim leading silence (`start_*`),
//! trailing silence, or every silent gap in the middle
//! (`stop_periods = -1`, the default here).
//!
//! # Period semantics (from `ffmpeg-filters` docs)
//!
//! - `stop_periods = 0` disables end/middle trimming. A **positive**
//!   value trims once that many trailing silent periods have been
//!   seen (keeps the first N non-silent sections). A **negative**
//!   value (e.g. `-1`) re-arms the counter after each removal, so
//!   *every* silent period meeting `stop_duration`/`stop_threshold`
//!   is dropped — this is what "remove all silence" means.
//! - `start_periods = 0` (our `None` default) leaves the head
//!   untouched; `Some(n)` with `n >= 1` trims leading silence up to
//!   the `n`-th non-silent period.
//!
//! The first audio stream only is processed (`-vn -map 0:a:0`); the
//! output is always re-encoded because removing samples is not
//! possible with stream copy.

use std::ffi::OsString;

use serde::{Deserialize, Serialize};

use crate::crates::multimedia_engine::error::{MediaError, MediaResult};
use crate::crates::multimedia_engine::model::{MediaLimits, MediaOutput};
use crate::crates::multimedia_engine::runtime::discovery::Runtime;
use crate::crates::multimedia_engine::runtime::process;
use crate::crates::multimedia_engine::runtime::tempdir::TempDir;

/// Default minimum silence duration removed by `stop_*`: 0.5 s.
pub const DEFAULT_STOP_DURATION: f64 = 0.5;
/// Default silence threshold: −50 dB.
pub const DEFAULT_THRESHOLD_DB: f64 = -50.0;

/// Options for [`remove_silence`].
///
/// All fields map onto `silenceremove` filter options; see the module
/// docs for period semantics. Thresholds are always interpreted as
/// decibels (`dB` is appended to the filter value).
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct RemoveSilenceOptions {
    /// Number of trailing silence periods to remove (`stop_periods=`).
    /// `None` maps to `-1` — remove *every* silent gap meeting
    /// `stop_duration`/`stop_threshold_db`. Positive values keep only
    /// the first N non-silent sections (trailing trim); negative
    /// values other than `-1` re-arm every N periods. Must be an
    /// integer when `Some`.
    #[serde(default)]
    pub stop_periods: Option<f32>,
    /// Minimum silence duration in seconds that is removed
    /// (`stop_duration=`). Default [`DEFAULT_STOP_DURATION`] (0.5 s).
    #[serde(default = "default_stop_duration")]
    pub stop_duration: f64,
    /// Silence threshold in dB (`stop_threshold=`): samples quieter
    /// than this count as silence. Default [`DEFAULT_THRESHOLD_DB`]
    /// (−50 dB).
    #[serde(default = "default_threshold_db")]
    pub stop_threshold_db: f64,
    /// Leading-silence counterpart of `stop_periods`
    /// (`start_periods=`). `None`/`0` leaves the head untouched;
    /// `Some(n >= 1)` trims leading silence up to the `n`-th
    /// non-silent period. Must be a non-negative integer.
    #[serde(default)]
    pub start_periods: Option<f32>,
    /// Minimum non-silence run that ends leading-silence trimming
    /// (`start_duration=`). Default `0.0` (any non-silent sample
    /// ends the trim).
    #[serde(default)]
    pub start_duration: f64,
    /// Silence threshold in dB for leading-silence detection
    /// (`start_threshold=`). Default [`DEFAULT_THRESHOLD_DB`].
    #[serde(default = "default_threshold_db")]
    pub start_threshold_db: f64,
    /// Detection method (`detection=`): `"avg"`, `"rms"`, `"peak"`,
    /// `"median"`, `"ptp"` or `"dev"`. `None` uses the ffmpeg default
    /// (`rms`).
    #[serde(default)]
    pub detection: Option<String>,
    /// Analysis window in seconds (`window=`, 0–10). `None` uses the
    /// ffmpeg default (0.02 s).
    #[serde(default)]
    pub window_secs: Option<f64>,
    /// Output container extension without a leading dot. Default
    /// `"wav"` (lossless PCM, the safest choice for an audio op).
    #[serde(default = "default_output_format")]
    pub output_format: String,
    /// Audio encoder (`-c:a`, an `ffmpeg -encoders` name). `None`
    /// lets the muxer pick its default codec.
    #[serde(default)]
    pub codec: Option<String>,
}

fn default_stop_duration() -> f64 {
    DEFAULT_STOP_DURATION
}
fn default_threshold_db() -> f64 {
    DEFAULT_THRESHOLD_DB
}
fn default_output_format() -> String {
    "wav".to_string()
}

impl Default for RemoveSilenceOptions {
    fn default() -> Self {
        Self {
            stop_periods: None,
            stop_duration: DEFAULT_STOP_DURATION,
            stop_threshold_db: DEFAULT_THRESHOLD_DB,
            start_periods: None,
            start_duration: 0.0,
            start_threshold_db: DEFAULT_THRESHOLD_DB,
            detection: None,
            window_secs: None,
            output_format: "wav".to_string(),
            codec: None,
        }
    }
}

/// Detection methods accepted by `silenceremove`'s `detection=` option.
const DETECTION_METHODS: [&str; 6] = ["avg", "rms", "peak", "median", "ptp", "dev"];

fn validate_periods(name: &str, value: f32, allow_negative: bool) -> MediaResult<()> {
    if !value.is_finite() || value.fract() != 0.0 {
        return Err(MediaError::InvalidInput(format!(
            "{name} must be a finite integer, got {value}"
        )));
    }
    if !allow_negative && value < 0.0 {
        return Err(MediaError::InvalidInput(format!(
            "{name} must be >= 0, got {value}"
        )));
    }
    Ok(())
}

fn validate_options(options: &RemoveSilenceOptions) -> MediaResult<()> {
    for (name, value) in [
        ("stop_duration", options.stop_duration),
        ("start_duration", options.start_duration),
    ] {
        if !value.is_finite() || value < 0.0 {
            return Err(MediaError::InvalidInput(format!(
                "{name} must be finite and >= 0, got {value}"
            )));
        }
    }
    for (name, value) in [
        ("stop_threshold_db", options.stop_threshold_db),
        ("start_threshold_db", options.start_threshold_db),
    ] {
        if !value.is_finite() {
            return Err(MediaError::InvalidInput(format!(
                "{name} must be finite, got {value}"
            )));
        }
    }
    if let Some(p) = options.stop_periods {
        // Negative values are meaningful (-1 = all periods); 0 would
        // disable stop-side trimming, which is allowed but unusual.
        validate_periods("stop_periods", p, true)?;
    }
    if let Some(p) = options.start_periods {
        validate_periods("start_periods", p, false)?;
    }
    if let Some(d) = options.detection.as_deref() {
        let d = d.trim().to_ascii_lowercase();
        if !DETECTION_METHODS.contains(&d.as_str()) {
            return Err(MediaError::InvalidInput(format!(
                "detection must be one of {DETECTION_METHODS:?}, got {d:?}"
            )));
        }
    }
    if let Some(w) = options.window_secs
        && (!w.is_finite() || !(0.0..=10.0).contains(&w))
    {
        return Err(MediaError::InvalidInput(format!(
            "window_secs must be in 0..=10, got {w}"
        )));
    }
    if options.output_format.trim().is_empty() {
        return Err(MediaError::InvalidInput(
            "output_format must not be empty".to_string(),
        ));
    }
    Ok(())
}

/// Build the `silenceremove=...` filter string. Every managed option
/// is emitted explicitly so ffmpeg defaults cannot drift under us;
/// `None` period values map to `0`/`-1` per the module docs.
fn silenceremove_filter(options: &RemoveSilenceOptions) -> String {
    let stop_periods = options.stop_periods.unwrap_or(-1.0);
    let start_periods = options.start_periods.unwrap_or(0.0);
    let mut filter = format!(
        "silenceremove=\
         start_periods={start_periods:.0}:\
         start_duration={}:\
         start_threshold={}dB:\
         stop_periods={stop_periods:.0}:\
         stop_duration={}:\
         stop_threshold={}dB",
        fmt_f64(options.start_duration),
        fmt_f64(options.start_threshold_db),
        fmt_f64(options.stop_duration),
        fmt_f64(options.stop_threshold_db),
    );
    if let Some(d) = options.detection.as_deref() {
        filter.push_str(&format!(":detection={}", d.trim().to_ascii_lowercase()));
    }
    if let Some(w) = options.window_secs {
        filter.push_str(&format!(":window={}", fmt_f64(w)));
    }
    filter
}

/// Format an f64 for a filter argument: plain decimal, enough
/// precision for sub-millisecond durations. Inputs are validated
/// finite beforehand.
fn fmt_f64(v: f64) -> String {
    debug_assert!(v.is_finite(), "non-finite value in silenceremove filter");
    format!("{v}")
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

/// Map an output extension to the ffmpeg muxer name that writes it.
fn muxer_for(ext: &str) -> Option<&'static str> {
    match ext {
        "wav" => Some("wav"),
        "mp3" => Some("mp3"),
        "flac" => Some("flac"),
        "ogg" | "oga" => Some("ogg"),
        "opus" => Some("ogg"),
        "m4a" | "mp4" | "mov" => Some("mp4"),
        "mkv" => Some("matroska"),
        "webm" => Some("webm"),
        "aac" | "adts" => Some("adts"),
        _ => None,
    }
}

/// Best-effort MIME type for common audio containers.
fn content_type_for(ext: &str) -> Option<String> {
    let mime = match ext {
        "mp3" => "audio/mpeg",
        "wav" => "audio/wav",
        "flac" => "audio/flac",
        "ogg" | "oga" | "opus" => "audio/ogg",
        "aac" | "adts" => "audio/aac",
        "m4a" | "mp4" | "mov" | "mkv" => "audio/mp4",
        "webm" => "audio/webm",
        _ => return None,
    };
    Some(mime.to_string())
}

/// Remove silent portions of the first audio stream of `input`.
///
/// Runs `ffmpeg -i input -vn -map 0:a:0 -af silenceremove=...` and
/// re-encodes to `output_format` (default `wav`). With the default
/// options, every silent gap of at least 0.5 s below −50 dB is
/// removed — including leading and trailing silence once `start_*`
/// or `stop_periods` semantics apply (see module docs).
///
/// # Errors
///
/// - [`MediaError::InvalidInput`] — empty/oversized input, non-finite
///   or out-of-range options.
/// - [`MediaError::LimitExceeded`] — input/output byte caps.
/// - [`MediaError::Unsupported`] — `silenceremove`, the requested
///   encoder, or the output muxer is missing from this FFmpeg build.
/// - [`MediaError::ToolFailed`] — undecodable input or no audio
///   stream.
pub fn remove_silence(
    input: &[u8],
    options: &RemoveSilenceOptions,
    limits: &MediaLimits,
    rt: &Runtime,
) -> MediaResult<MediaOutput> {
    if input.is_empty() {
        return Err(MediaError::InvalidInput("empty input buffer".to_string()));
    }
    if let Some(max) = limits.max_input_bytes
        && (input.len() as u64) > max
    {
        return Err(MediaError::LimitExceeded(format!(
            "input is {} bytes, exceeding max_input_bytes {max}",
            input.len()
        )));
    }
    validate_options(options)?;

    let caps = rt.capabilities()?;
    if !caps.has_filter("silenceremove") {
        return Err(MediaError::Unsupported(
            "filter \"silenceremove\" is not available in this FFmpeg build".to_string(),
        ));
    }

    let out_ext = options
        .output_format
        .trim()
        .trim_start_matches('.')
        .to_ascii_lowercase();
    let muxer = muxer_for(&out_ext).ok_or_else(|| {
        MediaError::Unsupported(format!("no known muxer for output_format {out_ext:?}"))
    })?;
    if !caps.has_muxer(muxer) {
        return Err(MediaError::Unsupported(format!(
            "muxer {muxer:?} (for output_format {out_ext:?}) is not available in this FFmpeg build"
        )));
    }
    if let Some(codec) = options.codec.as_deref()
        && !caps.has_encoder(codec)
    {
        return Err(MediaError::Unsupported(format!(
            "audio encoder {codec:?} is not available in this FFmpeg build"
        )));
    }

    let dir = TempDir::new("silenceremove")?;
    let in_name = format!("input.{}", sniff_extension(input));
    let in_path = dir.write(&in_name, input)?;

    let out_name = format!("output.{out_ext}");
    let out_path = dir.file(&out_name);

    let filter = silenceremove_filter(options);
    let mut argv: Vec<OsString> = vec![
        "-hide_banner".into(),
        "-loglevel".into(),
        "error".into(),
        "-nostdin".into(),
        "-y".into(),
        "-i".into(),
        in_path.into_os_string(),
        // Audio-only output: drop video, take the first audio stream.
        "-vn".into(),
        "-map".into(),
        "0:a:0".into(),
        "-af".into(),
        filter.into(),
    ];
    if let Some(codec) = options.codec.as_deref() {
        argv.extend(["-c:a".into(), codec.into()]);
    }
    argv.push(out_path.into_os_string());

    let out = process::run_checked("ffmpeg", &rt.ffmpeg, &argv, None, limits)?;

    let bytes = dir.read_limited(&out_name, limits.max_output_bytes)?;
    if bytes.is_empty() {
        return Err(MediaError::ToolFailed {
            tool: "ffmpeg",
            status: "empty output".to_string(),
            detail: format!("ffmpeg produced an empty {out_name}"),
        });
    }

    let mut result = MediaOutput::single(out_name.clone(), bytes);
    if let Some(file) = result.primary.as_mut() {
        file.content_type = content_type_for(&out_ext);
    }
    if !out.stderr.is_empty() {
        result.warnings.push(out.stderr);
    }
    Ok(result)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::crates::multimedia_engine::probe::inspect::inspect;
    use crate::crates::multimedia_engine::runtime::process;

    /// 1.6 s wav: 0.3 s tone, 1.0 s digital silence, 0.3 s tone.
    fn tone_silence_tone() -> Option<Vec<u8>> {
        let rt = Runtime::discover().ok()?;
        let dir = TempDir::new("silenceremove-test").ok()?;
        let out = dir.file("fixture.wav");
        let args = process::args(&[
            "-hide_banner",
            "-loglevel",
            "error",
            "-nostdin",
            "-y",
            "-f",
            "lavfi",
            "-i",
            "sine=frequency=440:duration=0.3",
            "-f",
            "lavfi",
            "-i",
            "anullsrc=r=44100:cl=mono:d=1.0",
            "-f",
            "lavfi",
            "-i",
            "sine=frequency=440:duration=0.3",
            "-filter_complex",
            "[0:a][1:a][2:a]concat=n=3:v=0:a=1[a]",
            "-map",
            "[a]",
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
    fn remove_silence_shortens_tone_gap_tone() {
        let Some(input) = tone_silence_tone() else {
            eprintln!("skipping: ffmpeg lavfi unavailable");
            return;
        };
        let rt = match Runtime::discover() {
            Ok(rt) => rt,
            Err(_) => return,
        };
        let opts = RemoveSilenceOptions {
            // Only remove the 1 s middle gap; keep both 0.3 s tones.
            stop_periods: Some(-1.0),
            stop_duration: 0.5,
            ..Default::default()
        };
        let out = remove_silence(&input, &opts, &MediaLimits::default(), &rt)
            .expect("remove_silence failed");
        let file = out.primary.expect("primary output");
        assert_eq!(file.name, "output.wav");
        assert!(
            file.bytes.len() < input.len(),
            "output ({} bytes) should be smaller than input ({} bytes)",
            file.bytes.len(),
            input.len()
        );
        assert!(file.bytes.len() > 44, "wav should have data");
        assert_eq!(
            file.content_type.as_deref(),
            Some("audio/wav"),
            "expected audio/wav content type"
        );

        // ~0.6 s of tone should remain (plus encoder padding).
        let info = inspect(&file.bytes, &MediaLimits::default()).expect("probe output");
        let dur = info.duration_secs.expect("duration");
        assert!(
            dur < 1.2,
            "expected ~0.6s after removing the 1s gap, got {dur}s"
        );
    }

    #[test]
    fn remove_silence_rejects_bad_options() {
        let rt = match Runtime::discover() {
            Ok(rt) => rt,
            Err(_) => return,
        };
        let input = b"junk".to_vec();
        let opts = RemoveSilenceOptions {
            stop_duration: f64::NAN,
            ..Default::default()
        };
        assert!(remove_silence(&input, &opts, &MediaLimits::default(), &rt).is_err());
        let opts = RemoveSilenceOptions {
            detection: Some("bogus".to_string()),
            ..Default::default()
        };
        assert!(remove_silence(&input, &opts, &MediaLimits::default(), &rt).is_err());
    }
}
