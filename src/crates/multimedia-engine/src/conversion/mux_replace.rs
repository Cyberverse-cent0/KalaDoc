//! Stream-level muxing: attach a new audio track to a video and
//! optionally mux a subtitle track alongside it.
//!
//! Both ops stream-copy the video (`-c:v copy`) and re-encode only the
//! audio (and subtitle) streams:
//!
//! ```text
//! ffmpeg -i video -i audio [-i subs] -map 0:v -map 1:a [-map 2:s] \
//!        -c:v copy -c:a <codec> [-c:s <codec>] [-shortest] out.<fmt>
//! ```
//!
//! # Container compatibility
//!
//! `-c:v copy` does not transcode, so the source video codec must be
//! storable in the target container. Practically:
//!
//! - **mp4 / mov / m4a**: H.264, HEVC, MPEG-4 part 2, ProRes, etc.
//!   Audio must be AAC or MP3; subtitles become `mov_text`.
//! - **mkv**: almost anything (H.264, HEVC, VP9, AV1, ProRes, ...).
//!   Audio: AAC, MP3, Opus, FLAC, ... Subtitles: any text codec.
//! - **webm**: VP8/VP9/AV1 video only — H.264 will fail at mux time.
//!   Audio: Opus or Vorbis; subtitles: WebVTT.
//!
//! Choosing an incompatible combination surfaces as
//! [`MediaError::ToolFailed`] from ffmpeg — we validate codec/muxer
//! *presence* via the capability registry but cannot validate
//! codec-in-container rules without decoding every stream.
//!
//! # Capability honesty
//!
//! The output muxer and every requested codec are checked against the
//! discovered FFmpeg build before spawning. Missing pieces return
//! [`MediaError::Unsupported`]; nothing silently falls back.

use std::ffi::OsString;

use crate::crates::multimedia_engine::error::{MediaError, MediaResult};
use crate::crates::multimedia_engine::model::{MediaLimits, MediaOutput};
use crate::crates::multimedia_engine::runtime::discovery::Runtime;
use crate::crates::multimedia_engine::runtime::process;
use crate::crates::multimedia_engine::runtime::tempdir::TempDir;

/// Options for [`replace_audio`].
#[derive(Debug, Clone, Default)]
pub struct ReplaceAudioOptions {
    /// Output container extension without the dot (`"mp4"`, `"mkv"`,
    /// `"webm"`, ...). Defaults to `"mp4"`.
    pub output_format: Option<String>,
    /// Audio encoder name (`"aac"`, `"libmp3lame"`, `"libopus"`, ...).
    /// Defaults per container: `aac` for mp4-family, `libmp3lame` for
    /// mp3, `libopus` for ogg/opus, `pcm_s16le` for wav, `aac`
    /// otherwise.
    pub audio_codec: Option<String>,
    /// When true, pass `-shortest` so the output ends with the
    /// shortest input stream.
    pub shortest: bool,
}

/// Options for [`mux_streams`].
#[derive(Debug, Clone, Default)]
pub struct MuxStreamsOptions {
    /// Output container extension without the dot. Defaults to `"mp4"`.
    pub output_format: Option<String>,
    /// Audio encoder name; same defaults as [`ReplaceAudioOptions`].
    pub audio_codec: Option<String>,
    /// Subtitle encoder name. Defaults to `mov_text` for mp4/mov/m4a
    /// targets and `srt` for everything else.
    pub subtitle_codec: Option<String>,
    /// When true, pass `-shortest`.
    pub shortest: bool,
}

/// Mux a new audio track onto a video, replacing whatever audio the
/// video had (the video input contributes only its video stream via
/// `-map 0:v`).
///
/// `video` and `audio` are raw media bytes in any container ffmpeg can
/// demux; `audio` contributes its first audio stream (`-map 1:a`).
///
/// See the module docs for container/codec compatibility notes.
pub fn replace_audio(
    video: &[u8],
    audio: &[u8],
    options: &ReplaceAudioOptions,
    limits: &MediaLimits,
    rt: &Runtime,
) -> MediaResult<MediaOutput> {
    mux_impl(
        video,
        Some(audio),
        None,
        options.output_format.as_deref(),
        options.audio_codec.as_deref(),
        None,
        options.shortest,
        limits,
        rt,
    )
}

/// Mux video + audio + an optional subtitle track into one container.
///
/// `subtitle`, when `Some`, is raw subtitle bytes — SRT or WebVTT are
/// auto-detected by content (`WEBVTT` magic); anything else is written
/// with an `.srt` extension. Its first subtitle stream is mapped via
/// `-map 2:s`.
///
/// The subtitle codec defaults to `mov_text` for mp4-family outputs
/// and `srt` otherwise (matroska accepts `srt` as `subrip`).
pub fn mux_streams(
    video: &[u8],
    audio: &[u8],
    subtitle: Option<&[u8]>,
    options: &MuxStreamsOptions,
    limits: &MediaLimits,
    rt: &Runtime,
) -> MediaResult<MediaOutput> {
    mux_impl(
        video,
        Some(audio),
        subtitle,
        options.output_format.as_deref(),
        options.audio_codec.as_deref(),
        options.subtitle_codec.as_deref(),
        options.shortest,
        limits,
        rt,
    )
}

