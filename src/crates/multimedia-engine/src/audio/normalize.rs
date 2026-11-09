//! Two-pass EBU R128 loudness normalization via ffmpeg's `loudnorm`
//! filter.
//!
//! Loudness normalization is inherently two-pass: pass 1 measures the
//! input's integrated loudness, true peak, loudness range and gating
//! threshold; pass 2 feeds those measurements back into `loudnorm`
//! with `linear=true` so the filter applies a **constant gain** (plus
//! true-peak limiting when needed) instead of dynamic compression —
//! preserving the program's dynamics.
//!
//! - [`measure_loudness`] runs pass 1 only and returns a
//!   [`LoudnessReport`] — reusable by analysis ops.
//! - [`normalize_loudness`] runs both passes and returns the
//!   re-encoded audio as a [`MediaOutput`], with the measured values
//!   summarised in `warnings`.

use std::ffi::OsString;

use serde::{Deserialize, Serialize};

use crate::crates::multimedia_engine::error::{MediaError, MediaResult};
use crate::crates::multimedia_engine::model::{MediaLimits, MediaOutput};
use crate::crates::multimedia_engine::runtime::discovery::Runtime;
use crate::crates::multimedia_engine::runtime::process;
use crate::crates::multimedia_engine::runtime::tempdir::TempDir;

/// Default integrated-loudness target: −16 LUFS, a common target for
/// web/streaming audio (EBU R128 broadcast uses −23).
pub const DEFAULT_TARGET_LUFS: f64 = -16.0;
/// Default true-peak ceiling: −1.5 dBTP (EBU R128 recommendation).
pub const DEFAULT_TRUE_PEAK: f64 = -1.5;
/// Default loudness-range target: 11 LU (EBU R128 default).
pub const DEFAULT_LOUDNESS_RANGE: f64 = 11.0;

/// Options for [`normalize_loudness`].
///
/// `output_format` is a container extension without a leading dot
/// (`"wav"`, `"mp4"`, `"m4a"`, `"ogg"`, ...). `audio_codec` and
/// `audio_bitrate` are passed as `-c:a` / `-b:a`; `None` lets ffmpeg
/// pick the container's default codec and bitrate.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct NormalizeOptions {
    /// Integrated-loudness target in LUFS. Valid range `−40..=−5`.
    pub target_lufs: f64,
    /// True-peak ceiling in dBTP. Passed to `loudnorm` as `TP`.
    pub true_peak: f64,
    /// Loudness-range target in LU. Passed to `loudnorm` as `LRA`.
    pub loudness_range: f64,
    /// Output container extension without a leading dot
    /// (e.g. `"wav"`, `"m4a"`). Becomes `output.<format>`.
    pub output_format: String,
    /// Audio encoder name (`ffmpeg -encoders` token, e.g. `"aac"`,
    /// `"libopus"`, `"pcm_s16le"`). `None` = muxer default.
    pub audio_codec: Option<String>,
    /// Audio bitrate string (e.g. `"128k"`). Passed as `-b:a`.
    /// `None` = encoder default.
    pub audio_bitrate: Option<String>,
}

impl Default for NormalizeOptions {
    fn default() -> Self {
        Self {
            target_lufs: DEFAULT_TARGET_LUFS,
            true_peak: DEFAULT_TRUE_PEAK,
            loudness_range: DEFAULT_LOUDNESS_RANGE,
            // WAV/PCM is the least surprising lossless-ish default for
            // an audio-only op; callers pick compressed formats
            // explicitly.
            output_format: "wav".to_string(),
            audio_codec: None,
            audio_bitrate: None,
        }
    }
}

impl NormalizeOptions {
    fn validate(&self) -> MediaResult<()> {
        if !self.target_lufs.is_finite() || !(-40.0..=-5.0).contains(&self.target_lufs) {
            return Err(MediaError::InvalidInput(format!(
                "target_lufs must be in -40..=-5, got {}",
                self.target_lufs
            )));
        }
        if !self.true_peak.is_finite() || self.true_peak > 0.0 {
            return Err(MediaError::InvalidInput(format!(
                "true_peak must be a finite value <= 0 dBTP, got {}",
                self.true_peak
            )));
        }
        if !self.loudness_range.is_finite() || self.loudness_range <= 0.0 {
            return Err(MediaError::InvalidInput(format!(
                "loudness_range must be a positive finite number, got {}",
                self.loudness_range
            )));
        }
        if self.output_format.trim().is_empty() {
            return Err(MediaError::InvalidInput(
                "output_format must not be empty".to_string(),
            ));
        }
        Ok(())
    }
}

