//! Silence detection via ffmpeg's `silencedetect` audio filter.
//!
//! `silencedetect` decodes the audio stream, measures per-frame
//! levels, and reports `silence_start` / `silence_end` /
//! `silence_duration` lines on **stderr** while output goes to the
//! `null` muxer — same pattern as `blackdetect`/`freezedetect`.
//!
//! # Options mapping
//!
//! `silencedetect=n=<noise_db>dB:d=<min_duration>`: a frame is
//! silent when its level stays below `noise_db` (default `-50`) for
//! at least `min_duration` seconds (default `2.0`). `mono=1`
//! collapses channels to mono before measuring, which matches what
//! most users expect from "silence".

use serde::{Deserialize, Serialize};

use crate::crates::multimedia_engine::error::{MediaError, MediaResult};
use crate::crates::multimedia_engine::model::MediaLimits;
use crate::crates::multimedia_engine::runtime::discovery::Runtime;
use crate::crates::multimedia_engine::runtime::process;
use crate::crates::multimedia_engine::runtime::tempdir::TempDir;

/// Options for [`silence_detect`].
#[derive(Debug, Clone, Copy, PartialEq, Serialize, Deserialize)]
pub struct SilenceOptions {
    /// Noise floor in dB: frames quieter than this count as silence
    /// (`n=`). Default `-50.0`.
    pub noise_db: f64,
    /// Minimum silence duration in seconds (`d=`). Default `2.0`.
    pub min_duration_secs: f64,
    /// Measure on a mono downmix (`mono=1`) instead of per-channel.
    /// Default `true`.
    #[serde(default = "default_true")]
    pub mono: bool,
}

fn default_true() -> bool {
    true
}

impl Default for SilenceOptions {
    fn default() -> Self {
        Self {
            noise_db: -50.0,
            min_duration_secs: 2.0,
            mono: true,
        }
    }
}

/// One detected silent interval.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct SilenceEvent {
    /// Interval start in seconds.
    pub start_secs: f64,
    /// Interval end in seconds. `None` when silence runs to EOF.
    pub end_secs: Option<f64>,
    /// Reported duration in seconds when ffmpeg emitted one
    /// (`silence_duration`); equals `end - start` for closed
    /// intervals.
    pub duration_secs: Option<f64>,
}

/// Result of a silence-detection run.
#[derive(Debug, Clone, Default)]
pub struct SilenceReport {
    /// Detected silent intervals in presentation order.
    pub events: Vec<SilenceEvent>,
}

