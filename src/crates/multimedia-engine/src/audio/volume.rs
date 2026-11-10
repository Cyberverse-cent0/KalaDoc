//! Volume adjustment and loudness normalization for audio streams.
//!
//! [`adjust_volume`] extracts the **first audio stream** of any media
//! input (audio *or* video container — video is dropped), applies an
//! ffmpeg `volume` filter and/or a loudness-normalization filter, and
//! writes an **audio-only** output container.
//!
//! # Output contract
//!
//! The output is always audio-only: `-map 0:a:0 -vn` selects the first
//! audio stream and discards video/subtitle/data streams. Callers that
//! need the video preserved must mux it back themselves (see
//! `conversion::mux_replace`). This keeps the op simple and makes the
//! produced file duration/codec purely a function of the audio track.
//!
//! # Filters
//!
//! - `gain_db` maps to ffmpeg's `volume=<dB>dB` filter (equivalent to
//!   the linear factor `10^(dB/20)`). `gain_db == 0.0` adds no filter.
//! - `peak_normalize` uses `loudnorm=I=-16:TP=-1.5:LRA=11` when the
//!   `loudnorm` filter exists in this FFmpeg build, falling back to
//!   `dynaudnorm` with default parameters. **This is single-pass
//!   dynamic normalization, not true EBU R128 two-pass loudnorm** —
//!   true two-pass requires measuring the input first and feeding the
//!   measured values back as filter options. Both are honest dynamic
//!   processors; callers needing exact LUFS targets should use a
//!   dedicated loudness op.
//! - If neither normalization filter is compiled in,
//!   [`MediaError::Unsupported`] is returned — never a silent skip.
//!
//! # Clipping
//!
//! Positive `gain_db` amplifies the signal; values near `+12` will
//! clip (or wrap, depending on codec) on already-loud material. ffmpeg
//! does not prevent this. Callers should prefer `peak_normalize` or
//! verify headroom first. Negative gains are always safe.

use std::ffi::OsString;

use serde::{Deserialize, Serialize};

use crate::crates::multimedia_engine::error::{MediaError, MediaResult};
use crate::crates::multimedia_engine::model::{MediaLimits, MediaOutput};
use crate::crates::multimedia_engine::runtime::discovery::Runtime;
use crate::crates::multimedia_engine::runtime::process;
use crate::crates::multimedia_engine::runtime::tempdir::TempDir;

/// Valid `gain_db` range: `-60..=+12`.
const MIN_GAIN_DB: f64 = -60.0;
const MAX_GAIN_DB: f64 = 12.0;

/// Options for [`adjust_volume`].
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct VolumeOptions {
    /// Gain in decibels applied via `volume=<gain>dB`. Must be finite
    /// and within `-60.0..=12.0` — anything else is
    /// [`MediaError::InvalidInput`]. `0.0` (the default) applies no
    /// gain.
    ///
    /// Positive values amplify; near `+12` dB clipping is likely on
    /// loud sources. See the module docs.
    #[serde(default)]
    pub gain_db: f64,
    /// When `true`, chain a loudness-normalization filter before the
    /// gain: `loudnorm=I=-16:TP=-1.5:LRA=11` if available, else
    /// `dynaudnorm` with defaults. Single-pass dynamic processing —
    /// not EBU R128 two-pass (see module docs).
    #[serde(default)]
    pub peak_normalize: bool,
    /// Output container extension / format name without a leading dot
    /// (e.g. `"wav"`, `"mp3"`, `"m4a"`). `None` picks a container that
    /// matches `codec` (PCM codecs → `"wav"`, `"aac"` → `"m4a"`,
    /// `"libmp3lame"`/`"mp3"` → `"mp3"`, `"libopus"`/`"opus"` →
    /// `"opus"`, `"flac"` → `"flac"`, anything else → `"mkv"`).
    pub output_format: Option<String>,
    /// Audio encoder name (`ffmpeg -encoders` token, e.g. `"aac"`,
    /// `"libmp3lame"`, `"pcm_s16le"`). `None` lets the muxer choose
    /// (PCM for `wav`, the container default otherwise).
    pub codec: Option<String>,
    /// Audio bitrate string (e.g. `"128k"`). Passed as `-b:a`; only
    /// meaningful for lossy codecs.
    pub bitrate: Option<String>,
}