/// Shared implementation. `audio`/`subtitle` are `Option` so the same
/// path can serve future video-only or subtitle-only variants; today
/// both public entry points always pass `Some(audio)`.
fn mux_impl(
    video: &[u8],
    audio: Option<&[u8]>,
    subtitle: Option<&[u8]>,
    output_format: Option<&str>,
    audio_codec: Option<&str>,
    subtitle_codec: Option<&str>,
    shortest: bool,
    limits: &MediaLimits,
    rt: &Runtime,
) -> MediaResult<MediaOutput> {
    if video.is_empty() {
        return Err(MediaError::InvalidInput("video input is empty".into()));
    }
    if let Some(a) = audio
        && a.is_empty()
    {
        return Err(MediaError::InvalidInput("audio input is empty".into()));
    }
    if let Some(s) = subtitle
        && s.is_empty()
    {
        return Err(MediaError::InvalidInput("subtitle input is empty".into()));
    }
    check_input_size(video.len(), "video", limits)?;
    if let Some(a) = audio {
        check_input_size(a.len(), "audio", limits)?;
    }
    if let Some(s) = subtitle {
        check_input_size(s.len(), "subtitle", limits)?;
    }

    let format = normalize_format(output_format.unwrap_or("mp4"));
    let caps = rt.capabilities()?;

    // Muxer must exist in this build.
    if !caps.has_muxer(&format) {
        return Err(MediaError::Unsupported(format!(
            "output muxer '{format}' is not available in this FFmpeg build"
        )));
    }

    // Resolve + validate the audio encoder.
    let audio_codec = audio_codec
        .map(str::to_string)
        .unwrap_or_else(|| default_audio_codec(&format).to_string());
    if !caps.has_encoder(&audio_codec) {
        return Err(MediaError::Unsupported(format!(
            "audio encoder '{audio_codec}' is not available in this FFmpeg build"
        )));
    }

    // Resolve + validate the subtitle encoder when a subtitle input is
    // present.
    let sub_codec = subtitle.map(|_| {
        subtitle_codec
            .map(str::to_string)
            .unwrap_or_else(|| default_subtitle_codec(&format).to_string())
    });
    if let Some(sc) = &sub_codec
        && !caps.has_encoder(sc)
    {
        return Err(MediaError::Unsupported(format!(
            "subtitle encoder '{sc}' is not available in this FFmpeg build"
        )));
    }

    let tmp = TempDir::new("mux")?;
    let video_path = tmp.write("input_video", video)?;
    let audio_path = audio.map(|a| tmp.write("input_audio", a)).transpose()?;
    let subtitle_path = subtitle
        .map(|s| {
            let name = if looks_like_webvtt(s) {
                "input_sub.vtt"
            } else {
                "input_sub.srt"
            };
            tmp.write(name, s)
        })
        .transpose()?;
    let out_name = format!("output.{format}");
    let out_path = tmp.file(&out_name);

    // Build the argv vector. Inputs have no extension on purpose:
    // ffmpeg's content probe identifies the container, and a wrong
    // extension hint could only hurt. Subtitles are the exception —
    // their demuxers key off the extension, handled above.
    let mut argv: Vec<OsString> = vec![
        OsString::from("-hide_banner"),
        OsString::from("-y"),
        OsString::from("-i"),
        video_path.into_os_string(),
    ];
    if let Some(p) = &audio_path {
        argv.extend([OsString::from("-i"), p.clone().into_os_string()]);
    }
    if let Some(p) = &subtitle_path {
        argv.extend([OsString::from("-i"), p.clone().into_os_string()]);
    }
    argv.extend([
        OsString::from("-map"),
        OsString::from("0:v"),
        OsString::from("-map"),
        OsString::from("1:a"),
        OsString::from("-c:v"),
        OsString::from("copy"),
        OsString::from("-c:a"),
        OsString::from(audio_codec),
    ]);
    if let (Some(_), Some(sc)) = (&subtitle_path, &sub_codec) {
        argv.extend([
            OsString::from("-map"),
            OsString::from("2:s"),
            OsString::from("-c:s"),
            OsString::from(sc.clone()),
        ]);
    }
    if shortest {
        argv.push(OsString::from("-shortest"));
    }
    argv.push(out_path.into_os_string());

    process::run_checked("ffmpeg", &rt.ffmpeg, &argv, None, limits)?;

    let bytes = tmp.read_limited(&out_name, limits.max_output_bytes)?;
    Ok(MediaOutput {
        primary: Some(crate::crates::multimedia_engine::model::MediaFile {
            name: out_name,
            bytes,
            content_type: Some(content_type_for(&format).to_string()),
        }),
        extra: Vec::new(),
        warnings: Vec::new(),
    })
}

