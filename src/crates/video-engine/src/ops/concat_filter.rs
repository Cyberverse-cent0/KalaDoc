//! Filter-based concatenation for heterogeneous media inputs.
//!
//! [`concat`](super::concat) uses the ffmpeg concat *demuxer*, which is
//! fast but requires every input to expose the same stream layout. This
//! module complements it with the concat *filter*: every input is
//! decoded, normalized to a common canvas/frame-rate/pixel-format and
//! common audio layout, then concatenated segment-by-segment.
//!
//! The filter path therefore accepts inputs with different codecs,
//! resolutions, frame rates, containers and even missing audio or
//! video streams. Missing counterparts are synthesized with lavfi
//! sources (`anullsrc` for silence, `color` for black video) so the
//! concat filter always sees the same stream shape per segment.

use crate::crates::core::engine::EngineLimits;
use crate::crates::video_engine::error::{VideoError, VideoResult};
use crate::crates::video_engine::model::VideoInfo;
use crate::crates::video_engine::ops::probe::probe;
use crate::crates::video_engine::ops::{
    COMMAND_TIMEOUT, FFMPEG, TempDir, check_input, check_limits, checked_ext, encoder_available,
    ext_for_format, find_tool, h264_aac_args, read_output, run_tool, tool_failure, write_input,
};

/// Inclusive input-count bounds for filter concat. The concat filter
/// graph grows linearly with `n`; 20 keeps command lines and decode
/// memory bounded for in-memory API use.
const MIN_INPUTS: usize = 2;
const MAX_INPUTS: usize = 20;

/// Containers that can carry the normalized video+audio output.
const VIDEO_FORMATS: &[&str] = &["mp4", "mkv", "mov", "avi", "webm"];

/// Containers/codecs accepted when every input is audio-only. `mp4`/
/// `mkv`/`mov`/`avi`/`webm` are included because they can legally carry
/// an audio-only elementary stream.
const AUDIO_FORMATS: &[&str] = &[
    "mp3", "wav", "aac", "m4a", "ogg", "opus", "flac", "mp4", "mkv", "mov", "avi", "webm",
];

/// Union used for the initial extension check; the mode-specific list
/// is enforced after probing decides whether video is present.
const ALL_FORMATS: &[&str] = &[
    "mp4", "mkv", "mov", "avi", "webm", "mp3", "wav", "aac", "m4a", "ogg", "opus", "flac",
];

/// Normalized audio target shared by every concat segment.
#[derive(Debug, Clone)]
struct AudioTarget {
    sample_rate: u32,
    channel_layout: String,
}

/// A lavfi input appended after the real inputs to supply a missing
/// audio or video counterpart for one concat segment.
struct SyntheticInput {
    /// lavfi source expression, e.g. `anullsrc=r=48000:cl=stereo`.
    source: String,
    /// Duration in seconds used for the lavfi `-t` input bound.
    duration_secs: f64,
}

/// Probe and limit-check every input before any ffmpeg invocation.
fn probe_all(inputs: &[Vec<u8>], limits: &EngineLimits) -> VideoResult<Vec<VideoInfo>> {
    if inputs.len() < MIN_INPUTS {
        return Err(VideoError::InvalidInput(format!(
            "concat_filter requires at least {MIN_INPUTS} inputs, got {}",
            inputs.len()
        )));
    }
    if inputs.len() > MAX_INPUTS {
        return Err(VideoError::LimitExceeded(format!(
            "concat_filter accepts at most {MAX_INPUTS} inputs, got {}",
            inputs.len()
        )));
    }

    let mut infos = Vec::with_capacity(inputs.len());
    for input in inputs {
        let len = check_input(input)?;
        let info = probe(input)?;
        check_limits(&info, len, limits, false)?;
        if !info.has_video && !info.has_audio {
            return Err(VideoError::InvalidInput(
                "concat_filter input has neither video nor audio streams".to_string(),
            ));
        }
        infos.push(info);
    }

    // `check_limits` bounds each input individually; concat's documented
    // contract bounds the *sum*. Only enforce when every duration is
    // known — a missing duration must not silently bypass the limit,
    // but it also cannot be summed honestly.
    if let Some(max) = limits.max_duration_secs {
        let mut total = 0.0;
        let mut all_known = true;
        for info in &infos {
            match info.duration_secs {
                Some(d) => total += d,
                None => all_known = false,
            }
        }
        if all_known && total > max as f64 {
            return Err(VideoError::LimitExceeded(format!(
                "combined duration {total:.3}s exceeds max_duration_secs {max}"
            )));
        }
    }

    Ok(infos)
}

