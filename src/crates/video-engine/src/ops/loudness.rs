//! Audio loudness measurement and normalization (EBU R128 family).
//!
//! Measurement prefers the `ebur128=peak=true` filter and parses its
//! end-of-stream `Summary:` block on stderr (integrated loudness in
//! LUFS, loudness range in LU, true peak in dBTP). When this ffmpeg
//! build lacks `ebur128`, `loudnorm=print_format=json` is used
//! instead and its trailing JSON object (`input_i`/`input_tp`/
//! `input_lra`) is parsed.
//!
//! `channel_lra` stays `None`: the ebur128 summary reports a single
//! program loudness range, not per-channel values.
//!
//! Normalization uses a single-pass `loudnorm` (dynamic mode — no
//! measured-values two-pass). When `loudnorm` is absent it degrades
//! to a static `volume` gain computed from a loudness measurement.
//! The output container family is preserved via `ext_for_format`;
//! video streams are stream-copied while audio is re-encoded to a
//! codec the target container accepts.
//!
//! `audio_stats` is best-effort: it merges the `Overall` section of
//! `astats=measure_perchannel=none` (RMS/peak level dB, DC offset,
//! flat factor, noise floor, ...) with `volumedetect`'s
//! `mean_volume`/`max_volume`. Filters that are missing or fail are
//! skipped; the returned map may be empty on exotic builds.

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};
use std::sync::OnceLock;

use serde_json::Value;

use crate::crates::core::engine::EngineLimits;
use crate::crates::video_engine::error::{VideoError, VideoResult};
use crate::crates::video_engine::model::{LoudnessReport, VideoInfo};
use crate::crates::video_engine::ops::probe::probe;
use crate::crates::video_engine::ops::{
    COMMAND_TIMEOUT, FFMPEG, ProcOutput, PROBE_TIMEOUT, TempDir, check_input, check_limits,
    encoder_available, ext_for_format, find_tool, read_output, run_tool, tool_failure, write_input,
};

/// Default normalization target: EBU R128 broadcast loudness.
const DEFAULT_TARGET_LUFS: f64 = -23.0;
/// Fixed true-peak ceiling and loudness-range target for loudnorm.
const TARGET_TP_DBTP: f64 = -1.5;
const TARGET_LRA_LU: f64 = 11.0;

/// Whether this ffmpeg build lists the named *filter* (as opposed to
/// an encoder). The `ffmpeg -filters` list is fetched once and cached.
pub(crate) fn filter_available(name: &str) -> bool {
    static FILTERS: OnceLock<Vec<String>> = OnceLock::new();
    let list = FILTERS.get_or_init(|| {
        let Ok(program) = find_tool(FFMPEG) else {
            return Vec::new();
        };
        let args = vec!["-hide_banner".to_string(), "-filters".to_string()];
        match run_tool(FFMPEG, &program, &args, PROBE_TIMEOUT) {
            Ok(output) if output.status.success() => {
                // Lines look like: ` .S. ebur128  A->N  EBU R128 scanner.`
                String::from_utf8_lossy(&output.stdout)
                    .lines()
                    .filter_map(|line| line.split_whitespace().nth(1))
                    .map(str::to_string)
                    .collect()
            }
            _ => Vec::new(),
        }
    });
    list.iter().any(|f| f == name)
}

/// Run `ffmpeg -nostats -i <input> -af <af> -f null -` and return the
/// captured output. Callers inspect `status`/`stderr` themselves.
fn run_audio_filter(program: &Path, input_path: &Path, af: &str) -> VideoResult<ProcOutput> {
    let args = vec![
        "-nostats".to_string(),
        "-i".to_string(),
        input_path.to_string_lossy().into_owned(),
        "-af".to_string(),
        af.to_string(),
        "-f".to_string(),
        "null".to_string(),
        "-".to_string(),
    ];
    run_tool(FFMPEG, program, &args, COMMAND_TIMEOUT)
}

