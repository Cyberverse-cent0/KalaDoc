//! Parametric equalization via ffmpeg's `equalizer` filter.
//!
//! [`equalizer`] extracts the **first audio stream** of any media
//! input (audio *or* video container — video is dropped, same
//! contract as `audio::volume`) and applies one or more peaking-EQ
//! bands, each rendered as `equalizer=f=<hz>:t=<type>:w=<width>:g=<db>`
//! and chained with commas in a single `-af` argument. An optional
//! overall preamp (`options.gain_db`) is applied with the `volume`
//! filter ahead of the EQ chain.
//!
//! # Filter parameters
//!
//! ffmpeg's `equalizer` accepts `frequency` (`f`), `width_type` (`t`:
//! `q`/`hz`/`oct`), `width` (`w`) and `gain` (`g`). [`EqBand`]
//! defaults to `width_type="q"` and `width=1.0` when unspecified —
//! `w` means a Q factor under `t=q`, a bandwidth in Hz under `t=hz`,
//! and octave width under `t=oct`.
//!
//! # Clipping
//!
//! Positive band gains or a positive preamp amplify the signal;
//! values near `+12` dB will clip on already-loud material. ffmpeg
//! does not prevent this — callers should verify headroom or prefer
//! `audio::normalize` afterwards.

use std::ffi::OsString;

use serde::{Deserialize, Serialize};

use crate::crates::multimedia_engine::error::{MediaError, MediaResult};
use crate::crates::multimedia_engine::model::{MediaLimits, MediaOutput};
use crate::crates::multimedia_engine::runtime::discovery::Runtime;
use crate::crates::multimedia_engine::runtime::process;
use crate::crates::multimedia_engine::runtime::tempdir::TempDir;

/// Maximum number of EQ bands accepted per call. Keeps the `-af`
/// filtergraph short and rejects accidental "one band per sample"
/// requests.
const MAX_EQ_BANDS: usize = 16;

/// Valid band-gain / preamp range (dB). Generous — ffmpeg itself
/// accepts any finite value, but beyond ±60 dB the result is
/// effectively silence or square-wave clipping.
const MIN_GAIN_DB: f64 = -60.0;
const MAX_GAIN_DB: f64 = 60.0;

/// Valid `width` values must be positive and finite; the magnitude
/// is interpreted per `width_type` (Q factor, Hz, or octaves), so no
/// fixed upper bound is enforced.
const MIN_WIDTH: f64 = f64::EPSILON;

/// Width types accepted by ffmpeg's `equalizer` filter (`t=`).
const WIDTH_TYPES: [&str; 3] = ["q", "hz", "oct"];

/// One peaking-EQ band for [`equalizer`].
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct EqBand {
    /// Centre frequency in Hz (`equalizer=f=`). Must be finite and
    /// greater than `0`.
    pub frequency_hz: f64,
    /// Gain in dB (`equalizer=g=`). Must be finite and within
    /// `-60..=60`.
    pub gain_db: f64,
    /// Filter width type (`equalizer=t=`): `"q"`, `"hz"` or `"oct"`
    /// (case-insensitive). `None` → `"q"`.
    #[serde(default)]
    pub width_type: Option<String>,
    /// Filter width (`equalizer=w=`): Q factor for `t=q`, bandwidth
    /// in Hz for `t=hz`, octave width for `t=oct`. `None` → `1.0`
    /// (ffmpeg's default).
    #[serde(default)]
    pub width: Option<f64>,
}

impl EqBand {
    fn validate(&self) -> MediaResult<()> {
        if !self.frequency_hz.is_finite() || self.frequency_hz <= 0.0 {
            return Err(MediaError::InvalidInput(format!(
                "band frequency_hz must be finite and > 0, got {}",
                self.frequency_hz
            )));
        }
        validate_gain_db(self.gain_db, "band gain_db")?;
        if let Some(w) = self.width
            && (!w.is_finite() || w < MIN_WIDTH)
        {
            return Err(MediaError::InvalidInput(format!(
                "band width must be finite and > 0, got {w}"
            )));
        }
        if let Some(t) = self.width_type.as_deref()
            && !WIDTH_TYPES.contains(&t.trim().to_ascii_lowercase().as_str())
        {
            return Err(MediaError::InvalidInput(format!(
                "band width_type must be one of {WIDTH_TYPES:?}, got {t:?}"
            )));
        }
        Ok(())
    }

