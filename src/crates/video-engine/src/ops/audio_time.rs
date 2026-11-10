//! Audio-specific time and channel operations not covered by
//! [`audio`](super::audio) (volume/normalize/merge/concat/fade/remove)
//! or [`tempo`](super::tempo) (A/V speed+reverse+loop+freeze):
//!
//! - [`audio_reverse`] — reverse an audio stream (`areverse` filter).
//! - [`audio_loop`] — repeat the audio `count` extra times
//!   (`-stream_loop`, with a concat-demuxer fallback).
//! - [`audio_speed`] — tempo change via chained `atempo` filters;
//!   pitch is preserved because `atempo` never resamples pitch.
//! - [`audio_trim`] — cut a `[start_ms, end_ms)` range into an
//!   explicit audio container.
//! - [`audio_channels`] — explicit channel-layout matrix
//!   (mono/stereo/5.1/7.1/custom).
//! - [`audio_convert`] — audio→audio format conversion across the
//!   codec/muxer table below.
//!
//! Every op accepts audio-only *or* A/V input. When the input carries
//! video and the output stays in an A/V-capable container the video is
//! stream-copied (`-c:v copy`); audio-only output containers get `-vn`.
//! Output container for the container-preserving ops comes from
//! [`ext_for_format`] on the probed `format_name`, falling back to
//! `wav` + `pcm_s16le` when the container cannot be named or mapped.

use std::path::Path;

use crate::crates::core::engine::EngineLimits;
use crate::crates::video_engine::error::{VideoError, VideoResult};
use crate::crates::video_engine::model::VideoInfo;
use crate::crates::video_engine::ops::probe::probe;
use crate::crates::video_engine::ops::{
    COMMAND_TIMEOUT, FFMPEG, PROBE_TIMEOUT, TempDir, check_input, check_limits, encoder_available,
    ext_for_format, find_tool, read_output, run_tool, tool_failure, write_input,
};

/// Parameters for [`audio_speed`].
#[derive(Debug, Clone, Copy)]
pub struct AudioSpeedParams {
    /// Tempo multiplier: `2.0` = twice as fast (half duration),
    /// `0.5` = half speed (double duration). Must be within
    /// `0.5..=100.0` — see [`audio_speed`].
    pub factor: f64,
}

/// Parameters for [`audio_loop`].
#[derive(Debug, Clone, Copy)]
pub struct LoopParams {
    /// Number of *additional* times the input is repeated:
    /// `count = 1` plays the audio twice, `0` is a no-op passthrough.
    pub count: u32,
}

/// Explicit target channel layout for [`audio_channels`].
#[derive(Debug, Clone)]
pub enum ChannelLayout {
    /// 1 channel (`mono`).
    Mono,
    /// 2 channels (`stereo`).
    Stereo,
    /// 6 channels, ITU 5.1 (`5.1`).
    Surround51,
    /// 8 channels, ITU 7.1 (`7.1`).
    Surround71,
    /// Any other ffmpeg channel-layout name (e.g. `"2.1"`, `"quad"`,
    /// `"hexagonal"`). Passed through after validation: ASCII
    /// alphanumerics plus `.` and `_` only.
    Custom(String),
}

