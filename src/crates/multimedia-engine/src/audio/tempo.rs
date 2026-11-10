//! Playback-tempo change for audio — pitch-preserving speed/slowdown.
//!
//! [`tempo`] applies ffmpeg's `atempo` filter to the first audio
//! stream (`-map 0:a:0 -vn`). `atempo` changes playback speed without
//! shifting pitch, which is exactly what a "tempo" op wants: a 2.0
//! factor halves the duration while the voice/music keeps its key.
//!
//! `atempo` accepts a factor in `0.5..=2.0` per filter instance, so
//! factors outside that band are realized as a *chain* of filters —
//! factor 4 becomes `atempo=2.0,atempo=2.0`, factor 0.25 becomes
//! `atempo=0.5,atempo=0.5`. Each instance is clamped into
//! `[0.5, 2.0]` and the residual is pushed into the final instance.
//!
//! `preserve_pitch` is accepted for API symmetry with pitch-shifting
//! ops elsewhere, but `atempo` *always* preserves pitch — there is no
//! pitch-shift mode here. When `false`, the op still runs unchanged
//! and records a warning so callers know the flag had no effect.
//!
//! Works on audio files *and* video containers (video is dropped).

use std::ffi::OsString;

use serde::{Deserialize, Serialize};

use crate::crates::multimedia_engine::error::{MediaError, MediaResult};
use crate::crates::multimedia_engine::model::{MediaLimits, MediaOutput};
use crate::crates::multimedia_engine::runtime::discovery::Runtime;
use crate::crates::multimedia_engine::runtime::process;
use crate::crates::multimedia_engine::runtime::tempdir::TempDir;

/// Sanity bounds for the tempo factor. `atempo` itself accepts up to
/// 100 per instance, but beyond `0.05..=20` the chain gets silly
/// long and the output duration becomes impractically short/long.
const MIN_FACTOR: f64 = 0.05;
const MAX_FACTOR: f64 = 20.0;
/// `atempo` accepts `0.5..=100` per instance, but we cap each link at
/// 2.0 (the range where its internal resampling quality is best
/// understood) and chain links for larger factors.
const MAX_LINK: f64 = 2.0;
const MIN_LINK: f64 = 0.5;

/// Options for [`tempo`].
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct TempoOptions {
    /// Playback tempo multiplier: `2.0` plays twice as fast (half
    /// the duration), `0.5` half as fast (double the duration).
    /// Must be finite and within `0.05..=20`.
    pub factor: f64,
    /// Output container extension (no dot). `None` defaults to
    /// `"wav"` — the safest lossless-ish choice for tempo work.
    pub output_format: Option<String>,
    /// Audio encoder. `None` picks per container (`aac` for
    /// m4a/mp4, `libmp3lame` for mp3, `flac` for flac,
    /// `pcm_s16le` for wav, `libopus` for opus/ogg).
    pub codec: Option<String>,
    /// Pitch is always preserved by `atempo`; this flag is kept for
    /// API symmetry. `false` currently records a warning — no
    /// pitch-shift mode is implemented.
    #[serde(default = "default_true")]
    pub preserve_pitch: bool,
}

fn default_true() -> bool {
    true
}

impl Default for TempoOptions {
    fn default() -> Self {
        Self {
            factor: 1.0,
            output_format: None,
            codec: None,
            preserve_pitch: true,
        }
    }
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
            let brand = &input[8..12.min(input.len())];
            return match brand {
                b"qt  " => "mov",
                b"M4A " | b"M4B " => "m4a",
                _ => "mp4",
            };
        }
    }
    if input[0] == 0xFF && (input[1] & 0xE0) == 0xE0 {
        return "mp3";
    }
    "bin"
}

/// ffmpeg muxer name for an output container extension.
fn muxer_for(ext: &str) -> &'static str {
    match ext {
        "wav" => "wav",
        "mp3" => "mp3",
        "flac" => "flac",
        "ogg" | "oga" | "opus" => "ogg",
        "aac" | "adts" => "adts",
        "m4a" | "mp4" | "mov" => "mp4",
        "mkv" | "webm" => "matroska",
        _ => "",
    }
}

