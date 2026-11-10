//! Live-streaming support: URL validation, push-argument builders, and
//! capability reporting for this ffmpeg build.
//!
//! **Safety contract.** This module is deliberately *read-only* plus
//! *pure builders*: nothing here opens a socket, contacts a remote
//! endpoint, or executes an ffmpeg push. [`stream_args`] only returns
//! the argument vector a caller (or the job layer, after its own
//! policy checks) may hand to `ffmpeg`. Executing a push has real
//! side effects — network egress, credentials in the URL, unbounded
//! runtime — so it is left to the caller/job layer on purpose.
//!
//! Coverage notes (ffmpeg 7.1.5):
//!
//! - Pushing to a URL is just `ffmpeg -i input <codec args> <url>`;
//!   ffmpeg infers the muxer from the URL scheme, so [`stream_args`]
//!   deliberately does NOT emit `-f` — protocol and muxer names do
//!   not always coincide (e.g. all `rtmp*` protocols share the `rtmp`
//!   muxer family).
//! - [`stream_validate_url`] whitelists real streaming protocols and
//!   rejects whitespace/control characters plus shell/ffmpeg-option
//!   metacharacters, so a crafted URL cannot smuggle extra arguments
//!   or a different scheme past the whitelist.
//! - [`stream_targets`] and [`stream_capability_report`] enumerate
//!   what THIS build supports by running `ffmpeg -protocols` and
//!   `ffmpeg -muxers`; absent builds yield honest `MissingTool`/
//!   `ExternalTool` errors, never guessed lists.
//!
//! HLS/DASH *local* packaging already lives in [`super::packaging`];
//! this module does not duplicate it.

use std::path::Path;

use crate::crates::video_engine::error::{VideoError, VideoResult};
use crate::crates::video_engine::ops::capture::list_protocols;
use crate::crates::video_engine::ops::{FFMPEG, PROBE_TIMEOUT, find_tool, h264_encoder, run_tool};

/// Output protocols this engine will push to. Everything outside this
/// list — `file`, `pipe`, `crypto`, `concat`, ... — is rejected by
/// [`stream_validate_url`] even when the build supports it, because
/// pushing to them is either meaningless for "live" delivery or a
/// local-file side effect this module must not enable.
pub(crate) const STREAM_PROTOCOLS: &[&str] = &[
    // Real-Time Messaging Protocol family (all share the RTMP muxer).
    "rtmp",
    "rtmps",
    "rtmpt",
    "rtmpe",
    "rtmpte",
    "rtmpts",
    // RTP/RTSP realtime transport.
    "rtp",
    "rtsp",
    // Secure Reliable Transport.
    "srt",
    // Datagram transport for raw MPEG-TS fan-out.
    "udp",
    // HTTP-based ingestion (HLS-STYLE push endpoints, CMAF, ...).
    "http",
    "https",
    // Icecast/Shoutcast source protocol.
    "icecast",
];

/// Characters that must never appear in a stream URL: ASCII
/// whitespace/control (argument/log injection) plus the usual
/// shell/ffmpeg metacharacters. `?`, `&`, `=`, `%`, `:`, `/`, `.`,
/// `-`, `_`, `~`, `+`, `,`, `@` stay legal — they are normal URL
/// punctuation (query strings, ports, credentials).
fn url_has_dangerous_chars(url: &str) -> bool {
    url.chars().any(|c| {
        c.is_control()
            || c.is_whitespace()
            || matches!(
                c,
                '\'' | '"'
                    | '\\'
                    | ';'
                    | '|'
                    | '<'
                    | '>'
                    | '`'
                    | '$'
                    | '('
                    | ')'
                    | '{'
                    | '}'
                    | '['
                    | ']'
                    | '*'
                    | '!'
            )
    })
}

