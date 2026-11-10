//! Side-by-side / top-bottom split-screen compositing of two videos.
//!
//! [`split_screen`] takes exactly two video inputs and produces one
//! output where they play simultaneously: `Horizontal` places them
//! left/right via ffmpeg's `hstack` filter, `Vertical` stacks them
//! top/bottom via `vstack`.
//!
//! Both inputs are normalized inside a single `filter_complex`:
//!
//! 1. `fps=` forces a common frame rate (first input's probed rate,
//!    else 30 fps).
//! 2. `scale=` + `pad=` fit each input into an equal half-canvas.
//!    With [`SplitScreenOptions::preserve_aspect`] (default) each
//!    input is scaled down to fit the half box keeping its aspect
//!    ratio and padded with [`SplitScreenOptions::pad_color`];
//!    otherwise it is stretched to fill the box exactly.
//!    The half box is `max(w0, w1) x max(h0, h1)` so identical
//!    inputs keep their native size.
//! 3. `setsar=1` normalizes sample aspect ratio and `format=yuv420p`
//!    gives the encoder a safe pixel format.
//!
//! `hstack`/`vstack` are framesync filters: the output ends when the
//! *shorter* input ends, so the longer input is truncated, not
//! looped or frozen.
//!
//! Audio: when **both** inputs have audio they are mixed with
//! `amix=duration=shortest` (matching the framesync-shortest video
//! length). When **only one** input has audio, that stream alone is
//! resampled and carried through — there is no silence padding for
//! the missing side. When neither has audio the output is video-only.
//!
//! Encoding: the video codec defaults to the first available of
//! `libopenh264`, `h264`, `mpeg4` — **libx264 is intentionally not
//! in the default list** (GPL/encode-permission concerns for
//! redistribution); a caller may still request it explicitly via
//! [`SplitScreenOptions::video_codec`], which is capability-checked.
//! Audio defaults to AAC (`libopus` is tried for `.webm` output when
//! AAC is absent).

use std::ffi::OsString;
use std::path::Path;

use serde::{Deserialize, Serialize};

use crate::crates::multimedia_engine::error::{MediaError, MediaResult};
use crate::crates::multimedia_engine::model::{MediaLimits, MediaOutput};
use crate::crates::multimedia_engine::probe::{MediaStream, ProbeReport};
use crate::crates::multimedia_engine::runtime::discovery::{Runtime, RuntimeCapabilities};
use crate::crates::multimedia_engine::runtime::process;
use crate::crates::multimedia_engine::runtime::tempdir::TempDir;

/// Arrangement of the two inputs in the output frame.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
pub enum SplitScreenLayout {
    /// Left input on the left, right input on the right (`hstack`).
    #[default]
    Horizontal,
    /// Left input on top, right input on the bottom (`vstack`).
    Vertical,
}

/// Options for [`split_screen`].
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SplitScreenOptions {
    /// Side-by-side or top/bottom arrangement.
    #[serde(default)]
    pub layout: SplitScreenLayout,
    /// Output container extension without a dot (`"mp4"`, `"mkv"`,
    /// `"webm"`, ...). Default `"mp4"`.
    #[serde(default = "default_output_format")]
    pub output_format: String,
    /// Explicit video encoder name. `None` picks the first available
    /// of `libopenh264`/`h264`/`mpeg4` (libx264 excluded by default).
    /// Any requested name is verified against this build's encoders —
    /// `libx264` works only if explicitly requested *and* present.
    pub video_codec: Option<String>,
    /// Explicit audio encoder name. `None` uses `aac` (`libopus` for
    /// `.webm` when AAC is missing). Ignored when neither input has
    /// audio.
    pub audio_codec: Option<String>,
    /// Scale each input to fit its half-canvas keeping aspect ratio
    /// and pad the remainder with `pad_color`. `false` stretches each
    /// input to fill its half exactly (may distort). Default `true`.
    #[serde(default = "default_preserve_aspect")]
    pub preserve_aspect: bool,
    /// ffmpeg color name or `0xRRGGBB` used for padding when
    /// `preserve_aspect` is set. Default `"black"`. Restricted to a
    /// safe character set to prevent filter-graph injection.
    #[serde(default = "default_pad_color")]
    pub pad_color: String,
}

