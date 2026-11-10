//! Edit-proxy / mezzanine generation for professional workflows.
//!
//! [`generate_proxy`] produces three flavours of derived media from one
//! input buffer, all as full re-encodes (never stream copies — the
//! whole point is a different codec profile):
//!
//! - [`ProxyKind::Mezzanine`] — an intra-frame editing master: every
//!   frame is independently decodable, which is what NLEs need for
//!   smooth scrubbing and frame-accurate cuts. Preferred encoder is
//!   `prores_ks` (Apple ProRes) at profile **3 = HQ** in a QuickTime
//!   `.mov` with `-vendor apl0` and `pcm_s16le` audio. `crf`/`preset`
//!   do not apply to ProRes. Fallback chain when `prores_ks` is
//!   absent: `ffv1` (Matroska `.mkv`, `-level 3`) with `flac` audio.
//!   `dnxhd`/`dnxhr` were considered and deliberately skipped: they
//!   require specific frame sizes/rates/bitrates per profile and fail
//!   opaquely on arbitrary sources. When neither encoder exists the
//!   op fails honestly with [`VideoError::ExternalTool`].
//! - [`ProxyKind::Proxy`] — a small, cheap-to-decode editing proxy:
//!   H.264 (`h264_encoder()` → `libx264` else `libopenh264`) in
//!   `.mp4`, long-GOP, `aac` 128k audio. Height is capped at
//!   `target_height` (default 720) via `scale=-2:{h}` — aspect ratio
//!   is preserved and the width stays even for `yuv420p`. The scale
//!   filter is only added when the probed source height *exceeds*
//!   the target (or is unknown); small sources are never upscaled.
//!   `crf` (default 23) and `preset` (default `veryfast`) apply to
//!   `libx264`; `libopenh264` has no CRF/preset AVOptions, so it runs
//!   `-rc_mode quality` with a bitrate derived from the target height
//!   instead — the params are ignored there, as documented.
//! - [`ProxyKind::Archival`] — a lossless preservation master:
//!   `ffv1 -level 3` video + `flac` audio in `.mkv`. When `ffv1` is
//!   absent the fallback is `prores_ks` profile **4 = 4444** (full
//!   RGBA, mathematically near-lossless for 8-bit sources) in `.mov`
//!   with `pcm_s16le` audio. When neither exists the op fails
//!   honestly.
//!
//! Audio streams: all audio streams are mapped (`-map 0:v? -map
//! 0:a?`); inputs without audio still succeed. Subtitle/data streams
//! are dropped. Inputs with no video stream are rejected with
//! [`VideoError::InvalidInput`].

use std::path::Path;

use crate::crates::core::engine::EngineLimits;
use crate::crates::video_engine::error::{VideoError, VideoResult};
use crate::crates::video_engine::model::VideoInfo;
use crate::crates::video_engine::ops::probe::probe;
use crate::crates::video_engine::ops::{
    COMMAND_TIMEOUT, FFMPEG, TempDir, check_input, check_limits, encoder_available, find_tool,
    h264_encoder, read_output, run_tool, tool_failure, write_input,
};

/// Which derived-media flavour [`generate_proxy`] should produce.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ProxyKind {
    /// Intra-frame editing master (ProRes HQ, ffv1 fallback).
    Mezzanine,
    /// Small long-GOP H.264 editing proxy capped at a target height.
    Proxy,
    /// Lossless preservation master (ffv1+flac, ProRes 4444 fallback).
    Archival,
}

/// Tunables for [`generate_proxy`]. All fields are optional; defaults
/// are documented per kind in the module docs.
#[derive(Debug, Clone)]
pub struct ProxyParams {
    /// Which output flavour to generate.
    pub kind: ProxyKind,
    /// Maximum output height for [`ProxyKind::Proxy`] (default 720).
    /// Ignored by the other kinds, which keep source geometry.
    /// `0` is rejected as [`VideoError::InvalidInput`].
    pub target_height: Option<u32>,
    /// H.264 CRF quality for [`ProxyKind::Proxy`] when `libx264` is
    /// the encoder (default 23, valid range 0–51). Ignored for
    /// `libopenh264` and for the intra-frame kinds.
    pub crf: Option<u32>,
    /// x264 preset name for [`ProxyKind::Proxy`] (default
    /// `veryfast`). Validated against the x264 preset list before
    /// ffmpeg runs. Ignored for `libopenh264` and the intra-frame
    /// kinds.
    pub preset: Option<String>,
}

