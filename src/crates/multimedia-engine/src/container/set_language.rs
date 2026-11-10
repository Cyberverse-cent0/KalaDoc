//! Set per-stream `language` tags via `-metadata:s:<type>:<n>`.
//!
//! [`set_language`] probes the input with `ffprobe` (`-show_streams`),
//! resolves each [`LanguageAssignment::stream_spec`] to concrete
//! streams, and remuxes with
//! `ffmpeg -i <in> -map 0 -c copy -metadata:s:<v|a|s>:<ordinal>
//! language=<tag> ... <out>` — no re-encode, only the tag changes.
//!
//! ## Stream spec syntax
//!
//! `stream_spec` is either `<type>:<index>` or a bare `<type>`:
//!
//! - `"v:0"`, `"a:1"`, `"s:0"` — the stream at *per-type* index
//!   `index` (0-based) among streams of that codec type in ffprobe
//!   order. This matches ffmpeg's own `a:1` stream-specifier
//!   semantics, so the spec passes straight through to
//!   `-metadata:s:<type>:<index>` after verification.
//! - `"v"`, `"a"`, `"s"` — **all** streams of that type. The
//!   assignment expands to one `-metadata` argument per matching
//!   stream (ffmpeg's bare `s:a` metadata specifier is not portable
//!   across muxers, so we expand explicitly).
//!
//! Types map to codec types as `v`→video, `a`→audio, `s`→subtitle.
//! An unknown type, a non-numeric index, or an index with no
//! corresponding stream is [`MediaError::InvalidInput`].
//!
//! When several assignments target the same stream the last one
//! wins (ffmpeg applies `-metadata` options in argv order).

use std::ffi::OsString;

use serde::{Deserialize, Serialize};

use crate::crates::multimedia_engine::container::select_streams::media_streams;
use crate::crates::multimedia_engine::error::{MediaError, MediaResult};
use crate::crates::multimedia_engine::model::{MediaLimits, MediaOutput};
use crate::crates::multimedia_engine::probe::types::MediaStream;
use crate::crates::multimedia_engine::runtime::discovery::Runtime;
use crate::crates::multimedia_engine::runtime::{process, tempdir::TempDir};

/// One language-tag assignment: which streams get which tag.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct LanguageAssignment {
    /// Stream specifier: `"v:0"` / `"a:1"` / `"s:0"` (per-type index)
    /// or a bare `"v"` / `"a"` / `"s"` for every stream of that type.
    /// See the module docs for the full grammar.
    pub stream_spec: String,
    /// ISO 639-2/T bibliographic 3-letter language tag
    /// (`"eng"`, `"fre"`, `"deu"`, ...). Matched case-insensitively
    /// and normalized to lowercase before being written.
    pub language: String,
}

/// Options for [`set_language`].
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct LanguageOptions {
    /// Tag assignments, applied in order. Must be non-empty.
    pub assignments: Vec<LanguageAssignment>,
    /// Output container extension without a leading dot
    /// (`"mp4"`, `"mkv"`, ...). `None` keeps the input's sniffed
    /// container — stream copy cannot freely change containers, so
    /// an explicit different extension may fail inside ffmpeg when a
    /// codec is incompatible with the new muxer.
    #[serde(default)]
    pub output_format: Option<String>,
}