fn default_output_format() -> String {
    "mp4".to_string()
}
fn default_preserve_aspect() -> bool {
    true
}
fn default_pad_color() -> String {
    "black".to_string()
}

impl Default for SplitScreenOptions {
    fn default() -> Self {
        Self {
            layout: SplitScreenLayout::default(),
            output_format: default_output_format(),
            video_codec: None,
            audio_codec: None,
            preserve_aspect: default_preserve_aspect(),
            pad_color: default_pad_color(),
        }
    }
}

/// H.264-family encoders acceptable for split-screen output, in
/// preference order. libx264 is deliberately absent — see module docs.
const VIDEO_ENCODERS: &[&str] = &["libopenh264", "h264", "mpeg4"];

/// First available default video encoder; for `.webm` output prefer
/// VP8/VP9 since the H.264 fallbacks are rarely useful there.
fn pick_video_encoder<'a>(caps: &'a RuntimeCapabilities, ext: &str) -> Option<&'a str> {
    if ext == "webm" {
        for e in ["libvpx-vp9", "libvpx"] {
            if caps.has_encoder(e) {
                return Some(e);
            }
        }
    }
    VIDEO_ENCODERS.iter().copied().find(|e| caps.has_encoder(e))
}

/// Pick the audio encoder: explicit option wins, then `aac`, then
/// `libopus` as a last resort for `.webm`. Returns an owned `String`
/// because a requested name borrows from `options`, not `caps`.
fn pick_audio_encoder(
    caps: &RuntimeCapabilities,
    requested: Option<&str>,
    ext: &str,
) -> Option<String> {
    if let Some(name) = requested {
        return caps.has_encoder(name).then(|| name.to_string());
    }
    if caps.has_encoder("aac") {
        return Some("aac".to_string());
    }
    if ext == "webm" && caps.has_encoder("libopus") {
        return Some("libopus".to_string());
    }
    None
}

