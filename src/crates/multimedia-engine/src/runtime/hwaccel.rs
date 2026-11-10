//! Hardware-acceleration discovery and probing.
//!
//! [`Runtime::capabilities`](crate::crates::multimedia_engine::runtime::discovery::Runtime::capabilities)
//! already lists the hwaccel *names* an FFmpeg build was compiled with
//! (`ffmpeg -hwaccels`). That list is necessary but not sufficient: a
//! name like `vaapi` or `nvenc` only works when the matching driver,
//! device node and codec support are actually present on the host.
//! This module adds the runtime checks on top of the static list:
//!
//! - [`available_hwaccels`] — re-runs `ffmpeg -hwaccels` and parses the
//!   advertised names (independent of the capability cache).
//! - [`test_hwaccel`] — attempts a real one-frame hardware decode of a
//!   given input with `-hwaccel <name>` and reports success plus the
//!   stderr tail for diagnostics.
//! - [`working_hwaccels`] — convenience: test every advertised name
//!   against one input and return the subset that decoded.
//!
//! ## What this does NOT check
//!
//! Encoding support is per-codec, not per-hwaccel. A hwaccel name
//! working for decode says nothing about whether, say,
//! `h264_nvenc` or `hevc_vaapi` exists in this build. Before choosing
//! an encoder, verify it via the capability registry
//! (`RuntimeCapabilities::has_encoder`) and — when in doubt — inspect
//! `ffmpeg -hide_banner -h encoder=<name>` for hardware-device hints.
//! [`encoder_mentions_hw`] wraps that last check.

use std::collections::BTreeSet;
use std::ffi::OsString;
use std::path::Path;
use std::time::Duration;

use crate::crates::multimedia_engine::error::MediaResult;
use crate::crates::multimedia_engine::model::MediaLimits;
use crate::crates::multimedia_engine::runtime::discovery::Runtime;
use crate::crates::multimedia_engine::runtime::process;

/// Upper bound for a single hwaccel probe invocation. One-frame
/// decodes are fast when they work; a missing device can otherwise
/// stall inside driver init, so probes get a much shorter leash than
/// `MediaLimits::default().timeout` (30 min).
const PROBE_TIMEOUT: Duration = Duration::from_secs(15);

/// Result of one [`test_hwaccel`] invocation.
#[derive(Debug, Clone)]
pub struct HardwareProbe {
    /// The hwaccel name that was tested (e.g. `"vaapi"`).
    pub hwaccel: String,
    /// Whether `ffmpeg -hwaccel <name> -i <input> -frames:v 1 -f null -`
    /// exited 0. Note this only proves the *decode* path initialized —
    /// FFmpeg may still have silently fallen back to software for
    /// individual frames; inspect `stderr_tail` for
    /// "software fallback" style messages when it matters.
    pub decode_ok: bool,
    /// Tail of the tool's stderr (already trimmed by
    /// [`process::run`]) — carries "No VAAPI-capable GPU found"-style
    /// diagnostics when `decode_ok` is false.
    pub stderr_tail: String,
}

/// Run `ffmpeg -hwaccels` and return the advertised accelerator names.
///
/// The listing goes to stdout on FFmpeg ≥ 5; older builds and error
/// paths may put it on stderr, so both are scanned. This is a fresh
/// probe — for the cached view use
/// `Runtime::capabilities()?.hwaccels` instead.
pub fn available_hwaccels(runtime: &Runtime) -> MediaResult<BTreeSet<String>> {
    let limits = MediaLimits {
        timeout: Duration::from_secs(30),
        ..MediaLimits::default()
    };
    let out = process::run(
        "ffmpeg",
        &runtime.ffmpeg,
        &[OsString::from("-hwaccels")],
        None,
        &limits,
    )?;
    // The name list is the payload; the huge version/configuration
    // banner goes to stderr. Prefer stdout, fall back to stderr.
    if !out.stdout.is_empty() {
        return Ok(parse_hwaccels(&String::from_utf8_lossy(&out.stdout)));
    }
    Ok(parse_hwaccels(&out.stderr))
}

