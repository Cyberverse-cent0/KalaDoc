//! Slideshow and image-sequence → video, plus animated-image output
//! (GIF, animated WebP, APNG).

use crate::crates::core::engine::EngineLimits;
use crate::crates::video_engine::error::VideoResult;

/// Build a video from `inputs` (PNG/JPEG/etc. images, in order).
/// `fps` is the output frame rate; `duration_per_image` overrides
/// per-image timing (`None` → `1/fps` each). `target_ext` selects
/// the container (`mp4`, `webm`, `gif`, `webp`, `apng`, `mkv`).
///
/// Inputs may be mixed sizes — the largest dimensions win and smaller
/// frames are centered on black via `pad`/`overlay` as needed.
pub(crate) fn images_to_video(
    inputs: &[Vec<u8>],
    fps: f64,
    duration_per_image: Option<f64>,
    target_ext: &str,
    limits: &EngineLimits,
) -> VideoResult<Vec<u8>> {
    let _ = (inputs, fps, duration_per_image, target_ext, limits);
    Err(crate::crates::video_engine::error::VideoError::InvalidInput(
        "images_to_video not implemented".to_string(),
    ))
}

/// Convert `input` video to an animated GIF. `fps` caps the output
/// frame rate (default 10); `width` scales proportionally (`None` →
/// source width, capped at 640 for sanity).
pub(crate) fn video_to_gif(
    input: &[u8],
    fps: Option<f64>,
    width: Option<u32>,
    limits: &EngineLimits,
) -> VideoResult<Vec<u8>> {
    let _ = (input, fps, width, limits);
    Err(crate::crates::video_engine::error::VideoError::InvalidInput(
        "video_to_gif not implemented".to_string(),
    ))
}

/// Convert `input` video to an animated WebP (`-loop 0`). `fps`
/// optional; quality is fixed at a sane default (libwebp `-quality 75`).
pub(crate) fn video_to_animated_webp(
    input: &[u8],
    fps: Option<f64>,
    limits: &EngineLimits,
) -> VideoResult<Vec<u8>> {
    let _ = (input, fps, limits);
    Err(crate::crates::video_engine::error::VideoError::InvalidInput(
        "video_to_animated_webp not implemented".to_string(),
    ))
}

/// Convert `input` video to APNG (animated PNG) when the build's
/// `apng` muxer/encoder exists; honest `ExternalTool` error
/// otherwise.
pub(crate) fn video_to_apng(input: &[u8], fps: Option<f64>, limits: &EngineLimits) -> VideoResult<Vec<u8>> {
    let _ = (input, fps, limits);
    Err(crate::crates::video_engine::error::VideoError::InvalidInput(
        "video_to_apng not implemented".to_string(),
    ))
}
