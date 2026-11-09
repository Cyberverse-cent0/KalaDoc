//! Video geometry operations: resize, crop, pad, letterbox/pillarbox
//! and black-border removal.
//!
//! All ops are process-backed: the input bytes are staged in a
//! [`TempDir`], one `ffmpeg` invocation applies a `-vf` filter chain,
//! and the result is read back under [`MediaLimits`].
//!
//! # Re-encode contract
//!
//! Geometry changes (scale/crop/pad) alter pixel layout and **cannot**
//! be stream-copied — every op re-encodes video to H.264 in MP4.
//! The encoder is chosen from the runtime capability registry:
//! `libx264` (`-crf 23 -preset veryfast`) is preferred; `libopenh264`
//! is used when x264 is not compiled in, and a warning is recorded in
//! [`MediaOutput::warnings`]. When neither exists the op fails with
//! [`MediaError::Unsupported`] — there is no silent fallback to a
//! different codec family. Audio is re-encoded to AAC when present;
//! subtitle/data streams are dropped (`-map 0:v:0 -map 0:a? -sn -dn`).
//!
//! Each options struct carries a `copy_safe` flag for API uniformity
//! with other engine ops. It is currently **ignored**: geometry ops
//! always re-encode because stream copy cannot express these
//! transforms.

use std::ffi::OsString;
use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};

use crate::crates::multimedia_engine::error::{MediaError, MediaResult};
use crate::crates::multimedia_engine::model::{MediaInput, MediaLimits, MediaOutput};
use crate::crates::multimedia_engine::probe::ProbeReport;
use crate::crates::multimedia_engine::runtime::discovery::{Runtime, RuntimeCapabilities};
use crate::crates::multimedia_engine::runtime::process::{self, args};
use crate::crates::multimedia_engine::runtime::tempdir::TempDir;

// ---------------------------------------------------------------------------
// Option types (serde-friendly)
// ---------------------------------------------------------------------------

/// How [`resize`] maps the source frame into the requested box.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum FitMode {
    /// Distort to exactly `width`×`height`, ignoring aspect ratio.
    /// When only one dimension is given the other keeps the source
    /// aspect ratio (ffmpeg `scale=-1` semantics).
    Stretch,
    /// Scale down so the whole frame fits inside the box, preserving
    /// aspect ratio. Output may be smaller than requested on one axis.
    /// Requires both dimensions.
    #[default]
    FitInside,
    /// Scale up so the frame covers the box, then center-crop to
    /// exactly `width`×`height`. Requires both dimensions.
    ///
    /// Note: [`FitMode::Cover`] currently produces identical output —
    /// both mean "fill the box, crop the overflow". A future revision
    /// may give `Cover` focal-point control.
    FitOutside,
    /// Same filter chain as [`FitMode::FitOutside`]: scale to cover
    /// then crop to the requested box.
    Cover,
}

/// Anchor position for [`pad`].
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum PadPosition {
    /// Centered on both axes.
    #[default]
    Center,
    TopLeft,
    TopRight,
    BottomLeft,
    BottomRight,
    /// Centered horizontally, flush to the top.
    Top,
    /// Centered horizontally, flush to the bottom.
    Bottom,
    /// Centered vertically, flush to the left.
    Left,
    /// Centered vertically, flush to the right.
    Right,
}

/// Options for [`resize`].
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ResizeOptions {
    /// Target width in pixels. `None` keeps aspect on that axis
    /// (only meaningful for [`FitMode::Stretch`]; other modes require
    /// both dimensions).
    pub width: Option<u32>,
    /// Target height in pixels.
    pub height: Option<u32>,
    /// Fit strategy.
    #[serde(default)]
    pub fit: FitMode,
    /// Reserved for API uniformity — geometry ops always re-encode,
    /// so this flag currently has no effect.
    #[serde(default)]
    pub copy_safe: bool,
}

/// Options for [`crop`]. Offsets are in source pixels.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct CropOptions {
    /// Left offset. Must be `>= 0`.
    pub x: i64,
    /// Top offset. Must be `>= 0`.
    pub y: i64,
    /// Crop width. `None` crops to the right/bottom edge (`iw`/`ih`).
    pub width: Option<u32>,
    /// Crop height. `None` crops to the right/bottom edge.
    pub height: Option<u32>,
    /// Reserved — see [`ResizeOptions::copy_safe`].
    #[serde(default)]
    pub copy_safe: bool,
}

