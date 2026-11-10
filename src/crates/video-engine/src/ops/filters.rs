//! Generic filter-graph passthrough plus a named preset catalog.
//!
//! [`apply_filters`] accepts raw ffmpeg `-vf`/`-af` filter strings —
//! powerful but dangerous if fed untrusted input, so every graph is
//! checked by [`check_graph_safety`] before ffmpeg sees it: filters
//! that can open files or network connections are denied outright and
//! file-path parameters (`fontfile=`, `textfile=`) may not contain
//! `/` or `..`.
//!
//! [`filter_complex`] forwards a full `-filter_complex` graph
//! verbatim (still sandboxed) and auto-maps labeled outputs: link
//! labels that appear exactly once are emitted as `-map "[name]"`
//! pairs. A graph with no output labels gets no `-map` — ffmpeg's
//! default stream selection then applies, which is the caller's
//! responsibility.
//!
//! [`FILTER_PRESETS`] maps friendly names to vetted filter chains.

use std::collections::BTreeMap;
use std::sync::OnceLock;

use serde::Serialize;

use crate::crates::core::engine::EngineLimits;
use crate::crates::video_engine::error::{VideoError, VideoResult};
use crate::crates::video_engine::ops::probe::probe;
use crate::crates::video_engine::ops::{
    COMMAND_TIMEOUT, FFMPEG, PROBE_TIMEOUT, TempDir, check_input, check_limits, ext_for_format,
    find_tool, h264_aac_args, normalize_ext, read_output, run_tool, tool_failure, write_input,
};

/// One entry from `ffmpeg -filters`: a filter name, its capability
/// flags (e.g. `"TSC"`), its input/output stream types (e.g. `"V->V"`),
/// and its one-line description.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct FilterInfo {
    /// Filter name as used in filter graphs (e.g. `"scale"`).
    pub name: String,
    /// Capability flags column (`T` timeline, `S` slice threading,
    /// `C` command support); `""` when the column was absent.
    pub flags: String,
    /// Input/output types column (e.g. `"V->V"`, `"AA->A"`, `"|->V"`).
    pub io: String,
    /// ffmpeg's one-line description of the filter.
    pub description: String,
}

/// Filters that can read files or open network connections — always
/// rejected from user-supplied graphs regardless of parameters.
const DENIED_FILTERS: &[&str] = &["subtitles", "sendcmd", "zmq", "azmq", "movie", "amovie"];

/// Audio-only filters: a bare name (before `=`/`:`) in this set routes
/// a chain to `-af` instead of `-vf`.
const AUDIO_ONLY_FILTERS: &[&str] = &[
    "volume",
    "atempo",
    "afade",
    "aecho",
    "acompressor",
    "equalizer",
    "loudnorm",
    "highpass",
    "lowpass",
    "aloop",
    "apad",
    "atrim",
    "asetpts",
    "aresample",
    "aformat",
    "amix",
    "pan",
    "afftdn",
    "anlmdn",
    "firequalizer",
    "speechnorm",
];

/// Parameters whose values may point at host files; a value containing
/// `/` or `..` would let a graph read arbitrary paths, so it is denied.
const PATH_PARAMS: &[&str] = &["fontfile=", "textfile="];

/// Enumerate the filters compiled into this ffmpeg build via
/// `ffmpeg -hide_banner -filters`. The listing is fetched once and
/// cached; a missing binary is [`VideoError::MissingTool`] and a
/// failed/unparseable run is [`VideoError::ExternalTool`].
pub(crate) fn filters_list() -> VideoResult<Vec<FilterInfo>> {
    static FILTER_LIST: OnceLock<Vec<FilterInfo>> = OnceLock::new();
    let program = find_tool(FFMPEG)?;
    let list = FILTER_LIST.get_or_init(|| {
        let args = vec!["-hide_banner".to_string(), "-filters".to_string()];
        match run_tool(FFMPEG, &program, &args, PROBE_TIMEOUT) {
            Ok(output) if output.status.success() => {
                parse_filter_list(&String::from_utf8_lossy(&output.stdout))
            }
            _ => Vec::new(),
        }
    });
    if list.is_empty() {
        return Err(VideoError::ExternalTool {
            tool: FFMPEG.to_string(),
            detail: "ffmpeg -filters produced no parseable filter entries".to_string(),
        });
    }
    Ok(list.clone())
}

