//! Arbitrary validated filter-graph passthrough — the escape hatch.
//!
//! [`apply_filters`] forwards caller-supplied `-vf`/`-af` filter
//! chains, `-map` stream specs and `-frames:v` counts straight to
//! `ffmpeg`. Because the graphs are arbitrary strings, this module
//! validates them before spawning:
//!
//! - Each chain is split on **top-level** commas. The splitter is
//!   intentionally minimal: it tracks `()` nesting depth and
//!   single/double quotes (with `\` escaping the next character
//!   inside quotes). It does not implement ffmpeg's full escaping
//!   rules — callers needing exotic quoting should keep graphs
//!   simple. Splitting is used only for validation; the original
//!   string is passed to ffmpeg verbatim.
//! - Every filter name (the token before the first `=` in each
//!   comma-separated element) must exist in
//!   [`RuntimeCapabilities::has_filter`]; unknown names yield
//!   [`MediaError::Unsupported`] naming all missing filters.
//! - Shell metacharacters (`; | & $` and newlines) are rejected with
//!   [`MediaError::InvalidInput`]. argv execution means no shell is
//!   involved, but `;` is *also* ffmpeg's filter-chain separator —
//!   allowing it would silently smuggle in filters the validation
//!   above never checked.
//! - Unbalanced `()`/quotes are rejected for the same reason: a
//!   malformed graph could hide filter elements from the splitter.

use std::collections::BTreeSet;
use std::ffi::OsString;

use serde::{Deserialize, Serialize};

use crate::crates::multimedia_engine::error::{MediaError, MediaResult};
use crate::crates::multimedia_engine::model::{MediaLimits, MediaOutput};
use crate::crates::multimedia_engine::runtime::discovery::Runtime;
use crate::crates::multimedia_engine::runtime::process;
use crate::crates::multimedia_engine::runtime::tempdir::TempDir;

/// Characters that must never appear in a filter graph, map spec or
/// output-format token. `;` doubles as ffmpeg's filter-chain
/// separator, so it is rejected outright rather than parsed.
const FORBIDDEN_CHARS: &[char] = &[';', '|', '&', '$', '\n', '\r'];

/// Options for [`apply_filters`].
///
/// All fields are optional except `output_format`; unset fields
/// leave ffmpeg's defaults (all streams copied/mapped, no frame cap).
///
/// `video_filter` / `audio_filter` are raw ffmpeg filtergraph
/// strings passed verbatim as `-vf` / `-af` values after validation —
/// e.g. `"scale=640:360,eq=brightness=0.1"`. `map` is a raw `-map`
/// argument (e.g. `"0:v:0"`, `"0:a?"`). `frames` becomes
/// `-frames:v N`.
///
/// Output is always **re-encoded** — filter graphs cannot run on
/// stream-copied data, so ffmpeg picks the container's default
/// encoders unless the graph itself forces otherwise.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct FilterGraphOptions {
    /// Video filtergraph for `-vf` (comma-separated filter chain).
    pub video_filter: Option<String>,
    /// Audio filtergraph for `-af` (comma-separated filter chain).
    pub audio_filter: Option<String>,
    /// `-map` stream specifier (e.g. `"0:v:0"`, `"0:a?"`).
    pub map: Option<String>,
    /// Output video frame cap, passed as `-frames:v N`.
    pub frames: Option<u32>,
    /// Output container extension / format name without a leading
    /// dot (e.g. `"mp4"`, `"mkv"`). Becomes `output.<format>`.
    pub output_format: String,
}

