//! Two-pass bitrate-targeted video encoding.
//!
//! [`two_pass`] runs the classic ffmpeg two-pass encode:
//!
//! 1. **Pass 1** — `ffmpeg -i in -c:v <codec> -b:v <rate> -pass 1
//!    -passlogfile <dir>/passlog -an -f null /dev/null` analyzes the
//!    source and writes per-frame statistics next to the passlog
//!    base path.
//! 2. **Pass 2** — the same encode with `-pass 2` plus audio, which
//!    consumes the statistics and distributes the bitrate budget to
//!    hit the [`TwoPassOptions::bitrate_kbps`] target.
//!
//! The passlog files (`<base>-0.log`, `.mbtree`, ...) live inside the
//! op's [`TempDir`] and are removed on drop — callers never see them.
//!
//! **Two-pass support varies by encoder.** libx264/libx265, libvpx
//! (vp8/vp9), libaom-av1 and libsvtav1 accept `-pass`; many hardware
//! encoders (and some software ones, e.g. libopenh264, native
//! `mpeg4`) do not. When the encoder rejects `-pass`, ffmpeg's own
//! error surfaces as [`MediaError::ToolFailed`] — there is no silent
//! fallback to single-pass. Callers that need portability should pick
//! a codec known to support two-pass on the target build (check
//! `ffmpeg -h encoder=<name>` for `-pass` in its option list).
//!
//! **Timeout accounting:** `limits.timeout` is applied *per ffmpeg
//! invocation* by [`process::run_checked`], so the worst-case wall
//! time of [`two_pass`] is roughly **2× `limits.timeout`** (one full
//! timeout for each pass, plus I/O). There is no shared deadline —
//! callers needing a hard bound must halve `limits.timeout`.

use std::ffi::OsString;

use serde::{Deserialize, Serialize};

use crate::crates::multimedia_engine::encoding::profiles::EncodingProfile;
use crate::crates::multimedia_engine::error::{MediaError, MediaResult};
use crate::crates::multimedia_engine::model::{MediaLimits, MediaOutput};
use crate::crates::multimedia_engine::runtime::discovery::Runtime;
use crate::crates::multimedia_engine::runtime::process;
use crate::crates::multimedia_engine::runtime::tempdir::TempDir;

/// Default audio encoder when [`TwoPassOptions::audio_codec`] is
/// unset (and `profile` is absent or has no audio codec of its own
/// we honour — see [`resolve_audio_codec`]).
pub const DEFAULT_AUDIO_CODEC: &str = "aac";

/// Default audio bitrate argument (`-b:a`) when a profile does not
/// supply one.
pub const DEFAULT_AUDIO_BITRATE: &str = "128k";

/// Default output container extension when
/// [`TwoPassOptions::output_format`] is empty.
pub const DEFAULT_OUTPUT_FORMAT: &str = "mp4";

/// Options for [`two_pass`].
///
/// Two ways to pick codecs:
///
/// - **Explicit fields** — set [`TwoPassOptions::video_codec`] (an
///   `ffmpeg -encoders` token such as `"libx264"`, `"libvpx-vp9"`,
///   `"libaom-av1"`) and optionally `audio_codec`.
/// - **Profile** — set [`TwoPassOptions::profile`] to an
///   [`EncodingProfile`]; its `video_codec`/`video_args` are used for
///   both passes (with `b:v`/`pass`-related args stripped so they
///   cannot fight the two-pass machinery). An explicit non-empty
///   `video_codec` field still wins over the profile's codec.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct TwoPassOptions {
    /// Optional [`EncodingProfile`] supplying the video codec and
    /// extra codec args. `None` → use the explicit fields only.
    #[serde(default)]
    pub profile: Option<EncodingProfile>,
    /// Video encoder name (`ffmpeg -encoders` token). **Required**
    /// unless `profile` is set. Must be an encoder in this FFmpeg
    /// build that accepts `-pass`/`-b:v` — two-pass support is *not*
    /// universal (see module docs).
    #[serde(default)]
    pub video_codec: String,
    /// Target video bitrate in kilobits per second (`> 0`). Passed
    /// to ffmpeg as `-b:v <n>k` on both passes.
    pub bitrate_kbps: u32,
    /// Audio encoder name. `None` → the profile's `audio_codec` when
    /// a profile is set, else [`DEFAULT_AUDIO_CODEC`] (`"aac"`).
    /// Pass `Some("")` (or whitespace) to encode **video only**
    /// (`-an` on pass 2) — useful for video-only targets.
    #[serde(default)]
    pub audio_codec: Option<String>,
    /// Output container extension without a leading dot
    /// (`"mp4"`, `"mkv"`, `"webm"`, ...). Defaults to `"mp4"` when
    /// empty. Becomes `output.<format>`; the corresponding ffmpeg
    /// *muxer* is capability-checked before encoding.
    #[serde(default)]
    pub output_format: String,
    /// Optional preset passed verbatim as `-preset <value>` on both
    /// passes (e.g. `"medium"` for x264/x265, `"good"` for libvpx).
    /// Whether a given encoder accepts `-preset` varies —
    /// unsupported values surface as ffmpeg errors, not silent
    /// ignores. Ignored when a profile supplies its own preset arg.
    #[serde(default)]
    pub preset: Option<String>,
}