/// Parse `ffmpeg -filters` output. Filter rows look like
/// ` TSC scale   V->V   Scale the input ...`; the input/output type
/// column always contains `->`, which distinguishes rows from the
/// legend/header lines.
fn parse_filter_list(text: &str) -> Vec<FilterInfo> {
    let mut filters = Vec::new();
    for raw in text.lines() {
        let toks: Vec<&str> = raw.split_whitespace().collect();
        let Some(io_pos) = toks.iter().position(|t| t.contains("->")) else {
            continue;
        };
        if io_pos == 0 {
            continue;
        }
        let name = toks[io_pos - 1];
        if name.is_empty() || !name.chars().all(|c| c.is_ascii_alphanumeric() || c == '_') {
            continue;
        }
        // The column before the name is the flags field when present.
        let flags = if io_pos >= 2 && is_flags_column(toks[io_pos - 2]) {
            toks[io_pos - 2]
        } else {
            ""
        };
        filters.push(FilterInfo {
            name: name.to_string(),
            flags: flags.to_string(),
            io: toks[io_pos].to_string(),
            description: toks[io_pos + 1..].join(" "),
        });
    }
    filters
}

/// Whether a token looks like the ` TSC`-style capability column:
/// only the flag characters, at most 5 wide.
fn is_flags_column(tok: &str) -> bool {
    !tok.is_empty()
        && tok.len() <= 5
        && tok
            .chars()
            .all(|c| matches!(c, 'T' | 'S' | 'C' | 'A' | 'V' | 'N' | '|' | '.'))
}

/// Extract the leading filter name of one graph segment: the run of
/// ASCII alphanumerics/underscores at the start (e.g. `"scale"` from
/// `"scale=640:480"`).
fn segment_name(segment: &str) -> &str {
    let end = segment
        .find(|c: char| !(c.is_ascii_alphanumeric() || c == '_'))
        .unwrap_or(segment.len());
    &segment[..end]
}

/// Sandbox a user-supplied filter graph (a `-vf`/`-af` chain or a
/// `-filter_complex` graph). Rejects:
///
/// - any filter in [`DENIED_FILTERS`] (file/network inputs);
/// - `fontfile=`/`textfile=` values containing `/` or `..`.
///
/// Parsing is deliberately conservative: the graph is split on the
/// link/chain separators `[ ] , ; |` and the first identifier of each
/// segment is checked. Anything ambiguous fails closed — ffmpeg still
/// validates the graph itself, so a false negative here only means an
/// honest tool error, never silent acceptance of a dangerous filter.
fn check_graph_safety(graph: &str) -> VideoResult<()> {
    // Path-valued parameters: scan the raw text so the check also
    // fires when the option appears mid-chain.
    for param in PATH_PARAMS {
        let mut rest = graph;
        while let Some(pos) = rest.find(param) {
            let after = &rest[pos + param.len()..];
            let value: String = after
                .chars()
                .take_while(|&c| {
                    !matches!(c, ':' | ',' | ';' | ']' | '[' | '|' | ' ' | '\t' | '\n')
                })
                .collect();
            if value.contains('/') || value.contains("..") {
                return Err(VideoError::InvalidInput(format!(
                    "filter parameter '{param}' may not reference host paths \
                     (got {value:?}); absolute paths and '..' are forbidden"
                )));
            }
            rest = after;
        }
    }

    for segment in graph.split(|c| matches!(c, '[' | ']' | ',' | ';' | '|')) {
        let segment = segment.trim_start();
        if segment.is_empty() {
            continue;
        }
        let name = segment_name(segment);
        if name.is_empty() {
            continue;
        }
        if DENIED_FILTERS.contains(&name) {
            return Err(VideoError::InvalidInput(format!(
                "filter '{name}' is not allowed in user-supplied graphs: it can \
                 read files or open network connections"
            )));
        }
    }
    Ok(())
}

