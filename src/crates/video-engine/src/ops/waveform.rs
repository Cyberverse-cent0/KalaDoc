//! Still-image audio visualizations: waveform, spectrogram, frequency
//! spectrum, vectorscope, and volume-meter renders exported as a single
//! PNG frame.
//!
//! This is distinct from [`crate::crates::video_engine::ops::av_visual`]
//! (audio → *video* conversions) and from `audio_fx` (video-spectrogram
//! overlays): every kind here produces exactly one still image via the
//! `*pic`/single-frame ffmpeg filters.
//!
//! # Filter backends (ffmpeg 7.1.x)
//!
//! | [`WaveformKind`] | ffmpeg filter | Output |
//! |---|---|---|
//! | `Waveform`      | `showwavespic`   | single frame |
//! | `Spectrogram`   | `showspectrumpic`| single frame |
//! | `FreqSpectrum`  | `showfreqs`      | video → tail frame |
//! | `Vectorscope`   | `avectorscope`   | video → tail frame |
//! | `Volume`        | `showvolume`     | video → tail frame |
//!
//! `showfreqs`, `avectorscope`, and `showvolume` are `A->V` filters that
//! emit one video frame per `rate` tick across the whole input; there is
//! no "picture" variant. For those kinds we append a `select` filter
//! that keeps only frames at/after `duration - 0.2s` and take the first
//! surviving frame with `-frames:v 1`, i.e. the meter/scope state near
//! the end of the audio rather than the empty first frame.
//!
//! `showvolume` has no `s=WxH` option: `w` is the *total* canvas width
//! (split across channels) and `h` is the per-channel meter height, so
//! the real PNG is roughly `width × (channels*height + borders + text)`.
//! `w`/`h` are clamped to the filter's accepted ranges.

use crate::crates::video_engine::error::{VideoError, VideoResult};
use crate::crates::video_engine::ops::probe::probe;
use crate::crates::video_engine::ops::{
    COMMAND_TIMEOUT, FFMPEG, TempDir, check_input, find_tool, read_output, run_tool, tool_failure,
    write_input,
};

const PNG_MAGIC: &[u8; 8] = b"\x89PNG\r\n\x1a\n";

/// Amplitude-scale values accepted by `showwavespic:scale`.
const SCALE_WAVEFORM: &[&str] = &["lin", "log", "sqrt", "cbrt"];
/// Display-scale values accepted by `showspectrumpic:scale`.
const SCALE_SPECTROGRAM: &[&str] = &["lin", "sqrt", "cbrt", "log", "4thrt", "5thrt"];
/// `showfreqs` has separate `ascale`/`fscale`; `scale` maps to `ascale`.
const SCALE_FREQS: &[&str] = &["lin", "sqrt", "cbrt", "log"];
/// `avectorscope:scale` amplitude modes.
const SCALE_VECTORSCOPE: &[&str] = &["lin", "sqrt", "cbrt", "log"];
/// `showvolume:ds` display scales (linear/log only).
const SCALE_VOLUME: &[&str] = &["lin", "log"];

/// `showspectrumpic:color` palette names (int enum; names verified
/// against ffmpeg 7.1).
const SPECTROGRAM_COLORS: &[&str] = &[
    "channel",
    "intensity",
    "rainbow",
    "moreland",
    "nebulae",
    "fire",
    "fiery",
    "fruit",
    "cool",
    "magma",
    "green",
    "viridis",
    "plasma",
    "cividis",
    "terrain",
];

/// Which still-image visualization [`audio_waveform_image`] renders.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum WaveformKind {
    /// Oscilloscope-style amplitude waveform (`showwavespic`).
    Waveform,
    /// Frequency-vs-time spectrogram with legend (`showspectrumpic`).
    Spectrogram,
    /// Real-time FFT bar/line display (`showfreqs`, `mode=bar`).
    FreqSpectrum,
    /// Stereo Lissajous/phase vectorscope (`avectorscope`). Requires a
    /// stereo (≥2 channel) audio stream.
    Vectorscope,
    /// Per-channel volume meter (`showvolume`); renders the meter state
    /// near the end of the audio.
    Volume,
}