/// Set stream language tags on `input` and remux (stream copy).
///
/// All streams are preserved (`-map 0`); chapters and container
/// metadata are kept (`-map_chapters 0`, `-map_metadata 0`).
///
/// # Errors
///
/// - [`MediaError::InvalidInput`] — empty input, empty assignments,
///   a malformed `stream_spec`, an out-of-range stream index, or a
///   language value that is not `^[a-zA-Z]{3}$`.
/// - [`MediaError::LimitExceeded`] — input exceeds
///   `max_input_bytes`, or the output exceeds `max_output_bytes`.
/// - [`MediaError::Unsupported`] — output muxer missing from this
///   FFmpeg build.
/// - [`MediaError::ToolFailed`] — ffmpeg rejected the remux.
pub fn set_language(
    input: &[u8],
    options: &LanguageOptions,
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
    if options.assignments.is_empty() {
        return Err(MediaError::InvalidInput(
            "at least one language assignment is required".to_string(),
        ));
    }

    // Validate and normalize every language up front so a bad tag
    // never reaches the ffmpeg argv.
    let assignments: Vec<(&LanguageAssignment, String)> = options
        .assignments
        .iter()
        .map(|a| normalize_language(&a.language).map(|lang| (a, lang)))
        .collect::<MediaResult<Vec<_>>>()?;

    // Capability gate before probing/spawning: stream copy needs no
    // encoder, only the output muxer.
    let caps = rt.capabilities()?;
    let output_ext = resolve_output_ext(input, options.output_format.as_deref())?;
    let muxer = muxer_name_for(&output_ext);
    if !caps.has_muxer(muxer) {
        return Err(MediaError::Unsupported(format!(
            "muxer '{muxer}' (for .{output_ext}) is not available in this FFmpeg build"
        )));
    }

    let dir = TempDir::new("set-language")?;
    let input_path = dir.write(&format!("input.{}", sniff_extension(input)), input)?;
    let streams = media_streams(&input_path, limits, rt)?;
    if streams.is_empty() {
        return Err(MediaError::InvalidInput(
            "input has no media streams".to_string(),
        ));
    }

    // Resolve every spec to (specifier, language) argv entries.
    let mut metadata_args: Vec<(String, String)> = Vec::new();
    for (a, language) in &assignments {
        for s in resolve_spec(&streams, &a.stream_spec)? {
            let specifier = stream_specifier(&streams, s)?;
            metadata_args.push((specifier, language.clone()));
        }
    }

    let output_name = format!("output.{output_ext}");
    let output_path = dir.file(&output_name);

    let mut argv: Vec<OsString> = vec![
        OsString::from("-hide_banner"),
        OsString::from("-loglevel"),
        OsString::from("error"),
        OsString::from("-nostdin"),
        OsString::from("-y"),
        OsString::from("-i"),
        input_path.into_os_string(),
        OsString::from("-map"),
        OsString::from("0"),
        OsString::from("-c"),
        OsString::from("copy"),
        OsString::from("-map_chapters"),
        OsString::from("0"),
        OsString::from("-map_metadata"),
        OsString::from("0"),
    ];
    for (specifier, language) in &metadata_args {
        argv.push(OsString::from(format!("-metadata:{specifier}")));
        argv.push(OsString::from(format!("language={language}")));
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
    if let Some(primary) = out.primary.as_mut() {
        primary.content_type = content_type_for(&output_ext);
    }
    Ok(out)
}

/// Parsed `stream_spec`: a codec type plus an optional per-type
/// index (`None` = every stream of that type).
struct StreamSpec {
    /// ffprobe `codec_type` this spec selects.
    codec_type: &'static str,
    /// Per-type 0-based index, or `None` for "all of this type".
    index: Option<u32>,
}

/// Parse `"v:0"` / `"a:1"` / `"s:0"` or bare `"v"` / `"a"` / `"s"`.
fn parse_stream_spec(spec: &str) -> MediaResult<StreamSpec> {
    let spec = spec.trim();
    let codec_type = match spec {
        "v" | "video" => Some("video"),
        "a" | "audio" => Some("audio"),
        "s" | "subtitle" | "sub" => Some("subtitle"),
        _ => None,
    };
    if let Some(codec_type) = codec_type {
        return Ok(StreamSpec {
            codec_type,
            index: None,
        });
    }
    // Maybe "<letter>:<index>" form.
    let Some((kind, idx)) = spec.split_once(':') else {
        return Err(MediaError::InvalidInput(format!(
            "invalid stream_spec {spec:?}: expected \"v:0\", \"a:1\", \"s:0\", \"v\", \"a\" or \"s\""
        )));
    };
    let codec_type = match kind.trim() {
        "v" | "video" => "video",
        "a" | "audio" => "audio",
        "s" | "subtitle" | "sub" => "subtitle",
        _ => {
            return Err(MediaError::InvalidInput(format!(
                "invalid stream_spec {spec:?}: unknown stream type {kind:?} (want v/a/s)"
            )));
        }
    };
    let idx = idx.trim().parse::<u32>().map_err(|_| {
        MediaError::InvalidInput(format!(
            "invalid stream_spec {spec:?}: index must be a non-negative integer"
        ))
    })?;
    Ok(StreamSpec {
        codec_type,
        index: Some(idx),
    })
}

/// Resolve a parsed [`StreamSpec`] against the probed streams.
///
/// `"a:1"` selects the *second* audio stream in ffprobe order
/// (per-type index, matching ffmpeg's `a:1` specifier). A bare
/// `"a"` selects every audio stream. Empty result is
/// [`MediaError::InvalidInput`].
fn resolve_spec<'a>(streams: &'a [MediaStream], spec: &str) -> MediaResult<Vec<&'a MediaStream>> {
    let parsed = parse_stream_spec(spec)?;
    let matches: Vec<&MediaStream> = streams
        .iter()
        .filter(|s| s.codec_type.as_deref() == Some(parsed.codec_type))
        .collect();
    match parsed.index {
        None => {
            if matches.is_empty() {
                return Err(MediaError::InvalidInput(format!(
                    "stream_spec {spec:?} matched no {} streams",
                    parsed.codec_type
                )));
            }
            Ok(matches)
        }
        Some(idx) => matches.get(idx as usize).map(|m| vec![*m]).ok_or_else(|| {
            MediaError::InvalidInput(format!(
                "stream_spec {spec:?} is out of range: input has {} {} stream(s)",
                matches.len(),
                parsed.codec_type
            ))
        }),
    }
}

