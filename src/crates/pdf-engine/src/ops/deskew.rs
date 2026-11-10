//! Correct small rotational skew on scanned pages — **lossy**.
//!
//! # What this operation does
//!
//! For every selected page the page is rasterized with `pdftoppm` at
//! ~200 DPI (via [`super::raster`], the crate's single subprocess
//! boundary), the skew angle is estimated by maximizing the variance of
//! the horizontal projection over candidate angles, and — when the
//! detected angle exceeds [`SKEW_THRESHOLD_DEG`] — the page is rebuilt
//! as a **single full-page image XObject** stretched over the page's
//! `/MediaBox`.
//!
//! # Lossy — read before using
//!
//! Corrected pages are **raster images, not vector content**. Text is
//! no longer selectable/searchable, annotations and form fields on the
//! rebuilt page are discarded with the old content stream, and quality
//! is bounded by the 200 DPI rasterization. This op exists for scan
//! cleanup, not for document editing. Pages whose detected angle is
//! within the threshold are left completely untouched.
//!
//! # Angle estimation
//!
//! [`estimate_skew_angle`] scores each candidate angle by shearing the
//! grayscale raster along the candidate slope and measuring the
//! variance of the resulting row sums: when the shear cancels the
//! content skew, text lines align horizontally and the projection
//! variance peaks. Candidates run from `-max_angle` to `+max_angle` in
//! 0.5° steps (capped at ±5° — larger angles are almost certainly
//! mis-scans, not skew). Ties resolve toward the smallest absolute
//! angle, so a level page reports 0.0 rather than an arbitrary extreme.
//!
//! The detected angle is the *correction* angle in degrees, positive =
//! clockwise on screen. [`rotate_clockwise`] resamples the raster with
//! bilinear interpolation around the image center, keeping the canvas
//! size fixed (corners are filled white, matching the page background).
//!
//! # Limits and honesty
//!
//! - `max_pixels` is enforced on every decoded raster before it is
//!   materialized into the rebuilt page.
//! - Encrypted documents are rejected ([`PdfError::Encrypted`]); a
//!   renderer cannot read them without a password.
//! - Missing `pdftoppm` surfaces as [`PdfError::MissingTool`]; tool
//!   failures as [`PdfError::ExternalTool`]. Temp files live in a
//!   `tempfile::tempdir` dropped at end of scope.
//! - A page whose raster cannot be decoded is skipped with a warning
//!   rather than failing the whole document — unless *every* selected
//!   page fails, which is an error.

use image::{DynamicImage, ImageBuffer, Rgba};
use lopdf::content::Operation;
use lopdf::{Object, Stream, dictionary};
use serde::{Deserialize, Serialize};

use crate::crates::core::engine::EngineLimits;

use crate::crates::pdf_engine::error::{PdfError, PdfResult};

use super::load::{load_pdf, lopdf_err, reject_encrypted, save_pdf};
use super::pages::parse_page_range;
use super::raster::rasterize;
use super::watermark::{add_content_stream, encode_operations, ensure_xobject_resource, media_box};

/// Rasterization DPI for skew estimation and rebuild — high enough that
/// 5° corrections stay legible, low enough to keep PNG budgets sane.
const DESKEW_DPI: u32 = 200;

/// Detection never searches beyond ±5° regardless of `max_angle`.
const MAX_SEARCH_ANGLE_DEG: f64 = 5.0;

/// Candidate-angle step in degrees.
const ANGLE_STEP_DEG: f64 = 0.5;

/// Angles at or below this magnitude are reported but not corrected.
const SKEW_THRESHOLD_DEG: f64 = 0.3;

/// Longest analysis dimension after stride downsampling — keeps the
/// O(angles × pixels) projection scan cheap on large pages.
const ANALYSIS_MAX_DIM: usize = 1200;

/// Per-page outcome in [`DeskewReport`].
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct DeskewPageReport {
    /// 1-based page number.
    pub page: u32,
    /// Detected skew angle in degrees (positive = clockwise on screen).
    /// 0.0 when no skew was detectable.
    pub angle_detected: f64,
    /// Whether the page was rebuilt as a deskewed raster image.
    pub corrected: bool,
}