/// Pick the audio format every segment is normalized to: the first
/// audio-bearing input's parameters, falling back to 48 kHz stereo
/// when ffprobe did not report usable values.
fn audio_target(infos: &[VideoInfo]) -> AudioTarget {
    for info in infos {
        if !info.has_audio {
            continue;
        }
        let stream = info.streams.iter().find(|s| s.codec_type == "audio");
        let sample_rate = stream
            .and_then(|s| s.sample_rate)
            .filter(|r| *r > 0)
            .unwrap_or(48_000);
        let channel_layout = stream
            .and_then(|s| s.channel_layout.as_deref())
            .and_then(sanitize_layout)
            .or_else(|| stream.and_then(|s| s.channels).map(layout_for_channels))
            .unwrap_or_else(|| "stereo".to_string());
        return AudioTarget {
            sample_rate,
            channel_layout,
        };
    }
    AudioTarget {
        sample_rate: 48_000,
        channel_layout: "stereo".to_string(),
    }
}

/// Accept only layout names that are safe inside a filtergraph option.
/// Quirky layouts containing parentheses/spaces (e.g. `5.1(side)`)
/// fall back to the channel-count-derived name.
fn sanitize_layout(layout: &str) -> Option<String> {
    let trimmed = layout.trim();
    if trimmed.is_empty()
        || !trimmed
            .chars()
            .all(|c| c.is_ascii_alphanumeric() || matches!(c, '.' | '_' | '-'))
    {
        return None;
    }
    Some(trimmed.to_string())
}

/// Best-effort standard layout name for an untagged channel count.
fn layout_for_channels(channels: u32) -> String {
    match channels {
        1 => "mono".to_string(),
        2 => "stereo".to_string(),
        4 => "quad".to_string(),
        6 => "5.1".to_string(),
        8 => "7.1".to_string(),
        _ => "stereo".to_string(),
    }
}

/// Format an ffmpeg `fps` filter value. Six decimals is enough for
/// common rates (30000/1001 → 29.970030) without scientific notation.
fn fps_arg(fps: f64) -> String {
    let mut s = format!("{fps:.6}");
    while s.contains('.') && s.ends_with('0') {
        s.pop();
    }
    if s.ends_with('.') {
        s.pop();
    }
    s
}

/// Duration to bound a synthesized counterpart stream. Prefers the
/// probed container duration, then the relevant stream duration.
fn counterpart_duration(info: &VideoInfo, missing_video: bool) -> Option<f64> {
    if let Some(d) = info.duration_secs {
        return Some(d);
    }
    let wanted = if missing_video { "audio" } else { "video" };
    info.streams
        .iter()
        .filter(|s| s.codec_type == wanted && !s.attached_pic)
        .filter_map(|s| s.duration_secs)
        .reduce(f64::max)
}

/// Encode arguments for the normalized A/V output. `webm` cannot carry
/// H.264/AAC, so it uses the same VP9/Opus profile as `transcode`;
/// every other supported container uses H.264+AAC.
fn av_encode_args(ext: &str) -> VideoResult<Vec<String>> {
    if ext == "webm" {
        if !encoder_available("libvpx-vp9") || !encoder_available("libopus") {
            return Err(VideoError::ExternalTool {
                tool: FFMPEG.to_string(),
                detail: "webm concat output requires the libvpx-vp9 and libopus \
                         encoders, which this ffmpeg build does not provide"
                    .to_string(),
            });
        }
        return Ok(vec![
            "-c:v".into(),
            "libvpx-vp9".into(),
            "-deadline".into(),
            "good".into(),
            "-cpu-used".into(),
            "8".into(),
            "-c:a".into(),
            "libopus".into(),
        ]);
    }
    h264_aac_args()
}

