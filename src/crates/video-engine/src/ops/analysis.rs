//! Media analysis: black/freeze/silence detection, scene changes,
//! and aggregate metrics — all via ffmpeg analysis filters writing
//! to `null`/metadata outputs that we parse.
//!
//! Detector invocations (each `-f null -`, logs parsed from stderr):
//!
//! - `blackdetect=d=0.1:pic_th=0.98` — lines like
//!   `[blackdetect @ ..] black_start:0 black_end:1 black_duration:1`.
//! - `freezedetect=n=0.003:d=2` — lines like
//!   `[freezedetect @ ..] lavfi.freezedetect.freeze_start: 0` /
//!   `lavfi.freezedetect.freeze_end: 3`. A freeze still active at EOF
//!   emits no `freeze_end`; it is closed at the probed duration.
//! - `silencedetect=n=-50dB:d=0.5` (audio only) — lines like
//!   `silence_start: 0` / `silence_end: 2 | silence_duration: 2`;
//!   same EOF-close rule.
//! - Scene changes: `select='gt(scene\,{thr})'` followed by
//!   `metadata=mode=print:file=<path>`. The `select` filter stamps
//!   `lavfi.scene_score` on kept frames; the metadata filter writes
//!   `frame:N pts:.. pts_time:T` + `lavfi.scene_score=S` records to
//!   the file. When the metadata file cannot be read, a
//!   `select`+`showinfo` pass over stderr `pts_time:` fields is the
//!   fallback (score reported as the threshold — a lower bound,
//!   since showinfo does not print the scene score).
//!
//! `analyze` runs at most three ffmpeg passes: one video pass chaining
//! `blackdetect,freezedetect` (both are `V->V` and log independently),
//! one `silencedetect` audio pass, and one scene pass. Loudness is
//! deliberately left `None` — `loudness.rs` owns EBU R128 measurement
//! and duplicating it here would fork the implementation.

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};

use crate::crates::core::engine::EngineLimits;
use crate::crates::video_engine::error::{VideoError, VideoResult};
use crate::crates::video_engine::model::{MediaAnalysis, VideoInfo};
use crate::crates::video_engine::ops::probe::probe;
use crate::crates::video_engine::ops::{
    COMMAND_TIMEOUT, FFMPEG, ProcOutput, TempDir, check_input, check_limits, find_tool,
    read_output, run_tool, tool_failure, write_input,
};

/// Default `scene` score threshold used by [`analyze`] (the public
/// [`scene_timestamps`] takes an explicit threshold).
const DEFAULT_SCENE_THRESHOLD: f64 = 0.4;

/// blackdetect parameters: 0.1s minimum black, 98% black picture ratio.
const BLACKDETECT_FILTER: &str = "blackdetect=d=0.1:pic_th=0.98";
/// freezedetect parameters: noise floor 0.003, 2s minimum freeze.
const FREEZEDETECT_FILTER: &str = "freezedetect=n=0.003:d=2";
/// silencedetect parameters: -50 dB noise tolerance, 0.5s minimum.
const SILENCEDETECT_FILTER: &str = "silencedetect=n=-50dB:d=0.5";

/// Input written to scratch plus its probe result — everything the
/// detectors need, prepared once.
struct Prepared {
    /// Scratch dir owning `input_path`; must outlive ffmpeg runs.
    _dir: TempDir,
    input_path: PathBuf,
    info: VideoInfo,
}

/// Validate `input`, probe it, enforce [`EngineLimits`], and write it
/// to a scratch file for ffmpeg. `check_pixels` guards against
/// decompression bombs since analysis decodes every frame.
fn prepare(input: &[u8], limits: &EngineLimits, tag: &str) -> VideoResult<Prepared> {
    let input_len = check_input(input)?;
    let info = probe(input)?;
    check_limits(&info, input_len, limits, true)?;
    let dir = TempDir::new(tag)?;
    let input_path = write_input(&dir, "input.bin", input)?;
    Ok(Prepared {
        _dir: dir,
        input_path,
        info,
    })
}

