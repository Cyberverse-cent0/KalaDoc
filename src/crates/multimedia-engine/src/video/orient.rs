//! Video orientation operations: rotation, flips and transposes.
//!
//! [`orient`] re-encodes the video stream through an FFmpeg
//! `transpose`/`hflip`/`vflip` filter chain. Re-encoding is required —
//! these ops change pixel geometry, so stream copy is impossible.
//!
//! Rotation **metadata** is handled explicitly: FFmpeg's auto-rotate
//! (`-autorotate`, on by default) first normalizes any display-matrix
//! rotation into the pixels, the requested [`OrientOp`] is then applied
//! to those upright pixels, and the output is written with
//! `-display_rotation 0` plus a cleared `rotate` stream tag so players
//! never double-rotate the result.
//!
//! Only the first video stream and any audio streams are carried over;
//! subtitle/data/attachment streams are dropped. The output container
//! is MP4 — the op takes raw bytes with no filename, so a fixed,
//! universally-muxable target keeps naming deterministic
//! (`"output.mp4"`).

use std::ffi::OsString;

use serde::{Deserialize, Serialize};

use crate::crates::multimedia_engine::error::{MediaError, MediaResult};
use crate::crates::multimedia_engine::model::{MediaFile, MediaLimits, MediaOutput};
use crate::crates::multimedia_engine::runtime::discovery::Runtime;
use crate::crates::multimedia_engine::runtime::{process, tempdir::TempDir};

/// A geometric orientation operation applied to every video frame.
///
/// Serde representation is snake_case (`"rotate_90"`, `"flip_h"`, ...).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum OrientOp {
    /// Rotate 90° clockwise. Swaps width/height.
    Rotate90,
    /// Rotate 180°. Dimensions unchanged.
    Rotate180,
    /// Rotate 90° counter-clockwise (270° clockwise). Swaps
    /// width/height.
    Rotate270,
    /// Mirror horizontally (left-right flip). Dimensions unchanged.
    FlipH,
    /// Mirror vertically (top-bottom flip). Dimensions unchanged.
    FlipV,
    /// Transpose across the main diagonal (top-left ↔ bottom-right).
    /// Swaps width/height.
    Transpose,
    /// Transpose across the anti-diagonal (top-right ↔ bottom-left).
    /// Swaps width/height.
    Transverse,
}

impl OrientOp {
    /// FFmpeg `-vf` filter chain implementing the op.
    ///
    /// `transpose` direction values (verified against FFmpeg's
    /// `vf_transpose`): `0` = counter-clockwise + vertical flip
    /// (main-diagonal transpose), `1` = 90° clockwise, `2` = 90°
    /// counter-clockwise, `3` = clockwise + vertical flip
    /// (anti-diagonal transpose).
    fn filter_chain(self) -> &'static str {
        match self {
            // Two 90° CW turns = 180°; keeps the op inside the single
            // `transpose` filter.
            Self::Rotate90 => "transpose=1",
            Self::Rotate180 => "transpose=1,transpose=1",
            Self::Rotate270 => "transpose=2",
            Self::FlipH => "hflip",
            Self::FlipV => "vflip",
            Self::Transpose => "transpose=0",
            Self::Transverse => "transpose=3",
        }
    }

    /// FFmpeg filters that must exist in this build for the op.
    fn required_filters(self) -> &'static [&'static str] {
        match self {
            Self::Rotate90
            | Self::Rotate180
            | Self::Rotate270
            | Self::Transpose
            | Self::Transverse => &["transpose"],
            Self::FlipH => &["hflip"],
            Self::FlipV => &["vflip"],
        }
    }
}

/// Video encoder preference for the MP4 output, best-first. The first
/// candidate present in the capability registry wins; native `mpeg4`
/// is the universal fallback.
const MP4_VIDEO_ENCODERS: &[&str] = &["libx264", "h264", "libopenh264", "mpeg4"];

/// Audio encoder preference for the MP4 output. When none exists the
/// audio streams are dropped rather than failing the whole op.
const MP4_AUDIO_ENCODERS: &[&str] = &["aac", "libfdk_aac"];