/// Measured loudness characteristics of an input, as reported by
/// ffmpeg's `loudnorm` filter in pass-1 (`print_format=json`) mode.
///
/// All values are `Option` because ffmpeg emits `"-inf"`/`"inf"` for
/// degenerate inputs (silence) and older builds may omit keys.
/// Consumers must treat `None`/non-finite as "unknown".
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct LoudnessReport {
    /// Integrated loudness of the input, LUFS (`input_i`).
    pub measured_i: Option<f64>,
    /// True peak of the input, dBTP (`input_tp`).
    pub measured_tp: Option<f64>,
    /// Loudness range of the input, LU (`input_lra`).
    pub measured_lra: Option<f64>,
    /// Absolute gating threshold of the input, LUFS (`input_thresh`).
    pub measured_thresh: Option<f64>,
    /// Offset ffmpeg computed between measured and target loudness
    /// (`target_offset`), LU.
    pub target_offset: Option<f64>,
    /// Whether loudnorm chose `linear` or `dynamic` normalisation for
    /// this input (`normalization_type`).
    pub normalization_type: Option<String>,
    /// The target parameters the measurement was run against.
    pub target_lufs: f64,
    /// True-peak target used.
    pub target_tp: f64,
    /// Loudness-range target used.
    pub target_lra: f64,
}

/// Pass-1-only measurement: run `loudnorm` in analysis mode and parse
/// its JSON report. Useful for analysis/inspection without producing
/// audio.
pub fn measure_loudness(
    input: &[u8],
    limits: &MediaLimits,
    rt: &Runtime,
) -> MediaResult<LoudnessReport> {
    let opts = NormalizeOptions::default();
    let dir = TempDir::new("loudnorm-measure")?;
    let input_path = write_input(&dir, input, limits)?;

    let filter = format!(
        "loudnorm=I={}:TP={}:LRA={}:print_format=json",
        fmt_f64(opts.target_lufs),
        fmt_f64(opts.true_peak),
        fmt_f64(opts.loudness_range)
    );

    // The JSON block is printed at the end of stderr.
    let raw = run_pass1(rt, &input_path, &filter, limits)?;
    let report = parse_loudnorm_json(&raw)?;
    Ok(LoudnessReport {
        measured_i: report.input_i,
        measured_tp: report.input_tp,
        measured_lra: report.input_lra,
        measured_thresh: report.input_thresh,
        target_offset: report.target_offset,
        normalization_type: report.normalization_type,
        target_lufs: opts.target_lufs,
        target_tp: opts.true_peak,
        target_lra: opts.loudness_range,
    })
}