/// Apply validated filter graphs to `input` and return the
/// re-encoded media.
///
/// Runs `ffmpeg -hide_banner -loglevel error -nostdin -y -i <in>`
/// followed by the validated `-vf`/`-af`/`-map`/`-frames:v` options,
/// then reads the output back under `limits`.
///
/// # Errors
///
/// - [`MediaError::InvalidInput`] — empty/oversized input, empty
///   `output_format`, metacharacters (`;|&$`, newlines), unbalanced
///   quotes/parens, or a malformed filter element.
/// - [`MediaError::Unsupported`] — a filter named in `video_filter`
///   or `audio_filter` is not in this FFmpeg build's capability set.
///   All missing names are listed in one error.
/// - [`MediaError::ToolFailed`] / [`MediaError::Timeout`] — ffmpeg
///   rejected the input or graph.
pub fn apply_filters(
    input: &[u8],
    options: &FilterGraphOptions,
    limits: &MediaLimits,
    rt: &Runtime,
) -> MediaResult<MediaOutput> {
    if input.is_empty() {
        return Err(MediaError::InvalidInput("empty input buffer".to_string()));
    }
    if let Some(max) = limits.max_input_bytes
        && (input.len() as u64) > max
    {
        return Err(MediaError::LimitExceeded(format!(
            "input is {} bytes, exceeding max_input_bytes {max}",
            input.len()
        )));
    }
    let output_ext = options.output_format.trim().trim_start_matches('.');
    if output_ext.is_empty() {
        return Err(MediaError::InvalidInput(
            "output_format must not be empty".to_string(),
        ));
    }
    reject_forbidden("output_format", output_ext)?;

    // Validate both graphs against the runtime capability set before
    // spawning ffmpeg — one Unsupported error lists every missing
    // filter name across both chains.
    let caps = rt.capabilities()?;
    let mut missing: BTreeSet<String> = BTreeSet::new();
    for (kind, graph) in [
        ("video_filter", options.video_filter.as_deref()),
        ("audio_filter", options.audio_filter.as_deref()),
    ] {
        if let Some(graph) = graph.filter(|g| !g.trim().is_empty()) {
            for name in filter_names(graph, kind)? {
                if !caps.has_filter(&name) {
                    missing.insert(name);
                }
            }
        }
    }
    if !missing.is_empty() {
        return Err(MediaError::Unsupported(format!(
            "filter(s) not available in this FFmpeg build: {}",
            missing.into_iter().collect::<Vec<_>>().join(", ")
        )));
    }
    if let Some(map) = options.map.as_deref().filter(|m| !m.trim().is_empty()) {
        reject_forbidden("map", map)?;
    }

    let dir = TempDir::new("filtergraph")?;
    let input_name = "input.bin";
    // Neutral extension: ffmpeg probes by content so raw elementary
    // streams still demux correctly.
    dir.write(input_name, input)?;

    let output_name = format!("output.{output_ext}");
    let output_path = dir.file(&output_name);

    let mut argv: Vec<OsString> = vec![
        OsString::from("-hide_banner"),
        OsString::from("-loglevel"),
        OsString::from("error"),
        OsString::from("-nostdin"),
        OsString::from("-y"),
        OsString::from("-i"),
        dir.file(input_name).into_os_string(),
    ];

    if let Some(graph) = options
        .video_filter
        .as_deref()
        .filter(|g| !g.trim().is_empty())
    {
        argv.push(OsString::from("-vf"));
        argv.push(OsString::from(graph));
    }
    if let Some(graph) = options
        .audio_filter
        .as_deref()
        .filter(|g| !g.trim().is_empty())
    {
        argv.push(OsString::from("-af"));
        argv.push(OsString::from(graph));
    }
    if let Some(map) = options.map.as_deref().filter(|m| !m.trim().is_empty()) {
        argv.push(OsString::from("-map"));
        argv.push(OsString::from(map));
    }
    if let Some(frames) = options.frames {
        argv.push(OsString::from("-frames:v"));
        argv.push(OsString::from(frames.to_string()));
    }

    argv.push(output_path.into_os_string());

    process::run_checked("ffmpeg", &rt.ffmpeg, &argv, None, limits)?;

    let bytes = dir.read_limited(&output_name, limits.max_output_bytes)?;
    if bytes.is_empty() {
        return Err(MediaError::ToolFailed {
            tool: "ffmpeg",
            status: "empty output".to_string(),
            detail: format!("ffmpeg produced an empty {output_name}"),
        });
    }

    let mut out = MediaOutput::single(output_name, bytes);
    if let Some(file) = out.primary.as_mut() {
        file.content_type = content_type_for(output_ext);
    }
    Ok(out)
}

