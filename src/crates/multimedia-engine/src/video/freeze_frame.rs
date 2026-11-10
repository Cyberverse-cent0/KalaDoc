//! Freeze a video frame for a duration (pause-style freeze frame).
//!
//! [`freeze_frame`] writes the input buffer to a temp file, probes it
//! with ffprobe (via [`inspect`]) to learn the container duration and
//! the primary stream parameters, then runs a single `ffmpeg` pass
//! with one `-filter_complex` graph:
//!
//! - **Video** is split into *before* (`[0, at)`), *frozen*, and
//!   *after* (`[at, end)`) branches with `trim`/`setpts`. The frozen
//!   branch keeps the tail of the video starting at `at` and prepends
//!   `duration_secs` of cloned first-frames via
//!   `tpad=start_duration=<dur>:start_mode=clone`, so the frame shown
//!   at `at`
//!   is held on screen while the rest of the video keeps playing
//!   afterwards. `fps`/`scale`/`setsar` normalize every branch so the
//!   `concat` filter accepts them.
//! - **Audio** (only when `freeze_audio` is true and the input has an
//!   audio stream) is split the same way around `at`: real audio
//!   `[0, at)`, `duration_secs` of `anullsrc` silence, then real audio
//!   `[at, end)`. The audio timeline is extended by `duration_secs` in
//!   lockstep with the video, so A/V stay aligned.
//!
//! Because the *after* branch resumes at `at` (not `at + duration`),
//! the output is `input_duration + duration_secs` long — the freeze
//! inserts a pause, it does not replace a time range.
//!
//! `freeze_audio = false` leaves the audio stream untouched (mapped
//! straight through, re-encoded with `audio_codec`); the audio then
//! keeps its original `input_duration` length while the video grows
//! by `duration_secs`.

use std::ffi::OsString;

use serde::{Deserialize, Serialize};

use crate::crates::multimedia_engine::error::{MediaError, MediaResult};
use crate::crates::multimedia_engine::model::{MediaFile, MediaLimits, MediaOutput};
use crate::crates::multimedia_engine::probe::inspect::{self, MediaInfo};
use crate::crates::multimedia_engine::runtime::discovery::Runtime;
use crate::crates::multimedia_engine::runtime::process;
use crate::crates::multimedia_engine::runtime::tempdir::TempDir;

/// Options for [`freeze_frame`]. Serde-friendly: all fields
/// deserialize with sensible defaults when omitted.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct FreezeOptions {
    /// Time in the input at which the freeze starts, in seconds.
    /// Must be finite, `>= 0`, and strictly less than the probed
    /// input duration.
    pub at_secs: f64,
    /// How long the frame is held, in seconds. Must be finite and
    /// `>= 0`; `0` produces an (almost) no-op output.
    pub duration_secs: f64,
    /// Output container extension without a leading dot (`"mp4"`,
    /// `"mkv"`, `"webm"`, ...). Becomes `output.<format>`.
    /// `None` defaults to `"mp4"`.
    #[serde(default)]
    pub output_format: Option<String>,
    /// Video encoder name passed to `-c:v` (`"libopenh264"`,
    /// `"h264"`, `"mpeg4"`, ...). `None` picks the first available
    /// candidate for the output container — never `libx264`.
    /// Verified against this FFmpeg build; unknown names fail with
    /// [`MediaError::Unsupported`].
    #[serde(default)]
    pub video_codec: Option<String>,
    /// Audio encoder name passed to `-c:a`. `None` picks the first
    /// available candidate for the output container.
    #[serde(default)]
    pub audio_codec: Option<String>,
    /// Replace the audio during the freeze window with silence
    /// (default `true`). `false` passes the audio through uncut —
    /// it keeps playing and does not gain the extra `duration_secs`.
    #[serde(default = "default_true")]
    pub freeze_audio: bool,
}

impl Default for FreezeOptions {
    fn default() -> Self {
        Self {
            at_secs: 0.0,
            duration_secs: 0.0,
            output_format: None,
            video_codec: None,
            audio_codec: None,
            freeze_audio: true,
        }
    }
}

fn default_true() -> bool {
    true
}

