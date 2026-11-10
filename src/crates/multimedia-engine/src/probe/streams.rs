//! Per-stream detail reporting via ffprobe.
//!
//! [`probe_streams`] runs `ffprobe -show_streams -show_format -of json`
//! against a temp copy of the input and returns every stream — video,
//! audio, subtitle, data and attachment alike — as a flat,
//! serde-serializable [`StreamDetail`] list. This is the "list all
//! streams" view; richer inspection lives in `probe::inspect`.

use serde::{Deserialize, Serialize};

use crate::crates::multimedia_engine::error::{MediaError, MediaResult};
use crate::crates::multimedia_engine::model::MediaLimits;
use crate::crates::multimedia_engine::probe::types::ProbeReport;
use crate::crates::multimedia_engine::runtime::discovery::Runtime;
use crate::crates::multimedia_engine::runtime::{process, tempdir::TempDir};

/// Full per-stream breakdown of a media input.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct StreamReport {
    /// Every stream in the file, in container order.
    pub streams: Vec<StreamDetail>,
    /// Container format name list (`"mov,mp4,m4a,3gp,3g2,mj2"`).
    pub format_name: Option<String>,
    /// Container duration in seconds, when reported.
    pub duration_secs: Option<f64>,
}

/// Pixel dimensions of a video stream.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub struct Resolution {
    /// Width in pixels.
    pub width: u32,
    /// Height in pixels.
    pub height: u32,
}

/// One stream's detail, flattened from ffprobe's JSON.
///
/// Fields that only apply to some stream kinds stay `None` for the
/// others — `resolution`/`pix_fmt`/`avg_frame_rate` are video-only,
/// `sample_rate`/`channels`/`channel_layout` are audio-only.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct StreamDetail {
    /// Stream index within the file.
    pub index: Option<u32>,
    /// Stream kind: `"video"`, `"audio"`, `"subtitle"`, `"data"`, ...
    pub codec_type: Option<String>,
    /// Codec identifier (`h264`, `aac`, `mov_text`, ...).
    pub codec_name: Option<String>,
    /// Long codec name.
    pub codec_long_name: Option<String>,
    /// Codec profile (`High`, `LC`, ...).
    pub profile: Option<String>,
    /// Display resolution — video streams only.
    pub resolution: Option<Resolution>,
    /// Pixel format (`yuv420p`, ...) — video only.
    pub pix_fmt: Option<String>,
    /// Average frame rate in fps — video only.
    pub avg_frame_rate: Option<f64>,
    /// Sample rate in Hz — audio only.
    pub sample_rate: Option<u64>,
    /// Channel count — audio only.
    pub channels: Option<u32>,
    /// Channel layout (`stereo`, `5.1`, ...) — audio only.
    pub channel_layout: Option<String>,
    /// Language tag from stream tags (`eng`, ...).
    pub language: Option<String>,
    /// Stream title from tags.
    pub title: Option<String>,
    /// Whether the stream is flagged default for its type.
    pub is_default: bool,
    /// Comma-separated summary of set disposition flags
    /// (`"default,forced"`); empty when none are set.
    pub disposition: String,
    /// Attached-picture (cover art) flag.
    pub attached_pic: bool,
    /// Stream bit rate in bits/s.
    pub bitrate: Option<u64>,
    /// Stream duration in seconds.
    pub duration_secs: Option<f64>,
    /// Number of frames/samples, when known.
    pub nb_frames: Option<u64>,
}

impl From<&crate::crates::multimedia_engine::probe::types::MediaStream> for StreamDetail {
    fn from(s: &crate::crates::multimedia_engine::probe::types::MediaStream) -> Self {
        let resolution = match (s.width, s.height) {
            (Some(width), Some(height)) => Some(Resolution { width, height }),
            _ => None,
        };
        Self {
            index: s.index,
            codec_type: s.codec_type.clone(),
            codec_name: s.codec_name.clone(),
            codec_long_name: s.codec_long_name.clone(),
            profile: s.profile.clone(),
            resolution,
            pix_fmt: s.pix_fmt.clone(),
            avg_frame_rate: s
                .avg_frame_rate
                .as_deref()
                .or(s.r_frame_rate.as_deref())
                .and_then(parse_rate),
            sample_rate: s.sample_rate.as_deref().and_then(parse_u64),
            channels: s.channels,
            channel_layout: s.channel_layout.clone(),
            language: s.language.clone(),
            title: s.title.clone(),
            is_default: s.is_default(),
            disposition: disposition_summary(&s.disposition),
            attached_pic: s.disposition.attached_pic.unwrap_or(0) != 0,
            bitrate: s.bit_rate.as_deref().and_then(parse_u64),
            duration_secs: s.duration_secs(),
            nb_frames: s.nb_frames.as_deref().and_then(parse_u64),
        }
    }
}