impl Default for CropOptions {
    fn default() -> Self {
        Self {
            x: 0,
            y: 0,
            width: None,
            height: None,
            copy_safe: false,
        }
    }
}

/// Options for [`pad`].
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct PadOptions {
    /// Output canvas width in pixels. Must be `> 0`.
    pub width: u32,
    /// Output canvas height in pixels. Must be `> 0`.
    pub height: u32,
    /// Where the source frame sits on the canvas.
    #[serde(default)]
    pub position: PadPosition,
    /// Pad color: ffmpeg color name (`"black"`) or `0xRRGGBB`.
    /// Must not contain `,` or `:` (filter-graph metacharacters).
    #[serde(default = "default_pad_color")]
    pub color: String,
    /// Reserved — see [`ResizeOptions::copy_safe`].
    #[serde(default)]
    pub copy_safe: bool,
}

fn default_pad_color() -> String {
    "black".to_string()
}

/// Options for [`letterbox`] / [`pillarbox`].
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct LetterboxOptions {
    /// Target frame width in pixels. Must be `> 0`.
    pub width: u32,
    /// Target frame height in pixels. Must be `> 0`.
    pub height: u32,
    /// Bar color (see [`PadOptions::color`]).
    #[serde(default = "default_pad_color")]
    pub color: String,
    /// Reserved — see [`ResizeOptions::copy_safe`].
    #[serde(default)]
    pub copy_safe: bool,
}

/// Options for [`remove_black_borders`], mapped onto the ffmpeg
/// `cropdetect` filter parameters.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct BlackBorderOptions {
    /// Luma difference below which a pixel counts as "black"
    /// (0–255). Default 24.
    #[serde(default = "default_border_limit")]
    pub limit: u8,
    /// Cropdetect rounding: must be a power of two (1, 2, 4, ...).
    /// Default 2 keeps chroma-subsampled dimensions even.
    #[serde(default = "default_border_round")]
    pub round: u32,
    /// Frames between re-detections (0 = continuous). Default 0.
    #[serde(default)]
    pub reset: u32,
    /// Reserved — see [`ResizeOptions::copy_safe`].
    #[serde(default)]
    pub copy_safe: bool,
}

impl Default for BlackBorderOptions {
    fn default() -> Self {
        Self {
            limit: default_border_limit(),
            round: default_border_round(),
            reset: 0,
            copy_safe: false,
        }
    }
}

fn default_border_limit() -> u8 {
    24
}
fn default_border_round() -> u32 {
    2
}

// ---------------------------------------------------------------------------
// Public ops
// ---------------------------------------------------------------------------

/// Resize a video. See [`FitMode`] for the meaning of each strategy.
///
/// Output is always `output.mp4` (H.264/AAC, see module docs).
pub fn resize(
    input: &MediaInput,
    options: &ResizeOptions,
    limits: &MediaLimits,
    rt: &Runtime,
) -> MediaResult<MediaOutput> {
    let (w, h) = (options.width, options.height);
    if w.is_none() && h.is_none() {
        return Err(MediaError::InvalidInput(
            "resize requires at least one of width/height".to_string(),
        ));
    }
    for (name, v) in [("width", w), ("height", h)] {
        if let Some(0) = v {
            return Err(MediaError::InvalidInput(format!(
                "resize {name} must be > 0"
            )));
        }
    }
    let needs_box = matches!(
        options.fit,
        FitMode::FitInside | FitMode::FitOutside | FitMode::Cover
    );
    if needs_box && (w.is_none() || h.is_none()) {
        return Err(MediaError::InvalidInput(format!(
            "{:?} requires both width and height",
            options.fit
        )));
    }

    let vf = match options.fit {
        FitMode::Stretch => format!(
            "scale={}:{}",
            w.map(|v| v.to_string()).unwrap_or_else(|| "-1".into()),
            h.map(|v| v.to_string()).unwrap_or_else(|| "-1".into())
        ),
        FitMode::FitInside => format!(
            "scale={}:{}:force_original_aspect_ratio=decrease",
            w.unwrap(),
            h.unwrap()
        ),
        FitMode::FitOutside | FitMode::Cover => format!(
            "scale={}:{}:force_original_aspect_ratio=increase,crop={}:{}",
            w.unwrap(),
            h.unwrap(),
            w.unwrap(),
            h.unwrap()
        ),
    };
    run_video_op(input, &["scale", "crop"], &vf, limits, rt)
}