/// Two-pass EBU R128 loudness normalization.
///
/// Pass 1 measures the input; pass 2 re-encodes the **first audio
/// stream only** (`-vn -map 0:a:0`) through `loudnorm` in
/// `linear=true` mode so dynamics are preserved.
///
/// # Errors
///
/// - [`MediaError::InvalidInput`] — empty/oversized input, bad option
///   values, unparseable loudnorm output, or a silent input that
///   cannot be gain-normalised.
/// - [`MediaError::Unsupported`] — the `loudnorm` filter or requested
///   audio encoder is not in this FFmpeg build.
/// - [`MediaError::ToolFailed`] / [`MediaError::Timeout`] — ffmpeg
///   rejected the input or options.
pub fn normalize_loudness(
    input: &[u8],
    options: &NormalizeOptions,
    limits: &MediaLimits,
    rt: &Runtime,
) -> MediaResult<MediaOutput> {
    options.validate()?;
    check_capabilities(options, rt)?;

    let dir = TempDir::new("loudnorm")?;
    let input_path = write_input(&dir, input, limits)?;

    // ---- Pass 1: measure -------------------------------------------
    let measure_filter = format!(
        "loudnorm=I={}:TP={}:LRA={}:print_format=json",
        fmt_f64(options.target_lufs),
        fmt_f64(options.true_peak),
        fmt_f64(options.loudness_range)
    );
    let measured = run_pass1(rt, &input_path, &measure_filter, limits)
        .and_then(|raw| parse_loudnorm_json(&raw))?;

    let (Some(i), Some(tp), Some(lra), Some(thresh), Some(offset)) = (
        measured.input_i,
        measured.input_tp,
        measured.input_lra,
        measured.input_thresh,
        measured.target_offset,
    ) else {
        return Err(MediaError::InvalidInput(format!(
            "loudnorm could not measure the input ({}), cannot normalise",
            measured.normalization_type.as_deref().unwrap_or("unknown")
        )));
    };
    if !i.is_finite() {
        return Err(MediaError::InvalidInput(
            "input measures as silence (-inf LUFS); nothing to normalise".to_string(),
        ));
    }

    // ---- Pass 2: normalise with measured_* feedback ------------------
    // linear=true applies a constant gain chosen so integrated
    // loudness lands on target without dynamic compression. offset is
    // loudnorm's own measured-vs-target correction.
    let norm_filter = format!(
        "loudnorm=I={}:TP={}:LRA={}:measured_I={}:measured_TP={}:measured_LRA={}:measured_thresh={}:offset={}:linear=true",
        fmt_f64(options.target_lufs),
        fmt_f64(options.true_peak),
        fmt_f64(options.loudness_range),
        fmt_f64(i),
        fmt_f64(tp),
        fmt_f64(lra),
        fmt_f64(thresh),
        fmt_f64(offset),
    );

    let output_ext = options.output_format.trim().trim_start_matches('.');
    let output_name = format!("output.{output_ext}");
    let output_path = dir.file(&output_name);

    let mut argv: Vec<OsString> = vec![
        OsString::from("-hide_banner"),
        OsString::from("-loglevel"),
        OsString::from("error"),
        OsString::from("-nostdin"),
        OsString::from("-y"),
        OsString::from("-i"),
        input_path.clone().into_os_string(),
        // Audio-only output: drop video, take the first audio stream.
        OsString::from("-vn"),
        OsString::from("-map"),
        OsString::from("0:a:0"),
        OsString::from("-af"),
        OsString::from(&norm_filter),
    ];
    if let Some(codec) = options.audio_codec.as_deref() {
        argv.push(OsString::from("-c:a"));
        argv.push(OsString::from(codec));
    }
    if let Some(bitrate) = options.audio_bitrate.as_deref() {
        argv.push(OsString::from("-b:a"));
        argv.push(OsString::from(bitrate));
    }
    argv.push(output_path.into_os_string());

    process::run_checked("ffmpeg", &rt.ffmpeg, &argv, None, limits)?;

    let bytes = dir.read_limited(&output_name, limits.max_output_bytes)?;
    if bytes.is_empty() {
        return Err(MediaError::ToolFailed {
            tool: "ffmpeg",
            status: "empty output".to_string(),
            detail: format!("ffmpeg produced an empty {output_name}"),
        });
    }

    let mut out = MediaOutput::single(output_name.clone(), bytes);
    if let Some(file) = out.primary.as_mut() {
        file.content_type = content_type_for(output_ext);
    }
    out.warnings.push(format!(
        "loudnorm two-pass: measured I={:.2} LUFS, TP={:.2} dBTP, LRA={:.2} LU, \
         thresh={:.2} LUFS -> target I={:.2} LUFS (linear gain)",
        i, tp, lra, thresh, options.target_lufs,
    ));
    Ok(out)
}

/// Validate input size and write it into `dir` as `input.bin`.
/// ffmpeg probes by content, so a neutral extension is fine.
fn write_input(
    dir: &TempDir,
    input: &[u8],
    limits: &MediaLimits,
) -> MediaResult<std::path::PathBuf> {
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
    dir.write("input.bin", input)
}

/// Honest capabilities: `loudnorm` filter and any requested audio
/// encoder must exist in this FFmpeg build.
fn check_capabilities(options: &NormalizeOptions, rt: &Runtime) -> MediaResult<()> {
    let caps = rt.capabilities()?;
    if !caps.has_filter("loudnorm") {
        return Err(MediaError::Unsupported(
            "filter 'loudnorm' is not available in this FFmpeg build".to_string(),
        ));
    }
    if let Some(codec) = options.audio_codec.as_deref()
        && !caps.has_encoder(codec)
    {
        return Err(MediaError::Unsupported(format!(
            "audio encoder '{codec}' is not available in this FFmpeg build"
        )));
    }
    Ok(())
}