/// Enforce `max_input_bytes` on one input.
fn check_input_size(len: usize, what: &str, limits: &MediaLimits) -> MediaResult<()> {
    if let Some(max) = limits.max_input_bytes
        && len as u64 > max
    {
        return Err(MediaError::LimitExceeded(format!(
            "{what} input is {len} bytes, exceeding max_input_bytes {max}"
        )));
    }
    Ok(())
}

/// Lowercase and strip a leading `.`/`ffmpeg`-style prefix from a
/// user-supplied format string.
fn normalize_format(format: &str) -> String {
    format.trim().trim_start_matches('.').to_ascii_lowercase()
}

/// Best-effort audio encoder default for a container.
fn default_audio_codec(format: &str) -> &'static str {
    match format {
        "mp3" => "libmp3lame",
        "ogg" | "opus" => "libopus",
        "wav" => "pcm_s16le",
        _ => "aac",
    }
}

/// Best-effort subtitle encoder default for a container.
fn default_subtitle_codec(format: &str) -> &'static str {
    match format {
        "mp4" | "mov" | "m4a" | "3gp" | "3g2" => "mov_text",
        _ => "srt",
    }
}

/// Rough WebVTT detection: the spec requires the file to start with
/// the `WEBVTT` signature (optionally preceded by a BOM).
fn looks_like_webvtt(data: &[u8]) -> bool {
    let data = if data.starts_with(&[0xEF, 0xBB, 0xBF]) {
        &data[3..]
    } else {
        data
    };
    data.starts_with(b"WEBVTT")
}

