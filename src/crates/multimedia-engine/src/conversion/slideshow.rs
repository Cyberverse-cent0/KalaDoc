//! Slideshow generation: still images → video, with optional audio.
//!
//! Implementation notes:
//!
//! - Inputs are image files (PNG/JPEG bytes) supplied as
//!   [`MediaInput`]s. Each is written into a [`TempDir`] with an
//!   image extension (taken from `MediaInput::name`, defaulting to
//!   `.png`) so ffmpeg's `image2` demuxer picks them up.
//! - Every image is looped for its per-image duration
//!   (`-loop 1 -t <dur>`), decoded, normalized to a common canvas
//!   (`scale` + `pad`, `setsar=1`, `fps`, `yuv420p`), then joined with
//!   the `concat` filter. The filter_complex approach is used rather
//!   than a concat-demuxer list file because it lets us normalize
//!   sizes in the same pass and avoids list-file path escaping.
//! - When `resolution` is `None` the canvas is the first image's
//!   dimensions (probed via ffprobe), so mixed-size inputs still
//!   produce a valid video; every image is letterboxed to fit.
//! - `fps` is the output frame rate. `duration_per_image`, when set,
//!   overrides the `1/fps` default for per-image dwell time.
//! - `audio` bytes are written to a temp file and added as a second
//!   input with `-stream_loop -1` plus `-shortest`, so the audio is
//!   looped as needed and trimmed to the video length.
//! - `transition` (xfade) is **not implemented**: transitions require
//!   per-input timestamp arithmetic and a different filtergraph
//!   topology. Passing `Some(_)` returns [`MediaError::Unsupported`]
//!   rather than silently ignoring the request.

use std::ffi::OsString;
use std::path::{Path, PathBuf};

use crate::crates::multimedia_engine::error::{MediaError, MediaResult};
use crate::crates::multimedia_engine::model::{MediaInput, MediaLimits, MediaOutput};
use crate::crates::multimedia_engine::runtime::discovery::Runtime;
use crate::crates::multimedia_engine::runtime::process;
use crate::crates::multimedia_engine::runtime::tempdir::TempDir;

/// Maximum number of images accepted per slideshow call.
const MAX_IMAGES: usize = 1000;

/// Options for [`slideshow`].
#[derive(Debug, Clone)]
pub struct SlideshowOptions {
    /// Output frame rate in frames per second. Must be finite and
    /// greater than zero. Default: `0.5`.
    pub fps: f64,
    /// Per-image dwell time in seconds. When `Some`, this overrides
    /// the `1 / fps` default. Must be finite and greater than zero.
    pub duration_per_image: Option<f64>,
    /// Target canvas `(width, height)`. Images are scaled to fit
    /// (aspect preserved) and padded to this size. When `None`, the
    /// first image's dimensions are used as the canvas.
    pub resolution: Option<(u32, u32)>,
    /// Output container/muxer name (`"mp4"`, `"webm"`, `"mkv"`, ...).
    /// Default: `"mp4"`.
    pub output_format: String,
    /// Video encoder name. Default: first available of
    /// `libx264`/`libopenh264`/`h264`/`mpeg4` (see
    /// [`Runtime::capabilities`]).
    pub video_codec: Option<String>,
    /// Audio encoder name, used only when `audio` is provided.
    /// Default: `"aac"`.
    pub audio_codec: Option<String>,
    /// Optional audio file bytes (any ffmpeg-decodable container),
    /// looped and trimmed to the video length via `-shortest`.
    pub audio: Option<Vec<u8>>,
    /// Optional xfade transition name (e.g. `"fade"`). **Not
    /// implemented** — any `Some` value returns
    /// [`MediaError::Unsupported`].
    pub transition: Option<String>,
}

impl Default for SlideshowOptions {
    fn default() -> Self {
        Self {
            fps: 0.5,
            duration_per_image: None,
            resolution: None,
            output_format: "mp4".to_string(),
            video_codec: None,
            audio_codec: None,
            audio: None,
            transition: None,
        }
    }
}