/// Compute the `s:<letter>:<ordinal>` specifier for `target`: the
/// ordinal counts streams of the same codec type appearing before
/// `target` in ffprobe order — matching ffmpeg's per-type indexing
/// once `-map 0` preserves input order in the output.
fn stream_specifier(streams: &[MediaStream], target: &MediaStream) -> MediaResult<String> {
    let codec_type = target.codec_type.as_deref().unwrap_or("");
    let letter = match codec_type {
        "video" => "v",
        "audio" => "a",
        "subtitle" => "s",
        _ => {
            return Err(MediaError::InvalidInput(format!(
                "cannot set language on stream of codec_type {codec_type:?}"
            )));
        }
    };
    let target_idx = target.index.unwrap_or(u32::MAX);
    let mut ordinal = 0u32;
    for s in streams {
        if s.index == Some(target_idx) {
            break;
        }
        if s.codec_type.as_deref() == Some(codec_type) {
            ordinal += 1;
        }
    }
    Ok(format!("s:{letter}:{ordinal}"))
}

/// Language must be an ISO 639-2/T-style 3-letter tag
/// (`^[a-zA-Z]{3}$`); accept any case and normalize to lowercase.
fn normalize_language(lang: &str) -> MediaResult<String> {
    let trimmed = lang.trim();
    if trimmed.len() == 3 && trimmed.bytes().all(|b| b.is_ascii_alphabetic()) {
        return Ok(trimmed.to_ascii_lowercase());
    }
    Err(MediaError::InvalidInput(format!(
        "invalid language {lang:?}: expected a 3-letter ISO 639-2 tag like \"eng\" or \"deu\""
    )))
}

/// Resolve the output container extension (same rules as
/// [`select_streams`](super::select_streams)).
fn resolve_output_ext(input: &[u8], output_format: Option<&str>) -> MediaResult<String> {
    if let Some(fmt) = output_format {
        let fmt = fmt.trim().trim_start_matches('.').to_ascii_lowercase();
        if fmt.is_empty() || !fmt.chars().all(|c| c.is_ascii_alphanumeric()) {
            return Err(MediaError::InvalidInput(format!(
                "invalid output_format {fmt:?}"
            )));
        }
        return Ok(fmt);
    }
    Ok(sniff_container(input).to_string())
}

