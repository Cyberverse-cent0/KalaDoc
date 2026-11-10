//! Audio-only trim/cut — the audio counterpart of `video::trim`.
//!
//! [`trim_audio`] extracts the first audio stream (`-map 0:a:0 -vn`)
//! and cuts it to `start..end`. Two modes mirror the video op:
//!
//! - **Accurate** (default): re-encode (`aac`/`libmp3lame`/`flac`/
//!   `pcm_s16le` picked by output container) for sample-exact cuts.
//! - **Fast**: `-c copy` — snaps to packet boundaries, much faster,
//!   but the cut may include a few ms of extra audio at the edges.
//!
//! Works on audio files *and* video containers (video is dropped).

use std::ffi::OsString;

use serde::{Deserialize, Serialize};

use crate::crates::multimedia_engine::error::{MediaError, MediaResult};
use crate::crates::multimedia_engine::model::{MediaLimits, MediaOutput};
use crate::crates::multimedia_engine::runtime::discovery::Runtime;
use crate::crates::multimedia_engine::runtime::process;
use crate::crates::multimedia_engine::runtime::tempdir::TempDir;

/// Options for [`trim_audio`].
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct AudioTrimOptions {
    /// Start of the kept range in seconds (`>= 0`, finite).
    pub start: f64,
    /// End of the kept range (`> start`, finite). `None` = to EOF.
    #[serde(default)]
    pub end: Option<f64>,
    /// Frame-accurate trim via re-encode (default `true`). `false`
    /// uses `-c copy` for a fast packet-snapped cut.
    #[serde(default = "default_true")]
    pub accurate: bool,
    /// Output container extension (no dot). `None` picks a default
    /// from `codec`/input sniff.
    pub output_format: Option<String>,
    /// Audio encoder for the accurate path. `None` picks per
    /// container (`aac` for m4a/mp4, `libmp3lame` for mp3, `flac`
    /// for flac, `pcm_s16le` for wav, `libopus` for opus/ogg).
    pub codec: Option<String>,
}

fn default_true() -> bool {
    true
}

