//! Chroma-key (green/blue screen) removal for video.
//!
//! [`chroma_key`] writes the input buffer to a temp file, applies an
//! ffmpeg `chromakey` filter (falling back to `colorkey` when the
//! newer filter is not compiled in) via `-vf`, and re-encodes to a
//! container chosen from [`ChromaKeyOptions::output_format`].
//!
//! # Alpha and containers
//!
//! `chromakey`/`colorkey` replace keyed pixels with **transparent**
//! ones — the alpha only survives if the chosen pixel format and
//! codec/muxer carry an alpha plane:
//!
//! - **WebM (VP8/VP9)** is the reliable alpha path: the filter output
//!   is encoded as `yuva420p` with `libvpx-vp9` (fallback `libvpx`).
//! - **MP4** has no common alpha-capable video codec. When MP4 is
//!   selected (explicitly or by fallback) the op still runs — it is
//!   often useful for compositing keys that become opaque — but a
//!   warning is recorded that alpha may be dropped by the encoder.
//!   `libx264` is never used: H.264 cannot store alpha, and the op
//!   picks another H.264 encoder (`libopenh264`, hardware encoders)
//!   or fails with [`MediaError::Unsupported`].
//!
//! When `output_format` is `None` the op prefers WebM whenever a WebM
//! muxer and a VPX encoder exist; otherwise it falls back to MP4 with
//! the honest alpha warning above.
//!
//! # Filter selection
//!
//! `chromakey` is preferred (it has a `format` option and wider
//! pixel-format support); `colorkey` is the legacy equivalent. Both
//! take `color`, `similarity` and `blend`. Neither supports feathered
//! edges — [`ChromaKeyOptions::feather`] is accepted for forward
//! compatibility but produces a warning when set.

use std::ffi::OsString;

use serde::{Deserialize, Serialize};

use crate::crates::multimedia_engine::error::{MediaError, MediaResult};
use crate::crates::multimedia_engine::model::{MediaFile, MediaLimits, MediaOutput};
use crate::crates::multimedia_engine::runtime::discovery::{Runtime, RuntimeCapabilities};
use crate::crates::multimedia_engine::runtime::process;
use crate::crates::multimedia_engine::runtime::tempdir::TempDir;

/// Options for [`chroma_key`]. Serde-friendly: all fields deserialize
/// with defaults when omitted.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ChromaKeyOptions {
    /// Key color to remove. Accepts `#RRGGBB`, `0xRRGGBB`, or any
    /// ffmpeg color name (`"green"`, `"blue"`, ...). `#RRGGBB` is
    /// normalized to `0xRRGGBB`. Must not contain `,` or `:`
    /// (filter-graph metacharacters).
    #[serde(default = "default_key_color")]
    pub color: String,
    /// How close a pixel's chroma must be to `color` to be keyed out
    /// (0.0–1.0). Larger = more aggressive removal.
    #[serde(default = "default_similarity")]
    pub similarity: f64,
    /// Edge feathering of the keyed region (0.0–1.0). `0.0` is a hard
    /// edge; larger softens the transition.
    #[serde(default)]
    pub blend: f64,
    /// Reserved edge feathering beyond `blend`. FFmpeg's
    /// `chromakey`/`colorkey` have no feather parameter — when `Some`,
    /// a warning is recorded and the value is ignored.
    #[serde(default)]
    pub feather: Option<f64>,
    /// Output container extension without a leading dot
    /// (`"webm"`, `"mp4"`, `"mkv"`, ...). `None` prefers WebM (the
    /// only common alpha-capable container here) and falls back to
    /// MP4 with a warning.
    #[serde(default)]
    pub output_format: Option<String>,
    /// Video encoder override (e.g. `"libvpx-vp9"`, `"libopenh264"`).
    /// Must exist in this FFmpeg build. `None` picks a container
    /// default; `libx264` is rejected because it cannot carry alpha.
    #[serde(default)]
    pub video_codec: Option<String>,
    /// Audio encoder override (e.g. `"libopus"`, `"aac"`). `None`
    /// picks a container default.
    #[serde(default)]
    pub audio_codec: Option<String>,
}

impl Default for ChromaKeyOptions {
    fn default() -> Self {
        Self {
            color: default_key_color(),
            similarity: default_similarity(),
            blend: 0.0,
            feather: None,
            output_format: None,
            video_codec: None,
            audio_codec: None,
        }
    }
}

