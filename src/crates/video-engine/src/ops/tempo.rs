//! Tempo and time-domain video ops: speed change, reverse, loop,
//! freeze-frame.
//!
//! Speed changes re-encode (`setpts` on video, chained `atempo` on
//! audio); reverse requires a full decode/re-encode with the `reverse`
//! / `areverse` filters; loop prefers `-stream_loop` + `-c copy` with
//! a concat-demuxer fallback; freeze-frame is a single-pass
//! `trim` + `tpad` (clone last frame) video filter paired with an
//! `atrim` audio filter, re-encoded to H.264/AAC.
//!
//! The output container is derived from the probed `format_name` via
//! [`ext_for_format`], falling back to `mp4` when the container cannot
//! be named (mp4 is the safest muxer target for H.264/AAC output).

use std::path::Path;

use crate::crates::core::engine::EngineLimits;
use crate::crates::video_engine::error::{VideoError, VideoResult};
use crate::crates::video_engine::model::VideoInfo;
use crate::crates::video_engine::ops::probe::probe;
use crate::crates::video_engine::ops::{
    COMMAND_TIMEOUT, FFMPEG, PROBE_TIMEOUT, TempDir, check_input, check_limits, ext_for_format,
    find_tool, h264_aac_args, read_output, run_tool, tool_failure, write_input,
};

/// Upper bound for [`set_speed`]'s factor. Matches the task contract;
/// larger factors are rejected rather than chained into dozens of
/// `atempo` instances.
const MAX_SPEED_FACTOR: f64 = 100.0;

/// `atempo`'s per-instance accepted range (ffmpeg ≥ 5.x). Wider
/// factors are split across chained filters.
const ATEMPO_MIN: f64 = 0.5;
const ATEMPO_MAX: f64 = 100.0;

/// Probe + limit-check `input` and resolve the ffmpeg binary plus the
/// output extension (`ext_for_format`, falling back to `mp4`).
fn prepare(
    input: &[u8],
    limits: &EngineLimits,
) -> VideoResult<(VideoInfo, std::path::PathBuf, String)> {
    let len = check_input(input)?;
    let info = probe(input)?;
    check_limits(&info, len, limits, false)?;
    let program = find_tool(FFMPEG)?;
    let ext = info
        .format_name
        .as_deref()
        .and_then(ext_for_format)
        .unwrap_or("mp4")
        .to_string();
    Ok((info, program, ext))
}