/// Compose `left` and `right` videos into a single split-screen file.
///
/// See the module docs for normalization, audio and duration
/// semantics. Both inputs must contain a video stream.
///
/// # Errors
///
/// - [`MediaError::InvalidInput`] — empty inputs, missing/undecodable
///   video streams, zero or absurd probed dimensions, unsafe
///   `pad_color`.
/// - [`MediaError::LimitExceeded`] — inputs exceed
///   `limits.max_input_bytes`, output exceeds `max_output_bytes`.
/// - [`MediaError::Unsupported`] — required filter (`hstack`/`vstack`/
///   `scale`/`pad`/`fps`/`setsar`/`format`/`aresample`/`aformat`/
///   `amix`), encoder, or muxer missing from this FFmpeg build.
/// - [`MediaError::ToolFailed`] — ffmpeg/ffprobe failure.
pub fn split_screen(
    left: &[u8],
    right: &[u8],
    options: &SplitScreenOptions,
    limits: &MediaLimits,
    rt: &Runtime,
) -> MediaResult<MediaOutput> {
    for (which, data) in [("left", left), ("right", right)] {
        if data.is_empty() {
            return Err(MediaError::InvalidInput(format!("{which} input is empty")));
        }
        if let Some(max) = limits.max_input_bytes
            && (data.len() as u64) > max
        {
            return Err(MediaError::LimitExceeded(format!(
                "{which} input is {} bytes, exceeding max_input_bytes {max}",
                data.len()
            )));
        }
    }

    let out_ext = options
        .output_format
        .trim()
        .trim_start_matches('.')
        .to_ascii_lowercase();
    if out_ext.is_empty()
        || out_ext.len() > 10
        || !out_ext.chars().all(|c| c.is_ascii_alphanumeric())
    {
        return Err(MediaError::InvalidInput(format!(
            "invalid output_format {:?}",
            options.output_format
        )));
    }

    // pad_color lands inside a filtergraph string; restrict it to
    // characters that cannot terminate the pad filter or inject new
    // filters (no ',', ';', '[', ']', '(', ')', '\'', ':').
    if !options
        .pad_color
        .chars()
        .all(|c| c.is_ascii_alphanumeric() || matches!(c, '#' | 'x' | 'X' | '_' | '-'))
    {
        return Err(MediaError::InvalidInput(format!(
            "pad_color {:?} contains unsafe characters",
            options.pad_color
        )));
    }

    // Capability checks up front — never invoke filters/encoders this
    // build lacks.
    let caps = rt.capabilities()?;
    let stack_filter = match options.layout {
        SplitScreenLayout::Horizontal => "hstack",
        SplitScreenLayout::Vertical => "vstack",
    };
    for name in [stack_filter, "scale", "pad", "fps", "setsar", "format"] {
        if !caps.has_filter(name) {
            return Err(MediaError::Unsupported(format!(
                "ffmpeg build lacks the '{name}' filter required for split-screen"
            )));
        }
    }
    let muxer = muxer_name(&out_ext);
    if !caps.has_muxer(muxer) {
        return Err(MediaError::Unsupported(format!(
            "ffmpeg build lacks the '{muxer}' muxer for .{out_ext} output"
        )));
    }
    let video_encoder = match options.video_codec.as_deref().map(str::trim) {
        Some(name) if !name.is_empty() => {
            if !caps.has_encoder(name) {
                return Err(MediaError::Unsupported(format!(
                    "video encoder {name:?} is not available in this FFmpeg build"
                )));
            }
            name.to_string()
        }
        _ => pick_video_encoder(caps, &out_ext)
            .ok_or_else(|| {
                MediaError::Unsupported(
                    "ffmpeg build lacks a usable video encoder (libopenh264/h264/mpeg4, \
                     or libvpx for webm) required for split-screen"
                        .to_string(),
                )
            })?
            .to_string(),
    };

    let tmp = TempDir::new("split-screen")?;
    let left_path = tmp.write(&format!("left.{}", sniff_ext(left)), left)?;
    let right_path = tmp.write(&format!("right.{}", sniff_ext(right)), right)?;

    // Probe both inputs: dimensions define the half-canvas, frame rate
    // the common fps, stream presence decides the audio path.
    let left_report = probe_file(&left_path, rt, limits)?;
    let right_report = probe_file(&right_path, rt, limits)?;
    let left_video = video_stream(&left_report)
        .ok_or_else(|| MediaError::InvalidInput("left input has no video stream".into()))?;
    let right_video = video_stream(&right_report)
        .ok_or_else(|| MediaError::InvalidInput("right input has no video stream".into()))?;
    let (lw, lh) = dims(left_video, "left")?;
    let (rw, rh) = dims(right_video, "right")?;

    // Half-canvas: each input gets an equal box of max(w) x max(h),
    // so two identical inputs keep native resolution and the output
    // is exactly 2x wide (horizontal) or 2x tall (vertical). Rounded
    // up to even — yuv420p/H.264 encoders reject odd dimensions.
    let half_w = even_up(lw.max(rw));
    let half_h = even_up(lh.max(rh));

    // Common frame rate: prefer the left input's rate (matches
    // concat's "first input wins" convention), then the right's,
    // else 30 fps.
    let fps = [left_video, right_video]
        .iter()
        .filter_map(|v| v.r_frame_rate.as_deref().or(v.avg_frame_rate.as_deref()))
        .find(|s| valid_rate(s))
        .unwrap_or("30")
        .to_string();

    let left_audio = audio_stream(&left_report).is_some();
    let right_audio = audio_stream(&right_report).is_some();

    // Audio path: both → amix; one → passthrough resample; none →
    // video-only. aresample/aformat/amix are only required when an
    // audio stream will actually be produced.
    let audio_mode = match (left_audio, right_audio) {
        (true, true) => Some(AudioMode::Mix),
        (true, false) => Some(AudioMode::LeftOnly),
        (false, true) => Some(AudioMode::RightOnly),
        (false, false) => None,
    };
    if audio_mode.is_some() {
        for name in ["aresample", "aformat"] {
            if !caps.has_filter(name) {
                return Err(MediaError::Unsupported(format!(
                    "ffmpeg build lacks the '{name}' filter required for split-screen audio"
                )));
            }
        }
        if matches!(audio_mode, Some(AudioMode::Mix)) && !caps.has_filter("amix") {
            return Err(MediaError::Unsupported(
                "ffmpeg build lacks the 'amix' filter required to mix both inputs' audio"
                    .to_string(),
            ));
        }
        let audio_encoder = pick_audio_encoder(
            caps,
            options
                .audio_codec
                .as_deref()
                .map(str::trim)
                .filter(|s| !s.is_empty()),
            &out_ext,
        )
        .ok_or_else(|| {
            MediaError::Unsupported(
                "ffmpeg build lacks a usable audio encoder (aac, or libopus for webm)".to_string(),
            )
        })?;
        return run_ffmpeg(
            &left_path,
            &right_path,
            &tmp,
            options,
            &out_ext,
            stack_filter,
            (half_w, half_h),
            &fps,
            &video_encoder,
            &audio_encoder,
            audio_mode.expect("checked"),
            limits,
            rt,
        );
    }

    run_ffmpeg(
        &left_path,
        &right_path,
        &tmp,
        options,
        &out_ext,
        stack_filter,
        (half_w, half_h),
        &fps,
        &video_encoder,
        "",
        audio_mode.unwrap_or(AudioMode::None_),
        limits,
        rt,
    )
}

