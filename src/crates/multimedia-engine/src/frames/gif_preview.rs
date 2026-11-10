//! Animated GIF preview generation from video.
//!
//! [`gif_preview`] downsamples a video to a small looping GIF —
//! default 320px wide at 10fps — for inline previews where a
//! thumbnail would lose the motion. When the FFmpeg build has the
//! `palettegen`/`paletteuse` filters a true two-pass pipeline is
//! used (palette statistics gathered in pass 1, applied in pass 2)
//! for the best quality; otherwise a single-pass `fps,scale` graph
//! with the built-in GIF quantizer is used and a warning is
//! recorded.
//!
//! Like [`thumbnail`](super::thumbnail) the input bytes are staged
//! into a temp dir, ffmpeg runs under [`MediaLimits`], and the GIF
//! is read back under `max_output_bytes`.

use std::ffi::OsString;

use crate::crates::multimedia_engine::error::{MediaError, MediaResult};
use crate::crates::multimedia_engine::model::{MediaLimits, MediaOutput};
use crate::crates::multimedia_engine::runtime::discovery::Runtime;
use crate::crates::multimedia_engine::runtime::process;
use crate::crates::multimedia_engine::runtime::tempdir::TempDir;

/// Maximum sane preview frame rate. Higher rates explode GIF size
/// for little perceptual gain at preview resolutions.
const MAX_FPS: f64 = 60.0;
/// Maximum palette size the GIF format supports.
const MAX_COLORS: u32 = 256;
/// Minimum palette size (2 entries).
const MIN_COLORS: u32 = 2;

/// Options for [`gif_preview`].
#[derive(Debug, Clone)]
pub struct GifPreviewOptions {
    /// Output width in pixels. Height is derived keeping the source
    /// aspect ratio (`scale=W:-2`, which also forces an even
    /// dimension). Default 320.
    pub width: u32,
    /// Preview frame rate in frames per second. Default 10, clamped
    /// to [`MAX_FPS`].
    pub fps: f64,
    /// Optional cap on the preview duration in seconds — the GIF
    /// stops at the first `max_duration_secs` of the source. `None`
    /// previews the whole clip.
    pub max_duration_secs: Option<f64>,
    /// Palette size for the two-pass pipeline, 2–256 (GIF maximum).
    /// Default 256. Ignored by the single-pass fallback, whose GIF
    /// encoder picks its own palette.
    pub colors: Option<u32>,
}

impl Default for GifPreviewOptions {
    fn default() -> Self {
        Self {
            width: 320,
            fps: 10.0,
            max_duration_secs: None,
            colors: None,
        }
    }
}