/// Generate a video from still images plus optional audio.
///
/// See the module docs for the filtergraph strategy. Returns a
/// [`MediaOutput`] whose primary file is `output.<format>`.
pub fn slideshow(
    images: &[MediaInput],
    options: &SlideshowOptions,
    limits: &MediaLimits,
    rt: &Runtime,
) -> MediaResult<MediaOutput> {
    validate(images, options, limits)?;

    // Transitions are explicitly unsupported — fail honestly.
    if options.transition.is_some() {
        return Err(MediaError::Unsupported(
            "slideshow transitions (xfade) are not implemented; \
             omit `transition` or implement crossfade support"
                .to_string(),
        ));
    }

    let caps = rt.capabilities()?;
    let format = options.output_format.trim().to_lowercase();
    if format.is_empty()
        || !format
            .chars()
            .all(|c| c.is_ascii_alphanumeric() || c == '_')
    {
        return Err(MediaError::InvalidInput(format!(
            "invalid output_format {format:?}"
        )));
    }
    if !caps.has_muxer(&format) {
        return Err(MediaError::Unsupported(format!(
            "output container/muxer {format:?} is not available in this FFmpeg build"
        )));
    }

    const DEFAULT_VIDEO_ENCODERS: &[&str] = &["libx264", "libopenh264", "h264", "mpeg4"];
    let video_codec = match &options.video_codec {
        Some(c) => c.clone(),
        None => DEFAULT_VIDEO_ENCODERS
            .iter()
            .copied()
            .find(|e| caps.has_encoder(e))
            .ok_or_else(|| {
                MediaError::Unsupported(
                    "no usable video encoder (libx264/libopenh264/h264/mpeg4) in this FFmpeg \
                     build"
                        .to_string(),
                )
            })?
            .to_string(),
    };
    if !caps.has_encoder(&video_codec) {
        return Err(MediaError::Unsupported(format!(
            "video codec {video_codec:?} is not available in this FFmpeg build"
        )));
    }
    let audio_codec = if options.audio.is_some() {
        let codec = options
            .audio_codec
            .clone()
            .unwrap_or_else(|| "aac".to_string());
        if !caps.has_encoder(&codec) {
            return Err(MediaError::Unsupported(format!(
                "audio codec {codec:?} is not available in this FFmpeg build"
            )));
        }
        Some(codec)
    } else {
        None
    };

    let tmp = TempDir::new("slideshow")?;
    let fps = options.fps;
    let per_image = options.duration_per_image.unwrap_or_else(|| 1.0 / fps);

    // Write each image input to the temp dir.
    let mut input_paths: Vec<PathBuf> = Vec::with_capacity(images.len());
    for (i, input) in images.iter().enumerate() {
        let name = format!("img_{i:04}{}", image_extension(&input.name));
        let path = tmp.file(&name);
        std::fs::write(&path, &input.data)?;
        input_paths.push(path);
    }

    // Determine the canvas size.
    let (width, height) = match options.resolution {
        Some((w, h)) => (w, h),
        None => probe_dimensions(&input_paths[0], limits, rt)?,
    };

    // Build the filtergraph: normalize each input, then concat.
    let filter = build_filter(images.len(), width, height, fps);

    // ffmpeg argv. `-y` overwrites; `-nostdin` is not needed because
    // stdin is /dev/null via `process::run`.
    let mut argv: Vec<OsString> = Vec::new();
    for path in &input_paths {
        argv.extend(process::args(&[
            "-loop",
            "1",
            "-framerate",
            &fps.to_string(),
            "-t",
            &per_image.to_string(),
            "-i",
        ]));
        argv.push(path.as_os_str().to_os_string());
    }

    // Optional audio input, looped forever; `-shortest` trims it.
    let mut audio_input_index: Option<usize> = None;
    if let Some(audio_bytes) = &options.audio {
        let audio_path = tmp.file("audio.bin");
        std::fs::write(&audio_path, audio_bytes)?;
        audio_input_index = Some(input_paths.len());
        argv.extend(process::args(&["-stream_loop", "-1", "-i"]));
        argv.push(audio_path.as_os_str().to_os_string());
    }

    argv.push(OsString::from("-filter_complex"));
    argv.push(OsString::from(filter));
    argv.push(OsString::from("-map"));
    argv.push(OsString::from("[vout]"));
    if audio_input_index.is_some() {
        let idx = audio_input_index.unwrap();
        argv.push(OsString::from("-map"));
        argv.push(OsString::from(format!("{idx}:a?")));
        argv.push(OsString::from("-c:a"));
        argv.push(OsString::from(audio_codec.unwrap()));
        argv.push(OsString::from("-shortest"));
    }
    argv.push(OsString::from("-c:v"));
    argv.push(OsString::from(video_codec));
    // `-loop 1` image inputs never signal EOF to the concat filter on
    // some FFmpeg builds, hanging the muxer; bound the output
    // explicitly to the computed total duration.
    let total_secs = images.len() as f64 * per_image;
    let total_arg = format!("{total_secs:.3}");
    argv.extend(process::args(&["-t", total_arg.as_str()]));
    argv.push(OsString::from("-y"));

    let out_path = tmp.file(&format!("output.{format}"));
    argv.push(out_path.as_os_str().to_os_string());

    process::run_checked("ffmpeg", &rt.ffmpeg, &argv, None, limits)?;

    let bytes = std::fs::read(&out_path)?;
    if let Some(max) = limits.max_output_bytes
        && bytes.len() as u64 > max
    {
        return Err(MediaError::LimitExceeded(format!(
            "slideshow output is {} bytes, exceeding max_output_bytes {max}",
            bytes.len()
        )));
    }

    let mut output = MediaOutput::single(format!("output.{format}"), bytes);
    if let Some(file) = output.primary.as_mut() {
        file.content_type = Some(match format.as_str() {
            "mp4" | "m4v" => "video/mp4".to_string(),
            "mov" => "video/quicktime".to_string(),
            "webm" => "video/webm".to_string(),
            "mkv" => "video/x-matroska".to_string(),
            _ => String::new(),
        });
        if file.content_type.as_deref() == Some("") {
            file.content_type = None;
        }
    }
    Ok(output)
}

