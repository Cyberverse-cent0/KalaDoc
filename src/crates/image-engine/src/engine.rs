//! [`ImageEngine`] — the `ProcessingEngine` implementation backed by the
//! in-process `image` crate.
//!
//! All operations are synchronous, CPU-bound functions over in-memory
//! byte buffers (see the `ops` module). They are exposed here as plain
//! methods rather than futures: callers running them on an async
//! runtime should wrap them in `spawn_blocking` themselves. The engine
//! itself holds no mutable state — only an [`EngineLimits`](crate::crates::core::engine::EngineLimits) snapshot —
//! so it is trivially `Send + Sync`.

use crate::crates::core::engine::{
    EngineCapabilities, EngineFuture, EngineHealth, EngineHealthStatus, EngineKind, EngineLimits,
    EngineOperation, ProcessingEngine,
};

use crate::crates::image_engine::error::ImageResult;
use crate::crates::image_engine::ops::{
    self, AdjustOptions, CompressOptions, FlipDirection, ImageFormat, ImageInfo, ImageMetadata,
    ResizeOptions, ValidationReport, WatermarkOptions,
};

/// The raster image processing engine.
///
/// Backed by `image`, which is compiled into this crate — there is no
/// external binary or service to probe, so
/// [`ProcessingEngine::health`] always reports
/// [`EngineHealthStatus::Healthy`].
///
/// # Limits
///
/// [`ImageEngine::new`] uses [`EngineLimits::default`] (no engine-side
/// limits; upstream application limits still apply). Use
/// [`ImageEngine::with_limits`] to enforce `max_input_bytes` and
/// `max_pixels` (the decompression-bomb guard, checked from the header
/// before decoding). `max_pages` and `max_outputs` have no meaning for
/// raster images and are ignored.
#[derive(Debug, Clone)]
pub struct ImageEngine {
    limits: EngineLimits,
}

impl ImageEngine {
    /// Create an engine with no engine-side resource limits.
    pub fn new() -> Self {
        Self::with_limits(EngineLimits::default())
    }

    /// Create an engine enforcing the given [`EngineLimits`](crate::crates::core::engine::EngineLimits).
    pub fn with_limits(limits: EngineLimits) -> Self {
        Self { limits }
    }

    /// The limits this engine enforces.
    pub fn limits(&self) -> &EngineLimits {
        &self.limits
    }

    /// Inspect an image: format, dimensions, pixel count, color type.
    ///
    /// Fully decodes the input so truncated/corrupt data is reported
    /// rather than silently passing. See [`ops::inspect`].
    pub fn inspect(&self, input: &[u8]) -> ImageResult<ImageInfo> {
        ops::inspect(input, &self.limits)
    }

    /// Resize `input` to fit within `options`, preserving aspect ratio.
    ///
    /// See [`ops::resize`].
    pub fn resize(&self, input: &[u8], options: ResizeOptions) -> ImageResult<Vec<u8>> {
        ops::resize(input, options, &self.limits)
    }

    /// Convenience wrapper for [`ImageEngine::resize`] with explicit
    /// optional bounds (`None` = unbounded on that axis).
    pub fn resize_to(
        &self,
        input: &[u8],
        width: Option<u32>,
        height: Option<u32>,
    ) -> ImageResult<Vec<u8>> {
        ops::resize_to(input, width, height, &self.limits)
    }

    /// Convert `input` to the format named by `target`
    /// (`"png"`, `"jpg"`/`"jpeg"`, `"gif"`, `"webp"`, `"bmp"`;
    /// case/whitespace-insensitive).
    ///
    /// See [`ops::convert`].
    pub fn convert(&self, input: &[u8], target: &str) -> ImageResult<Vec<u8>> {
        ops::convert(input, target, &self.limits)
    }

    /// Rotate `input` by `degrees` — must be 90, 180, or 270.
    ///
    /// See [`ops::rotate`].
    pub fn rotate(&self, input: &[u8], degrees: u16) -> ImageResult<Vec<u8>> {
        ops::rotate(input, degrees, &self.limits)
    }