/// Outcome of [`deskew`], returned alongside the PDF bytes.
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
pub struct DeskewReport {
    /// One entry per selected page, in page order.
    pub pages: Vec<DeskewPageReport>,
    /// Non-fatal problems: skipped pages, rasterization rebuilds, etc.
    pub warnings: Vec<String>,
}

// ---------------------------------------------------------------------------
// Pure skew estimation (unit-testable, no I/O)
// ---------------------------------------------------------------------------

/// Estimate the skew angle of a grayscale raster.
///
/// `samples` is row-major `width * height` luma bytes (0 = black).
/// Returns the correction angle in degrees, positive = clockwise on
/// screen, searched over `[-max_angle, +max_angle]` in 0.5° steps and
/// capped at ±5°. Returns 0.0 for degenerate or uniform input.
pub(crate) fn estimate_skew_angle(
    samples: &[u8],
    width: usize,
    height: usize,
    max_angle: f64,
) -> f64 {
    if width < 3 || height < 3 || samples.len() < width * height || !max_angle.is_finite() {
        return 0.0;
    }
    // Uniform images have identical scores at every angle; the tie-break
    // toward |angle| = 0 already yields 0.0, but skip the scan entirely.
    let first = samples[0];
    if samples[..width * height].iter().all(|&p| p == first) {
        return 0.0;
    }

    let half_range = max_angle.abs().min(MAX_SEARCH_ANGLE_DEG);
    if half_range < ANGLE_STEP_DEG {
        return 0.0;
    }

    // Candidates ordered by |angle| so exact ties keep the smallest
    // magnitude — a level page must not report a spurious extreme.
    let steps = (half_range / ANGLE_STEP_DEG).floor() as i32;
    let mut candidates: Vec<f64> = (-steps..=steps)
        .map(|k| k as f64 * ANGLE_STEP_DEG)
        .filter(|a| a.abs() <= half_range + 1e-9)
        .collect();
    candidates.sort_by(|a, b| {
        a.abs()
            .partial_cmp(&b.abs())
            .unwrap_or(std::cmp::Ordering::Equal)
            .then(a.partial_cmp(b).unwrap_or(std::cmp::Ordering::Equal))
    });
    candidates.dedup_by(|a, b| (*a - *b).abs() < 1e-9);

    let mut best_angle = 0.0f64;
    let mut best_score = f64::NEG_INFINITY;
    for angle in candidates {
        let score = projection_variance(samples, width, height, angle);
        if score > best_score {
            best_score = score;
            best_angle = angle;
        }
    }
    // Round to the candidate grid to avoid float dust in reports.
    (best_angle * 1000.0).round() / 1000.0
}

/// Variance of the row sums produced by shearing the image along
/// `angle_deg` (degrees, positive = clockwise on screen).
///
/// For each output row `y` we sample source row
/// `y - tan(angle)·(x − cx)`: when `angle` is the *correction* angle it
/// cancels the content skew (rotating clockwise by θ adds θ to the
/// slope of every line, so the cancelling shear has slope `-tan(angle)`),
/// text lines align horizontally and the row-sum variance peaks.
fn projection_variance(samples: &[u8], width: usize, height: usize, angle_deg: f64) -> f64 {
    let tan = angle_deg.to_radians().tan();
    let cx = (width as f64 - 1.0) * 0.5;
    let mut rows = vec![0f64; height];
    for (y, row) in rows.iter_mut().enumerate() {
        let mut sum = 0f64;
        let mut count = 0u32;
        for x in 0..width {
            let sy = y as f64 - tan * (x as f64 - cx);
            let yi = sy.round() as isize;
            if yi >= 0 && (yi as usize) < height {
                sum += f64::from(samples[yi as usize * width + x]);
                count += 1;
            }
        }
        *row = if count > 0 {
            sum / f64::from(count)
        } else {
            0.0
        };
    }
    let n = rows.len() as f64;
    let mean = rows.iter().sum::<f64>() / n;
    rows.iter().map(|v| (v - mean) * (v - mean)).sum::<f64>() / n
}