/// Strip ffmpeg's `[Parsed_filter_0 @ 0x...] ` log prefix from a
/// stderr line, returning the payload. Lines without the prefix are
/// returned unchanged (ebur128's indented summary block has none).
fn strip_log_prefix(line: &str) -> &str {
    if let Some(rest) = line.strip_prefix('[') {
        if let Some(end) = rest.find("] ") {
            return &rest[end + 2..];
        }
    }
    line
}

/// Extract the first parseable float after `label`'s colon on a line
/// of the form `Label: <float> <unit>`. Returns `None` when the label
/// is absent or the value is not numeric (e.g. `N/A`).
fn labeled_float(line: &str, label: &str) -> Option<f64> {
    let rest = line.trim().strip_prefix(label)?.trim_start();
    let rest = rest.strip_prefix(':')?.trim_start();
    rest.split_whitespace().next()?.parse::<f64>().ok()
}

/// Parse the `Summary:` block printed by `ebur128=peak=true` on
/// stderr. Tolerant string matching (no regex): values are read only
/// after a `Summary:` line, inside the `Integrated loudness`,
/// `Loudness range`, and `True peak` sub-sections. Per-frame `I:` /
/// `LRA:` / `TPK:` progress lines are ignored because they precede the
/// summary. Missing values stay `None`.
fn parse_ebur128_summary(stderr: &str) -> LoudnessReport {
    #[derive(PartialEq)]
    enum Section {
        None,
        Integrated,
        LoudnessRange,
        TruePeak,
    }

    let mut report = LoudnessReport {
        integrated_lufs: None,
        loudness_range_lu: None,
        true_peak_dbtp: None,
        channel_lra: None,
    };
    let mut section = Section::None;
    let mut in_summary = false;

    for raw in stderr.lines() {
        let line = strip_log_prefix(raw.trim());
        if line.contains("Summary:") {
            in_summary = true;
            continue;
        }
        if !in_summary {
            continue;
        }
        if line.starts_with("Integrated loudness") {
            section = Section::Integrated;
            continue;
        }
        if line.starts_with("Loudness range") {
            section = Section::LoudnessRange;
            continue;
        }
        if line.starts_with("True peak") {
            section = Section::TruePeak;
            continue;
        }
        let value = match section {
            Section::Integrated => labeled_float(line, "I"),
            Section::LoudnessRange => labeled_float(line, "LRA"),
            Section::TruePeak => labeled_float(line, "Peak"),
            Section::None => None,
        };
        if let Some(v) = value {
            match section {
                Section::Integrated if report.integrated_lufs.is_none() => {
                    report.integrated_lufs = Some(v)
                }
                Section::LoudnessRange if report.loudness_range_lu.is_none() => {
                    report.loudness_range_lu = Some(v)
                }
                Section::TruePeak if report.true_peak_dbtp.is_none() => {
                    report.true_peak_dbtp = Some(v)
                }
                _ => {}
            }
        }
    }
    report
}

/// Parse the trailing JSON object printed by
/// `loudnorm=print_format=json` on stderr. The object is flat; we try
/// each `{` position and keep the first that deserializes and carries
/// `input_i`, so surrounding log noise cannot break parsing.
fn parse_loudnorm_json(stderr: &str) -> Option<LoudnessReport> {
    for (idx, _) in stderr.match_indices('{') {
        let tail = &stderr[idx..];
        let mut stream = serde_json::Deserializer::from_str(tail).into_iter::<Value>();
        let Some(Ok(json)) = stream.next() else {
            continue;
        };
        let get = |key: &str| {
            json.get(key)
                .and_then(Value::as_str)
                .and_then(|s| s.parse().ok())
        };
        if json.get("input_i").is_some() {
            return Some(LoudnessReport {
                integrated_lufs: get("input_i"),
                loudness_range_lu: get("input_lra"),
                true_peak_dbtp: get("input_tp"),
                channel_lra: None,
            });
        }
    }
    None
}