/// Validate a stream destination URL for [`stream_args`]: scheme must
/// be whitelisted ([`STREAM_PROTOCOLS`]), the URL must use the
/// `scheme://authority...` form with a non-empty authority, and no
/// dangerous characters may appear anywhere. Returns `()` on success
/// or [`VideoError::InvalidInput`] with a caller-facing reason.
pub(crate) fn stream_validate_url(url: &str) -> VideoResult<()> {
    if url.is_empty() {
        return Err(VideoError::InvalidInput(
            "stream URL is empty".to_string(),
        ));
    }
    if url_has_dangerous_chars(url) {
        return Err(VideoError::InvalidInput(format!(
            "stream URL contains disallowed characters: {url}"
        )));
    }
    let Some((scheme, rest)) = url.split_once("://") else {
        return Err(VideoError::InvalidInput(format!(
            "stream URL must use scheme:// form: {url}"
        )));
    };
    if scheme.is_empty()
        || !scheme
            .chars()
            .all(|c| c.is_ascii_alphanumeric() || matches!(c, '+' | '-' | '.'))
        || scheme.starts_with(|c: char| c.is_ascii_digit())
    {
        return Err(VideoError::InvalidInput(format!(
            "stream URL has an invalid scheme: {url}"
        )));
    }
    let scheme = scheme.to_ascii_lowercase();
    if !STREAM_PROTOCOLS.contains(&scheme.as_str()) {
        return Err(VideoError::InvalidInput(format!(
            "stream protocol '{scheme}' is not supported; allowed: {}",
            STREAM_PROTOCOLS.join(", ")
        )));
    }
    // `rest` is "authority/path?query" — the authority (up to the
    // first '/', '?' or '#') must be non-empty so we never push to a
    // scheme-relative or hostless URL.
    let authority_end = rest
        .find(|c| matches!(c, '/' | '?' | '#'))
        .unwrap_or(rest.len());
    if authority_end == 0 || rest[..authority_end].trim().is_empty() {
        return Err(VideoError::InvalidInput(format!(
            "stream URL has no host/authority: {url}"
        )));
    }
    Ok(())
}

/// Whether `name` is one of the whitelisted stream protocols.
fn is_stream_protocol(name: &str) -> bool {
    STREAM_PROTOCOLS.contains(&name.to_ascii_lowercase().as_str())
}

/// Video codec selection for [`stream_args`]. `Copy` is the default:
/// live push normally reuses the source's already-encoded streams.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum StreamCodec {
    /// `-c copy` — remux only, no re-encode. Fastest, requires the
    /// source codecs to be acceptable to the target muxer.
    Copy,
    /// Re-encode video to H.264 (best available encoder) + AAC audio
    /// — the most compatible ingest format.
    H264,
    /// Re-encode to VP9 + Opus — for WebRTC-ish/SRT pipelines that
    /// prefer modern codecs.
    Vp9,
}

impl Default for StreamCodec {
    fn default() -> Self {
        Self::Copy
    }
}

/// Tunables for [`stream_args`]. All fields optional except `codec`;
/// bitrate strings must match `[0-9]+(\.[0-9]+)?[kKmMgG]?`.
#[derive(Debug, Clone, Default)]
pub(crate) struct StreamPushOpts {
    /// Codec strategy; defaults to [`StreamCodec::Copy`].
    pub codec: Option<StreamCodec>,
    /// Video bitrate, e.g. `"2500k"`, `"4M"`. Emitted as `-b:v`.
    pub video_bitrate: Option<String>,
    /// Audio bitrate, e.g. `"128k"`. Emitted as `-b:a`.
    pub audio_bitrate: Option<String>,
    /// GOP size in frames (`-g`).
    pub gop_size: Option<u32>,
}

/// Whether `s` is a sane ffmpeg bitrate expression: digits with an
/// optional fraction and an optional k/M/G suffix. Rejects anything
/// that could be parsed as another option.
fn valid_bitrate(s: &str) -> bool {
    let s = s.trim();
    if s.is_empty() {
        return false;
    }
    let num = match s.chars().last() {
        Some(c) if matches!(c, 'k' | 'K' | 'm' | 'M' | 'g' | 'G') => &s[..s.len() - 1],
        _ => s,
    };
    !num.is_empty()
        && num
            .chars()
            .all(|c| c.is_ascii_digit() || c == '.')
        && num.chars().filter(|&c| c == '.').count() <= 1
        && num.chars().any(|c| c.is_ascii_digit())
}

