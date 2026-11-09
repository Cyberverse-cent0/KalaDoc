//! Objective quality metrics between a reference and a distorted
//! media file: PSNR, SSIM, and VMAF where the ffmpeg build supports
//! them.
//!
//! Every metric is its own `ffmpeg -i ref -i dist -lavfi <graph>
//! -f null -` invocation; the summary line the metric filter prints to
//! stderr at info level is parsed for the score. Metrics the build
//! lacks (no `psnr`/`ssim` filter, no `libvmaf`) are reported in
//! [`QualityReport::supported_metrics`] and `notes` — never faked.

use std::path::Path;
use std::sync::OnceLock;

use crate::crates::core::engine::EngineLimits;
use crate::crates::video_engine::error::{VideoError, VideoResult};
use crate::crates::video_engine::model::QualityReport;
use crate::crates::video_engine::ops::probe::parse_probe;
use crate::crates::video_engine::ops::{
    COMMAND_TIMEOUT, FFMPEG, PROBE_TIMEOUT, TempDir, check_input, check_limits, failure_detail,
    find_tool, probe_json, run_tool, write_input,
};

/// Metric names accepted by [`compare_quality`], in canonical order.
const KNOWN_METRICS: [&str; 3] = ["psnr", "ssim", "vmaf"];

/// The ffmpeg video filter that implements `metric`. PSNR and SSIM
/// are built-in metadata filters; VMAF requires the `libvmaf` filter,
/// which only exists when ffmpeg was built against libvmaf.
fn metric_filter(metric: &str) -> &'static str {
    match metric {
        "psnr" => "psnr",
        "ssim" => "ssim",
        "vmaf" => "libvmaf",
        _ => unreachable!("metric names are validated before use"),
    }
}

/// Names of every filter in `ffmpeg -filters` output, fetched once and
/// cached (same pattern as [`encoder_available`](super::encoder_available)).
/// An empty list means the probe itself failed — callers then report
/// metrics as unavailable rather than guessing.
fn ffmpeg_filters() -> &'static [String] {
    static FILTERS: OnceLock<Vec<String>> = OnceLock::new();
    FILTERS.get_or_init(|| {
        let Ok(program) = find_tool(FFMPEG) else {
            return Vec::new();
        };
        let args = vec!["-hide_banner".to_string(), "-filters".to_string()];
        match run_tool(FFMPEG, &program, &args, PROBE_TIMEOUT) {
            Ok(output) if output.status.success() => {
                String::from_utf8_lossy(&output.stdout)
                    .lines()
                    .filter_map(|line| {
                        // Lines look like ` T. psnr   VV->V   Calculate ...`:
                        // a short flag field (`T.`, `..C`, `TS.`, ...) then
                        // the filter name.
                        let mut fields = line.split_whitespace();
                        let flags = fields.next()?;
                        if flags.len() <= 4
                            && flags.chars().all(|c| c.is_ascii_uppercase() || c == '.')
                        {
                            fields.next().map(str::to_string)
                        } else {
                            None
                        }
                    })
                    .collect()
            }
            _ => Vec::new(),
        }
    })
}

/// Whether `name` appears in `ffmpeg -filters`.
fn filter_available(name: &str) -> bool {
    ffmpeg_filters().iter().any(|f| f == name)
}

/// Validate `metrics` against [`KNOWN_METRICS`]. Empty means "all".
/// Returns the deduplicated, lowercased request list in caller order.
fn requested_metrics(metrics: &[&str]) -> VideoResult<Vec<String>> {
    if metrics.is_empty() {
        return Ok(KNOWN_METRICS.iter().map(|s| s.to_string()).collect());
    }
    let mut out: Vec<String> = Vec::new();
    for raw in metrics {
        let name = raw.trim().to_ascii_lowercase();
        if !KNOWN_METRICS.contains(&name.as_str()) {
            return Err(VideoError::InvalidInput(format!(
                "unknown quality metric '{raw}'; supported: {}",
                KNOWN_METRICS.join(", ")
            )));
        }
        if !out.contains(&name) {
            out.push(name);
        }
    }
    Ok(out)
}