/// Split a filter chain on top-level commas, honoring single-quoted
/// and backslash-escaped sections so `drawtext=text='a,b'` stays one
/// filter.
fn split_chain(chain: &str) -> Vec<String> {
    let mut parts = Vec::new();
    let mut cur = String::new();
    let mut in_quote = false;
    let mut escaped = false;
    for c in chain.chars() {
        if escaped {
            cur.push(c);
            escaped = false;
            continue;
        }
        match c {
            '\\' => {
                cur.push(c);
                escaped = true;
            }
            '\'' => {
                cur.push(c);
                in_quote = !in_quote;
            }
            ',' if !in_quote => {
                parts.push(cur.trim().to_string());
                cur.clear();
            }
            _ => cur.push(c),
        }
    }
    if !cur.trim().is_empty() {
        parts.push(cur.trim().to_string());
    }
    parts.retain(|p| !p.is_empty());
    parts
}

/// Whether a filter name belongs to the audio-only set.
fn is_audio_only(name: &str) -> bool {
    AUDIO_ONLY_FILTERS.contains(&name)
}

/// Route a list of filter specs into `(video_chain, audio_chain)`:
/// each spec's leading filter name decides the destination; specs
/// within each group are joined with `,` in input order. Returns
/// `None` for a group with no members.
pub(crate) fn route_filters(filters: &[String]) -> (Option<String>, Option<String>) {
    let mut video: Vec<String> = Vec::new();
    let mut audio: Vec<String> = Vec::new();
    for spec in filters {
        // A spec may itself be a comma chain; split it so each
        // individual filter routes correctly.
        for part in split_chain(spec) {
            let name = segment_name(part.trim_start());
            if is_audio_only(name) {
                audio.push(part);
            } else {
                video.push(part);
            }
        }
    }
    (
        (!video.is_empty()).then(|| video.join(",")),
        (!audio.is_empty()).then(|| audio.join(",")),
    )
}

/// Shared runner: probe + limit-check the input, apply `-vf`/`-af`
/// chains (each already sandboxed by the caller or re-checked here),
/// and re-encode to H.264/AAC in an MP4 container.
fn run_av_filters(
    input: &[u8],
    video_filter: Option<&str>,
    audio_filter: Option<&str>,
    limits: &EngineLimits,
) -> VideoResult<Vec<u8>> {
    let vf = video_filter.map(str::trim).filter(|s| !s.is_empty());
    let af = audio_filter.map(str::trim).filter(|s| !s.is_empty());
    if vf.is_none() && af.is_none() {
        return Err(VideoError::InvalidInput(
            "at least one non-empty video or audio filter is required".to_string(),
        ));
    }
    for chain in [vf, af].into_iter().flatten() {
        check_graph_safety(chain)?;
    }

    let input_len = check_input(input)?;
    let program = find_tool(FFMPEG)?;
    let info = probe(input)?;
    check_limits(&info, input_len, limits, false)?;

    let dir = TempDir::new("filters")?;
    let input_path = write_input(&dir, "input.bin", input)?;
    // Filters change pixels/samples, so the output is always a fresh
    // H.264/AAC MP4 regardless of the input container.
    let out_path = dir.file("out.mp4");

    let mut args = vec![
        "-y".to_string(),
        "-v".to_string(),
        "error".to_string(),
        "-i".to_string(),
        input_path.to_string_lossy().into_owned(),
    ];
    if let Some(chain) = vf {
        args.push("-vf".to_string());
        args.push(chain.to_string());
    }
    if let Some(chain) = af {
        args.push("-af".to_string());
        args.push(chain.to_string());
    }
    args.extend(h264_aac_args()?);
    args.push(out_path.to_string_lossy().into_owned());

    let output = run_tool(FFMPEG, &program, &args, COMMAND_TIMEOUT)?;
    if !output.status.success() {
        return Err(tool_failure(FFMPEG, &output));
    }
    read_output(&out_path)
}