    /// Render as an `equalizer` filter fragment:
    /// `equalizer=f=<hz>:t=<type>:w=<width>:g=<db>`.
    fn to_filter(&self) -> String {
        let t = self
            .width_type
            .as_deref()
            .map(|s| s.trim().to_ascii_lowercase())
            .filter(|s| !s.is_empty())
            .unwrap_or_else(|| "q".to_string());
        let w = self.width.unwrap_or(1.0);
        format!(
            "equalizer=f={}:t={}:w={}:g={}",
            self.frequency_hz, t, w, self.gain_db
        )
    }
}

/// Options for [`equalizer`].
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct EqOptions {
    /// Output container extension / format name without a leading
    /// dot (e.g. `"wav"`, `"mp3"`, `"m4a"`). `None` → `"wav"` (PCM,
    /// lossless — the least surprising default for an EQ op).
    pub output_format: Option<String>,
    /// Audio encoder name (`ffmpeg -encoders` token, e.g. `"aac"`,
    /// `"pcm_s16le"`). `None` lets the muxer choose (PCM for `wav`,
    /// the container default otherwise). Capability-checked when
    /// set.
    pub codec: Option<String>,
    /// Audio bitrate string (e.g. `"128k"`). Passed as `-b:a`; only
    /// meaningful for lossy codecs.
    pub bitrate: Option<String>,
    /// Overall preamp gain in dB applied via `volume=<g>dB` **before**
    /// the EQ chain (so bands see the boosted signal). `0.0` (the
    /// default) adds no filter. Must be finite and within
    /// `-60..=60`.
    #[serde(default)]
    pub gain_db: f64,
}