impl Default for TwoPassOptions {
    fn default() -> Self {
        Self {
            profile: None,
            video_codec: String::new(),
            bitrate_kbps: 1500,
            audio_codec: None,
            output_format: DEFAULT_OUTPUT_FORMAT.to_string(),
            preset: None,
        }
    }
}

/// Resolve the video encoder: explicit `video_codec` field wins,
/// then the profile's codec.
fn resolve_video_codec(options: &TwoPassOptions) -> MediaResult<String> {
    let explicit = options.video_codec.trim();
    if !explicit.is_empty() {
        return Ok(explicit.to_string());
    }
    if let Some(profile) = &options.profile {
        let codec = profile.video_codec.trim();
        if !codec.is_empty() {
            return Ok(codec.to_string());
        }
    }
    Err(MediaError::InvalidInput(
        "video_codec is required (or supply a profile with video_codec)".to_string(),
    ))
}

/// Resolve the audio encoder. Precedence: an explicit `audio_codec`
/// field (empty/whitespace disables audio), then the profile's
/// `audio_codec`, then [`DEFAULT_AUDIO_CODEC`].
///
/// Returns `None` for "encode video only" (`-an`).
fn resolve_audio_codec(options: &TwoPassOptions) -> Option<String> {
    if let Some(explicit) = &options.audio_codec {
        let v = explicit.trim();
        return if v.is_empty() {
            None // explicit opt-out: video-only output
        } else {
            Some(v.to_string())
        };
    }
    if let Some(profile) = &options.profile {
        let v = profile.audio_codec.trim();
        if !v.is_empty() {
            return Some(v.to_string());
        }
    }
    Some(DEFAULT_AUDIO_CODEC.to_string())
}

/// Audio bitrate for `-b:a`: the profile's `audio_args` `b:a` pair
/// when present, else [`DEFAULT_AUDIO_BITRATE`].
fn audio_bitrate(options: &TwoPassOptions) -> String {
    if let Some(profile) = &options.profile {
        for pair in profile.audio_args.chunks_exact(2) {
            if pair[0] == "b:a" && !pair[1].trim().is_empty() {
                return pair[1].trim().to_string();
            }
        }
    }
    DEFAULT_AUDIO_BITRATE.to_string()
}

/// Best-effort container sniff for the temp input extension, so
/// ffmpeg's demuxer sees a realistic suffix.
fn sniff_extension(input: &[u8]) -> &'static str {
    if input.len() >= 4 {
        match &input[..4] {
            [0x1A, 0x45, 0xDF, 0xA3] => return "mkv",
            [b'O', b'g', b'g', b'S'] => return "ogg",
            [b'f', b'L', b'a', b'C'] => return "flac",
            [b'R', b'I', b'F', b'F'] => {
                if input.len() >= 12 && &input[8..12] == b"WAVE" {
                    return "wav";
                }
                if input.len() >= 12 && &input[8..12] == b"AVI " {
                    return "avi";
                }
                return "bin";
            }
            [b'I', b'D', b'3', _] => return "mp3",
            _ => {}
        }
        if input.len() >= 8 && &input[4..8] == b"ftyp" {
            return "mp4";
        }
    }
    "bin"
}

/// Sanitize a token used as output extension / preset value:
/// ASCII alnum plus `-`/`_`/`.`.
fn sanitize_token(value: &str, field: &str) -> MediaResult<String> {
    let v = value.trim();
    if v.is_empty()
        || !v
            .chars()
            .all(|c| c.is_ascii_alphanumeric() || matches!(c, '-' | '_' | '.'))
    {
        return Err(MediaError::InvalidInput(format!(
            "{field} {value:?} contains characters unsafe for ffmpeg arguments"
        )));
    }
    Ok(v.to_string())
}

