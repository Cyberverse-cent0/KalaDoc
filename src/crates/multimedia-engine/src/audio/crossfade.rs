//! Audio crossfade between two inputs via ffmpeg's `acrossfade`
//! filter.
//!
//! [`crossfade`] takes exactly two audio byte buffers, canonicalizes
//! both to 44.1 kHz stereo float PCM inside a `-filter_complex` graph
//! (so mismatched sample rates / channel layouts are handled by
//! `aresample`/`aformat`), applies
//! `acrossfade=d=<secs>:c1=<curve>:c2=<curve>` (plus optional
//! `o=<0|1>`), and encodes the result once to an audio-only
//! container.
//!
//! Canonicalization is required because `acrossfade` needs both inputs
//! to share sample rate, sample format and channel layout. Each branch
//! is normalized explicitly; the overlap is taken from the **end** of
//! input A and the **start** of input B, so the output duration is
//! `dur_a + dur_b - duration`.

use std::ffi::OsString;

use serde::{Deserialize, Serialize};

use crate::crates::multimedia_engine::error::{MediaError, MediaResult};
use crate::crates::multimedia_engine::model::{MediaLimits, MediaOutput};
use crate::crates::multimedia_engine::runtime::discovery::Runtime;
use crate::crates::multimedia_engine::runtime::process;
use crate::crates::multimedia_engine::runtime::tempdir::TempDir;

/// Canonical sample rate both crossfade branches are resampled to.
const CANONICAL_SAMPLE_RATE: u32 = 44_100;
/// Canonical channel layout (stereo).
const CANONICAL_CHANNEL_LAYOUT: &str = "stereo";

/// `acrossfade` curve names accepted for both `c1`/`c2` (ffmpeg 7.1
/// names; this op applies one curve to both sides of the overlap).
const ALLOWED_CURVES: &[&str] = &[
    "tri", "qsin", "esin", "hsin", "log", "ipar", "qua", "cub", "squ", "cbr", "par", "exp",
    "iqsin", "ihsin", "dese", "desi", "losi", "sinc", "isinc", "quat", "quatr", "qsin2", "hsin2",
];

/// Options for [`crossfade`].
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct CrossfadeOptions {
    /// Crossfade duration in seconds (`acrossfade=d=`). Must be finite
    /// and `> 0`, and strictly shorter than the shorter input — the
    /// input durations are probed with ffprobe before running.
    pub duration_secs: f64,
    /// Fade curve applied to both sides of the overlap
    /// (`acrossfade=c1=`/`c2=`). One of `tri` (default), `qsin`,
    /// `esin`, `hsin`, `log`, `ipar`, `qua`, `cub`, `squ`, `cbr`,
    /// `par`, `exp`, `iqsin`, `ihsin`, `dese`, `desi`, `losi`,
    /// `sinc`, `isinc`, `quat`, `quatr`, `qsin2`, `hsin2`.
    #[serde(default = "default_curve")]
    pub curve: String,
    /// Overlap flag (`acrossfade=o=`): `true` overlaps the tail of A
    /// with the head of B (ffmpeg default); `false` concatenates
    /// without overlap. `None` → ffmpeg default (`true`).
    #[serde(default)]
    pub overlap: Option<bool>,
    /// Output container extension (no dot). Default `"wav"`.
    #[serde(default = "default_output_format")]
    pub output_format: String,
    /// Audio encoder. `None` → `pcm_s16le` for `wav`, a sensible
    /// lossy default otherwise.
    pub codec: Option<String>,
}

fn default_curve() -> String {
    "tri".to_string()
}

fn default_output_format() -> String {
    "wav".to_string()
}

impl Default for CrossfadeOptions {
    fn default() -> Self {
        Self {
            duration_secs: 1.0,
            curve: default_curve(),
            overlap: None,
            output_format: default_output_format(),
            codec: None,
        }
    }
}