fn default_key_color() -> String {
    "0x00FF00".to_string()
}
fn default_similarity() -> f64 {
    0.1
}

/// H.264 encoders acceptable for alpha-less containers, in preference
/// order. `libx264` is deliberately absent: it cannot encode an alpha
/// plane and the op must never silently drop alpha via it.
const H264_ENCODERS: &[&str] = &[
    "libopenh264",
    "h264_nvenc",
    "h264_qsv",
    "h264_amf",
    "h264_mf",
];

/// Alpha-capable VPX encoders for WebM, in preference order.
const VPX_ENCODERS: &[&str] = &["libvpx-vp9", "libvpx"];

/// Chroma-key `input` and write the keyed (alpha) video back under
/// `limits`. See module docs for container/codec selection.
///
/// # Errors
///
/// - [`MediaError::InvalidInput`] — empty input, invalid color string,
///   or `similarity`/`blend` outside 0.0–1.0.
/// - [`MediaError::LimitExceeded`] — input/output byte limits.
/// - [`MediaError::Unsupported`] — neither `chromakey` nor `colorkey`
///   filter exists, or no suitable encoder/muxer for the requested
///   container.
/// - [`MediaError::ToolFailed`] / [`MediaError::Timeout`] — ffmpeg
///   rejected the input or the filter chain failed.
pub fn chroma_key(
    input: &[u8],
    options: &ChromaKeyOptions,
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

    let color = normalize_color(&options.color)?;
    validate_unit("similarity", options.similarity)?;
    validate_unit("blend", options.blend)?;
    if let Some(f) = options.feather {
        if !f.is_finite() || !(0.0..=1.0).contains(&f) {
            return Err(MediaError::InvalidInput(format!(
                "feather must be a finite number in 0.0..=1.0, got {f}"
            )));
        }
    }

    let caps = rt.capabilities()?;
    let filter = pick_key_filter(caps)?;
    let plan = resolve_output_plan(options, caps)?;

    let dir = TempDir::new("chromakey")?;
    dir.write("input.bin", input)?;
    let output_name = format!("output.{}", plan.ext);
    let output_path = dir.file(&output_name);

    let vf = format!(
        "{filter}=color={color}:similarity={}:blend={}",
        fmt_unit(options.similarity),
        fmt_unit(options.blend),
    );

    let mut argv: Vec<OsString> = process::args(&[
        "-hide_banner",
        "-loglevel",
        "error",
        "-nostdin",
        "-y",
        "-i",
        dir.file("input.bin").to_str().unwrap_or("input"),
        "-vf",
        &vf,
        "-map",
        "0:v:0",
        "-map",
        "0:a?",
        "-sn",
        "-dn",
        "-c:v",
        &plan.video_codec,
        "-c:a",
        &plan.audio_codec,
        "-pix_fmt",
        &plan.pix_fmt,
    ])
    .into_iter()
    .collect();
    argv.extend(plan.extra_args.iter().cloned());
    argv.push(output_path.as_os_str().to_os_string());

    process::run_checked("ffmpeg", &rt.ffmpeg, &argv, None, limits)?;

    let bytes = dir.read_limited(&output_name, limits.max_output_bytes)?;
    if bytes.is_empty() {
        return Err(MediaError::ToolFailed {
            tool: "ffmpeg",
            status: "empty output".to_string(),
            detail: format!("ffmpeg produced an empty {output_name}"),
        });
    }

    let mut warnings = plan.warnings;
    if options.feather.is_some() {
        warnings.push(
            "feather is not supported by ffmpeg's chromakey/colorkey filters; ignored".to_string(),
        );
    }

    Ok(MediaOutput {
        primary: Some(MediaFile {
            name: output_name,
            bytes,
            content_type: content_type_for(&plan.ext),
        }),
        extra: Vec::new(),
        warnings,
    })
}

// ---------------------------------------------------------------------------
// Filter / container planning
// ---------------------------------------------------------------------------

/// Pick the chroma-key filter: `chromakey` preferred, `colorkey`
/// fallback. Both share the `color`/`similarity`/`blend` options.
fn pick_key_filter(caps: &RuntimeCapabilities) -> MediaResult<&'static str> {
    if caps.has_filter("chromakey") {
        Ok("chromakey")
    } else if caps.has_filter("colorkey") {
        Ok("colorkey")
    } else {
        Err(MediaError::Unsupported(
            "neither 'chromakey' nor 'colorkey' filter is available in this FFmpeg build"
                .to_string(),
        ))
    }
}

