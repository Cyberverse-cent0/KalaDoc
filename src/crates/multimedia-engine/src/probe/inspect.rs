//! Top-level media inspection via `ffprobe`.
//!
//! [`inspect`] writes the input buffer to a temp file (ffprobe needs a
//! seekable path), runs
//! `ffprobe -v error -print_format json -show_format -show_streams
//! -show_chapters`, parses the JSON into [`ProbeReport`] and folds it
//! into a friendly [`MediaInfo`] report.

use std::collections::BTreeMap;
use std::ffi::OsString;

use serde::Serialize;

use crate::crates::multimedia_engine::error::{MediaError, MediaResult};
use crate::crates::multimedia_engine::model::MediaLimits;
use crate::crates::multimedia_engine::probe::types::{MediaStream, ProbeReport};
use crate::crates::multimedia_engine::runtime::tempdir::TempDir;
use crate::crates::multimedia_engine::runtime::{discovery, process};

/// Friendly summary of a probed media file.
#[derive(Debug, Clone, Serialize)]
pub struct MediaInfo {
    /// Container format name(s) as ffprobe reports them
    /// (`"mov,mp4,m4a,3gp,3g2,mj2"`).
    pub format_name: Option<String>,
    /// Long format description (`"QuickTime / MOV"`, ...).
    pub format_long_name: Option<String>,
    /// Container duration in seconds.
    pub duration_secs: Option<f64>,
    /// File size in bytes.
    pub size_bytes: Option<u64>,
    /// Overall bit rate in bits/s.
    pub bit_rate: Option<u64>,
    /// ffprobe detection confidence (higher is better).
    pub probe_score: Option<u32>,
    /// Number of video streams.
    pub video_streams: u32,
    /// Number of audio streams.
    pub audio_streams: u32,
    /// Number of subtitle streams.
    pub subtitle_streams: u32,
    /// Number of data streams.
    pub data_streams: u32,
    /// Number of attachment streams.
    pub attachment_streams: u32,
    /// Summary of the primary video stream, when present.
    pub primary_video: Option<VideoStreamSummary>,
    /// Summary of the primary audio stream, when present.
    pub primary_audio: Option<AudioStreamSummary>,
    /// Whether any stream is flagged as an attached picture
    /// (cover art).
    pub has_attached_pic: bool,
    /// Number of chapters.
    pub chapter_count: usize,
    /// Container-level tags (title, artist, encoder, ...).
    pub format_tags: BTreeMap<String, String>,
}

/// Summary of the primary video stream.
#[derive(Debug, Clone, Serialize)]
pub struct VideoStreamSummary {
    /// Codec identifier (`h264`, `vp9`, `png`, ...).
    pub codec: Option<String>,
    /// Long codec name.
    pub codec_long_name: Option<String>,
    /// Display width in pixels.
    pub width: Option<u32>,
    /// Display height in pixels.
    pub height: Option<u32>,
    /// Frame rate in fps (parsed from `r_frame_rate`/`avg_frame_rate`).
    pub fps: Option<f64>,
    /// Pixel format (`yuv420p`, ...).
    pub pix_fmt: Option<String>,
}

/// Summary of the primary audio stream.
#[derive(Debug, Clone, Serialize)]
pub struct AudioStreamSummary {
    /// Codec identifier (`aac`, `opus`, `mp3`, ...).
    pub codec: Option<String>,
    /// Long codec name.
    pub codec_long_name: Option<String>,
    /// Sample rate in Hz.
    pub sample_rate: Option<u64>,
    /// Channel count.
    pub channels: Option<u32>,
    /// Channel layout (`stereo`, `5.1`, ...).
    pub channel_layout: Option<String>,
}