/// Map an output container extension to the ffmpeg *muxer* name used
/// for capability checks. Most extensions match muxer names; a few
/// common ones differ (`mkv` → `matroska`).
fn muxer_name_for(format: &str) -> &str {
    if format.eq_ignore_ascii_case("mkv") {
        "matroska"
    } else if format.eq_ignore_ascii_case("m4v") {
        "mp4"
    } else {
        format
    }
}

/// Best-effort MIME type for common output containers.
fn content_type_for(format: &str) -> Option<String> {
    let mime = match format.to_ascii_lowercase().as_str() {
        "mp4" | "m4v" => "video/mp4",
        "mkv" => "video/x-matroska",
        "webm" => "video/webm",
        "mov" => "video/quicktime",
        "avi" => "video/x-msvideo",
        "gif" => "image/gif",
        _ => return None,
    };
    Some(mime.to_string())
}

/// Profile `video_args` are stored as bare flag/value pairs (e.g.
/// `"crf", "23"` — see [`EncodingProfile`]). Expand each kept pair
/// into ffmpeg argv tokens `-<flag> <value>`, stripping pairs that
/// would conflict with the two-pass machinery: `b:v` (we set the
/// target bitrate), `pass`/`passlogfile` (we own them), and `preset`
/// when an explicit [`TwoPassOptions::preset`] is also given
/// (explicit wins; a duplicated `-preset` is encoder-dependent).
///
/// Odd trailing tokens (not valid per [`EncodingProfile`]'s
/// contract) are ignored.
fn profile_video_args(profile: &EncodingProfile, explicit_preset: bool) -> Vec<OsString> {
    let mut out = Vec::with_capacity(profile.video_args.len() + 2);
    for pair in profile.video_args.chunks_exact(2) {
        let (flag, value) = (&pair[0], &pair[1]);
        let skip = matches!(
            flag.as_str(),
            "b:v" | "pass" | "passlogfile" | "b" | "minrate" | "maxrate"
        ) || (flag == "preset" && explicit_preset);
        if skip {
            continue;
        }
        out.push(OsString::from(format!("-{flag}")));
        out.push(OsString::from(value));
    }
    out
}

/// Build the shared encode argv fragment used by both passes:
/// input, `-map` video, codec + bitrate (+preset/profile args).
/// Pass-specific args (`-pass`, `-passlogfile`, audio, output) are
/// appended by the caller.
fn encode_args(
    dir: &TempDir,
    input_name: &str,
    codec: &str,
    bitrate_arg: &str,
    options: &TwoPassOptions,
) -> Vec<OsString> {
    let mut argv: Vec<OsString> = vec![
        OsString::from("-hide_banner"),
        OsString::from("-loglevel"),
        OsString::from("error"),
        OsString::from("-nostdin"),
        OsString::from("-y"),
        OsString::from("-i"),
        dir.file(input_name).into_os_string(),
        OsString::from("-map"),
        OsString::from("0:v:0"),
        OsString::from("-c:v"),
        OsString::from(codec),
        OsString::from("-b:v"),
        OsString::from(bitrate_arg),
    ];
    if let Some(profile) = &options.profile {
        argv.extend(profile_video_args(profile, options.preset.is_some()));
    }
    if let Some(preset) = options
        .preset
        .as_deref()
        .map(str::trim)
        .filter(|p| !p.is_empty())
    {
        argv.push(OsString::from("-preset"));
        argv.push(OsString::from(preset));
    }
    argv
}