/// Parse `ffmpeg -hwaccels` output into a set of accelerator names.
///
/// Real output looks like:
///
/// ```text
/// Hardware acceleration methods:
/// vdpau
/// cuda
/// vaapi
/// qsv
/// drm
/// vulkan
/// ```
///
/// When fed stderr (older FFmpeg, or a failure path) the version
/// banner precedes the list, so banner/config lines are filtered out
/// by [`is_hwaccel_name`]. Exposed for tests; callers should prefer
/// [`available_hwaccels`].
pub(crate) fn parse_hwaccels(text: &str) -> BTreeSet<String> {
    let mut names = BTreeSet::new();
    // Names only count after the "Hardware acceleration methods:"
    // header — bare lowercase words in the version banner must not
    // be mistaken for accelerators.
    let mut in_list = false;
    for line in text.lines() {
        let line = line.trim();
        if line.starts_with("Hardware acceleration methods") {
            in_list = true;
            continue;
        }
        if !in_list {
            continue;
        }
        if line.is_empty() {
            continue;
        }
        // Listing entries are bare names, but tolerate a flags column
        // (`H.... vaapi`) just in case a future build reuses the
        // `-encoders`-style table format.
        let candidate = line.split_whitespace().next().unwrap_or("");
        if is_hwaccel_name(candidate) {
            names.insert(candidate.to_string());
        }
    }
    names
}

/// Whether `token` looks like a real hwaccel name rather than banner
/// noise. Names are lowercase identifiers (`vaapi`, `videotoolbox`,
/// `dxva2`, `opencl`); this rejects digits-leading tokens, paths and
/// the `ffmpeg`/`libav*`/`configuration:` words that appear in the
/// version banner.
fn is_hwaccel_name(token: &str) -> bool {
    if token.is_empty() || token.len() > 32 {
        return false;
    }
    let mut chars = token.chars();
    match chars.next() {
        Some(c) if c.is_ascii_lowercase() => {}
        _ => return false,
    }
    if !chars.all(|c| c.is_ascii_lowercase() || c.is_ascii_digit() || c == '_') {
        return false;
    }
    !matches!(
        token,
        "ffmpeg"
            | "configuration"
            | "libavutil"
            | "libavcodec"
            | "libavformat"
            | "libavdevice"
            | "libavfilter"
            | "libswscale"
            | "libswresample"
            | "libpostproc"
            | "built"
            | "version"
    )
}

/// Test whether `hwaccel` can actually decode `input_path` on this
/// host: runs
///
/// ```text
/// ffmpeg -hide_banner -hwaccel <name> -i <input> -frames:v 1 -f null -
/// ```
///
/// One frame is enough to force decoder + hwaccel-device init; the
/// `-f null` muxer discards output so no files are produced.
///
/// The invocation timeout is clamped to [`PROBE_TIMEOUT`] regardless
/// of `limits.timeout` — a probe must never hang a worker for the
/// full op budget. Spawn/io failures still surface as
/// [`MediaError`](crate::crates::multimedia_engine::error::MediaError);
/// a non-zero exit is *not* an error here — it is reported via
/// [`HardwareProbe::decode_ok`] = false with the stderr tail, since
/// "this hwaccel doesn't work here" is a normal discovery result.
pub fn test_hwaccel(
    runtime: &Runtime,
    hwaccel: &str,
    input_path: &Path,
    limits: &MediaLimits,
) -> MediaResult<HardwareProbe> {
    let mut probe_limits = limits.clone();
    probe_limits.timeout = probe_limits.timeout.min(PROBE_TIMEOUT);

    let args: Vec<OsString> = [
        OsString::from("-hide_banner"),
        OsString::from("-hwaccel"),
        OsString::from(hwaccel),
        OsString::from("-i"),
        input_path.as_os_str().to_os_string(),
        OsString::from("-frames:v"),
        OsString::from("1"),
        OsString::from("-f"),
        OsString::from("null"),
        OsString::from("-"),
    ]
    .into_iter()
    .collect();

    let out = process::run("ffmpeg", &runtime.ffmpeg, &args, None, &probe_limits)?;
    Ok(HardwareProbe {
        hwaccel: hwaccel.to_string(),
        decode_ok: out.success,
        stderr_tail: out.stderr,
    })
}