/// Inspect `input` bytes with `ffprobe` and return a [`MediaInfo`].
///
/// The buffer is written to a temp file named `input<ext>` where `<ext>`
/// is sniffed from magic bytes; unrecognized inputs get `.bin` and
/// ffprobe guesses the format itself.
///
/// Errors:
/// - [`MediaError::MissingTool`] when `ffprobe` is not on `PATH`.
/// - [`MediaError::LimitExceeded`] when `input` exceeds
///   [`MediaLimits::max_input_bytes`].
/// - [`MediaError::InvalidInput`] when ffprobe fails, produces no
///   output, or its JSON cannot be parsed.
pub fn inspect(input: &[u8], limits: &MediaLimits) -> MediaResult<MediaInfo> {
    if let Some(max) = limits.max_input_bytes
        && input.len() as u64 > max
    {
        return Err(MediaError::LimitExceeded(format!(
            "input is {} bytes, exceeding max_input_bytes {max}",
            input.len()
        )));
    }

    let ffprobe = discovery::find_tool("ffprobe")
        .ok_or_else(|| MediaError::MissingTool("ffprobe".to_string()))?;

    let dir = TempDir::new("inspect")?;
    let input_path = dir.write(&format!("input{}", sniff_extension(input)), input)?;

    let mut args: Vec<OsString> = [
        "-v",
        "error",
        "-print_format",
        "json",
        "-show_format",
        "-show_streams",
        "-show_chapters",
    ]
    .iter()
    .map(OsString::from)
    .collect();
    args.push(input_path.into_os_string());

    let out = process::run("ffprobe", &ffprobe, &args, None, limits)?;
    if !out.success || out.stdout.is_empty() {
        let detail = if out.stderr.is_empty() {
            "ffprobe produced no output".to_string()
        } else {
            out.stderr
        };
        return Err(MediaError::InvalidInput(detail));
    }

    let report: ProbeReport = serde_json::from_slice(&out.stdout)
        .map_err(|e| MediaError::InvalidInput(format!("ffprobe JSON parse failed: {e}")))?;

    if report.streams.is_empty() && report.format.is_none() {
        return Err(MediaError::InvalidInput(
            "ffprobe found no media streams or format".to_string(),
        ));
    }

    Ok(build_info(&report))
}

/// Fold a [`ProbeReport`] into a [`MediaInfo`].
fn build_info(report: &ProbeReport) -> MediaInfo {
    let format = report.format.as_ref();

    let mut counts = (0u32, 0u32, 0u32, 0u32, 0u32);
    for s in &report.streams {
        match s.kind() {
            Some("video") => counts.0 += 1,
            Some("audio") => counts.1 += 1,
            Some("subtitle") => counts.2 += 1,
            Some("data") => counts.3 += 1,
            Some("attachment") => counts.4 += 1,
            _ => {}
        }
    }

    MediaInfo {
        format_name: format.and_then(|f| f.format_name.clone()),
        format_long_name: format.and_then(|f| f.format_long_name.clone()),
        duration_secs: format.and_then(|f| f.duration_secs()),
        size_bytes: format.and_then(|f| f.size_bytes()),
        bit_rate: format.and_then(|f| f.bit_rate()),
        probe_score: format.and_then(|f| f.probe_score),
        video_streams: counts.0,
        audio_streams: counts.1,
        subtitle_streams: counts.2,
        data_streams: counts.3,
        attachment_streams: counts.4,
        primary_video: primary_stream(&report.streams, "video").map(video_summary),
        primary_audio: primary_stream(&report.streams, "audio").map(audio_summary),
        has_attached_pic: report.streams.iter().any(is_attached_pic),
        chapter_count: report.chapters.len(),
        format_tags: format.map(|f| f.tags.clone()).unwrap_or_default(),
    }
}

/// Pick the primary stream of `kind`: the default-flagged one when
/// present, otherwise the first in stream order.
fn primary_stream<'a>(streams: &'a [MediaStream], kind: &str) -> Option<&'a MediaStream> {
    streams
        .iter()
        .find(|s| s.kind() == Some(kind) && s.is_default())
        .or_else(|| streams.iter().find(|s| s.kind() == Some(kind)))
}

fn video_summary(s: &MediaStream) -> VideoStreamSummary {
    VideoStreamSummary {
        codec: s.codec_name.clone(),
        codec_long_name: s.codec_long_name.clone(),
        width: s.width,
        height: s.height,
        fps: s.frame_rate(),
        pix_fmt: s.pix_fmt.clone(),
    }
}

fn audio_summary(s: &MediaStream) -> AudioStreamSummary {
    AudioStreamSummary {
        codec: s.codec_name.clone(),
        codec_long_name: s.codec_long_name.clone(),
        sample_rate: s.sample_rate.as_deref().and_then(|r| r.parse::<u64>().ok()),
        channels: s.channels,
        channel_layout: s.channel_layout.clone(),
    }
}

/// Whether the stream is flagged as an attached picture.
fn is_attached_pic(s: &MediaStream) -> bool {
    s.disposition_attached_pic == Some(true) || s.disposition.attached_pic.is_some_and(|v| v != 0)
}

