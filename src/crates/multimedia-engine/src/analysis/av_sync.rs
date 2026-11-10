//! Audio/video synchronization analysis via `ffprobe` stream timing.
//!
//! This module answers "are the audio and video tracks roughly aligned
//! in this file?" using **container-level timestamps only**: it reads
//! each stream's `start_time` and `duration` from
//! `ffprobe -show_streams -show_format -of json` and compares the
//! primary video stream against the primary audio stream.
//!
//! What this measures:
//! - [`AvSyncReport::offset_ms`] — `audio_start - video_start` in
//!   milliseconds. Positive means audio begins *later* than video
//!   (lip-sync late); negative means audio starts early.
//! - [`AvSyncReport::duration_diff_ms`] — `audio_duration -
//!   video_duration` in milliseconds. Positive means the audio track
//!   runs longer than the video.
//!
//! What this does **not** measure — important caveats:
//! - Codec priming (e.g. AAC encoder delay ~20–45 ms) and edit lists
//!   are folded into `start_time` differently per container; treat
//!   offsets below ~50 ms as unreliable.
//! - Content-level drift *within* the file (audio gradually slipping
//!   against video over minutes) is invisible to container timestamps.
//!   Detecting that requires decoding and correlating frame/sample
//!   PTS — out of scope here.
//! - Precision is bounded by the container timebase; ffprobe prints
//!   `start_time`/`duration` with microsecond-ish precision but the
//!   underlying values are quantized to stream timebase ticks.

use std::ffi::OsString;

use serde::{Deserialize, Serialize};

use crate::crates::multimedia_engine::error::{MediaError, MediaResult};
use crate::crates::multimedia_engine::model::MediaLimits;
use crate::crates::multimedia_engine::probe::types::ProbeReport;
use crate::crates::multimedia_engine::runtime::discovery::Runtime;
use crate::crates::multimedia_engine::runtime::{process, tempdir::TempDir};

/// Options for [`av_sync`].
#[derive(Debug, Clone, Copy, PartialEq, Serialize, Deserialize)]
pub struct AvSyncOptions {
    /// Maximum acceptable |audio/video start offset| in
    /// milliseconds. [`AvSyncReport::sync_ok`] is `true` when the
    /// measured offset is within `±tolerance_ms`. Default `45.0` —
    /// roughly the threshold where casual viewers notice A/V
    /// desync, and above typical AAC priming noise.
    pub tolerance_ms: f64,
}

impl Default for AvSyncOptions {
    fn default() -> Self {
        Self { tolerance_ms: 45.0 }
    }
}

/// Timing detail for one stream, carried through for callers that
/// want the raw numbers behind the comparison.
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
pub struct AvSyncStream {
    /// Stream index within the file.
    pub index: Option<u32>,
    /// Stream kind (`"video"`, `"audio"`, `"subtitle"`, ...).
    pub codec_type: Option<String>,
    /// Codec identifier (`h264`, `aac`, ...).
    pub codec_name: Option<String>,
    /// `start_time` parsed to seconds, when reported.
    pub start_secs: Option<f64>,
    /// `duration` parsed to seconds, when reported.
    pub duration_secs: Option<f64>,
}

/// Result of an A/V sync check.
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
pub struct AvSyncReport {
    /// Primary video stream `start_time` in seconds.
    pub video_start_secs: Option<f64>,
    /// Primary audio stream `start_time` in seconds.
    pub audio_start_secs: Option<f64>,
    /// `audio_start - video_start` in milliseconds. Positive = audio
    /// delayed relative to video. `None` when either start time is
    /// missing or unparseable.
    pub offset_ms: Option<f64>,
    /// Primary video stream `duration` in seconds.
    pub video_duration: Option<f64>,
    /// Primary audio stream `duration` in seconds.
    pub audio_duration: Option<f64>,
    /// `audio_duration - video_duration` in milliseconds. Positive =
    /// audio track runs longer.
    pub duration_diff_ms: Option<f64>,
    /// `|offset_ms| <= tolerance_ms`. `false` when the offset could
    /// not be measured — absence of data is not "in sync".
    pub sync_ok: bool,
    /// Every stream's timing, in container order.
    pub streams: Vec<AvSyncStream>,
    /// Human-readable caveats (missing fields, precision limits,
    /// absent audio/video tracks).
    pub warnings: Vec<String>,
}

/// Parse ffprobe's stringly-typed seconds fields.
fn parse_secs(s: Option<&str>) -> Option<f64> {
    s.and_then(|v| v.trim().parse::<f64>().ok())
}

/// Pick the primary stream of `kind`: the default-flagged one when
/// present, otherwise the first in stream order — same policy as
/// `probe::inspect`.
fn primary<'a>(
    report: &'a ProbeReport,
    kind: &str,
) -> Option<&'a crate::crates::multimedia_engine::probe::types::MediaStream> {
    report
        .streams
        .iter()
        .find(|s| s.kind() == Some(kind) && s.is_default())
        .or_else(|| report.streams.iter().find(|s| s.kind() == Some(kind)))
}

