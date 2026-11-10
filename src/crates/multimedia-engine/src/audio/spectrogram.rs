//! Audio spectrogram image generation via ffmpeg's
//! `showspectrumpic` filter.
//!
//! [`render_spectrogram`] takes the first audio stream of `input`,
//! renders a single-frame spectrum video through
//! `showspectrumpic=s=<WxH>` inside a `-filter_complex` graph, and
//! encodes it to PNG. The output is an image, not audio — the
//! `MediaOutput.primary` file is `output.png` with
//! `content_type = "image/png"`.
//!
//! # Honest capability gating
//!
//! `showspectrumpic` produces a *video* stream, so the op requires
//! the filter **and** the `png` encoder in this FFmpeg build;
//! otherwise [`MediaError::Unsupported`] is returned — never a silent
//! skip.

use std::ffi::OsString;

use serde::{Deserialize, Serialize};

use crate::crates::multimedia_engine::error::{MediaError, MediaResult};
use crate::crates::multimedia_engine::model::{MediaLimits, MediaOutput};
use crate::crates::multimedia_engine::runtime::discovery::Runtime;
use crate::crates::multimedia_engine::runtime::process;
use crate::crates::multimedia_engine::runtime::tempdir::TempDir;

/// Minimum/maximum spectrogram dimensions (pixels).
const MIN_DIM: u32 = 16;
const MAX_DIM: u32 = 8192;

/// Channel coloring preset for `showspectrumpic=color=`.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
pub enum SpectrogramColor {
    /// Separate color per channel.
    Channel,
    /// Intensity-based coloring. Default.
    #[default]
    Intensity,
    /// Rainbow.
    Rainbow,
    /// Moreland.
    Moreland,
    /// Nebulae.
    Nebulae,
    /// Fire.
    Fire,
    /// Fiery.
    Fiery,
    /// Fruit.
    Fruit,
    /// Cool.
    Cool,
    /// Magma.
    Magma,
    /// Green.
    Green,
    /// Viridis.
    Viridis,
    /// Plasma.
    Plasma,
    /// Cividis.
    Cividis,
    /// Terrain.
    Terrain,
}

impl SpectrogramColor {
    fn ffmpeg_index(self) -> u8 {
        match self {
            Self::Channel => 0,
            Self::Intensity => 1,
            Self::Rainbow => 2,
            Self::Moreland => 3,
            Self::Nebulae => 4,
            Self::Fire => 5,
            Self::Fiery => 6,
            Self::Fruit => 7,
            Self::Cool => 8,
            Self::Magma => 9,
            Self::Green => 10,
            Self::Viridis => 11,
            Self::Plasma => 12,
            Self::Cividis => 13,
            Self::Terrain => 14,
        }
    }
}

/// Frequency-axis scale for `showspectrumpic=fscale=`.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
pub enum SpectrogramFreqScale {
    /// Linear frequency axis.
    Linear,
    /// Logarithmic frequency axis. Default.
    #[default]
    Log,
}

impl SpectrogramFreqScale {
    fn ffmpeg_index(self) -> u8 {
        match self {
            Self::Linear => 0,
            Self::Log => 1,
        }
    }
}

/// Display scale for `showspectrumpic=scale=`.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
pub enum SpectrogramScale {
    /// Linear.
    Linear,
    /// Square root.
    Sqrt,
    /// Cubic root.
    Cbrt,
    /// Logarithmic. Default.
    #[default]
    Log,
    /// Fourth root.
    FourthRoot,
    /// Fifth root.
    FifthRoot,
}

impl SpectrogramScale {
    fn ffmpeg_index(self) -> u8 {
        match self {
            Self::Linear => 0,
            Self::Sqrt => 1,
            Self::Cbrt => 2,
            Self::Log => 3,
            Self::FourthRoot => 4,
            Self::FifthRoot => 5,
        }
    }
}