impl Default for AudioTrimOptions {
    fn default() -> Self {
        Self {
            start: 0.0,
            end: None,
            accurate: true,
            output_format: None,
            codec: None,
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

/// Pick the encoder for the accurate path.
fn default_codec_for(ext: &str) -> &'static str {
    match ext {
        "mp3" => "libmp3lame",
        "wav" => "pcm_s16le",
        "flac" => "flac",
        "ogg" | "oga" => "libvorbis",
        "opus" => "libopus",
        "aac" | "adts" => "aac",
        _ => "aac", // m4a/mp4/mov/mkv
    }
}

fn content_type_for(ext: &str) -> Option<String> {
    let mime = match ext {
        "mp3" => "audio/mpeg",
        "wav" => "audio/wav",
        "flac" => "audio/flac",
        "ogg" | "oga" | "opus" => "audio/ogg",
        "aac" | "adts" => "audio/aac",
        "m4a" | "mp4" | "mov" | "mkv" => "audio/mp4",
        _ => return None,
    };
    Some(mime.to_string())
}

fn validate_options(options: &AudioTrimOptions) -> MediaResult<()> {
    if !options.start.is_finite() || options.start < 0.0 {
        return Err(MediaError::InvalidInput(format!(
            "start must be finite and >= 0, got {}",
            options.start
        )));
    }
    if let Some(end) = options.end {
        if !end.is_finite() {
            return Err(MediaError::InvalidInput(format!(
                "end must be finite, got {end}"
            )));
        }
        if end <= options.start {
            return Err(MediaError::InvalidInput(format!(
                "end ({end}) must be greater than start ({})",
                options.start
            )));
        }
    }
    Ok(())
}

/// Trim the first audio stream of `input` to `start..end`.
///
/// # Errors
///
/// - [`MediaError::InvalidInput`] — empty input, bad time range.
/// - [`MediaError::LimitExceeded`] — input/output byte caps.
/// - [`MediaError::Unsupported`] — chosen codec missing from build.
/// - [`MediaError::ToolFailed`] — undecodable input or no audio
///   stream.
pub fn trim_audio(
    input: &[u8],
    options: &AudioTrimOptions,
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

    let out_ext = options
        .output_format
        .as_deref()
        .map(|s| s.trim().trim_start_matches('.').to_ascii_lowercase())
        .filter(|s| !s.is_empty())
        .unwrap_or_else(|| {
            if options.accurate {
                // Audio-only default: wav is the safest lossless-ish
                // choice; m4a is smaller. Pick m4a for parity with
                // the video op's mp4 default.
                "m4a".to_string()
            } else {
                sniff_extension(input).to_string()
            }
        });

    let dir = TempDir::new("atrim")?;
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
        "-ss".into(),
        format!("{:.3}", options.start).into(),
    ];
    if let Some(end) = options.end {
        argv.push("-to".into());
        argv.push(format!("{end:.3}").into());
    }
    argv.extend([
        "-i".into(),
        in_path.into_os_string(),
        "-map".into(),
        "0:a:0".into(),
        "-vn".into(),
    ]);

    let mut warnings = Vec::new();
    if options.accurate {
        let codec = options
            .codec
            .as_deref()
            .unwrap_or_else(|| default_codec_for(&out_ext));
        if !caps.has_encoder(codec) {
            return Err(MediaError::Unsupported(format!(
                "audio encoder {codec:?} is not available in this FFmpeg build"
            )));
        }
        argv.extend(["-c:a".into(), codec.into()]);
        // Reasonable bitrate defaults for lossy codecs.
        match codec {
            "aac" => argv.extend(["-b:a".into(), "128k".into()]),
            "libmp3lame" => argv.extend(["-b:a".into(), "192k".into()]),
            "libopus" => argv.extend(["-b:a".into(), "128k".into()]),
            "libvorbis" => argv.extend(["-b:a".into(), "160k".into()]),
            _ => {}
        }
    } else {
        argv.extend(["-c".into(), "copy".into()]);
        warnings.push("fast copy trim: cut snaps to the nearest audio packet boundary".to_string());
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

    let mut result = MediaOutput {
        primary: Some(crate::crates::multimedia_engine::model::MediaFile {
            name: out_name.clone(),
            bytes,
            content_type: content_type_for(&out_ext),
        }),
        extra: Vec::new(),
        warnings,
    };
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

    /// 3s 440Hz sine wav.
    fn sine_wav(secs: u32) -> Option<Vec<u8>> {
        let rt = Runtime::discover().ok()?;
        let dir = TempDir::new("atrim-test").ok()?;
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
    fn trim_audio_produces_shorter_clip() {
        let Some(input) = sine_wav(3) else {
            eprintln!("skipping: ffmpeg lavfi unavailable");
            return;
        };
        let rt = Runtime::discover().unwrap();
        let opts = AudioTrimOptions {
            start: 1.0,
            end: Some(2.0),
            accurate: true,
            ..Default::default()
        };
        let out =
            trim_audio(&input, &opts, &MediaLimits::default(), &rt).expect("trim_audio failed");
        let file = out.primary.expect("primary");
        assert!(
            file.name.ends_with(".m4a")
                || file.name.ends_with(".wav")
                || file.name.ends_with(".mp4")
        );

        let info = inspect(&file.bytes, &MediaLimits::default()).expect("probe");
        let dur = info.duration_secs.expect("duration");
        assert!((dur - 1.0).abs() < 0.4, "expected ~1s, got {dur}s");
        assert_eq!(info.video_streams, 0, "audio trim must drop video");
    }

    #[test]
    fn trim_audio_rejects_bad_range() {
        let rt = match Runtime::discover() {
            Ok(rt) => rt,
            Err(_) => return,
        };
        let input = sine_wav(1).unwrap_or_default();
        let opts = AudioTrimOptions {
            start: 2.0,
            end: Some(1.0),
            ..Default::default()
        };
        assert!(trim_audio(&input, &opts, &MediaLimits::default(), &rt).is_err());
    }
}