/// Map a file extension to the ffmpeg muxer name used for the
/// capability check (ffmpeg itself resolves the muxer from the
/// output filename; this only translates aliases like `.mkv` →
/// `matroska` for `has_muxer`).
fn muxer_name_for(ext: &str) -> &str {
    match ext {
        "mkv" => "matroska",
        "m4a" => "ipod",
        "ts" | "mts" | "m2ts" => "mpegts",
        "mpga" | "oga" => "ogg",
        other => other,
    }
}

/// Best-effort container guess from magic bytes, used to pick a
/// default output extension for the remux.
fn sniff_container(data: &[u8]) -> &'static str {
    if data.len() >= 12 && &data[4..8] == b"ftyp" {
        return if &data[8..12] == b"qt  " {
            "mov"
        } else {
            "mp4"
        };
    }
    if data.starts_with(&[0x1A, 0x45, 0xDF, 0xA3]) {
        return "mkv";
    }
    if data.starts_with(b"RIFF") && data.len() >= 12 {
        return match &data[8..12] {
            b"WAVE" => "wav",
            b"AVI " => "avi",
            _ => "mp4",
        };
    }
    if data.starts_with(b"OggS") {
        return "ogg";
    }
    if data.starts_with(b"fLaC") {
        return "flac";
    }
    if data.starts_with(b"ID3") || (data.len() >= 2 && data[0] == 0xFF && (data[1] & 0xE0) == 0xE0)
    {
        return "mp3";
    }
    "mp4"
}

/// Best-effort container extension for the temp *input* file —
/// ffprobe/ffmpeg re-probe content anyway; `.bin` fallback keeps the
/// demuxer guess content-driven.
fn sniff_extension(data: &[u8]) -> &'static str {
    if data.len() >= 12 && &data[4..8] == b"ftyp" {
        return if &data[8..12] == b"qt  " {
            "mov"
        } else {
            "mp4"
        };
    }
    if data.starts_with(&[0x1A, 0x45, 0xDF, 0xA3]) {
        return "mkv";
    }
    if data.starts_with(b"RIFF") && data.len() >= 12 {
        return match &data[8..12] {
            b"WAVE" => "wav",
            b"AVI " => "avi",
            _ => "bin",
        };
    }
    if data.starts_with(b"OggS") {
        return "ogg";
    }
    if data.starts_with(b"fLaC") {
        return "flac";
    }
    if data.starts_with(b"ID3") || (data.len() >= 2 && data[0] == 0xFF && (data[1] & 0xE0) == 0xE0)
    {
        return "mp3";
    }
    "bin"
}

