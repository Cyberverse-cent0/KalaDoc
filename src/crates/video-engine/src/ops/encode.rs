//! Profile-driven encoding: map an [`EncodeProfile`] to ffmpeg
//! output arguments and run a transcode with them.
//!
//! Covers professional/intermediate codecs (ProRes, DNxHD/HR, FFV1,
//! Huffyuv, MJPEG, MPEG-2, Theora) plus the general quality knobs
//! (CRF, preset, bitrate, pix_fmt).
//!
//! Every encoder named by a preset or profile is verified against the
//! host ffmpeg's `-encoders` list *before* ffmpeg runs — a missing
//! encoder yields [`VideoError::ExternalTool`], never a doomed
//! invocation. Presets whose preferred encoders are absent are
//! skipped entirely so [`preset_profiles`] only advertises profiles
//! this build can actually honor.

use std::collections::BTreeMap;

use crate::crates::core::engine::EngineLimits;
use crate::crates::video_engine::error::{VideoError, VideoResult};
use crate::crates::video_engine::model::EncodeProfile;
use crate::crates::video_engine::ops::probe::probe;
use crate::crates::video_engine::ops::{
    COMMAND_TIMEOUT, FFMPEG, TempDir, check_input, check_limits, checked_ext, encoder_available,
    ext_for_format, failure_detail, find_tool, h264_encoder, read_output, run_tool, tool_failure,
    write_input,
};

/// Container extensions [`transcode_profile`] accepts. Each maps to an
/// explicit ffmpeg muxer (see [`muxer_for`]) so extension guessing
/// never picks an ambiguous demuxer/muxer pair.
pub(crate) const ENCODE_TARGETS: &[&str] = &[
    "mp4", "mov", "mkv", "webm", "avi", "ts", "flv", "mpg", "mpeg", "wav", "mp3", "flac", "ogg",
    "opus", "m4a",
];

/// Video encoders that accept `-crf`. `libopenh264` is deliberately
/// absent: it has no CRF option (quality is `rc_mode`/`qp`-based), so
/// emitting `-crf` for it would fail mid-run. `libaom-av1`,
/// `libsvtav1`, and `librav1e` are included alongside the spec set —
/// they are genuinely CRF-capable.
const CRF_CAPABLE: &[&str] = &[
    "libx264",
    "libx265",
    "libvpx-vp9",
    "mpeg4",
    "h264",
    "vp9",
    "av1",
    "libaom-av1",
    "libsvtav1",
    "librav1e",
];

/// Video encoders that accept `-preset`. `libsvtav1` is included with
/// the spec set (`libx264`, `libx265`, `libvpx-vp9`, `libaom-av1`) —
/// it takes `-preset` as a speed/quality integer.
const PRESET_CAPABLE: &[&str] = &[
    "libx264",
    "libx265",
    "libvpx-vp9",
    "libaom-av1",
    "libsvtav1",
];

/// ffmpeg muxer name for a validated `target_ext`. ffmpeg's extension
/// guessing is ambiguous for several of these (`.m4a` could pick
/// `mp4`, `.ts` must be `mpegts`, `.mpg`/`.mpeg` share `mpeg`), so we
/// always pass `-f` explicitly. Verified against `ffmpeg -muxers`.
fn muxer_for(ext: &str) -> &str {
    match ext {
        "mkv" => "matroska",
        "ts" => "mpegts",
        "mpg" | "mpeg" => "mpeg",
        "m4a" => "ipod",
        // `opus` is the Ogg-Opus muxer; `ogg`/`flac`/`mp3`/`wav`/`avi`/
        // `webm`/`mov`/`mp4`/`flv` muxer names match the extension.
        other => other,
    }
}

/// Build one [`EncodeProfile`] shorthand. Keeps call sites terse.
fn profile(
    vcodec: &str,
    acodec: &str,
    crf: Option<u32>,
    preset: Option<&str>,
    pix_fmt: Option<&str>,
) -> EncodeProfile {
    EncodeProfile {
        vcodec: vcodec.to_string(),
        acodec: acodec.to_string(),
        crf,
        preset: preset.map(str::to_string),
        video_bitrate: None,
        audio_bitrate: None,
        pix_fmt: pix_fmt.map(str::to_string),
        extra_args: Vec::new(),
    }
}

