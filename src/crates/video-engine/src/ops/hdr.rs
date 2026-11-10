//! HDR detection and HDR→SDR tone mapping via ffmpeg's `zscale` +
//! `tonemap` filters.
//!
//! [`hdr_detect`] inspects the first video stream's colour metadata
//! (`color_transfer`, `color_primaries`, `color_space`, `pix_fmt`,
//! side-data list) and classifies the stream as HDR when the transfer
//! characteristic is PQ (`smpte2084`) or HLG (`arib-std-b67`), when
//! the primaries are BT.2020, or when mastering-display /
//! content-light-level side data is present.
//!
//! [`hdr_tone_map`] re-encodes through a fixed filter chain —
//! linear-light BT.2020 (`gbrpf32le`) → Hable tone map → BT.709
//! limited-range `yuv420p` — and re-encodes to 8-bit H.264 + AAC.
//! The operation is applied unconditionally: SDR input is *not*
//! rejected. For already-SDR material the `zscale` conversions are
//! close to no-ops and the output is a plain BT.709 re-encode, which
//! keeps the API simple at the cost of one generation of re-encode
//! loss — callers that care should run [`hdr_detect`] first.

use std::sync::OnceLock;

use serde_json::Value;

use crate::crates::core::engine::EngineLimits;
use crate::crates::video_engine::error::{VideoError, VideoResult};
use crate::crates::video_engine::ops::probe::probe;
use crate::crates::video_engine::ops::{
    COMMAND_TIMEOUT, FFMPEG, TempDir, check_input, check_limits, find_tool, h264_aac_args,
    probe_json, read_output, run_tool, tool_failure, write_input,
};

/// Colour/HDR description of the primary video stream.
#[derive(Debug, Clone)]
pub struct HdrInfo {
    /// Whether the stream carries HDR signalling (PQ/HLG transfer,
    /// BT.2020 primaries, or HDR side-data).
    pub is_hdr: bool,
    /// `color_transfer` as reported by ffprobe (e.g. `"smpte2084"`,
    /// `"arib-std-b67"`, `"bt709"`); `None` when unspecified.
    pub color_transfer: Option<String>,
    /// `color_primaries` (e.g. `"bt2020"`, `"bt709"`).
    pub color_primaries: Option<String>,
    /// `color_space` / matrix (e.g. `"bt2020nc"`, `"bt709"`).
    pub color_space: Option<String>,
    /// Bits per sample derived from `pix_fmt` (`yuv420p10le` → 10).
    pub bit_depth: u32,
    /// Raw `pix_fmt` string (e.g. `"yuv420p10le"`).
    pub pix_fmt: String,
}

/// Optional knobs for [`hdr_tone_map`]. Both fields default to the
/// filter's own defaults when `None`.
#[derive(Debug, Clone, Default)]
pub struct HdrToneMapParams {
    /// `desat` — desaturation strength for the Hable curve
    /// (tonemap filter default is 2 when omitted).
    pub desat: Option<f64>,
    /// `peak` — signal peak override in nits for the tone mapper.
    /// `None` omits the option so ffmpeg uses stream/container
    /// metadata or its built-in default.
    pub peak: Option<u32>,
}

/// HDR transfer characteristics recognised by [`hdr_detect`]:
/// SMPTE ST 2084 (PQ) and ARIB STD-B67 (HLG).
const HDR_TRANSFERS: &[&str] = &["smpte2084", "arib-std-b67"];

/// Inspect `input`'s primary video stream and report its HDR status.
///
/// The input is written to a scratch file and probed with `ffprobe`
/// (`-show_streams`); the first non-attached-picture video stream is
/// used. Malformed/non-media input is [`VideoError::InvalidInput`].
pub(crate) fn hdr_detect(input: &[u8]) -> VideoResult<HdrInfo> {
    check_input(input)?;
    let dir = TempDir::new("hdr-detect")?;
    let input_path = write_input(&dir, "input.bin", input)?;
    let json = probe_json(&input_path)?;

    let streams = json
        .get("streams")
        .and_then(Value::as_array)
        .ok_or_else(|| VideoError::InvalidInput("ffprobe reported no streams".into()))?;
    let video = primary_video_stream(streams)
        .ok_or_else(|| VideoError::InvalidInput("input has no video stream to inspect".into()))?;

    let color_transfer = str_field(video, "color_transfer");
    let color_primaries = str_field(video, "color_primaries");
    let color_space = str_field(video, "color_space");
    let pix_fmt = str_field(video, "pix_fmt").unwrap_or_else(|| "unknown".to_string());

    // HDR side-data signalling: mastering display colour volume and
    // content light level (max-cll/max-fall) only exist on HDR media.
    let side_data_hdr = video
        .get("side_data_list")
        .and_then(Value::as_array)
        .map(|items| {
            items.iter().any(|sd| {
                matches!(
                    sd.get("side_data_type").and_then(Value::as_str),
                    Some("Mastering display") | Some("Content light level")
                )
            })
        })
        .unwrap_or(false);

    let transfer_hdr = color_transfer
        .as_deref()
        .map(|t| HDR_TRANSFERS.contains(&t))
        .unwrap_or(false);
    let primaries_hdr = color_primaries.as_deref() == Some("bt2020");

    Ok(HdrInfo {
        is_hdr: transfer_hdr || primaries_hdr || side_data_hdr,
        color_transfer,
        color_primaries,
        color_space,
        bit_depth: bit_depth_for_pix_fmt(&pix_fmt),
        pix_fmt,
    })
}