/// Apply `op` to the video stream(s) of `input`, returning an MP4.
///
/// # Errors
///
/// - [`MediaError::LimitExceeded`] when `input` exceeds
///   `limits.max_input_bytes` or the produced file exceeds
///   `limits.max_output_bytes`.
/// - [`MediaError::Unsupported`] when this FFmpeg build lacks the
///   `transpose`/`hflip`/`vflip` filter, the `mp4` muxer, or any
///   usable video encoder.
/// - [`MediaError::ToolFailed`] / [`MediaError::InvalidInput`] /
///   [`MediaError::Timeout`] from the ffmpeg invocation itself.
pub fn orient(
    input: &[u8],
    op: OrientOp,
    limits: &MediaLimits,
    rt: &Runtime,
) -> MediaResult<MediaOutput> {
    if let Some(max) = limits.max_input_bytes
        && input.len() as u64 > max
    {
        return Err(MediaError::LimitExceeded(format!(
            "input is {} bytes, exceeding max_input_bytes {max}",
            input.len()
        )));
    }

    let caps = rt.capabilities()?;

    // Honest capabilities: refuse up front when the required filter,
    // muxer or encoder is absent from this FFmpeg build.
    let missing_filters: Vec<&str> = op
        .required_filters()
        .iter()
        .copied()
        .filter(|f| !caps.has_filter(f))
        .collect();
    if !missing_filters.is_empty() {
        return Err(MediaError::Unsupported(format!(
            "ffmpeg build lacks required filter(s): {}",
            missing_filters.join(", ")
        )));
    }
    if !caps.has_muxer("mp4") {
        return Err(MediaError::Unsupported(
            "ffmpeg build lacks the mp4 muxer".to_string(),
        ));
    }
    let video_encoder = MP4_VIDEO_ENCODERS
        .iter()
        .copied()
        .find(|e| caps.has_encoder(e))
        .ok_or_else(|| {
            MediaError::Unsupported(
                "ffmpeg build has no usable mp4 video encoder \
                 (libx264/h264/libopenh264/mpeg4)"
                    .to_string(),
            )
        })?;

    // Audio is re-encoded to AAC when possible; otherwise dropped with
    // a warning rather than failing an otherwise-valid op.
    let mut warnings = Vec::new();
    let mut extra_args: Vec<OsString> = Vec::new();
    if let Some(audio_encoder) = MP4_AUDIO_ENCODERS
        .iter()
        .copied()
        .find(|e| caps.has_encoder(e))
    {
        extra_args.extend([OsString::from("-c:a"), OsString::from(audio_encoder)]);
    } else {
        extra_args.push(OsString::from("-an"));
        warnings
            .push("no AAC audio encoder in this ffmpeg build; audio streams dropped".to_string());
    }

    let dir = TempDir::new("orient")?;
    let input_path = dir.write("input.bin", input)?;
    let output_name = "output.mp4";
    let output_path = dir.file(output_name);

    // `-autorotate` (default on) normalizes any input display-matrix
    // rotation into pixels before our filter runs, so the requested op
    // always applies to the upright image. `-display_rotation 0` and
    // the cleared `rotate` tag then guarantee the output carries no
    // residual rotation metadata for players to re-apply.
    let args: Vec<OsString> = [
        OsString::from("-y"),
        OsString::from("-v"),
        OsString::from("error"),
        OsString::from("-i"),
        input_path.as_os_str().to_os_string(),
        OsString::from("-map"),
        OsString::from("0:v:0"),
        OsString::from("-map"),
        OsString::from("0:a?"),
        OsString::from("-filter:v"),
        OsString::from(op.filter_chain()),
        OsString::from("-c:v"),
        OsString::from(video_encoder),
        OsString::from("-pix_fmt"),
        OsString::from("yuv420p"),
        OsString::from("-fps_mode"),
        OsString::from("passthrough"),
        OsString::from("-sn"),
        OsString::from("-dn"),
        OsString::from("-display_rotation"),
        OsString::from("0"),
        OsString::from("-metadata:s:v"),
        OsString::from("rotate=0"),
    ]
    .into_iter()
    .chain(extra_args)
    .chain([output_path.as_os_str().to_os_string()])
    .collect();

    process::run_checked("ffmpeg", &rt.ffmpeg, &args, None, limits)?;

    let bytes = dir.read_limited(output_name, limits.max_output_bytes)?;
    Ok(MediaOutput {
        primary: Some(MediaFile {
            name: output_name.to_string(),
            bytes,
            content_type: Some("video/mp4".to_string()),
        }),
        extra: Vec::new(),
        warnings,
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::crates::multimedia_engine::runtime::discovery::{self, find_tool};
    use crate::crates::multimedia_engine::runtime::tempdir::TempDir;
    use std::path::Path;
    use std::process::Command;

    fn runtime() -> Option<Runtime> {
        if find_tool("ffmpeg").is_none() || find_tool("ffprobe").is_none() {
            eprintln!("skipping: ffmpeg/ffprobe not on PATH");
            return None;
        }
        match Runtime::discover() {
            Ok(rt) => Some(rt),
            Err(e) => {
                eprintln!("skipping: runtime discovery failed: {e}");
                None
            }
        }
    }

    /// First video encoder usable for the test fixtures, mirroring the
    /// op's own preference order.
    fn fixture_video_encoder() -> &'static str {
        for cand in MP4_VIDEO_ENCODERS {
            // Cheap probe: try encoding a single frame; keep the first
            // that succeeds.
            let dir = match TempDir::new("orient-fixture") {
                Ok(d) => d,
                Err(_) => return "mpeg4",
            };
            let out = dir.file("probe.mp4");
            let ok = Command::new("ffmpeg")
                .args([
                    "-y",
                    "-v",
                    "error",
                    "-f",
                    "lavfi",
                    "-i",
                    "testsrc=size=16x16:duration=0.1:rate=10",
                    "-frames:v",
                    "1",
                    "-c:v",
                    cand,
                    "-pix_fmt",
                    "yuv420p",
                    out.to_string_lossy().as_ref(),
                ])
                .status()
                .map(|s| s.success())
                .unwrap_or(false);
            if ok {
                return cand;
            }
        }
        "mpeg4"
    }

    /// Asymmetric 64x48 video: top-left 32x24 quadrant blue, rest red.
    /// Orientation ops move the blue quadrant in predictable ways.
    fn make_quadrant_mp4() -> Option<Vec<u8>> {
        let dir = TempDir::new("orient-test-src").ok()?;
        let out = dir.file("quad.mp4");
        let encoder = fixture_video_encoder();
        let ok = Command::new("ffmpeg")
            .args([
                "-y",
                "-v",
                "error",
                "-f",
                "lavfi",
                "-i",
                "color=c=red:s=64x48:d=0.4:r=10,drawbox=x=0:y=0:w=32:h=24:c=blue:t=fill",
                "-c:v",
                encoder,
                "-pix_fmt",
                "yuv420p",
                "-an",
                out.to_string_lossy().as_ref(),
            ])
            .status()
            .map(|s| s.success())
            .unwrap_or(false);
        if !ok {
            return None;
        }
        std::fs::read(&out).ok()
    }

    /// Plain asymmetric testsrc2 clip (no color markers needed — only
    /// dimensions are asserted).
    fn make_testsrc_mp4() -> Option<Vec<u8>> {
        let dir = TempDir::new("orient-test-src").ok()?;
        let out = dir.file("src.mp4");
        let encoder = fixture_video_encoder();
        let ok = Command::new("ffmpeg")
            .args([
                "-y",
                "-v",
                "error",
                "-f",
                "lavfi",
                "-i",
                "testsrc2=size=64x48:duration=0.4:rate=10",
                "-c:v",
                encoder,
                "-pix_fmt",
                "yuv420p",
                "-an",
                out.to_string_lossy().as_ref(),
            ])
            .status()
            .map(|s| s.success())
            .unwrap_or(false);
        if !ok {
            return None;
        }
        std::fs::read(&out).ok()
    }

    /// Video stream dimensions of `bytes`, via ffprobe.
    fn probe_dims(bytes: &[u8]) -> Option<(u32, u32)> {
        let dir = TempDir::new("orient-probe").ok()?;
        let input = dir.write("in.mp4", bytes).ok()?;
        let probe = find_tool("ffprobe")?;
        let out = Command::new(probe)
            .args([
                "-v",
                "error",
                "-select_streams",
                "v:0",
                "-show_entries",
                "stream=width,height",
                "-of",
                "csv=p=0",
                input.to_string_lossy().as_ref(),
            ])
            .output()
            .ok()?;
        if !out.status.success() {
            return None;
        }
        let text = String::from_utf8_lossy(&out.stdout);
        let (w, h) = text.trim().split_once(',')?;
        Some((w.trim().parse().ok()?, h.trim().parse().ok()?))
    }

    /// First decoded frame as RGB24 bytes at `width`x`height`.
    fn first_frame_rgb(bytes: &[u8], width: u32, height: u32) -> Option<Vec<u8>> {
        let dir = TempDir::new("orient-frame").ok()?;
        let input = dir.write("in.mp4", bytes).ok()?;
        let ffmpeg = find_tool("ffmpeg")?;
        let out = Command::new(ffmpeg)
            .args([
                "-v",
                "error",
                "-i",
                input.to_string_lossy().as_ref(),
                "-an",
                "-vf",
                "select='eq(n,0)'",
                "-frames:v",
                "1",
                "-f",
                "rawvideo",
                "-pix_fmt",
                "rgb24",
                "-",
            ])
            .output()
            .ok()?;
        if !out.status.success() {
            return None;
        }
        let _ = (width, height); // dims implied by caller's expectations
        Some(out.stdout)
    }

    /// Sample the RGB triple at pixel (x, y).
    fn pixel(frame: &[u8], width: u32, x: u32, y: u32) -> (i32, i32, i32) {
        let i = ((y * width + x) * 3) as usize;
        (frame[i] as i32, frame[i + 1] as i32, frame[i + 2] as i32)
    }

    /// yuv420p conversion shifts pure colors; assert dominance instead
    /// of exact values.
    fn assert_blue(p: (i32, i32, i32), ctx: &str) {
        assert!(
            p.2 > p.0 + 40,
            "{ctx}: expected blue-dominant pixel, got {p:?}"
        );
    }
    fn assert_red(p: (i32, i32, i32), ctx: &str) {
        assert!(
            p.0 > p.2 + 40,
            "{ctx}: expected red-dominant pixel, got {p:?}"
        );
    }

    #[test]
    fn dimension_swapping_matches_op_kind() {
        let Some(rt) = runtime() else { return };
        let Some(input) = make_testsrc_mp4() else {
            eprintln!("skipping: could not build test mp4");
            return;
        };
        let limits = MediaLimits::default();

        // Quarter-turns and diagonal transposes swap width/height.
        for op in [
            OrientOp::Rotate90,
            OrientOp::Rotate270,
            OrientOp::Transpose,
            OrientOp::Transverse,
        ] {
            let out = orient(&input, op, &limits, &rt).expect("orient");
            let bytes = &out.primary.expect("primary output").bytes;
            assert_eq!(
                probe_dims(bytes),
                Some((48, 64)),
                "{op:?} should swap 64x48 to 48x64"
            );
        }

        // 180° rotation and axis flips keep dimensions.
        for op in [OrientOp::Rotate180, OrientOp::FlipH, OrientOp::FlipV] {
            let out = orient(&input, op, &limits, &rt).expect("orient");
            let primary = out.primary.as_ref().expect("primary output");
            assert_eq!(
                probe_dims(&primary.bytes),
                Some((64, 48)),
                "{op:?} should keep 64x48 dimensions"
            );
            assert_eq!(primary.content_type.as_deref(), Some("video/mp4"));
        }
    }

    #[test]
    fn flips_and_180_move_quadrant_as_expected() {
        let Some(rt) = runtime() else { return };
        let Some(input) = make_quadrant_mp4() else {
            eprintln!("skipping: could not build quadrant mp4");
            return;
        };
        let limits = MediaLimits::default();
        // Input 64x48, blue quadrant = top-left (x<32, y<24).
        // Sample points sit well inside each quadrant, away from the
        // 4:2:0 chroma edge bleed.

        // FlipH: blue moves to top-right.
        let out = orient(&input, OrientOp::FlipH, &limits, &rt).expect("flip_h");
        let frame = first_frame_rgb(&out.primary.unwrap().bytes, 64, 48).expect("frame");
        assert_blue(pixel(&frame, 64, 48, 12), "flip_h top-right");
        assert_red(pixel(&frame, 64, 16, 12), "flip_h top-left");
        assert_red(pixel(&frame, 64, 48, 36), "flip_h bottom-right");

        // FlipV: blue moves to bottom-left.
        let out = orient(&input, OrientOp::FlipV, &limits, &rt).expect("flip_v");
        let frame = first_frame_rgb(&out.primary.unwrap().bytes, 64, 48).expect("frame");
        assert_blue(pixel(&frame, 64, 16, 36), "flip_v bottom-left");
        assert_red(pixel(&frame, 64, 48, 12), "flip_v top-right");

        // Rotate180: blue moves to bottom-right.
        let out = orient(&input, OrientOp::Rotate180, &limits, &rt).expect("rotate_180");
        let frame = first_frame_rgb(&out.primary.unwrap().bytes, 64, 48).expect("frame");
        assert_blue(pixel(&frame, 64, 48, 36), "rotate_180 bottom-right");
        assert_red(pixel(&frame, 64, 16, 12), "rotate_180 top-left");
    }

    #[test]
    fn quarter_turns_and_transpose_move_quadrant_as_expected() {
        let Some(rt) = runtime() else { return };
        let Some(input) = make_quadrant_mp4() else {
            eprintln!("skipping: could not build quadrant mp4");
            return;
        };
        let limits = MediaLimits::default();
        // All of these produce 48x64 output from the 64x48 input.

        // Rotate90 CW: top-left quadrant -> top-right.
        let out = orient(&input, OrientOp::Rotate90, &limits, &rt).expect("rotate_90");
        let bytes = &out.primary.unwrap().bytes;
        assert_eq!(probe_dims(bytes), Some((48, 64)));
        let frame = first_frame_rgb(bytes, 48, 64).expect("frame");
        assert_blue(pixel(&frame, 48, 36, 16), "rotate_90 top-right");
        assert_red(pixel(&frame, 48, 12, 16), "rotate_90 top-left");

        // Rotate270 (CCW): top-left quadrant -> bottom-left.
        let out = orient(&input, OrientOp::Rotate270, &limits, &rt).expect("rotate_270");
        let bytes = &out.primary.unwrap().bytes;
        assert_eq!(probe_dims(bytes), Some((48, 64)));
        let frame = first_frame_rgb(bytes, 48, 64).expect("frame");
        assert_blue(pixel(&frame, 48, 12, 48), "rotate_270 bottom-left");
        assert_red(pixel(&frame, 48, 36, 16), "rotate_270 top-right");

        // Transpose (main diagonal): blue top-left rectangle maps to a
        // top-left region of the 48x64 output.
        let out = orient(&input, OrientOp::Transpose, &limits, &rt).expect("transpose");
        let bytes = &out.primary.unwrap().bytes;
        assert_eq!(probe_dims(bytes), Some((48, 64)));
        let frame = first_frame_rgb(bytes, 48, 64).expect("frame");
        assert_blue(pixel(&frame, 48, 12, 16), "transpose top-left");
        assert_red(pixel(&frame, 48, 36, 48), "transpose bottom-right");
    }

    #[test]
    fn rejects_invalid_and_oversized_input() {
        let Some(rt) = runtime() else { return };

        // Garbage bytes: ffmpeg cannot demux them.
        let err = orient(
            b"not a media file at all",
            OrientOp::FlipH,
            &MediaLimits::default(),
            &rt,
        );
        assert!(err.is_err(), "expected error for garbage input");

        // Input byte cap is enforced before ffmpeg ever runs.
        let limits = MediaLimits {
            max_input_bytes: Some(4),
            ..MediaLimits::default()
        };
        let err = orient(&[0u8; 16], OrientOp::Rotate90, &limits, &rt).unwrap_err();
        assert!(matches!(err, MediaError::LimitExceeded(_)));
    }
}