    /// Crop `input` to the rectangle `(x, y, width, height)`.
    ///
    /// See [`ops::crop`].
    #[allow(clippy::too_many_arguments)]
    pub fn crop(
        &self,
        input: &[u8],
        x: u32,
        y: u32,
        width: u32,
        height: u32,
    ) -> ImageResult<Vec<u8>> {
        ops::crop(input, x, y, width, height, &self.limits)
    }

    /// Flip `input` in `direction` (horizontal, vertical, or both).
    ///
    /// See [`ops::flip`].
    pub fn flip(&self, input: &[u8], direction: FlipDirection) -> ImageResult<Vec<u8>> {
        ops::flip(input, direction, &self.limits)
    }

    /// Convenience wrapper for [`ImageEngine::flip`] with an explicit
    /// horizontal flag.
    pub fn flip_mirror(&self, input: &[u8], horizontal: bool) -> ImageResult<Vec<u8>> {
        ops::flip_mirror(input, horizontal, &self.limits)
    }

    /// Apply brightness/contrast/hue adjustments per `options`.
    ///
    /// See [`ops::adjust`].
    pub fn adjust(&self, input: &[u8], options: &AdjustOptions) -> ImageResult<Vec<u8>> {
        ops::adjust(input, options, &self.limits)
    }

    /// Generate a thumbnail fitting within `(max_width, max_height)`.
    ///
    /// Never upscales. See [`ops::thumbnail`].
    pub fn thumbnail(
        &self,
        input: &[u8],
        max_width: u32,
        max_height: u32,
    ) -> ImageResult<Vec<u8>> {
        ops::thumbnail(input, max_width, max_height, &self.limits)
    }

    /// Re-encode `input` with optional format override and quality.
    ///
    /// See [`ops::compress`].
    pub fn compress(&self, input: &[u8], options: CompressOptions) -> ImageResult<Vec<u8>> {
        ops::compress(input, options, &self.limits)
    }

    /// Overlay `mark` onto `input` per `options`.
    ///
    /// See [`ops::watermark`].
    pub fn watermark(
        &self,
        input: &[u8],
        mark: &[u8],
        options: &WatermarkOptions,
    ) -> ImageResult<Vec<u8>> {
        ops::watermark(input, mark, options, &self.limits)
    }

    /// Validate `input`'s header and limits without decoding pixels.
    ///
    /// See [`ops::validate`].
    pub fn validate(&self, input: &[u8]) -> ImageResult<ValidationReport> {
        ops::validate(input, &self.limits)
    }

    /// Extract descriptive metadata for `input`.
    ///
    /// See [`ops::metadata`].
    pub fn metadata(&self, input: &[u8]) -> ImageResult<ImageMetadata> {
        ops::metadata(input, &self.limits)
    }
}

impl Default for ImageEngine {
    fn default() -> Self {
        Self::new()
    }
}

impl ProcessingEngine for ImageEngine {
    fn name(&self) -> &'static str {
        "image"
    }

    fn version(&self) -> &'static str {
        env!("CARGO_PKG_VERSION")
    }

    fn kind(&self) -> EngineKind {
        EngineKind::Image
    }

    fn capabilities(&self) -> EngineCapabilities {
        let formats = ImageFormat::identifiers();
        EngineCapabilities {
            operations: vec![
                EngineOperation::Inspect,
                EngineOperation::Resize,
                EngineOperation::Convert,
                EngineOperation::Rotate,
                EngineOperation::Crop,
                EngineOperation::Adjust,
                EngineOperation::Flip,
                EngineOperation::Thumbnail,
                EngineOperation::Compress,
                EngineOperation::Watermark,
                EngineOperation::Validate,
                EngineOperation::ExtractMetadata,
            ],
            input_formats: formats.clone(),
            output_formats: formats,
            // Advertise only the limits this engine actually enforces:
            // max_pages/max_outputs are meaningless for rasters.
            limits: EngineLimits {
                max_input_bytes: self.limits.max_input_bytes,
                max_pages: None,
                max_pixels: self.limits.max_pixels,
                max_outputs: None,
            },
        }
    }

    fn health<'a>(&'a self) -> EngineFuture<'a, EngineHealth> {
        Box::pin(async move {
            // image is embedded in this crate — there is no external
            // process, service, or license to probe.
            EngineHealth {
                status: EngineHealthStatus::Healthy,
                message: None,
            }
        })
    }
}