/// List every filter name in this FFmpeg build's capability set,
/// sorted. Convenience wrapper over
/// [`Runtime::capabilities`].
pub fn list_filters(rt: &Runtime) -> MediaResult<Vec<String>> {
    let caps = rt.capabilities()?;
    Ok(caps.filters.iter().cloned().collect())
}

/// Reject a string containing any [`FORBIDDEN_CHARS`].
fn reject_forbidden(field: &str, value: &str) -> MediaResult<()> {
    if let Some(bad) = value.chars().find(|c| FORBIDDEN_CHARS.contains(c)) {
        return Err(MediaError::InvalidInput(format!(
            "{field} contains forbidden character {bad:?}"
        )));
    }
    Ok(())
}

/// Split `graph` on top-level commas and return the name of each
/// filter element (the token before the first `=`, trimmed).
///
/// The splitter respects `()` nesting and single/double quotes so a
/// comma inside `drawtext=text='a,b'` does not split. It is a
/// *validation* splitter only — ffmpeg receives the original string.
///
/// Errors: forbidden characters, unbalanced quotes/parens, or an
/// element with no usable filter name.
fn filter_names(graph: &str, field: &str) -> MediaResult<Vec<String>> {
    reject_forbidden(field, graph)?;

    let mut elements: Vec<&str> = Vec::new();
    let mut depth = 0i32;
    let mut quote: Option<char> = None;
    let mut start = 0usize;
    let mut chars = graph.char_indices().peekable();

    while let Some((i, c)) = chars.next() {
        match quote {
            Some(q) => {
                if c == '\\' {
                    // Escaped char inside quotes — skip it so an
                    // escaped quote doesn't end the quoting.
                    chars.next();
                } else if c == q {
                    quote = None;
                }
            }
            None => match c {
                '\'' | '"' => quote = Some(c),
                '(' => depth += 1,
                ')' => {
                    depth -= 1;
                    if depth < 0 {
                        return Err(MediaError::InvalidInput(format!(
                            "{field} has unbalanced ')'"
                        )));
                    }
                }
                ',' if depth == 0 => {
                    elements.push(&graph[start..i]);
                    start = i + 1;
                }
                _ => {}
            },
        }
    }
    if quote.is_some() {
        return Err(MediaError::InvalidInput(format!(
            "{field} has an unterminated quote"
        )));
    }
    if depth != 0 {
        return Err(MediaError::InvalidInput(format!(
            "{field} has unbalanced parentheses"
        )));
    }
    elements.push(&graph[start..]);

    let mut names = Vec::with_capacity(elements.len());
    for element in elements {
        let element = element.trim();
        if element.is_empty() {
            return Err(MediaError::InvalidInput(format!(
                "{field} has an empty filter element"
            )));
        }
        // Name = token before the first '='. Strip ffmpeg's
        // "filter@" instance-label suffix is NOT done — ffmpeg itself
        // would treat `name@` as a label, and labels like `split=2[a]`
        // never reach this code path as a name anyway.
        let name = element
            .split('=')
            .next()
            .unwrap_or("")
            .trim()
            .trim_end_matches('@');
        if name.is_empty() {
            return Err(MediaError::InvalidInput(format!(
                "{field} element {element:?} has no filter name"
            )));
        }
        if !name.chars().all(|c| c.is_ascii_alphanumeric() || c == '_') {
            return Err(MediaError::InvalidInput(format!(
                "{field} filter name {name:?} contains invalid characters"
            )));
        }
        names.push(name.to_string());
    }
    Ok(names)
}