/// Build the ffmpeg argument vector for pushing `input_path` to `url`.
///
/// This is a **pure function** — it validates inputs and returns argv;
/// it never executes anything. The returned vector is:
///
/// ```text
/// ffmpeg -hide_banner -y -v error -re -i <input> <codec args> <url>
/// ```
///
/// `-re` paces the read at native rate so the push behaves like a live
/// source rather than dumping the whole file as fast as the encoder
/// allows. No `-f` is emitted: ffmpeg infers the muxer from the URL
/// scheme. `url` is re-validated here (callers may pass untrusted
/// strings); failures are [`VideoError::InvalidInput`]. When `opts`
/// requests H.264 re-encode and this build has no H.264 encoder, the
/// result is the honest [`VideoError::ExternalTool`] from
/// [`h264_encoder`].
pub(crate) fn stream_args(
    input_path: &Path,
    url: &str,
    opts: &StreamPushOpts,
) -> VideoResult<Vec<String>> {
    stream_validate_url(url)?;
    let input = input_path.to_string_lossy().into_owned();
    if input.is_empty() {
        return Err(VideoError::InvalidInput(
            "stream input path is empty".to_string(),
        ));
    }

    let mut args = vec![
        "-hide_banner".to_string(),
        "-y".to_string(),
        "-v".to_string(),
        "error".to_string(),
        "-re".to_string(),
        "-i".to_string(),
        input,
    ];

    // Codec block (see StreamCodec docs).
    match opts.codec.unwrap_or_default() {
        StreamCodec::Copy => {
            args.extend(["-c".to_string(), "copy".to_string()]);
        }
        StreamCodec::H264 => {
            let enc = h264_encoder().ok_or_else(|| VideoError::ExternalTool {
                tool: FFMPEG.to_string(),
                detail: "H.264 stream push requires an H.264 encoder (libx264 or \
                         libopenh264); this ffmpeg build provides neither"
                    .to_string(),
            })?;
            args.extend([
                "-c:v".to_string(),
                enc.to_string(),
                "-pix_fmt".to_string(),
                "yuv420p".to_string(),
                "-c:a".to_string(),
                "aac".to_string(),
            ]);
        }
        StreamCodec::Vp9 => {
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
        }
    }

    if let Some(b) = &opts.video_bitrate {
        if !valid_bitrate(b) {
            return Err(VideoError::InvalidInput(format!(
                "invalid video bitrate '{b}': expected digits with optional k/M/G suffix"
            )));
        }
        args.extend(["-b:v".to_string(), b.clone()]);
    }
    if let Some(b) = &opts.audio_bitrate {
        if !valid_bitrate(b) {
            return Err(VideoError::InvalidInput(format!(
                "invalid audio bitrate '{b}': expected digits with optional k/M/G suffix"
            )));
        }
        args.extend(["-b:a".to_string(), b.clone()]);
    }
    if let Some(g) = opts.gop_size {
        if g == 0 || g > 1_000_000 {
            return Err(VideoError::InvalidInput(format!(
                "invalid gop_size {g}: expected 1..=1000000"
            )));
        }
        args.extend(["-g".to_string(), g.to_string()]);
    }

    args.push(url.to_string());
    Ok(args)
}