/// Audio container/codec targets shared by [`audio_trim`] and
/// [`audio_convert`]. Extension → ffmpeg codec args + the encoder
/// name verified via `ffmpeg -encoders` before running.
struct AudioFormat {
    /// Output file extension (also selects the muxer).
    ext: &'static str,
    /// `-c:a <codec>` argument pair.
    codec_args: [&'static str; 2],
    /// Encoder that must exist in this ffmpeg build.
    encoder: &'static str,
}

/// Codec/muxer table for audio-only output containers. Modeled on the
/// `audio_args`/`required_encoder` pattern in `extract_audio.rs`
/// (which is private to that module, so the table is duplicated here
/// and extended with flac/opus).
const AUDIO_FORMATS: &[AudioFormat] = &[
    AudioFormat {
        ext: "mp3",
        codec_args: ["-c:a", "libmp3lame"],
        encoder: "libmp3lame",
    },
    AudioFormat {
        ext: "wav",
        codec_args: ["-c:a", "pcm_s16le"],
        encoder: "pcm_s16le",
    },
    AudioFormat {
        ext: "flac",
        codec_args: ["-c:a", "flac"],
        encoder: "flac",
    },
    AudioFormat {
        ext: "ogg",
        codec_args: ["-c:a", "libvorbis"],
        encoder: "libvorbis",
    },
    AudioFormat {
        ext: "opus",
        codec_args: ["-c:a", "libopus"],
        encoder: "libopus",
    },
    // "aac" (ADTS) and "m4a" (IPOD/MP4 muxer) both carry AAC; the
    // extension picks the muxer.
    AudioFormat {
        ext: "m4a",
        codec_args: ["-c:a", "aac"],
        encoder: "aac",
    },
    AudioFormat {
        ext: "aac",
        codec_args: ["-c:a", "aac"],
        encoder: "aac",
    },
];

/// Extensions [`audio_trim`]/[`audio_convert`] accept.
pub(crate) const AUDIO_FORMAT_EXTS: &[&str] = &["mp3", "wav", "flac", "ogg", "opus", "m4a", "aac"];

/// Look up an [`AudioFormat`] by already-normalized extension.
fn format_spec(ext: &str) -> Option<&'static AudioFormat> {
    AUDIO_FORMATS.iter().find(|f| f.ext == ext)
}

/// Formats whose required encoder exists in this ffmpeg build.
fn supported_formats() -> Vec<&'static str> {
    AUDIO_FORMATS
        .iter()
        .filter(|f| encoder_available(f.encoder))
        .map(|f| f.ext)
        .collect()
}

/// Validate `ext` against [`AUDIO_FORMATS`] *and* verify the encoder
/// exists. Unknown extension or missing encoder →
/// [`VideoError::InvalidInput`] listing the supported formats, per the
/// module contract (no mid-run ffmpeg failure).
fn checked_format(ext: &str, op: &str) -> VideoResult<&'static AudioFormat> {
    let norm = ext.trim().trim_start_matches('.').to_ascii_lowercase();
    let Some(spec) = format_spec(&norm) else {
        return Err(VideoError::InvalidInput(format!(
            "{op} target format '{ext}' is not supported; supported: {}",
            AUDIO_FORMAT_EXTS.join(", ")
        )));
    };
    if !encoder_available(spec.encoder) {
        return Err(VideoError::InvalidInput(format!(
            "{op} target format '{ext}' requires the {} encoder, which this ffmpeg \
             build does not provide; supported: {}",
            spec.encoder,
            supported_formats().join(", ")
        )));
    }
    Ok(spec)
}

/// Codec args + required encoder for *container-preserving* outputs
/// (reverse/loop/speed/channels). `ext` is an [`ext_for_format`]
/// result; unmapped extensions yield `None` so the caller falls back
/// to `wav`.
fn container_codec(ext: &str) -> Option<(&'static str, Option<&'static str>)> {
    // (codec name, required encoder) — encoder `None` means the codec
    // is native to ffmpeg and assumed present.
    Some(match ext {
        "mp4" | "mkv" | "ts" => ("aac", None),
        "webm" => ("libopus", Some("libopus")),
        "mpg" | "wmv" => ("libmp3lame", Some("libmp3lame")),
        "ogg" => ("libvorbis", Some("libvorbis")),
        "mp3" => ("libmp3lame", Some("libmp3lame")),
        "wav" => ("pcm_s16le", None),
        _ => return None,
    })
}

/// Whether `ext` names a pure audio container (video must be dropped
/// with `-vn`). A/V-capable containers get `-c:v copy` instead.
fn is_audio_only_ext(ext: &str) -> bool {
    matches!(ext, "mp3" | "wav" | "ogg" | "flac" | "opus" | "aac" | "m4a")
}