/// Crop a video to a rectangle. `width`/`height` of `None` extend to
/// the frame edge; `x`/`y` must be non-negative.
pub fn crop(
    input: &MediaInput,
    options: &CropOptions,
    limits: &MediaLimits,
    rt: &Runtime,
) -> MediaResult<MediaOutput> {
    if options.x < 0 || options.y < 0 {
        return Err(MediaError::InvalidInput(
            "crop x/y must be >= 0".to_string(),
        ));
    }
    for (name, v) in [("width", options.width), ("height", options.height)] {
        if let Some(0) = v {
            return Err(MediaError::InvalidInput(format!("crop {name} must be > 0")));
        }
    }
    let vf = format!(
        "crop={}:{}:{}:{}",
        options
            .width
            .map(|v| v.to_string())
            .unwrap_or_else(|| "iw".into()),
        options
            .height
            .map(|v| v.to_string())
            .unwrap_or_else(|| "ih".into()),
        options.x,
        options.y
    );
    run_video_op(input, &["crop"], &vf, limits, rt)
}

/// Pad a video onto a `width`×`height` canvas at `position`, filled
/// with `color`. The canvas may be smaller than the source — the
/// `pad` filter then anchors per `position` and clips the overflow.
pub fn pad(
    input: &MediaInput,
    options: &PadOptions,
    limits: &MediaLimits,
    rt: &Runtime,
) -> MediaResult<MediaOutput> {
    if options.width == 0 || options.height == 0 {
        return Err(MediaError::InvalidInput(
            "pad width/height must be > 0".to_string(),
        ));
    }
    validate_color(&options.color)?;
    let (x, y) = position_expr(options.position);
    let vf = format!(
        "pad={}:{}:{}:{}:{}",
        options.width, options.height, x, y, options.color
    );
    run_video_op(input, &["pad"], &vf, limits, rt)
}

/// Fit the video inside `width`×`height` preserving aspect ratio and
/// pad the remaining area with `color`, centered — i.e. classic
/// letterboxing when the frame is wider than the video.
///
/// Implemented as a `scale` (down to the largest even-sized frame
/// fitting the box) followed by a centered `pad` to the exact target
/// size. [`pillarbox`] is the same operation; the names describe the
/// typical bar orientation, not different mechanics.
pub fn letterbox(
    input: &MediaInput,
    options: &LetterboxOptions,
    limits: &MediaLimits,
    rt: &Runtime,
) -> MediaResult<MediaOutput> {
    fit_and_pad(input, options, limits, rt)
}

/// Fit the video inside `width`×`height` preserving aspect ratio and
/// pad the remaining area — classic pillarboxing (bars left/right)
/// when the frame is taller than the video. Mechanically identical to
/// [`letterbox`]; see its docs.
pub fn pillarbox(
    input: &MediaInput,
    options: &LetterboxOptions,
    limits: &MediaLimits,
    rt: &Runtime,
) -> MediaResult<MediaOutput> {
    fit_and_pad(input, options, limits, rt)
}

/// Remove black borders detected by ffmpeg's `cropdetect` filter.
///
/// Two passes: first `ffmpeg -vf cropdetect` decodes the input and
/// reports `crop=w:h:x:y` on stderr; the last reported region is then
/// applied as a real [`crop`]. Fails with [`MediaError::InvalidInput`]
/// when no crop region is detected (e.g. borderless content).
///
/// Detection parameters are configurable via [`BlackBorderOptions`].
pub fn remove_black_borders(
    input: &MediaInput,
    options: &BlackBorderOptions,
    limits: &MediaLimits,
    rt: &Runtime,
) -> MediaResult<MediaOutput> {
    if !options.round.is_power_of_two() {
        return Err(MediaError::InvalidInput(
            "cropdetect round must be a power of two".to_string(),
        ));
    }
    validate_input_size(input, limits)?;
    let caps = rt.capabilities()?;
    require_filter(caps, "cropdetect")?;

    let (dir, in_path) = stage_input(input)?;
    let vf = format!(
        "cropdetect=limit={}:round={}:reset={}",
        options.limit, options.round, options.reset
    );
    let argv = build_probe_argv(&in_path, &vf);
    let out = process::run_checked("ffmpeg", &rt.ffmpeg, &argv, None, limits)?;

    let region = parse_last_crop(&out.stderr).ok_or_else(|| {
        MediaError::InvalidInput(
            "cropdetect reported no crop region (no black borders found?)".to_string(),
        )
    })?;

    let crop_opts = CropOptions {
        x: region.2 as i64,
        y: region.3 as i64,
        width: Some(region.0),
        height: Some(region.1),
        copy_safe: options.copy_safe,
    };
    let vf = format!("crop={}:{}:{}:{}", region.0, region.1, region.2, region.3);
    transcode(&dir, &in_path, caps, &["crop"], &vf, limits, rt, &[]).map(|mut o| {
        o.warnings.push(format!(
            "black borders removed via cropdetect: crop={}x{}+{}+{}",
            crop_opts.width.unwrap(),
            crop_opts.height.unwrap(),
            crop_opts.x,
            crop_opts.y
        ));
        o
    })
}