/// Stream output protocols this build actually supports: the
/// intersection of [`STREAM_PROTOCOLS`] with the protocols this
/// ffmpeg reports as output-capable (`ffmpeg -protocols`, via
/// [`list_protocols`]). Sorted, deduplicated, `Vec<String>` of
/// lowercase scheme names. Errors propagate honestly
/// (`MissingTool`/`ExternalTool`).
pub(crate) fn stream_targets() -> VideoResult<Vec<String>> {
    let protocols = list_protocols()?;
    let mut targets: Vec<String> = protocols
        .iter()
        .filter(|p| p.kind == "protocol")
        .filter(|p| is_stream_protocol(&p.name))
        .filter(|p| {
            p.description
                .as_deref()
                .map(|d| d.split(',').any(|part| part.trim() == "output"))
                .unwrap_or(false)
        })
        .map(|p| p.name.to_ascii_lowercase())
        .collect();
    targets.sort();
    targets.dedup();
    Ok(targets)
}

/// Per-protocol capability row for [`stream_capability_report`].
#[derive(Debug, Clone)]
pub(crate) struct StreamProtocolCap {
    /// Protocol/scheme name (lowercase), e.g. `"rtmp"`.
    pub name: String,
    /// Whether ffmpeg lists it under `Input:`.
    pub input: bool,
    /// Whether ffmpeg lists it under `Output:` — required for push.
    pub output: bool,
    /// Whether a muxer with the same name exists (`ffmpeg -muxers`).
    /// Not strictly required (ffmpeg infers muxers by protocol), but
    /// a same-named muxer is the common case and a useful sanity
    /// signal.
    pub muxer: bool,
}

/// What this ffmpeg build can actually push to: whitelisted protocols
/// with their direction support plus the muxer names present.
#[derive(Debug, Clone)]
pub(crate) struct StreamCapabilityReport {
    /// Whitelisted stream protocols found in `ffmpeg -protocols`,
    /// sorted by name.
    pub protocols: Vec<StreamProtocolCap>,
    /// All muxer names from `ffmpeg -muxers`, sorted.
    pub muxers: Vec<String>,
}

/// Parse `ffmpeg -muxers` stdout into sorted muxer names. Lines look
/// like `  E  hls   Apple HTTP Live Streaming`; the flags column may
/// be absent on older builds, so the first whitespace-separated
/// token is skipped only when it is a pure flag string.
fn parse_muxer_names(stdout: &str) -> Vec<String> {
    let mut names: Vec<String> = stdout
        .lines()
        .filter_map(|line| {
            let mut it = line.split_whitespace();
            let first = it.next()?;
            // Flag column: 1-2 chars of E/D/. — skip it and take name.
            let name = if first.chars().all(|c| matches!(c, 'E' | 'D' | '.'))
                && first.len() <= 2
            {
                it.next()?
            } else {
                first
            };
            // Muxer names are bare tokens; skip headers/titles.
            if name.is_empty() || name.contains(':') {
                None
            } else {
                Some(name.to_ascii_lowercase())
            }
        })
        .collect();
    names.sort();
    names.dedup();
    names
}

/// Enumerate muxer names via `ffmpeg -muxers`.
fn list_muxers() -> VideoResult<Vec<String>> {
    let program = find_tool(FFMPEG)?;
    let args = vec!["-hide_banner".to_string(), "-muxers".to_string()];
    let output = run_tool(FFMPEG, &program, &args, PROBE_TIMEOUT)?;
    if !output.status.success() {
        return Err(crate::crates::video_engine::ops::tool_failure(
            FFMPEG, &output,
        ));
    }
    Ok(parse_muxer_names(&String::from_utf8_lossy(&output.stdout)))
}