/// Probe `input` with ffprobe and return every stream plus format
/// duration. Includes subtitle, data and attachment streams — use
/// `probe::inspect` for the summarized view.
///
/// The input is written to a temp file because ffprobe needs a
/// seekable path; the temp dir is removed on return.
pub fn probe_streams(input: &[u8], limits: &MediaLimits) -> MediaResult<StreamReport> {
    if let Some(max) = limits.max_input_bytes
        && input.len() as u64 > max
    {
        return Err(MediaError::LimitExceeded(format!(
            "input is {} bytes, exceeding max_input_bytes {max}",
            input.len()
        )));
    }

    let runtime = Runtime::discover()?;
    let dir = TempDir::new("probe-streams")?;
    let input_path = dir.write("input.bin", input)?;

    let args: Vec<std::ffi::OsString> = [
        "-v",
        "error",
        "-show_streams",
        "-show_format",
        "-of",
        "json",
        input_path.to_string_lossy().as_ref(),
    ]
    .iter()
    .map(std::ffi::OsString::from)
    .collect();

    let out = process::run_checked("ffprobe", &runtime.ffprobe, &args, None, limits)?;
    let report: ProbeReport = serde_json::from_slice(&out.stdout)
        .map_err(|e| MediaError::InvalidInput(format!("ffprobe produced unparseable JSON: {e}")))?;

    Ok(StreamReport {
        streams: report.streams.iter().map(StreamDetail::from).collect(),
        format_name: report.format.as_ref().and_then(|f| f.format_name.clone()),
        duration_secs: report.format.as_ref().and_then(|f| f.duration_secs()),
    })
}

/// Parse `"30000/1001"` or `"25"` into a float rate.
fn parse_rate(s: &str) -> Option<f64> {
    let (num, den) = s.split_once('/').unwrap_or((s, "1"));
    let n: f64 = num.trim().parse().ok()?;
    let d: f64 = den.trim().parse().ok()?;
    if d == 0.0 { None } else { Some(n / d) }
}

/// Parse a decimal string (`"48000"`, `"192000"`) into u64.
fn parse_u64(s: &str) -> Option<u64> {
    s.trim().parse().ok()
}