/// Named presets callers can select instead of hand-building an
/// [`EncodeProfile`]. Keys are stable identifiers surfaced through
/// the CLI/API.
///
/// Each preset is included only when every encoder it needs exists in
/// this ffmpeg build (verified via [`encoder_available`]). Skipped on
/// this build (ffmpeg 7.1.5 without libx264/libx265):
///
/// - `hevc` — needs `libx265`, which is absent.
///
/// `h264` resolves through [`h264_encoder`] (`libx264` preferred,
/// `libopenh264` fallback), so its `vcodec` is always a real encoder
/// name — on this build that is `libopenh264`, which has no `-crf`
/// option, so the CRF knob is dropped by [`encode_args`] rather than
/// failing.
pub(crate) fn preset_profiles() -> BTreeMap<&'static str, EncodeProfile> {
    let mut presets = BTreeMap::new();

    // H.264/AVC + AAC — best available H.264 encoder, CRF 23.
    if let Some(enc) = h264_encoder() {
        presets.insert("h264", profile(enc, "aac", Some(23), Some("medium"), None));
    }

    // HEVC + AAC — libx265 only; absent on this build → skipped.
    // if encoder_available("libx265") {
    //     presets.insert("hevc", profile("libx265", "aac", Some(28), Some("medium"), None));
    // }

    // AV1 + Opus — prefer libaom-av1, then SVT-AV1, then libsvtav1.
    for cand in ["libaom-av1", "libsvtav1", "librav1e"] {
        if encoder_available(cand) {
            // libaom-av1 preset scale is 0 (best) .. 8 (fastest);
            // libsvtav1/librav1e interpret it as a speed knob — both
            // are valid `-preset` values, just different ranges.
            presets.insert("av1", profile(cand, "libopus", Some(30), Some("6"), None));
            break;
        }
    }

    // VP9 + Opus (WebM delivery).
    if encoder_available("libvpx-vp9") && encoder_available("libopus") {
        presets.insert(
            "vp9",
            profile("libvpx-vp9", "libopus", Some(32), None, None),
        );
    }

    // ProRes 422 HQ + PCM into .mov (mezzanine/intermediate).
    if encoder_available("prores_ks") && encoder_available("pcm_s16le") {
        presets.insert(
            "prores",
            profile("prores_ks", "pcm_s16le", None, None, None),
        );
    }

    // DNxHD + PCM into .mov (broadcast intermediate).
    if encoder_available("dnxhd") && encoder_available("pcm_s16le") {
        presets.insert(
            "dnxhd",
            profile("dnxhd", "pcm_s16le", None, None, Some("yuv422p")),
        );
    }

    // FFV1 + FLAC into .mkv (lossless archival).
    if encoder_available("ffv1") && encoder_available("flac") {
        presets.insert("ffv1", profile("ffv1", "flac", None, None, None));
    }

    // MPEG-2 video + MP2 audio (broadcast/DVB).
    if encoder_available("mpeg2video") && encoder_available("mp2") {
        presets.insert("mpeg2", profile("mpeg2video", "mp2", None, None, None));
    }

    // Motion JPEG (AVI/legacy capture).
    if encoder_available("mjpeg") && encoder_available("mp2") {
        presets.insert("mjpeg", profile("mjpeg", "mp2", None, None, None));
    }

    presets
}

/// Resolve a profile `vcodec` to a concrete ffmpeg encoder name.
/// `"h264"`/`"avc"` (any case) are aliases resolved via
/// [`h264_encoder`]; anything else is used verbatim.
fn resolve_vcodec(vcodec: &str) -> VideoResult<String> {
    if vcodec.eq_ignore_ascii_case("h264") || vcodec.eq_ignore_ascii_case("avc") {
        return h264_encoder()
            .map(str::to_string)
            .ok_or_else(|| VideoError::ExternalTool {
                tool: FFMPEG.to_string(),
                detail: "h264 output requires an H.264 encoder (libx264 or \
                         libopenh264); this ffmpeg build provides neither"
                    .to_string(),
            });
    }
    Ok(vcodec.to_string())
}