/// Snapshot of live-push capability for this build: whitelisted
/// protocols with input/output direction and same-named muxer
/// presence, plus the full muxer list. Read-only; safe to call from
/// health/capability probes.
pub(crate) fn stream_capability_report() -> VideoResult<StreamCapabilityReport> {
    let protocols = list_protocols()?;
    let muxers = list_muxers()?;
    let mut caps: Vec<StreamProtocolCap> = protocols
        .iter()
        .filter(|p| p.kind == "protocol" && is_stream_protocol(&p.name))
        .map(|p| {
            let (input, output) = p
                .description
                .as_deref()
                .map(|d| {
                    let parts: Vec<&str> =
                        d.split(',').map(str::trim).collect();
                    (
                        parts.contains(&"input"),
                        parts.contains(&"output"),
                    )
                })
                .unwrap_or((false, false));
            let name = p.name.to_ascii_lowercase();
            StreamProtocolCap {
                muxer: muxers.iter().any(|m| m == &name),
                name,
                input,
                output,
            }
        })
        .collect();
    caps.sort_by(|a, b| a.name.cmp(&b.name));
    Ok(StreamCapabilityReport {
        protocols: caps,
        muxers,
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::crates::video_engine::ops::tool_available;

    #[test]
    fn accepts_whitelisted_urls() {
        for url in [
            "rtmp://ingest.example.com/live/stream",
            "rtmps://ingest.example.com:443/app/key",
            "rtsp://camera.local:8554/live",
            "srt://host:9000?mode=caller",
            "udp://239.1.1.1:5004",
            "rtp://239.1.1.1:5004",
            "http://ingest.example.com/push",
            "https://ingest.example.com/push?token=abc123",
            "icecast://source.example.com:8000/mount",
        ] {
            assert!(
                stream_validate_url(url).is_ok(),
                "expected {url} to validate"
            );
        }
    }

    #[test]
    fn rejects_non_whitelisted_schemes() {
        for url in [
            "file:///etc/passwd",
            "pipe:1",
            "crypto://rtmp://host/app", // scheme itself not whitelisted
            "ftp://host/file",
            "gopher://host/",
            "rtmp;rm -rf /://host/app", // dangerous char AND bad scheme
        ] {
            let err = stream_validate_url(url).unwrap_err();
            assert!(
                matches!(err, VideoError::InvalidInput(_)),
                "expected InvalidInput for {url}, got {err:?}"
            );
        }
    }

    #[test]
    fn rejects_malformed_and_dangerous_urls() {
        for url in [
            "",
            "rtmp://",
            "rtmp:///nohost",
            "rtmp://host name/app",      // whitespace
            "rtmp://host/\"quoted\"",    // quote
            "rtmp://host/$(whoami)",     // command substitution
            "rtmp://host/`id`",
            "rtmp://host/a;b",
            "rtmp://host/a|b",
            "rtmp://host/a\\b",
            "1rtmp://host/app",          // scheme can't start with digit
            "rtmp:host/app",             // missing //
            "://host/app",
        ] {
            assert!(
                stream_validate_url(url).is_err(),
                "expected {url:?} to be rejected"
            );
        }
    }

    #[test]
    fn stream_args_copy_is_minimal() {
        let args = stream_args(
            Path::new("/tmp/in.mp4"),
            "rtmp://ingest.example.com/live/key",
            &StreamPushOpts::default(),
        )
        .unwrap();
        assert_eq!(
            args,
            vec![
                "-hide_banner",
                "-y",
                "-v",
                "error",
                "-re",
                "-i",
                "/tmp/in.mp4",
                "-c",
                "copy",
                "rtmp://ingest.example.com/live/key",
            ]
        );
    }

    #[test]
    fn stream_args_h264_uses_available_encoder() {
        let opts = StreamPushOpts {
            codec: Some(StreamCodec::H264),
            video_bitrate: Some("2500k".to_string()),
            audio_bitrate: Some("128k".to_string()),
            gop_size: Some(60),
        };
        let result = stream_args(Path::new("in.ts"), "srt://host:9000", &opts);
        match h264_encoder() {
            Some(enc) => {
                let args = result.expect("stream_args");
                let pos = |needle: &str| {
                    args.iter().position(|a| a == needle).unwrap()
                };
                assert_eq!(args[pos("-c:v") + 1], enc);
                assert_eq!(args[pos("-c:a") + 1], "aac");
                assert_eq!(args[pos("-b:v") + 1], "2500k");
                assert_eq!(args[pos("-b:a") + 1], "128k");
                assert_eq!(args[pos("-g") + 1], "60");
                assert_eq!(args.last().unwrap(), "srt://host:9000");
                // No -f: muxer is inferred from the URL scheme.
                assert!(!args.contains(&"-f".to_string()));
            }
            None => {
                // No H.264 encoder on this build: honest error.
                assert!(matches!(
                    result.unwrap_err(),
                    VideoError::ExternalTool { .. }
                ));
            }
        }
    }

    #[test]
    fn stream_args_vp9() {
        let opts = StreamPushOpts {
            codec: Some(StreamCodec::Vp9),
            ..StreamPushOpts::default()
        };
        let args = stream_args(
            Path::new("in.webm"),
            "udp://239.0.0.1:5004",
            &opts,
        )
        .unwrap();
        let joined = args.join(" ");
        assert!(joined.contains("libvpx-vp9"));
        assert!(joined.contains("libopus"));
    }

    #[test]
    fn stream_args_rejects_bad_url_and_bitrates() {
        let opts = StreamPushOpts::default();
        assert!(stream_args(Path::new("in"), "file:///tmp/x", &opts).is_err());
        for bad in ["abc", "12x34k", "-5k", "1.2.3M", ""] {
            let opts = StreamPushOpts {
                video_bitrate: Some(bad.to_string()),
                ..StreamPushOpts::default()
            };
            assert!(
                stream_args(Path::new("in"), "rtmp://h/a", &opts).is_err(),
                "bitrate {bad:?} should be rejected"
            );
        }
        for good in ["128k", "2500K", "4M", "1.5m", "800"] {
            let opts = StreamPushOpts {
                video_bitrate: Some(good.to_string()),
                ..StreamPushOpts::default()
            };
            assert!(
                stream_args(Path::new("in"), "rtmp://h/a", &opts).is_ok(),
                "bitrate {good:?} should be accepted"
            );
        }
        let opts = StreamPushOpts {
            gop_size: Some(0),
            ..StreamPushOpts::default()
        };
        assert!(stream_args(Path::new("in"), "rtmp://h/a", &opts).is_err());
    }

    #[test]
    fn protocol_filter_matches_whitelist() {
        assert!(is_stream_protocol("rtmp"));
        assert!(is_stream_protocol("RTMPS")); // case-insensitive
        assert!(is_stream_protocol("icecast"));
        assert!(!is_stream_protocol("file"));
        assert!(!is_stream_protocol("pipe"));
        assert!(!is_stream_protocol("hls")); // muxer name, not a push protocol
    }

    #[test]
    fn parse_muxer_names_handles_flag_column() {
        let stdout = "Muxers:\n  E  hls             Apple HTTP Live Streaming\n \
                      E  rtmp            RTMP output\nE dash            Dynamic Adaptive Streaming\n";
        let names = parse_muxer_names(stdout);
        assert!(names.contains(&"hls".to_string()));
        assert!(names.contains(&"rtmp".to_string()));
        assert!(names.contains(&"dash".to_string()));
        assert!(!names.iter().any(|n| n.contains(' ')));
    }

    #[test]
    fn live_capability_smoke() {
        if !tool_available(FFMPEG) {
            return; // ffmpeg absent: tool-dependent paths can't run.
        }
        let targets = stream_targets().expect("stream_targets");
        // Every returned target is whitelisted and output-capable.
        assert!(targets.iter().all(|t| is_stream_protocol(t)));
        for t in &targets {
            assert!(STREAM_PROTOCOLS.contains(&t.as_str()));
        }

        let report = stream_capability_report().expect("stream_capability_report");
        assert!(!report.muxers.is_empty());
        for cap in &report.protocols {
            assert!(is_stream_protocol(&cap.name));
            assert!(cap.input || cap.output);
        }
        // On a normal build rtmp/udp/rtp output should be present.
        assert!(
            report
                .protocols
                .iter()
                .any(|p| p.output && p.name == "rtmp"),
            "expected rtmp output protocol, got {:?}",
            report.protocols
        );
    }
}