/// Options for [`render_spectrogram`].
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SpectrogramOptions {
    /// Image width in pixels (`s=<W>x<H>`). Must be in
    /// `16..=8192`. Default `1024`.
    #[serde(default = "default_width")]
    pub width: u32,
    /// Image height in pixels. Must be in `16..=8192`. Default `512`.
    #[serde(default = "default_height")]
    pub height: u32,
    /// Draw the legend / axis labels (`legend=1`). Default `true`.
    #[serde(default = "default_true")]
    pub legend: bool,
    /// Channel display mode: `false` = combined, `true` = separate
    /// per-channel panels (`mode=1`). Default `false`.
    #[serde(default)]
    pub separate_channels: bool,
    /// Color preset (`color=`).
    #[serde(default)]
    pub color: SpectrogramColor,
    /// Frequency-axis scale (`fscale=`).
    #[serde(default)]
    pub freq_scale: SpectrogramFreqScale,
    /// Magnitude display scale (`scale=`).
    #[serde(default)]
    pub scale: SpectrogramScale,
    /// Color saturation multiplier (`saturation=`). Must be finite and
    /// in `-10..=10`. `None` → ffmpeg default `1`.
    #[serde(default)]
    pub saturation: Option<f64>,
}

fn default_width() -> u32 {
    1024
}
fn default_height() -> u32 {
    512
}
fn default_true() -> bool {
    true
}

impl Default for SpectrogramOptions {
    fn default() -> Self {
        Self {
            width: default_width(),
            height: default_height(),
            legend: true,
            separate_channels: false,
            color: SpectrogramColor::default(),
            freq_scale: SpectrogramFreqScale::default(),
            scale: SpectrogramScale::default(),
            saturation: None,
        }
    }
}

/// Render a spectrogram PNG of the first audio stream of `input`.
///
/// Runs `ffmpeg -i <in> -filter_complex "[0:a:0]showspectrumpic=...[v]"
/// -map "[v]" -frames:v 1 output.png`.
///
/// # Errors
///
/// - [`MediaError::InvalidInput`] — empty/oversized input, dimensions
///   or saturation out of range.
/// - [`MediaError::LimitExceeded`] — byte caps.
/// - [`MediaError::Unsupported`] — `showspectrumpic` filter or `png`
///   encoder missing.
/// - [`MediaError::ToolFailed`] — undecodable input / no audio.
pub fn render_spectrogram(
    input: &[u8],
    options: &SpectrogramOptions,
    limits: &MediaLimits,
    rt: &Runtime,
) -> MediaResult<MediaOutput> {
    check_size(input, limits)?;
    if input.is_empty() {
        return Err(MediaError::InvalidInput("empty input buffer".to_string()));
    }
    options.validate()?;

    let caps = rt.capabilities()?;
    if !caps.has_filter("showspectrumpic") {
        return Err(MediaError::Unsupported(
            "filter \"showspectrumpic\" is not available in this FFmpeg build".to_string(),
        ));
    }
    if !caps.has_encoder("png") {
        return Err(MediaError::Unsupported(
            "PNG encoder is not available in this FFmpeg build".to_string(),
        ));
    }
    if !caps.has_muxer("image2") {
        return Err(MediaError::Unsupported(
            "image2 muxer is not available in this FFmpeg build".to_string(),
        ));
    }

    let filter = options.filter_string();

    let dir = TempDir::new("spectrogram")?;
    let in_ext = sniff_extension(input);
    let in_name = format!("input.{in_ext}");
    let in_path = dir.write(&in_name, input)?;

    let out_name = "output.png".to_string();
    let argv: Vec<OsString> = vec![
        "-hide_banner".into(),
        "-loglevel".into(),
        "error".into(),
        "-nostdin".into(),
        "-y".into(),
        "-i".into(),
        in_path.into_os_string(),
        "-filter_complex".into(),
        filter.into(),
        "-map".into(),
        "[v]".into(),
        "-frames:v".into(),
        "1".into(),
        "-c:v".into(),
        "png".into(),
        dir.file(&out_name).into_os_string(),
    ];

    process::run_checked("ffmpeg", &rt.ffmpeg, &argv, None, limits)?;

    let bytes = dir.read_limited(&out_name, limits.max_output_bytes)?;
    if bytes.is_empty() {
        return Err(MediaError::ToolFailed {
            tool: "ffmpeg",
            status: "empty output".to_string(),
            detail: "showspectrumpic produced a zero-byte file".to_string(),
        });
    }
    // Sanity: PNG magic bytes.
    if !bytes.starts_with(&[0x89, b'P', b'N', b'G']) {
        return Err(MediaError::ToolFailed {
            tool: "ffmpeg",
            status: "bad output".to_string(),
            detail: "showspectrumpic output is not a PNG".to_string(),
        });
    }

    let mut out = MediaOutput::single(out_name, bytes);
    if let Some(file) = out.primary.as_mut() {
        file.content_type = Some("image/png".to_string());
    }
    Ok(out)
}