/// Everything needed to invoke ffmpeg for one container/codec plan.
struct OutputPlan {
    /// Output extension (`"webm"`, `"mp4"`, ...).
    ext: String,
    /// Video encoder name passed to `-c:v`.
    video_codec: String,
    /// Audio encoder name passed to `-c:a`.
    audio_codec: String,
    /// Pixel format passed to `-pix_fmt`.
    pix_fmt: String,
    /// Extra codec-tuning argv (`-crf`, `-b:v`, ...).
    extra_args: Vec<OsString>,
    warnings: Vec<String>,
}

/// Resolve container, encoders and pixel format. `libx264` is never
/// selected; alpha-capable WebM is preferred when the caller did not
/// pin a container.
fn resolve_output_plan(
    options: &ChromaKeyOptions,
    caps: &RuntimeCapabilities,
) -> MediaResult<OutputPlan> {
    // The filter actually used is decided once here so warnings can
    // name it; pick_key_filter's result is re-derived identically.
    pick_key_filter(caps)?;
    let mut warnings = Vec::new();

    let ext = match options.output_format.as_deref() {
        Some(fmt) => {
            let ext = fmt.trim().trim_start_matches('.').to_ascii_lowercase();
            if ext.is_empty() {
                return Err(MediaError::InvalidInput(
                    "output_format must be non-empty".to_string(),
                ));
            }
            if !caps.has_muxer(&ext) {
                return Err(MediaError::Unsupported(format!(
                    "ffmpeg muxer '{ext}' not available in this build"
                )));
            }
            ext
        }
        None => {
            if caps.has_muxer("webm") && VPX_ENCODERS.iter().any(|e| caps.has_encoder(e)) {
                "webm".to_string()
            } else if caps.has_muxer("mp4") {
                warnings.push(
                    "output_format not set and WebM/VPX unavailable; using MP4 — \
                     alpha from the key may be dropped by the chosen encoder"
                        .to_string(),
                );
                "mp4".to_string()
            } else {
                return Err(MediaError::Unsupported(
                    "no usable output muxer (webm/mp4) in this FFmpeg build".to_string(),
                ));
            }
        }
    };

    let is_alpha_container = matches!(ext.as_str(), "webm" | "mkv");
    let (video_codec, pix_fmt, extra_args) = match options.video_codec.as_deref() {
        Some(codec) => {
            if codec.eq_ignore_ascii_case("libx264") {
                return Err(MediaError::InvalidInput(
                    "video_codec 'libx264' cannot carry an alpha channel; \
                     use a VPX/VP9 encoder (WebM) or accept alpha loss in MP4"
                        .to_string(),
                ));
            }
            if !caps.has_encoder(codec) {
                return Err(MediaError::Unsupported(format!(
                    "video encoder '{codec}' not available in this FFmpeg build"
                )));
            }
            let pix = if is_alpha_container {
                "yuva420p"
            } else {
                "yuv420p"
            };
            if !is_alpha_container {
                warnings.push(format!(
                    "container '{ext}' with encoder '{codec}' may not preserve the \
                     alpha channel produced by chroma keying"
                ));
            }
            (codec.to_string(), pix.to_string(), Vec::new())
        }
        None => default_video_codec(&ext, caps, &mut warnings)?,
    };

    let audio_codec = match options.audio_codec.as_deref() {
        Some(codec) => {
            if !caps.has_encoder(codec) {
                return Err(MediaError::Unsupported(format!(
                    "audio encoder '{codec}' not available in this FFmpeg build"
                )));
            }
            codec.to_string()
        }
        None => default_audio_codec(&ext, caps)?,
    };

    Ok(OutputPlan {
        ext,
        video_codec,
        audio_codec,
        pix_fmt,
        extra_args,
        warnings,
    })
}