fn check_finite(name: &str, value: f64) -> MediaResult<()> {
    if !value.is_finite() {
        return Err(MediaError::InvalidInput(format!(
            "{name} must be finite, got {value}"
        )));
    }
    Ok(())
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

fn value_after(line: &str, key: &str) -> Option<f64> {
    let pos = line.find(key)?;
    let rest = &line[pos + key.len()..];
    let rest = rest.trim_start_matches([':', ' ']);
    let end = rest
        .find(|c: char| {
            !(c.is_ascii_digit() || c == '.' || c == '-' || c == '+' || c == 'e' || c == 'E')
        })
        .unwrap_or(rest.len());
    rest[..end].parse().ok()
}

/// Detect silent intervals in `input`.
///
/// Runs `ffmpeg -i input -af "silencedetect=n=..:d=..:mono=1" -vn
/// -f null -` and parses the `silence_start`/`silence_end` lines on
/// stderr. Inputs with no audio stream fail via ffmpeg.
///
/// # Errors
///
/// - [`MediaError::LimitExceeded`] when `input` exceeds
///   `limits.max_input_bytes`.
/// - [`MediaError::Unsupported`] when `silencedetect` is not in this
///   FFmpeg build.
/// - [`MediaError::InvalidInput`] for non-finite options or
///   undecodable input.
/// - [`MediaError::ToolFailed`] when ffmpeg cannot process the input.
pub fn silence_detect(
    input: &[u8],
    options: &SilenceOptions,
    limits: &MediaLimits,
    rt: &Runtime,
) -> MediaResult<SilenceReport> {
    check_finite("noise_db", options.noise_db)?;
    check_finite("min_duration_secs", options.min_duration_secs)?;

    let caps = rt.capabilities()?;
    if !caps.has_filter("silencedetect") {
        return Err(MediaError::Unsupported(
            "filter \"silencedetect\" is not available in this FFmpeg build".to_string(),
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

    let dir = TempDir::new("silence")?;
    let in_name = format!("input.{}", sniff_extension(input));
    let in_path = dir.write(&in_name, input)?;

    let filter = format!(
        "silencedetect=n={}:d:{}:mono={}",
        options.noise_db,
        options.min_duration_secs,
        u8::from(options.mono)
    );
    let argv = process::args(&[
        "-hide_banner",
        "-nostdin",
        "-i",
        in_path.to_str().unwrap_or("input"),
        "-af",
        &filter,
        "-vn",
        "-f",
        "null",
        "-",
    ]);
    // Detection diagnostics are emitted at info level — keep default
    // loglevel (do not pass `-loglevel error`).
    let out = process::run_checked("ffmpeg", &rt.ffmpeg, &argv, None, limits)?;

    let mut events = Vec::new();
    let mut pending: Option<f64> = None;
    for line in out.stderr.lines() {
        if !line.contains("silence") {
            continue;
        }
        if let Some(start) = value_after(line, "silence_start:") {
            if let Some(prev) = pending.take() {
                events.push(SilenceEvent {
                    start_secs: prev,
                    end_secs: None,
                    duration_secs: None,
                });
            }
            pending = Some(start);
        }
        if let Some(end) = value_after(line, "silence_end:")
            && let Some(start) = pending.take()
        {
            let duration = value_after(line, "silence_duration:");
            events.push(SilenceEvent {
                start_secs: start,
                end_secs: Some(end),
                duration_secs: duration,
            });
        }
    }
    if let Some(start) = pending {
        events.push(SilenceEvent {
            start_secs: start,
            end_secs: None,
            duration_secs: None,
        });
    }

    Ok(SilenceReport { events })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::crates::multimedia_engine::runtime::process;

    /// 3s clip: 1s tone, 1.5s digital silence, 0.5s tone.
    fn tone_silence_tone() -> Option<Vec<u8>> {
        let rt = Runtime::discover().ok()?;
        let dir = TempDir::new("sil-test").ok()?;
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
            "sine=frequency=440:duration=1",
            "-f",
            "lavfi",
            "-i",
            "anullsrc=r=44100:cl=mono:d=1.5",
            "-f",
            "lavfi",
            "-i",
            "sine=frequency=440:duration=0.5",
            "-filter_complex",
            "[0:a][1:a][2:a]concat=n=3:v=0:a=1[a]",
            "-map",
            "[a]",
            "-c:a",
            "pcm_s16le",
            out.to_str()?,
        ]);
        let out_res =
            process::run("ffmpeg", &rt.ffmpeg, &args, None, &MediaLimits::default()).ok()?;
        if !out_res.success {
            return None;
        }
        std::fs::read(&out).ok()
    }

    #[test]
    fn silence_detect_finds_middle_gap() {
        let Some(input) = tone_silence_tone() else {
            eprintln!("skipping: ffmpeg lavfi unavailable");
            return;
        };
        let rt = Runtime::discover().unwrap();
        let report = silence_detect(
            &input,
            &SilenceOptions {
                min_duration_secs: 0.5,
                ..Default::default()
            },
            &MediaLimits::default(),
            &rt,
        )
        .expect("silence_detect failed");
        assert!(
            report
                .events
                .iter()
                .any(|e| e.start_secs >= 0.8 && e.start_secs <= 1.3),
            "expected silence near t=1s, got {:?}",
            report.events
        );
    }

    #[test]
    fn silence_detect_rejects_bad_options() {
        let rt = match Runtime::discover() {
            Ok(rt) => rt,
            Err(_) => return,
        };
        let input = b"junk".to_vec();
        let opts = SilenceOptions {
            noise_db: f64::NAN,
            ..Default::default()
        };
        assert!(silence_detect(&input, &opts, &MediaLimits::default(), &rt).is_err());
    }
}