/// Output-side args shared by every op: drop video for audio-only
/// containers, stream-copy it otherwise, then apply the audio codec.
fn output_args(ext: &str, codec: &str) -> Vec<String> {
    let mut args = Vec::with_capacity(6);
    if is_audio_only_ext(ext) {
        args.push("-vn".to_string());
    } else {
        args.extend(["-c:v".to_string(), "copy".to_string()]);
    }
    args.extend(["-c:a".to_string(), codec.to_string()]);
    args
}

/// Probe + limit-check the input and require an audio stream.
fn checked_input(input: &[u8], limits: &EngineLimits) -> VideoResult<VideoInfo> {
    let len = check_input(input)?;
    let info = probe(input)?;
    check_limits(&info, len, limits, false)?;
    if !info.has_audio {
        return Err(VideoError::InvalidInput(
            "input has no audio stream".to_string(),
        ));
    }
    Ok(info)
}

/// Resolve the output container for container-preserving ops:
/// `(ext, codec)` where `codec` is the ffmpeg `-c:a` codec name.
/// Falls back to `wav`/`pcm_s16le` when the input container cannot be
/// named or mapped to an audio codec.
fn container_target(info: &VideoInfo) -> (String, String) {
    let ext = info
        .format_name
        .as_deref()
        .and_then(ext_for_format)
        .unwrap_or("wav");
    match container_codec(ext) {
        Some((codec, required)) => {
            if let Some(enc) = required {
                if !encoder_available(enc) {
                    // Codec exists but encoder missing — degrade to wav
                    // rather than fail, since this is a convenience path.
                    return ("wav".to_string(), "pcm_s16le".to_string());
                }
            }
            (ext.to_string(), codec.to_string())
        }
        None => ("wav".to_string(), "pcm_s16le".to_string()),
    }
}

/// Run ffmpeg and map a non-zero exit to [`VideoError::ExternalTool`].
fn run_ffmpeg(program: &Path, args: &[String]) -> VideoResult<()> {
    let output = run_tool(FFMPEG, program, args, COMMAND_TIMEOUT)?;
    if output.status.success() {
        Ok(())
    } else {
        Err(tool_failure(FFMPEG, &output))
    }
}

/// Whether `ffmpeg -filters` lists `name`. Cached for the process
/// lifetime; a failed listing yields `false` so callers surface an
/// honest error instead of a mid-run filter parse failure.
fn filter_available(name: &str) -> bool {
    use std::sync::OnceLock;
    static FILTERS: OnceLock<Vec<String>> = OnceLock::new();
    let list = FILTERS.get_or_init(|| {
        let Ok(program) = find_tool(FFMPEG) else {
            return Vec::new();
        };
        let args = vec!["-hide_banner".to_string(), "-filters".to_string()];
        match run_tool(FFMPEG, &program, &args, PROBE_TIMEOUT) {
            Ok(output) if output.status.success() => String::from_utf8_lossy(&output.stdout)
                .lines()
                .filter_map(|line| line.split_whitespace().nth(1))
                .map(str::to_string)
                .collect(),
            _ => Vec::new(),
        }
    });
    list.iter().any(|f| f == name)
}

/// Assemble and run the common container-preserving command:
/// `[pre…] -i input [post…] <output args> out.ext`.
fn run_preserving(
    program: &Path,
    dir: &TempDir,
    input: &[u8],
    ext: &str,
    codec: &str,
    pre: &[String],
    post: &[String],
) -> VideoResult<Vec<u8>> {
    let input_path = write_input(dir, "input.bin", input)?;
    let out_path = dir.file(&format!("out.{ext}"));
    let mut args = vec!["-y".to_string(), "-v".to_string(), "error".to_string()];
    args.extend_from_slice(pre);
    args.extend(["-i".to_string(), input_path.to_string_lossy().into_owned()]);
    args.extend_from_slice(post);
    args.extend(output_args(ext, codec));
    args.push(out_path.to_string_lossy().into_owned());
    run_ffmpeg(program, &args)?;
    read_output(&out_path)
}

