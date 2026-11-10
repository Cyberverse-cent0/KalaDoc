//! A/V synchronization correction and drift handling.
//!
//! [`av_sync_report`] inspects the probed `start_time` of the first
//! video and audio streams and reports them (plus zero applied
//! shifts — nothing is modified). [`av_sync_fix`] re-encodes the
//! media with per-stream input offsets: the same file is opened
//! twice and `-itsoffset` is applied per input, then video is mapped
//! from the first input and audio from the second, so each stream's
//! timeline can be shifted independently. [`av_sync_auto`] measures
//! the start-time delta and applies the correction that aligns the
//! two streams.
//!
//! Offset sign convention (both [`SyncParams`] fields): **positive
//! delays the stream** (it starts later), negative advances it. This
//! matches ffmpeg's `-itsoffset`, which adds the offset to the
//! input's timestamps.
//!
//! Because shifting timestamps requires re-encoding for reliable
//! results across containers, [`av_sync_fix`] always emits MP4
//! (H.264 via [`h264_aac_args`] + AAC). Non-AV streams (subtitles,
//! data, attachments) are dropped — only `0:v` and `1:a` are mapped.

use serde_json::Value;

use crate::crates::core::engine::EngineLimits;
use crate::crates::video_engine::error::{VideoError, VideoResult};
use crate::crates::video_engine::ops::probe::probe;
use crate::crates::video_engine::ops::{
    COMMAND_TIMEOUT, FFMPEG, TempDir, check_input, check_limits, find_tool, h264_aac_args,
    probe_json, read_output, run_tool, tool_failure, write_input,
};

/// Manual A/V shift parameters for [`av_sync_fix`].
///
/// Both fields are milliseconds; **positive delays that stream**
/// (its start moves later), negative advances it. `0` leaves the
/// stream untouched.
#[derive(Debug, Clone, Copy, Default)]
pub struct SyncParams {
    /// Shift applied to the audio stream(s): positive = audio starts
    /// later (lip-sync fix when audio is early), negative = audio
    /// starts earlier.
    pub audio_offset_ms: i64,
    /// Shift applied to the video stream(s): positive = video starts
    /// later, negative = video starts earlier.
    pub video_offset_ms: i64,
}

/// Result of an A/V sync inspection ([`av_sync_report`]).
///
/// `*_start_ms` are the container-reported stream start times in
/// milliseconds (`None` when ffprobe reported `"N/A"` or the stream
/// kind is absent). `applied_*_shift_ms` record the offsets actually
/// applied — always `0` for a report-only call.
#[derive(Debug, Clone, Copy)]
pub struct SyncReport {
    /// First audio stream's `start_time` in ms, when reported.
    pub audio_start_ms: Option<i64>,
    /// First video stream's `start_time` in ms, when reported.
    pub video_start_ms: Option<i64>,
    /// Audio shift applied by the producing operation (ms).
    pub applied_audio_shift_ms: i64,
    /// Video shift applied by the producing operation (ms).
    pub applied_video_shift_ms: i64,
}

/// Parse a stream's ffprobe `start_time` into milliseconds.
///
/// ffprobe emits it as a decimal string (`"0.276009"`) on most
/// demuxers, occasionally a JSON number, and `"N/A"` when unknown —
/// all non-finite or unparseable values yield `None`.
fn stream_start_ms(stream: &Value) -> Option<i64> {
    let raw = stream.get("start_time")?;
    let secs = raw
        .as_f64()
        .or_else(|| raw.as_str().and_then(|s| s.trim().parse::<f64>().ok()))?;
    secs.is_finite().then(|| (secs * 1000.0).round() as i64)
}

/// Whether a stream object is attached cover art (same heuristic as
/// `ops::probe`: `disposition.attached_pic` non-zero).
fn is_attached_pic(stream: &Value) -> bool {
    stream
        .get("disposition")
        .and_then(|d| d.get("attached_pic"))
        .and_then(|v| {
            v.as_i64()
                .or_else(|| v.as_str().and_then(|s| s.parse::<i64>().ok()))
        })
        .map(|v| v != 0)
        .unwrap_or(false)
}