/// astats `Overall`-section labels mapped to stable snake_case keys.
const ASTATS_KEYS: &[(&str, &str)] = &[
    ("DC offset", "dc_offset"),
    ("Min level", "min_level"),
    ("Max level", "max_level"),
    ("Peak level dB", "peak_db"),
    ("RMS level dB", "rms_db"),
    ("RMS peak dB", "rms_peak_db"),
    ("RMS trough dB", "rms_trough_db"),
    ("Crest factor", "crest_factor"),
    ("Flat factor", "flat_factor"),
    ("Noise floor dB", "noise_floor_db"),
    ("Dynamic range", "dynamic_range"),
    ("Entropy", "entropy"),
];

/// Parse the `Overall` section of `astats` stderr output into `map`.
/// Only lines after an `Overall` header are considered; per-channel
/// sections (when enabled) are ignored.
fn parse_astats_overall(stderr: &str, map: &mut BTreeMap<String, f64>) {
    let mut in_overall = false;
    for raw in stderr.lines() {
        let line = strip_log_prefix(raw.trim());
        if line == "Overall" {
            in_overall = true;
            continue;
        }
        if line.starts_with("Channel:") {
            in_overall = false;
            continue;
        }
        if !in_overall {
            continue;
        }
        for (label, key) in ASTATS_KEYS {
            if let Some(v) = labeled_float(line, label) {
                map.insert((*key).to_string(), v);
                break;
            }
        }
    }
}

/// Parse `volumedetect` stderr (`mean_volume: -41.1 dB`,
/// `max_volume: -38.1 dB`) into `map`.
fn parse_volumedetect(stderr: &str, map: &mut BTreeMap<String, f64>) {
    for raw in stderr.lines() {
        let line = strip_log_prefix(raw.trim());
        for tag in ["mean_volume", "max_volume"] {
            if let Some(v) = labeled_float(line, tag) {
                map.insert(tag.to_string(), v);
            }
        }
    }
}

/// Shared preamble for the loudness ops: validate input, resolve
/// ffmpeg, probe, enforce limits, and require an audio stream.
/// Returns `(program, VideoInfo)` — the caller owns tempdir setup.
fn prepare_audio_input(
    input: &[u8],
    limits: &EngineLimits,
) -> VideoResult<(PathBuf, VideoInfo)> {
    let input_len = check_input(input)?;
    let program = find_tool(FFMPEG)?;
    let info = probe(input)?;
    check_limits(&info, input_len, limits, false)?;
    if !info.has_audio {
        return Err(VideoError::InvalidInput(
            "input has no audio stream".to_string(),
        ));
    }
    Ok((program, info))
}

/// Audio codec arguments for the normalized output container.
/// `has_video` selects between stream-copying video and dropping it.
/// Encoders that may be absent (`libmp3lame`, `libvorbis`) are checked
/// up front so minimal ffmpeg builds fail honestly.
fn output_codec_args(ext: &str, has_video: bool) -> VideoResult<Vec<String>> {
    let audio_encoder = match ext {
        "wav" => "pcm_s16le",
        "mp3" => {
            if !encoder_available("libmp3lame") {
                return Err(VideoError::ExternalTool {
                    tool: FFMPEG.to_string(),
                    detail: "mp3 output requires the libmp3lame encoder, \
                             which this ffmpeg build does not provide"
                        .to_string(),
                });
            }
            "libmp3lame"
        }
        "ogg" | "webm" => {
            if !encoder_available("libvorbis") {
                return Err(VideoError::ExternalTool {
                    tool: FFMPEG.to_string(),
                    detail: format!(
                        "{ext} output requires the libvorbis encoder, \
                         which this ffmpeg build does not provide"
                    ),
                });
            }
            "libvorbis"
        }
        // mp4/mkv/avi/ts/mpg/wmv/flv and friends all accept AAC.
        _ => "aac",
    };
    let mut args = Vec::with_capacity(6);
    if has_video {
        args.extend(["-c:v".to_string(), "copy".to_string()]);
    } else {
        args.push("-vn".to_string());
    }
    args.extend(["-c:a".to_string(), audio_encoder.to_string()]);
    Ok(args)
}