/// Default video encoder for a container. WebM prefers VP9 (then VP8)
/// with `yuva420p`; MP4-like containers use the first available
/// non-x264 H.264 encoder with `yuv420p` and an alpha warning.
fn default_video_codec(
    ext: &str,
    caps: &RuntimeCapabilities,
    warnings: &mut Vec<String>,
) -> MediaResult<(String, String, Vec<OsString>)> {
    match ext {
        "webm" => {
            for name in VPX_ENCODERS {
                if caps.has_encoder(name) {
                    let mut extra = Vec::new();
                    if *name == "libvpx-vp9" {
                        extra = process::args(&["-crf", "33", "-b:v", "0"]);
                    }
                    return Ok((name.to_string(), "yuva420p".to_string(), extra));
                }
            }
            Err(MediaError::Unsupported(
                "no WebM video encoder (libvpx-vp9/libvpx) in this FFmpeg build".to_string(),
            ))
        }
        "mkv" => {
            // Matroska can carry alpha too, but no mainstream VPX-free
            // alpha codec is guaranteed; prefer VPX when present,
            // otherwise fall through to H.264 with a warning.
            if let Ok(plan) = default_video_codec("webm", caps, warnings) {
                return Ok(plan);
            }
            let codec = pick_h264(caps).ok_or_else(|| {
                MediaError::Unsupported(
                    "no suitable video encoder for 'mkv' output in this FFmpeg build".to_string(),
                )
            })?;
            warnings.push(format!(
                "container 'mkv' encoded with '{codec}'; alpha from chroma keying may be lost"
            ));
            Ok((codec.to_string(), "yuv420p".to_string(), Vec::new()))
        }
        _ => {
            let codec = pick_h264(caps).ok_or_else(|| {
                MediaError::Unsupported(format!(
                    "no H.264 encoder for '{ext}' output in this FFmpeg build \
                     (libx264 is never used by chroma_key)"
                ))
            })?;
            warnings.push(format!(
                "container '{ext}' encoded with '{codec}'; alpha from chroma keying \
                 will be lost — use output_format \"webm\" to keep transparency"
            ));
            Ok((codec.to_string(), "yuv420p".to_string(), Vec::new()))
        }
    }
}

/// First available non-x264 H.264 encoder.
fn pick_h264(caps: &RuntimeCapabilities) -> Option<&'static str> {
    H264_ENCODERS.iter().copied().find(|e| caps.has_encoder(e))
}

/// Default audio encoder for a container: Opus for WebM, AAC
/// otherwise (Opus/Vorbis fallbacks for WebM).
fn default_audio_codec(ext: &str, caps: &RuntimeCapabilities) -> MediaResult<String> {
    let candidates: &[&str] = if ext == "webm" {
        &["libopus", "libvorbis"]
    } else {
        &["aac", "libopus", "libvorbis"]
    };
    for name in candidates {
        if caps.has_encoder(name) {
            return Ok(name.to_string());
        }
    }
    Err(MediaError::Unsupported(format!(
        "no usable audio encoder for '{ext}' output in this FFmpeg build"
    )))
}

// ---------------------------------------------------------------------------
// Validation / formatting helpers
// ---------------------------------------------------------------------------

/// Normalize a key color to ffmpeg's `0xRRGGBB` form. `#RRGGBB` is
/// rewritten; `0xRRGGBB` and color names pass through (validated
/// against filter-graph metacharacters).
fn normalize_color(color: &str) -> MediaResult<String> {
    let trimmed = color.trim();
    let normalized = if let Some(hex) = trimmed.strip_prefix('#') {
        if hex.len() != 6 || !hex.bytes().all(|b| b.is_ascii_hexdigit()) {
            return Err(MediaError::InvalidInput(format!(
                "invalid color {color:?}: expected #RRGGBB, 0xRRGGBB or a color name"
            )));
        }
        format!("0x{hex}")
    } else {
        trimmed.to_string()
    };
    if normalized.is_empty()
        || normalized.contains(',')
        || normalized.contains(':')
        || normalized.contains('\\')
    {
        return Err(MediaError::InvalidInput(format!(
            "invalid color {color:?}: must be #RRGGBB, 0xRRGGBB or a color name \
             and contain no ',', ':' or '\\'"
        )));
    }
    Ok(normalized)
}

fn validate_unit(name: &str, v: f64) -> MediaResult<()> {
    if !v.is_finite() || !(0.0..=1.0).contains(&v) {
        return Err(MediaError::InvalidInput(format!(
            "{name} must be a finite number in 0.0..=1.0, got {v}"
        )));
    }
    Ok(())
}