/// Shared prologue for the container-preserving ops: validate input,
/// resolve ffmpeg, pick the output container/codec.
fn prepare(
    input: &[u8],
    limits: &EngineLimits,
) -> VideoResult<(VideoInfo, std::path::PathBuf, String, String)> {
    let info = checked_input(input, limits)?;
    let program = find_tool(FFMPEG)?;
    let (ext, codec) = container_target(&info);
    Ok((info, program, ext, codec))
}

/// Reverse the audio stream (`areverse`). Duration is unchanged; the
/// waveform plays back end-to-start. Requires the `areverse` filter
/// (present in all normal ffmpeg builds — verified up front).
pub(crate) fn audio_reverse(input: &[u8], limits: &EngineLimits) -> VideoResult<Vec<u8>> {
    if !filter_available("areverse") {
        return Err(VideoError::ExternalTool {
            tool: FFMPEG.to_string(),
            detail: "the areverse audio filter is not present in this ffmpeg build".to_string(),
        });
    }
    let (_info, program, ext, codec) = prepare(input, limits)?;
    let dir = TempDir::new("audio-reverse")?;
    let post = vec!["-af".to_string(), "areverse".to_string()];
    run_preserving(&program, &dir, input, &ext, &codec, &[], &post)
}

/// Repeat the audio `params.count` additional times
/// (`count = 1` → the clip plays twice).
///
/// Primary path: `-stream_loop {count}` on the input plus an audio
/// re-encode (`-stream_loop N` yields N+1 total passes). If that fails
/// — e.g. a demuxer that ignores `-stream_loop` — falls back to the
/// concat demuxer fed the same file `count + 1` times, mirroring the
/// approach in `concat.rs`.
///
/// The estimated output duration (`input × (count + 1)`) is checked
/// against `limits.max_duration_secs` when both are known.
pub(crate) fn audio_loop(
    input: &[u8],
    params: LoopParams,
    limits: &EngineLimits,
) -> VideoResult<Vec<u8>> {
    let info = checked_input(input, limits)?;
    let program = find_tool(FFMPEG)?;
    let (ext, codec) = container_target(&info);

    let total = params.count as u64 + 1;
    if let (Some(max), Some(duration)) = (limits.max_duration_secs, info.duration_secs) {
        let estimated = duration * total as f64;
        if estimated > max as f64 {
            return Err(VideoError::LimitExceeded(format!(
                "looping {}s input {total}× yields ~{estimated:.1}s, exceeding \
                 max_duration_secs {max}",
                duration
            )));
        }
    }

    let dir = TempDir::new("audio-loop")?;

    // Primary: stream_loop on the input. `-stream_loop N` repeats the
    // input N extra times → N+1 total plays.
    let pre = vec!["-stream_loop".to_string(), params.count.to_string()];
    match run_preserving(&program, &dir, input, &ext, &codec, &pre, &[]) {
        Ok(bytes) => return Ok(bytes),
        Err(primary_err) => {
            // Fallback: concat demuxer with count+1 identical entries.
            // Same-file repeats share codec parameters, so stream copy
            // of video is safe where the container allows it.
            let input_path = write_input(&dir, "loop.bin", input)?;
            let mut list = String::new();
            for _ in 0..total {
                list.push_str(&format!("file '{}'\n", input_path.display()));
            }
            let list_path = dir.file("list.txt");
            std::fs::write(&list_path, list)?;
            let out_path = dir.file(&format!("out.{ext}"));
            let mut args = vec![
                "-y".to_string(),
                "-v".to_string(),
                "error".to_string(),
                "-f".to_string(),
                "concat".to_string(),
                "-safe".to_string(),
                "0".to_string(),
                "-i".to_string(),
                list_path.to_string_lossy().into_owned(),
            ];
            args.extend(output_args(&ext, &codec));
            args.push(out_path.to_string_lossy().into_owned());
            match run_ffmpeg(&program, &args).and_then(|()| read_output(&out_path)) {
                Ok(bytes) => return Ok(bytes),
                Err(fallback_err) => {
                    // Surface the concat attempt's error — it is the
                    // most recent and usually the most informative.
                    let _ = primary_err;
                    return Err(fallback_err);
                }
            }
        }
    }
}