/// Sniff a container extension from magic bytes so ffprobe gets a
/// format hint. Same policy as `probe::inspect::sniff_extension`.
fn sniff_extension(data: &[u8]) -> &'static str {
    if data.len() >= 12 && &data[4..8] == b"ftyp" {
        return if &data[8..12] == b"qt  " {
            ".mov"
        } else {
            ".mp4"
        };
    }
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
    if data.starts_with(b"ID3") || (data.len() >= 2 && data[0] == 0xFF && (data[1] & 0xE0) == 0xE0)
    {
        return ".mp3";
    }
    ".bin"
}

/// Measure audio/video synchronization of `input` with ffprobe.
///
/// Runs `ffprobe -v error -print_format json -show_format
/// -show_streams` against a temp copy of the input and compares the
/// primary video stream's `start_time`/`duration` with the primary
/// audio stream's. The offset sign convention and precision caveats
/// are documented in the module header — read them before treating
/// small offsets as meaningful.
///
/// # Errors
///
/// - [`MediaError::LimitExceeded`] when `input` exceeds
///   `limits.max_input_bytes`.
/// - [`MediaError::InvalidInput`] when `tolerance_ms` is not finite
///   or negative, when ffprobe fails, or when its JSON cannot be
///   parsed / contains no media.
pub fn av_sync(
    input: &[u8],
    options: &AvSyncOptions,
    limits: &MediaLimits,
    rt: &Runtime,
) -> MediaResult<AvSyncReport> {
    if !options.tolerance_ms.is_finite() || options.tolerance_ms < 0.0 {
        return Err(MediaError::InvalidInput(format!(
            "tolerance_ms must be a finite non-negative number, got {}",
            options.tolerance_ms
        )));
    }

    if let Some(max) = limits.max_input_bytes
        && (input.len() as u64) > max
    {
        return Err(MediaError::LimitExceeded(format!(
            "input is {} bytes, exceeding max_input_bytes {max}",
            input.len()
        )));
    }

    let dir = TempDir::new("av-sync")?;
    let in_path = dir.write(&format!("input{}", sniff_extension(input)), input)?;

    let args: Vec<OsString> = [
        "-v",
        "error",
        "-print_format",
        "json",
        "-show_format",
        "-show_streams",
    ]
    .iter()
    .map(OsString::from)
    .chain(std::iter::once(in_path.into_os_string()))
    .collect();
    let out = process::run("ffprobe", &rt.ffprobe, &args, None, limits)?;
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
    if report.streams.is_empty() {
        return Err(MediaError::InvalidInput(
            "ffprobe found no media streams".to_string(),
        ));
    }

    let video = primary(&report, "video");
    let audio = primary(&report, "audio");

    let mut warnings: Vec<String> = Vec::new();
    if video.is_none() {
        warnings.push("no video stream found; sync cannot be assessed".to_string());
    }
    if audio.is_none() {
        warnings.push("no audio stream found; sync cannot be assessed".to_string());
    }

    let video_start = video.and_then(|s| parse_secs(s.start_time.as_deref()));
    let audio_start = audio.and_then(|s| parse_secs(s.start_time.as_deref()));
    let video_duration = video.and_then(|s| s.duration_secs());
    let audio_duration = audio.and_then(|s| s.duration_secs());

    if video.is_some() && video_start.is_none() {
        warnings.push("primary video stream has no parseable start_time".to_string());
    }
    if audio.is_some() && audio_start.is_none() {
        warnings.push("primary audio stream has no parseable start_time".to_string());
    }
    if video.is_some() && video_duration.is_none() {
        warnings.push("primary video stream has no parseable duration".to_string());
    }
    if audio.is_some() && audio_duration.is_none() {
        warnings.push("primary audio stream has no parseable duration".to_string());
    }

    let offset_ms = match (video_start, audio_start) {
        (Some(v), Some(a)) => Some((a - v) * 1000.0),
        _ => None,
    };
    let duration_diff_ms = match (video_duration, audio_duration) {
        (Some(v), Some(a)) => Some((a - v) * 1000.0),
        _ => None,
    };

    let sync_ok = offset_ms.is_some_and(|o| o.abs() <= options.tolerance_ms);

    if offset_ms.is_some() {
        warnings.push(
            "offset derived from container stream timestamps; codec priming, edit lists \
             and intra-file A/V drift are not measured (sub-~50ms offsets are unreliable)"
                .to_string(),
        );
    }

    Ok(AvSyncReport {
        video_start_secs: video_start,
        audio_start_secs: audio_start,
        offset_ms,
        video_duration,
        audio_duration,
        duration_diff_ms,
        sync_ok,
        streams: report
            .streams
            .iter()
            .map(|s| AvSyncStream {
                index: s.index,
                codec_type: s.codec_type.clone(),
                codec_name: s.codec_name.clone(),
                start_secs: parse_secs(s.start_time.as_deref()),
                duration_secs: s.duration_secs(),
            })
            .collect(),
        warnings,
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::crates::multimedia_engine::runtime::discovery;

    /// Generate a small mp4: 1s of 64x64@10fps testsrc video plus
    /// `audio_offset_secs`-delayed 440Hz sine audio (via `-itsoffset`
    /// on the audio input). Returns `None` when ffmpeg or the needed
    /// encoders are unavailable so tests skip gracefully.
    fn make_av_clip(audio_offset_secs: f64) -> Option<Vec<u8>> {
        let ffmpeg = discovery::find_tool("ffmpeg")?;
        let dir = TempDir::new("av-sync-test").ok()?;
        let out_path = dir.file("clip.mp4");
        let offset = format!("{audio_offset_secs}");
        let args: Vec<OsString> = [
            "-y",
            "-v",
            "error",
            "-f",
            "lavfi",
            "-i",
            "testsrc=size=64x64:rate=10:duration=1",
            "-itsoffset",
            &offset,
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
    fn av_sync_detects_delayed_audio() {
        let Some(clip) = make_av_clip(0.3) else {
            eprintln!("skipping: ffmpeg lavfi/x264/aac unavailable");
            return;
        };
        let rt = match Runtime::discover() {
            Ok(rt) => rt,
            Err(_) => return,
        };
        let report = av_sync(
            &clip,
            &AvSyncOptions::default(),
            &MediaLimits::default(),
            &rt,
        )
        .expect("av_sync failed");

        let offset = report.offset_ms.expect("offset_ms");
        // Audio was delayed 300ms; codec priming/muxer rounding can
        // shift it by tens of ms, so assert sign and a loose band.
        assert!(
            offset > 100.0 && offset < 500.0,
            "expected ~+300ms audio delay, got {offset} (report: {report:?})"
        );
        assert!(!report.sync_ok, "300ms offset must exceed 45ms tolerance");
        assert_eq!(report.streams.len(), 2);
        assert!(
            report.warnings.iter().any(|w| w.contains("container")),
            "expected precision caveat warning, got {:?}",
            report.warnings
        );
    }

    #[test]
    fn av_sync_accepts_aligned_clip_with_wide_tolerance() {
        let Some(clip) = make_av_clip(0.0) else {
            eprintln!("skipping: ffmpeg lavfi/x264/aac unavailable");
            return;
        };
        let rt = match Runtime::discover() {
            Ok(rt) => rt,
            Err(_) => return,
        };
        // Wide tolerance absorbs AAC priming / container rounding.
        let opts = AvSyncOptions {
            tolerance_ms: 250.0,
        };
        let report = av_sync(&clip, &opts, &MediaLimits::default(), &rt).expect("av_sync failed");
        let offset = report.offset_ms.expect("offset_ms");
        assert!(
            offset.abs() < 150.0,
            "expected near-zero offset, got {offset}"
        );
        assert!(report.sync_ok);
    }

    #[test]
    fn av_sync_rejects_garbage_input() {
        let rt = match Runtime::discover() {
            Ok(rt) => rt,
            Err(_) => return,
        };
        let garbage = vec![0xDEu8, 0xAD, 0xBE, 0xEF, 1, 2, 3, 4, 5, 6, 7, 8];
        let err = av_sync(
            &garbage,
            &AvSyncOptions::default(),
            &MediaLimits::default(),
            &rt,
        )
        .expect_err("must fail");
        assert!(
            matches!(err, MediaError::InvalidInput(_)),
            "unexpected error: {err:?}"
        );
    }

    #[test]
    fn av_sync_enforces_max_input_bytes() {
        let rt = match Runtime::discover() {
            Ok(rt) => rt,
            Err(_) => return,
        };
        let limits = MediaLimits {
            max_input_bytes: Some(16),
            ..MediaLimits::default()
        };
        let err =
            av_sync(&[0u8; 64], &AvSyncOptions::default(), &limits, &rt).expect_err("must fail");
        assert!(
            matches!(err, MediaError::LimitExceeded(_)),
            "unexpected error: {err:?}"
        );
    }

    #[test]
    fn av_sync_rejects_bad_tolerance() {
        let rt = match Runtime::discover() {
            Ok(rt) => rt,
            Err(_) => return,
        };
        for bad in [f64::NAN, f64::INFINITY, -1.0] {
            let opts = AvSyncOptions { tolerance_ms: bad };
            assert!(
                av_sync(b"x", &opts, &MediaLimits::default(), &rt).is_err(),
                "tolerance {bad} must be rejected"
            );
        }
    }
}