/// Probe every name in `names` against `input_path`, returning the
/// subset whose one-frame hardware decode succeeded.
///
/// Each candidate costs one ffmpeg invocation (≤ [`PROBE_TIMEOUT`]);
/// call this once per input — hwaccel availability is host- and
/// codec-dependent, so results should be cached per `(input codec,
/// hwaccel)` pair by the caller rather than re-probed per frame.
pub fn working_hwaccels(
    runtime: &Runtime,
    names: &BTreeSet<String>,
    input_path: &Path,
    limits: &MediaLimits,
) -> MediaResult<BTreeSet<String>> {
    let mut ok = BTreeSet::new();
    for name in names {
        let probe = test_hwaccel(runtime, name, input_path, limits)?;
        if probe.decode_ok {
            ok.insert(name.clone());
        }
    }
    Ok(ok)
}

/// Heuristic check for hardware *encode* support of `encoder`.
///
/// Runs `ffmpeg -hide_banner -h encoder=<name>` and looks for
/// "hardware"/"hwaccel" markers in the help text. This is a hint, not
/// a guarantee: the authoritative check remains whether the encoder
/// name (e.g. `h264_nvenc`, `hevc_vaapi`) appears in
/// `RuntimeCapabilities::encoders`, because `-h encoder=` fails for
/// names this build doesn't have. Returns `Ok(false)` for unknown
/// encoders rather than an error.
pub fn encoder_mentions_hw(
    runtime: &Runtime,
    encoder: &str,
    limits: &MediaLimits,
) -> MediaResult<bool> {
    let args: Vec<OsString> = [
        OsString::from("-hide_banner"),
        OsString::from("-h"),
        OsString::from(format!("encoder={encoder}")),
    ]
    .into_iter()
    .collect();
    let mut probe_limits = limits.clone();
    probe_limits.timeout = probe_limits.timeout.min(Duration::from_secs(15));
    let out = process::run("ffmpeg", &runtime.ffmpeg, &args, None, &probe_limits)?;
    if !out.success {
        return Ok(false);
    }
    let text = String::from_utf8_lossy(&out.stdout).to_lowercase();
    let text_err = out.stderr.to_lowercase();
    Ok(text.contains("hwaccel")
        || text.contains("hardware")
        || text_err.contains("hwaccel")
        || text_err.contains("hardware"))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::crates::multimedia_engine::runtime::tempdir::TempDir;

    /// Real `ffmpeg -hwaccels` stdout (FFmpeg 7.x shape).
    const SAMPLE_STDOUT: &str =
        "Hardware acceleration methods:\nvdpau\ncuda\nvaapi\nqsv\ndrm\nvulkan\n";

    /// stderr of `ffmpeg -hwaccels`: version banner + config dump,
    /// then the same list at the end.
    const SAMPLE_STDERR: &str = "ffmpeg version 7.1.5 Copyright (c) 2000-2026 the FFmpeg developers\n  built with gcc 15 (GCC)\n  configuration: --enable-vaapi --enable-nvenc\n  libavutil      59. 39.100 / 59. 39.100\n  libavcodec     61. 19.101 / 61. 19.101\nHardware acceleration methods:\nvdpau\ncuda\nvaapi\nqsv\ndrm\nvulkan\n";

    #[test]
    fn parses_clean_stdout() {
        let names = parse_hwaccels(SAMPLE_STDOUT);
        assert_eq!(
            names.iter().cloned().collect::<Vec<_>>(),
            vec!["cuda", "drm", "qsv", "vaapi", "vdpau", "vulkan"]
        );
    }

    #[test]
    fn parses_banner_prefixed_stderr() {
        let names = parse_hwaccels(SAMPLE_STDERR);
        assert!(names.contains("vaapi"));
        assert!(names.contains("vulkan"));
        assert!(!names.contains("ffmpeg"));
        assert!(!names.contains("configuration"));
        assert!(!names.contains("libavutil"));
        assert_eq!(names.len(), 6);
    }

    #[test]
    fn rejects_banner_noise() {
        assert!(is_hwaccel_name("vaapi"));
        assert!(is_hwaccel_name("videotoolbox"));
        assert!(is_hwaccel_name("dxva2"));
        assert!(!is_hwaccel_name("ffmpeg"));
        assert!(!is_hwaccel_name("libavcodec"));
        assert!(!is_hwaccel_name("configuration:"));
        assert!(!is_hwaccel_name("--enable-vaapi"));
        assert!(!is_hwaccel_name("59."));
        assert!(!is_hwaccel_name(""));
    }

    #[test]
    fn empty_and_garbage_input_yields_empty_set() {
        assert!(parse_hwaccels("").is_empty());
        assert!(parse_hwaccels("not a listing\n").is_empty());
    }

    /// Live smoke test: requires ffmpeg/ffprobe on PATH. Skips
    /// gracefully when the tools are absent (CI without ffmpeg).
    #[test]
    fn available_hwaccels_live() {
        let runtime = match Runtime::discover() {
            Ok(rt) => rt,
            Err(_) => return,
        };
        let names = available_hwaccels(&runtime).expect("ffmpeg -hwaccels should parse");
        // Every FFmpeg build ships at least one hwaccel listing.
        assert!(!names.is_empty(), "expected at least one hwaccel name");
        // Names must match the capability registry's view.
        if let Ok(caps) = runtime.capabilities() {
            for name in &names {
                assert!(
                    caps.hwaccels.contains(name),
                    "hwaccel {name} missing from capability registry"
                );
            }
        }
    }

    /// End-to-end: generate a tiny lavfi clip, then probe `vaapi`.
    /// Asserts only the contract (returns a HardwareProbe), not that
    /// vaapi works — headless CI has no GPU. Skips without ffmpeg.
    #[test]
    fn test_hwaccel_on_lavfi_input() {
        let runtime = match Runtime::discover() {
            Ok(rt) => rt,
            Err(_) => return,
        };
        let tmp = TempDir::new("hwaccel-test").expect("tempdir");
        let input = tmp.file("clip.mp4");
        let limits = MediaLimits::default();

        // Generate ~6 frames of testsrc; mpeg4 encoder is in every
        // ffmpeg build's default set, mp4 muxer likewise.
        let gen_result = process::run(
            "ffmpeg",
            &runtime.ffmpeg,
            &[
                OsString::from("-hide_banner"),
                OsString::from("-y"),
                OsString::from("-f"),
                OsString::from("lavfi"),
                OsString::from("-i"),
                OsString::from("testsrc=duration=0.5:size=64x64:rate=12"),
                OsString::from("-frames:v"),
                OsString::from("6"),
                OsString::from("-c:v"),
                OsString::from("mpeg4"),
                input.as_os_str().to_os_string(),
            ],
            None,
            &limits,
        )
        .expect("spawn ffmpeg");
        if !gen_result.success {
            // Encoder/muxer missing in this build — nothing to probe.
            return;
        }

        let names = available_hwaccels(&runtime).expect("hwaccels");
        let probe = test_hwaccel(&runtime, "vaapi", &input, &limits)
            .expect("test_hwaccel should not error on tool failure");
        assert_eq!(probe.hwaccel, "vaapi");
        // Whether vaapi actually works is host-dependent; the contract
        // is only that we get a structured result either way.
        let _ = names;
    }
}