/// Create an animated GIF preview of `input` and return it as a
/// single `image/gif` [`MediaOutput`] (`preview.gif`).
///
/// Runs ffmpeg with argv vectors under `limits`:
///
/// - **Two-pass** (preferred): pass 1 renders `fps,scale` frames
///   through `palettegen` into `palette.png`; pass 2 re-renders the
///   same filter prefix and applies `paletteuse=dither=bayer`.
/// - **Single-pass fallback**: `-vf fps=…,scale=…:flags=lanczos`
///   straight into the GIF encoder, with a warning recorded.
///
/// Errors:
/// - [`MediaError::LimitExceeded`] when `input` exceeds
///   `max_input_bytes` or the GIF exceeds `max_output_bytes`.
/// - [`MediaError::InvalidInput`] for bad option values or media
///   ffmpeg cannot decode.
/// - [`MediaError::Unsupported`] when the build lacks the `fps` or
///   `scale` filters, the `gif` muxer, or the `gif` encoder.
pub fn gif_preview(
    input: &[u8],
    options: &GifPreviewOptions,
    limits: &MediaLimits,
    rt: &Runtime,
) -> MediaResult<MediaOutput> {
    check_input_size(input, limits)?;
    let resolved = ResolvedOptions::from_options(options)?;

    let caps = rt.capabilities()?;
    for filter in ["fps", "scale"] {
        if !caps.has_filter(filter) {
            return Err(MediaError::Unsupported(format!(
                "ffmpeg build lacks required filter '{filter}'"
            )));
        }
    }
    if !caps.has_muxer("gif") {
        return Err(MediaError::Unsupported(
            "ffmpeg build lacks the 'gif' muxer".to_string(),
        ));
    }
    if !caps.has_encoder("gif") {
        return Err(MediaError::Unsupported(
            "ffmpeg build lacks the 'gif' encoder".to_string(),
        ));
    }
    let two_pass = caps.has_filter("palettegen") && caps.has_filter("paletteuse");

    let (dir, input_name) = stage_input(input)?;
    let input_path = dir.file(&input_name);
    let gif_path = dir.file("preview.gif");

    let mut warnings: Vec<String> = Vec::new();
    let ffmpeg = rt.ffmpeg.clone();
    if two_pass {
        run_two_pass(
            &ffmpeg,
            &input_path,
            &resolved,
            &dir,
            &gif_path,
            limits,
            &mut warnings,
        )?;
    } else {
        run_single_pass(
            &ffmpeg,
            &input_path,
            &resolved,
            &gif_path,
            limits,
            &mut warnings,
        )?;
        warnings.push(
            "palettegen/paletteuse unavailable in this ffmpeg build; \
             used single-pass GIF quantization (lower quality)"
                .to_string(),
        );
    }

    let bytes = dir.read_limited("preview.gif", limits.max_output_bytes)?;
    let mut out = MediaOutput::single("preview.gif", bytes);
    if let Some(file) = out.primary.as_mut() {
        file.content_type = Some("image/gif".to_string());
    }
    out.warnings = warnings;
    Ok(out)
}

/// Validated/clamped option values shared by both pipelines.
struct ResolvedOptions {
    colors: u32,
    duration_arg: Option<String>,
    /// `fps=…,scale=W:-2:flags=lanczos` prefix shared by every pass.
    vf_prefix: String,
}

impl ResolvedOptions {
    fn from_options(options: &GifPreviewOptions) -> MediaResult<Self> {
        if options.width == 0 {
            return Err(MediaError::InvalidInput(
                "gif_preview width must be greater than zero".to_string(),
            ));
        }
        if !options.fps.is_finite() || options.fps <= 0.0 {
            return Err(MediaError::InvalidInput(format!(
                "gif_preview fps must be positive and finite, got {}",
                options.fps
            )));
        }
        if let Some(d) = options.max_duration_secs
            && (!d.is_finite() || d <= 0.0)
        {
            return Err(MediaError::InvalidInput(format!(
                "gif_preview max_duration_secs must be positive and finite, got {d}"
            )));
        }
        let colors = options
            .colors
            .unwrap_or(MAX_COLORS)
            .clamp(MIN_COLORS, MAX_COLORS);
        let fps = options.fps.min(MAX_FPS);
        let vf_prefix = format!(
            "fps={},scale={}:-2:flags=lanczos",
            trim_float(fps),
            options.width
        );
        Ok(Self {
            colors,
            duration_arg: options.max_duration_secs.map(|d| format!("{:.3}", d)),
            vf_prefix,
        })
    }
}

/// Base argv shared by every ffmpeg invocation: `-y -v error -i in`.
fn base_args(input_path: &std::path::Path) -> Vec<OsString> {
    vec![
        OsString::from("-y"),
        OsString::from("-v"),
        OsString::from("error"),
        OsString::from("-i"),
        input_path.as_os_str().to_os_string(),
    ]
}

/// Append `-t <secs>` (output duration cap) when configured.
fn push_duration(args: &mut Vec<OsString>, resolved: &ResolvedOptions) {
    if let Some(d) = &resolved.duration_arg {
        args.push(OsString::from("-t"));
        args.push(OsString::from(d.clone()));
    }
}