/// Change audio tempo by `params.factor` using `atempo` filters.
///
/// `atempo` adjusts tempo *without* pitch shifting (it is a WSOLA-style
/// time-stretch, not a resample), so a `2.0×` speed-up keeps the
/// original pitch. `factor` must be within `0.5..=100.0` — the
/// per-instance range of `atempo` in ffmpeg ≥5 — so a single filter
/// always suffices; the chain builder below still splits wider
/// factors defensively should the bounds ever change.
pub(crate) fn audio_speed(
    input: &[u8],
    params: AudioSpeedParams,
    limits: &EngineLimits,
) -> VideoResult<Vec<u8>> {
    let factor = params.factor;
    if !factor.is_finite() || !(0.5..=100.0).contains(&factor) {
        return Err(VideoError::InvalidInput(format!(
            "audio_speed factor must be within 0.5..=100.0, got {factor}"
        )));
    }
    if !filter_available("atempo") {
        return Err(VideoError::ExternalTool {
            tool: FFMPEG.to_string(),
            detail: "the atempo audio filter is not present in this ffmpeg build".to_string(),
        });
    }
    let (_info, program, ext, codec) = prepare(input, limits)?;
    let dir = TempDir::new("audio-speed")?;
    let chain = atempo_chain(factor).join(",");
    let post = vec!["-af".to_string(), chain];
    run_preserving(&program, &dir, input, &ext, &codec, &[], &post)
}

/// Build the `atempo` filter chain for `factor`. Each instance stays
/// inside ffmpeg's `0.5..=100` range; out-of-range totals are split
/// into multiple filters (e.g. `0.25` → `atempo=0.5,atempo=0.5`).
fn atempo_chain(factor: f64) -> Vec<String> {
    const MIN: f64 = 0.5;
    const MAX: f64 = 100.0;
    let mut filters = Vec::new();
    let mut f = factor;
    while f > MAX {
        filters.push(format!("atempo={MAX}"));
        f /= MAX;
    }
    while f < MIN {
        filters.push(format!("atempo={MIN}"));
        f /= MIN;
    }
    filters.push(format!("atempo={f:.6}"));
    filters
}

/// Trim the audio to `[start_ms, end_ms)` and write it to the
/// `format` container (`mp3`, `wav`, `flac`, `ogg`, `opus`, `m4a`,
/// `aac`). `-ss`/`-to` are placed before `-i` (input-side seeking,
/// frame-accurate because the audio is re-encoded); video is dropped
/// since every accepted target is an audio-only container.
///
/// `end_ms` must be greater than `start_ms`; both are milliseconds.
pub(crate) fn audio_trim(
    input: &[u8],
    start_ms: u64,
    end_ms: u64,
    format: &str,
    limits: &EngineLimits,
) -> VideoResult<Vec<u8>> {
    if end_ms <= start_ms {
        return Err(VideoError::InvalidInput(format!(
            "audio_trim requires end_ms > start_ms, got {start_ms}..{end_ms}"
        )));
    }
    let spec = checked_format(format, "audio_trim")?;
    checked_input(input, limits)?;
    let program = find_tool(FFMPEG)?;
    let dir = TempDir::new("audio-trim")?;
    let input_path = write_input(&dir, "input.bin", input)?;
    let out_path = dir.file(&format!("out.{}", spec.ext));

    let start = start_ms as f64 / 1000.0;
    let end = end_ms as f64 / 1000.0;
    let mut args = vec![
        "-y".to_string(),
        "-v".to_string(),
        "error".to_string(),
        "-ss".to_string(),
        format!("{start:.3}"),
        "-to".to_string(),
        format!("{end:.3}"),
        "-i".to_string(),
        input_path.to_string_lossy().into_owned(),
        "-vn".to_string(),
    ];
    args.extend(spec.codec_args.map(str::to_string));
    args.push(out_path.to_string_lossy().into_owned());

    run_ffmpeg(&program, &args)?;
    read_output(&out_path)
}