/// How input audio streams are combined (see module docs).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum AudioMode {
    /// No audio on either input — video-only output.
    None_,
    /// Both inputs have audio — mixed via `amix`.
    Mix,
    /// Only the left input has audio.
    LeftOnly,
    /// Only the right input has audio.
    RightOnly,
}

/// Run the actual ffmpeg pass and read back the produced file.
#[allow(clippy::too_many_arguments)]
fn run_ffmpeg(
    left_path: &Path,
    right_path: &Path,
    tmp: &TempDir,
    options: &SplitScreenOptions,
    out_ext: &str,
    stack_filter: &str,
    (half_w, half_h): (u32, u32),
    fps: &str,
    video_encoder: &str,
    audio_encoder: &str,
    audio_mode: AudioMode,
    limits: &MediaLimits,
    rt: &Runtime,
) -> MediaResult<MediaOutput> {
    // Per-input video normalization. `force_original_aspect_ratio=
    // decrease` only makes sense with preserve_aspect; the stretch
    // path omits it so scale fills the half box exactly. pad centers
    // the (possibly smaller) scaled frame in the half box.
    let scale_chain = |idx: usize| -> String {
        let mut chain = format!("[{idx}:v]fps={fps},");
        if options.preserve_aspect {
            chain.push_str(&format!(
                "scale={half_w}:{half_h}:force_original_aspect_ratio=decrease,"
            ));
        } else {
            chain.push_str(&format!("scale={half_w}:{half_h},"));
        }
        chain.push_str(&format!(
            "pad={half_w}:{half_h}:(ow-iw)/2:(oh-ih)/2:{},setsar=1,format=yuv420p[v{idx}]",
            options.pad_color
        ));
        chain
    };

    let mut graph = String::new();
    graph.push_str(&scale_chain(0));
    graph.push(';');
    graph.push_str(&scale_chain(1));
    graph.push_str(&format!(";[v0][v1]{stack_filter}=inputs=2[vout]"));

    let (aout_label, audio_args): (&str, Vec<OsString>) = match audio_mode {
        AudioMode::None_ => ("", Vec::new()),
        AudioMode::Mix => {
            graph.push_str(
                ";[0:a]aresample=48000,aformat=sample_fmts=fltp:channel_layouts=stereo[a0]",
            );
            graph.push_str(
                ";[1:a]aresample=48000,aformat=sample_fmts=fltp:channel_layouts=stereo[a1]",
            );
            // duration=shortest matches the framesync-shortest video
            // length; dropout_transition=0 avoids fade artifacts.
            graph.push_str(";[a0][a1]amix=inputs=2:duration=shortest:dropout_transition=0[am]");
            (
                "[am]",
                vec![OsString::from("-c:a"), OsString::from(audio_encoder)],
            )
        }
        AudioMode::LeftOnly | AudioMode::RightOnly => {
            let idx = usize::from(matches!(audio_mode, AudioMode::RightOnly));
            graph.push_str(&format!(
                ";[{idx}:a]aresample=48000,aformat=sample_fmts=fltp:channel_layouts=stereo[aout]"
            ));
            (
                "[aout]",
                vec![OsString::from("-c:a"), OsString::from(audio_encoder)],
            )
        }
    };

    let out_name = format!("output.{out_ext}");
    let mut args: Vec<OsString> = vec![
        OsString::from("-hide_banner"),
        OsString::from("-y"),
        OsString::from("-i"),
        left_path.as_os_str().to_os_string(),
        OsString::from("-i"),
        right_path.as_os_str().to_os_string(),
        OsString::from("-filter_complex"),
        OsString::from(graph),
        OsString::from("-map"),
        OsString::from("[vout]"),
    ];
    if !aout_label.is_empty() {
        args.push(OsString::from("-map"));
        args.push(OsString::from(aout_label));
    }
    args.extend([
        OsString::from("-c:v"),
        OsString::from(video_encoder),
        OsString::from("-pix_fmt"),
        OsString::from("yuv420p"),
    ]);
    if video_encoder == "libx264" {
        args.extend([
            OsString::from("-preset"),
            OsString::from("veryfast"),
            OsString::from("-crf"),
            OsString::from("23"),
        ]);
    }
    if audio_mode != AudioMode::None_ {
        args.extend(audio_args);
        if audio_encoder == "aac" {
            args.extend([OsString::from("-b:a"), OsString::from("128k")]);
        }
    }
    args.push(tmp.file(&out_name).into_os_string());

    process::run_checked("ffmpeg", &rt.ffmpeg, &args, None, limits)?;

    let bytes = tmp.read_limited(&out_name, limits.max_output_bytes)?;
    if bytes.is_empty() {
        return Err(MediaError::ToolFailed {
            tool: "ffmpeg",
            status: "empty output".to_string(),
            detail: "split-screen produced a zero-byte file".to_string(),
        });
    }
    let mut output = MediaOutput::single(out_name, bytes);
    if let Some(primary) = output.primary.as_mut() {
        primary.content_type = content_type(out_ext).map(str::to_string);
    }
    Ok(output)
}