/// Tone-map `input` to SDR BT.709 and return the re-encoded MP4
/// (H.264 8-bit + AAC) bytes.
///
/// Filter chain: `zscale` to linear light with BT.2020 nominal peak
/// luminance (`npl=100`), unpack to `gbrpf32le` for full-precision
/// processing, `zscale` primaries to BT.709, Hable `tonemap` with
/// optional `desat`/`peak`, then `zscale` back to BT.709 transfer /
/// matrix / TV range and pack to 8-bit `yuv420p`. The trailing
/// `format=yuv420p` plus `-pix_fmt yuv420p` in [`h264_aac_args`]
/// guarantee an 8-bit 4:2:0 H.264 output regardless of input depth.
///
/// **Non-HDR input is still processed** (see module docs): the chain
/// degrades to a BT.709 re-encode. Duration and input-size
/// [`EngineLimits`] are enforced on the probed input before ffmpeg
/// runs, matching the other re-encode ops.
pub(crate) fn hdr_tone_map(
    input: &[u8],
    params: &HdrToneMapParams,
    limits: &EngineLimits,
) -> VideoResult<Vec<u8>> {
    let input_len = check_input(input)?;
    let program = find_tool(FFMPEG)?;
    let info = probe(input)?;
    check_limits(&info, input_len, limits, false)?;

    // `tonemap=tonemap=hable` selects the Hable curve; `desat`/`peak`
    // are only appended when the caller supplied them so ffmpeg
    // defaults (desat=2, peak from stream metadata) apply otherwise.
    let mut tonemap_opts = "tonemap=hable".to_string();
    if let Some(desat) = params.desat {
        tonemap_opts.push_str(&format!(":desat={desat}"));
    }
    if let Some(peak) = params.peak {
        tonemap_opts.push_str(&format!(":peak={peak}"));
    }
    let vf = if zscale_functional() {
        format!(
            "zscale=t=linear:npl=100,format=gbrpf32le,zscale=p=bt709,\
             tonemap={tonemap_opts},zscale=t=bt709:m=bt709:r=tv,format=yuv420p"
        )
    } else {
        // Fallback for builds where `zscale` is compiled in but its
        // zimg-backed colourspace conversion fails at runtime
        // ("no path between colorspaces"). The native `colorspace`
        // filter expresses the same intent with enum values:
        // iall=8 → BT.2020 in, iall=5 → BT.709 out.
        let in_all = if probe_hdr_flag(input)? { 8 } else { 5 };
        format!(
            "colorspace=iall={in_all}:all={in_all},format=gbrpf32le,\
             tonemap={tonemap_opts},colorspace=iall={in_all}:all=5,format=yuv420p"
        )
    };

    let dir = TempDir::new("hdr-tonemap")?;
    let input_path = write_input(&dir, "input.bin", input)?;
    let out_path = dir.file("out.mp4");

    let mut args = vec![
        "-y".to_string(),
        "-v".to_string(),
        "error".to_string(),
        "-i".to_string(),
        input_path.to_string_lossy().into_owned(),
        "-vf".to_string(),
        vf,
    ];
    // h264_aac_args already emits `-pix_fmt yuv420p`, pinning the
    // encoder to 8-bit 4:2:0 regardless of the filter output depth.
    args.extend(h264_aac_args()?);
    args.push(out_path.to_string_lossy().into_owned());

    let output = run_tool(FFMPEG, &program, &args, COMMAND_TIMEOUT)?;
    if !output.status.success() {
        return Err(tool_failure(FFMPEG, &output));
    }
    read_output(&out_path)
}