/// Validate inputs and option values before touching the filesystem.
fn validate(
    images: &[MediaInput],
    options: &SlideshowOptions,
    limits: &MediaLimits,
) -> MediaResult<()> {
    if images.is_empty() {
        return Err(MediaError::InvalidInput(
            "slideshow requires at least one image".to_string(),
        ));
    }
    if images.len() > MAX_IMAGES {
        return Err(MediaError::InvalidInput(format!(
            "slideshow accepts at most {MAX_IMAGES} images, got {}",
            images.len()
        )));
    }
    for (i, input) in images.iter().enumerate() {
        if input.data.is_empty() {
            return Err(MediaError::InvalidInput(format!(
                "image {i} ({:?}) is empty",
                input.name
            )));
        }
        if let Some(max) = limits.max_input_bytes
            && input.data.len() as u64 > max
        {
            return Err(MediaError::LimitExceeded(format!(
                "image {i} input is {} bytes, exceeding max_input_bytes {max}",
                input.data.len()
            )));
        }
    }
    if let Some(max) = limits.max_input_bytes
        && let Some(audio) = &options.audio
        && audio.len() as u64 > max
    {
        return Err(MediaError::LimitExceeded(format!(
            "audio input is {} bytes, exceeding max_input_bytes {max}",
            audio.len()
        )));
    }
    if !options.fps.is_finite() || options.fps <= 0.0 {
        return Err(MediaError::InvalidInput(format!(
            "fps must be finite and > 0, got {}",
            options.fps
        )));
    }
    if let Some(d) = options.duration_per_image
        && (!d.is_finite() || d <= 0.0)
    {
        return Err(MediaError::InvalidInput(format!(
            "duration_per_image must be finite and > 0, got {d}"
        )));
    }
    if let Some((w, h)) = options.resolution
        && (w == 0 || h == 0 || w > 16384 || h > 16384)
    {
        return Err(MediaError::InvalidInput(format!(
            "resolution ({w}, {h}) is out of range"
        )));
    }
    Ok(())
}

/// Extract a safe lowercase extension (with dot) from an input name,
/// defaulting to `.png` for image inputs without one.
fn image_extension(name: &str) -> String {
    let ext = Path::new(name)
        .extension()
        .and_then(|e| e.to_str())
        .map(|e| e.to_ascii_lowercase())
        .filter(|e| !e.is_empty() && e.len() <= 8 && e.chars().all(|c| c.is_ascii_alphanumeric()));
    match ext {
        Some(e) => format!(".{e}"),
        None => ".png".to_string(),
    }
}

/// Probe `(width, height)` of the first video stream in `path`.
fn probe_dimensions(path: &Path, limits: &MediaLimits, rt: &Runtime) -> MediaResult<(u32, u32)> {
    let out = process::run_checked(
        "ffprobe",
        &rt.ffprobe,
        &process::args(&[
            "-v",
            "error",
            "-select_streams",
            "v:0",
            "-show_entries",
            "stream=width,height",
            "-of",
            "csv=p=0",
        ])
        .into_iter()
        .chain([path.as_os_str().to_os_string()])
        .collect::<Vec<OsString>>(),
        None,
        limits,
    )?;
    let text = String::from_utf8_lossy(&out.stdout);
    let dims = text
        .lines()
        .next()
        .and_then(|l| {
            let mut it = l.split(',');
            let w: u32 = it.next()?.trim().parse().ok()?;
            let h: u32 = it.next()?.trim().parse().ok()?;
            Some((w, h))
        })
        .ok_or_else(|| {
            MediaError::InvalidInput(format!(
                "could not determine dimensions of {:?}",
                path.file_name().unwrap_or_default()
            ))
        })?;
    if dims.0 == 0 || dims.1 == 0 {
        return Err(MediaError::InvalidInput(
            "probed image has zero dimensions".to_string(),
        ));
    }
    Ok(dims)
}