/// Convert an [`EncodeProfile`] into the ffmpeg argument tail placed
/// before the output path. Verifies codec/encoder availability via
/// [`crate::crates::video_engine::ops::encoder_available`] and returns
/// [`VideoError::ExternalTool`] when a required encoder is missing —
/// never emits an invocation that is guaranteed to fail.
///
/// Emission rules:
///
/// - `-c:v` only when `vcodec` is non-empty and not `"none"`; `"none"`
///   (or empty) emits `-vn` so audio-only inputs still work.
/// - `-crf` only for encoders in [`CRF_CAPABLE`]; `-preset` only for
///   [`PRESET_CAPABLE`] — silently skipped otherwise (e.g.
///   `libopenh264` has no CRF).
/// - `-c:a` when `acodec` is set; `-an` when it is empty or `"none"`.
/// - `-b:v`/`-b:a`/`-pix_fmt` when `Some`.
/// - `extra_args` are appended LAST, verbatim — they are a trusted-only
///   escape hatch; callers must never pass untrusted input here.
pub(crate) fn encode_args(profile: &EncodeProfile) -> VideoResult<Vec<String>> {
    let mut args = Vec::new();

    let vc = profile.vcodec.trim();
    let has_video = !(vc.is_empty() || vc.eq_ignore_ascii_case("none"));
    if has_video {
        let enc = resolve_vcodec(vc)?;
        if !encoder_available(&enc) {
            return Err(VideoError::ExternalTool {
                tool: FFMPEG.to_string(),
                detail: format!("video encoder '{enc}' is not available in this ffmpeg build"),
            });
        }
        args.extend(["-c:v".to_string(), enc.clone()]);

        if let Some(crf) = profile.crf {
            if CRF_CAPABLE.contains(&enc.as_str()) {
                args.extend(["-crf".to_string(), crf.to_string()]);
            }
        }
        if let Some(preset) = &profile.preset {
            if PRESET_CAPABLE.contains(&enc.as_str()) {
                args.extend(["-preset".to_string(), preset.clone()]);
            }
        }
        if let Some(bitrate) = &profile.video_bitrate {
            args.extend(["-b:v".to_string(), bitrate.clone()]);
        }
        if let Some(pix_fmt) = &profile.pix_fmt {
            args.extend(["-pix_fmt".to_string(), pix_fmt.clone()]);
        }
    } else {
        args.push("-vn".to_string());
    }

    let ac = profile.acodec.trim();
    let has_audio = !(ac.is_empty() || ac.eq_ignore_ascii_case("none"));
    if has_audio {
        if !encoder_available(ac) {
            return Err(VideoError::ExternalTool {
                tool: FFMPEG.to_string(),
                detail: format!("audio encoder '{ac}' is not available in this ffmpeg build"),
            });
        }
        args.extend(["-c:a".to_string(), ac.to_string()]);
        if let Some(bitrate) = &profile.audio_bitrate {
            args.extend(["-b:a".to_string(), bitrate.clone()]);
        }
    } else {
        args.push("-an".to_string());
    }

    // Trusted-only escape hatch: appended verbatim, last, so callers
    // can override anything above (e.g. tune/x264-params).
    args.extend(profile.extra_args.iter().cloned());
    Ok(args)
}