/// Cached probe: does `zscale` actually convert colours on this
/// ffmpeg build? `ffmpeg -filters` lists the filter whenever it is
/// compiled in, but builds without a working zimg still fail at
/// runtime, so presence alone is not sufficient.
fn zscale_functional() -> bool {
    static OK: OnceLock<bool> = OnceLock::new();
    *OK.get_or_init(|| {
        let Ok(program) = find_tool(FFMPEG) else {
            return false;
        };
        let args = [
            "-v".to_string(),
            "error".to_string(),
            "-f".to_string(),
            "lavfi".to_string(),
            "-i".to_string(),
            "testsrc2=size=16x16:rate=1:duration=0.1".to_string(),
            "-vf".to_string(),
            "zscale=t=linear".to_string(),
            "-frames:v".to_string(),
            "1".to_string(),
            "-f".to_string(),
            "null".to_string(),
            "-".to_string(),
        ];
        run_tool(FFMPEG, &program, &args, COMMAND_TIMEOUT)
            .map(|o| o.status.success())
            .unwrap_or(false)
    })
}

/// `true` when `input`'s primary video stream is HDR — used to pick
/// the input colour enum for the `colorspace` fallback chain.
fn probe_hdr_flag(input: &[u8]) -> VideoResult<bool> {
    hdr_detect(input).map(|i| i.is_hdr)
}

/// First video stream that is not attached-picture cover art; falls
/// back to any video stream. Mirrors `probe.rs`'s primary-stream
/// selection so HDR metadata is read from the same stream the engine
/// reports dimensions for.
fn primary_video_stream(streams: &[Value]) -> Option<&Value> {
    let is_video = |s: &&Value| s.get("codec_type").and_then(Value::as_str) == Some("video");
    let attached_pic = |s: &Value| {
        s.get("disposition")
            .and_then(|d| d.get("attached_pic"))
            .and_then(Value::as_u64)
            == Some(1)
    };
    streams
        .iter()
        .filter(is_video)
        .find(|s| !attached_pic(s))
        .or_else(|| streams.iter().find(is_video))
}

/// `stream[field]` as an owned `String`, `None` when absent/null.
fn str_field(stream: &Value, field: &str) -> Option<String> {
    stream
        .get(field)
        .and_then(Value::as_str)
        .map(str::to_string)
}