/// Build the `-lavfi` graph for `metric`.
///
/// `scaled = true` routes the distorted stream through `scale2ref`
/// first so mismatched geometry (dimensions, SAR, frame rate) does not
/// make the metric filter fail:
///
/// ```text
/// [1:v][0:v]scale2ref[scaled][ref];[scaled][ref]<filter>
/// ```
///
/// `scale2ref` scales its *first* input (the distorted stream) to the
/// geometry of its *second* input (the reference) and passes the
/// reference through unchanged as its second output — verified against
/// ffmpeg 7.1. Both outputs must be consumed; the graph errors on
/// unconnected pads. Input order matters for `libvmaf`, whose first
/// input is "main" (the distorted stream being scored) and second is
/// "reference"; PSNR/SSIM are symmetric.
fn metric_graph(metric: &str, scaled: bool) -> String {
    let filter = metric_filter(metric);
    if scaled {
        format!("[1:v][0:v]scale2ref[scaled][ref];[scaled][ref]{filter}")
    } else {
        format!("[1:v][0:v]{filter}")
    }
}

/// Result of scanning a metric filter's stderr for its summary score.
enum Parsed {
    /// A finite score was found.
    Score(f64),
    /// The score is mathematically infinite (identical streams).
    Infinite,
    /// No parseable summary line was found.
    Missing,
}

/// Extract the score a metric filter prints to stderr.
///
/// - PSNR: `[Parsed_psnr_N] PSNR r:.. g:.. b:.. average:41.2 min:..`
///   → the `average:` value. Identical streams print `average:inf`.
/// - SSIM: `[Parsed_ssim_N] SSIM Y:.. U:.. V:.. All:0.97 (...)`
///   → the `All:` value (0.0–1.0 index).
/// - VMAF: `[Parsed_libvmaf_N] VMAF score: 95.2` → the score.
fn parse_metric(metric: &str, stderr: &str) -> Parsed {
    let (line_key, value_key) = match metric {
        "psnr" => ("PSNR", "average:"),
        "ssim" => ("SSIM", "All:"),
        "vmaf" => ("VMAF", "score:"),
        _ => return Parsed::Missing,
    };
    for line in stderr.lines() {
        if !line.contains(line_key) {
            continue;
        }
        let Some(pos) = line.find(value_key) else {
            continue;
        };
        let Some(token) = line[pos + value_key.len()..].split_whitespace().next() else {
            continue;
        };
        if token.eq_ignore_ascii_case("inf") {
            return Parsed::Infinite;
        }
        if let Ok(value) = token.parse::<f64>() {
            if value.is_finite() {
                return Parsed::Score(value);
            }
        }
    }
    Parsed::Missing
}

/// Run one metric comparison. Tries the `scale2ref` graph first
/// (geometry-tolerant), then falls back to the plain two-input graph
/// for ffmpeg builds without `scale2ref`.
///
/// Returns `(score, notes)`. A `None` score is *not* an error: the
/// failure detail is appended to `notes` so the report stays honest
/// about which metrics could not be computed. Spawn/timeout failures
/// still propagate as [`VideoError::ExternalTool`].
fn run_metric(
    program: &Path,
    ref_path: &Path,
    dist_path: &Path,
    metric: &str,
) -> VideoResult<(Option<f64>, Vec<String>)> {
    let mut notes = Vec::new();
    let mut last_detail = String::new();
    for scaled in [true, false] {
        let args = vec![
            "-hide_banner".to_string(),
            "-i".to_string(),
            ref_path.to_string_lossy().into_owned(),
            "-i".to_string(),
            dist_path.to_string_lossy().into_owned(),
            "-lavfi".to_string(),
            metric_graph(metric, scaled),
            "-f".to_string(),
            "null".to_string(),
            "-".to_string(),
        ];
        let output = run_tool(FFMPEG, program, &args, COMMAND_TIMEOUT)?;
        if !output.status.success() {
            last_detail = failure_detail(&output, "ffmpeg exited non-zero");
            continue;
        }
        let stderr = String::from_utf8_lossy(&output.stderr);
        match parse_metric(metric, &stderr) {
            Parsed::Score(value) => return Ok((Some(value), notes)),
            Parsed::Infinite => {
                notes.push(format!(
                    "{metric} reported an infinite score (streams are identical)"
                ));
                return Ok((Some(f64::INFINITY), notes));
            }
            Parsed::Missing => {
                last_detail = "metric summary line not found in ffmpeg output".to_string();
            }
        }
    }
    notes.push(format!(
        "{metric} could not be computed: {}",
        if last_detail.is_empty() {
            "no parseable output".to_string()
        } else {
            last_detail
        }
    ));
    Ok((None, notes))
}