/// First video stream of a probe report.
fn video_stream(report: &ProbeReport) -> Option<&MediaStream> {
    report.streams.iter().find(|s| s.kind() == Some("video"))
}

/// First audio stream of a probe report.
fn audio_stream(report: &ProbeReport) -> Option<&MediaStream> {
    report.streams.iter().find(|s| s.kind() == Some("audio"))
}

/// Round `v` up to the next even number (yuv420p requirement).
fn even_up(v: u32) -> u32 {
    v.saturating_add(1) & !1
}

/// `(width, height)` of a probed video stream, rejecting missing or
/// absurd dimensions (a 0-dim or multi-8K frame would produce a broken
/// or runaway filter graph).
fn dims(stream: &MediaStream, which: &str) -> MediaResult<(u32, u32)> {
    let (w, h) = stream.width.zip(stream.height).ok_or_else(|| {
        MediaError::InvalidInput(format!("{which} input video dimensions unknown"))
    })?;
    if w == 0 || h == 0 || w > 8192 || h > 8192 {
        return Err(MediaError::InvalidInput(format!(
            "{which} input has implausible video dimensions {w}x{h}"
        )));
    }
    Ok((w, h))
}

/// Probe a media file with ffprobe, returning the parsed report.
fn probe_file(path: &Path, rt: &Runtime, limits: &MediaLimits) -> MediaResult<ProbeReport> {
    let mut args = process::args(&[
        "-v",
        "error",
        "-show_format",
        "-show_streams",
        "-of",
        "json",
    ]);
    args.push(path.as_os_str().to_os_string());
    let out = process::run_checked("ffprobe", &rt.ffprobe, &args, None, limits)?;
    serde_json::from_slice(&out.stdout)
        .map_err(|e| MediaError::InvalidInput(format!("ffprobe returned unparseable JSON: {e}")))
}