/// ffmpeg argument vector: global opts, `-i input`, caller-supplied
/// filter/output options, then the `null` muxer on stdout.
fn ffmpeg_args(input_path: &Path, tail: &[String]) -> Vec<String> {
    let mut args = vec![
        "-y".to_string(),
        "-hide_banner".to_string(),
        "-v".to_string(),
        "info".to_string(),
        "-i".to_string(),
        input_path.to_string_lossy().into_owned(),
    ];
    args.extend(tail.iter().cloned());
    args.extend(["-f".to_string(), "null".to_string(), "-".to_string()]);
    args
}

/// Run ffmpeg and require a zero exit; non-zero becomes
/// [`VideoError::ExternalTool`] via [`tool_failure`].
fn run_ffmpeg(program: &Path, args: &[String]) -> VideoResult<ProcOutput> {
    let output = run_tool(FFMPEG, program, args, COMMAND_TIMEOUT)?;
    if !output.status.success() {
        return Err(tool_failure(FFMPEG, &output));
    }
    Ok(output)
}

/// Parse a leading float from `s` (`"1.2"`, `"-0.5e2"`, ...), stopping
/// at the first non-numeric character.
fn leading_f64(s: &str) -> Option<f64> {
    let end = s
        .find(|c: char| !(c.is_ascii_digit() || matches!(c, '-' | '+' | '.' | 'e' | 'E')))
        .unwrap_or(s.len());
    if end == 0 {
        return None;
    }
    s[..end].parse().ok()
}

/// Find every `key: value` mark on one log line, in column order.
/// `key` matches as a suffix-tolerant substring so dotted prefixes like
/// `lavfi.freezedetect.freeze_start` hit; the key must be followed by
/// `:` (spaces allowed) or the occurrence is ignored.
fn marks(line: &str, keys: &[(&str, bool)]) -> Vec<(bool, f64)> {
    let mut hits = Vec::new();
    for &(key, is_start) in keys {
        let mut from = 0;
        while let Some(rel) = line[from..].find(key) {
            let abs = from + rel;
            let after = abs + key.len();
            let rest = line[after..].trim_start();
            if let Some(rest) = rest.strip_prefix(':') {
                if let Some(v) = leading_f64(rest.trim_start()) {
                    hits.push((abs, is_start, v));
                }
            }
            from = after;
        }
    }
    hits.sort_by_key(|h| h.0);
    hits.into_iter().map(|(_, s, v)| (s, v)).collect()
}

/// Pair `start_key`/`end_key` marks into `[start, end)` intervals.
/// Handles both layouts ffmpeg emits: both keys on one line
/// (`black_start:0 black_end:1`) and separate lines
/// (`freeze_start: 0` / `freeze_end: 3`). An interval still open at
/// EOF is closed at `duration` (the probed container duration); if the
/// duration is unknown the open tail is dropped rather than invented.
fn interval_segments(
    text: &str,
    start_key: &str,
    end_key: &str,
    duration: Option<f64>,
) -> Vec<(f64, f64)> {
    let keys = [(start_key, true), (end_key, false)];
    let mut segs = Vec::new();
    let mut open: Option<f64> = None;
    for line in text.lines() {
        for (is_start, val) in marks(line, &keys) {
            if is_start {
                // A new start supersedes an unclosed one — close it at
                // this timestamp so overlapping detector glitches
                // cannot swallow a segment.
                if let Some(s) = open.take() {
                    if val > s {
                        segs.push((s, val));
                    }
                }
                open = Some(val);
            } else if let Some(s) = open.take() {
                if val > s {
                    segs.push((s, val));
                }
            }
            // An end mark with nothing open is ignored.
        }
    }
    if let (Some(s), Some(d)) = (open, duration) {
        if d > s {
            segs.push((s, d));
        }
    }
    segs.sort_by(|a, b| a.0.partial_cmp(&b.0).unwrap_or(std::cmp::Ordering::Equal));
    segs
}