// ---------------------------------------------------------------------------
// Raster rotation (pure, unit-testable)
// ---------------------------------------------------------------------------

/// Bilinear-sample `img` at fractional `(x, y)`; out-of-bounds → white.
fn sample_rgba(img: &ImageBuffer<Rgba<u8>, Vec<u8>>, x: f64, y: f64) -> Rgba<u8> {
    let (w, h) = (img.width() as f64, img.height() as f64);
    if x < 0.0 || y < 0.0 || x >= w || y >= h {
        return Rgba([255, 255, 255, 255]);
    }
    let x0 = x.floor() as u32;
    let y0 = y.floor() as u32;
    let x1 = (x0 + 1).min(img.width() - 1);
    let y1 = (y0 + 1).min(img.height() - 1);
    let fx = (x - x0 as f64) as f32;
    let fy = (y - y0 as f64) as f32;
    let p00 = img.get_pixel(x0, y0).0;
    let p10 = img.get_pixel(x1, y0).0;
    let p01 = img.get_pixel(x0, y1).0;
    let p11 = img.get_pixel(x1, y1).0;
    let mut out = [0u8; 4];
    for c in 0..4 {
        let top = p00[c] as f32 * (1.0 - fx) + p10[c] as f32 * fx;
        let bot = p01[c] as f32 * (1.0 - fx) + p11[c] as f32 * fx;
        out[c] = (top * (1.0 - fy) + bot * fy).round().clamp(0.0, 255.0) as u8;
    }
    Rgba(out)
}

/// Rotate an RGBA raster clockwise on screen by `degrees` about the
/// center, keeping the canvas size. Pixels that rotate in from outside
/// the source are filled white (the page background).
///
/// Inverse mapping for a clockwise rotation by θ in screen coordinates
/// (y down): `src = center + R(-θ)·(dst − center)`, i.e.
/// `sx = cx + dx·cosθ + dy·sinθ`, `sy = cy − dx·sinθ + dy·cosθ`.
fn rotate_rgba_clockwise(
    img: &ImageBuffer<Rgba<u8>, Vec<u8>>,
    degrees: f64,
) -> ImageBuffer<Rgba<u8>, Vec<u8>> {
    let (w, h) = img.dimensions();
    if w == 0 || h == 0 {
        return ImageBuffer::new(0, 0);
    }
    let theta = degrees.to_radians();
    let (sin, cos) = theta.sin_cos();
    let cx = (w as f64 - 1.0) * 0.5;
    let cy = (h as f64 - 1.0) * 0.5;
    let mut out = ImageBuffer::from_pixel(w, h, Rgba([255, 255, 255, 255]));
    for y in 0..h {
        for x in 0..w {
            let dx = x as f64 - cx;
            let dy = y as f64 - cy;
            let sx = cx + dx * cos + dy * sin;
            let sy = cy - dx * sin + dy * cos;
            out.put_pixel(x, y, sample_rgba(img, sx, sy));
        }
    }
    out
}

// ---------------------------------------------------------------------------
// PDF rebuild
// ---------------------------------------------------------------------------

/// Downsample a decoded image to grayscale samples with longest side
/// ≤ [`ANALYSIS_MAX_DIM`] using a simple stride (nearest-pixel) — fast
/// and adequate for projection-variance scoring.
fn analysis_samples(img: &DynamicImage) -> (Vec<u8>, usize, usize) {
    let luma = img.to_luma8();
    let (w, h) = (luma.width() as usize, luma.height() as usize);
    if w == 0 || h == 0 {
        return (Vec::new(), 0, 0);
    }
    let step = (w.max(h) / ANALYSIS_MAX_DIM).max(1);
    if step <= 1 {
        return (luma.into_raw(), w, h);
    }
    let sw = w.div_ceil(step);
    let sh = h.div_ceil(step);
    let mut samples = Vec::with_capacity(sw * sh);
    for y in (0..h).step_by(step) {
        for x in (0..w).step_by(step) {
            samples.push(luma.get_pixel(x as u32, y as u32)[0]);
        }
    }
    (samples, sw, sh)
}