/// Measure integrated loudness, loudness range, and true peak via
/// `ebur128=peak=true` (or `loudnorm print_format=json` fallback).
/// `None` fields indicate values the filter did not report.
pub(crate) fn measure_loudness(
    input: &[u8],
    limits: &EngineLimits,
) -> VideoResult<LoudnessReport> {
    let (program, _info) = prepare_audio_input(input, limits)?;
    let dir = TempDir::new("loudness")?;
    let input_path = write_input(&dir, "input.bin", input)?;

    // Preferred path: ebur128 summary block.
    if filter_available("ebur128") {
        let output = run_audio_filter(&program, &input_path, "ebur128=peak=true")?;
        if !output.status.success() {
            return Err(tool_failure(FFMPEG, &output));
        }
        let stderr = String::from_utf8_lossy(&output.stderr);
        let report = parse_ebur128_summary(&stderr);
        // A summary with at least one value is a success; an empty
        // parse falls through to the loudnorm fallback below.
        if report.integrated_lufs.is_some()
            || report.loudness_range_lu.is_some()
            || report.true_peak_dbtp.is_some()
        {
            return Ok(report);
        }
    }

    // Fallback: loudnorm's print_format=json measurement pass.
    if filter_available("loudnorm") {
        let af = format!(
            "loudnorm=I={DEFAULT_TARGET_LUFS}:TP={TARGET_TP_DBTP}:LRA={TARGET_LRA_LU}:print_format=json"
        );
        let output = run_audio_filter(&program, &input_path, &af)?;
        if !output.status.success() {
            return Err(tool_failure(FFMPEG, &output));
        }
        let stderr = String::from_utf8_lossy(&output.stderr);
        return parse_loudnorm_json(&stderr).ok_or_else(|| VideoError::ExternalTool {
            tool: FFMPEG.to_string(),
            detail: "loudnorm ran but produced no parseable JSON measurement".to_string(),
        });
    }

    Err(VideoError::ExternalTool {
        tool: FFMPEG.to_string(),
        detail: "neither the ebur128 nor the loudnorm filter is available \
                 in this ffmpeg build"
            .to_string(),
    })
}

/// Normalize loudness to `target_lufs` (default -23 LUFS) using
/// `loudnorm` in single-pass dynamic mode. Falls back to a static
/// `volume` gain derived from a loudness measurement when `loudnorm`
/// is unavailable. Video streams are copied; audio is re-encoded to
/// the target container's codec.
pub(crate) fn normalize_loudness(
    input: &[u8],
    target_lufs: Option<f64>,
    limits: &EngineLimits,
) -> VideoResult<Vec<u8>> {
    let (program, info) = prepare_audio_input(input, limits)?;
    let target = target_lufs.unwrap_or(DEFAULT_TARGET_LUFS);
    if !target.is_finite() {
        return Err(VideoError::InvalidInput(format!(
            "target_lufs must be finite, got {target}"
        )));
    }
    let ext = info
        .format_name
        .as_deref()
        .and_then(ext_for_format)
        .ok_or_else(|| {
            VideoError::InvalidInput(
                "cannot determine output container from probed format_name".to_string(),
            )
        })?;

    // Pick the filter chain: loudnorm dynamic normalization, or a
    // static volume gain when loudnorm is missing.
    let af = if filter_available("loudnorm") {
        format!("loudnorm=I={target}:TP={TARGET_TP_DBTP}:LRA={TARGET_LRA_LU}")
    } else if filter_available("volume") {
        let report = measure_loudness(input, limits)?;
        let integrated = report.integrated_lufs.ok_or_else(|| {
            VideoError::ExternalTool {
                tool: FFMPEG.to_string(),
                detail: "cannot normalize without loudnorm: loudness \
                         measurement reported no integrated loudness"
                    .to_string(),
            }
        })?;
        // Clamp the gain to a sane range; a wildly wrong measurement
        // should not produce a uselessly clipped or silent file.
        let gain_db = (target - integrated).clamp(-60.0, 60.0);
        format!("volume={gain_db:.2}dB")
    } else {
        return Err(VideoError::ExternalTool {
            tool: FFMPEG.to_string(),
            detail: "neither the loudnorm nor the volume filter is available \
                     in this ffmpeg build"
                .to_string(),
        });
    };

    let dir = TempDir::new("loudnorm")?;
    let input_path = write_input(&dir, "input.bin", input)?;
    let out_path = dir.file(&format!("out.{ext}"));

    let mut args = vec![
        "-y".to_string(),
        "-v".to_string(),
        "error".to_string(),
        "-i".to_string(),
        input_path.to_string_lossy().into_owned(),
        "-af".to_string(),
        af,
    ];
    args.extend(output_codec_args(ext, info.has_video)?);
    args.push(out_path.to_string_lossy().into_owned());

    let output = run_tool(FFMPEG, &program, &args, COMMAND_TIMEOUT)?;
    if !output.status.success() {
        return Err(tool_failure(FFMPEG, &output));
    }
    read_output(&out_path)
}