/// Apply `video_filter` (`-vf`) and/or `audio_filter` (`-af`) to
/// `input`, re-encoding via the standard path. At least one filter
/// must be non-empty. Filter strings are passed as single argv
/// elements — never through a shell — and are sandboxed by
/// [`check_graph_safety`] first.
pub(crate) fn apply_filters(
    input: &[u8],
    video_filter: Option<&str>,
    audio_filter: Option<&str>,
    limits: &EngineLimits,
) -> VideoResult<Vec<u8>> {
    run_av_filters(input, video_filter, audio_filter, limits)
}

/// Apply a list of individual filter specs (e.g. `["hflip",
/// "volume=6dB"]`): audio-only filters are routed to `-af`, everything
/// else to `-vf`, then both chains run through [`run_av_filters`].
/// This is the list-oriented counterpart of [`apply_filters`].
pub(crate) fn apply_filter_list(
    input: &[u8],
    filters: &[String],
    limits: &EngineLimits,
) -> VideoResult<Vec<u8>> {
    if filters.is_empty() {
        return Err(VideoError::InvalidInput(
            "apply_filter_list requires at least one filter".to_string(),
        ));
    }
    let (vf, af) = route_filters(filters);
    run_av_filters(input, vf.as_deref(), af.as_deref(), limits)
}

/// Link labels inside `[...]` that are stream specifiers rather than
/// named link labels: anything containing `:` (`[0:v]`) or purely
/// numeric (`[0]`) addresses an input stream, not a filter output.
fn is_stream_specifier(label: &str) -> bool {
    label.contains(':') || label.chars().all(|c| c.is_ascii_digit())
}

/// Collect `[name]` link labels from a `-filter_complex` graph and
/// return those that appear exactly once — i.e. labels produced by a
/// filter but never consumed — in order of first appearance. These
/// are the graph's outputs and become `-map "[name]"` arguments.
fn output_labels(graph: &str) -> Vec<String> {
    let mut counts: BTreeMap<String, usize> = BTreeMap::new();
    let mut order: Vec<String> = Vec::new();
    let mut rest = graph;
    while let Some(open) = rest.find('[') {
        let after_open = &rest[open + 1..];
        let Some(close) = after_open.find(']') else {
            break;
        };
        let label = &after_open[..close];
        if !label.is_empty() && !is_stream_specifier(label) {
            if !counts.contains_key(label) {
                order.push(label.to_string());
            }
            *counts.entry(label.to_string()).or_insert(0) += 1;
        }
        rest = &after_open[close + 1..];
    }
    order
        .into_iter()
        .filter(|l| counts.get(l) == Some(&1))
        .collect()
}

/// Resolve a caller-supplied `output_format` to a container extension.
/// Accepts either a probed format name (`"mov,mp4,..."`, `"matroska"`)
/// via [`ext_for_format`] or a plain extension/format token
/// (`"mp4"`, `"mkv"`, `"webm"`, `"avi"`, `"mov"`, `"ts"`) via an
/// explicit allowlist. Anything else is [`VideoError::InvalidInput`].
fn resolve_output_ext(output_format: &str) -> VideoResult<String> {
    let norm = normalize_ext(output_format);
    if let Some(ext) = ext_for_format(&norm) {
        return Ok(ext.to_string());
    }
    const ALLOWED_EXTS: &[&str] = &["mp4", "mkv", "webm", "avi", "mov", "ts"];
    if ALLOWED_EXTS.contains(&norm.as_str()) {
        return Ok(norm);
    }
    Err(VideoError::InvalidInput(format!(
        "unsupported output format '{output_format}'; allowed containers: \
         mp4, mkv, webm, avi, mov, mpeg-ts (or a probed format name)"
    )))
}