/// Compare `reference` against `distorted`. `metrics` selects which
/// of `"psnr"`, `"ssim"`, `"vmaf"` to run (empty → all supported).
/// Metrics the build lacks are reported in
/// [`QualityReport::supported_metrics`] and `notes`, never faked.
///
/// Both inputs are probed first; `max_input_bytes` and
/// `max_duration_secs` limits are enforced on each. Video quality
/// metrics require a video stream in *both* inputs — otherwise the
/// metric is skipped with a note. Geometry mismatches between the two
/// video streams are handled by routing the distorted stream through
/// `scale2ref` (see [`metric_graph`]), with a plain-graph retry.
pub(crate) fn compare_quality(
    reference: &[u8],
    distorted: &[u8],
    metrics: &[&str],
    limits: &EngineLimits,
) -> VideoResult<QualityReport> {
    // Validate names before touching ffmpeg so an unknown metric fails
    // fast with InvalidInput even when the tools are absent.
    let requested = requested_metrics(metrics)?;

    let ref_len = check_input(reference)?;
    let dist_len = check_input(distorted)?;
    let program = find_tool(FFMPEG)?;

    let dir = TempDir::new("quality")?;
    let ref_path = write_input(&dir, "reference.bin", reference)?;
    let dist_path = write_input(&dir, "distorted.bin", distorted)?;

    // Probe both inputs once: limits plus video-stream presence.
    let ref_info = parse_probe(&probe_json(&ref_path)?, ref_len)?;
    let dist_info = parse_probe(&probe_json(&dist_path)?, dist_len)?;
    check_limits(&ref_info, ref_len, limits, false)?;
    check_limits(&dist_info, dist_len, limits, false)?;

    let both_video = ref_info.has_video && dist_info.has_video;
    let mut report = QualityReport {
        psnr_db: None,
        ssim: None,
        vmaf: None,
        supported_metrics: Vec::new(),
        notes: Vec::new(),
    };
    if !both_video {
        report.notes.push(
            "reference and/or distorted input has no video stream; \
             video quality metrics cannot be computed"
                .to_string(),
        );
    }

    for metric in &requested {
        // Support is a property of the ffmpeg build: a metric counts
        // as supported when its filter exists, regardless of whether
        // this particular input pair can use it.
        let filter = metric_filter(metric);
        if !filter_available(filter) {
            report.notes.push(format!(
                "{metric} is not available in this ffmpeg build (missing '{filter}' filter)"
            ));
            continue;
        }
        report.supported_metrics.push(metric.clone());
        if !both_video {
            continue;
        }
        let (value, mut notes) = run_metric(&program, &ref_path, &dist_path, metric)?;
        match metric.as_str() {
            "psnr" => report.psnr_db = value,
            "ssim" => report.ssim = value,
            "vmaf" => report.vmaf = value,
            _ => {}
        }
        report.notes.append(&mut notes);
    }
    Ok(report)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::crates::video_engine::ops::{FFMPEG, find_tool};

    fn unlimited() -> EngineLimits {
        EngineLimits {
            max_input_bytes: None,
            max_pages: None,
            max_pixels: None,
            max_outputs: None,
            max_duration_secs: None,
        }
    }

    /// Encode a 1s 160x120 clip with the native `mpeg4` encoder (no
    /// external codec libs needed). `extra_filter` is appended to the
    /// lavfi source chain, e.g. `",noise=alls=20:allf=t"`.
    /// Returns `None` when ffmpeg or the encoder is unavailable.
    fn make_clip(dir: &TempDir, name: &str, extra_filter: &str) -> Option<Vec<u8>> {
        let program = find_tool(FFMPEG).ok()?;
        let out = dir.file(name);
        let src = format!("testsrc=duration=1:size=160x120:rate=10{extra_filter}");
        let args: Vec<String> = [
            "-hide_banner",
            "-loglevel",
            "error",
            "-y",
            "-f",
            "lavfi",
            "-i",
            src.as_str(),
            "-c:v",
            "mpeg4",
            "-pix_fmt",
            "yuv420p",
        ]
        .iter()
        .map(|s| s.to_string())
        .chain([out.to_string_lossy().into_owned()])
        .collect();
        let output = run_tool(FFMPEG, &program, &args, COMMAND_TIMEOUT).ok()?;
        if !output.status.success() {
            return None;
        }
        std::fs::read(&out).ok()
    }

    /// Two fixtures: clean `testsrc` and the same source with strong
    /// temporal noise. Skips gracefully without ffmpeg.
    fn fixtures() -> Option<(Vec<u8>, Vec<u8>)> {
        if find_tool(FFMPEG).is_err() {
            return None;
        }
        let dir = TempDir::new("quality-test").ok()?;
        let ref_clip = make_clip(&dir, "ref.avi", "")?;
        let dist_clip = make_clip(&dir, "dist.avi", ",noise=alls=20:allf=t")?;
        Some((ref_clip, dist_clip))
    }

    #[test]
    fn bogus_metric_is_invalid_input() {
        // Validated before any ffmpeg invocation — no tools needed.
        let err = compare_quality(b"x", b"y", &["bogus"], &unlimited()).unwrap_err();
        assert!(matches!(err, VideoError::InvalidInput(_)));
    }

    #[test]
    fn compare_noisy_pair_reports_psnr_and_ssim() {
        let Some((reference, distorted)) = fixtures() else {
            return; // ffmpeg absent — skip.
        };
        let report = compare_quality(
            &reference,
            &distorted,
            &["psnr", "ssim", "vmaf"],
            &unlimited(),
        )
        .unwrap();

        let psnr = report.psnr_db.expect("psnr filter is built into ffmpeg");
        if psnr.is_finite() {
            assert!((0.0..=60.0).contains(&psnr), "psnr out of range: {psnr}");
        }

        let ssim = report.ssim.expect("ssim filter is built into ffmpeg");
        assert!((0.0..=1.0).contains(&ssim), "ssim out of range: {ssim}");

        if filter_available("libvmaf") {
            assert!(report.supported_metrics.iter().any(|m| m == "vmaf"));
            let vmaf = report.vmaf.expect("libvmaf filter present");
            assert!((0.0..=100.0).contains(&vmaf), "vmaf out of range: {vmaf}");
        } else {
            assert!(report.vmaf.is_none());
            assert!(
                report
                    .notes
                    .iter()
                    .any(|n| n.contains("libvmaf") || n.contains("vmaf"))
            );
        }
        assert!(report.supported_metrics.iter().any(|m| m == "psnr"));
        assert!(report.supported_metrics.iter().any(|m| m == "ssim"));
    }

    #[test]
    fn identical_inputs_give_infinite_psnr() {
        let Some((reference, _)) = fixtures() else {
            return;
        };
        let report = compare_quality(&reference, &reference, &["psnr"], &unlimited()).unwrap();
        let psnr = report.psnr_db.expect("psnr should run on identical inputs");
        assert_eq!(psnr, f64::INFINITY);
        assert!(report.notes.iter().any(|n| n.contains("infinite")));
    }

    #[test]
    fn parse_helpers() {
        assert!(matches!(
            parse_metric(
                "psnr",
                "[Parsed_psnr_0 @ 0x1] PSNR r:30.0 g:30.0 b:30.0 average:29.5 min:29.0"
            ),
            Parsed::Score(v) if (v - 29.5).abs() < 1e-9
        ));
        assert!(matches!(
            parse_metric(
                "psnr",
                "[Parsed_psnr_0 @ 0x1] PSNR r:inf g:inf b:inf average:inf min:inf"
            ),
            Parsed::Infinite
        ));
        assert!(matches!(
            parse_metric(
                "ssim",
                "[Parsed_ssim_0 @ 0x1] SSIM Y:0.9 U:0.9 V:0.9 All:0.9123 (12.3)"
            ),
            Parsed::Score(v) if (v - 0.9123).abs() < 1e-9
        ));
        assert!(matches!(
            parse_metric("vmaf", "[Parsed_libvmaf_0 @ 0x1] VMAF score: 95.25"),
            Parsed::Score(v) if (v - 95.25).abs() < 1e-9
        ));
        assert!(matches!(
            parse_metric("psnr", "no metrics here"),
            Parsed::Missing
        ));
        // Metric names are validated upstream; unknown ones never parse.
        assert!(matches!(
            parse_metric("bogus", "PSNR average:1.0"),
            Parsed::Missing
        ));
    }
}