/// MIME type for the produced container, for downstream sniffing.
fn content_type_for(format: &str) -> &'static str {
    match format {
        "mp4" | "m4a" | "mov" | "3gp" | "3g2" => "video/mp4",
        "mkv" => "video/x-matroska",
        "webm" => "video/webm",
        "mp3" => "audio/mpeg",
        "wav" => "audio/wav",
        "flac" => "audio/flac",
        "ogg" | "opus" => "audio/ogg",
        "avi" => "video/x-msvideo",
        _ => "application/octet-stream",
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::crates::multimedia_engine::error::MediaError;
    use crate::crates::multimedia_engine::model::MediaLimits;
    use crate::crates::multimedia_engine::probe::types::ProbeReport;
    use crate::crates::multimedia_engine::runtime::discovery::Runtime;
    use crate::crates::multimedia_engine::runtime::process;
    use crate::crates::multimedia_engine::runtime::tempdir::TempDir;
    use std::ffi::OsString;
    use std::path::Path;

    fn rt() -> Runtime {
        Runtime::discover().expect("ffmpeg/ffprobe must be on PATH for tests")
    }

    /// Run ffmpeg with `args` (all `&str`) plus `output` as the final
    /// argv element.
    fn ffmpeg_to(rt: &Runtime, args: &[&str], output: &Path) -> Vec<u8> {
        let mut argv: Vec<OsString> = args.iter().map(OsString::from).collect();
        argv.push(output.as_os_str().to_os_string());
        process::run_checked("ffmpeg", &rt.ffmpeg, &argv, None, &MediaLimits::default())
            .expect("ffmpeg generation failed");
        std::fs::read(output).expect("generated file missing")
    }

    /// Tiny H.264 mp4 with NO audio, via lavfi `testsrc`.
    fn lavfi_video(dir: &TempDir) -> Vec<u8> {
        ffmpeg_to(
            &rt(),
            &[
                "-hide_banner",
                "-y",
                "-f",
                "lavfi",
                "-i",
                "testsrc=duration=1:size=160x120:rate=10",
                "-c:v",
                "libx264",
                "-pix_fmt",
                "yuv420p",
                "-an",
            ],
            &dir.file("gen_video.mp4"),
        )
    }

    /// 1s sine wave as 16-bit PCM wav, via lavfi `sine`.
    fn sine_wav(dir: &TempDir) -> Vec<u8> {
        ffmpeg_to(
            &rt(),
            &[
                "-hide_banner",
                "-y",
                "-f",
                "lavfi",
                "-i",
                "sine=frequency=440:duration=1",
                "-c:a",
                "pcm_s16le",
            ],
            &dir.file("gen_audio.wav"),
        )
    }

    /// Minimal SRT subtitle.
    fn srt_subtitle() -> Vec<u8> {
        b"1\n00:00:00,000 --> 00:00:00,500\nHello\n\n2\n00:00:00,500 --> 00:00:01,000\nWorld\n"
            .to_vec()
    }

    /// ffprobe the bytes (written to a temp file) and return the
    /// parsed report.
    fn probe_bytes(dir: &TempDir, name: &str, data: &[u8]) -> ProbeReport {
        let path = dir.write(name, data).unwrap();
        let argv: Vec<OsString> = vec![
            OsString::from("-v"),
            OsString::from("error"),
            OsString::from("-show_streams"),
            OsString::from("-show_format"),
            OsString::from("-of"),
            OsString::from("json"),
            OsString::from("-i"),
            path.into_os_string(),
        ];
        let out = process::run_checked(
            "ffprobe",
            &rt().ffprobe,
            &argv,
            None,
            &MediaLimits::default(),
        )
        .expect("ffprobe failed");
        serde_json::from_slice(&out.stdout).expect("ffprobe JSON parse failed")
    }

    #[test]
    fn replace_audio_adds_aac_track_to_mp4() {
        let dir = TempDir::new("mux-test").unwrap();
        let video = lavfi_video(&dir);
        let audio = sine_wav(&dir);

        let out = replace_audio(
            &video,
            &audio,
            &ReplaceAudioOptions::default(),
            &MediaLimits::default(),
            &rt(),
        )
        .expect("replace_audio failed");

        let file = out.primary.expect("no primary output");
        assert!(file.name.ends_with(".mp4"), "name: {}", file.name);
        assert_eq!(file.content_type.as_deref(), Some("video/mp4"));

        // Write output to a named temp file and probe it.
        let probe_dir = TempDir::new("mux-probe").unwrap();
        let report = probe_bytes(&probe_dir, "out.mp4", &file.bytes);
        let kinds: Vec<&str> = report.streams.iter().filter_map(|s| s.kind()).collect();
        assert!(kinds.contains(&"video"), "streams: {kinds:?}");
        assert!(kinds.contains(&"audio"), "streams: {kinds:?}");
        let audio_stream = report
            .streams
            .iter()
            .find(|s| s.kind() == Some("audio"))
            .unwrap();
        assert_eq!(audio_stream.codec_name.as_deref(), Some("aac"));
    }

    #[test]
    fn replace_audio_explicit_codec_and_mkv() {
        let dir = TempDir::new("mux-test").unwrap();
        let video = lavfi_video(&dir);
        let audio = sine_wav(&dir);

        let out = replace_audio(
            &video,
            &audio,
            &ReplaceAudioOptions {
                output_format: Some("mkv".into()),
                audio_codec: Some("libmp3lame".into()),
                shortest: true,
            },
            &MediaLimits::default(),
            &rt(),
        )
        .expect("replace_audio mkv failed");
        let file = out.primary.unwrap();
        assert!(file.name.ends_with(".mkv"));

        let probe_dir = TempDir::new("mux-probe").unwrap();
        let report = probe_bytes(&probe_dir, "out.mkv", &file.bytes);
        let audio_stream = report
            .streams
            .iter()
            .find(|s| s.kind() == Some("audio"))
            .expect("no audio stream");
        assert_eq!(audio_stream.codec_name.as_deref(), Some("mp3"));
    }

    #[test]
    fn mux_streams_adds_mov_text_subtitles_to_mp4() {
        let dir = TempDir::new("mux-test").unwrap();
        let video = lavfi_video(&dir);
        let audio = sine_wav(&dir);
        let subs = srt_subtitle();

        let out = mux_streams(
            &video,
            &audio,
            Some(&subs),
            &MuxStreamsOptions::default(),
            &MediaLimits::default(),
            &rt(),
        )
        .expect("mux_streams failed");
        let file = out.primary.unwrap();

        let probe_dir = TempDir::new("mux-probe").unwrap();
        let report = probe_bytes(&probe_dir, "out.mp4", &file.bytes);
        let kinds: Vec<&str> = report.streams.iter().filter_map(|s| s.kind()).collect();
        assert!(kinds.contains(&"video"), "streams: {kinds:?}");
        assert!(kinds.contains(&"audio"), "streams: {kinds:?}");
        assert!(kinds.contains(&"subtitle"), "streams: {kinds:?}");
        let sub = report
            .streams
            .iter()
            .find(|s| s.kind() == Some("subtitle"))
            .unwrap();
        assert_eq!(sub.codec_name.as_deref(), Some("mov_text"));
    }

    #[test]
    fn replace_audio_rejects_empty_video() {
        let err = replace_audio(
            &[],
            b"x",
            &ReplaceAudioOptions::default(),
            &MediaLimits::default(),
            &rt(),
        )
        .unwrap_err();
        assert!(matches!(err, MediaError::InvalidInput(_)), "{err}");
    }
}