// ---------------------------------------------------------------------------
// Shared helpers
// ---------------------------------------------------------------------------

/// Shared `letterbox`/`pillarbox` implementation: probe the source
/// size, compute the largest even-sized frame that fits the target
/// box, then `scale` + centered `pad`.
fn fit_and_pad(
    input: &MediaInput,
    options: &LetterboxOptions,
    limits: &MediaLimits,
    rt: &Runtime,
) -> MediaResult<MediaOutput> {
    if options.width == 0 || options.height == 0 {
        return Err(MediaError::InvalidInput(
            "letterbox/pillarbox width/height must be > 0".to_string(),
        ));
    }
    validate_color(&options.color)?;
    validate_input_size(input, limits)?;
    let caps = rt.capabilities()?;
    require_filters(caps, &["scale", "pad"])?;

    let (dir, in_path) = stage_input(input)?;
    let (iw, ih) = probe_video_size(&in_path, rt, limits)?;

    // Largest frame <= target box preserving aspect ratio, forced to
    // even dimensions for yuv420 chroma.
    let scale = (options.width as f64 / iw as f64).min(options.height as f64 / ih as f64);
    let sw = even_floor((iw as f64 * scale) as u64).max(2);
    let sh = even_floor((ih as f64 * scale) as u64).max(2);
    if sw > options.width as u64 || sh > options.height as u64 {
        return Err(MediaError::InvalidInput(format!(
            "target {}x{} too small for source {}x{}",
            options.width, options.height, iw, ih
        )));
    }

    let vf = format!(
        "scale={}:{}:force_original_aspect_ratio=disable,pad={}:{}:(ow-iw)/2:(oh-ih)/2:{}",
        sw, sh, options.width, options.height, options.color
    );
    transcode(
        &dir,
        &in_path,
        caps,
        &["scale", "pad"],
        &vf,
        limits,
        rt,
        &[],
    )
}

/// Round down to an even integer (>= 2 guaranteed by caller).
fn even_floor(v: u64) -> u64 {
    v & !1
}

/// Run a geometry op end-to-end: stage input, verify filters/encoder,
/// build the `-vf` chain, transcode, read output.
fn run_video_op(
    input: &MediaInput,
    filters: &[&str],
    vf: &str,
    limits: &MediaLimits,
    rt: &Runtime,
) -> MediaResult<MediaOutput> {
    validate_input_size(input, limits)?;
    let caps = rt.capabilities()?;
    require_filters(caps, filters)?;

    let (dir, in_path) = stage_input(input)?;
    transcode(&dir, &in_path, caps, filters, vf, limits, rt, &[])
}