/// Crossfade audio inputs `a` and `b` into one file.
///
/// Both inputs are decoded to canonical 44.1 kHz stereo float PCM,
/// joined with `acrossfade`, and encoded once to
/// `options.output_format`. Output duration is
/// `dur_a + dur_b - duration_secs`.
///
/// # Errors
///
/// - [`MediaError::InvalidInput`] — empty/oversized input, bad
///   `duration_secs`, unknown `curve`/`overlap`, undecodable input,
///   or duration probe failure.
/// - [`MediaError::LimitExceeded`] — byte caps.
/// - [`MediaError::Unsupported`] — `acrossfade`/`aresample`/`aformat`
///   filter, chosen encoder or muxer missing.
/// - [`MediaError::ToolFailed`] — ffmpeg failure.
pub fn crossfade(
    a: &[u8],
    b: &[u8],
    options: &CrossfadeOptions,
    limits: &MediaLimits,
    rt: &Runtime,
) -> MediaResult<MediaOutput> {
    check_size(a, limits)?;
    check_size(b, limits)?;
    if a.is_empty() {
        return Err(MediaError::InvalidInput("input a is empty".to_string()));
    }
    if b.is_empty() {
        return Err(MediaError::InvalidInput("input b is empty".to_string()));
    }
    if !options.duration_secs.is_finite() || options.duration_secs <= 0.0 {
        return Err(MediaError::InvalidInput(format!(
            "duration_secs must be finite and > 0, got {}",
            options.duration_secs
        )));
    }
    let curve = options.curve.trim().to_ascii_lowercase();
    if !ALLOWED_CURVES.contains(&curve.as_str()) {
        return Err(MediaError::InvalidInput(format!(
            "unknown crossfade curve {curve:?}; expected one of {}",
            ALLOWED_CURVES.join(", ")
        )));
    }
    let overlap = options.overlap;
    let out_ext = options
        .output_format
        .trim()
        .trim_start_matches('.')
        .to_ascii_lowercase();
    if out_ext.is_empty() {
        return Err(MediaError::InvalidInput(
            "output_format must not be empty".to_string(),
        ));
    }

    let caps = rt.capabilities()?;
    for filter in ["acrossfade", "aresample", "aformat"] {
        if !caps.has_filter(filter) {
            return Err(MediaError::Unsupported(format!(
                "filter {filter:?} is not available in this FFmpeg build"
            )));
        }
    }
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

    // --- stage inputs ----------------------------------------------------
    let dir = TempDir::new("crossfade")?;
    let in_a = dir.write(&format!("a.{}", sniff_extension(a)), a)?;
    let in_b = dir.write(&format!("b.{}", sniff_extension(b)), b)?;

    // --- probe durations; crossfade must fit inside both inputs ---------
    let dur_a = probe_duration_secs(&in_a, limits, rt)?.ok_or_else(|| {
        MediaError::InvalidInput("could not determine duration of input a".to_string())
    })?;
    let dur_b = probe_duration_secs(&in_b, limits, rt)?.ok_or_else(|| {
        MediaError::InvalidInput("could not determine duration of input b".to_string())
    })?;
    let min_dur = dur_a.min(dur_b);
    if options.duration_secs >= min_dur {
        return Err(MediaError::InvalidInput(format!(
            "duration_secs {} must be shorter than the shorter input ({min_dur}s)",
            options.duration_secs
        )));
    }

    // --- canonicalize both branches, then acrossfade --------------------
    // `acrossfade` requires identical sample rate, sample format and
    // channel layout on both inputs; normalize explicitly.
    let canon = format!(
        "aresample={CANONICAL_SAMPLE_RATE},\
         aformat=sample_fmts=fltp:channel_layouts={CANONICAL_CHANNEL_LAYOUT}"
    );
    let mut acrossfade = format!(
        "acrossfade=d={}:c1={curve}:c2={curve}",
        options.duration_secs
    );
    if let Some(o) = overlap {
        acrossfade.push_str(&format!(":o={}", o as u8));
    }
    let filter = format!(
        "[0:a:0]{canon}[a0];[1:a:0]{canon}[a1];\
         [a0][a1]{acrossfade}[aout]"
    );

    let out_name = format!("output.{out_ext}");
    let mut argv: Vec<OsString> = vec![
        "-hide_banner".into(),
        "-loglevel".into(),
        "error".into(),
        "-nostdin".into(),
        "-y".into(),
    ];
    for path in [&in_a, &in_b] {
        argv.push("-i".into());
        argv.push(path.clone().into_os_string());
    }
    argv.extend([
        "-filter_complex".into(),
        filter.into(),
        "-map".into(),
        "[aout]".into(),
        "-vn".into(),
        "-c:a".into(),
        codec.into(),
    ]);
    argv.push(dir.file(&out_name).into_os_string());

    let out =
        process::run_checked("ffmpeg", &rt.ffmpeg, &argv, None, limits).map_err(|e| match e {
            MediaError::ToolFailed { detail, .. } => MediaError::InvalidInput(format!(
                "an input could not be decoded as audio: {detail}"
            )),
            other => other,
        })?;

    let bytes = dir.read_limited(&out_name, limits.max_output_bytes)?;
    if bytes.is_empty() {
        return Err(MediaError::ToolFailed {
            tool: "ffmpeg",
            status: "empty output".to_string(),
            detail: "crossfade produced a zero-byte file".to_string(),
        });
    }

    let mut result = MediaOutput::single(out_name, bytes);
    if let Some(primary) = result.primary.as_mut() {
        primary.content_type = content_type_for(&out_ext);
    }
    if !out.stderr.is_empty() {
        result.warnings.push(out.stderr);
    }
    Ok(result)
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

/// Probe the media duration via ffprobe. Returns `None` when duration
/// is unknown — the caller treats that as an invalid input since the
/// crossfade length cannot be validated.
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

#[cfg(test)]
mod tests {
    use super::*;
    use crate::crates::multimedia_engine::probe::inspect::inspect;
    use crate::crates::multimedia_engine::runtime::process;

    fn runtime() -> Option<Runtime> {
        Runtime::discover().ok()
    }

    /// Generate a sine WAV of `duration` seconds at `frequency` Hz.
    fn sine_wav(freq: u32, duration_secs: u32) -> Option<Vec<u8>> {
        let rt = runtime()?;
        let dir = TempDir::new("crossfade-test-in").ok()?;
        let out_path = dir.file("tone.wav");
        let argv = process::args(&[
            "-hide_banner",
            "-loglevel",
            "error",
            "-nostdin",
            "-y",
            "-f",
            "lavfi",
            "-i",
            &format!("sine=frequency={freq}:duration={duration_secs}:sample_rate=44100"),
            "-c:a",
            "pcm_s16le",
            out_path.to_str()?,
        ]);
        let out = process::run("ffmpeg", &rt.ffmpeg, &argv, None, &MediaLimits::default()).ok()?;
        if !out.success {
            return None;
        }
        std::fs::read(&out_path).ok()
    }

    /// Duration of the primary output in seconds via ffprobe.
    fn out_duration(out: &MediaOutput) -> f64 {
        let primary = out.primary.as_ref().expect("primary output");
        let info = inspect(&primary.bytes, &MediaLimits::default()).expect("probe output");
        info.duration_secs.expect("output duration")
    }

    #[test]
    fn rejects_bad_duration_and_curve() {
        let Some(rt) = runtime() else {
            eprintln!("skipping: ffmpeg unavailable");
            return;
        };
        let dummy = vec![0x52, 0x49, 0x46, 0x46, 0, 0, 0, 0, b'W', b'A', b'V', b'E'];
        for bad in [0.0, -1.0, f64::NAN, f64::INFINITY] {
            let opts = CrossfadeOptions {
                duration_secs: bad,
                ..Default::default()
            };
            let err = crossfade(&dummy, &dummy, &opts, &MediaLimits::default(), &rt)
                .expect_err("bad duration must fail");
            assert!(matches!(err, MediaError::InvalidInput(_)), "got {err:?}");
        }
        let opts = CrossfadeOptions {
            curve: "bogus".to_string(),
            ..Default::default()
        };
        let err = crossfade(&dummy, &dummy, &opts, &MediaLimits::default(), &rt)
            .expect_err("bad curve must fail");
        assert!(matches!(err, MediaError::InvalidInput(_)), "got {err:?}");
    }

    #[test]
    fn crossfade_two_sines_duration_is_sum_minus_overlap() {
        let (Some(a), Some(b), Some(rt)) = (sine_wav(440, 1), sine_wav(880, 1), runtime()) else {
            eprintln!("skipping: ffmpeg lavfi unavailable");
            return;
        };
        let caps = match rt.capabilities() {
            Ok(c) => c,
            Err(_) => return,
        };
        if !caps.has_filter("acrossfade")
            || !caps.has_filter("aresample")
            || !caps.has_filter("aformat")
            || !caps.has_encoder("pcm_s16le")
        {
            eprintln!("skipping: acrossfade/aresample/aformat/pcm_s16le unavailable");
            return;
        }
        let opts = CrossfadeOptions {
            duration_secs: 0.3,
            curve: "qsin".to_string(),
            ..Default::default()
        };
        let out = crossfade(&a, &b, &opts, &MediaLimits::default(), &rt).expect("crossfade");
        let primary = out.primary.as_ref().expect("primary");
        assert!(primary.name.ends_with(".wav"));
        assert_eq!(primary.content_type.as_deref(), Some("audio/wav"));
        assert!(!primary.bytes.is_empty());
        // 1s + 1s - 0.3s overlap = 1.7s (allow encoder padding).
        let dur = out_duration(&out);
        assert!((dur - 1.7).abs() < 0.2, "duration {dur}");
    }
}