/// Shared argv for pass 1: decode all audio, run loudnorm in
/// measure-only mode (`-f null -` discards output).
fn analysis_argv(input_path: &std::path::Path, filter: &str) -> Vec<OsString> {
    [
        "-hide_banner",
        "-nostdin",
        "-i",
        &input_path.to_string_lossy(),
        "-vn",
        "-af",
        filter,
        "-f",
        "null",
        "-",
    ]
    .iter()
    .map(OsString::from)
    .collect()
}

/// Run pass 1 and return the raw stderr (which contains the loudnorm
/// JSON block).
fn run_pass1(
    rt: &Runtime,
    input_path: &std::path::Path,
    filter: &str,
    limits: &MediaLimits,
) -> MediaResult<String> {
    let argv = analysis_argv(input_path, filter);
    let out = process::run("ffmpeg", &rt.ffmpeg, &argv, None, limits)?;
    if !out.success {
        return Err(MediaError::ToolFailed {
            tool: "ffmpeg",
            status: "non-zero exit".to_string(),
            detail: out.stderr.clone(),
        });
    }
    Ok(out.stderr)
}

/// Raw loudnorm JSON fields (all optional — ffmpeg may emit `-inf`
/// or omit keys).
#[derive(Debug, Default, Deserialize)]
struct LoudnormJson {
    #[serde(default)]
    input_i: Option<f64>,
    #[serde(default)]
    input_tp: Option<f64>,
    #[serde(default)]
    input_lra: Option<f64>,
    #[serde(default)]
    input_thresh: Option<f64>,
    #[serde(default)]
    target_offset: Option<f64>,
    #[serde(default)]
    normalization_type: Option<String>,
}

/// ffmpeg prints the loudnorm JSON as the last `{...}` block on
/// stderr. Extract the outermost braces span and parse it.
fn parse_loudnorm_json(stderr: &str) -> MediaResult<LoudnormJson> {
    let start = stderr
        .rfind('{')
        .ok_or_else(|| MediaError::InvalidInput("loudnorm produced no JSON report".to_string()))?;
    let end = stderr.rfind('}').ok_or_else(|| {
        MediaError::InvalidInput("loudnorm produced malformed JSON report".to_string())
    })?;
    if end <= start {
        return Err(MediaError::InvalidInput(
            "loudnorm produced malformed JSON report".to_string(),
        ));
    }
    // loudnorm emits values like `"input_i" : "-30.12"` — quoted
    // strings, possibly "-inf". Deserialize as raw strings first.
    let json_text = &stderr[start..=end];
    let raw: serde_json::Value = serde_json::from_str(json_text)
        .map_err(|e| MediaError::InvalidInput(format!("loudnorm JSON parse failed: {e}")))?;

    Ok(LoudnormJson {
        input_i: get_f64(&raw, "input_i"),
        input_tp: get_f64(&raw, "input_tp"),
        input_lra: get_f64(&raw, "input_lra"),
        input_thresh: get_f64(&raw, "input_thresh"),
        target_offset: get_f64(&raw, "target_offset"),
        normalization_type: raw
            .get("normalization_type")
            .and_then(|v| v.as_str())
            .map(str::to_string),
    })
}

/// Read a numeric-or-string field, tolerating `"-inf"`/`"inf"`/`"nan"`.
fn get_f64(v: &serde_json::Value, key: &str) -> Option<f64> {
    match v.get(key)? {
        serde_json::Value::Number(n) => n.as_f64(),
        serde_json::Value::String(s) => parse_loudnorm_float(s.trim()),
        _ => None,
    }
}

/// Parse an ffmpeg loudnorm numeric string. `"-inf"`/`"inf"`/`"nan"`
/// map to the corresponding f64 constants; anything else is `None`.
fn parse_loudnorm_float(s: &str) -> Option<f64> {
    match s {
        "-inf" | "-infinity" => Some(f64::NEG_INFINITY),
        "inf" | "infinity" => Some(f64::INFINITY),
        "nan" => Some(f64::NAN),
        _ => s.parse::<f64>().ok(),
    }
}

/// Format an f64 for an ffmpeg filter argument: finite values get
/// 4 decimal places (enough for LUFS precision), non-finite values
/// are rejected earlier so this never sees them.
fn fmt_f64(v: f64) -> String {
    debug_assert!(v.is_finite(), "non-finite value in loudnorm filter");
    format!("{v:.4}")
}