/// Freeze the frame shown at `options.at_secs` for
/// `options.duration_secs`, keeping the rest of the video (and, by
/// default, aligned silence for the audio) playing.
///
/// # Errors
///
/// - [`MediaError::InvalidInput`] — empty input, non-finite or
///   negative `at_secs`/`duration_secs`, input with no video stream,
///   or `at_secs >=` the probed input duration.
/// - [`MediaError::LimitExceeded`] — input exceeds
///   `max_input_bytes`, or the output exceeds `max_output_bytes`.
/// - [`MediaError::Unsupported`] — a required filter, encoder or
///   muxer is missing from this FFmpeg build.
/// - [`MediaError::ToolFailed`] / [`MediaError::Timeout`] — ffmpeg
///   rejected the graph (e.g. `at_secs` lands past the last video
///   frame so a concat segment is empty).
pub fn freeze_frame(
    input: &[u8],
    options: &FreezeOptions,
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

    // Probe first: we need the container duration (to validate
    // `at_secs`) and the primary stream parameters that drive the
    // filter graph (fps, geometry, audio rate/layout).
    let info = inspect::inspect(input, limits)?;
    let duration = info
        .duration_secs
        .filter(|d| d.is_finite() && *d > 0.0)
        .ok_or_else(|| {
            MediaError::InvalidInput("could not determine input duration".to_string())
        })?;
    if info.video_streams == 0 {
        return Err(MediaError::InvalidInput(
            "input has no video stream to freeze".to_string(),
        ));
    }
    if options.at_secs >= duration {
        return Err(MediaError::InvalidInput(format!(
            "at_secs ({}) must be less than input duration ({duration:.3}s)",
            options.at_secs
        )));
    }

    let caps = rt.capabilities()?;
    let output_ext = resolve_output_ext(options);
    check_muxer(caps, &output_ext)?;
    let video_codec = pick_encoder(
        caps,
        options.video_codec.as_deref(),
        video_codec_candidates(&output_ext),
        "video",
    )?;
    // Audio is needed when we build the silence graph or when the
    // caller asked for an audio track at all.
    let wants_audio = info.audio_streams > 0;
    let audio_codec = if wants_audio {
        Some(pick_encoder(
            caps,
            options.audio_codec.as_deref(),
            audio_codec_candidates(&output_ext),
            "audio",
        )?)
    } else {
        None
    };

    let dir = TempDir::new("freeze-frame")?;
    // ffmpeg probes by content; a neutral name avoids misleading the
    // demuxer when callers pass raw elementary streams.
    dir.write("input.bin", input)?;
    let output_name = format!("output.{output_ext}");
    let output_path = dir.file(&output_name);

    let video = info.primary_video.as_ref();
    let fps = video
        .and_then(|v| v.fps)
        .filter(|f| f.is_finite() && *f > 0.0);
    let width = video.and_then(|v| v.width);
    let height = video.and_then(|v| v.height);

    // A "before" segment only exists when `at_secs` is far enough
    // into the stream to contain at least one frame; below half a
    // frame period the trim would be empty and concat would fail.
    let has_before = options.at_secs > 0.0 && fps.is_none_or(|f| options.at_secs >= 0.5 / f);

    let (filter_complex, video_map, audio_map) =
        build_graph(options, fps, width, height, has_before, wants_audio, &info);

    // Every filter the graph names must exist in this build.
    let mut required = vec!["split", "trim", "setpts", "tpad", "concat", "setsar"];
    if fps.is_some() {
        required.push("fps");
    }
    if width.is_some() && height.is_some() {
        required.push("scale");
    }
    let audio_filtered = wants_audio && options.freeze_audio;
    if audio_filtered {
        required.extend(["asplit", "atrim", "asetpts", "anullsrc", "aformat"]);
    }
    for filter in required {
        require_filter(caps, filter)?;
    }

    let mut argv: Vec<OsString> = vec![
        OsString::from("-hide_banner"),
        OsString::from("-loglevel"),
        OsString::from("error"),
        OsString::from("-nostdin"),
        OsString::from("-y"),
        OsString::from("-i"),
        dir.file("input.bin").into_os_string(),
        OsString::from("-filter_complex"),
        OsString::from(filter_complex),
        OsString::from("-map"),
        OsString::from(format!("[{video_map}]")),
    ];

    match audio_map {
        AudioMap::Filtered(label) => {
            argv.push(OsString::from("-map"));
            argv.push(OsString::from(format!("[{label}]")));
        }
        AudioMap::Passthrough => {
            argv.push(OsString::from("-map"));
            // `?` makes the map optional so video-only inputs still
            // work when `wants_audio` was misjudged.
            argv.push(OsString::from("0:a:0?"));
        }
        AudioMap::None => {}
    }

    argv.push(OsString::from("-c:v"));
    argv.push(OsString::from(video_codec));
    if is_mp4_family(&output_ext) {
        // MP4-family muxers require 4:2:0 8-bit; our default codecs
        // all support it.
        argv.push(OsString::from("-pix_fmt"));
        argv.push(OsString::from("yuv420p"));
    }
    if let Some(acodec) = audio_codec {
        argv.push(OsString::from("-c:a"));
        argv.push(OsString::from(acodec));
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

    let mut warnings = Vec::new();
    if wants_audio && !options.freeze_audio {
        warnings.push(
            "freeze_audio=false: audio plays through uncut and keeps the original duration"
                .to_string(),
        );
    }
    if !is_mp4_family(&output_ext) {
        warnings.push(format!(
            "output container '{output_ext}' may not be playable everywhere; mp4 is the safest default"
        ));
    }

    Ok(MediaOutput {
        primary: Some(MediaFile {
            name: output_name,
            bytes,
            content_type: content_type_for(&output_ext),
        }),
        extra: Vec::new(),
        warnings,
    })
}

/// How the audio stream is mapped into the output.
enum AudioMap {
    /// Filtered `[0, at)` + silence + `[at, end)` graph output label.
    Filtered(&'static str),
    /// Original `0:a:0` mapped through untouched.
    Passthrough,
    /// No audio in the output.
    None,
}

/// Validate the numeric fields of `options`.
fn validate_options(options: &FreezeOptions) -> MediaResult<()> {
    if !options.at_secs.is_finite() || options.at_secs < 0.0 {
        return Err(MediaError::InvalidInput(format!(
            "at_secs must be a finite number >= 0, got {}",
            options.at_secs
        )));
    }
    if !options.duration_secs.is_finite() || options.duration_secs < 0.0 {
        return Err(MediaError::InvalidInput(format!(
            "duration_secs must be a finite number >= 0, got {}",
            options.duration_secs
        )));
    }
    Ok(())
}

/// Build the `-filter_complex` graph. Returns the graph string, the
/// video output pad name, and how audio is mapped.
///
/// Video topology (pause semantics — output is `input + duration`):
///
/// ```text
/// [0:v]split=N[vb_in][vf_in][va_in]
/// [vb_in]trim=0..at,setpts,…,fps/scale/setsar            → [v0]
/// [vf_in]trim=at..end,setpts,…,fps/scale/setsar,
///         tpad=start_duration=DUR:start_mode=clone           → [v1]
/// [va_in]trim=at..end,setpts,…,fps/scale/setsar          → [v2]
/// [v0][v1][v2]concat=n=N:v=1:a=0                          → [vout]
/// ```
///
/// The frozen branch is the *same* tail segment as `after`; `tpad`
/// prepends `duration_secs` of clones of its first frame (the frame
/// at `at_secs`), so the picture freezes while time advances.
///
/// Audio topology (only when `freeze_audio` and an audio stream
/// exists):
///
/// ```text
/// [0:a:0]asplit=2[ab_in][aa_in]
/// [ab_in]atrim=0..at,asetpts,…,aformat                  → [a0]
/// anullsrc=…,atrim=duration,eformat…                    → [a1]
/// [aa_in]atrim=at..end,asetpts,…,aformat                → [a2]
/// [a0][a1][a2]concat=n=3:v=0:a=1                          → [aout]
/// ```
fn build_graph(
    options: &FreezeOptions,
    fps: Option<f64>,
    width: Option<u32>,
    height: Option<u32>,
    has_before: bool,
    wants_audio: bool,
    info: &MediaInfo,
) -> (String, &'static str, AudioMap) {
    let at = fmt_secs(options.at_secs);
    let dur = fmt_secs(options.duration_secs);

    // Shared per-branch normalization so concat sees identical
    // geometry, aspect ratio and frame rate on every segment.
    let mut norm: Vec<String> = Vec::new();
    if let Some(f) = fps {
        norm.push(format!("fps={f}"));
    }
    if let (Some(w), Some(h)) = (width, height) {
        norm.push(format!("scale={w}:{h}"));
    }
    norm.push("setsar=1".to_string());
    let vnorm = norm.join(",");

    // Branch count: before (optional) + frozen + after. Split pad
    // indices follow branch order: [before?, frozen, after].
    let n = if has_before { 3 } else { 2 };
    let frozen_idx = if has_before { 1 } else { 0 };
    let after_idx = n - 1;

    let mut fc = String::new();
    fc.push_str(&format!("[0:v]split={n}"));
    for i in 0..n {
        fc.push_str(&format!("[v{i}in]"));
    }
    fc.push(';');

    let mut outputs: Vec<&'static str> = Vec::new();
    if has_before {
        outputs.push("v0");
        fc.push_str(&format!(
            "[v0in]trim=start=0:end={at},setpts=PTS-STARTPTS,{vnorm}[v0];"
        ));
    }
    let frozen_out = if has_before { "v1" } else { "v0" };
    let after_out = "v2";
    outputs.push(frozen_out);
    outputs.push(after_out);
    fc.push_str(&format!(
        "[v{frozen_idx}in]trim=start={at},setpts=PTS-STARTPTS,{vnorm},tpad=start_duration={dur}:start_mode=clone[{frozen_out}];"
    ));
    fc.push_str(&format!(
        "[v{after_idx}in]trim=start={at},setpts=PTS-STARTPTS,{vnorm}[{after_out}];"
    ));
    fc.push_str(&format!(
        "{}concat=n={n}:v=1:a=0[vout]",
        outputs.iter().map(|l| format!("[{l}]")).collect::<String>()
    ));

    let audio_map = if wants_audio && options.freeze_audio {
        append_audio_graph(&mut fc, options, info, has_before);
        AudioMap::Filtered("aout")
    } else if wants_audio {
        AudioMap::Passthrough
    } else {
        AudioMap::None
    };

    (fc, "vout", audio_map)
}

/// Append the audio branch (`before` + `anullsrc` silence + `after`)
/// to `fc`, producing `[aout]`. Assumes the video graph already ends
/// without a trailing `;`-terminated segment issue — we simply append
/// `;`-separated chains.
fn append_audio_graph(
    fc: &mut String,
    options: &FreezeOptions,
    info: &MediaInfo,
    has_before: bool,
) {
    let at = fmt_secs(options.at_secs);
    let dur = fmt_secs(options.duration_secs);
    let audio = info.primary_audio.as_ref();
    let sample_rate = audio
        .and_then(|a| a.sample_rate)
        .filter(|r| *r > 0)
        .unwrap_or(48_000);
    let channel_layout = audio
        .and_then(|a| a.channel_layout.clone())
        .filter(|l| !l.is_empty())
        .unwrap_or_else(|| "stereo".to_string());
    // anullsrc layout names must be ones ffmpeg knows; fall back to
    // stereo when the probed layout is exotic/unknown-looking.
    let cl = if channel_layout
        .chars()
        .all(|c| c.is_ascii_alphanumeric() || matches!(c, '_' | '-' | '(' | ')' | '.'))
    {
        channel_layout
    } else {
        "stereo".to_string()
    };

    let anorm = format!("aformat=sample_rates={sample_rate}:channel_layouts={cl}");
    let n = if has_before { 3 } else { 2 };

    fc.push_str(";[0:a:0]asplit=2[ab_in][aa_in];");
    if has_before {
        fc.push_str(&format!(
            "[ab_in]atrim=start=0:end={at},asetpts=PTS-STARTPTS,{anorm}[a0];"
        ));
    }
    // anullsrc is an infinite source; its `duration` option bounds
    // the generated silence to `dur` seconds.
    fc.push_str(&format!(
        "anullsrc=sample_rate={sample_rate}:channel_layout={cl}:duration={dur},asetpts=PTS-STARTPTS,{anorm}[a1];"
    ));
    fc.push_str(&format!(
        "[aa_in]atrim=start={at},asetpts=PTS-STARTPTS,{anorm}[a2];"
    ));
    let ins: Vec<String> = if has_before {
        vec!["[a0]".into(), "[a1]".into(), "[a2]".into()]
    } else {
        vec!["[a1]".into(), "[a2]".into()]
    };
    fc.push_str(&format!("{}concat=n={n}:v=0:a=1[aout]", ins.join("")));
}

/// Verify the named filter exists in this FFmpeg build.
fn require_filter(
    caps: &crate::crates::multimedia_engine::runtime::discovery::RuntimeCapabilities,
    filter: &str,
) -> MediaResult<()> {
    if caps.has_filter(filter) {
        Ok(())
    } else {
        Err(MediaError::Unsupported(format!(
            "filter {filter:?} is required by freeze_frame but is not available in this FFmpeg build"
        )))
    }
}

/// Verify the output container's muxer exists in this build.
fn check_muxer(
    caps: &crate::crates::multimedia_engine::runtime::discovery::RuntimeCapabilities,
    ext: &str,
) -> MediaResult<()> {
    if caps.has_muxer(ext) {
        return Ok(());
    }
    // A few extensions map onto a differently-named muxer.
    let alias = match ext {
        "m4v" | "m4a" | "3gp" | "3g2" => Some("mp4"),
        "mpg" | "mpeg" | "vob" => Some("mpeg"),
        _ => None,
    };
    if alias.is_some_and(|m| caps.has_muxer(m)) {
        return Ok(());
    }
    Err(MediaError::Unsupported(format!(
        "muxer for output format '{ext}' is not available in this FFmpeg build"
    )))
}

/// Pick the encoder for `kind`: the caller's explicit choice when
/// given (verified against this build), otherwise the first available
/// candidate. Never falls back silently to a different codec.
fn pick_encoder(
    caps: &crate::crates::multimedia_engine::runtime::discovery::RuntimeCapabilities,
    explicit: Option<&str>,
    candidates: &[&'static str],
    kind: &str,
) -> MediaResult<String> {
    if let Some(name) = explicit {
        let name = name.trim();
        if name.is_empty() {
            return Err(MediaError::InvalidInput(format!(
                "{kind}_codec must not be empty"
            )));
        }
        if caps.has_encoder(name) {
            return Ok(name.to_string());
        }
        return Err(MediaError::Unsupported(format!(
            "{kind} encoder '{name}' is not available in this FFmpeg build"
        )));
    }
    for cand in candidates {
        if caps.has_encoder(cand) {
            return Ok((*cand).to_string());
        }
    }
    Err(MediaError::Unsupported(format!(
        "no usable {kind} encoder ({}) in this FFmpeg build",
        candidates.join("/")
    )))
}

/// Default video encoder candidates per container. Deliberately
/// excludes `libx264` — callers on GPL-free builds should not need
/// it; `libopenh264`/`h264`/`mpeg4` cover H.264/MPEG-4 output.
fn video_codec_candidates(ext: &str) -> &'static [&'static str] {
    match ext {
        "webm" => &["libvpx-vp9", "libvpx"],
        "mp3" | "wav" | "flac" | "ogg" | "oga" | "opus" | "aac" | "adts" => &[],
        _ => &["libopenh264", "h264", "mpeg4"],
    }
}

/// Default audio encoder candidates per container.
fn audio_codec_candidates(ext: &str) -> &'static [&'static str] {
    match ext {
        "webm" => &["libopus", "libvorbis"],
        "mp3" => &["libmp3lame"],
        "wav" => &["pcm_s16le"],
        "flac" => &["flac"],
        "opus" => &["libopus"],
        "ogg" | "oga" => &["libvorbis"],
        "aac" | "adts" => &["aac"],
        _ => &["aac"],
    }
}

/// Determine the output container extension: `output_format` when
/// given (leading dot stripped, lowercased), else `"mp4"`.
fn resolve_output_ext(options: &FreezeOptions) -> String {
    if let Some(fmt) = options.output_format.as_deref() {
        let fmt = fmt.trim().trim_start_matches('.').to_ascii_lowercase();
        if !fmt.is_empty() {
            return fmt;
        }
    }
    "mp4".to_string()
}

/// Whether `ext` is an MP4-family container that requires
/// `yuv420p`-style pixel formats.
fn is_mp4_family(ext: &str) -> bool {
    matches!(ext, "mp4" | "m4v" | "m4a" | "mov" | "3gp" | "3g2")
}

/// Format seconds for ffmpeg filter options (same precision policy
/// as `trim`: three decimals is plenty for media time).
fn fmt_secs(secs: f64) -> String {
    format!("{secs:.3}")
}

/// Best-effort MIME type for the produced container.
fn content_type_for(ext: &str) -> Option<String> {
    let mime = match ext {
        "mp4" | "m4v" | "m4a" => "video/mp4",
        "mkv" => "video/x-matroska",
        "webm" => "video/webm",
        "mov" => "video/quicktime",
        "avi" => "video/x-msvideo",
        "mp3" => "audio/mpeg",
        "aac" | "adts" => "audio/aac",
        "wav" => "audio/wav",
        "ogg" | "oga" | "opus" => "audio/ogg",
        "flac" => "audio/flac",
        _ => return None,
    };
    Some(mime.to_string())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::crates::multimedia_engine::probe::inspect::inspect;
    use crate::crates::multimedia_engine::runtime::discovery;

    fn runtime() -> Runtime {
        Runtime::discover().expect("ffmpeg/ffprobe must be on PATH")
    }

    fn limits() -> MediaLimits {
        MediaLimits::default()
    }

    /// Generate a 2-second 64x64@15fps testsrc + 440Hz sine mp4 via
    /// lavfi. Returns `None` when ffmpeg or the needed lavfi
    /// sources/encoders are unavailable so tests skip gracefully.
    fn sample_input() -> Option<Vec<u8>> {
        let ffmpeg = discovery::find_tool("ffmpeg")?;
        let dir = TempDir::new("freeze-test-in").ok()?;
        let out_path = dir.file("sample.mp4");
        let argv = process::args(&[
            "-hide_banner",
            "-loglevel",
            "error",
            "-nostdin",
            "-y",
            "-f",
            "lavfi",
            "-i",
            "testsrc=size=64x64:rate=15:duration=2",
            "-f",
            "lavfi",
            "-i",
            "sine=frequency=440:duration=2",
            "-c:v",
            "libx264",
            "-preset",
            "ultrafast",
            "-pix_fmt",
            "yuv420p",
            "-c:a",
            "aac",
            "-shortest",
            out_path.to_str()?,
        ]);
        let out = process::run("ffmpeg", &ffmpeg, &argv, None, &limits()).ok()?;
        if !out.success {
            return None;
        }
        std::fs::read(&out_path).ok()
    }

    /// True when this build has at least one default video encoder
    /// and the filters we need — otherwise the op cannot succeed and
    /// the test should skip.
    fn op_supported(rt: &Runtime) -> bool {
        let Ok(caps) = rt.capabilities() else {
            return false;
        };
        let v_ok = video_codec_candidates("mp4")
            .iter()
            .any(|c| caps.has_encoder(c));
        let a_ok = audio_codec_candidates("mp4")
            .iter()
            .any(|c| caps.has_encoder(c));
        [
            "split", "trim", "setpts", "tpad", "concat", "setsar", "fps", "scale",
        ]
        .iter()
        .all(|f| caps.has_filter(f))
            && v_ok
            && a_ok
            && caps.has_muxer("mp4")
    }

    #[test]
    fn freeze_frame_extends_duration_by_freeze_length() {
        let Some(input) = sample_input() else {
            eprintln!("skipping: ffmpeg lavfi/libx264/aac unavailable");
            return;
        };
        let rt = runtime();
        if !op_supported(&rt) {
            eprintln!("skipping: no default freeze_frame encoder/filters in this build");
            return;
        }
        let options = FreezeOptions {
            at_secs: 1.0,
            duration_secs: 0.5,
            ..Default::default()
        };
        let out = freeze_frame(&input, &options, &limits(), &rt).expect("freeze_frame");
        let file = out.primary.expect("primary output");
        assert_eq!(file.name, "output.mp4");
        assert!(!file.bytes.is_empty());

        let info = inspect(&file.bytes, &limits()).expect("probe frozen");
        let dur = info.duration_secs.expect("duration");
        // Pause semantics: 2.0s input + 0.5s freeze = ~2.5s output.
        assert!((dur - 2.5).abs() < 0.3, "expected ~2.5s, got {dur}s");
        assert!(info.video_streams >= 1);
        assert!(info.audio_streams >= 1);
    }

    #[test]
    fn freeze_frame_rejects_invalid_options() {
        let rt = runtime();
        let input = sample_input().unwrap_or_else(|| vec![0u8; 64]);
        for options in [
            FreezeOptions {
                at_secs: -1.0,
                duration_secs: 0.5,
                ..Default::default()
            },
            FreezeOptions {
                at_secs: f64::NAN,
                duration_secs: 0.5,
                ..Default::default()
            },
            FreezeOptions {
                at_secs: 1.0,
                duration_secs: -0.5,
                ..Default::default()
            },
            FreezeOptions {
                at_secs: 1.0,
                duration_secs: f64::INFINITY,
                ..Default::default()
            },
        ] {
            let err = freeze_frame(&input, &options, &limits(), &rt).expect_err("must fail");
            assert!(
                matches!(err, MediaError::InvalidInput(_)),
                "unexpected error: {err:?}"
            );
        }
    }
}