impl SpectrogramOptions {
    fn validate(&self) -> MediaResult<()> {
        for (name, v) in [("width", self.width), ("height", self.height)] {
            if !(MIN_DIM..=MAX_DIM).contains(&v) {
                return Err(MediaError::InvalidInput(format!(
                    "{name} must be in {MIN_DIM}..={MAX_DIM}, got {v}"
                )));
            }
        }
        if let Some(s) = self.saturation
            && (!s.is_finite() || !(-10.0..=10.0).contains(&s))
        {
            return Err(MediaError::InvalidInput(format!(
                "saturation must be finite and in -10..=10, got {s}"
            )));
        }
        Ok(())
    }

    /// Render the `-filter_complex` script producing `[v]`.
    fn filter_string(&self) -> String {
        let mut f = format!(
            "[0:a:0]showspectrumpic=s={}x{}:legend={}:mode={}:color={}:fscale={}:scale={}",
            self.width,
            self.height,
            self.legend as u8,
            self.separate_channels as u8,
            self.color.ffmpeg_index(),
            self.freq_scale.ffmpeg_index(),
            self.scale.ffmpeg_index(),
        );
        if let Some(s) = self.saturation {
            f.push_str(&format!(":saturation={s}"));
        }
        f.push_str("[v]");
        f
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
            return "mp4";
        }
    }
    if input[0] == 0xFF && (input[1] & 0xE0) == 0xE0 {
        return "mp3";
    }
    "bin"
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

    /// Generate a 1 s 440 Hz WAV via lavfi.
    fn sine_wav() -> Option<Vec<u8>> {
        let rt = runtime()?;
        let dir = TempDir::new("spectrogram-test-in").ok()?;
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

    #[test]
    fn filter_string_format() {
        let opts = SpectrogramOptions {
            width: 640,
            height: 480,
            legend: true,
            separate_channels: false,
            color: SpectrogramColor::Rainbow,
            freq_scale: SpectrogramFreqScale::Log,
            scale: SpectrogramScale::Log,
            saturation: Some(1.5),
        };
        let f = opts.filter_string();
        assert!(
            f.starts_with(
                "[0:a:0]showspectrumpic=s=640x480:legend=1:mode=0:color=2:fscale=1:scale=3"
            ),
            "{f}"
        );
        assert!(f.contains("saturation=1.5"), "{f}");
        assert!(f.ends_with("[v]"), "{f}");
    }

    #[test]
    fn rejects_bad_dimensions() {
        let Some(rt) = runtime() else {
            eprintln!("skipping: ffmpeg unavailable");
            return;
        };
        let input = sine_wav().unwrap_or_default();
        for (w, h) in [(0u32, 512u32), (8, 512), (1024, 9000)] {
            let opts = SpectrogramOptions {
                width: w,
                height: h,
                ..Default::default()
            };
            let err =
                render_spectrogram(&input, &opts, &limits(), &rt).expect_err("bad dims must fail");
            assert!(matches!(err, MediaError::InvalidInput(_)), "got {err:?}");
        }
    }

    #[test]
    fn spectrogram_produces_png() {
        let (Some(input), Some(rt)) = (sine_wav(), runtime()) else {
            eprintln!("skipping: ffmpeg lavfi unavailable");
            return;
        };
        let caps = match rt.capabilities() {
            Ok(c) => c,
            Err(_) => return,
        };
        if !caps.has_filter("showspectrumpic") || !caps.has_encoder("png") {
            eprintln!("skipping: showspectrumpic/png unavailable");
            return;
        }
        let opts = SpectrogramOptions {
            width: 640,
            height: 480,
            ..Default::default()
        };
        let out = render_spectrogram(&input, &opts, &limits(), &rt).expect("spectrogram");
        let file = out.primary.expect("primary");
        assert_eq!(file.name, "output.png");
        assert_eq!(file.content_type.as_deref(), Some("image/png"));
        assert!(file.bytes.starts_with(&[0x89, b'P', b'N', b'G']));
        assert!(file.bytes.len() > 1000);
    }
}