/// Format a 0..=1 float compactly for the filter graph (trailing
/// zeros stripped by ffmpeg's parser anyway).
fn fmt_unit(v: f64) -> String {
    format!("{v:.4}")
}

/// Best-effort MIME type for the produced container.
fn content_type_for(ext: &str) -> Option<String> {
    let mime = match ext {
        "mp4" | "m4v" | "m4a" => "video/mp4",
        "mkv" => "video/x-matroska",
        "webm" => "video/webm",
        "mov" => "video/quicktime",
        _ => return None,
    };
    Some(mime.to_string())
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;
    use crate::crates::multimedia_engine::probe::inspect::inspect;
    use crate::crates::multimedia_engine::runtime::discovery;

    fn runtime() -> Runtime {
        Runtime::discover().expect("ffmpeg/ffprobe must be on PATH")
    }

    fn limits() -> MediaLimits {
        MediaLimits::default()
    }

    /// Generate a 1s 64x64 green clip with a solid red box in the
    /// middle via lavfi `color` + `drawbox`, encoded with libx264.
    /// Returns `None` when lavfi/libx264 are unavailable — callers
    /// skip gracefully.
    fn sample_input() -> Option<Vec<u8>> {
        let ffmpeg = discovery::find_tool("ffmpeg")?;
        let dir = TempDir::new("chromakey-test-in").ok()?;
        let out_path = dir.file("sample.mp4");
        let argv = process::args(&[
            "-hide_banner",
            "-loglevel",
            "error",
            "-nostdin",
            "-y",
            "-f",
            "lavfi",
            "-i",
            "color=c=green:size=64x64:rate=15:duration=1,drawbox=x=16:y=16:w=32:h=32:c=red:t=fill",
            "-c:v",
            "libx264",
            "-preset",
            "ultrafast",
            "-pix_fmt",
            "yuv420p",
            out_path.to_str()?,
        ]);
        let out = process::run("ffmpeg", &ffmpeg, &argv, None, &limits()).ok()?;
        if !out.success {
            return None;
        }
        std::fs::read(&out_path).ok()
    }

    #[test]
    fn chroma_key_runs_and_produces_video_stream() {
        let Some(input) = sample_input() else {
            eprintln!("skipping: ffmpeg lavfi/libx264 unavailable");
            return;
        };
        let rt = runtime();
        let caps = match rt.capabilities() {
            Ok(c) => c,
            Err(e) => {
                eprintln!("skipping: capability probe failed: {e}");
                return;
            }
        };
        if !caps.has_filter("chromakey") && !caps.has_filter("colorkey") {
            eprintln!("skipping: no chromakey/colorkey filter in this build");
            return;
        }

        let options = ChromaKeyOptions {
            color: "green".to_string(),
            ..Default::default()
        };
        let out = chroma_key(&input, &options, &limits(), &rt).expect("chroma_key");
        let file = out.primary.expect("primary output");
        assert!(!file.bytes.is_empty());

        let info = inspect(&file.bytes, &limits()).expect("probe output");
        assert!(info.video_streams >= 1, "expected a video stream");
    }

    #[test]
    fn chroma_key_rejects_invalid_options() {
        let rt = runtime();
        let input = vec![0u8; 64];
        for options in [
            ChromaKeyOptions {
                color: "bad:color".to_string(),
                ..Default::default()
            },
            ChromaKeyOptions {
                color: "#12".to_string(),
                ..Default::default()
            },
            ChromaKeyOptions {
                similarity: 1.5,
                ..Default::default()
            },
            ChromaKeyOptions {
                blend: f64::NAN,
                ..Default::default()
            },
        ] {
            let err = chroma_key(&input, &options, &limits(), &rt).expect_err("must fail");
            assert!(
                matches!(err, MediaError::InvalidInput(_)),
                "unexpected error: {err:?}"
            );
        }
    }

    #[test]
    fn normalize_color_accepts_hex_and_names() {
        assert_eq!(normalize_color("#00FF00").unwrap(), "0x00FF00");
        assert_eq!(normalize_color("0x00ff00").unwrap(), "0x00ff00");
        assert_eq!(normalize_color("green").unwrap(), "green");
        assert!(normalize_color("#GGGGGG").is_err());
        assert!(normalize_color("red,blue").is_err());
    }
}