impl ProxyParams {
    /// Parameters for `kind` with every tunable left at its default.
    pub fn new(kind: ProxyKind) -> Self {
        Self {
            kind,
            target_height: None,
            crf: None,
            preset: None,
        }
    }
}

/// prores_ks profile for the mezzanine master: 3 = HQ (422 10-bit,
/// intra-frame). ProRes "Proxy" (0) is intentionally *not* used —
/// it is a low-bitrate offline-review codec, not an edit master.
const PRORES_MEZZANINE_PROFILE: u8 = 3;
/// prores_ks profile for the archival fallback: 4 = 4444 (RGBA).
const PRORES_ARCHIVAL_PROFILE: u8 = 4;
/// Default proxy height when `target_height` is unset.
const DEFAULT_PROXY_HEIGHT: u32 = 720;
/// Default H.264 CRF for the proxy kind.
const DEFAULT_PROXY_CRF: u32 = 23;
/// Default x264 preset for the proxy kind.
const DEFAULT_PROXY_PRESET: &str = "veryfast";
/// AAC bitrate for proxy audio.
const PROXY_AUDIO_BITRATE: &str = "128k";

/// x264 presets accepted by `-preset`. `libx264` fails opaquely on
/// anything else, so we validate up front.
const X264_PRESETS: &[&str] = &[
    "ultrafast",
    "superfast",
    "veryfast",
    "faster",
    "fast",
    "medium",
    "slow",
    "slower",
    "veryslow",
    "placebo",
];

/// Bitrate ladder for `libopenh264` proxy encodes, keyed on the
/// target height. OpenH264 has no CRF AVOption; `rc_mode quality`
/// with a sane bitrate is the honest equivalent.
fn openh264_bitrate_kbps(target_height: u32) -> u32 {
    match target_height {
        0..=360 => 800,
        361..=480 => 1200,
        481..=720 => 2000,
        721..=1080 => 3500,
        _ => 5000,
    }
}

/// Validate `params.crf`/`params.preset` for the H.264 proxy path.
/// Returns `(crf, preset)` resolved to their defaults.
fn resolve_proxy_params(params: &ProxyParams) -> VideoResult<(u32, String)> {
    let crf = params.crf.unwrap_or(DEFAULT_PROXY_CRF);
    if crf > 51 {
        return Err(VideoError::InvalidInput(format!(
            "crf {crf} out of range for H.264 (valid: 0-51)"
        )));
    }
    let preset = params
        .preset
        .clone()
        .unwrap_or_else(|| DEFAULT_PROXY_PRESET.to_string());
    if !X264_PRESETS.contains(&preset.as_str()) {
        return Err(VideoError::InvalidInput(format!(
            "unknown x264 preset '{preset}'; supported: {}",
            X264_PRESETS.join(", ")
        )));
    }
    Ok((crf, preset))
}

/// Run `ffmpeg` with `args` and return the produced file's bytes.
fn run_ffmpeg(program: &std::path::Path, args: &[String], out_path: &Path) -> VideoResult<Vec<u8>> {
    let output = run_tool(FFMPEG, program, args, COMMAND_TIMEOUT)?;
    if !output.status.success() {
        return Err(tool_failure(FFMPEG, &output));
    }
    read_output(out_path)
}

/// Common ffmpeg argument preamble: overwrite, quiet errors, input.
fn base_args(input_path: &Path) -> Vec<String> {
    vec![
        "-y".to_string(),
        "-v".to_string(),
        "error".to_string(),
        "-i".to_string(),
        input_path.to_string_lossy().into_owned(),
    ]
}