/// Apply parametric EQ bands to the first audio stream of `input`.
///
/// Runs `ffmpeg -hide_banner -loglevel error -nostdin -y -i <in>
/// -map 0:a:0 -vn -af <volume?>,<equalizer>,<equalizer>,...
/// [-c:a codec] [-b:a bitrate] output.<format>` and reads the
/// audio-only output back under `limits`. Works on video inputs too
/// — video is discarded.
///
/// # Errors
///
/// - [`MediaError::InvalidInput`] — empty input, no bands, too many
///   bands, non-finite or out-of-range band/option values, empty
///   `output_format`, unknown `width_type`.
/// - [`MediaError::LimitExceeded`] — `max_input_bytes` /
///   `max_output_bytes` exceeded.
/// - [`MediaError::Unsupported`] — `equalizer`/`volume` filter, the
///   requested encoder, or the output muxer is missing from this
///   FFmpeg build.
/// - [`MediaError::ToolFailed`] / [`MediaError::Timeout`] — ffmpeg
///   rejected the input (including inputs with no audio stream) or
///   the options.
pub fn equalizer(
    input: &[u8],
    bands: &[EqBand],
    options: &EqOptions,
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
    if bands.is_empty() {
        return Err(MediaError::InvalidInput(
            "at least one EQ band is required".to_string(),
        ));
    }
    if bands.len() > MAX_EQ_BANDS {
        return Err(MediaError::InvalidInput(format!(
            "too many EQ bands ({}), maximum is {MAX_EQ_BANDS}",
            bands.len()
        )));
    }
    for band in bands {
        band.validate()?;
    }
    validate_gain_db(options.gain_db, "preamp gain_db")?;
    if let Some(fmt) = options.output_format.as_deref()
        && fmt.trim().is_empty()
    {
        return Err(MediaError::InvalidInput(
            "output_format must not be empty".to_string(),
        ));
    }

    // Honest capabilities: requested encoder/muxer/filters must exist
    // in this build. `volume` is only needed when the preamp is
    // non-zero.
    let caps = rt.capabilities()?;
    if !caps.has_filter("equalizer") {
        return Err(MediaError::Unsupported(
            "filter 'equalizer' is not available in this FFmpeg build".to_string(),
        ));
    }
    if options.gain_db != 0.0 && !caps.has_filter("volume") {
        return Err(MediaError::Unsupported(
            "filter 'volume' is not available in this FFmpeg build".to_string(),
        ));
    }
    if let Some(codec) = options.codec.as_deref()
        && !caps.has_encoder(codec)
    {
        return Err(MediaError::Unsupported(format!(
            "audio encoder '{codec}' is not available in this FFmpeg build"
        )));
    }

    let output_ext = match options.output_format.as_deref() {
        Some(fmt) => fmt.trim().trim_start_matches('.').to_ascii_lowercase(),
        None => "wav".to_string(),
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

    // Build the -af chain: optional preamp first, then all EQ bands
    // comma-separated in one filtergraph.
    let mut filters: Vec<String> = Vec::with_capacity(bands.len() + 1);
    if options.gain_db != 0.0 {
        filters.push(format!("volume={}dB", options.gain_db));
    }
    for band in bands {
        filters.push(band.to_filter());
    }

    let dir = TempDir::new("equalizer")?;
    // ffmpeg probes by content; a neutral extension avoids misleading
    // the demuxer when callers pass raw elementary streams.
    dir.write("input.bin", input)?;

    let output_name = format!("output.{output_ext}");
    let output_path = dir.file(&output_name);

    let mut argv: Vec<OsString> = vec![
        OsString::from("-hide_banner"),
        OsString::from("-loglevel"),
        OsString::from("error"),
        OsString::from("-nostdin"),
        OsString::from("-y"),
        OsString::from("-i"),
        dir.file("input.bin").into_os_string(),
        // First audio stream only; -vn is belt-and-braces since the
        // map already excludes video, but makes intent explicit.
        OsString::from("-map"),
        OsString::from("0:a:0"),
        OsString::from("-vn"),
        OsString::from("-af"),
        OsString::from(filters.join(",")),
    ];
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

/// Validate a dB gain value: finite and within `-60..=60`.
fn validate_gain_db(value: f64, what: &str) -> MediaResult<()> {
    if !value.is_finite() || !(MIN_GAIN_DB..=MAX_GAIN_DB).contains(&value) {
        return Err(MediaError::InvalidInput(format!(
            "{what} must be finite and within {MIN_GAIN_DB}..={MAX_GAIN_DB}, got {value}"
        )));
    }
    Ok(())
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
    use crate::crates::multimedia_engine::runtime::process;
    use std::ffi::OsString;

    fn runtime() -> Option<Runtime> {
        Runtime::discover().ok()
    }

    fn limits() -> MediaLimits {
        MediaLimits::default()
    }

    /// Generate a 1-second 440 Hz sine WAV via lavfi. Returns `None`
    /// when ffmpeg/lavfi is unavailable so tests skip gracefully.
    fn sine_wav() -> Option<Vec<u8>> {
        let rt = runtime()?;
        let dir = TempDir::new("equalizer-test-in").ok()?;
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
            out_path.to_str()?,
        ]);
        let out = process::run("ffmpeg", &rt.ffmpeg, &argv, None, &limits()).ok()?;
        if !out.success {
            return None;
        }
        std::fs::read(&out_path).ok()
    }

    /// Probe `format=duration` (seconds) of an audio buffer.
    fn duration_secs(rt: &Runtime, name: &str, bytes: &[u8]) -> Option<f64> {
        let dir = TempDir::new("equalizer-test-probe").ok()?;
        let path = dir.write(name, bytes).ok()?;
        let argv: Vec<OsString> = [
            OsString::from("-v"),
            OsString::from("error"),
            OsString::from("-show_entries"),
            OsString::from("format=duration"),
            OsString::from("-of"),
            OsString::from("csv=p=0"),
            path.into_os_string(),
        ]
        .into_iter()
        .collect();
        let out = process::run_checked("ffprobe", &rt.ffprobe, &argv, None, &limits()).ok()?;
        String::from_utf8_lossy(&out.stdout)
            .trim()
            .parse::<f64>()
            .ok()
    }

    #[test]
    fn band_filter_string_format() {
        let band = EqBand {
            frequency_hz: 1000.0,
            gain_db: -6.0,
            width_type: Some("q".to_string()),
            width: Some(2.0),
        };
        assert_eq!(band.to_filter(), "equalizer=f=1000:t=q:w=2:g=-6");
        let band_default = EqBand {
            frequency_hz: 250.0,
            gain_db: 3.5,
            width_type: None,
            width: None,
        };
        assert_eq!(band_default.to_filter(), "equalizer=f=250:t=q:w=1:g=3.5");
        let band_hz = EqBand {
            frequency_hz: 500.0,
            gain_db: 1.0,
            width_type: Some("HZ".to_string()),
            width: Some(200.0),
        };
        assert_eq!(band_hz.to_filter(), "equalizer=f=500:t=hz:w=200:g=1");
    }

    #[test]
    fn rejects_invalid_inputs_and_bands() {
        let Some(rt) = runtime() else {
            eprintln!("skipping: ffmpeg unavailable");
            return;
        };
        let Some(input) = sine_wav() else {
            eprintln!("skipping: ffmpeg lavfi unavailable");
            return;
        };
        let band = EqBand {
            frequency_hz: 440.0,
            gain_db: 3.0,
            width_type: None,
            width: None,
        };
        let opts = EqOptions::default();

        // Empty input.
        let err = equalizer(&[], &[band.clone()], &opts, &limits(), &rt)
            .expect_err("empty input must fail");
        assert!(matches!(err, MediaError::InvalidInput(_)), "got {err:?}");

        // No bands.
        let err = equalizer(&input, &[], &opts, &limits(), &rt).expect_err("no bands must fail");
        assert!(matches!(err, MediaError::InvalidInput(_)), "got {err:?}");

        // Bad bands.
        for bad in [
            EqBand {
                frequency_hz: 0.0,
                gain_db: 0.0,
                width_type: None,
                width: None,
            },
            EqBand {
                frequency_hz: f64::NAN,
                gain_db: 0.0,
                width_type: None,
                width: None,
            },
            EqBand {
                frequency_hz: 440.0,
                gain_db: 100.0,
                width_type: None,
                width: None,
            },
            EqBand {
                frequency_hz: 440.0,
                gain_db: 0.0,
                width_type: Some("bogus".to_string()),
                width: None,
            },
            EqBand {
                frequency_hz: 440.0,
                gain_db: 0.0,
                width_type: None,
                width: Some(-1.0),
            },
        ] {
            let err =
                equalizer(&input, &[bad], &opts, &limits(), &rt).expect_err("bad band must fail");
            assert!(matches!(err, MediaError::InvalidInput(_)), "got {err:?}");
        }

        // Bad preamp.
        let opts_bad = EqOptions {
            gain_db: f64::INFINITY,
            ..Default::default()
        };
        let err = equalizer(&input, &[band], &opts_bad, &limits(), &rt)
            .expect_err("bad preamp must fail");
        assert!(matches!(err, MediaError::InvalidInput(_)), "got {err:?}");
    }

    /// Primary check: a +12 dB boost at the 440 Hz sine frequency must
    /// produce non-empty audio whose duration matches the input.
    #[test]
    fn equalizer_boosts_and_preserves_duration() {
        let (Some(input), Some(rt)) = (sine_wav(), runtime()) else {
            eprintln!("skipping: ffmpeg lavfi unavailable");
            return;
        };
        let caps = match rt.capabilities() {
            Ok(c) => c,
            Err(_) => return,
        };
        if !caps.has_filter("equalizer") {
            eprintln!("skipping: equalizer filter unavailable");
            return;
        }

        let bands = [EqBand {
            frequency_hz: 440.0,
            gain_db: 12.0,
            width_type: Some("q".to_string()),
            width: Some(1.0),
        }];
        let options = EqOptions {
            gain_db: -3.0, // preamp exercises the volume chain too
            ..Default::default()
        };
        if options.gain_db != 0.0 && !caps.has_filter("volume") {
            eprintln!("skipping: volume filter unavailable");
            return;
        }

        let out = equalizer(&input, &bands, &options, &limits(), &rt).expect("equalizer");
        let file = out.primary.expect("primary output");
        assert_eq!(file.name, "output.wav");
        assert_eq!(file.content_type.as_deref(), Some("audio/wav"));
        assert!(!file.bytes.is_empty());

        let (Some(d_in), Some(d_out)) = (
            duration_secs(&rt, "in.wav", &input),
            duration_secs(&rt, "out.wav", &file.bytes),
        ) else {
            eprintln!("skipping: ffprobe duration unavailable");
            return;
        };
        assert!(
            (d_out - d_in).abs() < 0.1,
            "duration changed: in={d_in:.3}s out={d_out:.3}s"
        );
    }
}