/// Aggregate audio statistics via `astats`/`volumedetect`: mean/max
/// volume (dB), RMS, DC offset, flat factor, peak level — as a flat
/// `BTreeMap` keyed by stable metric names. Best-effort: filters that
/// are missing or fail contribute nothing; an empty map is returned
/// rather than an error when nothing could be measured.
pub(crate) fn audio_stats(
    input: &[u8],
    limits: &EngineLimits,
) -> VideoResult<BTreeMap<String, f64>> {
    let (program, _info) = prepare_audio_input(input, limits)?;
    let dir = TempDir::new("audio-stats")?;
    let input_path = write_input(&dir, "input.bin", input)?;

    let mut map = BTreeMap::new();

    // astats: time-domain statistics, Overall section only.
    if filter_available("astats") {
        if let Ok(output) =
            run_audio_filter(&program, &input_path, "astats=measure_perchannel=none")
        {
            if output.status.success() {
                parse_astats_overall(&String::from_utf8_lossy(&output.stderr), &mut map);
            }
        }
    }

    // volumedetect: mean/max volume in dB.
    if filter_available("volumedetect") {
        if let Ok(output) = run_audio_filter(&program, &input_path, "volumedetect") {
            if output.status.success() {
                parse_volumedetect(&String::from_utf8_lossy(&output.stderr), &mut map);
            }
        }
    }

    Ok(map)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::crates::video_engine::ops::{FFPROBE, find_tool};
    use std::fs;

    fn tools_available() -> bool {
        find_tool(FFMPEG).is_ok() && find_tool(FFPROBE).is_ok()
    }

    /// A 2s 440 Hz sine at -20 dBFS in a WAV container.
    fn sine_wav() -> Option<Vec<u8>> {
        let program = find_tool(FFMPEG).ok()?;
        let dir = TempDir::new("loudness-test").ok()?;
        let out = dir.file("sine.wav");
        let args = vec![
            "-y".to_string(),
            "-v".to_string(),
            "error".to_string(),
            "-f".to_string(),
            "lavfi".to_string(),
            "-i".to_string(),
            "sine=frequency=440:duration=2:sample_rate=48000".to_string(),
            "-af".to_string(),
            "volume=-20dB".to_string(),
            out.to_string_lossy().into_owned(),
        ];
        let output = run_tool(FFMPEG, &program, &args, COMMAND_TIMEOUT).ok()?;
        if !output.status.success() {
            return None;
        }
        fs::read(out).ok()
    }

    /// A 1s testsrc MP4 with no audio stream.
    fn video_only_mp4() -> Option<Vec<u8>> {
        let program = find_tool(FFMPEG).ok()?;
        let enc = crate::crates::video_engine::ops::h264_encoder()?;
        let dir = TempDir::new("loudness-test").ok()?;
        let out = dir.file("video_only.mp4");
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
    fn parse_ebur128_summary_block() {
        let stderr = "\
[Parsed_ebur128_0 @ 0x7f15b80030c0] t: 1.999979   TARGET:-23 LUFS    M: -41.8 S:-120.7     I: -41.8 LUFS       LRA:   0.0 LU  FTPK: -38.1 dBFS  TPK: -38.1 dBFS
[Parsed_ebur128_0 @ 0x7f15b80030c0] Summary:

  Integrated loudness:
    I:         -41.8 LUFS
    Threshold: -51.8 LUFS

  Loudness range:
    LRA:         0.0 LU
    Threshold:   0.0 LUFS
    LRA low:     0.0 LUFS
    LRA high:    0.0 LUFS

  True peak:
    Peak:      -38.1 dBFS
[out#0/null @ 0x561aa39f18c0] video:0KiB audio:188KiB
";
        let report = parse_ebur128_summary(stderr);
        assert_eq!(report.integrated_lufs, Some(-41.8));
        assert_eq!(report.loudness_range_lu, Some(0.0));
        assert_eq!(report.true_peak_dbtp, Some(-38.1));
        assert_eq!(report.channel_lra, None);
    }

    #[test]
    fn parse_loudnorm_json_tail() {
        let stderr = "[Parsed_loudnorm_0 @ 0x1] noise\n{\n\t\"input_i\" : \"-27.61\",\n\t\"input_tp\" : \"-4.47\",\n\t\"input_lra\" : \"7.20\",\n\t\"input_thresh\" : \"-37.90\"\n}\n[out#0/null @ 0x2] done\n";
        let report = parse_loudnorm_json(stderr).expect("json parsed");
        assert_eq!(report.integrated_lufs, Some(-27.61));
        assert_eq!(report.true_peak_dbtp, Some(-4.47));
        assert_eq!(report.loudness_range_lu, Some(7.2));
        assert_eq!(report.channel_lra, None);
    }

    #[test]
    fn measure_loudness_sine_reports_values() {
        if !tools_available() {
            return;
        }
        let Some(wav) = sine_wav() else { return };
        let report = measure_loudness(&wav, &EngineLimits::default()).unwrap();
        let i = report.integrated_lufs.expect("integrated loudness");
        assert!((-60.0..0.0).contains(&i), "integrated {i} out of range");
        assert!(report.loudness_range_lu.is_some());
        assert!(report.true_peak_dbtp.is_some());
    }

    #[test]
    fn normalize_loudness_hits_target_and_keeps_duration() {
        if !tools_available() {
            return;
        }
        let Some(wav) = sine_wav() else { return };
        let out = normalize_loudness(&wav, Some(-23.0), &EngineLimits::default()).unwrap();
        assert!(!out.is_empty());
        // Output stays in the same container family (wav -> wav).
        let info = probe(&out).unwrap();
        let duration = info.duration_secs.expect("duration");
        assert!((1.5..=2.5).contains(&duration), "duration {duration}");
        // Re-measure: loudnorm should land near the -23 LUFS target.
        let report = measure_loudness(&out, &EngineLimits::default()).unwrap();
        let i = report.integrated_lufs.expect("integrated loudness");
        assert!((-30.0..=-16.0).contains(&i), "normalized loudness {i}");
    }

    #[test]
    fn audio_stats_returns_metrics() {
        if !tools_available() {
            return;
        }
        let Some(wav) = sine_wav() else { return };
        let stats = audio_stats(&wav, &EngineLimits::default()).unwrap();
        assert!(
            stats.contains_key("peak_db") || stats.contains_key("max_volume"),
            "expected peak_db or max_volume in {stats:?}"
        );
    }

    #[test]
    fn no_audio_input_is_invalid() {
        if !tools_available() {
            return;
        }
        let Some(mp4) = video_only_mp4() else { return };
        let limits = EngineLimits::default();
        assert!(matches!(
            measure_loudness(&mp4, &limits),
            Err(VideoError::InvalidInput(_))
        ));
        assert!(matches!(
            normalize_loudness(&mp4, None, &limits),
            Err(VideoError::InvalidInput(_))
        ));
        assert!(matches!(
            audio_stats(&mp4, &limits),
            Err(VideoError::InvalidInput(_))
        ));
    }
}