/// Codec arguments for an audio-only output container.
fn audio_encode_args(ext: &str) -> VideoResult<Vec<String>> {
    let (codec, required) = match ext {
        "wav" | "avi" => ("pcm_s16le", None),
        "mp3" => ("libmp3lame", Some("libmp3lame")),
        "ogg" => ("libvorbis", Some("libvorbis")),
        "opus" | "webm" => ("libopus", Some("libopus")),
        "flac" => ("flac", Some("flac")),
        // aac/m4a/mp4/mkv/mov all carry AAC in this engine's profile.
        _ => ("aac", Some("aac")),
    };
    if let Some(name) = required {
        if !encoder_available(name) {
            return Err(VideoError::ExternalTool {
                tool: FFMPEG.to_string(),
                detail: format!(
                    "{ext} concat output requires the {name} encoder, which this \
                     ffmpeg build does not provide"
                ),
            });
        }
    }
    Ok(vec!["-c:a".into(), codec.to_string()])
}

/// Run ffmpeg with already-built args; non-zero exits become
/// [`tool_failure`] errors.
fn run_ffmpeg(program: &std::path::Path, args: &[String]) -> VideoResult<()> {
    let output = run_tool(FFMPEG, program, args, COMMAND_TIMEOUT)?;
    if !output.status.success() {
        return Err(tool_failure(FFMPEG, &output));
    }
    Ok(())
}

/// Shared implementation behind [`concat_hetero`] and
/// [`concat_mixed_av`]. `limits` is `EngineLimits::default()` for the
/// mixed-AV convenience entry point.
fn concat_impl(inputs: &[Vec<u8>], format: &str, limits: &EngineLimits) -> VideoResult<Vec<u8>> {
    let ext = checked_ext(format, ALL_FORMATS, "concat_filter")?;
    let infos = probe_all(inputs, limits)?;
    let program = find_tool(FFMPEG)?;

    let any_video = infos.iter().any(|i| i.has_video);
    let any_audio = infos.iter().any(|i| i.has_audio);

    if !any_video {
        if !AUDIO_FORMATS.contains(&ext.as_str()) {
            return Err(VideoError::InvalidInput(format!(
                "audio-only concat cannot use video-only target format '{ext}'"
            )));
        }
        return concat_audio_only(&program, inputs, &infos, &ext);
    }

    if !VIDEO_FORMATS.contains(&ext.as_str()) {
        return Err(VideoError::InvalidInput(format!(
            "video concat cannot use audio-only target format '{ext}'"
        )));
    }
    concat_av(&program, inputs, &infos, &ext, any_audio)
}