/// Comma-separated names of disposition flags set to non-zero.
fn disposition_summary(d: &crate::crates::multimedia_engine::probe::types::Disposition) -> String {
    let flags: &[(&str, Option<i64>)] = &[
        ("default", d.default),
        ("dub", d.dub),
        ("original", d.original),
        ("comment", d.comment),
        ("lyrics", d.lyrics),
        ("karaoke", d.karaoke),
        ("forced", d.forced),
        ("hearing_impaired", d.hearing_impaired),
        ("visual_impaired", d.visual_impaired),
        ("clean_effects", d.clean_effects),
        ("attached_pic", d.attached_pic),
        ("timed_thumbnails", d.timed_thumbnails),
        ("captions", d.captions),
        ("descriptions", d.descriptions),
        ("metadata", d.metadata),
        ("dependent", d.dependent),
        ("still_image", d.still_image),
    ];
    flags
        .iter()
        .filter(|(_, v)| v.unwrap_or(0) != 0)
        .map(|(name, _)| *name)
        .collect::<Vec<_>>()
        .join(",")
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::crates::multimedia_engine::runtime::discovery::find_tool;
    use std::path::Path;
    use std::process::Command;

    /// Build a 1s mp4 with one video (160x120 @10fps, yuv420p) and one
    /// audio stream via lavfi. Returns the file bytes.
    fn make_av_mp4() -> Option<Vec<u8>> {
        let dir = TempDir::new("streams-test-src").ok()?;
        let out = dir.file("av.mp4");
        let base = [
            "-y",
            "-v",
            "error",
            "-f",
            "lavfi",
            "-i",
            "testsrc=duration=1:size=160x120:rate=10",
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
        ];
        let status = Command::new("ffmpeg").args(base).arg(&out).status().ok()?;
        if !status.success() {
            // Fall back to the always-present native mpeg4 encoder.
            let mut args: Vec<&str> = base.to_vec();
            args.retain(|a| *a != "libx264" && *a != "-pix_fmt" && *a != "yuv420p");
            let status = Command::new("ffmpeg")
                .args(&args)
                .arg("-c:v")
                .arg("mpeg4")
                .arg(&out)
                .status()
                .ok()?;
            if !status.success() {
                return None;
            }
        }
        std::fs::read(&out).ok()
    }

    /// Run ffmpeg with the given args inside `dir`; returns success.
    fn ffmpeg(dir: &TempDir, args: &[&str]) -> bool {
        Command::new("ffmpeg")
            .args(args)
            .current_dir(dir.path())
            .status()
            .map(|s| s.success())
            .unwrap_or(false)
    }

    #[test]
    fn probe_streams_reports_video_and_audio() {
        if find_tool("ffmpeg").is_none() || find_tool("ffprobe").is_none() {
            eprintln!("skipping: ffmpeg/ffprobe not on PATH");
            return;
        }
        let Some(bytes) = make_av_mp4() else {
            eprintln!("skipping: could not build test mp4");
            return;
        };
        let report = probe_streams(&bytes, &MediaLimits::default()).expect("probe_streams");

        assert_eq!(report.streams.len(), 2, "expected video+audio streams");
        assert!(
            report.format_name.as_deref().unwrap_or("").contains("mp4")
                || report.format_name.as_deref().unwrap_or("").contains("mov"),
            "format_name should mention mp4/mov, got {:?}",
            report.format_name
        );
        let dur = report.duration_secs.expect("container duration");
        assert!((dur - 1.0).abs() < 0.5, "duration {dur}");

        let video = report
            .streams
            .iter()
            .find(|s| s.codec_type.as_deref() == Some("video"))
            .expect("video stream");
        assert!(
            matches!(video.codec_name.as_deref(), Some("h264") | Some("mpeg4")),
            "unexpected video codec {:?}",
            video.codec_name
        );
        let res = video.resolution.expect("video resolution");
        assert_eq!((res.width, res.height), (160, 120));
        assert!(video.pix_fmt.is_some());
        let fps = video.avg_frame_rate.expect("frame rate");
        assert!((fps - 10.0).abs() < 0.5, "fps {fps}");

        let audio = report
            .streams
            .iter()
            .find(|s| s.codec_type.as_deref() == Some("audio"))
            .expect("audio stream");
        assert_eq!(audio.codec_name.as_deref(), Some("aac"));
        assert!(audio.sample_rate.is_some());
        assert!(audio.channels.is_some());

        // Disposition flags parse into `is_default`/`disposition`
        // without panicking; at least the video stream should report
        // some value for either field.
        for s in &report.streams {
            assert!(
                s.is_default || !s.disposition.is_empty() || s.attached_pic,
                "stream {:?} has no disposition info at all",
                s.index
            );
        }
    }

    #[test]
    fn probe_streams_includes_subtitle_stream() {
        if find_tool("ffmpeg").is_none() || find_tool("ffprobe").is_none() {
            eprintln!("skipping: ffmpeg/ffprobe not on PATH");
            return;
        }
        let Some(av) = make_av_mp4() else {
            eprintln!("skipping: could not build test mp4");
            return;
        };
        let dir = match TempDir::new("streams-test-sub") {
            Ok(d) => d,
            Err(_) => return,
        };
        let _ = dir.write("av.mp4", &av);
        let srt = "1\n00:00:00,000 --> 00:00:00,500\nHello\n\n\
                   2\n00:00:00,500 --> 00:00:01,000\nWorld\n";
        let _ = dir.write("sub.srt", srt.as_bytes());

        let ok = ffmpeg(
            &dir,
            &[
                "-y", "-v", "error", "-i", "av.mp4", "-i", "sub.srt", "-map", "0", "-map", "1",
                "-c", "copy", "-c:s", "mov_text", "sub.mp4",
            ],
        );
        if !ok || !Path::new(&dir.file("sub.mp4")).exists() {
            eprintln!("skipping: subtitle mux failed");
            return;
        }
        let sub_bytes = std::fs::read(dir.file("sub.mp4")).expect("read sub.mp4");
        let report = probe_streams(&sub_bytes, &MediaLimits::default()).expect("probe_streams");

        assert_eq!(report.streams.len(), 3, "expected video+audio+subtitle");
        let sub = report
            .streams
            .iter()
            .find(|s| s.codec_type.as_deref() == Some("subtitle"))
            .expect("subtitle stream");
        assert_eq!(sub.codec_name.as_deref(), Some("mov_text"));
    }

    #[test]
    fn probe_streams_rejects_oversized_input() {
        let limits = MediaLimits {
            max_input_bytes: Some(4),
            ..MediaLimits::default()
        };
        let err = probe_streams(&[0u8; 8], &limits).unwrap_err();
        assert!(matches!(err, MediaError::LimitExceeded(_)));
    }
}