/// `blackdetect` pass → black intervals.
fn black_run(
    program: &Path,
    input_path: &Path,
    duration: Option<f64>,
) -> VideoResult<Vec<(f64, f64)>> {
    let args = ffmpeg_args(
        input_path,
        &[
            "-an".to_string(),
            "-vf".to_string(),
            BLACKDETECT_FILTER.to_string(),
        ],
    );
    let out = run_ffmpeg(program, &args)?;
    Ok(interval_segments(
        &String::from_utf8_lossy(&out.stderr),
        "black_start",
        "black_end",
        duration,
    ))
}

/// `freezedetect` pass → freeze intervals.
fn freeze_run(
    program: &Path,
    input_path: &Path,
    duration: Option<f64>,
) -> VideoResult<Vec<(f64, f64)>> {
    let args = ffmpeg_args(
        input_path,
        &[
            "-an".to_string(),
            "-vf".to_string(),
            FREEZEDETECT_FILTER.to_string(),
        ],
    );
    let out = run_ffmpeg(program, &args)?;
    Ok(interval_segments(
        &String::from_utf8_lossy(&out.stderr),
        "freeze_start",
        "freeze_end",
        duration,
    ))
}

/// `silencedetect` pass → silence intervals. Caller must have verified
/// an audio stream exists.
fn silence_run(
    program: &Path,
    input_path: &Path,
    duration: Option<f64>,
) -> VideoResult<Vec<(f64, f64)>> {
    let args = ffmpeg_args(
        input_path,
        &[
            "-vn".to_string(),
            "-af".to_string(),
            SILENCEDETECT_FILTER.to_string(),
        ],
    );
    let out = run_ffmpeg(program, &args)?;
    Ok(interval_segments(
        &String::from_utf8_lossy(&out.stderr),
        "silence_start",
        "silence_end",
        duration,
    ))
}

/// Parse `metadata=mode=print` output: `frame:N pts:.. pts_time:T`
/// headers followed by `lavfi.scene_score=S` lines.
fn parse_scene_meta(text: &str) -> Vec<(f64, f64)> {
    let mut scenes = Vec::new();
    let mut pts_time: Option<f64> = None;
    for line in text.lines() {
        let line = line.trim();
        if line.starts_with("frame:") {
            pts_time = line
                .split_whitespace()
                .find_map(|tok| tok.strip_prefix("pts_time:"))
                .and_then(|v| v.parse::<f64>().ok());
        } else if let Some(v) = line.strip_prefix("lavfi.scene_score=") {
            if let (Some(t), Ok(s)) = (pts_time, v.trim().parse::<f64>()) {
                scenes.push((t, s));
            }
        }
    }
    scenes
}

/// Fallback scene pass: `select` + `showinfo`, timestamps from stderr.
/// showinfo does not print the scene score, so the reported score is
/// the threshold itself — a guaranteed lower bound for frames that
/// passed `gt(scene, threshold)`.
fn scene_via_showinfo(
    program: &Path,
    input_path: &Path,
    threshold: f64,
) -> VideoResult<Vec<(f64, f64)>> {
    let filter = format!("select='gt(scene\\,{threshold})',showinfo");
    let args = ffmpeg_args(input_path, &["-an".to_string(), "-vf".to_string(), filter]);
    let out = run_ffmpeg(program, &args)?;
    let mut times = Vec::new();
    for line in String::from_utf8_lossy(&out.stderr).lines() {
        if let Some(t) = line
            .split_whitespace()
            .find_map(|tok| tok.strip_prefix("pts_time:"))
            .and_then(|v| v.parse::<f64>().ok())
        {
            times.push((t, threshold));
        }
    }
    Ok(times)
}