/// Extract `(video_start_ms, audio_start_ms)` from a `probe_json`
/// document. The first video stream that is not attached cover art
/// and the first audio stream win; streams lacking a parseable
/// `start_time` yield `None` for that slot.
fn stream_starts(json: &Value) -> (Option<i64>, Option<i64>) {
    let mut video = None;
    let mut audio = None;
    if let Some(streams) = json.get("streams").and_then(Value::as_array) {
        for s in streams {
            match s.get("codec_type").and_then(Value::as_str) {
                Some("video") if video.is_none() && !is_attached_pic(s) => {
                    video = stream_start_ms(s);
                }
                Some("audio") if audio.is_none() => {
                    audio = stream_start_ms(s);
                }
                _ => {}
            }
        }
    }
    (video, audio)
}

/// Report the probed start times of the primary video and audio
/// streams. Nothing is modified; `applied_*_shift_ms` are `0`.
///
/// Streams without a reported `start_time` (ffprobe `"N/A"`) produce
/// `None` in the corresponding field rather than an error.
pub(crate) fn av_sync_report(input: &[u8]) -> VideoResult<SyncReport> {
    check_input(input)?;
    let dir = TempDir::new("sync-report")?;
    let input_path = write_input(&dir, "input.bin", input)?;
    let json = probe_json(&input_path)?;
    let (video_start_ms, audio_start_ms) = stream_starts(&json);
    Ok(SyncReport {
        audio_start_ms,
        video_start_ms,
        applied_audio_shift_ms: 0,
        applied_video_shift_ms: 0,
    })
}

/// Shift the audio and video timelines relative to each other by the
/// requested offsets and return the re-encoded MP4 bytes.
///
/// The input file is opened twice: `-itsoffset {video_offset}` on
/// the first input (video source) and `-itsoffset {audio_offset}` on
/// the second (audio source). `-map 0:v -map 1:a` then takes video
/// from input 0 and audio from input 1, giving each stream an
/// independent shift — `itsoffset` cannot be applied per-stream
/// within a single input.
///
/// Positive offsets delay a stream; negative offsets advance it.
pub(crate) fn av_sync_fix(
    input: &[u8],
    params: &SyncParams,
    limits: &EngineLimits,
) -> VideoResult<Vec<u8>> {
    let input_len = check_input(input)?;
    let program = find_tool(FFMPEG)?;

    // Probe once: limits, media validation, and has_video/has_audio.
    // Both stream kinds are required — the mapping below cannot
    // succeed without them.
    let info = probe(input)?;
    check_limits(&info, input_len, limits, false)?;
    if !info.has_video {
        return Err(VideoError::InvalidInput(
            "av_sync_fix requires a video stream".to_string(),
        ));
    }
    if !info.has_audio {
        return Err(VideoError::InvalidInput(
            "av_sync_fix requires an audio stream".to_string(),
        ));
    }

    let dir = TempDir::new("sync-fix")?;
    let input_path = write_input(&dir, "input.bin", input)?;
    let out_path = dir.file("out.mp4");

    let video_off = params.video_offset_ms as f64 / 1000.0;
    let audio_off = params.audio_offset_ms as f64 / 1000.0;
    let input_arg = input_path.to_string_lossy().into_owned();

    let mut args = vec![
        "-y".to_string(),
        "-v".to_string(),
        "error".to_string(),
        // Input 0: video timeline shifted by video_offset_ms.
        "-itsoffset".to_string(),
        format!("{video_off:.6}"),
        "-i".to_string(),
        input_arg.clone(),
        // Input 1: audio timeline shifted by audio_offset_ms.
        "-itsoffset".to_string(),
        format!("{audio_off:.6}"),
        "-i".to_string(),
        input_arg,
        // Video from the first input, audio from the second.
        "-map".to_string(),
        "0:v".to_string(),
        "-map".to_string(),
        "1:a".to_string(),
    ];
    args.extend(h264_aac_args()?);
    args.push(out_path.to_string_lossy().into_owned());

    let output = run_tool(FFMPEG, &program, &args, COMMAND_TIMEOUT)?;
    if !output.status.success() {
        return Err(tool_failure(FFMPEG, &output));
    }
    read_output(&out_path)
}