/// Apply a raw `-filter_complex` graph to `input`.
///
/// `graph` is passed to ffmpeg verbatim as a single argv element after
/// [`check_graph_safety`] sandboxing. `output_format` selects the
/// container (see [`resolve_output_ext`]); `webm` re-encodes to
/// VP9/Opus, everything else to H.264/AAC — stream copy is never used
/// because it cannot carry filter output.
///
/// Labeled outputs are auto-mapped: every `[name]` link label that
/// appears exactly once in the graph is emitted as `-map "[name]"`.
/// A graph with no such labels gets no `-map` and ffmpeg's default
/// stream selection applies.
pub(crate) fn filter_complex(
    input: &[u8],
    graph: &str,
    output_format: &str,
    limits: &EngineLimits,
) -> VideoResult<Vec<u8>> {
    if graph.trim().is_empty() {
        return Err(VideoError::InvalidInput(
            "filter_complex requires a non-empty graph".to_string(),
        ));
    }
    check_graph_safety(graph)?;
    let ext = resolve_output_ext(output_format)?;

    let input_len = check_input(input)?;
    let program = find_tool(FFMPEG)?;
    let info = probe(input)?;
    check_limits(&info, input_len, limits, false)?;

    let dir = TempDir::new("filter-complex")?;
    let input_path = write_input(&dir, "input.bin", input)?;
    let out_path = dir.file(&format!("out.{ext}"));

    let mut args = vec![
        "-y".to_string(),
        "-v".to_string(),
        "error".to_string(),
        "-i".to_string(),
        input_path.to_string_lossy().into_owned(),
        "-filter_complex".to_string(),
        graph.to_string(),
    ];
    for label in output_labels(graph) {
        args.push("-map".to_string());
        args.push(format!("[{label}]"));
    }
    if ext == "webm" {
        args.extend([
            "-c:v".to_string(),
            "libvpx-vp9".to_string(),
            "-deadline".to_string(),
            "good".to_string(),
            "-cpu-used".to_string(),
            "8".to_string(),
            "-c:a".to_string(),
            "libopus".to_string(),
        ]);
    } else {
        args.extend(h264_aac_args()?);
    }
    args.push(out_path.to_string_lossy().into_owned());

    let output = run_tool(FFMPEG, &program, &args, COMMAND_TIMEOUT)?;
    if !output.status.success() {
        return Err(tool_failure(FFMPEG, &output));
    }
    read_output(&out_path)
}

/// A named, vetted filter preset: `name` → (video filter, audio
/// filter). Both may be empty for analysis-only presets.
pub struct FilterPreset {
    /// Stable preset identifier (CLI/API surface).
    pub name: &'static str,
    /// ffmpeg `-vf` chain, `""` for none.
    pub video_filter: &'static str,
    /// ffmpeg `-af` chain, `""` for none.
    pub audio_filter: &'static str,
    /// One-line description for help output.
    pub description: &'static str,
}