/// Best-effort container sniff for a temp input file extension.
fn sniff_ext(data: &[u8]) -> &'static str {
    if data.len() >= 4 {
        match &data[..4] {
            [0x1A, 0x45, 0xDF, 0xA3] => return "mkv",
            [b'O', b'g', b'g', b'S'] => return "ogg",
            [b'f', b'L', b'a', b'C'] => return "flac",
            [b'R', b'I', b'F', b'F'] => {
                if data.len() >= 12 && &data[8..12] == b"WAVE" {
                    return "wav";
                }
                if data.len() >= 12 && &data[8..12] == b"AVI " {
                    return "avi";
                }
                return "bin";
            }
            [b'I', b'D', b'3', _] => return "mp3",
            _ => {}
        }
        if data.len() >= 8 && &data[4..8] == b"ftyp" {
            return "mp4";
        }
    }
    "mp4"
}

/// Whether `s` looks like an ffprobe frame-rate string (`"30"`,
/// `"30000/1001"`) usable directly as an `fps=` filter value.
fn valid_rate(s: &str) -> bool {
    let (num, den) = s.split_once('/').unwrap_or((s, "1"));
    num.trim().parse::<f64>().is_ok() && den.trim().parse::<f64>().is_ok()
}

/// Map a file extension to the ffmpeg muxer name used for capability
/// checks. Note ffmpeg registers `mp4` and `mov` as *separate* muxers
/// (`-muxers` lists both) — there is no `mp4_mov` name.
fn muxer_name(ext: &str) -> &'static str {
    match ext {
        "mp4" | "m4v" => "mp4",
        "mov" => "mov",
        "mkv" => "matroska",
        "webm" => "webm",
        "avi" => "avi",
        "wav" => "wav",
        "mp3" => "mp3",
        "flac" => "flac",
        "ogg" => "ogg",
        "opus" => "opus",
        "m4a" => "ipod",
        "aac" => "adts",
        _ => "mp4",
    }
}