/// Automatically align the audio and video start times.
///
/// Probes both streams' `start_time`, computes the audio shift that
/// makes the audio start coincide with the video start
/// (`audio_offset_ms = video_start - audio_start`), and delegates to
/// [`av_sync_fix`]. Fails with [`VideoError::InvalidInput`] when
/// either stream lacks a reported start time — there is no honest
/// basis for a correction then. The output is the same re-encoded
/// MP4 [`av_sync_fix`] produces.
pub(crate) fn av_sync_auto(input: &[u8], limits: &EngineLimits) -> VideoResult<Vec<u8>> {
    check_input(input)?;
    let dir = TempDir::new("sync-auto")?;
    let input_path = write_input(&dir, "input.bin", input)?;
    let json = probe_json(&input_path)?;
    let (video_start_ms, audio_start_ms) = stream_starts(&json);
    let (Some(video), Some(audio)) = (video_start_ms, audio_start_ms) else {
        return Err(VideoError::InvalidInput(
            "av_sync_auto requires a reported start_time on both the video \
             and audio streams"
                .to_string(),
        ));
    };
    let params = SyncParams {
        audio_offset_ms: video.saturating_sub(audio),
        video_offset_ms: 0,
    };
    av_sync_fix(input, &params, limits)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::crates::video_engine::ops::{FFPROBE, find_tool};
    use std::fs;

    fn tools_available() -> bool {
        find_tool(FFMPEG).is_ok() && find_tool(FFPROBE).is_ok()
    }

    /// Build a tiny 1s H.264+AAC mp4 via lavfi with in-sync streams.
    /// Returns `None` when ffmpeg or an H.264 encoder is unavailable.
    fn make_fixture() -> Option<Vec<u8>> {
        make_av_fixture(None)
    }

    /// Same as [`make_fixture`] but with the audio input delayed by
    /// `audio_delay_secs` via `-itsoffset` on the lavfi input, so the
    /// muxed audio stream genuinely starts late.
    fn make_offset_fixture(audio_delay_secs: f64) -> Option<Vec<u8>> {
        make_av_fixture(Some(audio_delay_secs))
    }

    fn make_av_fixture(audio_delay_secs: Option<f64>) -> Option<Vec<u8>> {
        let program = find_tool(FFMPEG).ok()?;
        let enc = crate::crates::video_engine::ops::h264_encoder()?;
        let dir = TempDir::new("sync-test").ok()?;
        let out = dir.file("fixture.mp4");
        let mut args = vec![
            "-y".to_string(),
            "-v".to_string(),
            "error".to_string(),
            "-f".to_string(),
            "lavfi".to_string(),
            "-i".to_string(),
            "testsrc=duration=1:size=160x120:rate=10".to_string(),
        ];
        if let Some(delay) = audio_delay_secs {
            args.push("-itsoffset".to_string());
            args.push(format!("{delay:.3}"));
        }
        args.extend([
            "-f".to_string(),
            "lavfi".to_string(),
            "-i".to_string(),
            "sine=frequency=440:duration=1.3".to_string(),
            "-c:v".to_string(),
            enc.to_string(),
            "-pix_fmt".to_string(),
            "yuv420p".to_string(),
            "-c:a".to_string(),
            "aac".to_string(),
            "-shortest".to_string(),
            out.to_string_lossy().into_owned(),
        ]);
        let output = run_tool(FFMPEG, &program, &args, COMMAND_TIMEOUT).ok()?;
        if !output.status.success() {
            return None;
        }
        fs::read(out).ok()
    }

    /// Video-only fixture (no audio stream).
    fn make_video_only_fixture() -> Option<Vec<u8>> {
        let program = find_tool(FFMPEG).ok()?;
        let enc = crate::crates::video_engine::ops::h264_encoder()?;
        let dir = TempDir::new("sync-test").ok()?;
        let out = dir.file("video-only.mp4");
        let args = vec![
            "-y".to_string(),
            "-v".to_string(),
            "error".to_string(),
            "-f".to_string(),
            "lavfi".to_string(),
            "-i".to_string(),
            "testsrc=duration=1:size=160x120:rate=10".to_string(),
            "-c:v".to_string(),
            enc.to_string(),
            "-pix_fmt".to_string(),
            "yuv420p".to_string(),
            "-an".to_string(),
            out.to_string_lossy().into_owned(),
        ];
        let output = run_tool(FFMPEG, &program, &args, COMMAND_TIMEOUT).ok()?;
        if !output.status.success() {
            return None;
        }
        fs::read(out).ok()
    }

    #[test]
    fn report_populates_stream_start_times() {
        if !tools_available() {
            return;
        }
        let Some(fixture) = make_fixture() else {
            return;
        };
        let report = av_sync_report(&fixture).unwrap();
        assert!(report.video_start_ms.is_some(), "video start missing");
        assert!(report.audio_start_ms.is_some(), "audio start missing");
        assert_eq!(report.applied_audio_shift_ms, 0);
        assert_eq!(report.applied_video_shift_ms, 0);
    }

    #[test]
    fn report_detects_deliberate_audio_delay() {
        if !tools_available() {
            return;
        }
        let Some(fixture) = make_offset_fixture(0.3) else {
            return;
        };
        let report = av_sync_report(&fixture).unwrap();
        let (Some(v), Some(a)) = (report.video_start_ms, report.audio_start_ms) else {
            return;
        };
        // The audio stream should start ~300ms late (AAC encoder
        // priming may shave a few ms off the container start_time).
        let delta = a - v;
        assert!(
            delta > 150 && delta < 500,
            "expected audio ~300ms behind video, got delta {delta}ms"
        );
    }

    #[test]
    fn fix_applies_requested_audio_delay() {
        if !tools_available() {
            return;
        }
        let Some(fixture) = make_fixture() else {
            return;
        };
        let params = SyncParams {
            audio_offset_ms: 200,
            video_offset_ms: 0,
        };
        let out = av_sync_fix(&fixture, &params, &EngineLimits::default()).unwrap();
        let report = av_sync_report(&out).unwrap();
        let (Some(v), Some(a)) = (report.video_start_ms, report.audio_start_ms) else {
            return;
        };
        let delta = a - v;
        assert!(
            (100..=400).contains(&delta),
            "expected audio ~200ms behind video, got delta {delta}ms"
        );
        // Output stays a sane ~1s file.
        let info = probe(&out).unwrap();
        let d = info.duration_secs.unwrap_or(0.0);
        assert!(d > 0.5 && d < 2.0, "unexpected output duration {d}");
    }

    #[test]
    fn auto_aligns_delayed_audio() {
        if !tools_available() {
            return;
        }
        let Some(fixture) = make_offset_fixture(0.3) else {
            return;
        };
        let before = av_sync_report(&fixture).unwrap();
        let out = av_sync_auto(&fixture, &EngineLimits::default()).unwrap();
        let after = av_sync_report(&out).unwrap();
        let (Some(v), Some(a)) = (after.video_start_ms, after.audio_start_ms) else {
            return;
        };
        let delta = (a - v).abs();
        assert!(
            delta <= 100,
            "auto sync left {delta}ms residual (before: {:?})",
            before
                .audio_start_ms
                .zip(before.video_start_ms)
                .map(|(a, v)| a - v)
        );
    }

    #[test]
    fn fix_rejects_video_only_input() {
        if !tools_available() {
            return;
        }
        let Some(fixture) = make_video_only_fixture() else {
            return;
        };
        let params = SyncParams::default();
        assert!(matches!(
            av_sync_fix(&fixture, &params, &EngineLimits::default()),
            Err(VideoError::InvalidInput(_))
        ));
        assert!(matches!(
            av_sync_auto(&fixture, &EngineLimits::default()),
            Err(VideoError::InvalidInput(_))
        ));
    }

    #[test]
    fn report_and_fix_reject_garbage() {
        if !tools_available() {
            return;
        }
        let garbage = b"this is definitely not a media file".to_vec();
        assert!(matches!(
            av_sync_report(&garbage),
            Err(VideoError::InvalidInput(_))
        ));
        assert!(matches!(
            av_sync_fix(&garbage, &SyncParams::default(), &EngineLimits::default()),
            Err(VideoError::InvalidInput(_))
        ));
    }
}
