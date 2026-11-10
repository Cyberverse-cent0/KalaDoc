//! Arbitrary channel remixing via ffmpeg's `pan` filter.
//!
//! [`remix_channels`] extracts the first audio stream, probes its
//! channel count with ffprobe, and applies a `pan` filter whose
//! output matrix is caller-supplied:
//!
//! ```text
//! pan=<output_layout>|c0=<g00>*c0+<g01>*c1+...|c1=...
//! ```
//!
//! `matrix` is indexed `[output_channel][input_channel]`; each row
//! must have exactly `input_channels` entries and there must be one
//! row per channel of `output_layout`. Input channels are addressed
//! by index (`c0..cN`), which `pan` accepts for any input layout.
//!
//! Output is audio-only — same contract as `audio::volume`.

use std::ffi::OsString;

use serde::{Deserialize, Serialize};

use crate::crates::multimedia_engine::error::{MediaError, MediaResult};
use crate::crates::multimedia_engine::model::{MediaLimits, MediaOutput};
use crate::crates::multimedia_engine::runtime::discovery::Runtime;
use crate::crates::multimedia_engine::runtime::process;
use crate::crates::multimedia_engine::runtime::tempdir::TempDir;

/// Maximum supported channel count (input or output).
const MAX_CHANNELS: usize = 18;

/// A channel-remix matrix: `matrix[out_ch][in_ch]` gain coefficients.
///
/// Construct directly or via [`ChannelRemixMatrix::mono`] /
/// [`ChannelRemixMatrix::stereo`] for common down/up-mix shapes.
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
pub struct ChannelRemixMatrix {
    /// Gain matrix. `matrix[i][j]` is the contribution of input
    /// channel `j` to output channel `i`. All rows must have equal
    /// length; that length is the input channel count.
    pub matrix: Vec<Vec<f64>>,
}

impl ChannelRemixMatrix {
    /// Equal-power downmix to mono: every input channel contributes
    /// `1/n` to the single output.
    pub fn mono(input_channels: usize) -> Self {
        Self {
            matrix: vec![vec![1.0 / input_channels.max(1) as f64; input_channels]],
        }
    }

    /// Two-channel down/up-mix: output 0 sums the "left-ish" half of
    /// inputs, output 1 sums the "right-ish" half. For stereo input
    /// this yields an identity matrix; for mono both outputs equal the
    /// input; for wider layouts channels are split evenly.
    pub fn stereo(input_channels: usize) -> Self {
        let n = input_channels.max(1);
        let half = n.div_ceil(2);
        let left: Vec<f64> = (0..n).map(|j| if j < half { 1.0 } else { 0.0 }).collect();
        let right: Vec<f64> = (0..n).map(|j| if j >= half { 1.0 } else { 0.0 }).collect();
        Self {
            matrix: vec![left, right],
        }
    }

    /// Number of output channels (matrix rows).
    pub fn output_channels(&self) -> usize {
        self.matrix.len()
    }

    /// Number of input channels (columns); `None` for an empty matrix.
    pub fn input_channels(&self) -> Option<usize> {
        self.matrix.first().map(|r| r.len())
    }

    fn validate(&self) -> MediaResult<()> {
        if self.matrix.is_empty() {
            return Err(MediaError::InvalidInput(
                "remix matrix must have at least one row".to_string(),
            ));
        }
        if self.matrix.len() > MAX_CHANNELS {
            return Err(MediaError::InvalidInput(format!(
                "remix matrix has {} output channels, maximum is {MAX_CHANNELS}",
                self.matrix.len()
            )));
        }
        let cols = self.matrix[0].len();
        if cols == 0 || cols > MAX_CHANNELS {
            return Err(MediaError::InvalidInput(format!(
                "remix matrix input channel count must be in 1..={MAX_CHANNELS}, got {cols}"
            )));
        }
        for (i, row) in self.matrix.iter().enumerate() {
            if row.len() != cols {
                return Err(MediaError::InvalidInput(format!(
                    "remix matrix row {i} has {} entries, expected {cols}",
                    row.len()
                )));
            }
            for (j, g) in row.iter().enumerate() {
                if !g.is_finite() {
                    return Err(MediaError::InvalidInput(format!(
                        "remix matrix[{i}][{j}] is not finite"
                    )));
                }
            }
        }
        Ok(())
    }
}