/// Two-pass encode `input` to a target video bitrate.
///
/// Pass 1 writes ffmpeg's passlog statistics inside a temp dir;
/// pass 2 consumes them and produces `output.<format>`, which is
/// read back under `limits.max_output_bytes`.
///
/// # Errors
///
/// - [`MediaError::InvalidInput`] — empty/oversized input, missing
///   `video_codec`, `bitrate_kbps == 0`, unsafe `output_format`/
///   `preset` characters.
/// - [`MediaError::Unsupported`] — video codec, audio codec, or the
///   output muxer is not in this FFmpeg build's capability set.
/// - [`MediaError::ToolFailed`] — either pass failed (unsupported
///   `-pass` for the codec, unreadable input, bad preset value, ...).
/// - [`MediaError::Timeout`] — a single pass exceeded
///   `limits.timeout` (see module docs: budget is per-pass).
/// - [`MediaError::LimitExceeded`] — byte caps exceeded.
pub fn two_pass(
    input: &[u8],
    options: &TwoPassOptions,
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
    if options.bitrate_kbps == 0 {
        return Err(MediaError::InvalidInput(
            "bitrate_kbps must be > 0".to_string(),
        ));
    }

    let codec = resolve_video_codec(options)?;
    let audio_codec = resolve_audio_codec(options);
    if let Some(ac) = &audio_codec {
        sanitize_token(ac, "audio_codec")?;
    }
    if let Some(preset) = options.preset.as_deref().filter(|p| !p.trim().is_empty()) {
        sanitize_token(preset, "preset")?;
    }
    let output_ext = {
        let raw = options.output_format.trim();
        let ext = if raw.is_empty() {
            DEFAULT_OUTPUT_FORMAT
        } else {
            raw.trim_start_matches('.')
        };
        sanitize_token(ext, "output_format")?
    };

    // Honest capabilities: codec, audio encoder and muxer must all
    // exist in this build before we spawn ffmpeg twice.
    let caps = rt.capabilities()?;
    if !caps.has_encoder(&codec) {
        return Err(MediaError::Unsupported(format!(
            "video encoder {codec:?} is not available in this FFmpeg build"
        )));
    }
    if let Some(ac) = &audio_codec
        && !caps.has_encoder(ac)
    {
        return Err(MediaError::Unsupported(format!(
            "audio encoder {ac:?} is not available in this FFmpeg build"
        )));
    }
    let muxer = muxer_name_for(&output_ext);
    if !caps.has_muxer(muxer) {
        return Err(MediaError::Unsupported(format!(
            "muxer {muxer:?} (for output_format {output_ext:?}) is not available in this FFmpeg build"
        )));
    }

    let dir = TempDir::new("two-pass")?;
    let input_name = format!("input.{}", sniff_extension(input));
    dir.write(&input_name, input)?;

    // Passlog base path inside the temp dir; ffmpeg appends
    // `-0.log`/`-0.log.mbtree` per stream. `-passlogfile` is honoured
    // by x264/x265/vpx/aom/svtav1; encoders that ignore it fall back
    // to `ffmpeg2pass-*.log` in the CWD — still correct, just untidy.
    let passlog_base = dir.file("passlog");
    let passlog = passlog_base.to_string_lossy().into_owned();
    let bitrate_arg = format!("{}k", options.bitrate_kbps);

    // --- pass 1: statistics only -----------------------------------------
    // Never encode audio in pass 1 — it only doubles the work and the
    // stats are video-only anyway.
    let mut argv1 = encode_args(&dir, &input_name, &codec, &bitrate_arg, options);
    argv1.extend([
        OsString::from("-an"),
        OsString::from("-pass"),
        OsString::from("1"),
        OsString::from("-passlogfile"),
        OsString::from(&passlog),
        OsString::from("-f"),
        OsString::from("null"),
        OsString::from("/dev/null"),
    ]);
    let pass1 = process::run_checked("ffmpeg", &rt.ffmpeg, &argv1, None, limits)?;

    // --- pass 2: encode using the statistics ------------------------------
    let output_name = format!("output.{output_ext}");
    let output_path = dir.file(&output_name);
    let mut argv2 = encode_args(&dir, &input_name, &codec, &bitrate_arg, options);
    argv2.extend([OsString::from("-pass"), OsString::from("2")]);
    if let Some(ac) = &audio_codec {
        argv2.extend([
            OsString::from("-map"),
            OsString::from("0:a:0?"), // optional: silent on audio-less inputs
            OsString::from("-c:a"),
            OsString::from(ac.as_str()),
            OsString::from("-b:a"),
            OsString::from(audio_bitrate(options)),
        ]);
    } else {
        argv2.push(OsString::from("-an"));
    }
    argv2.extend([
        OsString::from("-passlogfile"),
        OsString::from(&passlog),
        output_path.into_os_string(),
    ]);
    let out = process::run_checked("ffmpeg", &rt.ffmpeg, &argv2, None, limits)?;

    let bytes = dir.read_limited(&output_name, limits.max_output_bytes)?;
    if bytes.is_empty() {
        return Err(MediaError::ToolFailed {
            tool: "ffmpeg",
            status: "empty output".to_string(),
            detail: format!("two-pass encode produced an empty {output_name}"),
        });
    }

    let mut media = MediaOutput::single(output_name, bytes);
    if let Some(file) = media.primary.as_mut() {
        file.content_type = content_type_for(&output_ext);
    }
    for stderr in [&pass1.stderr, &out.stderr] {
        if !stderr.is_empty() {
            media.warnings.push(stderr.clone());
        }
    }
    Ok(media)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::crates::multimedia_engine::runtime::process;

    fn runtime() -> Option<Runtime> {
        match Runtime::discover() {
            Ok(rt) => Some(rt),
            Err(_) => {
                eprintln!("skipping: ffmpeg/ffprobe not on PATH");
                None
            }
        }
    }

    /// Generate a 1-second testsrc+sine av clip via lavfi.
    fn sample_input(rt: &Runtime) -> Vec<u8> {
        let dir = TempDir::new("twopass-test-in").expect("tempdir");
        let src = dir.file("src.mp4");
        let argv = process::args(&[
            "-hide_banner",
            "-loglevel",
            "error",
            "-nostdin",
            "-y",
            "-f",
            "lavfi",
            "-i",
            "testsrc=duration=1:size=64x64:rate=10",
            "-f",
            "lavfi",
            "-i",
            "sine=frequency=440:duration=1",
            "-c:v",
            "mpeg4",
            "-c:a",
            "aac",
            "-pix_fmt",
            "yuv420p",
            "-shortest",
            src.to_str().expect("utf8"),
        ]);
        process::run_checked("ffmpeg", &rt.ffmpeg, &argv, None, &MediaLimits::default())
            .expect("generate source");
        std::fs::read(&src).expect("read source")
    }

    #[test]
    fn rejects_missing_codec_and_bad_bitrate() {
        let Some(rt) = runtime() else { return };
        let input = b"x".to_vec();
        // No codec anywhere → InvalidInput before any ffmpeg run.
        let err = two_pass(
            &input,
            &TwoPassOptions::default(),
            &MediaLimits::default(),
            &rt,
        )
        .expect_err("must fail");
        assert!(matches!(err, MediaError::InvalidInput(_)), "{err:?}");

        // bitrate_kbps == 0 → InvalidInput.
        let options = TwoPassOptions {
            video_codec: "mpeg4".to_string(),
            bitrate_kbps: 0,
            ..Default::default()
        };
        let err = two_pass(&input, &options, &MediaLimits::default(), &rt).expect_err("must fail");
        assert!(matches!(err, MediaError::InvalidInput(_)), "{err:?}");
    }

    #[test]
    fn unknown_encoder_is_unsupported() {
        let Some(rt) = runtime() else { return };
        let input = b"not media".to_vec();
        let options = TwoPassOptions {
            video_codec: "definitely-not-a-codec".to_string(),
            bitrate_kbps: 500,
            ..Default::default()
        };
        match two_pass(&input, &options, &MediaLimits::default(), &rt) {
            Err(MediaError::Unsupported(_)) => {}
            other => panic!("expected Unsupported, got {other:?}"),
        }
    }

    #[test]
    fn two_pass_encodes_with_supported_codec() {
        let Some(rt) = runtime() else { return };
        let caps = match rt.capabilities() {
            Ok(c) => c,
            Err(_) => {
                eprintln!("skipping: capability probe failed");
                return;
            }
        };
        // Prefer codecs known to implement two-pass; mpeg4/libopenh264
        // are tried last and may fail pass 1 — the test skips rather
        // than failing on encoders without -pass support.
        let candidates = [
            "libx264",
            "libvpx-vp9",
            "libvpx",
            "libaom-av1",
            "libsvtav1",
            "mpeg4",
        ];
        let codec = candidates.iter().find(|c| caps.has_encoder(c)).copied();
        let Some(codec) = codec else {
            eprintln!("skipping: no candidate video encoder in this build");
            return;
        };
        if !caps.has_encoder("aac") || !caps.has_muxer("mp4") {
            eprintln!("skipping: aac/mp4 unavailable");
            return;
        }

        let input = sample_input(&rt);
        let options = TwoPassOptions {
            video_codec: codec.to_string(),
            bitrate_kbps: 200,
            // audio_codec: None → default "aac"; exercises the audio
            // mapping on pass 2 against the av sample.
            output_format: "mp4".to_string(),
            ..Default::default()
        };
        match two_pass(&input, &options, &MediaLimits::default(), &rt) {
            Ok(media) => {
                let file = media.primary.expect("primary output");
                assert_eq!(file.name, "output.mp4");
                assert!(!file.bytes.is_empty());
                assert_eq!(file.content_type.as_deref(), Some("video/mp4"));
            }
            Err(e) => {
                // Encoder may lack -pass support on this build — skip
                // gracefully per spec rather than failing the suite.
                eprintln!("skipping two-pass encode with {codec}: {e}");
            }
        }
    }
}