/// Concatenate inputs that contain at least one video stream.
///
/// Every segment is normalized to the maximum input width/height,
/// maximum frame rate and `yuv420p`/square-pixel video, plus the shared
/// audio target. Inputs lacking a counterpart stream get a bounded
/// lavfi source so `concat=n=N:v=1:a=1` (or `a=0` when no input has
/// audio) always sees a uniform segment shape.
fn concat_av(
    program: &std::path::Path,
    inputs: &[Vec<u8>],
    infos: &[VideoInfo],
    ext: &str,
    include_audio: bool,
) -> VideoResult<Vec<u8>> {
    // Canvas = max width/height across *video-bearing* inputs; audio-only
    // inputs do not contribute geometry.
    let mut canvas_w = 0u32;
    let mut canvas_h = 0u32;
    let mut target_fps: Option<f64> = None;
    for info in infos.iter().filter(|i| i.has_video) {
        let w = info.width.ok_or_else(|| {
            VideoError::InvalidInput("video input is missing probed width".to_string())
        })?;
        let h = info.height.ok_or_else(|| {
            VideoError::InvalidInput("video input is missing probed height".to_string())
        })?;
        canvas_w = canvas_w.max(w);
        canvas_h = canvas_h.max(h);
        if let Some(fps) = info.fps {
            if fps.is_finite() && fps > 0.0 {
                target_fps = Some(target_fps.map_or(fps, |cur: f64| cur.max(fps)));
            }
        }
    }
    if canvas_w == 0 || canvas_h == 0 {
        return Err(VideoError::InvalidInput(
            "could not determine a video canvas from the inputs".to_string(),
        ));
    }
    let fps = target_fps.ok_or_else(|| {
        VideoError::InvalidInput("could not determine a frame rate from the inputs".to_string())
    })?;
    let fps = fps_arg(fps);
    let audio = include_audio.then(|| audio_target(infos));

    let dir = TempDir::new("concat-filter")?;

    // Write real inputs with a sniffable extension, then reserve input
    // indices for synthetic lavfi sources appended after them.
    let mut input_paths = Vec::with_capacity(inputs.len());
    for (idx, input) in inputs.iter().enumerate() {
        let in_ext = infos[idx]
            .format_name
            .as_deref()
            .and_then(ext_for_format)
            .unwrap_or("bin");
        input_paths.push(write_input(&dir, &format!("in{idx}.{in_ext}"), input)?);
    }

    let mut synthetic: Vec<SyntheticInput> = Vec::new();
    let mut filter_parts: Vec<String> = Vec::new();
    let mut concat_pads: Vec<String> = Vec::new();

    for (idx, info) in infos.iter().enumerate() {
        let video_source = if info.has_video {
            idx
        } else {
            let duration = counterpart_duration(info, true).ok_or_else(|| {
                VideoError::InvalidInput(format!(
                    "input {idx} has audio but no video and no duration for a black-video fill"
                ))
            })?;
            let source_index = inputs.len() + synthetic.len();
            synthetic.push(SyntheticInput {
                source: format!("color=c=black:s={canvas_w}x{canvas_h}:r={fps}"),
                duration_secs: duration,
            });
            source_index
        };

        filter_parts.push(format!(
            "[{video_source}:v]scale={canvas_w}:{canvas_h}:force_original_aspect_ratio=decrease,\
             pad={canvas_w}:{canvas_h}:(ow-iw)/2:(oh-ih)/2:black,fps={fps},\
             format=yuv420p,setsar=1[v{idx}]"
        ));
        concat_pads.push(format!("[v{idx}]"));

        if let Some(target) = &audio {
            let audio_source = if info.has_audio {
                idx
            } else {
                let duration = counterpart_duration(info, false).ok_or_else(|| {
                    VideoError::InvalidInput(format!(
                        "input {idx} has video but no audio and no duration for a silent fill"
                    ))
                })?;
                let source_index = inputs.len() + synthetic.len();
                synthetic.push(SyntheticInput {
                    source: format!(
                        "anullsrc=r={}:cl={}",
                        target.sample_rate, target.channel_layout
                    ),
                    duration_secs: duration,
                });
                source_index
            };

            filter_parts.push(format!(
                "[{audio_source}:a]aformat=sample_fmts=fltp:sample_rates={}:channel_layouts={}\
                 [a{idx}]",
                target.sample_rate, target.channel_layout
            ));
            concat_pads.push(format!("[a{idx}]"));
        }
    }

    let v_out = "[v]";
    let a_out = "[a]";
    let concat_outs = if include_audio {
        format!("{v_out}{a_out}")
    } else {
        v_out.to_string()
    };
    filter_parts.push(format!(
        "{}concat=n={}:v=1:a={}{}",
        concat_pads.join(""),
        infos.len(),
        u8::from(include_audio),
        concat_outs
    ));
    let filter_complex = filter_parts.join(";");

    let mut args = vec!["-y".to_string(), "-v".to_string(), "error".to_string()];
    for path in &input_paths {
        args.push("-i".to_string());
        args.push(path.to_string_lossy().into_owned());
    }
    for synth in &synthetic {
        args.extend([
            "-f".to_string(),
            "lavfi".to_string(),
            "-t".to_string(),
            format!("{:.6}", synth.duration_secs),
            "-i".to_string(),
            synth.source.clone(),
        ]);
    }
    args.extend([
        "-filter_complex".to_string(),
        filter_complex,
        "-map".to_string(),
        "[v]".to_string(),
    ]);
    if include_audio {
        args.extend(["-map".to_string(), "[a]".to_string()]);
    }
    args.extend(av_encode_args(ext)?);

    let out_path = dir.file(&format!("out.{ext}"));
    args.push(out_path.to_string_lossy().into_owned());
    run_ffmpeg(program, &args)?;
    read_output(&out_path)
}