/// Scene-change `(time, score)` pairs via `select='gt(scene,thr)'` +
/// `metadata=print` (score = `lavfi.scene_score`), with a `showinfo`
/// fallback. Caller must have verified a video stream exists.
fn scene_run(
    program: &Path,
    input_path: &Path,
    meta_path: &Path,
    threshold: f64,
) -> VideoResult<Vec<(f64, f64)>> {
    let filter = format!(
        "select='gt(scene\\,{threshold})',metadata=mode=print:file={}",
        meta_path.to_string_lossy()
    );
    let args = ffmpeg_args(input_path, &["-an".to_string(), "-vf".to_string(), filter]);
    // stderr is informational only; scene records come from the file.
    run_ffmpeg(program, &args)?;
    match read_output(meta_path) {
        Ok(bytes) => Ok(parse_scene_meta(&String::from_utf8_lossy(&bytes))),
        Err(_) => scene_via_showinfo(program, input_path, threshold),
    }
}

/// Validate a caller-supplied scene threshold: finite and
/// non-negative. Values above the filter's score range simply match
/// nothing.
fn check_threshold(threshold: f64) -> VideoResult<()> {
    if !threshold.is_finite() || threshold < 0.0 {
        return Err(VideoError::InvalidInput(format!(
            "scene threshold must be a finite non-negative number, got {threshold}"
        )));
    }
    Ok(())
}

/// Run the analysis suite: `blackdetect`, `freezedetect`,
/// `silencedetect` (audio), and scene detection — each a separate
/// ffmpeg pass to the `null` muxer (black+freeze share one chained
/// `-vf` pass inside [`analyze`]). Detectors whose stream kind is
/// absent yield empty vectors, never errors. `loudness` stays `None`:
/// measurement is owned by `loudness.rs` and is not duplicated here.
pub(crate) fn analyze(input: &[u8], limits: &EngineLimits) -> VideoResult<MediaAnalysis> {
    let input_len = check_input(input)?;
    let info = probe(input)?;
    check_limits(&info, input_len, limits, true)?;
    let program = find_tool(FFMPEG)?;

    let dir = TempDir::new("analyze")?;
    let input_path = write_input(&dir, "input.bin", input)?;
    let duration = info.duration_secs;

    let mut black_segments = Vec::new();
    let mut freeze_segments = Vec::new();
    let mut silence_segments = Vec::new();
    let mut scenes = Vec::new();

    if info.has_video {
        // One video pass: both detectors are V->V filters that log
        // independently to stderr.
        let args = ffmpeg_args(
            &input_path,
            &[
                "-an".to_string(),
                "-vf".to_string(),
                format!("{BLACKDETECT_FILTER},{FREEZEDETECT_FILTER}"),
            ],
        );
        let out = run_ffmpeg(&program, &args)?;
        let text = String::from_utf8_lossy(&out.stderr);
        black_segments = interval_segments(&text, "black_start", "black_end", duration);
        freeze_segments = interval_segments(&text, "freeze_start", "freeze_end", duration);

        let meta_path = dir.file("scenes.txt");
        scenes = scene_run(&program, &input_path, &meta_path, DEFAULT_SCENE_THRESHOLD)?;
    }
    if info.has_audio {
        silence_segments = silence_run(&program, &input_path, duration)?;
    }

    let mut metrics = BTreeMap::new();
    if let Some(d) = info.duration_secs {
        metrics.insert("duration_secs".to_string(), d);
    }
    metrics.insert(
        "nb_video_streams".to_string(),
        info.streams
            .iter()
            .filter(|s| s.codec_type == "video")
            .count() as f64,
    );
    metrics.insert(
        "nb_audio_streams".to_string(),
        info.streams
            .iter()
            .filter(|s| s.codec_type == "audio")
            .count() as f64,
    );
    metrics.insert("scene_threshold".to_string(), DEFAULT_SCENE_THRESHOLD);
    // NOTE: `format_name` is intentionally absent — `metrics` is
    // `BTreeMap<String, f64>` and the container name is a string;
    // stuffing a hash/code into it would fabricate data.

    Ok(MediaAnalysis {
        black_segments,
        freeze_segments,
        silence_segments,
        scenes,
        loudness: None,
        metrics,
    })
}

