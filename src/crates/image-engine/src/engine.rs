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
    self, AdjustOptions, ColorOptions, ColorReplaceOptions, CompressOptions, DeskewResult,
    FlipDirection, ImageFormat, ImageInfo, ImageMetadata, LevelsOptions, OcrOptions, OcrReport,
    PhotocopyOptions, ResizeOptions, ScannerMode, ValidationReport, WatermarkOptions,
};

/// The raster image processing engine.
///
/// Backed by `image`, which is compiled into this crate — there is no
/// external binary or service to probe for the core ops, so
/// [`ProcessingEngine::health`] reports
/// [`EngineHealthStatus::Healthy`] unless `tesseract` (used only by
/// `ocr_image`/`ocr_languages`) is missing from `PATH`, in which case
/// it reports [`EngineHealthStatus::Degraded`].
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

    /// Apply color manipulations per `options`: per-channel gain and
    /// offset, HSV saturation, auto-levels (high-contrast) stretch,
    /// grayscale conversion, and inversion.
    ///
    /// See [`ops::color`].
    pub fn color(&self, input: &[u8], options: &ColorOptions) -> ImageResult<Vec<u8>> {
        ops::color(input, options, &self.limits)
    }

    /// Apply a levels/gamma curve per `options` (input black/white
    /// points plus gamma exponent).
    ///
    /// See [`ops::levels`].
    pub fn levels(&self, input: &[u8], options: &LevelsOptions) -> ImageResult<Vec<u8>> {
        ops::levels(input, options, &self.limits)
    }

    /// Simulate a photocopy/scanner pass per `options`: grayscale,
    /// threshold with optional softness, deterministic grain noise,
    /// and ordered dithering.
    ///
    /// See [`ops::photocopy`].
    pub fn photocopy(&self, input: &[u8], options: &PhotocopyOptions) -> ImageResult<Vec<u8>> {
        ops::photocopy(input, options, &self.limits)
    }

    /// Replace colors near `options.source` with `options.target`,
    /// blended across the `tolerance`/`feather` band.
    ///
    /// See [`ops::replace_color`].
    pub fn replace_color(
        &self,
        input: &[u8],
        options: &ColorReplaceOptions,
    ) -> ImageResult<Vec<u8>> {
        ops::replace_color(input, options, &self.limits)
    }

    /// Convert `input` to CMYK, returning uncompressed CMYK TIFF
    /// bytes (always TIFF regardless of input format — the `image`
    /// crate's TIFF codec is not compiled in, so the op writes the
    /// container itself). Alpha is flattened over white.
    ///
    /// See [`ops::cmyk`].
    pub fn cmyk(&self, input: &[u8]) -> ImageResult<Vec<u8>> {
        ops::cmyk(input, &self.limits)
    }

    /// Generate a thumbnail fitting within `(max_width, max_height)`.
    ///
    /// Never upscales. See [`ops::thumbnail`].
    pub fn thumbnail(&self, input: &[u8], max_width: u32, max_height: u32) -> ImageResult<Vec<u8>> {
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

    /// Reduce impulse noise in `input` by `strength` passes of a 3×3
    /// per-channel median filter. `strength` must be in `1..=5`.
    ///
    /// See [`ops::denoise`].
    pub fn denoise(&self, input: &[u8], strength: u8) -> ImageResult<Vec<u8>> {
        ops::denoise(input, strength, &self.limits)
    }

    /// Normalize the estimated background ("paper") color of `input`
    /// toward white via per-channel multiplicative rescaling. Suited
    /// to scans with uneven illumination.
    ///
    /// See [`ops::clean_background`].
    pub fn clean_background(&self, input: &[u8]) -> ImageResult<Vec<u8>> {
        ops::clean_background(input, &self.limits)
    }

    /// Estimate and remove skew from `input`, returning the deskewed
    /// image bytes plus a [`DeskewReport`] (angle and confidence).
    /// `max_angle` of `None` uses ±5°; must be finite and within
    /// `(0.0, 45.0]`.
    ///
    /// Best-effort, tuned for text pages — check
    /// `report.confidence` before trusting the result.
    ///
    /// See [`ops::deskew`].
    pub fn deskew(&self, input: &[u8], max_angle: Option<f64>) -> ImageResult<DeskewResult> {
        ops::deskew(input, max_angle, &self.limits)
    }

    /// Apply a scanner-cleanup preset to `input` per `mode`
    /// ([`ScannerMode::Clean`], [`ScannerMode::Photocopy`], or
    /// [`ScannerMode::HighContrast`]).
    ///
    /// See [`ops::scanner_preset`].
    pub fn scanner_preset(&self, input: &[u8], mode: ScannerMode) -> ImageResult<Vec<u8>> {
        ops::scanner_preset(input, mode, &self.limits)
    }

    /// Recognize text in `input` via the external `tesseract`
    /// binary, returning an [`OcrReport`] (text, per-word bounding
    /// boxes, confidences).
    ///
    /// Fails with [`ImageError::ExternalTool`](crate::crates::image_engine::ImageError::ExternalTool)
    /// when `tesseract` is not on `PATH` — results are never faked.
    /// `tesseract` runs as a blocking child process; async callers
    /// should wrap this in `spawn_blocking`.
    ///
    /// See [`ops::ocr_image`].
    pub fn ocr_image(&self, input: &[u8], options: &OcrOptions) -> ImageResult<OcrReport> {
        ops::ocr_image(input, options, &self.limits)
    }

    /// List the language codes installed for `tesseract`
    /// (`tesseract --list-langs`). Fails with
    /// [`ImageError::ExternalTool`](crate::crates::image_engine::ImageError::ExternalTool)
    /// when the tool is absent.
    ///
    /// See [`ops::ocr_languages`].
    pub fn ocr_languages(&self) -> ImageResult<Vec<String>> {
        ops::ocr_languages()
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
                // `watermark` is a positioned image overlay with
                // opacity/scale/tile controls — the same compositing
                // primitive `Overlay` names.
                EngineOperation::Overlay,
                EngineOperation::Validate,
                EngineOperation::ExtractMetadata,
                EngineOperation::Ocr,
                EngineOperation::Deskew,
                // `scanner_preset` (background normalization + denoise
                // + contrast/threshold) is the image-side analogue of
                // the document-cleanup `CleanScan` operation.
                EngineOperation::CleanScan,
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
            // image is embedded in this crate — the core ops need
            // nothing external. `ocr` delegates to `tesseract`;
            // report degraded when it is absent so `engine health`/
            // `doctor` see it (the op itself still fails honestly
            // with ExternalTool).
            if ops::tesseract_available() {
                EngineHealth {
                    status: EngineHealthStatus::Healthy,
                    message: None,
                }
            } else {
                EngineHealth {
                    status: EngineHealthStatus::Degraded,
                    message: Some(
                        "external tool not found on PATH (ocr unavailable): tesseract".to_string(),
                    ),
                }
            }
        })
    }
}