/// Execute `ffmpeg -vf <vf>` on the staged input and collect the MP4
/// output. `extra_warnings` are appended to the result.
#[allow(clippy::too_many_arguments)]
fn transcode(
    dir: &TempDir,
    in_path: &Path,
    caps: &RuntimeCapabilities,
    filters: &[&str],
    vf: &str,
    limits: &MediaLimits,
    rt: &Runtime,
    extra_warnings: &[&str],
) -> MediaResult<MediaOutput> {
    let _ = filters; // filters already verified by caller; kept for clarity
    let (encoder, fallback) = pick_video_encoder(caps)?;
    require_encoder(caps, "aac")?;
    require_muxer(caps, "mp4")?;

    let out_path = dir.file("output.mp4");
    let mut argv: Vec<OsString> = args(&[
        "-y",
        "-hide_banner",
        "-i",
        "",
        "-vf",
        "",
        "-map",
        "0:v:0",
        "-map",
        "0:a?",
        "-sn",
        "-dn",
        "-c:v",
        "",
        "-c:a",
        "aac",
        "-movflags",
        "+faststart",
        "",
    ])
    .into_iter()
    .collect();
    // Fill placeholders: argv positions are fixed above.
    argv[4] = in_path.as_os_str().to_os_string();
    argv[6] = OsString::from(vf);
    argv[13] = OsString::from(encoder);
    if encoder == "libx264" {
        argv.splice(14..14, args(&["-crf", "23", "-preset", "veryfast"]));
    }
    // Last empty slot is the output path (now shifted if we spliced).
    let last = argv.len() - 1;
    argv[last] = out_path.as_os_str().to_os_string();

    process::run_checked("ffmpeg", &rt.ffmpeg, &argv, None, limits)?;

    let bytes = dir.read_limited("output.mp4", limits.max_output_bytes)?;
    let mut out = MediaOutput::single("output.mp4", bytes);
    if fallback {
        out.warnings.push(format!(
            "libx264 unavailable in this FFmpeg build; used {encoder} instead"
        ));
    }
    for w in extra_warnings {
        out.warnings.push((*w).to_string());
    }
    Ok(out)
}

/// Stage `input.data` into a fresh [`TempDir`] under its real file
/// name (extension matters for demuxer probing).
fn stage_input(input: &MediaInput) -> MediaResult<(TempDir, PathBuf)> {
    let dir = TempDir::new("geom")?;
    let name = Path::new(&input.name)
        .file_name()
        .and_then(|s| s.to_str())
        .filter(|s| !s.is_empty())
        .unwrap_or("input.mp4")
        .to_string();
    let path = dir.write(&name, &input.data)?;
    Ok((dir, path))
}

/// Enforce `max_input_bytes` on the input buffer.
fn validate_input_size(input: &MediaInput, limits: &MediaLimits) -> MediaResult<()> {
    if let Some(max) = limits.max_input_bytes
        && input.data.len() as u64 > max
    {
        return Err(MediaError::LimitExceeded(format!(
            "input {} is {} bytes, exceeding max_input_bytes {max}",
            input.name,
            input.data.len()
        )));
    }
    Ok(())
}

/// Verify every required filter exists in this FFmpeg build.
fn require_filters(caps: &RuntimeCapabilities, names: &[&str]) -> MediaResult<()> {
    for name in names {
        require_filter(caps, name)?;
    }
    Ok(())
}

fn require_filter(caps: &RuntimeCapabilities, name: &str) -> MediaResult<()> {
    if caps.has_filter(name) {
        Ok(())
    } else {
        Err(MediaError::Unsupported(format!(
            "ffmpeg filter '{name}' not available in this build"
        )))
    }
}

fn require_encoder(caps: &RuntimeCapabilities, name: &str) -> MediaResult<()> {
    if caps.has_encoder(name) {
        Ok(())
    } else {
        Err(MediaError::Unsupported(format!(
            "ffmpeg encoder '{name}' not available in this build"
        )))
    }
}

fn require_muxer(caps: &RuntimeCapabilities, name: &str) -> MediaResult<()> {
    if caps.has_muxer(name) {
        Ok(())
    } else {
        Err(MediaError::Unsupported(format!(
            "ffmpeg muxer '{name}' not available in this build"
        )))
    }
}

/// Pick the H.264 encoder: prefer `libx264` (crf23/veryfast applied by
/// caller), fall back to `libopenh264`. Returns `(name, is_fallback)`.
fn pick_video_encoder(caps: &RuntimeCapabilities) -> MediaResult<(&'static str, bool)> {
    if caps.has_encoder("libx264") {
        Ok(("libx264", false))
    } else if caps.has_encoder("libopenh264") {
        Ok(("libopenh264", true))
    } else {
        Err(MediaError::Unsupported(
            "no usable H.264 encoder (libx264/libopenh264) in this FFmpeg build".to_string(),
        ))
    }
}