/// Scene-change timestamps `(time_secs, score)` via `select='gt(scene,
/// threshold)'` + `metadata=print` (real `lavfi.scene_score`), with a
/// `showinfo` fallback reporting the threshold as a lower-bound score.
/// Empty when the input has no video stream.
pub(crate) fn scene_timestamps(
    input: &[u8],
    threshold: f64,
    limits: &EngineLimits,
) -> VideoResult<Vec<(f64, f64)>> {
    check_threshold(threshold)?;
    let prepared = prepare(input, limits, "scene")?;
    if !prepared.info.has_video {
        return Ok(Vec::new());
    }
    let program = find_tool(FFMPEG)?;
    let meta_path = prepared._dir.file("scenes.txt");
    scene_run(&program, &prepared.input_path, &meta_path, threshold)
}

/// Black-frame intervals `[start, end)` via `blackdetect`.
/// Empty when the input has no video stream.
pub(crate) fn detect_black_frames(
    input: &[u8],
    limits: &EngineLimits,
) -> VideoResult<Vec<(f64, f64)>> {
    let prepared = prepare(input, limits, "black")?;
    if !prepared.info.has_video {
        return Ok(Vec::new());
    }
    let program = find_tool(FFMPEG)?;
    black_run(&program, &prepared.input_path, prepared.info.duration_secs)
}

/// Freeze-frame intervals via `freezedetect`. Empty when the input
/// has no video stream.
pub(crate) fn detect_freeze_frames(
    input: &[u8],
    limits: &EngineLimits,
) -> VideoResult<Vec<(f64, f64)>> {
    let prepared = prepare(input, limits, "freeze")?;
    if !prepared.info.has_video {
        return Ok(Vec::new());
    }
    let program = find_tool(FFMPEG)?;
    freeze_run(&program, &prepared.input_path, prepared.info.duration_secs)
}