/// Parameters for [`audio_waveform_image`].
///
/// `width`/`height` are the requested canvas in pixels; filters that
/// cannot honor them exactly (notably `showvolume`) approximate as
/// documented in the module docs. `color` and `scale` are optional
/// per-kind overrides — see the module table for which filter option
/// each maps to; unsupported combinations are rejected with
/// [`VideoError::InvalidInput`] rather than silently dropped where the
/// filter has an equivalent option, and ignored only where the filter
/// genuinely has no color control (vectorscope, volume meter).
#[derive(Debug, Clone)]
pub struct WaveformParams {
    /// Which visualization to render.
    pub kind: WaveformKind,
    /// Requested image width in pixels.
    pub width: u32,
    /// Requested image height in pixels.
    pub height: u32,
    /// Optional color override:
    /// - `Waveform`/`FreqSpectrum` → `colors` channel-color list
    ///   (e.g. `"red|green|blue"`).
    /// - `Spectrogram` → `color` palette name (e.g. `"fire"`).
    /// - `Vectorscope`/`Volume` → ignored (no named-color option).
    pub color: Option<String>,
    /// Optional amplitude/display scale override; validated against the
    /// per-filter allowed set (see module docs).
    pub scale: Option<String>,
}

/// Validate a `colors`-style option value: ffmpeg color lists are
/// `|`-separated names, so only alphanumerics plus `|_-` are allowed —
/// anything else could break filtergraph parsing.
fn color_list(value: &str, op: &str) -> VideoResult<String> {
    if value.is_empty()
        || !value
            .chars()
            .all(|c| c.is_ascii_alphanumeric() || matches!(c, '|' | '_' | '-'))
    {
        return Err(VideoError::InvalidInput(format!(
            "{op}: invalid color list {value:?} (expected names separated by '|')"
        )));
    }
    Ok(value.to_string())
}

/// Validate an enum-style option (scale/palette) case-insensitively.
fn enum_arg(value: &str, allowed: &[&str], op: &str, option: &str) -> VideoResult<String> {
    let lower = value.to_ascii_lowercase();
    if allowed.contains(&lower.as_str()) {
        Ok(lower)
    } else {
        Err(VideoError::InvalidInput(format!(
            "{op}: unsupported {option} {value:?} (allowed: {})",
            allowed.join(", ")
        )))
    }
}

/// Build the audio→video filter chain (without the `[0:a]` pad or
/// `[v]` label) for `params`. Returns the chain; the caller wraps it
/// and appends the tail-frame `select` for video-producing filters.
fn filter_chain(params: &WaveformParams, op: &str) -> VideoResult<String> {
    let size = format!("{}x{}", params.width, params.height);
    match params.kind {
        WaveformKind::Waveform => {
            let mut f = format!("showwavespic=s={size}");
            match &params.color {
                Some(c) => {
                    f.push_str(":split_channels=1:colors=");
                    f.push_str(&color_list(c, op)?);
                }
                None => f.push_str(":split_channels=0"),
            }
            if let Some(s) = &params.scale {
                f.push_str(":scale=");
                f.push_str(&enum_arg(s, SCALE_WAVEFORM, op, "scale")?);
            }
            Ok(f)
        }
        WaveformKind::Spectrogram => {
            let mut f = format!("showspectrumpic=s={size}:legend=1");
            if let Some(s) = &params.scale {
                f.push_str(":scale=");
                f.push_str(&enum_arg(s, SCALE_SPECTROGRAM, op, "scale")?);
            }
            if let Some(c) = &params.color {
                let name = enum_arg(c, SPECTROGRAM_COLORS, op, "color")?;
                f.push_str(":color=");
                f.push_str(&name);
            }
            Ok(f)
        }
        WaveformKind::FreqSpectrum => {
            let mut f = format!("showfreqs=s={size}:mode=bar");
            if let Some(s) = &params.scale {
                f.push_str(":ascale=");
                f.push_str(&enum_arg(s, SCALE_FREQS, op, "scale")?);
            }
            if let Some(c) = &params.color {
                f.push_str(":colors=");
                f.push_str(&color_list(c, op)?);
            }
            Ok(f)
        }
        WaveformKind::Vectorscope => {
            let mut f = format!("avectorscope=s={size}:draw=dot");
            if let Some(s) = &params.scale {
                f.push_str(":scale=");
                f.push_str(&enum_arg(s, SCALE_VECTORSCOPE, op, "scale")?);
            }
            Ok(f)
        }
        WaveformKind::Volume => {
            // showvolume: w = total canvas width, h = per-channel meter
            // height (ranges w:80..8192, h:1..900, rate defaults to 25).
            let w = params.width.clamp(80, 8192);
            let h = params.height.clamp(1, 900);
            let mut f = format!("showvolume=w={w}:h={h}");
            if let Some(s) = &params.scale {
                f.push_str(":ds=");
                f.push_str(&enum_arg(s, SCALE_VOLUME, op, "scale")?);
            }
            Ok(f)
        }
    }
}