/// prores_ks + pcm_s16le encode args into `ext`'s container.
fn prores_args(profile: u8) -> Vec<String> {
    vec![
        "-map".to_string(),
        "0:v?".to_string(),
        "-map".to_string(),
        "0:a?".to_string(),
        "-c:v".to_string(),
        "prores_ks".to_string(),
        "-profile:v".to_string(),
        profile.to_string(),
        "-vendor".to_string(),
        "apl0".to_string(),
        "-c:a".to_string(),
        "pcm_s16le".to_string(),
    ]
}

/// ffv1 level-3 + flac encode args.
fn ffv1_args() -> Vec<String> {
    vec![
        "-map".to_string(),
        "0:v?".to_string(),
        "-map".to_string(),
        "0:a?".to_string(),
        "-c:v".to_string(),
        "ffv1".to_string(),
        "-level".to_string(),
        "3".to_string(),
        "-c:a".to_string(),
        "flac".to_string(),
    ]
}

/// Mezzanine encode: `prores_ks` HQ `.mov` preferred, `ffv1` `.mkv`
/// fallback. `dnxhd`/`dnxhr` are skipped — they demand fixed
/// resolutions/frame-rates per profile and fail opaquely on general
/// input.
fn mezzanine_args() -> Option<(Vec<String>, &'static str)> {
    if encoder_available("prores_ks") {
        return Some((prores_args(PRORES_MEZZANINE_PROFILE), "mov"));
    }
    if encoder_available("ffv1") {
        return Some((ffv1_args(), "mkv"));
    }
    None
}

/// Archival encode: `ffv1` + `flac` `.mkv` preferred, `prores_ks`
/// 4444 `.mov` fallback.
fn archival_args() -> Option<(Vec<String>, &'static str)> {
    if encoder_available("ffv1") {
        return Some((ffv1_args(), "mkv"));
    }
    if encoder_available("prores_ks") {
        return Some((prores_args(PRORES_ARCHIVAL_PROFILE), "mov"));
    }
    None
}

/// Proxy encode args: downscale-only H.264 + AAC into `.mp4`.
/// `scale=-2:{h}` keeps aspect ratio and even width; applied only
/// when the source is taller than the target so small sources are
/// never upscaled. `crf`/`preset` (already resolved to defaults and
/// validated) are used with `libx264`; `libopenh264` has no such
/// AVOptions and gets `rc_mode quality` + a height-derived bitrate.
fn proxy_args(
    params: &ProxyParams,
    info: &VideoInfo,
    encoder: &str,
    crf: u32,
    preset: &str,
) -> VideoResult<Vec<String>> {
    let target = params.target_height.unwrap_or(DEFAULT_PROXY_HEIGHT);
    if target == 0 {
        return Err(VideoError::InvalidInput(
            "target_height must be greater than zero".to_string(),
        ));
    }

    let mut args = vec![
        "-map".to_string(),
        "0:v?".to_string(),
        "-map".to_string(),
        "0:a?".to_string(),
    ];

    // Downscale only: keep source geometry when it already fits.
    let needs_scale = match info.height {
        Some(h) => h > target,
        None => true,
    };
    if needs_scale {
        args.extend(["-vf".to_string(), format!("scale=-2:{target}")]);
    }

    args.extend(["-c:v".to_string(), encoder.to_string()]);
    if encoder == "libx264" {
        args.extend([
            "-crf".to_string(),
            crf.to_string(),
            "-preset".to_string(),
            preset.to_string(),
        ]);
    } else {
        // libopenh264: no CRF/preset AVOptions — quality-mode
        // rate control with a height-derived bitrate instead.
        args.extend([
            "-rc_mode".to_string(),
            "quality".to_string(),
            "-b:v".to_string(),
            format!("{}k", openh264_bitrate_kbps(target)),
        ]);
    }
    args.extend([
        "-pix_fmt".to_string(),
        "yuv420p".to_string(),
        "-c:a".to_string(),
        "aac".to_string(),
        "-b:a".to_string(),
        PROXY_AUDIO_BITRATE.to_string(),
        "-movflags".to_string(),
        "+faststart".to_string(),
    ]);
    Ok(args)
}