/// Probe the first video stream's pixel dimensions via ffprobe.
fn probe_video_size(path: &Path, rt: &Runtime, limits: &MediaLimits) -> MediaResult<(u32, u32)> {
    let argv = args(&[
        "-v",
        "error",
        "-select_streams",
        "v:0",
        "-show_entries",
        "stream=width,height",
        "-of",
        "json",
    ])
    .into_iter()
    .chain(std::iter::once(OsString::from(path)))
    .collect::<Vec<_>>();
    let out = process::run_checked("ffprobe", &rt.ffprobe, &argv, None, limits)?;
    let report: ProbeReport = serde_json::from_slice(&out.stdout)
        .map_err(|e| MediaError::InvalidInput(format!("ffprobe JSON parse failed: {e}")))?;
    let stream = report
        .streams
        .iter()
        .find(|s| s.kind() == Some("video"))
        .ok_or_else(|| MediaError::InvalidInput("no video stream found".to_string()))?;
    let w = stream
        .width
        .ok_or_else(|| MediaError::InvalidInput("video stream has no width".to_string()))?;
    let h = stream
        .height
        .ok_or_else(|| MediaError::InvalidInput("video stream has no height".to_string()))?;
    Ok((w, h))
}

/// ffmpeg argv for the cropdetect pass: decode-only, output to null.
fn build_probe_argv(in_path: &Path, vf: &str) -> Vec<OsString> {
    args(&[
        "-hide_banner",
        "-i",
        "",
        "-vf",
        "",
        "-an",
        "-f",
        "null",
        "-",
    ])
    .into_iter()
    .enumerate()
    .map(|(i, a)| match i {
        2 => in_path.as_os_str().to_os_string(),
        4 => OsString::from(vf),
        _ => a,
    })
    .collect()
}

/// Extract the last `crop=w:h:x:y` region reported by cropdetect on
/// stderr. cropdetect logs one line per detected change; the tail of
/// stderr (kept by [`process::run`]) holds the final frames' verdict,
/// which is what we apply.
fn parse_last_crop(stderr: &str) -> Option<(u32, u32, u32, u32)> {
    let idx = stderr.rfind("crop=")?;
    let rest = &stderr[idx + "crop=".len()..];
    let end = rest
        .find(|c: char| !(c.is_ascii_digit() || c == ':'))
        .unwrap_or(rest.len());
    let mut parts = rest[..end].split(':');
    let w: u32 = parts.next()?.parse().ok()?;
    let h: u32 = parts.next()?.parse().ok()?;
    let x: u32 = parts.next()?.parse().ok()?;
    let y: u32 = parts.next()?.parse().ok()?;
    if w == 0 || h == 0 {
        return None;
    }
    Some((w, h, x, y))
}

/// Map a [`PadPosition`] to pad-filter `x:y` expressions. `ow`/`oh`
/// are the output canvas, `iw`/`ih` the incoming frame.
fn position_expr(pos: PadPosition) -> (&'static str, &'static str) {
    match pos {
        PadPosition::Center => ("(ow-iw)/2", "(oh-ih)/2"),
        PadPosition::TopLeft => ("0", "0"),
        PadPosition::TopRight => ("ow-iw", "0"),
        PadPosition::BottomLeft => ("0", "oh-ih"),
        PadPosition::BottomRight => ("ow-iw", "oh-ih"),
        PadPosition::Top => ("(ow-iw)/2", "0"),
        PadPosition::Bottom => ("(ow-iw)/2", "oh-ih"),
        PadPosition::Left => ("0", "(oh-ih)/2"),
        PadPosition::Right => ("ow-iw", "(oh-ih)/2"),
    }
}