/// Pick the encoder for the output container.
fn default_codec_for(ext: &str) -> &'static str {
    match ext {
        "mp3" => "libmp3lame",
        "wav" => "pcm_s16le",
        "flac" => "flac",
        "ogg" | "oga" => "libvorbis",
        "opus" => "libopus",
        "aac" | "adts" => "aac",
        _ => "aac", // m4a/mp4/mov/mkv/webm
    }
}

fn content_type_for(ext: &str) -> Option<String> {
    let mime = match ext {
        "mp3" => "audio/mpeg",
        "wav" => "audio/wav",
        "flac" => "audio/flac",
        "ogg" | "oga" | "opus" => "audio/ogg",
        "aac" | "adts" => "audio/aac",
        "m4a" | "mp4" | "mov" | "mkv" | "webm" => "audio/mp4",
        _ => return None,
    };
    Some(mime.to_string())
}

/// Build the `atempo` filter chain for `factor`.
///
/// Each instance is clamped to `[0.5, 2.0]`; factors outside that
/// band become multiple links whose product equals `factor` (within
/// float rounding). Examples: `4.0` → `[2.0, 2.0]`,
/// `0.25` → `[0.5, 0.5]`, `0.05` → `[0.5, 0.5, 0.2]`.
fn atempo_chain(factor: f64) -> Vec<f64> {
    let mut links = Vec::new();
    let mut remaining = factor;
    while remaining > MAX_LINK {
        links.push(MAX_LINK);
        remaining /= MAX_LINK;
    }
    while remaining < MIN_LINK {
        links.push(MIN_LINK);
        remaining /= MIN_LINK;
    }
    links.push(remaining.clamp(MIN_LINK, MAX_LINK));
    links
}

fn validate_options(options: &TempoOptions) -> MediaResult<()> {
    if !options.factor.is_finite() {
        return Err(MediaError::InvalidInput(format!(
            "factor must be finite, got {}",
            options.factor
        )));
    }
    if !(MIN_FACTOR..=MAX_FACTOR).contains(&options.factor) {
        return Err(MediaError::InvalidInput(format!(
            "factor must be within [{MIN_FACTOR}, {MAX_FACTOR}], got {}",
            options.factor
        )));
    }
    Ok(())
}