/// Best-effort MIME type for common output containers (mirrors
/// `conversion::transcode`).
fn content_type_for(format: &str) -> Option<String> {
    let mime = match format.to_ascii_lowercase().as_str() {
        "mp4" | "m4v" => "video/mp4",
        "mkv" => "video/x-matroska",
        "webm" => "video/webm",
        "mov" => "video/quicktime",
        "avi" => "video/x-msvideo",
        "mp3" => "audio/mpeg",
        "m4a" | "aac" => "audio/mp4",
        "wav" => "audio/wav",
        "ogg" | "opus" => "audio/ogg",
        "flac" => "audio/flac",
        "gif" => "image/gif",
        _ => return None,
    };
    Some(mime.to_string())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::crates::multimedia_engine::runtime::process::run;
    use std::path::PathBuf;

    fn runtime() -> Runtime {
        Runtime::discover().expect("ffmpeg/ffprobe must be on PATH")
    }

    fn limits() -> MediaLimits {
        MediaLimits::default()
    }

    /// Generate a 1-second testsrc+sine mp4 via lavfi.
    fn sample_input() -> Vec<u8> {
        let rt = runtime();
        let dir = TempDir::new("graph-test-in").expect("tempdir");
        let out_path: PathBuf = dir.file("sample.mp4");
        let argv = process::args(&[
            "-hide_banner",
            "-loglevel",
            "error",
            "-nostdin",
            "-y",
            "-f",
            "lavfi",
            "-i",
            "testsrc=duration=1:size=160x120:rate=10",
            "-f",
            "lavfi",
            "-i",
            "sine=frequency=440:duration=1",
            "-c:v",
            "mpeg4",
            "-c:a",
            "aac",
            "-shortest",
            out_path.to_str().expect("utf8 path"),
        ]);
        run("ffmpeg", &rt.ffmpeg, &argv, None, &limits()).expect("generate sample");
        std::fs::read(&out_path).expect("read sample")
    }

    #[test]
    fn apply_filters_accepts_valid_chain() {
        let rt = runtime();
        let input = sample_input();
        assert!(!input.is_empty());

        let options = FilterGraphOptions {
            video_filter: Some("hflip,scale=32:32".to_string()),
            audio_filter: Some("volume=0.5".to_string()),
            frames: Some(5),
            output_format: "mp4".to_string(),
            ..Default::default()
        };
        let out = apply_filters(&input, &options, &limits(), &rt).expect("apply filters");
        let file = out.primary.expect("primary output");
        assert_eq!(file.name, "output.mp4");
        assert!(!file.bytes.is_empty());
    }

    #[test]
    fn apply_filters_rejects_unknown_filter() {
        let rt = runtime();
        let input = sample_input();
        let options = FilterGraphOptions {
            video_filter: Some("hflip,no_such_filter_xyz".to_string()),
            output_format: "mp4".to_string(),
            ..Default::default()
        };
        let err = apply_filters(&input, &options, &limits(), &rt).expect_err("must fail");
        match err {
            MediaError::Unsupported(msg) => {
                assert!(msg.contains("no_such_filter_xyz"), "msg: {msg}");
                assert!(msg.contains("hflip") == false, "msg: {msg}");
            }
            other => panic!("expected Unsupported, got {other:?}"),
        }
    }

    #[test]
    fn apply_filters_rejects_injection_chars() {
        let rt = runtime();
        let input = sample_input();
        for bad in ["hflip;rm -rf /", "scale=32:32|cat", "eq=brightness=0.1&x"] {
            let options = FilterGraphOptions {
                video_filter: Some(bad.to_string()),
                output_format: "mp4".to_string(),
                ..Default::default()
            };
            let err =
                apply_filters(&input, &options, &limits(), &rt).expect_err("must reject {bad}");
            assert!(
                matches!(err, MediaError::InvalidInput(_)),
                "{bad}: expected InvalidInput, got {err:?}"
            );
        }
    }

    #[test]
    fn list_filters_returns_sorted_names() {
        let rt = runtime();
        let filters = list_filters(&rt).expect("list filters");
        assert!(filters.iter().any(|f| f == "scale"), "no scale filter");
        assert!(filters.iter().any(|f| f == "hflip"), "no hflip filter");
        let mut sorted = filters.clone();
        sorted.sort();
        assert_eq!(filters, sorted, "filters not sorted");
    }
}