/// Convert the audio stream to an explicit `ChannelLayout`.
///
/// Emits `-ac <channels>` plus `-channel_layout <name>` for the named
/// layouts. For [`ChannelLayout::Custom`] the name is validated
/// (ASCII alphanumerics, `.`, `_` only, ≤64 chars) and `-ac` is emitted
/// when the name matches a known ffmpeg layout; otherwise ffmpeg
/// derives the channel count from the layout itself.
pub(crate) fn audio_channels(
    input: &[u8],
    layout: &ChannelLayout,
    limits: &EngineLimits,
) -> VideoResult<Vec<u8>> {
    let (name, channels) = layout_name_channels(layout)?;
    let (_info, program, ext, codec) = prepare(input, limits)?;
    let dir = TempDir::new("audio-channels")?;
    let mut post = Vec::with_capacity(6);
    if let Some(n) = channels {
        post.extend(["-ac".to_string(), n.to_string()]);
    }
    post.extend(["-channel_layout".to_string(), name]);
    run_preserving(&program, &dir, input, &ext, &codec, &[], &post)
}

/// Resolve a [`ChannelLayout`] to `(ffmpeg layout name, channel count)`.
/// `None` channel count means "let ffmpeg infer from the layout name".
fn layout_name_channels(layout: &ChannelLayout) -> VideoResult<(String, Option<u32>)> {
    match layout {
        ChannelLayout::Mono => Ok(("mono".to_string(), Some(1))),
        ChannelLayout::Stereo => Ok(("stereo".to_string(), Some(2))),
        ChannelLayout::Surround51 => Ok(("5.1".to_string(), Some(6))),
        ChannelLayout::Surround71 => Ok(("7.1".to_string(), Some(8))),
        ChannelLayout::Custom(raw) => {
            let name = sanitize_layout_name(raw)?;
            Ok((name.clone(), layout_channel_count(&name)))
        }
    }
}

/// Validate a custom layout name: ASCII alphanumerics plus `.` and
/// `_` only, non-empty, ≤64 chars. Anything else is rejected rather
/// than silently mangled.
fn sanitize_layout_name(raw: &str) -> VideoResult<String> {
    let trimmed = raw.trim();
    if trimmed.is_empty() || trimmed.len() > 64 {
        return Err(VideoError::InvalidInput(format!(
            "channel layout name must be 1-64 characters, got {raw:?}"
        )));
    }
    if !trimmed
        .chars()
        .all(|c| c.is_ascii_alphanumeric() || c == '.' || c == '_')
    {
        return Err(VideoError::InvalidInput(format!(
            "channel layout name {raw:?} may only contain ASCII letters, digits, '.', '_'"
        )));
    }
    Ok(trimmed.to_string())
}

/// Channel count for ffmpeg's standard named layouts, used to emit
/// `-ac` alongside `-channel_layout`. `None` for names outside the
/// common set — ffmpeg resolves those from the layout itself.
fn layout_channel_count(name: &str) -> Option<u32> {
    Some(match name {
        "mono" => 1,
        "stereo" => 2,
        "2.1" => 3,
        "3.0" => 3,
        "quad" => 4,
        "4.0" => 4,
        "5.0" => 5,
        "5.1" => 6,
        "6.1" => 7,
        "7.1" => 8,
        "octagonal" => 8,
        _ => return None,
    })
}