/// Change the playback tempo of `input`'s first audio stream without
/// shifting pitch.
///
/// # Errors
///
/// - [`MediaError::InvalidInput`] — empty input or `factor` outside
///   `0.05..=20` / non-finite.
/// - [`MediaError::LimitExceeded`] — input/output byte caps.
/// - [`MediaError::Unsupported`] — `atempo` filter, chosen encoder or
///   muxer missing from this FFmpeg build.
/// - [`MediaError::ToolFailed`] — undecodable input or no audio
///   stream.
pub fn tempo(
    input: &[u8],
    options: &TempoOptions,
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

    if !caps.has_filter("atempo") {
        return Err(MediaError::Unsupported(
            "atempo filter is not available in this FFmpeg build".to_string(),
        ));
    }

    let out_ext = options
        .output_format
        .as_deref()
        .map(|s| s.trim().trim_start_matches('.').to_ascii_lowercase())
        .filter(|s| !s.is_empty())
        .unwrap_or_else(|| "wav".to_string());

    let muxer = muxer_for(&out_ext);
    if muxer.is_empty() || !caps.has_muxer(muxer) {
        return Err(MediaError::Unsupported(format!(
            "muxer for output format {out_ext:?} is not available in this FFmpeg build"
        )));
    }

    let codec = options
        .codec
        .as_deref()
        .unwrap_or_else(|| default_codec_for(&out_ext));
    if !caps.has_encoder(codec) {
        return Err(MediaError::Unsupported(format!(
            "audio encoder {codec:?} is not available in this FFmpeg build"
        )));
    }

    let chain = atempo_chain(options.factor);
    let filter = chain
        .iter()
        .map(|f| format!("atempo={f:.4}"))
        .collect::<Vec<_>>()
        .join(",");

    let dir = TempDir::new("atempo")?;
    let in_ext = sniff_extension(input);
    let in_name = format!("input.{in_ext}");
    let in_path = dir.write(&in_name, input)?;

    let out_name = format!("output.{out_ext}");
    let out_path = dir.file(&out_name);

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
        filter.into(),
        "-c:a".into(),
        codec.into(),
    ];
    // Reasonable bitrate defaults for lossy codecs.
    match codec {
        "aac" => argv.extend(["-b:a".into(), "128k".into()]),
        "libmp3lame" => argv.extend(["-b:a".into(), "192k".into()]),
        "libopus" => argv.extend(["-b:a".into(), "128k".into()]),
        "libvorbis" => argv.extend(["-b:a".into(), "160k".into()]),
        _ => {}
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

    let mut warnings = Vec::new();
    if !options.preserve_pitch {
        warnings
            .push("preserve_pitch=false has no effect: atempo always preserves pitch".to_string());
    }
    if chain.len() > 1 {
        warnings.push(format!(
            "factor {} realized as {} chained atempo filters",
            options.factor,
            chain.len()
        ));
    }

    let mut result = MediaOutput::single(out_name, bytes);
    result
        .primary
        .as_mut()
        .expect("single() sets primary")
        .content_type = content_type_for(&out_ext);
    result.warnings = warnings;
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

    /// 2s 440Hz sine wav.
    fn sine_wav() -> Option<Vec<u8>> {
        let rt = Runtime::discover().ok()?;
        let dir = TempDir::new("atempo-test").ok()?;
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
            "sine=frequency=440:duration=2",
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
    fn tempo_factor_two_halves_duration() {
        let Some(input) = sine_wav() else {
            eprintln!("skipping: ffmpeg lavfi unavailable");
            return;
        };
        let rt = Runtime::discover().unwrap();
        let opts = TempoOptions {
            factor: 2.0,
            ..Default::default()
        };
        let out = tempo(&input, &opts, &MediaLimits::default(), &rt).expect("tempo failed");
        let file = out.primary.expect("primary");
        assert!(file.name.ends_with(".wav"), "unexpected {}", file.name);
        assert_eq!(
            file.content_type.as_deref(),
            Some("audio/wav"),
            "content_type"
        );

        let info = inspect(&file.bytes, &MediaLimits::default()).expect("probe");
        let dur = info.duration_secs.expect("duration");
        assert!((dur - 1.0).abs() < 0.4, "expected ~1s, got {dur}s");
        assert_eq!(info.video_streams, 0, "tempo must drop video");
    }

    #[test]
    fn tempo_rejects_bad_factor() {
        let rt = match Runtime::discover() {
            Ok(rt) => rt,
            Err(_) => return,
        };
        let input = sine_wav().unwrap_or_default();
        for factor in [f64::NAN, 0.0, -1.0, 0.01, 50.0] {
            let opts = TempoOptions {
                factor,
                ..Default::default()
            };
            assert!(
                tempo(&input, &opts, &MediaLimits::default(), &rt).is_err(),
                "factor {factor} should be rejected"
            );
        }
    }

    #[test]
    fn atempo_chain_clamps_each_link() {
        // factor 4 → two links of 2.0
        assert_eq!(atempo_chain(4.0), vec![2.0, 2.0]);
        // factor 0.25 → two links of 0.5
        assert_eq!(atempo_chain(0.25), vec![0.5, 0.5]);
        // factor 1.0 → single identity link
        assert_eq!(atempo_chain(1.0), vec![1.0]);
        // factor 20 → 2.0 x4 then 1.25
        let chain = atempo_chain(20.0);
        assert!(chain.iter().all(|f| (0.5..=2.0).contains(f)));
        let product: f64 = chain.iter().product();
        assert!((product - 20.0).abs() < 1e-9);
    }
}