/// Best-effort MIME type for common audio containers.
fn content_type_for(format: &str) -> Option<String> {
    let mime = match format.to_ascii_lowercase().as_str() {
        "mp4" | "m4v" | "m4a" | "aac" => "audio/mp4",
        "mkv" => "video/x-matroska",
        "webm" => "audio/webm",
        "mov" => "video/quicktime",
        "avi" => "video/x-msvideo",
        "mp3" => "audio/mpeg",
        "wav" => "audio/wav",
        "ogg" | "opus" => "audio/ogg",
        "flac" => "audio/flac",
        "gif" => "image/gif",
        _ => return None,
    };
    Some(mime.to_string())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::crates::multimedia_engine::runtime::process;

    fn runtime() -> Runtime {
        Runtime::discover().expect("ffmpeg/ffprobe must be on PATH")
    }

    fn limits() -> MediaLimits {
        MediaLimits::default()
    }

    /// Generate a quiet sine wav: 2 s, 440 Hz, volume −30 dB,
    /// pcm_s16le. Returns `None` when ffmpeg/lavfi is unavailable.
    fn quiet_wav() -> Option<Vec<u8>> {
        let rt = runtime();
        let dir = TempDir::new("loudnorm-test-in").ok()?;
        let out_path = dir.file("quiet.wav");
        let argv = process::args(&[
            "-hide_banner",
            "-loglevel",
            "error",
            "-nostdin",
            "-y",
            "-f",
            "lavfi",
            "-i",
            "sine=frequency=440:duration=2",
            "-af",
            "volume=-30dB",
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
    fn measure_then_normalize_quiet_sine() {
        let rt = runtime();
        let Some(input) = quiet_wav() else {
            eprintln!("skipping: ffmpeg lavfi unavailable");
            return;
        };

        // Pass 1 alone: input should measure well below target.
        let before = measure_loudness(&input, &limits(), &rt).expect("measure input");
        let i_before = before.measured_i.expect("measured_i");
        assert!(
            i_before < -25.0,
            "quiet sine should measure quiet, got {i_before} LUFS"
        );

        // Two-pass normalise to the default −16 LUFS.
        let opts = NormalizeOptions::default();
        let out = normalize_loudness(&input, &opts, &limits(), &rt).expect("normalize");
        let file = out.primary.expect("primary output");
        assert_eq!(file.name, "output.wav");
        assert!(file.bytes.len() > 44, "wav should have data");
        assert!(
            out.warnings.iter().any(|w| w.contains("loudnorm")),
            "expected loudnorm warning, got {:?}",
            out.warnings
        );

        // Measure the normalised output: should be much closer to
        // −16 LUFS than the input was.
        let after = measure_loudness(&file.bytes, &limits(), &rt).expect("measure output");
        let i_after = after.measured_i.expect("measured_i after");
        assert!(
            (i_after - opts.target_lufs).abs() < 2.0,
            "normalised loudness {i_after} LUFS not within 2 LU of {}",
            opts.target_lufs
        );
        assert!(
            (i_after - i_before).abs() > 5.0,
            "normalisation should change loudness significantly ({i_before} -> {i_after})"
        );
    }

    #[test]
    fn normalize_rejects_invalid_target() {
        let rt = runtime();
        let input = vec![0u8; 1024]; // contents irrelevant; validation runs first
        for bad in [-50.0, -3.0, f64::NAN] {
            let opts = NormalizeOptions {
                target_lufs: bad,
                ..Default::default()
            };
            let err = normalize_loudness(&input, &opts, &limits(), &rt)
                .expect_err("must reject bad target_lufs");
            assert!(
                matches!(err, MediaError::InvalidInput(_)),
                "target {bad}: unexpected error {err:?}"
            );
        }
    }

    #[test]
    fn measure_rejects_garbage() {
        let rt = runtime();
        let garbage = vec![0xDEu8, 0xAD, 0xBE, 0xEF, 1, 2, 3, 4];
        let err = measure_loudness(&garbage, &limits(), &rt).expect_err("must fail");
        assert!(
            matches!(
                err,
                MediaError::ToolFailed { .. } | MediaError::InvalidInput(_)
            ),
            "unexpected error: {err:?}"
        );
    }
}