/// Convert the audio stream to `target_format` — an audio→audio
/// transcode across [`AUDIO_FORMATS`] (`mp3`, `wav`, `flac`, `ogg`,
/// `opus`, `m4a`, `aac`). Video is always dropped (`-vn`).
///
/// The target extension *and* its encoder are verified up front via
/// `ffmpeg -encoders`; unsupported targets yield
/// [`VideoError::InvalidInput`] listing the formats this build can
/// actually produce.
pub(crate) fn audio_convert(
    input: &[u8],
    target_format: &str,
    limits: &EngineLimits,
) -> VideoResult<Vec<u8>> {
    let spec = checked_format(target_format, "audio_convert")?;
    let _info = checked_input(input, limits)?;
    let program = find_tool(FFMPEG)?;
    let dir = TempDir::new("audio-convert")?;
    let input_path = write_input(&dir, "input.bin", input)?;
    let out_path = dir.file(&format!("out.{}", spec.ext));

    let mut args = vec![
        "-y".to_string(),
        "-v".to_string(),
        "error".to_string(),
        "-i".to_string(),
        input_path.to_string_lossy().into_owned(),
        "-vn".to_string(),
    ];
    args.extend(spec.codec_args.map(str::to_string));
    args.push(out_path.to_string_lossy().into_owned());

    run_ffmpeg(&program, &args)?;
    read_output(&out_path)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::crates::video_engine::ops::{FFPROBE, find_tool};
    use std::fs;

    fn tools_available() -> bool {
        find_tool(FFMPEG).is_ok() && find_tool(FFPROBE).is_ok()
    }

    /// 2s mono sine WAV — long enough that tempo/loop ratios are
    /// distinguishable within probe tolerance.
    fn make_wav_fixture() -> Option<Vec<u8>> {
        let program = find_tool(FFMPEG).ok()?;
        let dir = TempDir::new("at-test-wav").ok()?;
        let out = dir.file("tone.wav");
        let args = vec![
            "-y".to_string(),
            "-v".to_string(),
            "error".to_string(),
            "-f".to_string(),
            "lavfi".to_string(),
            "-i".to_string(),
            "sine=frequency=440:duration=2".to_string(),
            "-c:a".to_string(),
            "pcm_s16le".to_string(),
            out.to_string_lossy().into_owned(),
        ];
        let output = run_tool(FFMPEG, &program, &args, COMMAND_TIMEOUT).ok()?;
        if !output.status.success() {
            return None;
        }
        fs::read(out).ok()
    }

    /// 2s mono MP3 via lavfi (needs libmp3lame); `None` when absent.
    fn make_mp3_fixture() -> Option<Vec<u8>> {
        let program = find_tool(FFMPEG).ok()?;
        if !encoder_available("libmp3lame") {
            return None;
        }
        let dir = TempDir::new("at-test-mp3").ok()?;
        let out = dir.file("tone.mp3");
        let args = vec![
            "-y".to_string(),
            "-v".to_string(),
            "error".to_string(),
            "-f".to_string(),
            "lavfi".to_string(),
            "-i".to_string(),
            "sine=frequency=660:duration=2".to_string(),
            "-c:a".to_string(),
            "libmp3lame".to_string(),
            out.to_string_lossy().into_owned(),
        ];
        let output = run_tool(FFMPEG, &program, &args, COMMAND_TIMEOUT).ok()?;
        if !output.status.success() {
            return None;
        }
        fs::read(out).ok()
    }

    fn duration_of(bytes: &[u8]) -> Option<f64> {
        probe(bytes).ok().and_then(|i| i.duration_secs)
    }

    fn audio_stream_channels(bytes: &[u8]) -> Option<u32> {
        probe(bytes)
            .ok()?
            .streams
            .into_iter()
            .find(|s| s.codec_type == "audio")
            .and_then(|s| s.channels)
    }

    fn assert_duration_near(actual: f64, expected: f64, what: &str) {
        assert!(
            (actual - expected).abs() <= 0.25,
            "{what}: expected ~{expected}s, got {actual}s"
        );
    }

    #[test]
    fn reverse_preserves_duration() {
        if !tools_available() {
            return;
        }
        let Some(wav) = make_wav_fixture() else {
            return;
        };
        let out = audio_reverse(&wav, &EngineLimits::default()).unwrap();
        let dur = duration_of(&out).expect("probe reversed output");
        assert_duration_near(dur, 2.0, "audio_reverse");
    }

    #[test]
    fn loop_count_one_doubles_duration() {
        if !tools_available() {
            return;
        }
        let Some(wav) = make_wav_fixture() else {
            return;
        };
        let out = audio_loop(&wav, LoopParams { count: 1 }, &EngineLimits::default()).unwrap();
        let dur = duration_of(&out).expect("probe looped output");
        assert_duration_near(dur, 4.0, "audio_loop(count=1)");
    }

    #[test]
    fn speed_factor_two_halves_duration() {
        if !tools_available() {
            return;
        }
        let Some(wav) = make_wav_fixture() else {
            return;
        };
        let out = audio_speed(
            &wav,
            AudioSpeedParams { factor: 2.0 },
            &EngineLimits::default(),
        )
        .unwrap();
        let dur = duration_of(&out).expect("probe sped output");
        assert_duration_near(dur, 1.0, "audio_speed(2.0)");
    }

    #[test]
    fn speed_rejects_out_of_range_factor() {
        if !tools_available() {
            return;
        }
        let Some(wav) = make_wav_fixture() else {
            return;
        };
        assert!(matches!(
            audio_speed(
                &wav,
                AudioSpeedParams { factor: 0.1 },
                &EngineLimits::default()
            ),
            Err(VideoError::InvalidInput(_))
        ));
    }

    #[test]
    fn trim_500_to_1500_is_one_second() {
        if !tools_available() {
            return;
        }
        let Some(wav) = make_wav_fixture() else {
            return;
        };
        let out = audio_trim(&wav, 500, 1500, "wav", &EngineLimits::default()).unwrap();
        let dur = duration_of(&out).expect("probe trimmed output");
        assert_duration_near(dur, 1.0, "audio_trim(500,1500)");
    }

    #[test]
    fn channels_stereo_produces_two_channels() {
        if !tools_available() {
            return;
        }
        let Some(wav) = make_wav_fixture() else {
            return;
        };
        let out = audio_channels(&wav, &ChannelLayout::Stereo, &EngineLimits::default()).unwrap();
        assert_eq!(audio_stream_channels(&out), Some(2));
    }

    #[test]
    fn channels_rejects_bad_custom_layout() {
        if !tools_available() {
            return;
        }
        let Some(wav) = make_wav_fixture() else {
            return;
        };
        assert!(matches!(
            audio_channels(
                &wav,
                &ChannelLayout::Custom("bad;layout".to_string()),
                &EngineLimits::default()
            ),
            Err(VideoError::InvalidInput(_))
        ));
    }

    #[test]
    fn convert_wav_to_mp3_has_mp3_magic() {
        if !tools_available() || !encoder_available("libmp3lame") {
            return;
        }
        let Some(wav) = make_wav_fixture() else {
            return;
        };
        let out = audio_convert(&wav, "mp3", &EngineLimits::default()).unwrap();
        // MP3 frames start with an ID3 tag or a bare MPEG frame sync
        // (0xFF 0xFB / 0xFF 0xF3 / 0xFF 0xF2).
        let is_mp3 =
            out.starts_with(b"ID3") || (out.len() > 1 && out[0] == 0xFF && (out[1] & 0xE0) == 0xE0);
        assert!(
            is_mp3,
            "expected MP3 magic bytes, got {:02x?}",
            &out[..4.min(out.len())]
        );
        let dur = duration_of(&out).expect("probe converted mp3");
        assert_duration_near(dur, 2.0, "audio_convert wav→mp3");
    }

    #[test]
    fn convert_mp3_to_wav_roundtrip() {
        if !tools_available() {
            return;
        }
        let Some(mp3) = make_mp3_fixture() else {
            return;
        };
        let out = audio_convert(&mp3, "wav", &EngineLimits::default()).unwrap();
        assert!(out.starts_with(b"RIFF"), "expected WAV magic bytes");
        let dur = duration_of(&out).expect("probe converted wav");
        assert_duration_near(dur, 2.0, "audio_convert mp3→wav");
    }

    #[test]
    fn convert_rejects_unsupported_format() {
        if !tools_available() {
            return;
        }
        let Some(wav) = make_wav_fixture() else {
            return;
        };
        assert!(matches!(
            audio_convert(&wav, "exe", &EngineLimits::default()),
            Err(VideoError::InvalidInput(_))
        ));
    }
}