/// Best-effort MIME type for the produced container.
fn content_type_for(ext: &str) -> Option<String> {
    let mime = match ext {
        "mp4" | "m4v" | "m4a" => "video/mp4",
        "mkv" => "video/x-matroska",
        "webm" => "video/webm",
        "mov" => "video/quicktime",
        "avi" => "video/x-msvideo",
        "mp3" => "audio/mpeg",
        "aac" | "adts" => "audio/aac",
        "wav" => "audio/wav",
        "ogg" | "oga" | "opus" => "audio/ogg",
        "flac" => "audio/flac",
        "ts" | "mts" | "m2ts" => "video/mp2t",
        _ => return None,
    };
    Some(mime.to_string())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::crates::multimedia_engine::runtime::discovery;

    fn runtime() -> Runtime {
        Runtime::discover().expect("ffmpeg/ffprobe must be on PATH")
    }

    fn limits() -> MediaLimits {
        MediaLimits::default()
    }

    /// Build a 1s mp4 with one video (64x64 @10fps) and one audio
    /// stream via lavfi. Returns the file bytes, or `None` when the
    /// needed encoders are unavailable — callers skip gracefully.
    fn make_av_mp4() -> Option<Vec<u8>> {
        let ffmpeg = discovery::find_tool("ffmpeg")?;
        let dir = TempDir::new("lang-test-in").ok()?;
        let out_path = dir.file("av.mp4");
        let argv = process::args(&[
            "-hide_banner",
            "-loglevel",
            "error",
            "-nostdin",
            "-y",
            "-f",
            "lavfi",
            "-i",
            "testsrc=size=64x64:rate=10:duration=1",
            "-f",
            "lavfi",
            "-i",
            "sine=frequency=440:duration=1",
            "-c:v",
            "mpeg4",
            "-pix_fmt",
            "yuv420p",
            "-c:a",
            "aac",
            "-shortest",
            out_path.to_str()?,
        ]);
        let out = process::run("ffmpeg", &ffmpeg, &argv, None, &limits()).ok()?;
        if !out.success {
            return None;
        }
        std::fs::read(&out_path).ok()
    }

    fn media_stream(index: u32, codec_type: &str) -> MediaStream {
        MediaStream {
            index: Some(index),
            codec_type: Some(codec_type.to_string()),
            codec_name: Some(format!("{codec_type}_codec")),
            ..Default::default()
        }
    }

    /// ffprobe-reported language of the first stream of `codec_type`,
    /// checking the top-level `language` field and the `language`
    /// stream tag (containers differ in which they populate).
    /// `None` when no such stream exists.
    fn probed_language(bytes: &[u8], codec_type: &str) -> Option<String> {
        let rt = runtime();
        let dir = TempDir::new("lang-verify").ok()?;
        let path = dir.write("out.bin", bytes).ok()?;
        let streams = media_streams(&path, &limits(), &rt).ok()?;
        let s = streams
            .iter()
            .find(|s| s.codec_type.as_deref() == Some(codec_type))?;
        Some(
            s.language
                .clone()
                .or_else(|| s.tags.get("language").cloned())
                .unwrap_or_default(),
        )
    }

    #[test]
    fn parse_stream_spec_forms() {
        let s = parse_stream_spec("a:1").unwrap();
        assert_eq!((s.codec_type, s.index), ("audio", Some(1)));
        let s = parse_stream_spec("v").unwrap();
        assert_eq!((s.codec_type, s.index), ("video", None));
        let s = parse_stream_spec(" s:0 ").unwrap();
        assert_eq!((s.codec_type, s.index), ("subtitle", Some(0)));
        // Long-form aliases also accepted.
        assert_eq!(parse_stream_spec("audio:0").unwrap().codec_type, "audio");
        // Bad specs.
        assert!(parse_stream_spec("x:0").is_err());
        assert!(parse_stream_spec("a:x").is_err());
        assert!(parse_stream_spec("a:0:1").is_err());
        assert!(parse_stream_spec("").is_err());
    }

    #[test]
    fn specifier_ordinals_are_per_type() {
        let streams = vec![
            media_stream(0, "video"),
            media_stream(1, "audio"),
            media_stream(2, "audio"),
            media_stream(3, "subtitle"),
        ];
        assert_eq!(stream_specifier(&streams, &streams[1]).unwrap(), "s:a:0");
        assert_eq!(stream_specifier(&streams, &streams[2]).unwrap(), "s:a:1");
        assert_eq!(stream_specifier(&streams, &streams[0]).unwrap(), "s:v:0");
        assert_eq!(stream_specifier(&streams, &streams[3]).unwrap(), "s:s:0");
    }

    #[test]
    fn resolve_spec_expands_bare_type_and_checks_range() {
        let streams = vec![
            media_stream(0, "video"),
            media_stream(1, "audio"),
            media_stream(2, "audio"),
        ];
        // Bare "a" → all audio streams.
        assert_eq!(resolve_spec(&streams, "a").unwrap().len(), 2);
        // "a:1" → second audio stream (absolute index 2).
        let m = resolve_spec(&streams, "a:1").unwrap();
        assert_eq!(m.len(), 1);
        assert_eq!(m[0].index, Some(2));
        // Out of range and missing type.
        assert!(resolve_spec(&streams, "a:2").is_err());
        assert!(resolve_spec(&streams, "s").is_err());
    }

    #[test]
    fn normalize_language_enforces_three_letters() {
        assert_eq!(normalize_language("eng").unwrap(), "eng");
        assert_eq!(normalize_language("DEU").unwrap(), "deu");
        assert_eq!(normalize_language(" Fre ").unwrap(), "fre");
        assert!(normalize_language("en").is_err());
        assert!(normalize_language("engl").is_err());
        assert!(normalize_language("e1g").is_err());
        assert!(normalize_language("").is_err());
    }

    #[test]
    fn set_language_tags_audio_stream() {
        let Some(mp4) = make_av_mp4() else {
            eprintln!("skipping: ffmpeg lavfi/mpeg4/aac unavailable");
            return;
        };
        let rt = runtime();
        let options = LanguageOptions {
            assignments: vec![LanguageAssignment {
                stream_spec: "a:0".to_string(),
                language: "deu".to_string(),
            }],
            output_format: None,
        };
        let out = set_language(&mp4, &options, &limits(), &rt).expect("set_language");
        let primary = out.primary.expect("primary output");
        assert_eq!(primary.name, "output.mp4");
        assert_eq!(primary.content_type.as_deref(), Some("video/mp4"));

        // Verify the tag landed on the audio stream of the output.
        // ffprobe exposes stream language either as the top-level
        // `language` field or as a `language` tag depending on the
        // container — check both.
        assert_eq!(
            probed_language(&primary.bytes, "audio").as_deref(),
            Some("deu")
        );
        // The video stream must be untouched.
        assert_ne!(
            probed_language(&primary.bytes, "video").as_deref(),
            Some("deu")
        );
    }

    #[test]
    fn set_language_bare_type_tags_all_audio() {
        let Some(mp4) = make_av_mp4() else {
            eprintln!("skipping: ffmpeg lavfi/mpeg4/aac unavailable");
            return;
        };
        let rt = runtime();
        let options = LanguageOptions {
            assignments: vec![LanguageAssignment {
                stream_spec: "a".to_string(),
                language: "FRE".to_string(), // case-insensitive → "fre"
            }],
            output_format: Some("mkv".to_string()),
        };
        let out = set_language(&mp4, &options, &limits(), &rt).expect("set_language");
        let primary = out.primary.expect("primary output");
        assert_eq!(primary.name, "output.mkv");
        assert_eq!(
            probed_language(&primary.bytes, "audio").as_deref(),
            Some("fre")
        );
    }

    #[test]
    fn set_language_rejects_bad_input() {
        let rt = runtime();
        let mk = |assignments: Vec<LanguageAssignment>| LanguageOptions {
            assignments,
            output_format: None,
        };

        // Empty buffer.
        let err = set_language(
            &[],
            &mk(vec![LanguageAssignment {
                stream_spec: "a".to_string(),
                language: "eng".to_string(),
            }]),
            &limits(),
            &rt,
        )
        .expect_err("must fail");
        assert!(matches!(err, MediaError::InvalidInput(_)), "{err:?}");

        // No assignments.
        let err = set_language(&[0u8; 16], &mk(vec![]), &limits(), &rt).expect_err("must fail");
        assert!(matches!(err, MediaError::InvalidInput(_)), "{err:?}");

        // Bad language tag — rejected before probing.
        let err = set_language(
            &[0u8; 16],
            &mk(vec![LanguageAssignment {
                stream_spec: "a".to_string(),
                language: "english".to_string(),
            }]),
            &limits(),
            &rt,
        )
        .expect_err("must fail");
        assert!(matches!(err, MediaError::InvalidInput(_)), "{err:?}");
    }

    #[test]
    fn set_language_rejects_unknown_stream_on_real_media() {
        let Some(mp4) = make_av_mp4() else {
            eprintln!("skipping: ffmpeg lavfi/mpeg4/aac unavailable");
            return;
        };
        let rt = runtime();
        let options = LanguageOptions {
            assignments: vec![LanguageAssignment {
                stream_spec: "a:5".to_string(), // only one audio stream exists
                language: "eng".to_string(),
            }],
            output_format: None,
        };
        let err = set_language(&mp4, &options, &limits(), &rt).expect_err("must fail");
        assert!(matches!(err, MediaError::InvalidInput(_)), "{err:?}");
    }
}