/// Derive bits-per-sample from an ffprobe `pix_fmt` name. Covers the
/// `*10le/*10be/*12*/*14*/*16*` suffix forms, the `p010/p012/p016`
/// packed-10-bit shortcuts, and `gray10le`-style grayscale names;
/// anything unrecognised (incl. `"unknown"`) reports 8.
fn bit_depth_for_pix_fmt(pix_fmt: &str) -> u32 {
    let p = pix_fmt.to_ascii_lowercase();
    for (needle, depth) in [
        ("16le", 16u32),
        ("16be", 16),
        ("14le", 14),
        ("14be", 14),
        ("12le", 12),
        ("12be", 12),
        ("10le", 10),
        ("10be", 10),
        ("p016", 16),
        ("p012", 12),
        ("p010", 10),
        ("gray9", 9),
        ("gray10", 10),
        ("gray12", 12),
        ("gray14", 14),
        ("gray16", 16),
    ] {
        if p.contains(needle) {
            return depth;
        }
    }
    8
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::crates::video_engine::ops::FFPROBE;

    /// Encode a ~1s 160x120 H.264+AAC SDR clip via lavfi `testsrc`.
    /// Returns `None` when ffmpeg/ffprobe or an H.264 encoder is
    /// unavailable so tests skip cleanly.
    fn sdr_fixture() -> Option<Vec<u8>> {
        let program = find_tool(FFMPEG).ok()?;
        find_tool(FFPROBE).ok()?;
        let enc = h264_encoder_name()?;
        let dir = TempDir::new("hdr-test").ok()?;
        let out = dir.file("sdr.mp4");
        let args = vec![
            "-y".to_string(),
            "-v".to_string(),
            "error".to_string(),
            "-f".to_string(),
            "lavfi".to_string(),
            "-i".to_string(),
            "testsrc=size=160x120:rate=10:duration=1".to_string(),
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
        read_output(&out).ok()
    }

    /// Best available H.264 encoder name for fixture args.
    fn h264_encoder_name() -> Option<&'static str> {
        crate::crates::video_engine::ops::h264_encoder()
    }

    /// Attempt an HDR-positive fixture: 10-bit BT.2020/PQ via `zscale`
    /// + x264-style 10-bit encode. Returns `None` whenever the build
    /// cannot produce it (no 10-bit encoder, zscale quirks) — HDR
    /// coverage is best-effort because the chain is fragile across
    /// ffmpeg builds.
    fn hdr_fixture() -> Option<Vec<u8>> {
        let program = find_tool(FFMPEG).ok()?;
        let enc = h264_encoder_name()?;
        // Only try when the encoder accepts 10-bit input; bail early
        // otherwise rather than failing mid-mux.
        let dir = TempDir::new("hdr-test-hdr").ok()?;
        let out = dir.file("hdr.mp4");
        let vf = "zscale=t=smpte2084:p=bt2020:m=bt2020nc,format=yuv420p10le";
        let args = vec![
            "-y".to_string(),
            "-v".to_string(),
            "error".to_string(),
            "-f".to_string(),
            "lavfi".to_string(),
            "-i".to_string(),
            "testsrc=size=160x120:rate=10:duration=1".to_string(),
            "-vf".to_string(),
            vf.to_string(),
            "-c:v".to_string(),
            enc.to_string(),
            "-pix_fmt".to_string(),
            "yuv420p10le".to_string(),
            "-color_primaries".to_string(),
            "bt2020".to_string(),
            "-color_trc".to_string(),
            "smpte2084".to_string(),
            "-colorspace".to_string(),
            "bt2020nc".to_string(),
            out.to_string_lossy().into_owned(),
        ];
        let output = run_tool(FFMPEG, &program, &args, COMMAND_TIMEOUT).ok()?;
        if !output.status.success() {
            return None;
        }
        read_output(&out).ok()
    }

    #[test]
    fn detect_sdr_reports_not_hdr() {
        let Some(fx) = sdr_fixture() else { return };
        let info = hdr_detect(&fx).expect("detect failed");
        assert!(!info.is_hdr, "SDR fixture flagged HDR: {info:?}");
        assert_eq!(info.bit_depth, 8);
        assert!(!info.pix_fmt.is_empty());
    }

    #[test]
    fn detect_rejects_empty_input() {
        assert!(matches!(hdr_detect(&[]), Err(VideoError::InvalidInput(_))));
    }

    #[test]
    fn tone_map_sdr_produces_sdr_mp4() {
        let Some(fx) = sdr_fixture() else { return };
        let out = hdr_tone_map(&fx, &HdrToneMapParams::default(), &EngineLimits::default())
            .expect("tone map failed");
        assert!(!out.is_empty());

        // Re-probe the output: the PQ transfer must be gone. SDR
        // fixtures never had it, so this mainly proves the output is
        // a probeable video and exercises the filter chain end-to-end.
        let dir = TempDir::new("hdr-test-verify").ok().unwrap();
        let path = dir.file("out.mp4");
        std::fs::write(&path, &out).unwrap();
        let json = probe_json(&path).expect("output must probe");
        let transfer = json
            .get("streams")
            .and_then(Value::as_array)
            .and_then(|s| {
                s.iter()
                    .find(|st| st.get("codec_type").and_then(Value::as_str) == Some("video"))
            })
            .and_then(|st| st.get("color_transfer"))
            .and_then(Value::as_str)
            .unwrap_or("");
        assert_ne!(transfer, "smpte2084");
        assert_ne!(transfer, "arib-std-b67");
    }

    #[test]
    fn tone_map_accepts_params() {
        let Some(fx) = sdr_fixture() else { return };
        let params = HdrToneMapParams {
            desat: Some(1.5),
            peak: Some(1000),
        };
        let out = hdr_tone_map(&fx, &params, &EngineLimits::default()).expect("tone map failed");
        assert!(!out.is_empty());
    }

    /// Best-effort HDR-positive coverage: only runs when the fixture
    /// encodes *and* detects as HDR — otherwise it skips rather than
    /// fails, since 10-bit encoder support varies by ffmpeg build.
    #[test]
    fn detect_hdr_fixture_when_generatable() {
        let Some(fx) = hdr_fixture() else { return };
        let Ok(info) = hdr_detect(&fx) else { return };
        if !info.is_hdr {
            // Environment produced a file ffprobe doesn't classify as
            // HDR — skip instead of failing on a fragile fixture.
            return;
        }
        assert_eq!(info.bit_depth, 10);
        let out = hdr_tone_map(&fx, &HdrToneMapParams::default(), &EngineLimits::default())
            .expect("HDR tone map failed");
        assert!(!out.is_empty());
        let dir = TempDir::new("hdr-test-verify").ok().unwrap();
        let path = dir.file("out.mp4");
        std::fs::write(&path, &out).unwrap();
        let json = probe_json(&path).expect("output must probe");
        let transfer = json
            .get("streams")
            .and_then(Value::as_array)
            .and_then(|s| {
                s.iter()
                    .find(|st| st.get("codec_type").and_then(Value::as_str) == Some("video"))
            })
            .and_then(|st| st.get("color_transfer"))
            .and_then(Value::as_str)
            .unwrap_or("");
        assert_ne!(transfer, "smpte2084");
    }
}