/// Silence intervals via `silencedetect` (audio only; empty when the
/// input has no audio stream).
pub(crate) fn detect_silence(input: &[u8], limits: &EngineLimits) -> VideoResult<Vec<(f64, f64)>> {
    let prepared = prepare(input, limits, "silence")?;
    if !prepared.info.has_audio {
        return Ok(Vec::new());
    }
    let program = find_tool(FFMPEG)?;
    silence_run(&program, &prepared.input_path, prepared.info.duration_secs)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::crates::video_engine::ops::{FFPROBE, h264_encoder};
    use std::fs;

    fn tools_available() -> bool {
        find_tool(FFMPEG).is_ok() && find_tool(FFPROBE).is_ok()
    }

    /// Run ffmpeg to produce `out` inside `dir`; `None` on any failure
    /// so tests skip cleanly when the toolchain is absent.
    fn encode(dir: &TempDir, name: &str, args: Vec<String>) -> Option<Vec<u8>> {
        let program = find_tool(FFMPEG).ok()?;
        let out = dir.file(name);
        let mut full = vec!["-y".to_string(), "-v".to_string(), "error".to_string()];
        full.extend(args);
        full.push(out.to_string_lossy().into_owned());
        let output = run_tool(FFMPEG, &program, &full, COMMAND_TIMEOUT).ok()?;
        if !output.status.success() {
            return None;
        }
        fs::read(out).ok()
    }

    fn lavfi_input(src: &str) -> Vec<String> {
        vec![
            "-f".to_string(),
            "lavfi".to_string(),
            "-i".to_string(),
            src.to_string(),
        ]
    }

    /// ~1s 160x120 testsrc + 440Hz sine, H.264+AAC mp4.
    fn av_fixture() -> Option<Vec<u8>> {
        if !tools_available() {
            return None;
        }
        let enc = h264_encoder()?;
        let dir = TempDir::new("analysis-test-av").ok()?;
        let mut args = lavfi_input("testsrc=duration=1:size=160x120:rate=10");
        args.extend(lavfi_input("sine=frequency=440:duration=1"));
        args.extend([
            "-c:v".to_string(),
            enc.to_string(),
            "-pix_fmt".to_string(),
            "yuv420p".to_string(),
            "-c:a".to_string(),
            "aac".to_string(),
            "-shortest".to_string(),
        ]);
        encode(&dir, "av.mp4", args)
    }

    /// 1s solid-black video-only clip.
    fn black_fixture() -> Option<Vec<u8>> {
        if !tools_available() {
            return None;
        }
        let enc = h264_encoder()?;
        let dir = TempDir::new("analysis-test-black").ok()?;
        let mut args = lavfi_input("color=black:duration=1:size=160x120:rate=10");
        args.extend([
            "-c:v".to_string(),
            enc.to_string(),
            "-pix_fmt".to_string(),
            "yuv420p".to_string(),
            "-an".to_string(),
        ]);
        encode(&dir, "black.mp4", args)
    }

    /// 2s pure-silence (anullsrc) WAV — audio only.
    fn silence_fixture() -> Option<Vec<u8>> {
        if !tools_available() {
            return None;
        }
        let dir = TempDir::new("analysis-test-silence").ok()?;
        let mut args = lavfi_input("anullsrc=r=44100:cl=mono");
        args.extend([
            "-t".to_string(),
            "2".to_string(),
            "-c:a".to_string(),
            "pcm_s16le".to_string(),
        ]);
        encode(&dir, "silence.wav", args)
    }

    /// 3s static-color clip — frozen for the whole duration
    /// (freezedetect minimum is 2s).
    fn frozen_fixture() -> Option<Vec<u8>> {
        if !tools_available() {
            return None;
        }
        let enc = h264_encoder()?;
        let dir = TempDir::new("analysis-test-frozen").ok()?;
        let mut args = lavfi_input("color=red:duration=3:size=160x120:rate=10");
        args.extend([
            "-c:v".to_string(),
            enc.to_string(),
            "-pix_fmt".to_string(),
            "yuv420p".to_string(),
            "-an".to_string(),
        ]);
        encode(&dir, "frozen.mp4", args)
    }

    /// testsrc → testsrc2 concat: one hard scene cut at ~1s.
    fn scene_fixture() -> Option<Vec<u8>> {
        if !tools_available() {
            return None;
        }
        let enc = h264_encoder()?;
        let dir = TempDir::new("analysis-test-scene").ok()?;
        let mut p1_args = lavfi_input("testsrc=duration=1:size=160x120:rate=10");
        p1_args.extend([
            "-c:v".to_string(),
            enc.to_string(),
            "-pix_fmt".to_string(),
            "yuv420p".to_string(),
            "-an".to_string(),
        ]);
        let part1 = encode(&dir, "part1.mp4", p1_args)?;
        let p1_path = dir.file("part1.mp4");
        let _ = part1; // bytes unused; concat reads the file

        let mut p2_args = lavfi_input("testsrc2=duration=1:size=160x120:rate=10");
        p2_args.extend([
            "-c:v".to_string(),
            enc.to_string(),
            "-pix_fmt".to_string(),
            "yuv420p".to_string(),
            "-an".to_string(),
        ]);
        encode(&dir, "part2.mp4", p2_args)?;
        let p2_path = dir.file("part2.mp4");

        let list = dir.file("list.txt");
        fs::write(
            &list,
            format!(
                "file '{}'\nfile '{}'\n",
                p1_path.display(),
                p2_path.display()
            ),
        )
        .ok()?;
        let program = find_tool(FFMPEG).ok()?;
        let out = dir.file("scene.mp4");
        let args = vec![
            "-y".to_string(),
            "-v".to_string(),
            "error".to_string(),
            "-f".to_string(),
            "concat".to_string(),
            "-safe".to_string(),
            "0".to_string(),
            "-i".to_string(),
            list.to_string_lossy().into_owned(),
            "-c".to_string(),
            "copy".to_string(),
            out.to_string_lossy().into_owned(),
        ];
        let output = run_tool(FFMPEG, &program, &args, COMMAND_TIMEOUT).ok()?;
        if !output.status.success() {
            return None;
        }
        fs::read(out).ok()
    }

    #[test]
    fn analyze_av_fixture_reports_metrics() {
        let Some(fx) = av_fixture() else { return };
        let report = analyze(&fx, &EngineLimits::default()).expect("analyze failed");
        let duration = report
            .metrics
            .get("duration_secs")
            .copied()
            .expect("duration_secs metric");
        assert!((duration - 1.0).abs() < 0.5, "duration {duration}");
        assert_eq!(report.metrics.get("nb_video_streams").copied(), Some(1.0));
        assert_eq!(report.metrics.get("nb_audio_streams").copied(), Some(1.0));
        // A sine tone is not silence; testsrc is neither black nor a
        // >2s freeze.
        assert!(report.silence_segments.is_empty());
        assert!(report.black_segments.is_empty());
        assert!(report.freeze_segments.is_empty());
        assert!(report.loudness.is_none());
    }

    #[test]
    fn detect_black_frames_finds_black_clip() {
        let Some(fx) = black_fixture() else { return };
        let segs = detect_black_frames(&fx, &EngineLimits::default()).expect("blackdetect");
        assert!(!segs.is_empty(), "expected >=1 black segment");
        let (s, e) = segs[0];
        assert!(s < 0.5 && e > 0.5, "black segment {s}..{e} should span ~1s");
    }

    #[test]
    fn detect_freeze_frames_finds_frozen_clip() {
        let Some(fx) = frozen_fixture() else { return };
        let segs = detect_freeze_frames(&fx, &EngineLimits::default()).expect("freezedetect");
        assert!(!segs.is_empty(), "expected >=1 freeze segment");
        // Freeze runs to EOF: the unclosed tail must close at duration.
        let (s, e) = segs[0];
        assert!(s < 0.5, "freeze should start near 0, got {s}");
        assert!(e > 2.0, "freeze should extend past 2s, got {e}");
    }

    #[test]
    fn detect_silence_finds_silence() {
        let Some(fx) = silence_fixture() else { return };
        let segs = detect_silence(&fx, &EngineLimits::default()).expect("silencedetect");
        assert!(!segs.is_empty(), "expected >=1 silence segment");
        // anullsrc is silent end-to-end: coverage should be most of 2s.
        let covered: f64 = segs.iter().map(|(s, e)| e - s).sum();
        assert!(covered > 1.0, "silence coverage {covered}s of 2s");
    }

    #[test]
    fn scene_timestamps_finds_scene_cut() {
        let Some(fx) = scene_fixture() else { return };
        let scenes =
            scene_timestamps(&fx, 0.3, &EngineLimits::default()).expect("scene_timestamps");
        assert!(!scenes.is_empty(), "expected >=1 scene change");
        let (t, score) = scenes[0];
        assert!(t > 0.3 && t < 1.8, "scene at {t}s (cut at ~1s)");
        assert!(score > 0.3, "score {score} should exceed threshold");
    }

    #[test]
    fn detectors_return_empty_without_matching_stream() {
        // WAV fixture has audio but no video: video detectors must
        // return empty vecs, not errors.
        let Some(fx) = silence_fixture() else { return };
        let limits = EngineLimits::default();
        assert!(detect_black_frames(&fx, &limits).unwrap().is_empty());
        assert!(detect_freeze_frames(&fx, &limits).unwrap().is_empty());
        assert!(scene_timestamps(&fx, 0.3, &limits).unwrap().is_empty());
        // And silence on a video-only clip is empty too.
        let Some(black) = black_fixture() else { return };
        assert!(detect_silence(&black, &limits).unwrap().is_empty());
    }
}