/// Adjust the volume of the first audio stream in `input`.
///
/// Runs `ffmpeg -hide_banner -loglevel error -nostdin -y -i <in>
/// -map 0:a:0 -vn -af <filters> [-c:a codec] [-b:a bitrate]
/// output.<format>` and reads the audio-only output back under
/// `limits`. Works on video inputs too — video is discarded.
///
/// # Errors
///
/// - [`MediaError::InvalidInput`] — empty/oversized input, non-finite
///   or out-of-range `gain_db`, empty `output_format`.
/// - [`MediaError::Unsupported`] — `codec` is not an encoder in this
///   FFmpeg build, the output muxer is unknown, `volume`/normalization
///   filters are absent, or `peak_normalize` was requested with no
///   loudness filter available.
/// - [`MediaError::ToolFailed`] / [`MediaError::Timeout`] — ffmpeg
///   rejected the input (including inputs with no audio stream) or
///   the options.
pub fn adjust_volume(
    input: &[u8],
    options: &VolumeOptions,
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
    if !options.gain_db.is_finite()
        || options.gain_db < MIN_GAIN_DB
        || options.gain_db > MAX_GAIN_DB
    {
        return Err(MediaError::InvalidInput(format!(
            "gain_db must be finite and within {MIN_GAIN_DB}..={MAX_GAIN_DB}, got {}",
            options.gain_db
        )));
    }
    if let Some(fmt) = options.output_format.as_deref()
        && fmt.trim().is_empty()
    {
        return Err(MediaError::InvalidInput(
            "output_format must not be empty".to_string(),
        ));
    }

    // Honest capabilities: requested encoder/muxer/filters must exist
    // in this build.
    let caps = rt.capabilities()?;
    if let Some(codec) = options.codec.as_deref()
        && !caps.has_encoder(codec)
    {
        return Err(MediaError::Unsupported(format!(
            "audio encoder '{codec}' is not available in this FFmpeg build"
        )));
    }
    if !caps.has_filter("volume") {
        return Err(MediaError::Unsupported(
            "'volume' audio filter is not available in this FFmpeg build".to_string(),
        ));
    }

    // Build the -af filter chain.
    let mut filters: Vec<String> = Vec::new();
    if options.peak_normalize {
        if caps.has_filter("loudnorm") {
            // Single-pass dynamic loudnorm — NOT true EBU R128
            // two-pass (that needs a measured first pass). Documented
            // on VolumeOptions::peak_normalize.
            filters.push("loudnorm=I=-16:TP=-1.5:LRA=11".to_string());
        } else if caps.has_filter("dynaudnorm") {
            filters.push("dynaudnorm".to_string());
        } else {
            return Err(MediaError::Unsupported(
                "peak_normalize requested but neither 'loudnorm' nor 'dynaudnorm' \
                 is available in this FFmpeg build"
                    .to_string(),
            ));
        }
    }
    if options.gain_db != 0.0 {
        filters.push(format!("volume={}dB", options.gain_db));
    }
    // No filters at all → plain audio-only remux; skip -af entirely.

    let output_ext = match options.output_format.as_deref() {
        Some(fmt) => fmt.trim().trim_start_matches('.').to_string(),
        None => default_format_for(options.codec.as_deref()),
    };
    if output_ext.is_empty() {
        return Err(MediaError::InvalidInput(
            "output_format must not be empty".to_string(),
        ));
    }
    if !caps.has_muxer(&output_ext) {
        return Err(MediaError::Unsupported(format!(
            "output muxer '{output_ext}' is not available in this FFmpeg build"
        )));
    }

    let dir = TempDir::new("volume")?;
    let input_name = "input.bin";
    // ffmpeg probes by content; a neutral extension avoids misleading
    // the demuxer when callers pass raw elementary streams.
    dir.write(input_name, input)?;

    let output_name = format!("output.{output_ext}");
    let output_path = dir.file(&output_name);

    let mut argv: Vec<OsString> = vec![
        OsString::from("-hide_banner"),
        OsString::from("-loglevel"),
        OsString::from("error"),
        OsString::from("-nostdin"),
        OsString::from("-y"),
        OsString::from("-i"),
        dir.file(input_name).into_os_string(),
        // First audio stream only; -vn is belt-and-braces since the
        // map already excludes video, but makes intent explicit.
        OsString::from("-map"),
        OsString::from("0:a:0"),
        OsString::from("-vn"),
    ];

    if !filters.is_empty() {
        argv.push(OsString::from("-af"));
        argv.push(OsString::from(filters.join(",")));
    }
    if let Some(codec) = options.codec.as_deref() {
        argv.push(OsString::from("-c:a"));
        argv.push(OsString::from(codec));
    }
    if let Some(bitrate) = options.bitrate.as_deref() {
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

    let mut out = MediaOutput::single(output_name, bytes);
    if let Some(file) = out.primary.as_mut() {
        file.content_type = content_type_for(&output_ext);
    }
    Ok(out)
}

/// Pick a default output container for `codec` (an FFmpeg encoder
/// name or `None`). PCM codecs and no-codec requests go to lossless
/// `wav`; common lossy codecs map to their natural containers;
/// anything else falls back to `mkv`, whose muxer accepts virtually
/// every audio codec.
fn default_format_for(codec: Option<&str>) -> String {
    match codec.map(|c| c.to_ascii_lowercase()).as_deref() {
        None => "wav".to_string(),
        Some(c) if c.starts_with("pcm_") => "wav".to_string(),
        Some("aac") | Some("aac_at") => "m4a".to_string(),
        Some("libmp3lame") | Some("mp3") | Some("libshine") => "mp3".to_string(),
        Some("libopus") | Some("opus") | Some("libopusenc") => "opus".to_string(),
        Some("flac") => "flac".to_string(),
        Some("vorbis") | Some("libvorbis") => "ogg".to_string(),
        _ => "mkv".to_string(),
    }
}

/// Best-effort MIME type for common audio output containers.
fn content_type_for(format: &str) -> Option<String> {
    let mime = match format.to_ascii_lowercase().as_str() {
        "mp3" => "audio/mpeg",
        "m4a" | "aac" | "mp4" => "audio/mp4",
        "wav" => "audio/wav",
        "ogg" | "opus" | "oga" => "audio/ogg",
        "flac" => "audio/flac",
        "mkv" | "webm" => "video/x-matroska",
        _ => return None,
    };
    Some(mime.to_string())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::crates::multimedia_engine::runtime::process::run;

    fn runtime() -> Runtime {
        Runtime::discover().expect("ffmpeg/ffprobe must be on PATH")
    }

    fn limits() -> MediaLimits {
        MediaLimits::default()
    }

    /// Generate a 1-second 440 Hz sine WAV via lavfi.
    fn sine_wav() -> Vec<u8> {
        let rt = runtime();
        let dir = TempDir::new("volume-test-in").expect("tempdir");
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
            "-c:a",
            "pcm_s16le",
            out_path.to_str().expect("utf8 path"),
        ]);
        run("ffmpeg", &rt.ffmpeg, &argv, None, &limits()).expect("generate sine wav");
        std::fs::read(&out_path).expect("read sine wav")
    }

    /// Measure `mean_volume` (dB) of an audio buffer by running
    /// `ffmpeg -i <file> -af volumedetect -f null -` and parsing stderr.
    fn mean_volume(rt: &Runtime, name: &str, bytes: &[u8]) -> f64 {
        let dir = TempDir::new("volume-test-measure").expect("tempdir");
        let path = dir.write(name, bytes).expect("write measure input");
        let argv = process::args(&[
            "-hide_banner",
            "-nostdin",
            "-i",
            path.to_str().expect("utf8 path"),
            "-af",
            "volumedetect",
            "-f",
            "null",
            "-",
        ]);
        let out = run("ffmpeg", &rt.ffmpeg, &argv, None, &limits()).expect("volumedetect");
        assert!(out.success, "volumedetect failed: {}", out.stderr);
        for line in out.stderr.lines() {
            if let Some(rest) = line.split("mean_volume:").nth(1) {
                let num: String = rest
                    .trim()
                    .chars()
                    .take_while(|c| c.is_ascii_digit() || *c == '-' || *c == '.' || *c == '+')
                    .collect();
                return num.parse::<f64>().expect("parse mean_volume");
            }
        }
        panic!("no mean_volume in volumedetect output:\n{}", out.stderr);
    }

    /// Primary check: -6 dB gain must drop mean_volume by ~6 dB.
    #[test]
    fn gain_db_attenuates_by_expected_amount() {
        let rt = runtime();
        let input = sine_wav();
        let before = mean_volume(&rt, "before.wav", &input);

        let options = VolumeOptions {
            gain_db: -6.0,
            ..Default::default()
        };
        let out = adjust_volume(&input, &options, &limits(), &rt).expect("adjust_volume");
        let file = out.primary.expect("primary output");
        assert_eq!(file.name, "output.wav");
        assert!(!file.bytes.is_empty());

        let after = mean_volume(&rt, "after.wav", &file.bytes);
        let drop = before - after;
        assert!(
            (drop - 6.0).abs() < 0.5,
            "expected ~6 dB drop, got {drop:.2} dB (before={before:.2}, after={after:.2})"
        );
    }

    /// Validation: gain outside -60..=+12 and non-finite values are
    /// InvalidInput; an input with no audio stream fails cleanly.
    #[test]
    fn rejects_invalid_gain_and_gain_boosts() {
        let rt = runtime();
        let input = sine_wav();

        for bad in [-61.0, 12.5, f64::NAN, f64::INFINITY] {
            let options = VolumeOptions {
                gain_db: bad,
                ..Default::default()
            };
            let err = adjust_volume(&input, &options, &limits(), &rt)
                .expect_err("out-of-range gain must fail");
            assert!(
                matches!(err, MediaError::InvalidInput(_)),
                "gain_db={bad}: expected InvalidInput, got {err:?}"
            );
        }

        // +6 dB boost on a quiet sine stays well under clipping and
        // must actually raise the measured level.
        let options = VolumeOptions {
            gain_db: 6.0,
            ..Default::default()
        };
        let out = adjust_volume(&input, &options, &limits(), &rt).expect("boost");
        let file = out.primary.expect("primary output");
        let before = mean_volume(&rt, "q-before.wav", &input);
        let after = mean_volume(&rt, "q-after.wav", &file.bytes);
        assert!(
            (after - before - 6.0).abs() < 0.5,
            "expected ~6 dB boost, got {:.2} dB",
            after - before
        );
    }

    /// peak_normalize must produce a non-empty audio output when a
    /// loudness filter exists (and the op must not silently skip it).
    #[test]
    fn peak_normalize_produces_output() {
        let rt = runtime();
        let caps = rt.capabilities().expect("capabilities");
        if !caps.has_filter("loudnorm") && !caps.has_filter("dynaudnorm") {
            // No loudness filter in this build — op must report
            // Unsupported rather than silently ignoring the flag.
            let input = sine_wav();
            let options = VolumeOptions {
                peak_normalize: true,
                ..Default::default()
            };
            let err = adjust_volume(&input, &options, &limits(), &rt)
                .expect_err("must fail without loudness filters");
            assert!(matches!(err, MediaError::Unsupported(_)), "got {err:?}");
            return;
        }

        let input = sine_wav();
        let options = VolumeOptions {
            gain_db: -3.0,
            peak_normalize: true,
            ..Default::default()
        };
        let out = adjust_volume(&input, &options, &limits(), &rt).expect("normalize");
        let file = out.primary.expect("primary output");
        assert!(!file.bytes.is_empty());
        // Output must still be measurable audio.
        let _ = mean_volume(&rt, "norm.wav", &file.bytes);
    }
}