/// Two-pass pipeline: gather palette statistics, then quantize
/// against them. Both passes re-run the same `fps,scale` prefix so
/// the sampled frames match exactly.
fn run_two_pass(
    ffmpeg: &std::path::Path,
    input_path: &std::path::Path,
    resolved: &ResolvedOptions,
    dir: &TempDir,
    gif_path: &std::path::Path,
    limits: &MediaLimits,
    warnings: &mut Vec<String>,
) -> MediaResult<()> {
    let palette_name = "palette.png";
    let palette_path = dir.file(palette_name);

    // Pass 1: render the sampled frames through palettegen; the
    // output is a single PNG holding the generated palette.
    let mut args = base_args(input_path);
    push_duration(&mut args, resolved);
    args.push(OsString::from("-vf"));
    args.push(OsString::from(format!(
        "{},palettegen=max_colors={}:stats_mode=diff",
        resolved.vf_prefix, resolved.colors
    )));
    args.push(OsString::from("-frames:v"));
    args.push(OsString::from("1"));
    args.push(palette_path.as_os_str().to_os_string());
    let out = process::run_checked("ffmpeg", ffmpeg, &args, None, limits)?;
    if !out.stderr.is_empty() {
        warnings.push(format!("palettegen pass: {}", out.stderr));
    }

    // Pass 2: same frames, quantized against the palette with Bayer
    // dithering for smoother gradients.
    let mut args = base_args(input_path);
    args.push(OsString::from("-i"));
    args.push(palette_path.into_os_string());
    args.push(OsString::from("-filter_complex"));
    args.push(OsString::from(format!(
        "[0:v]{}[p];[p][1:v]paletteuse=dither=bayer[out]",
        resolved.vf_prefix
    )));
    args.push(OsString::from("-map"));
    args.push(OsString::from("[out]"));
    args.push(OsString::from("-an"));
    push_duration(&mut args, resolved);
    args.push(OsString::from("-loop"));
    args.push(OsString::from("0"));
    args.push(gif_path.as_os_str().to_os_string());
    let out = process::run_checked("ffmpeg", ffmpeg, &args, None, limits)?;
    if !out.stderr.is_empty() {
        warnings.push(format!("paletteuse pass: {}", out.stderr));
    }
    Ok(())
}

/// Single-pass fallback: let the GIF encoder quantize directly.
fn run_single_pass(
    ffmpeg: &std::path::Path,
    input_path: &std::path::Path,
    resolved: &ResolvedOptions,
    gif_path: &std::path::Path,
    limits: &MediaLimits,
    warnings: &mut Vec<String>,
) -> MediaResult<()> {
    let mut args = base_args(input_path);
    push_duration(&mut args, resolved);
    args.push(OsString::from("-vf"));
    args.push(OsString::from(resolved.vf_prefix.clone()));
    args.push(OsString::from("-an"));
    args.push(OsString::from("-loop"));
    args.push(OsString::from("0"));
    args.push(gif_path.as_os_str().to_os_string());
    let out = process::run_checked("ffmpeg", ffmpeg, &args, None, limits)?;
    if !out.stderr.is_empty() {
        warnings.push(format!("ffmpeg: {}", out.stderr));
    }
    Ok(())
}

/// Reject inputs over `max_input_bytes` before doing any work.
fn check_input_size(input: &[u8], limits: &MediaLimits) -> MediaResult<()> {
    if let Some(max) = limits.max_input_bytes
        && input.len() as u64 > max
    {
        return Err(MediaError::LimitExceeded(format!(
            "input is {} bytes, exceeding max_input_bytes {max}",
            input.len()
        )));
    }
    Ok(())
}

/// Write `input` into a fresh temp dir and return the dir plus the
/// sniffed input file name.
fn stage_input(input: &[u8]) -> MediaResult<(TempDir, String)> {
    let dir = TempDir::new("gifprev")?;
    let name = format!("input{}", sniff_extension(input));
    dir.write(&name, input)?;
    Ok((dir, name))
}

/// Sniff a container extension from magic bytes so ffmpeg gets a
/// format hint. `.bin` fallback — ffmpeg probes content itself.
fn sniff_extension(data: &[u8]) -> &'static str {
    if data.len() >= 12 && &data[4..8] == b"ftyp" {
        return if &data[8..12] == b"qt  " {
            ".mov"
        } else {
            ".mp4"
        };
    }
    if data.starts_with(&[0x1A, 0x45, 0xDF, 0xA3]) {
        return ".mkv";
    }
    if data.starts_with(b"RIFF") && data.len() >= 12 {
        return match &data[8..12] {
            b"AVI " => ".avi",
            _ => ".bin",
        };
    }
    if data.starts_with(b"OggS") {
        return ".ogg";
    }
    ".bin"
}