/// Build the `filter_complex` graph: one normalized branch per image
/// feeding a single `concat`.
fn build_filter(n: usize, width: u32, height: u32, fps: f64) -> String {
    let mut parts: Vec<String> = Vec::with_capacity(n + 1);
    for i in 0..n {
        // scale-to-fit then pad centers the image on the canvas;
        // setsar=1 + format=yuv420p make concat inputs uniform.
        parts.push(format!(
            "[{i}:v:0]scale={width}:{height}:force_original_aspect_ratio=decrease,\
             pad={width}:{height}:(ow-iw)/2:(oh-ih)/2,setsar=1,fps={fps},format=yuv420p[v{i}]"
        ));
    }
    let concat_inputs: String = (0..n).map(|i| format!("[v{i}]")).collect();
    parts.push(format!("{concat_inputs}concat=n={n}:v=1:a=0[vout]"));
    parts.join(";")
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::crates::multimedia_engine::runtime::discovery::Runtime;
    use image::{ImageEncoder as _, Rgb, RgbImage};

    /// Encode a solid-color PNG in memory.
    fn png_bytes(w: u32, h: u32, rgb: [u8; 3]) -> Vec<u8> {
        let img = RgbImage::from_pixel(w, h, Rgb(rgb));
        let mut buf = Vec::new();
        image::codecs::png::PngEncoder::new(&mut buf)
            .write_image(img.as_raw(), w, h, image::ExtendedColorType::Rgb8)
            .expect("png encode");
        buf
    }

    fn input(name: &str, data: Vec<u8>) -> MediaInput {
        MediaInput {
            name: name.to_string(),
            data,
        }
    }

    fn runtime() -> Runtime {
        Runtime::discover().expect("ffmpeg/ffprobe must be installed")
    }

    /// Run ffprobe and return the container duration in seconds.
    fn probe_duration(path: &Path, rt: &Runtime) -> f64 {
        let out = process::run_checked(
            "ffprobe",
            &rt.ffprobe,
            &process::args(&[
                "-v",
                "error",
                "-show_entries",
                "format=duration",
                "-of",
                "csv=p=0",
            ])
            .into_iter()
            .chain([path.as_os_str().to_os_string()])
            .collect::<Vec<OsString>>(),
            None,
            &MediaLimits::default(),
        )
        .expect("ffprobe duration");
        String::from_utf8_lossy(&out.stdout)
            .trim()
            .parse()
            .expect("duration parses")
    }

    /// Write output bytes to a temp file for probing; returns (dir, path).
    fn stage_output(output: &MediaOutput) -> (tempfile::TempDir, PathBuf) {
        let dir = tempfile::tempdir().expect("tempdir");
        let file = output.primary.as_ref().expect("primary output");
        let path = dir.path().join(&file.name);
        std::fs::write(&path, &file.bytes).expect("write output");
        (dir, path)
    }

    #[test]
    fn slideshow_three_images_produces_expected_duration() {
        let rt = runtime();
        let limits = MediaLimits::default();
        let images = vec![
            input("a.png", png_bytes(32, 32, [255, 0, 0])),
            input("b.jpg", png_bytes(32, 32, [0, 255, 0])),
            input("c.png", png_bytes(32, 32, [0, 0, 255])),
        ];
        let options = SlideshowOptions {
            duration_per_image: Some(0.4),
            ..SlideshowOptions::default()
        };

        let output = slideshow(&images, &options, &limits, &rt).expect("slideshow");
        let primary = output.primary.as_ref().expect("primary");
        assert_eq!(primary.name, "output.mp4");
        assert!(!primary.bytes.is_empty());

        let (_dir, path) = stage_output(&output);
        let dur = probe_duration(&path, &rt);
        // 3 images * 0.4s = 1.2s, allow encoder padding slack.
        assert!((dur - 1.2).abs() < 0.35, "expected ~1.2s, got {dur}s");
    }

    #[test]
    fn slideshow_scales_mixed_sizes_to_resolution() {
        let rt = runtime();
        let limits = MediaLimits::default();
        let images = vec![
            input("small.png", png_bytes(16, 16, [255, 0, 0])),
            input("large.png", png_bytes(48, 48, [0, 255, 0])),
            input("wide.png", png_bytes(64, 32, [0, 0, 255])),
        ];
        let options = SlideshowOptions {
            fps: 2.0, // 0.5s per image → 1.5s total
            resolution: Some((64, 64)),
            ..SlideshowOptions::default()
        };

        let output = slideshow(&images, &options, &limits, &rt).expect("slideshow");
        assert!(!output.primary.as_ref().unwrap().bytes.is_empty());

        let (_dir, path) = stage_output(&output);
        let dur = probe_duration(&path, &rt);
        assert!((dur - 1.5).abs() < 0.35, "expected ~1.5s, got {dur}s");

        // Canvas should be exactly 64x64.
        let probe = process::run_checked(
            "ffprobe",
            &rt.ffprobe,
            &process::args(&[
                "-v",
                "error",
                "-select_streams",
                "v:0",
                "-show_entries",
                "stream=width,height",
                "-of",
                "csv=p=0",
            ])
            .into_iter()
            .chain([path.as_os_str().to_os_string()])
            .collect::<Vec<OsString>>(),
            None,
            &limits,
        )
        .expect("ffprobe dims");
        let dims = String::from_utf8_lossy(&probe.stdout);
        assert!(dims.contains("64,64"), "unexpected dims: {dims}");
    }

    #[test]
    fn slideshow_with_audio_trims_audio_to_video_length() {
        let rt = runtime();
        let limits = MediaLimits::default();

        // Generate a 5s WAV tone via ffmpeg lavfi.
        let tmp = TempDir::new("slideshow-test").expect("tempdir");
        let wav_path = tmp.file("tone.wav");
        process::run_checked(
            "ffmpeg",
            &rt.ffmpeg,
            &process::args(&[
                "-y",
                "-f",
                "lavfi",
                "-i",
                "sine=frequency=440:duration=5",
                "-c:a",
                "pcm_s16le",
            ])
            .into_iter()
            .chain([wav_path.as_os_str().to_os_string()])
            .collect::<Vec<OsString>>(),
            None,
            &limits,
        )
        .expect("generate wav");
        let audio = std::fs::read(&wav_path).expect("read wav");

        let images = vec![
            input("a.png", png_bytes(32, 32, [255, 0, 0])),
            input("b.png", png_bytes(32, 32, [0, 0, 255])),
        ];
        let options = SlideshowOptions {
            fps: 1.0, // 1s per image → 2s video
            audio: Some(audio),
            ..SlideshowOptions::default()
        };

        let output = slideshow(&images, &options, &limits, &rt).expect("slideshow");
        let (_dir, path) = stage_output(&output);

        // Duration must match the ~2s video, not the 5s audio.
        let dur = probe_duration(&path, &rt);
        assert!(
            (dur - 2.0).abs() < 0.4,
            "expected ~2.0s (audio trimmed), got {dur}s"
        );

        // Both a video and an audio stream must be present.
        let probe = process::run_checked(
            "ffprobe",
            &rt.ffprobe,
            &process::args(&[
                "-v",
                "error",
                "-show_entries",
                "stream=codec_type",
                "-of",
                "csv=p=0",
            ])
            .into_iter()
            .chain([path.as_os_str().to_os_string()])
            .collect::<Vec<OsString>>(),
            None,
            &limits,
        )
        .expect("ffprobe streams");
        let kinds = String::from_utf8_lossy(&probe.stdout);
        assert!(kinds.contains("video"), "missing video: {kinds}");
        assert!(kinds.contains("audio"), "missing audio: {kinds}");
    }

    #[test]
    fn slideshow_rejects_invalid_inputs() {
        let rt = runtime();
        let limits = MediaLimits::default();
        let options = SlideshowOptions::default();

        // Empty image list.
        let err = slideshow(&[], &options, &limits, &rt).unwrap_err();
        assert!(matches!(err, MediaError::InvalidInput(_)));

        // Unsupported transition.
        let images = vec![input("a.png", png_bytes(8, 8, [0, 0, 0]))];
        let opts = SlideshowOptions {
            transition: Some("fade".to_string()),
            ..SlideshowOptions::default()
        };
        let err = slideshow(&images, &opts, &limits, &rt).unwrap_err();
        assert!(matches!(err, MediaError::Unsupported(_)));

        // Bad fps.
        let opts = SlideshowOptions {
            fps: 0.0,
            ..SlideshowOptions::default()
        };
        let err = slideshow(&images, &opts, &limits, &rt).unwrap_err();
        assert!(matches!(err, MediaError::InvalidInput(_)));
    }
}