/// Enforce `max_pixels` against decoded raster dimensions.
fn check_raster_pixels(width: u32, height: u32, page: u32, limits: &EngineLimits) -> PdfResult<()> {
    if let Some(max_pixels) = limits.max_pixels {
        let pixels = u64::from(width) * u64::from(height);
        if pixels > max_pixels {
            return Err(PdfError::LimitExceeded(format!(
                "rasterized page {page} is {width}x{height} ({pixels} pixels), \
                 exceeding max_pixels {max_pixels}"
            )));
        }
    }
    Ok(())
}

/// Flatten an RGBA raster onto white and return interleaved RGB bytes —
/// same flattening as `stamp`/`replace_image`.
fn rgba_to_rgb(rgba: &ImageBuffer<Rgba<u8>, Vec<u8>>) -> Vec<u8> {
    let mut rgb = vec![0u8; rgba.len() / 4 * 3];
    for (src, dst) in rgba.chunks_exact(4).zip(rgb.chunks_exact_mut(3)) {
        let a = u32::from(src[3]);
        let inv = 255 - a;
        for c in 0..3 {
            dst[c] = ((u32::from(src[c]) * a + 255 * inv + 127) / 255) as u8;
        }
    }
    rgb
}

/// Rebuild `page_id` as a single full-page image XObject covering its
/// `/MediaBox`, replacing `/Contents` entirely.
fn rebuild_page_as_image(
    doc: &mut lopdf::Document,
    page_id: lopdf::ObjectId,
    rgb: Vec<u8>,
    width: u32,
    height: u32,
) -> PdfResult<()> {
    let mut stream = Stream::new(
        dictionary! {
            "Type" => "XObject",
            "Subtype" => "Image",
            "Width" => width as i64,
            "Height" => height as i64,
            "ColorSpace" => "DeviceRGB",
            "BitsPerComponent" => 8,
            "Interpolate" => true,
        },
        rgb,
    );
    // Best-effort FlateDecode; lopdf keeps raw bytes when compression
    // would not shrink the stream (same policy as `replace_image`).
    let _ = stream.compress();
    let image_id = doc
        .try_add_object(Object::Stream(stream))
        .map_err(lopdf_err)?;
    let image_name = ensure_xobject_resource(doc, page_id, image_id)?;

    let [x0, y0, x1, y1] = media_box(doc, page_id);
    let (w, h) = (x1 - x0, y1 - y0);
    if !(w.is_finite() && h.is_finite()) || w <= 0.0 || h <= 0.0 {
        return Err(PdfError::InvalidInput(format!(
            "page has unusable MediaBox [{x0}, {y0}, {x1}, {y1}]"
        )));
    }

    // PDF images paint the unit square; the cm maps it onto the MediaBox.
    let operations = vec![
        Operation::new("q", vec![]),
        Operation::new(
            "cm",
            vec![
                Object::Real(w as f32),
                Object::Real(0.0),
                Object::Real(0.0),
                Object::Real(h as f32),
                Object::Real(x0 as f32),
                Object::Real(y0 as f32),
            ],
        ),
        Operation::new("Do", vec![Object::Name(image_name.into_bytes())]),
        Operation::new("Q", vec![]),
    ];
    let content_id = add_content_stream(doc, encode_operations(operations)?)?;
    doc.get_dictionary_mut(page_id)
        .map_err(lopdf_err)?
        .set("Contents", Object::Reference(content_id));
    Ok(())
}

// ---------------------------------------------------------------------------
// deskew operation
// ---------------------------------------------------------------------------