/// Render `input` (any media container with an audio stream) as a single
/// PNG still image of the requested [`WaveformKind`].
///
/// The input must contain at least one audio stream — probed up front,
/// so video-only input fails with [`VideoError::InvalidInput`] before
/// ffmpeg runs. `Vectorscope` additionally requires ≥2 channels when
/// the channel count is known.
pub fn audio_waveform_image(input: &[u8], params: &WaveformParams) -> VideoResult<Vec<u8>> {
    const OP: &str = "audio_waveform_image";

    check_input(input)?;
    let program = find_tool(FFMPEG)?;

    let info = probe(input)?;
    if !info.has_audio {
        return Err(VideoError::InvalidInput(format!(
            "{OP} input has no audio stream"
        )));
    }
    if params.width == 0 || params.height == 0 {
        return Err(VideoError::InvalidInput(format!(
            "{OP} width/height must be non-zero, got {}x{}",
            params.width, params.height
        )));
    }

    // Channel count of the first audio stream (used for the vectorscope
    // stereo requirement; `None` means ffprobe didn't report it).
    let channels = info
        .streams
        .iter()
        .find(|s| s.codec_type == "audio")
        .and_then(|s| s.channels);

    if params.kind == WaveformKind::Vectorscope && matches!(channels, Some(ch) if ch < 2) {
        return Err(VideoError::InvalidInput(format!(
            "{OP} Vectorscope requires a stereo (>=2 channel) audio stream"
        )));
    }

    let chain = filter_chain(params, OP)?;

    // `[0:a]<chain>[,select='gte(t,T)'][v]` — the select tail keeps only
    // frames at/after `duration - 0.2s` so `-frames:v 1` captures the
    // developed meter/scope instead of the empty first frame.
    let tail_select = match params.kind {
        WaveformKind::FreqSpectrum | WaveformKind::Vectorscope | WaveformKind::Volume => info
            .duration_secs
            .map(|d| format!(",select='gte(t,{:.3})'", (d - 0.2).max(0.0))),
        _ => None,
    };
    let filter_complex = format!("[0:a]{chain}{}[v]", tail_select.unwrap_or_default());

    let dir = TempDir::new("waveimg")?;
    let input_path = write_input(&dir, "input.bin", input)?;
    let out_path = dir.file("waveform.png");

    let args = vec![
        "-y".to_string(),
        "-v".to_string(),
        "error".to_string(),
        "-i".to_string(),
        input_path.to_string_lossy().into_owned(),
        "-filter_complex".to_string(),
        filter_complex.clone(),
        "-map".to_string(),
        "[v]".to_string(),
        "-frames:v".to_string(),
        "1".to_string(),
        "-f".to_string(),
        "image2".to_string(),
        out_path.to_string_lossy().into_owned(),
    ];
    let output = run_tool(FFMPEG, &program, &args, COMMAND_TIMEOUT)?;
    if !output.status.success() {
        return Err(tool_failure(FFMPEG, &output));
    }

    let png = read_output(&out_path)?;
    if png.len() < PNG_MAGIC.len() || &png[..PNG_MAGIC.len()] != PNG_MAGIC {
        return Err(VideoError::ExternalTool {
            tool: FFMPEG.to_string(),
            detail: format!("{OP} produced no PNG frame (filter chain: {filter_complex})"),
        });
    }
    Ok(png)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::crates::video_engine::ops::{FFPROBE, find_tool};
    use std::fs;

    fn tools_available() -> bool {
        find_tool(FFMPEG).is_ok() && find_tool(FFPROBE).is_ok()
    }

    /// Run ffmpeg to synthesize a fixture WAV; `None` when tools/encode
    /// fail so tests skip rather than fail on missing ffmpeg.
    fn synthesize(args: &[String], out_name: &str) -> Option<Vec<u8>> {
        let program = find_tool(FFMPEG).ok()?;
        let dir = TempDir::new("waveform-test").ok()?;
        let out = dir.file(out_name);
        let mut full = vec!["-y".to_string(), "-v".to_string(), "error".to_string()];
        full.extend_from_slice(args);
        full.push(out.to_string_lossy().into_owned());
        let output = run_tool(FFMPEG, &program, &full, COMMAND_TIMEOUT).ok()?;
        if !output.status.success() {
            return None;
        }
        fs::read(out).ok()
    }

    /// 1s mono 440 Hz sine WAV.
    fn mono_wav() -> Option<Vec<u8>> {
        synthesize(
            &[
                "-f".into(),
                "lavfi".into(),
                "-i".into(),
                "sine=frequency=440:duration=1:sample_rate=48000".into(),
                "-c:a".into(),
                "pcm_s16le".into(),
            ],
            "mono.wav",
        )
    }

    /// 1s stereo WAV: L = 440 Hz sine + pink noise, R = 880 Hz sine +
    /// brown noise. The noise keeps spectrum/scope renders non-trivial.
    fn stereo_wav() -> Option<Vec<u8>> {
        synthesize(
            &[
                "-f".into(),
                "lavfi".into(),
                "-i".into(),
                "sine=frequency=440:duration=1:sample_rate=48000".into(),
                "-f".into(),
                "lavfi".into(),
                "-i".into(),
                "anoisesrc=color=pink:duration=1:sample_rate=48000".into(),
                "-f".into(),
                "lavfi".into(),
                "-i".into(),
                "sine=frequency=880:duration=1:sample_rate=48000".into(),
                "-f".into(),
                "lavfi".into(),
                "-i".into(),
                "anoisesrc=color=brown:duration=1:sample_rate=48000".into(),
                "-filter_complex".into(),
                "[0:a][1:a]amix=inputs=2:normalize=0[x];\
                 [2:a][3:a]amix=inputs=2:normalize=0[y];\
                 [x][y]amerge=inputs=2[a]"
                    .into(),
                "-map".into(),
                "[a]".into(),
                "-c:a".into(),
                "pcm_s16le".into(),
            ],
            "stereo.wav",
        )
    }

    /// 1s testsrc MP4 with no audio stream (needs an H.264 encoder).
    fn video_only_mp4() -> Option<Vec<u8>> {
        let enc = crate::crates::video_engine::ops::h264_encoder()?;
        synthesize(
            &[
                "-f".into(),
                "lavfi".into(),
                "-i".into(),
                "testsrc=duration=1:size=160x120:rate=10".into(),
                "-c:v".into(),
                enc.to_string(),
                "-pix_fmt".into(),
                "yuv420p".into(),
                "-an".into(),
            ],
            "video_only.mp4",
        )
    }

    fn params(kind: WaveformKind) -> WaveformParams {
        WaveformParams {
            kind,
            width: 480,
            height: 240,
            color: None,
            scale: None,
        }
    }

    fn assert_png(png: &[u8]) {
        assert_eq!(&png[..8], PNG_MAGIC, "output is not a PNG");
        // Mostly-flat renders compress hard; a real render is still
        // tens-to-hundreds of bytes, far above a bare signature.
        assert!(
            png.len() > 100,
            "PNG suspiciously small ({} bytes)",
            png.len()
        );
    }

    /// Assert the PNG's IHDR carries `width`x`height`.
    fn assert_png_dims(png: &[u8], width: u32, height: u32) {
        let w = u32::from_be_bytes(png[16..20].try_into().unwrap());
        let h = u32::from_be_bytes(png[20..24].try_into().unwrap());
        assert_eq!((w, h), (width, height), "unexpected PNG dimensions");
    }

    #[test]
    fn waveform_mono_png() {
        if !tools_available() {
            return;
        }
        let input = mono_wav().expect("mono fixture");
        let png =
            audio_waveform_image(&input, &params(WaveformKind::Waveform)).expect("waveform render");
        assert_png(&png);
        assert_png_dims(&png, 480, 240);
    }

    #[test]
    fn waveform_split_channels_with_color() {
        if !tools_available() {
            return;
        }
        let input = stereo_wav().expect("stereo fixture");
        let mut p = params(WaveformKind::Waveform);
        p.color = Some("red|cyan".to_string());
        p.scale = Some("log".to_string());
        let png = audio_waveform_image(&input, &p).expect("waveform render");
        assert_png(&png);
    }

    #[test]
    fn spectrogram_png() {
        if !tools_available() {
            return;
        }
        let input = stereo_wav().expect("stereo fixture");
        let mut p = params(WaveformKind::Spectrogram);
        p.color = Some("fiery".to_string());
        let png = audio_waveform_image(&input, &p).expect("spectrogram render");
        assert_png(&png);
    }

    #[test]
    fn freq_spectrum_tail_frame_png() {
        if !tools_available() {
            return;
        }
        let input = stereo_wav().expect("stereo fixture");
        let png = audio_waveform_image(&input, &params(WaveformKind::FreqSpectrum))
            .expect("freq spectrum render");
        assert_png(&png);
    }

    #[test]
    fn vectorscope_stereo_png() {
        if !tools_available() {
            return;
        }
        let input = stereo_wav().expect("stereo fixture");
        let png = audio_waveform_image(&input, &params(WaveformKind::Vectorscope))
            .expect("vectorscope render");
        assert_png(&png);
    }

    #[test]
    fn volume_meter_tail_frame_png() {
        if !tools_available() {
            return;
        }
        let input = stereo_wav().expect("stereo fixture");
        let mut p = params(WaveformKind::Volume);
        p.width = 512;
        p.height = 128;
        let png = audio_waveform_image(&input, &p).expect("volume render");
        assert_png(&png);
    }

    #[test]
    fn rejects_input_without_audio() {
        if !tools_available() {
            return;
        }
        let Some(input) = video_only_mp4() else {
            return; // no H.264 encoder on this ffmpeg build
        };
        let err = audio_waveform_image(&input, &params(WaveformKind::Waveform))
            .expect_err("video-only input must fail");
        assert!(
            matches!(err, VideoError::InvalidInput(_)),
            "expected InvalidInput, got {err:?}"
        );
    }

    #[test]
    fn vectorscope_rejects_mono() {
        if !tools_available() {
            return;
        }
        let input = mono_wav().expect("mono fixture");
        let err = audio_waveform_image(&input, &params(WaveformKind::Vectorscope))
            .expect_err("mono input must fail vectorscope");
        assert!(
            matches!(err, VideoError::InvalidInput(_)),
            "expected InvalidInput, got {err:?}"
        );
    }

    #[test]
    fn rejects_bad_params() {
        if !tools_available() {
            return;
        }
        let input = mono_wav().expect("mono fixture");
        let mut p = params(WaveformKind::Waveform);
        p.scale = Some("bogus".to_string());
        assert!(matches!(
            audio_waveform_image(&input, &p),
            Err(VideoError::InvalidInput(_))
        ));
        let mut p = params(WaveformKind::Waveform);
        p.width = 0;
        assert!(matches!(
            audio_waveform_image(&input, &p),
            Err(VideoError::InvalidInput(_))
        ));
    }
}