/// Reject pad colors containing filter-graph metacharacters.
fn validate_color(color: &str) -> MediaResult<()> {
    if color.is_empty() || color.contains(',') || color.contains(':') {
        return Err(MediaError::InvalidInput(format!(
            "invalid pad color {color:?}: must be non-empty and contain no ',' or ':'"
        )));
    }
    Ok(())
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;
    use std::time::Duration;

    fn rt() -> Runtime {
        Runtime::discover().expect("ffmpeg/ffprobe must be installed")
    }

    fn limits() -> MediaLimits {
        MediaLimits {
            timeout: Duration::from_secs(120),
            ..MediaLimits::default()
        }
    }

    /// Render a lavfi source to MP4 bytes using whichever H.264
    /// encoder this build has.
    fn make_video(lavfi_src: &str) -> Vec<u8> {
        let dir = TempDir::new("geomtest").unwrap();
        let out = dir.file("src.mp4");
        let encoder = if rt().capabilities().unwrap().has_encoder("libx264") {
            "libx264"
        } else {
            "libopenh264"
        };
        let argv = args(&[
            "-y",
            "-hide_banner",
            "-f",
            "lavfi",
            "-i",
            lavfi_src,
            "-t",
            "1",
            "-pix_fmt",
            "yuv420p",
            "-c:v",
            encoder,
            out.to_str().unwrap(),
        ]);
        process::run_checked("ffmpeg", &rt().ffmpeg, &argv, None, &limits()).unwrap();
        std::fs::read(&out).unwrap()
    }

    fn input_of(name: &str, data: Vec<u8>) -> MediaInput {
        MediaInput {
            name: name.to_string(),
            data,
        }
    }

    fn output_bytes(out: &MediaOutput) -> &[u8] {
        &out.primary.as_ref().expect("primary output").bytes
    }

    /// Probe the video dimensions of produced output bytes.
    fn probe_wh(bytes: &[u8]) -> (u32, u32) {
        let dir = TempDir::new("geomprobe").unwrap();
        let p = dir.write("out.mp4", bytes).unwrap();
        let rt = rt();
        probe_video_size(&p, &rt, &limits()).unwrap()
    }

    #[test]
    fn resize_stretch_64_to_32() {
        let data = make_video("testsrc=size=64x64:rate=10");
        let input = input_of("in.mp4", data);
        let out = resize(
            &input,
            &ResizeOptions {
                width: Some(32),
                height: Some(32),
                fit: FitMode::Stretch,
                copy_safe: false,
            },
            &limits(),
            &rt(),
        )
        .unwrap();
        assert_eq!(probe_wh(output_bytes(&out)), (32, 32));
    }

    #[test]
    fn resize_fit_inside_preserves_aspect() {
        let data = make_video("testsrc=size=64x64:rate=10");
        let input = input_of("in.mp4", data);
        let out = resize(
            &input,
            &ResizeOptions {
                width: Some(128),
                height: Some(32),
                fit: FitMode::FitInside,
                copy_safe: false,
            },
            &limits(),
            &rt(),
        )
        .unwrap();
        // 64x64 fit inside 128x32 -> 32x32
        assert_eq!(probe_wh(output_bytes(&out)), (32, 32));
    }

    #[test]
    fn crop_center_square() {
        let data = make_video("testsrc=size=64x64:rate=10");
        let input = input_of("in.mp4", data);
        let out = crop(
            &input,
            &CropOptions {
                x: 16,
                y: 16,
                width: Some(32),
                height: Some(32),
                copy_safe: false,
            },
            &limits(),
            &rt(),
        )
        .unwrap();
        assert_eq!(probe_wh(output_bytes(&out)), (32, 32));
    }

    #[test]
    fn pad_to_128x128_centered() {
        let data = make_video("testsrc=size=64x64:rate=10");
        let input = input_of("in.mp4", data);
        let out = pad(
            &input,
            &PadOptions {
                width: 128,
                height: 128,
                position: PadPosition::Center,
                color: "black".to_string(),
                copy_safe: false,
            },
            &limits(),
            &rt(),
        )
        .unwrap();
        assert_eq!(probe_wh(output_bytes(&out)), (128, 128));
    }

    #[test]
    fn remove_black_borders_on_pillarboxed() {
        // 64x64 content pillarboxed into 128x64 with black side bars.
        let data = make_video("testsrc=size=64x64:rate=10,pad=128:64:0:0:black");
        let input = input_of("in.mp4", data);
        let out =
            remove_black_borders(&input, &BlackBorderOptions::default(), &limits(), &rt()).unwrap();
        assert_eq!(probe_wh(output_bytes(&out)), (64, 64));
        assert!(
            out.warnings.iter().any(|w| w.contains("cropdetect")),
            "expected cropdetect warning, got {:?}",
            out.warnings
        );
    }

    #[test]
    fn letterbox_pads_to_target() {
        let data = make_video("testsrc=size=64x64:rate=10");
        let input = input_of("in.mp4", data);
        let out = letterbox(
            &input,
            &LetterboxOptions {
                width: 128,
                height: 64,
                color: "black".to_string(),
                copy_safe: false,
            },
            &limits(),
            &rt(),
        )
        .unwrap();
        assert_eq!(probe_wh(output_bytes(&out)), (128, 64));
    }
}