/// Catalog of safe presets. Every filter referenced here was verified
/// against `ffmpeg -filters` on the reference build (ffmpeg 7.1);
/// presets whose filters are missing on a given build fail honestly
/// at ffmpeg time rather than being silently approximated.
pub(crate) fn filter_presets() -> Vec<FilterPreset> {
    vec![
        FilterPreset {
            name: "grayscale",
            video_filter: "hue=s=0",
            audio_filter: "",
            description: "Desaturate to grayscale",
        },
        FilterPreset {
            name: "sepia",
            video_filter: "colorchannelmixer=.393:.769:.189:0:.349:.686:.168:0:.272:.534:.131",
            audio_filter: "",
            description: "Warm sepia tone",
        },
        FilterPreset {
            name: "negate",
            video_filter: "negate",
            audio_filter: "",
            description: "Invert colors",
        },
        FilterPreset {
            name: "blur",
            video_filter: "gblur=sigma=4",
            audio_filter: "",
            description: "Gaussian blur",
        },
        FilterPreset {
            name: "sharpen",
            video_filter: "unsharp=5:5:1.0:5:5:0.0",
            audio_filter: "",
            description: "Sharpen edges",
        },
        FilterPreset {
            name: "denoise",
            video_filter: "hqdn3d",
            audio_filter: "",
            description: "Light temporal/spatial denoise",
        },
        FilterPreset {
            name: "deinterlace",
            video_filter: "yadif",
            audio_filter: "",
            description: "Deinterlace to progressive scan",
        },
        FilterPreset {
            name: "vignette",
            video_filter: "vignette",
            audio_filter: "",
            description: "Darken frame edges",
        },
        FilterPreset {
            name: "fade-in",
            video_filter: "fade=t=in:st=0:d=1",
            audio_filter: "",
            description: "Fade in over the first second",
        },
        FilterPreset {
            name: "fade-out",
            video_filter: "fade=t=out:st=0:d=1",
            audio_filter: "",
            description: "Fade out over the last-configured second \
                          (adjust st= for clips longer than 1s)",
        },
        FilterPreset {
            name: "volume-boost",
            video_filter: "",
            audio_filter: "volume=1.5",
            description: "Raise audio volume by ~3.5 dB",
        },
        FilterPreset {
            name: "mono",
            video_filter: "",
            audio_filter: "pan=1c|c0=0.5*c0+0.5*c1",
            description: "Downmix stereo to mono",
        },
        FilterPreset {
            name: "normalize",
            video_filter: "",
            audio_filter: "loudnorm",
            description: "Normalize loudness (EBU R128, dynamic mode)",
        },
    ]
}