/// Best-effort MIME type for the output container.
fn content_type(ext: &str) -> Option<&'static str> {
    Some(match ext {
        "mp4" | "m4v" | "mov" => "video/mp4",
        "mkv" => "video/x-matroska",
        "webm" => "video/webm",
        "avi" => "video/x-msvideo",
        "mp3" => "audio/mpeg",
        "wav" => "audio/wav",
        "flac" => "audio/flac",
        "ogg" | "opus" => "audio/ogg",
        "m4a" | "aac" => "audio/mp4",
        _ => return None,
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::time::Duration;

    fn rt() -> Option<Runtime> {
        Runtime::discover().ok()
    }

    fn limits() -> MediaLimits {
        MediaLimits {
            timeout: Duration::from_secs(120),
            ..MediaLimits::default()
        }
    }

    /// Render a 1s 64x64@30 lavfi clip (testsrc + sine) to bytes.
    fn make_clip(dir: &TempDir, name: &str, freq: u32, rt: &Runtime) -> Vec<u8> {
        let path = dir.file(name);
        let caps = rt.capabilities().expect("capabilities");
        let video_enc = pick_video_encoder(&caps, "mp4").expect("no usable video encoder");
        let mut args: Vec<OsString> = [
            OsString::from("-hide_banner"),
            OsString::from("-y"),
            OsString::from("-f"),
            OsString::from("lavfi"),
            OsString::from("-i"),
            OsString::from("testsrc=duration=1:size=64x64:rate=30"),
            OsString::from("-f"),
            OsString::from("lavfi"),
            OsString::from("-i"),
            OsString::from(format!("sine=frequency={freq}:duration=1")),
            OsString::from("-c:v"),
            OsString::from(video_enc),
            OsString::from("-pix_fmt"),
            OsString::from("yuv420p"),
            OsString::from("-c:a"),
            OsString::from("aac"),
            OsString::from("-shortest"),
        ]
        .into();
        args.push(path.as_os_str().to_os_string());
        process::run_checked("ffmpeg", &rt.ffmpeg, &args, None, &limits())
            .expect("failed to generate test clip");
        std::fs::read(&path).expect("clip written")
    }

    /// Probe the output's primary video dimensions.
    fn output_dims(output: &MediaOutput, rt: &Runtime) -> (u32, u32) {
        let dir = TempDir::new("split-screen-test-out").unwrap();
        let name = "out.mp4";
        let bytes = &output.primary.as_ref().unwrap().bytes;
        dir.write(name, bytes).unwrap();
        let report = probe_file(&dir.file(name), rt, &limits()).unwrap();
        let v = video_stream(&report).expect("output has no video stream");
        dims(v, "output").unwrap()
    }

    #[test]
    fn horizontal_split_doubles_width() {
        let Some(rt) = rt() else { return }; // skip: no ffmpeg/ffprobe
        let caps = match rt.capabilities() {
            Ok(c) => c,
            Err(_) => return,
        };
        if pick_video_encoder(caps, "mp4").is_none()
            || !caps.has_filter("hstack")
            || !caps.has_muxer("mp4")
        {
            return; // skip: build lacks required pieces
        }
        let dir = TempDir::new("split-screen-test").unwrap();
        let left = make_clip(&dir, "left.mp4", 440, &rt);
        let right = make_clip(&dir, "right.mp4", 550, &rt);

        let out = split_screen(
            &left,
            &right,
            &SplitScreenOptions {
                layout: SplitScreenLayout::Horizontal,
                ..Default::default()
            },
            &limits(),
            &rt,
        )
        .expect("horizontal split-screen failed");

        let (w, h) = output_dims(&out, &rt);
        assert_eq!(h, 64, "expected height 64, got {w}x{h}");
        assert!(
            (w as i64 - 2 * h as i64).abs() <= 2,
            "expected width ~2x height (128), got {w}x{h}"
        );
    }

    #[test]
    fn rejects_empty_input() {
        let Some(rt) = rt() else { return };
        let err = split_screen(
            b"",
            b"",
            &SplitScreenOptions::default(),
            &MediaLimits::default(),
            &rt,
        )
        .unwrap_err();
        assert!(matches!(err, MediaError::InvalidInput(_)));
    }

    #[test]
    fn rejects_unsafe_pad_color() {
        let Some(rt) = rt() else { return };
        let opts = SplitScreenOptions {
            pad_color: "black,format=rgb24".to_string(),
            ..Default::default()
        };
        let err = split_screen(b"x", b"y", &opts, &MediaLimits::default(), &rt).unwrap_err();
        assert!(matches!(err, MediaError::InvalidInput(_)));
    }
}