/// Format an f64 for an ffmpeg filter value without trailing noise
/// (`10` not `10.000000`).
fn trim_float(v: f64) -> String {
    let s = format!("{v:.6}");
    s.trim_end_matches('0').trim_end_matches('.').to_string()
}

// ------------------------------------------------------------------
// tests
// ------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;

    /// Generate a 1s 64x64@10fps testsrc mp4 via ffmpeg lavfi.
    /// Returns `None` when ffmpeg or the encoders are unavailable so
    /// tests skip gracefully on minimal builds.
    fn make_clip() -> Option<Vec<u8>> {
        let ffmpeg = crate::crates::multimedia_engine::runtime::discovery::find_tool("ffmpeg")?;
        let dir = TempDir::new("gifprev-test").ok()?;
        let out_path = dir.file("clip.mp4");
        let args: Vec<OsString> = [
            "-y",
            "-v",
            "error",
            "-f",
            "lavfi",
            "-i",
            "testsrc=size=64x64:rate=10:duration=1",
            "-c:v",
            "libx264",
            "-pix_fmt",
            "yuv420p",
            "-an",
            out_path.to_str()?,
        ]
        .iter()
        .map(OsString::from)
        .collect();
        let limits = MediaLimits::default();
        let out = process::run("ffmpeg", &ffmpeg, &args, None, &limits).ok()?;
        if !out.success {
            return None;
        }
        std::fs::read(&out_path).ok()
    }

    fn runtime() -> Option<Runtime> {
        Runtime::discover().ok()
    }

    #[test]
    fn gif_preview_produces_animated_gif() {
        let Some(clip) = make_clip() else {
            eprintln!("skipping: ffmpeg lavfi/x264 unavailable");
            return;
        };
        let Some(rt) = runtime() else {
            eprintln!("skipping: ffmpeg/ffprobe not on PATH");
            return;
        };
        let out = gif_preview(
            &clip,
            &GifPreviewOptions::default(),
            &MediaLimits::default(),
            &rt,
        )
        .expect("gif_preview");
        let file = out.primary.as_ref().expect("primary output");
        assert!(file.name.ends_with(".gif"), "name {}", file.name);
        assert_eq!(file.content_type.as_deref(), Some("image/gif"));
        let bytes = &file.bytes;
        assert!(
            bytes.len() > 200,
            "suspiciously small gif ({} bytes)",
            bytes.len()
        );
        assert!(
            bytes.starts_with(b"GIF8"),
            "missing GIF magic: {:02x?}",
            &bytes[..6.min(bytes.len())]
        );
    }

    #[test]
    fn gif_preview_rejects_bad_options() {
        let Some(rt) = runtime() else {
            eprintln!("skipping: ffmpeg/ffprobe not on PATH");
            return;
        };
        let clip = vec![0u8; 16];
        let opts = GifPreviewOptions {
            width: 0,
            ..GifPreviewOptions::default()
        };
        let err = gif_preview(&clip, &opts, &MediaLimits::default(), &rt).expect_err("must fail");
        assert!(matches!(err, MediaError::InvalidInput(_)), "got {err:?}");
    }

    #[test]
    fn gif_preview_rejects_garbage_input() {
        let Some(rt) = runtime() else {
            eprintln!("skipping: ffmpeg/ffprobe not on PATH");
            return;
        };
        let garbage = vec![0xDEu8, 0xAD, 0xBE, 0xEF, 1, 2, 3, 4, 5, 6, 7, 8];
        let err = gif_preview(
            &garbage,
            &GifPreviewOptions::default(),
            &MediaLimits::default(),
            &rt,
        )
        .expect_err("must fail");
        assert!(
            matches!(
                err,
                MediaError::InvalidInput(_)
                    | MediaError::ToolFailed { .. }
                    | MediaError::Unsupported(_)
            ),
            "unexpected error: {err:?}"
        );
    }
}