/// Whether the probed media carries a stream of `codec_type`.
fn has_stream(info: &VideoInfo, codec_type: &str) -> bool {
    info.streams.iter().any(|s| s.codec_type == codec_type)
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

/// Build the `atempo` filter chain for `factor`. Each instance stays
/// inside ffmpeg's `0.5..=100` range; out-of-range totals are split
/// into multiple filters (e.g. `0.25` → `atempo=0.5,atempo=0.5`).
fn atempo_chain(factor: f64) -> Vec<String> {
    let mut filters = Vec::new();
    let mut f = factor;
    while f > ATEMPO_MAX {
        filters.push(format!("atempo={ATEMPO_MAX}"));
        f /= ATEMPO_MAX;
    }
    while f < ATEMPO_MIN {
        filters.push(format!("atempo={ATEMPO_MIN}"));
        f /= ATEMPO_MIN;
    }
    filters.push(format!("atempo={f:.6}"));
    filters
}

/// Change playback speed by `factor` (`2.0` = 2× fast, `0.5` = half
/// speed). Audio tempo follows via `atempo` (chained for factors
/// outside 0.5–100). Re-encodes.
///
/// `factor` must be finite, positive, and ≤ [`MAX_SPEED_FACTOR`];
/// anything else is [`VideoError::InvalidInput`]. Video timing is
/// rescaled with `setpts=PTS/{factor}`; audio with an `atempo` chain
/// only when the input actually has an audio stream.
pub(crate) fn set_speed(input: &[u8], factor: f64, limits: &EngineLimits) -> VideoResult<Vec<u8>> {
    if !factor.is_finite() || factor <= 0.0 || factor > MAX_SPEED_FACTOR {
        return Err(VideoError::InvalidInput(format!(
            "set_speed factor must be finite, > 0 and <= {MAX_SPEED_FACTOR}, got {factor}"
        )));
    }
    let (info, program, ext) = prepare(input, limits)?;
    let has_video = has_stream(&info, "video");
    let has_audio = has_stream(&info, "audio");
    if !has_video && !has_audio {
        return Err(VideoError::InvalidInput(
            "set_speed requires an input with a video or audio stream".to_string(),
        ));
    }

    let dir = TempDir::new("set-speed")?;
    let input_path = write_input(&dir, "input.bin", input)?;
    let out_path = dir.file(&format!("out.{ext}"));

    let mut args = vec![
        "-y".to_string(),
        "-v".to_string(),
        "error".to_string(),
        "-i".to_string(),
        input_path.to_string_lossy().into_owned(),
    ];
    if has_video {
        // `factor` Display never uses scientific notation, so this is
        // always a plain decimal ffmpeg can parse.
        args.extend(["-vf".to_string(), format!("setpts=PTS/{factor}")]);
    }
    if has_audio {
        args.extend(["-af".to_string(), atempo_chain(factor).join(",")]);
    }
    args.extend(h264_aac_args()?);
    args.push(out_path.to_string_lossy().into_owned());

    run_ffmpeg(&program, &args)?;
    read_output(&out_path)
}

/// Reverse the clip (video reversed, audio reversed too — full
/// decode/re-encode; bounded by `limits.max_duration_secs`).
pub(crate) fn reverse(input: &[u8], limits: &EngineLimits) -> VideoResult<Vec<u8>> {
    let (info, program, ext) = prepare(input, limits)?;
    let has_video = has_stream(&info, "video");
    let has_audio = has_stream(&info, "audio");
    if !has_video && !has_audio {
        return Err(VideoError::InvalidInput(
            "reverse requires an input with a video or audio stream".to_string(),
        ));
    }
    if has_video && !filter_available("reverse") {
        return Err(VideoError::ExternalTool {
            tool: FFMPEG.to_string(),
            detail: "the reverse video filter is not present in this ffmpeg build".to_string(),
        });
    }
    if has_audio && !filter_available("areverse") {
        return Err(VideoError::ExternalTool {
            tool: FFMPEG.to_string(),
            detail: "the areverse audio filter is not present in this ffmpeg build".to_string(),
        });
    }

    let dir = TempDir::new("reverse")?;
    let input_path = write_input(&dir, "input.bin", input)?;
    let out_path = dir.file(&format!("out.{ext}"));

    let mut args = vec![
        "-y".to_string(),
        "-v".to_string(),
        "error".to_string(),
        "-i".to_string(),
        input_path.to_string_lossy().into_owned(),
    ];
    if has_video {
        args.extend(["-vf".to_string(), "reverse".to_string()]);
    }
    if has_audio {
        args.extend(["-af".to_string(), "areverse".to_string()]);
    }
    args.extend(h264_aac_args()?);
    args.push(out_path.to_string_lossy().into_owned());

    run_ffmpeg(&program, &args)?;
    read_output(&out_path)
}

/// Loop `input` `count` additional times (`count = 1` → plays twice,
/// i.e. `count + 1` total plays). `count` must be ≥ 1.
///
/// Primary path: `-stream_loop {count}` on the input with `-c copy`.
/// Verified empirically on ffmpeg 7.1: `-stream_loop N` yields N+1
/// plays of the input, so `count` extra plays need `N = count`.
///
/// Fallback: the concat demuxer listing the same file `count + 1`
/// times with `-c copy` (mirroring `concat.rs`). If stream copy fails
/// on either path the error is surfaced honestly — looping never
/// silently re-encodes.
///
/// The estimated output duration (`input × (count + 1)`) is checked
/// against `limits.max_duration_secs` when both are known.
pub(crate) fn loop_clip(input: &[u8], count: u32, limits: &EngineLimits) -> VideoResult<Vec<u8>> {
    if count < 1 {
        return Err(VideoError::InvalidInput(format!(
            "loop_clip count must be >= 1 (count = 1 plays the clip twice), got {count}"
        )));
    }
    let (info, program, ext) = prepare(input, limits)?;

    let total = count as u64 + 1;
    if let (Some(max), Some(duration)) = (limits.max_duration_secs, info.duration_secs) {
        let estimated = duration * total as f64;
        if estimated > max as f64 {
            return Err(VideoError::LimitExceeded(format!(
                "looping {duration:.3}s input {total}× yields ~{estimated:.1}s, \
                 exceeding max_duration_secs {max}"
            )));
        }
    }

    let dir = TempDir::new("loop")?;
    let input_path = write_input(&dir, "loop.bin", input)?;
    let out_path = dir.file(&format!("out.{ext}"));

    // Primary: -stream_loop count → count+1 total plays, stream copy.
    let mut args = vec![
        "-y".to_string(),
        "-v".to_string(),
        "error".to_string(),
        "-stream_loop".to_string(),
        count.to_string(),
        "-i".to_string(),
        input_path.to_string_lossy().into_owned(),
        "-c".to_string(),
        "copy".to_string(),
    ];
    args.push(out_path.to_string_lossy().into_owned());

    match run_ffmpeg(&program, &args).and_then(|()| read_output(&out_path)) {
        Ok(bytes) => return Ok(bytes),
        Err(primary_err) => {
            // Fallback: concat demuxer with count+1 identical entries.
            // Same-file repeats share codec parameters, so `-c copy`
            // is safe wherever the container allows it.
            let mut list = String::new();
            for _ in 0..total {
                // Temp paths are generated (pid/counter/nanos) and never
                // contain quotes, so single-quoting is safe.
                list.push_str(&format!("file '{}'\n", input_path.display()));
            }
            let list_path = dir.file("list.txt");
            std::fs::write(&list_path, list)?;
            let concat_out = dir.file(&format!("concat.{ext}"));
            let mut cargs = vec![
                "-y".to_string(),
                "-v".to_string(),
                "error".to_string(),
                "-f".to_string(),
                "concat".to_string(),
                "-safe".to_string(),
                "0".to_string(),
                "-i".to_string(),
                list_path.to_string_lossy().into_owned(),
                "-c".to_string(),
                "copy".to_string(),
            ];
            cargs.push(concat_out.to_string_lossy().into_owned());
            match run_ffmpeg(&program, &cargs).and_then(|()| read_output(&concat_out)) {
                Ok(bytes) => Ok(bytes),
                Err(fallback_err) => {
                    // Surface the concat attempt's error — it is the
                    // most recent and usually the most informative.
                    let _ = primary_err;
                    Err(fallback_err)
                }
            }
        }
    }
}

/// Freeze the frame at `at_secs` for `duration_secs`: the video shows
/// `[0, at)` normally, then holds the last frame before `at` for
/// `duration_secs`; the audio simply continues through
/// `[0, at + duration_secs)` (audio is never frozen).
///
/// Implementation: a single ffmpeg pass with
/// `-vf "trim=duration={at},setpts=PTS-STARTPTS,tpad=stop_mode=clone:stop_duration={dur}"`
/// and `-af "atrim=duration={at+dur},asetpts=PTS-STARTPTS"`, re-encoded
/// to H.264/AAC. `tpad` clones the last decoded frame (the frame at
/// `at`, quantized to the frame grid — best-effort, documented).
/// When `at_secs` exceeds the probed duration it is clamped to the
/// duration so the frozen frame is the last real frame. `at_secs = 0`
/// freezes the *first* frame via `select=eq(n,0)`.
pub(crate) fn freeze_frame(
    input: &[u8],
    at_secs: f64,
    duration_secs: f64,
    limits: &EngineLimits,
) -> VideoResult<Vec<u8>> {
    if !at_secs.is_finite() || at_secs < 0.0 {
        return Err(VideoError::InvalidInput(format!(
            "freeze_frame at_secs must be a finite non-negative number, got {at_secs}"
        )));
    }
    if !duration_secs.is_finite() || duration_secs <= 0.0 {
        return Err(VideoError::InvalidInput(format!(
            "freeze_frame duration_secs must be a finite positive number, got {duration_secs}"
        )));
    }
    let (info, program, ext) = prepare(input, limits)?;
    if !has_stream(&info, "video") {
        return Err(VideoError::InvalidInput(
            "freeze_frame requires an input with a video stream".to_string(),
        ));
    }
    let has_audio = has_stream(&info, "audio");

    // Clamp `at` to the media duration: a freeze point past the end
    // holds the last real frame rather than erroring.
    let at = match info.duration_secs {
        Some(d) if at_secs > d => d,
        _ => at_secs,
    };
    let total = at + duration_secs;

    let dir = TempDir::new("freeze")?;
    let input_path = write_input(&dir, "input.bin", input)?;
    let out_path = dir.file(&format!("out.{ext}"));

    let video_filter = if at > 0.0 {
        format!(
            "trim=duration={at:.6},setpts=PTS-STARTPTS,\
             tpad=stop_mode=clone:stop_duration={duration_secs:.6}"
        )
    } else {
        // No preceding video: keep only the first frame, then clone
        // it for the freeze duration.
        format!(
            "select='eq(n,0)',setpts=PTS-STARTPTS,\
             tpad=stop_mode=clone:stop_duration={duration_secs:.6}"
        )
    };

    let mut args = vec![
        "-y".to_string(),
        "-v".to_string(),
        "error".to_string(),
        "-i".to_string(),
        input_path.to_string_lossy().into_owned(),
        "-vf".to_string(),
        video_filter,
    ];
    if has_audio {
        args.extend([
            "-af".to_string(),
            format!("atrim=duration={total:.6},asetpts=PTS-STARTPTS"),
        ]);
    }
    args.extend(h264_aac_args()?);
    args.push(out_path.to_string_lossy().into_owned());

    run_ffmpeg(&program, &args)?;
    read_output(&out_path)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::crates::video_engine::ops::{FFPROBE, find_tool, h264_encoder};
    use std::fs;

    fn tools_available() -> bool {
        find_tool(FFMPEG).is_ok() && find_tool(FFPROBE).is_ok()
    }

    /// 1s 320x240@30fps testsrc + 440Hz sine, encoded with whatever
    /// H.264 encoder this ffmpeg build provides. `None` when the
    /// fixture cannot be produced (missing encoder/tool).
    fn make_av_fixture() -> Option<Vec<u8>> {
        let program = find_tool(FFMPEG).ok()?;
        let enc = h264_encoder()?;
        let dir = TempDir::new("tempo-test-av").ok()?;
        let out = dir.file("clip.mp4");
        let args = vec![
            "-y".to_string(),
            "-v".to_string(),
            "error".to_string(),
            "-f".to_string(),
            "lavfi".to_string(),
            "-i".to_string(),
            "testsrc=duration=1:size=320x240:rate=30".to_string(),
            "-f".to_string(),
            "lavfi".to_string(),
            "-i".to_string(),
            "sine=frequency=440:duration=1".to_string(),
            "-c:v".to_string(),
            enc.to_string(),
            "-pix_fmt".to_string(),
            "yuv420p".to_string(),
            "-c:a".to_string(),
            "aac".to_string(),
            "-shortest".to_string(),
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

    fn assert_duration_near(actual: f64, expected: f64, what: &str) {
        assert!(
            (actual - expected).abs() <= 0.2,
            "{what}: expected ~{expected}s, got {actual}s"
        );
    }

    #[test]
    fn set_speed_factor_two_halves_duration() {
        if !tools_available() {
            return;
        }
        let Some(fix) = make_av_fixture() else {
            return;
        };
        let out = set_speed(&fix, 2.0, &EngineLimits::default()).unwrap();
        let dur = duration_of(&out).expect("probe sped output");
        assert_duration_near(dur, 0.5, "set_speed(2.0)");
    }

    #[test]
    fn set_speed_factor_half_doubles_duration() {
        if !tools_available() {
            return;
        }
        let Some(fix) = make_av_fixture() else {
            return;
        };
        let out = set_speed(&fix, 0.5, &EngineLimits::default()).unwrap();
        let dur = duration_of(&out).expect("probe slowed output");
        assert_duration_near(dur, 2.0, "set_speed(0.5)");
    }

    #[test]
    fn set_speed_rejects_invalid_factors() {
        if !tools_available() {
            return;
        }
        let Some(fix) = make_av_fixture() else {
            return;
        };
        for bad in [0.0, -1.0, f64::NAN, f64::INFINITY, 100.5] {
            assert!(
                matches!(
                    set_speed(&fix, bad, &EngineLimits::default()),
                    Err(VideoError::InvalidInput(_))
                ),
                "factor {bad} should be rejected"
            );
        }
    }

    #[test]
    fn reverse_preserves_duration() {
        if !tools_available() {
            return;
        }
        let Some(fix) = make_av_fixture() else {
            return;
        };
        let out = reverse(&fix, &EngineLimits::default()).unwrap();
        let dur = duration_of(&out).expect("probe reversed output");
        assert_duration_near(dur, 1.0, "reverse");
    }

    #[test]
    fn loop_count_one_plays_twice() {
        if !tools_available() {
            return;
        }
        let Some(fix) = make_av_fixture() else {
            return;
        };
        let out = loop_clip(&fix, 1, &EngineLimits::default()).unwrap();
        let dur = duration_of(&out).expect("probe looped output");
        assert_duration_near(dur, 2.0, "loop_clip(count=1)");
        assert!(
            matches!(
                loop_clip(&fix, 0, &EngineLimits::default()),
                Err(VideoError::InvalidInput(_))
            ),
            "count=0 should be rejected"
        );
    }

    #[test]
    fn freeze_frame_midpoint_keeps_duration() {
        if !tools_available() {
            return;
        }
        let Some(fix) = make_av_fixture() else {
            return;
        };
        let out = freeze_frame(&fix, 0.5, 0.5, &EngineLimits::default()).unwrap();
        let dur = duration_of(&out).expect("probe frozen output");
        assert_duration_near(dur, 1.0, "freeze_frame(0.5, 0.5)");
    }
}