/// Look up a preset by name (case-insensitive).
pub(crate) fn preset_by_name(name: &str) -> Option<FilterPreset> {
    filter_presets()
        .into_iter()
        .find(|p| p.name.eq_ignore_ascii_case(name))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::crates::video_engine::ops::tool_available;
    use std::fs;
    use std::path::PathBuf;
    use std::process::Command;
    use std::sync::atomic::{AtomicU64, Ordering};
    use std::time::{SystemTime, UNIX_EPOCH};

    fn tools() -> bool {
        tool_available(FFMPEG) && tool_available("ffprobe")
    }

    /// 1s 320x240@10fps clip with a 440 Hz sine track.
    fn make_clip(dir: &PathBuf, name: &str) -> Vec<u8> {
        let out = dir.join(name);
        let status = Command::new(FFMPEG)
            .args([
                "-y",
                "-v",
                "error",
                "-f",
                "lavfi",
                "-i",
                "testsrc=duration=1:size=320x240:rate=10",
                "-f",
                "lavfi",
                "-i",
                "sine=frequency=440:duration=1",
                "-pix_fmt",
                "yuv420p",
                "-shortest",
            ])
            .arg(&out)
            .stdin(std::process::Stdio::null())
            .stdout(std::process::Stdio::null())
            .stderr(std::process::Stdio::piped())
            .status()
            .expect("failed to spawn ffmpeg");
        assert!(status.success(), "ffmpeg fixture generation failed");
        fs::read(&out).unwrap()
    }

    struct TestDir(PathBuf);

    impl TestDir {
        fn new(tag: &str) -> Self {
            static COUNTER: AtomicU64 = AtomicU64::new(0);
            let nanos = SystemTime::now()
                .duration_since(UNIX_EPOCH)
                .map(|d| d.as_nanos())
                .unwrap_or(0);
            let seq = COUNTER.fetch_add(1, Ordering::Relaxed);
            let path = std::env::temp_dir().join(format!(
                "kaladoc-video-filters-test-{tag}-{}-{nanos}-{seq}",
                std::process::id()
            ));
            fs::create_dir_all(&path).unwrap();
            Self(path)
        }
    }

    impl Drop for TestDir {
        fn drop(&mut self) {
            let _ = fs::remove_dir_all(&self.0);
        }
    }

    fn limits() -> EngineLimits {
        EngineLimits::default()
    }

    #[test]
    fn filters_list_non_empty_and_contains_scale() {
        if !tools() {
            return;
        }
        let list = filters_list().expect("filters_list");
        assert!(!list.is_empty());
        assert!(
            list.iter().any(|f| f.name == "scale"),
            "expected 'scale' in ffmpeg -filters output"
        );
    }

    #[test]
    fn apply_filters_video_chain() {
        if !tools() {
            return;
        }
        let dir = TestDir::new("vf");
        let clip = make_clip(&dir.0, "clip.mp4");
        let out =
            apply_filters(&clip, Some("hflip"), None, &limits()).expect("apply_filters hflip");
        assert!(!out.is_empty());
    }

    #[test]
    fn apply_filters_audio_chain_routes_to_af() {
        if !tools() {
            return;
        }
        let dir = TestDir::new("af");
        let clip = make_clip(&dir.0, "clip.mp4");
        let filters = vec!["volume=6dB".to_string()];
        let out = apply_filter_list(&clip, &filters, &limits()).expect("volume filter");
        assert!(!out.is_empty());
    }

    #[test]
    fn filter_complex_labeled_output() {
        if !tools() {
            return;
        }
        let dir = TestDir::new("fc");
        let clip = make_clip(&dir.0, "clip.mp4");
        let out =
            filter_complex(&clip, "[0:v]hflip[v]", "mp4", &limits()).expect("filter_complex hflip");
        assert!(!out.is_empty());
        // Sanity: output should still probe as media.
        let info = probe(&out).expect("output probes as media");
        assert!(info.has_video);
    }

    #[test]
    fn sandbox_rejects_file_reading_filters() {
        if !tools() {
            return;
        }
        let dir = TestDir::new("sandbox");
        let clip = make_clip(&dir.0, "clip.mp4");
        for graph in [
            "subtitles=x.srt",
            "[0:v]sendcmd=f=/tmp/cmds[v]",
            "movie=/etc/passwd",
        ] {
            let err =
                filter_complex(&clip, graph, "mp4", &limits()).expect_err("graph must be rejected");
            assert!(
                matches!(err, VideoError::InvalidInput(_)),
                "expected InvalidInput for {graph:?}, got {err:?}"
            );
        }
        let filters = vec!["subtitles=x.srt".to_string()];
        let err = apply_filter_list(&clip, &filters, &limits()).expect_err("must reject");
        assert!(matches!(err, VideoError::InvalidInput(_)));
    }

    #[test]
    fn sandbox_rejects_path_params() {
        if !tools() {
            return;
        }
        for graph in [
            "drawtext=textfile=/etc/passwd",
            "drawtext=fontfile=../secrets.ttf",
        ] {
            let err = check_graph_safety(graph).expect_err("must reject path param");
            assert!(matches!(err, VideoError::InvalidInput(_)));
        }
        assert!(check_graph_safety("drawtext=textfile=fonts/arial.ttf").is_err());
        assert!(check_graph_safety("drawtext=textfile=myfont.ttf").is_ok());
    }

    #[test]
    fn route_filters_splits_audio_and_video() {
        let filters = vec![
            "hflip".to_string(),
            "volume=3dB".to_string(),
            "scale=640:480".to_string(),
            "afade=t=in".to_string(),
        ];
        let (vf, af) = route_filters(&filters);
        assert_eq!(vf.as_deref(), Some("hflip,scale=640:480"));
        assert_eq!(af.as_deref(), Some("volume=3dB,afade=t=in"));
    }

    #[test]
    fn output_labels_picks_single_use_names() {
        let graph = "[0:v]split[a][b];[a]hflip[v];[b]scale[s]";
        // 'v' and 's' each appear once; 'a','b' appear twice (out+in);
        // '0:v' is a stream specifier.
        assert_eq!(output_labels(graph), vec!["v".to_string(), "s".to_string()]);
        assert_eq!(output_labels("[0:v]hflip"), Vec::<String>::new());
    }
}