/// Concatenate audio-only inputs with the concat filter (no video).
/// Every input is resampled to the shared audio target before
/// `concat=n=N:v=0:a=1`.
fn concat_audio_only(
    program: &std::path::Path,
    inputs: &[Vec<u8>],
    infos: &[VideoInfo],
    ext: &str,
) -> VideoResult<Vec<u8>> {
    let target = audio_target(infos);
    let dir = TempDir::new("concat-filter-audio")?;

    let mut input_paths = Vec::with_capacity(inputs.len());
    for (idx, input) in inputs.iter().enumerate() {
        let in_ext = infos[idx]
            .format_name
            .as_deref()
            .and_then(ext_for_format)
            .unwrap_or("bin");
        input_paths.push(write_input(&dir, &format!("in{idx}.{in_ext}"), input)?);
    }

    let mut filter_parts = Vec::with_capacity(infos.len() + 1);
    let mut concat_pads = String::new();
    for (idx, _) in infos.iter().enumerate() {
        filter_parts.push(format!(
            "[{idx}:a]aresample=sample_rate={},aformat=sample_fmts=fltp:sample_rates={}:channel_layouts={}\
             [a{idx}]",
            target.sample_rate, target.sample_rate, target.channel_layout
        ));
        concat_pads.push_str(&format!("[a{idx}]"));
    }
    filter_parts.push(format!("{concat_pads}concat=n={}:v=0:a=1[a]", infos.len()));

    let mut args = vec!["-y".to_string(), "-v".to_string(), "error".to_string()];
    for path in &input_paths {
        args.push("-i".to_string());
        args.push(path.to_string_lossy().into_owned());
    }
    args.extend([
        "-filter_complex".to_string(),
        filter_parts.join(";"),
        "-map".to_string(),
        "[a]".to_string(),
        "-vn".to_string(),
    ]);
    args.extend(audio_encode_args(ext)?);

    let out_path = dir.file(&format!("out.{ext}"));
    args.push(out_path.to_string_lossy().into_owned());
    run_ffmpeg(program, &args)?;
    read_output(&out_path)
}

/// Concatenate 2–20 heterogeneous inputs into `format` using the
/// ffmpeg concat *filter*.
///
/// Unlike [`concat`](super::concat), inputs may differ in codec,
/// resolution, frame rate, container and stream layout. The output is
/// normalized to the largest input canvas, the maximum input frame
/// rate, `yuv420p` video and a shared audio layout (the first
/// audio-bearing input's parameters, or 48 kHz stereo). Inputs missing
/// audio receive a silence track; inputs missing video receive a black
/// video track when the set also contains video. When no input has
/// audio the output is video-only; when no input has video the output
/// is an audio-only container.
///
/// `format` is an output extension: `mp4`/`mkv`/`mov`/`avi`/`webm` for
/// video-bearing output, and the audio-capable extensions in
/// [`AUDIO_FORMATS`] for audio-only output.
pub(crate) fn concat_hetero(
    inputs: &[Vec<u8>],
    format: &str,
    limits: &EngineLimits,
) -> VideoResult<Vec<u8>> {
    concat_impl(inputs, format, limits)
}