/// Generate a mezzanine, proxy, or archival master from `input`.
///
/// `input` is probed once for [`VideoInfo`] (used for limits,
/// video-stream presence, and the proxy downscale decision) and
/// written to a scratch file for ffmpeg. Returns the encoded bytes;
/// the container is `.mov` for ProRes outputs, `.mkv` for ffv1, and
/// `.mp4` for H.264 proxies — see the module docs for the full
/// fallback chain per [`ProxyKind`].
pub(crate) fn generate_proxy(
    input: &[u8],
    params: &ProxyParams,
    limits: &EngineLimits,
) -> VideoResult<Vec<u8>> {
    let input_len = check_input(input)?;
    let program = find_tool(FFMPEG)?;

    let info = probe(input)?;
    if !info.has_video {
        return Err(VideoError::InvalidInput(
            "input has no video stream; proxy generation requires video".to_string(),
        ));
    }
    check_limits(&info, input_len, limits, false)?;

    let (codec_args, ext) = match params.kind {
        ProxyKind::Mezzanine => mezzanine_args().ok_or_else(|| VideoError::ExternalTool {
            tool: FFMPEG.to_string(),
            detail: "mezzanine output requires the prores_ks or ffv1 encoder; \
                     this ffmpeg build provides neither"
                .to_string(),
        })?,
        ProxyKind::Archival => archival_args().ok_or_else(|| VideoError::ExternalTool {
            tool: FFMPEG.to_string(),
            detail: "archival output requires the ffv1 or prores_ks encoder; \
                     this ffmpeg build provides neither"
                .to_string(),
        })?,
        ProxyKind::Proxy => {
            // Validate crf/preset up front regardless of which H.264
            // encoder this build has — callers get the same honest
            // InvalidInput either way.
            let (crf, preset) = resolve_proxy_params(params)?;
            let encoder = h264_encoder().ok_or_else(|| VideoError::ExternalTool {
                tool: FFMPEG.to_string(),
                detail: "proxy output requires an H.264 encoder (libx264 or \
                         libopenh264); this ffmpeg build provides neither"
                    .to_string(),
            })?;
            (proxy_args(params, &info, encoder, crf, &preset)?, "mp4")
        }
    };

    let dir = TempDir::new("proxy")?;
    let input_path = write_input(&dir, "input.bin", input)?;
    let out_path = dir.file(&format!("out.{ext}"));

    let mut args = base_args(&input_path);
    args.extend(codec_args);
    args.push(out_path.to_string_lossy().into_owned());

    run_ffmpeg(&program, &args, &out_path)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::crates::video_engine::ops::probe::probe;
    use crate::crates::video_engine::ops::{FFMPEG, find_tool, run_tool};

    fn unlimited() -> EngineLimits {
        EngineLimits {
            max_input_bytes: None,
            max_pages: None,
            max_pixels: None,
            max_outputs: None,
            max_duration_secs: None,
        }
    }

    /// 1s 320x240 clip with a sine audio track, encoded with the
    /// native `mpeg4` video + `pcm_s16le` audio encoders (both
    /// always built into ffmpeg) into `.avi`. Returns `None` when
    /// ffmpeg is unavailable.
    fn fixture() -> Option<Vec<u8>> {
        let program = find_tool(FFMPEG).ok()?;
        let dir = TempDir::new("proxy-test").ok()?;
        let out = dir.file("fixture.avi");
        let args: Vec<String> = [
            "-hide_banner",
            "-loglevel",
            "error",
            "-y",
            "-f",
            "lavfi",
            "-i",
            "testsrc=duration=1:size=320x240:rate=15",
            "-f",
            "lavfi",
            "-i",
            "sine=frequency=440:duration=1",
            "-c:v",
            "mpeg4",
            "-pix_fmt",
            "yuv420p",
            "-c:a",
            "pcm_s16le",
            "-shortest",
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

    /// True when `out` looks like a QuickTime/MP4-family file
    /// (`....ftyp` at offset 4).
    fn is_mov_family(out: &[u8]) -> bool {
        out.len() > 12 && &out[4..8] == b"ftyp"
    }

    /// True when `out` is a Matroska/WebM EBML file.
    fn is_ebml(out: &[u8]) -> bool {
        out.starts_with(&[0x1A, 0x45, 0xDF, 0xA3])
    }

    #[test]
    fn proxy_downscales_to_target_height() {
        let Some(input) = fixture() else { return };
        if h264_encoder().is_none() {
            return; // no H.264 encoder — op would (correctly) fail.
        }
        let mut params = ProxyParams::new(ProxyKind::Proxy);
        params.target_height = Some(120);
        let out = generate_proxy(&input, &params, &unlimited()).unwrap();
        assert!(!out.is_empty());
        assert!(is_mov_family(&out), "proxy should be an mp4/mov file");
        let info = probe(&out).unwrap();
        // 320x240 → scale=-2:120 gives 160x120.
        assert_eq!(info.height, Some(120));
        assert_eq!(info.width, Some(160));
    }

    #[test]
    fn proxy_never_upscales_small_sources() {
        let Some(input) = fixture() else { return };
        if h264_encoder().is_none() {
            return;
        }
        // 240 < 720: geometry must be preserved.
        let out =
            generate_proxy(&input, &ProxyParams::new(ProxyKind::Proxy), &unlimited()).unwrap();
        let info = probe(&out).unwrap();
        assert_eq!(info.height, Some(240));
        assert_eq!(info.width, Some(320));
    }

    #[test]
    fn proxy_rejects_bad_params() {
        let Some(input) = fixture() else { return };
        if h264_encoder().is_none() {
            return;
        }
        let mut params = ProxyParams::new(ProxyKind::Proxy);
        params.target_height = Some(0);
        assert!(matches!(
            generate_proxy(&input, &params, &unlimited()),
            Err(VideoError::InvalidInput(_))
        ));
        let mut params = ProxyParams::new(ProxyKind::Proxy);
        params.crf = Some(99);
        assert!(matches!(
            generate_proxy(&input, &params, &unlimited()),
            Err(VideoError::InvalidInput(_))
        ));
    }

    #[test]
    fn mezzanine_produces_intra_frame_master() {
        let Some(input) = fixture() else { return };
        let out = generate_proxy(
            &input,
            &ProxyParams::new(ProxyKind::Mezzanine),
            &unlimited(),
        );
        let Ok(out) = out else {
            // Neither prores_ks nor ffv1 available — honest failure.
            assert!(matches!(out, Err(VideoError::ExternalTool { .. })));
            return;
        };
        assert!(!out.is_empty());
        let info = probe(&out).unwrap();
        if encoder_available("prores_ks") {
            assert!(is_mov_family(&out), "prores mezzanine should be .mov");
            assert_eq!(info.vcodec.as_deref(), Some("prores"));
        } else {
            assert!(is_ebml(&out), "ffv1 mezzanine fallback should be .mkv");
            assert_eq!(info.vcodec.as_deref(), Some("ffv1"));
        }
    }

    #[test]
    fn archival_produces_lossless_master() {
        let Some(input) = fixture() else { return };
        let out = generate_proxy(&input, &ProxyParams::new(ProxyKind::Archival), &unlimited());
        let Ok(out) = out else {
            assert!(matches!(out, Err(VideoError::ExternalTool { .. })));
            return;
        };
        assert!(!out.is_empty());
        let info = probe(&out).unwrap();
        if encoder_available("ffv1") {
            assert!(is_ebml(&out), "ffv1 archival should be .mkv");
            assert_eq!(info.vcodec.as_deref(), Some("ffv1"));
        } else {
            assert!(
                is_mov_family(&out),
                "prores 4444 archival fallback should be .mov"
            );
            assert_eq!(info.vcodec.as_deref(), Some("prores"));
        }
    }

    #[test]
    fn rejects_empty_and_audio_only_input() {
        assert!(matches!(
            generate_proxy(b"", &ProxyParams::new(ProxyKind::Proxy), &unlimited()),
            Err(VideoError::InvalidInput(_))
        ));
        // Garbage bytes: probe fails → InvalidInput, not a panic.
        assert!(matches!(
            generate_proxy(
                b"not media",
                &ProxyParams::new(ProxyKind::Proxy),
                &unlimited()
            ),
            Err(VideoError::InvalidInput(_))
        ));
    }
}