/// Options for [`remix_channels`].
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct ChannelRemixOptions {
    /// FFmpeg output channel-layout name (`"mono"`, `"stereo"`,
    /// `"quad"`, `"5.1"`, `"7.1"`, ...). Its channel count must equal
    /// the number of matrix rows.
    pub output_layout: String,
    /// Gain matrix (`[out][in]`). Columns must equal the probed input
    /// channel count.
    pub matrix: ChannelRemixMatrix,
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

/// Remix the channels of the first audio stream of `input`.
///
/// The input's channel count is probed with ffprobe (first audio
/// stream); the `pan` filter is then built from `options.matrix`.
///
/// # Errors
///
/// - [`MediaError::InvalidInput`] — empty/oversized input, unknown
///   `output_layout`, matrix shape/count mismatch, non-finite gains,
///   empty `output_format`.
/// - [`MediaError::LimitExceeded`] — byte caps.
/// - [`MediaError::Unsupported`] — `pan` filter, chosen encoder or
///   muxer missing.
/// - [`MediaError::ToolFailed`] — undecodable input / no audio.
pub fn remix_channels(
    input: &[u8],
    options: &ChannelRemixOptions,
    limits: &MediaLimits,
    rt: &Runtime,
) -> MediaResult<MediaOutput> {
    check_size(input, limits)?;
    if input.is_empty() {
        return Err(MediaError::InvalidInput("empty input buffer".to_string()));
    }
    options.matrix.validate()?;

    let output_layout = options.output_layout.trim();
    if output_layout.is_empty() {
        return Err(MediaError::InvalidInput(
            "output_layout must not be empty".to_string(),
        ));
    }
    let out_channels = channel_count_for_layout(output_layout).ok_or_else(|| {
        MediaError::InvalidInput(format!(
            "cannot determine channel count for layout '{output_layout}'"
        ))
    })? as usize;
    if options.matrix.output_channels() != out_channels {
        return Err(MediaError::InvalidInput(format!(
            "matrix has {} output rows but layout '{output_layout}' has {out_channels} channels",
            options.matrix.output_channels()
        )));
    }

    let caps = rt.capabilities()?;
    if !caps.has_filter("pan") {
        return Err(MediaError::Unsupported(
            "filter \"pan\" is not available in this FFmpeg build".to_string(),
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

    let dir = TempDir::new("channel-remix")?;
    let in_ext = sniff_extension(input);
    let in_name = format!("input.{in_ext}");
    let in_path = dir.write(&in_name, input)?;

    // Probe the input's channel count (first audio stream).
    let in_channels = probe_channel_count(&in_path, limits, rt)?;
    if in_channels == 0 || in_channels > MAX_CHANNELS {
        return Err(MediaError::InvalidInput(format!(
            "input has {in_channels} audio channels, expected 1..={MAX_CHANNELS}"
        )));
    }
    if options.matrix.input_channels() != Some(in_channels) {
        return Err(MediaError::InvalidInput(format!(
            "matrix has {} input columns but input has {in_channels} channels",
            options.matrix.input_channels().unwrap_or(0)
        )));
    }

    let af = build_pan_filter(output_layout, options.matrix.matrix.as_slice());

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
            detail: "channel remix produced a zero-byte file".to_string(),
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

/// Build the `pan=<layout>|c0=...|c1=...` filter argument.
///
/// Output channels are named `c0..cN` (accepted by `pan` for every
/// standard layout). Each expression sums `gain*c<input>` terms;
/// all-zero rows become `0*c0` (a bare `0` is rejected by `pan`).
fn build_pan_filter(output_layout: &str, matrix: &[Vec<f64>]) -> String {
    let mut s = format!("pan={output_layout}");
    for (out_ch, row) in matrix.iter().enumerate() {
        let terms: Vec<String> = row
            .iter()
            .enumerate()
            .filter(|(_, g)| **g != 0.0)
            .map(|(in_ch, g)| format!("{g}*c{in_ch}"))
            .collect();
        let expr = if terms.is_empty() {
            "0*c0".to_string()
        } else {
            terms.join("+")
        };
        s.push_str(&format!("|c{out_ch}={expr}"));
    }
    s
}

/// Derive the channel count for a standard FFmpeg layout name
/// (same rules as `audio::channels`): named layouts plus the
/// `N.M[.K]` numeric convention where components add.
fn channel_count_for_layout(layout: &str) -> Option<u32> {
    let base = layout
        .split('(')
        .next()
        .unwrap_or(layout)
        .trim()
        .to_ascii_lowercase();
    match base.as_str() {
        "mono" => return Some(1),
        "stereo" | "downmix" => return Some(2),
        "quad" => return Some(4),
        "hexagonal" => return Some(6),
        "octagonal" => return Some(8),
        _ => {}
    }
    let mut count: u32 = 0;
    let mut parts = 0u32;
    for part in base.split('.') {
        let n: u32 = part.parse().ok()?;
        count = count.checked_add(n)?;
        parts += 1;
    }
    (parts > 0).then_some(count)
}

/// Probe the channel count of the first audio stream via ffprobe.
fn probe_channel_count(
    path: &std::path::Path,
    limits: &MediaLimits,
    rt: &Runtime,
) -> MediaResult<usize> {
    let args = process::args(&[
        "-v",
        "error",
        "-select_streams",
        "a:0",
        "-show_entries",
        "stream=channels",
        "-of",
        "json",
        path.to_str().unwrap_or("input"),
    ]);
    let out = process::run("ffprobe", &rt.ffprobe, &args, None, limits)?;
    if !out.success {
        return Err(MediaError::ToolFailed {
            tool: "ffprobe",
            status: "non-zero exit".to_string(),
            detail: out.stderr.clone(),
        });
    }
    #[derive(serde::Deserialize)]
    struct Streams {
        streams: Option<Vec<serde_json::Value>>,
    }
    let parsed: Streams =
        serde_json::from_slice(&out.stdout).map_err(|e| MediaError::ToolFailed {
            tool: "ffprobe",
            status: "bad json".to_string(),
            detail: e.to_string(),
        })?;
    let channels = parsed
        .streams
        .and_then(|s| s.into_iter().next())
        .and_then(|s| s.get("channels").and_then(|c| c.as_u64()))
        .unwrap_or(0) as usize;
    Ok(channels)
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

    /// Generate a stereo sine WAV via lavfi.
    fn stereo_wav() -> Option<Vec<u8>> {
        let rt = runtime()?;
        let dir = TempDir::new("remix-test-in").ok()?;
        let out_path = dir.file("stereo.wav");
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
    fn pan_filter_string_format() {
        // Stereo → mono average.
        let m = ChannelRemixMatrix::mono(2);
        assert_eq!(
            build_pan_filter("mono", &m.matrix),
            "pan=mono|c0=0.5*c0+0.5*c1"
        );
        // Stereo identity.
        let id = ChannelRemixMatrix {
            matrix: vec![vec![1.0, 0.0], vec![0.0, 1.0]],
        };
        assert_eq!(
            build_pan_filter("stereo", &id.matrix),
            "pan=stereo|c0=1*c0|c1=1*c1"
        );
        // All-zero row → 0*c0 (bare `0` is rejected by pan).
        let z = ChannelRemixMatrix {
            matrix: vec![vec![0.0, 0.0], vec![1.0, 0.0]],
        };
        assert_eq!(
            build_pan_filter("stereo", &z.matrix),
            "pan=stereo|c0=0*c0|c1=1*c0"
        );
    }

    #[test]
    fn matrix_validation() {
        assert!(ChannelRemixMatrix::mono(2).validate().is_ok());
        // Ragged rows rejected.
        let bad = ChannelRemixMatrix {
            matrix: vec![vec![1.0, 0.0], vec![1.0]],
        };
        assert!(bad.validate().is_err());
        // NaN gain rejected.
        let nan = ChannelRemixMatrix {
            matrix: vec![vec![f64::NAN]],
        };
        assert!(nan.validate().is_err());
        // Empty rejected.
        let empty = ChannelRemixMatrix { matrix: vec![] };
        assert!(empty.validate().is_err());
    }

    #[test]
    fn remix_stereo_to_mono() {
        let (Some(input), Some(rt)) = (stereo_wav(), runtime()) else {
            eprintln!("skipping: ffmpeg lavfi unavailable");
            return;
        };
        let caps = match rt.capabilities() {
            Ok(c) => c,
            Err(_) => return,
        };
        if !caps.has_filter("pan") || !caps.has_encoder("pcm_s16le") {
            eprintln!("skipping: pan/pcm_s16le unavailable");
            return;
        }
        let opts = ChannelRemixOptions {
            output_layout: "mono".to_string(),
            matrix: ChannelRemixMatrix::mono(2),
            ..Default::default()
        };
        let out = remix_channels(&input, &opts, &limits(), &rt).expect("remix");
        let file = out.primary.expect("primary");
        assert!(file.name.ends_with(".wav"));
        assert!(!file.bytes.is_empty());

        // Verify 1 channel via ffprobe.
        let dir = TempDir::new("remix-test-probe").expect("tempdir");
        let path = dir.write("out.wav", &file.bytes).expect("write");
        let argv = process::args(&[
            "-v",
            "error",
            "-select_streams",
            "a:0",
            "-show_entries",
            "stream=channels",
            "-of",
            "csv=p=0",
            path.to_str().expect("utf8"),
        ]);
        let probe = process::run("ffprobe", &rt.ffprobe, &argv, None, &limits()).expect("ffprobe");
        assert!(probe.success);
        assert_eq!(String::from_utf8_lossy(&probe.stdout).trim(), "1");
    }

    #[test]
    fn remix_rejects_layout_matrix_mismatch() {
        let Some(rt) = runtime() else {
            eprintln!("skipping: ffmpeg unavailable");
            return;
        };
        let input = stereo_wav().unwrap_or_default();
        let opts = ChannelRemixOptions {
            output_layout: "stereo".to_string(),
            // Only one row for a 2-channel layout.
            matrix: ChannelRemixMatrix::mono(2),
            ..Default::default()
        };
        let err = remix_channels(&input, &opts, &limits(), &rt).expect_err("must fail");
        assert!(matches!(err, MediaError::InvalidInput(_)));
    }
}