/// Transcode `input` to `target_ext` using `profile`'s codec and
/// quality settings. `target_ext` selects the container/muxer; the
/// profile selects codecs. Stream-copy is never assumed — the caller
/// gets an honest error when the profile cannot be honored.
///
/// Audio-only inputs (no video stream) skip the video codec path
/// entirely (`-vn`) and encode the audio track per `profile.acodec`.
pub(crate) fn transcode_profile(
    input: &[u8],
    profile: &EncodeProfile,
    target_ext: &str,
    limits: &EngineLimits,
) -> VideoResult<Vec<u8>> {
    let input_len = check_input(input)?;
    let ext = checked_ext(target_ext, ENCODE_TARGETS, "transcode_profile")?;
    let program = find_tool(FFMPEG)?;

    // Probe once: validates media, enforces limits, and supplies the
    // probed `format_name` used to name the scratch input file so
    // ffmpeg's demuxer probing starts from a sensible extension.
    let info = probe(input)?;
    check_limits(&info, input_len, limits, false)?;

    let dir = TempDir::new("encode-profile")?;
    let input_ext = info
        .format_name
        .as_deref()
        .and_then(ext_for_format)
        .unwrap_or("bin");
    let input_path = write_input(&dir, &format!("input.{input_ext}"), input)?;
    let out_path = dir.file(&format!("out.{ext}"));

    // Audio-only input: drop the video codec path so ffmpeg does not
    // demand a `-c:v` encoder for a stream that does not exist.
    let effective = if info.has_video {
        profile.clone()
    } else {
        let mut p = profile.clone();
        p.vcodec = "none".to_string();
        p
    };
    let codec_args = encode_args(&effective)?;

    let mut args = vec![
        "-y".to_string(),
        "-v".to_string(),
        "error".to_string(),
        "-i".to_string(),
        input_path.to_string_lossy().into_owned(),
        // Deterministic stream selection: at most the first video and
        // first audio stream; `?` makes each optional so audio-only
        // inputs still map cleanly.
        "-map".to_string(),
        "0:v:0?".to_string(),
        "-map".to_string(),
        "0:a:0?".to_string(),
    ];
    args.extend(codec_args);
    // Explicit muxer: extension alone is ambiguous for m4a/ts/mpg.
    args.extend(["-f".to_string(), muxer_for(&ext).to_string()]);
    // Faststart for ISO-BMFF outputs (same treatment as remux).
    if matches!(ext.as_str(), "mp4" | "mov" | "m4a") {
        args.extend(["-movflags".to_string(), "+faststart".to_string()]);
    }
    args.push(out_path.to_string_lossy().into_owned());

    let output = run_tool(FFMPEG, &program, &args, COMMAND_TIMEOUT)?;
    if !output.status.success() {
        let detail = failure_detail(&output, "ffmpeg exited unsuccessfully");
        // Codec/container mismatches (e.g. h264 into webm) are honest
        // input errors, not tool faults — surface ffmpeg's own muxer
        // rejection when it says so.
        let lower = detail.to_lowercase();
        if lower.contains("could not find tag")
            || lower.contains("muxer")
            || lower.contains("codec")
            || lower.contains("could not write header")
            || lower.contains("invalid argument")
        {
            return Err(VideoError::InvalidInput(format!(
                "cannot encode into '{ext}': {detail}"
            )));
        }
        return Err(tool_failure(FFMPEG, &output));
    }
    read_output(&out_path)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::crates::video_engine::ops::{FFPROBE, find_tool};
    use std::fs;

    fn tools_available() -> bool {
        find_tool(FFMPEG).is_ok() && find_tool(FFPROBE).is_ok()
    }

    /// 1s testsrc+sine MP4 via lavfi, encoded with the best available
    /// H.264 encoder. `None` when ffmpeg or an H.264 encoder is
    /// unavailable.
    fn make_fixture() -> Option<Vec<u8>> {
        let program = find_tool(FFMPEG).ok()?;
        let enc = h264_encoder()?;
        let dir = TempDir::new("encode-test").ok()?;
        let out = dir.file("fixture.mp4");
        let args = vec![
            "-y".to_string(),
            "-v".to_string(),
            "error".to_string(),
            "-f".to_string(),
            "lavfi".to_string(),
            "-i".to_string(),
            "testsrc=duration=1:size=160x120:rate=10".to_string(),
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

    #[test]
    fn preset_profiles_non_empty_and_honest() {
        if !tools_available() {
            return;
        }
        let presets = preset_profiles();
        assert!(!presets.is_empty(), "expected at least one preset");
        // BTreeMap keys are unique by construction; still assert every
        // advertised vcodec is a real encoder on this build.
        for (name, p) in &presets {
            assert!(
                encoder_available(&p.vcodec),
                "preset '{name}' vcodec '{}' is not an available encoder",
                p.vcodec
            );
        }
    }

    #[test]
    fn encode_args_h264_alias_resolves() {
        if !tools_available() {
            return;
        }
        let Some(enc) = h264_encoder() else {
            return;
        };
        let p = profile("h264", "aac", Some(23), None, None);
        let args = encode_args(&p).unwrap();
        let pos = args.iter().position(|a| a == "-c:v").unwrap();
        assert_eq!(args[pos + 1], enc);
        let apos = args.iter().position(|a| a == "-c:a").unwrap();
        assert_eq!(args[apos + 1], "aac");
        // libopenh264 has no -crf: must NOT be emitted for it.
        if enc != "libx264" {
            assert!(!args.iter().any(|a| a == "-crf"));
        }
    }

    #[test]
    fn encode_args_none_codecs_and_extra_args() {
        if !tools_available() {
            return;
        }
        let mut p = profile("none", "none", Some(10), None, None);
        p.extra_args = vec!["-t".to_string(), "5".to_string()];
        let args = encode_args(&p).unwrap();
        assert!(args.contains(&"-vn".to_string()));
        assert!(args.contains(&"-an".to_string()));
        assert!(!args.iter().any(|a| a == "-c:v" || a == "-c:a"));
        // extra_args land last, verbatim.
        assert_eq!(
            &args[args.len() - 2..],
            &["-t".to_string(), "5".to_string()]
        );
    }

    #[test]
    fn encode_args_rejects_missing_encoder() {
        if !tools_available() {
            return;
        }
        let p = profile("definitely_not_an_encoder", "aac", None, None, None);
        assert!(matches!(
            encode_args(&p),
            Err(VideoError::ExternalTool { .. })
        ));
    }

    #[test]
    fn transcode_profile_ffv1_to_mkv() {
        if !tools_available() || !encoder_available("ffv1") || !encoder_available("flac") {
            return;
        }
        let Some(fixture) = make_fixture() else {
            return;
        };
        let presets = preset_profiles();
        let Some(p) = presets.get("ffv1") else {
            return;
        };
        let out = transcode_profile(&fixture, p, "mkv", &EngineLimits::default()).unwrap();
        assert!(!out.is_empty());
        let info = probe(&out).unwrap();
        assert!(
            info.format_name
                .as_deref()
                .map(|n| n.to_ascii_lowercase().contains("matroska"))
                .unwrap_or(false),
            "expected matroska container, got {:?}",
            info.format_name
        );
        assert_eq!(info.vcodec.as_deref(), Some("ffv1"));
    }

    #[test]
    fn transcode_profile_prores_to_mov() {
        if !tools_available() || !encoder_available("prores_ks") {
            return;
        }
        let Some(fixture) = make_fixture() else {
            return;
        };
        let presets = preset_profiles();
        let Some(p) = presets.get("prores") else {
            return;
        };
        let out = transcode_profile(&fixture, p, "mov", &EngineLimits::default()).unwrap();
        assert!(!out.is_empty());
        let info = probe(&out).unwrap();
        assert!(
            info.format_name
                .as_deref()
                .map(|n| n.to_ascii_lowercase().contains("mov")
                    || n.to_ascii_lowercase().contains("mp4"))
                .unwrap_or(false),
            "expected QuickTime container, got {:?}",
            info.format_name
        );
    }

    #[test]
    fn transcode_profile_rejects_bad_target() {
        if !tools_available() {
            return;
        }
        let Some(fixture) = make_fixture() else {
            return;
        };
        let presets = preset_profiles();
        let Some(p) = presets.values().next() else {
            return;
        };
        assert!(matches!(
            transcode_profile(&fixture, p, "exe", &EngineLimits::default()),
            Err(VideoError::InvalidInput(_))
        ));
    }
}