/// Sniff a container extension from magic bytes so ffprobe gets a
/// hint about the format. Returns `".bin"` for unrecognized input —
/// ffprobe will still probe the content itself.
fn sniff_extension(data: &[u8]) -> &'static str {
    // ISO BMFF: bytes 4..8 are "ftyp".
    if data.len() >= 12 && &data[4..8] == b"ftyp" {
        return if &data[8..12] == b"qt  " {
            ".mov"
        } else {
            ".mp4"
        };
    }
    // EBML header → Matroska/WebM.
    if data.starts_with(&[0x1A, 0x45, 0xDF, 0xA3]) {
        return ".mkv";
    }
    if data.starts_with(b"RIFF") && data.len() >= 12 {
        return match &data[8..12] {
            b"WAVE" => ".wav",
            b"AVI " => ".avi",
            _ => ".bin",
        };
    }
    if data.starts_with(b"OggS") {
        return ".ogg";
    }
    if data.starts_with(b"fLaC") {
        return ".flac";
    }
    // ID3 tag or MPEG audio frame sync (0xFF Ex).
    if data.starts_with(b"ID3") || (data.len() >= 2 && data[0] == 0xFF && (data[1] & 0xE0) == 0xE0)
    {
        return ".mp3";
    }
    ".bin"
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::crates::multimedia_engine::runtime::tempdir::TempDir;

    /// Generate a tiny mp4 with ffmpeg lavfi: 1s of 64x64@10fps
    /// testsrc video + 440Hz sine audio → h264/aac. Returns `None`
    /// when ffmpeg or the needed encoders are unavailable so tests
    /// skip gracefully on minimal builds.
    fn make_mp4() -> Option<Vec<u8>> {
        let ffmpeg = discovery::find_tool("ffmpeg")?;
        let dir = TempDir::new("inspect-test").ok()?;
        let out_path = dir.file("sample.mp4");
        let args: Vec<OsString> = [
            "-y",
            "-v",
            "error",
            "-f",
            "lavfi",
            "-i",
            "testsrc=size=64x64:rate=10:duration=1",
            "-f",
            "lavfi",
            "-i",
            "sine=frequency=440:duration=1",
            "-c:v",
            "libx264",
            "-pix_fmt",
            "yuv420p",
            "-c:a",
            "aac",
            "-shortest",
            out_path.to_str()?,
        ]
        .iter()
        .map(OsString::from)
        .collect();
        let limits = MediaLimits::default();
        let out = process::run("ffmpeg", &ffmpeg, &args, None, &limits).ok()?;
        if !out.success {
            return None;
        }
        std::fs::read(&out_path).ok()
    }

    #[test]
    fn inspect_mp4_reports_format_and_streams() {
        let Some(mp4) = make_mp4() else {
            eprintln!("skipping: ffmpeg lavfi/x264/aac unavailable");
            return;
        };
        let info = inspect(&mp4, &MediaLimits::default()).expect("inspect mp4");

        assert!(
            info.format_name
                .as_deref()
                .is_some_and(|n| n.contains("mp4") || n.contains("mov")),
            "unexpected format_name: {:?}",
            info.format_name
        );
        assert!(info.format_long_name.is_some());
        let dur = info.duration_secs.expect("duration");
        assert!((dur - 1.0).abs() < 0.2, "duration {dur}");
        assert!(info.size_bytes.is_some_and(|s| s > 0));
        assert!(info.probe_score.is_some());

        assert_eq!(info.video_streams, 1);
        assert_eq!(info.audio_streams, 1);
        assert_eq!(info.subtitle_streams, 0);
        assert!(!info.has_attached_pic);
        assert_eq!(info.chapter_count, 0);

        let v = info.primary_video.expect("primary video");
        assert_eq!(v.codec.as_deref(), Some("h264"));
        assert_eq!(v.width, Some(64));
        assert_eq!(v.height, Some(64));
        assert_eq!(v.pix_fmt.as_deref(), Some("yuv420p"));
        let fps = v.fps.expect("fps");
        assert!((fps - 10.0).abs() < 0.5, "fps {fps}");

        let a = info.primary_audio.expect("primary audio");
        assert_eq!(a.codec.as_deref(), Some("aac"));
        assert!(a.sample_rate.is_some());
        assert!(a.channels.is_some_and(|c| c >= 1));
    }

    #[test]
    fn inspect_rejects_garbage_input() {
        let garbage = vec![0xDEu8, 0xAD, 0xBE, 0xEF, 1, 2, 3, 4, 5, 6, 7, 8];
        let err = inspect(&garbage, &MediaLimits::default()).expect_err("must fail");
        assert!(
            matches!(err, MediaError::InvalidInput(_)),
            "unexpected error: {err:?}"
        );
    }

    #[test]
    fn inspect_rejects_empty_input() {
        let err = inspect(&[], &MediaLimits::default()).expect_err("must fail");
        assert!(
            matches!(err, MediaError::InvalidInput(_)),
            "unexpected error: {err:?}"
        );
    }

    #[test]
    fn inspect_enforces_max_input_bytes() {
        let input = vec![0u8; 4096];
        let limits = MediaLimits {
            max_input_bytes: Some(1024),
            ..MediaLimits::default()
        };
        let err = inspect(&input, &limits).expect_err("must fail");
        assert!(
            matches!(err, MediaError::LimitExceeded(_)),
            "unexpected error: {err:?}"
        );
    }
}