/// Convenience wrapper for mixed A/V concatenation with default
/// limits. Behavior is identical to [`concat_hetero`]; the separate
/// name documents call sites that intentionally mix audio-only,
/// video-only and A/V inputs.
pub(crate) fn concat_mixed_av(inputs: &[Vec<u8>], format: &str) -> VideoResult<Vec<u8>> {
    concat_impl(inputs, format, &EngineLimits::default())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::crates::video_engine::ops::tool_available;
    use std::fs;
    use std::path::PathBuf;
    use std::process::Command;
    use std::sync::atomic::{AtomicU64, Ordering};
    use std::time::{SystemTime, UNIX_EPOCH};

    struct TestDir(PathBuf);

    impl TestDir {
        fn new(tag: &str) -> Self {
            static COUNTER: AtomicU64 = AtomicU64::new(0);
            let nanos = SystemTime::now()
                .duration_since(UNIX_EPOCH)
                .map(|d| d.as_nanos())
                .unwrap_or(0);
            let seq = COUNTER.fetch_add(1, Ordering::Relaxed);
            let path = std::env::temp_dir().join(format!(
                "kaladoc-concat-filter-test-{tag}-{}-{nanos}-{seq}",
                std::process::id()
            ));
            fs::create_dir_all(&path).unwrap();
            Self(path)
        }
    }

    impl Drop for TestDir {
        fn drop(&mut self) {
            let _ = fs::remove_dir_all(&self.0);
        }
    }

    fn ffmpeg_available() -> bool {
        tool_available(FFMPEG) && tool_available("ffprobe")
    }

    fn run_fixture(args: Vec<String>, out: &std::path::Path) -> Vec<u8> {
        let status = Command::new(FFMPEG)
            .args(args)
            .arg(out)
            .stdin(std::process::Stdio::null())
            .stdout(std::process::Stdio::null())
            .stderr(std::process::Stdio::piped())
            .status()
            .expect("failed to spawn ffmpeg");
        assert!(status.success(), "ffmpeg fixture generation failed");
        fs::read(out).unwrap()
    }

    /// A/V fixture: `testsrc` video plus a sine tone.
    fn make_av(
        dir: &TestDir,
        name: &str,
        video_source: &str,
        size: &str,
        rate: u32,
        secs: f64,
        freq: u32,
    ) -> Vec<u8> {
        let out = dir.0.join(name);
        let dur = format!("{secs}");
        run_fixture(
            vec![
                "-y".into(),
                "-v".into(),
                "error".into(),
                "-f".into(),
                "lavfi".into(),
                "-i".into(),
                format!("{video_source}=duration={dur}:size={size}:rate={rate}"),
                "-f".into(),
                "lavfi".into(),
                "-i".into(),
                format!("sine=frequency={freq}:duration={dur}"),
                "-pix_fmt".into(),
                "yuv420p".into(),
                "-shortest".into(),
            ],
            &out,
        )
    }

    /// Video-only fixture (no audio stream at all).
    fn make_video_only(dir: &TestDir, name: &str, size: &str, rate: u32, secs: f64) -> Vec<u8> {
        let out = dir.0.join(name);
        let dur = format!("{secs}");
        run_fixture(
            vec![
                "-y".into(),
                "-v".into(),
                "error".into(),
                "-f".into(),
                "lavfi".into(),
                "-i".into(),
                format!("testsrc=duration={dur}:size={size}:rate={rate}"),
                "-an".into(),
                "-pix_fmt".into(),
                "yuv420p".into(),
            ],
            &out,
        )
    }

    /// Audio-only WAV fixture.
    fn make_wav(dir: &TestDir, name: &str, secs: f64, freq: u32) -> Vec<u8> {
        let out = dir.0.join(name);
        let dur = format!("{secs}");
        run_fixture(
            vec![
                "-y".into(),
                "-v".into(),
                "error".into(),
                "-f".into(),
                "lavfi".into(),
                "-i".into(),
                format!("sine=frequency={freq}:duration={dur}"),
                "-c:a".into(),
                "pcm_s16le".into(),
            ],
            &out,
        )
    }

    #[test]
    fn concat_hetero_normalizes_canvas_fps_and_duration() {
        if !ffmpeg_available() {
            return;
        }
        let dir = TestDir::new("hetero");
        // Fixture A: testsrc 160x120 @ 30fps + 440 Hz; fixture B:
        // testsrc2 320x240 @ 24fps + 880 Hz — different geometry,
        // frame rate and audio frequency.
        let a = make_av(&dir, "a.mp4", "testsrc", "160x120", 30, 0.5, 440);
        let b = make_av(&dir, "b.mp4", "testsrc2", "320x240", 24, 0.7, 880);

        let joined = concat_hetero(&[a, b], "mp4", &EngineLimits::default()).unwrap();
        let info = probe(&joined).unwrap();
        assert_eq!(info.width, Some(320));
        assert_eq!(info.height, Some(240));
        let fps = info.fps.expect("output fps missing");
        assert!((fps - 30.0).abs() < 0.5, "fps {fps}");
        let duration = info.duration_secs.expect("output duration missing");
        assert!(
            (duration - 1.2).abs() < 0.35,
            "duration {duration}, expected ~1.2s"
        );
        assert!(info.has_audio, "expected normalized audio track");
        assert!(info.has_video);
    }

    #[test]
    fn concat_hetero_audio_only_inputs() {
        if !ffmpeg_available() {
            return;
        }
        let dir = TestDir::new("audio-only");
        let w1 = make_wav(&dir, "a.wav", 0.4, 440);
        let w2 = make_wav(&dir, "b.wav", 0.6, 880);

        let joined = concat_hetero(&[w1, w2], "wav", &EngineLimits::default()).unwrap();
        let info = probe(&joined).unwrap();
        assert!(info.has_audio);
        assert!(!info.has_video, "audio-only concat must not add video");
        let duration = info.duration_secs.expect("duration missing");
        assert!(
            (duration - 1.0).abs() < 0.3,
            "duration {duration}, expected ~1.0s"
        );
    }

    #[test]
    fn concat_hetero_generates_silence_for_video_only_input() {
        if !ffmpeg_available() {
            return;
        }
        let dir = TestDir::new("video-only");
        let av = make_av(&dir, "av.mp4", "testsrc", "160x120", 30, 0.5, 440);
        let video_only = make_video_only(&dir, "v.mp4", "320x240", 24, 0.5);

        let joined = concat_hetero(&[av, video_only], "mp4", &EngineLimits::default()).unwrap();
        let info = probe(&joined).unwrap();
        assert!(info.has_video);
        assert!(info.has_audio, "missing audio should become silence");
        let duration = info.duration_secs.expect("duration missing");
        assert!(
            (duration - 1.0).abs() < 0.35,
            "duration {duration}, expected ~1.0s"
        );
    }

    #[test]
    fn concat_mixed_av_handles_audio_only_segment() {
        if !ffmpeg_available() {
            return;
        }
        let dir = TestDir::new("mixed");
        let av = make_av(&dir, "av.mp4", "testsrc", "160x120", 30, 0.5, 440);
        let audio = make_wav(&dir, "tone.wav", 0.5, 880);

        let joined = concat_mixed_av(&[av, audio], "mp4").unwrap();
        let info = probe(&joined).unwrap();
        assert!(info.has_video, "audio-only segment should get black video");
        assert!(info.has_audio);
        let duration = info.duration_secs.expect("duration missing");
        assert!(
            (duration - 1.0).abs() < 0.35,
            "duration {duration}, expected ~1.0s"
        );
    }

    #[test]
    fn concat_filter_validates_input_count() {
        assert!(matches!(
            concat_hetero(&[vec![1]], "mp4", &EngineLimits::default()),
            Err(VideoError::InvalidInput(_))
        ));
        assert!(matches!(
            concat_hetero(
                &vec![vec![1]; MAX_INPUTS + 1],
                "mp4",
                &EngineLimits::default()
            ),
            Err(VideoError::LimitExceeded(_))
        ));
    }
}