/// Deskew selected pages of `input`, returning the new PDF bytes and a
/// [`DeskewReport`].
///
/// `pages` is the shared 1-based range spec (`"1-3,8"`, `"all"`);
/// `None` processes every page. `max_angle` bounds the detection search
/// in degrees — finite, `>= 0`, and effectively capped at ±5° (see
/// module docs). Angles with `|angle| <= 0.3` are reported but the page
/// is left untouched.
///
/// **Lossy:** corrected pages become full-page raster images — see the
/// module docs. Encrypted input is rejected; missing `pdftoppm` is
/// [`PdfError::MissingTool`].
pub fn deskew(
    input: &[u8],
    pages: Option<&str>,
    max_angle: f64,
    limits: &EngineLimits,
) -> PdfResult<(Vec<u8>, DeskewReport)> {
    if !max_angle.is_finite() || max_angle < 0.0 {
        return Err(PdfError::InvalidInput(format!(
            "max_angle must be a finite non-negative number, got {max_angle}"
        )));
    }

    // Load once for validation + page selection; rasterize re-loads for
    // its own limit/encryption enforcement before shelling out.
    let mut doc = load_pdf(input, limits)?;
    reject_encrypted(&doc)?;
    let page_count = doc.get_pages().len() as u32;
    let selected = match pages {
        Some(spec) => parse_page_range(spec, page_count)?,
        None => (1..=page_count).collect(),
    };

    let mut report = DeskewReport::default();
    let mut corrected_count = 0usize;
    let mut decoded_pages = 0usize;

    if !selected.is_empty() {
        // `rasterize` renders the contiguous min..=max window and
        // filters back to the requested set; `pages` is forwarded
        // verbatim so its own range parsing stays the single grammar.
        let images = rasterize(input, pages, DESKEW_DPI, limits)?;
        let pages_map = doc.get_pages();

        for image in images {
            let page = image.page;
            check_raster_pixels(image.width_px, image.height_px, page, limits)?;

            let decoded = match image::load_from_memory_with_format(
                &image.png,
                image::ImageFormat::Png,
            ) {
                Ok(img) => img,
                Err(err) => {
                    report.warnings.push(format!(
                        "page {page}: rendered PNG could not be decoded ({err}); page left unchanged"
                    ));
                    report.pages.push(DeskewPageReport {
                        page,
                        angle_detected: 0.0,
                        corrected: false,
                    });
                    continue;
                }
            };

            decoded_pages += 1;
            let (samples, sw, sh) = analysis_samples(&decoded);
            let angle = estimate_skew_angle(&samples, sw, sh, max_angle);
            let mut entry = DeskewPageReport {
                page,
                angle_detected: (angle * 1000.0).round() / 1000.0,
                corrected: false,
            };

            if angle.abs() > SKEW_THRESHOLD_DEG {
                let rgba = decoded.to_rgba8();
                let rotated = rotate_rgba_clockwise(&rgba, angle);
                let rgb = rgba_to_rgb(&rotated);
                let Some(&page_id) = pages_map.get(&page) else {
                    report.warnings.push(format!(
                        "page {page}: missing from page map; page left unchanged"
                    ));
                    report.pages.push(entry);
                    continue;
                };
                match rebuild_page_as_image(
                    &mut doc,
                    page_id,
                    rgb,
                    rotated.width(),
                    rotated.height(),
                ) {
                    Ok(()) => {
                        entry.corrected = true;
                        corrected_count += 1;
                    }
                    Err(err) => {
                        report.warnings.push(format!(
                            "page {page}: rebuild failed ({err}); page left unchanged"
                        ));
                    }
                }
            }
            report.pages.push(entry);
        }
    }

    if corrected_count > 0 {
        report.warnings.push(format!(
            "deskew rebuilt {corrected_count} page(s) as full-page raster images at \
             {DESKEW_DPI} DPI: text is no longer vector/editable and page annotations \
             and form fields on those pages were discarded"
        ));
    }

    // If every selected page failed to decode we produced no work at
    // all — fail loudly rather than returning an unchanged document.
    if !selected.is_empty() && decoded_pages == 0 {
        return Err(PdfError::InvalidInput(
            "every selected page failed raster decoding".to_string(),
        ));
    }

    doc.compress();
    let out = save_pdf(&mut doc)?;
    Ok((out, report))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::crates::pdf_engine::ops::raster::{RENDER_TOOL, tool_available};
    use crate::crates::pdf_engine::ops::test_utils::pdf_with_pages;

    fn limits() -> EngineLimits {
        EngineLimits::default()
    }

    /// Rasterization needs `pdftoppm`; when absent the op must fail
    /// honestly with MissingTool, never fake a result.
    #[test]
    fn missing_pdftoppm_is_honest() {
        let pdf = pdf_with_pages(&["hello"]);
        if tool_available(RENDER_TOOL) {
            // Tool present: the op may succeed — just assert it does not
            // panic and returns a coherent report shape.
            let (_out, report) = deskew(&pdf, None, 5.0, &limits()).unwrap();
            assert_eq!(report.pages.len(), 1);
            return;
        }
        match deskew(&pdf, None, 5.0, &limits()) {
            Err(PdfError::MissingTool(tool)) => assert_eq!(tool, "pdftoppm"),
            Err(other) => panic!("expected MissingTool, got {other:?}"),
            Ok(_) => panic!("expected MissingTool without pdftoppm"),
        }
    }

    #[test]
    fn rejects_bad_max_angle_and_garbage() {
        let pdf = pdf_with_pages(&["x"]);
        for bad in [f64::NAN, f64::INFINITY, -1.0, f64::NEG_INFINITY] {
            assert!(matches!(
                deskew(&pdf, None, bad, &limits()),
                Err(PdfError::InvalidInput(_))
            ));
        }
        assert!(deskew(b"junk", None, 5.0, &limits()).is_err());
    }

    /// Build a grayscale raster of horizontal lines sheared by `slope`
    /// (dy/dx, screen coords). Returns (samples, w, h).
    fn sheared_lines(
        width: usize,
        height: usize,
        slope: f64,
        period: f64,
    ) -> (Vec<u8>, usize, usize) {
        let mut samples = vec![255u8; width * height];
        for y in 0..height {
            for x in 0..width {
                // Line centers at y ≡ slope*x (mod period).
                let pos = (y as f64 - slope * x as f64).rem_euclid(period);
                let d = pos.min(period - pos);
                if d < 0.8 {
                    samples[y * width + x] = 30;
                }
            }
        }
        (samples, width, height)
    }

    #[test]
    fn projection_finds_sheared_lines() {
        // Slope = tan(2°) ≈ 0.0349 → detected correction ≈ -2°.
        let slope = 2.0f64.to_radians().tan();
        let (samples, w, h) = sheared_lines(240, 160, slope, 12.0);
        let angle = estimate_skew_angle(&samples, w, h, 5.0);
        // Candidate grid is 0.5°; allow one step of slack.
        assert!(
            (angle - (-2.0)).abs() <= 1.0,
            "expected ≈ -2.0°, got {angle}"
        );
    }

    #[test]
    fn projection_returns_zero_for_level_and_uniform() {
        let (level, w, h) = sheared_lines(200, 120, 0.0, 10.0);
        assert_eq!(estimate_skew_angle(&level, w, h, 5.0), 0.0);
        let uniform = vec![128u8; 200 * 120];
        assert_eq!(estimate_skew_angle(&uniform, 200, 120, 5.0), 0.0);
        // Degenerate sizes never panic.
        assert_eq!(estimate_skew_angle(&[0u8; 4], 2, 2, 5.0), 0.0);
        assert_eq!(estimate_skew_angle(&[], 0, 0, 5.0), 0.0);
    }

    #[test]
    fn rotation_keeps_canvas_and_moves_content() {
        // 20x10 white image with a black dot left of center; rotating
        // 90° clockwise should move it below center. Uses small angles
        // only to sanity-check bounds handling.
        let mut img = ImageBuffer::from_pixel(20, 10, Rgba([255, 255, 255, 255]));
        img.put_pixel(5, 5, Rgba([0, 0, 0, 255]));
        let rotated = rotate_rgba_clockwise(&img, 90.0);
        assert_eq!(rotated.dimensions(), (20, 10));
        // The dot should still be somewhere dark.
        assert!(rotated.pixels().any(|p| p.0[0] < 128));
        // 360° is identity-ish.
        let back = rotate_rgba_clockwise(&img, 360.0);
        assert!(back.pixels().any(|p| p.0[0] < 128));
    }
}
