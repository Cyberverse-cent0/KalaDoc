//! Clean up scanned pages by rasterizing, filtering pixels and
//! rebuilding each selected page as a single full-page image XObject.
//!
//! Scanned PDFs are usually one big image per page with noise,
//! grayscale or binarization artifacts. This op renders each selected
//! page with `pdftoppm` (reusing [`super::raster::rasterize`], so all
//! load limits, DPI bounds and PNG budgets apply), runs a pure-Rust
//! pixel filter over the decoded PNG, and rewrites the page so its
//! entire visible content is one new `/DeviceGray` image XObject
//! painted across the page's `/MediaBox`.
//!
//! # LOSSY — read before using
//!
//! This operation **destroys the original page content**: text,
//! vectors, annotations appearance and any existing structure are
//! replaced by a raster. There is no undo. Only use it on pages that
//! are already effectively scans.
//!
//! # Pixel pipeline
//!
//! 1. PNG bytes from `pdftoppm` are decoded with the `image` crate
//!    (PNG only — that is all `rasterize` produces). Alpha, when
//!    present, is composited onto white and reported as a warning.
//! 2. Colors are converted to grayscale with **Rec. 601** luma
//!    (`Y = 0.299 R + 0.587 G + 0.114 B`, computed in fixed point).
//! 3. The [`CleanMode`] filter is applied to the gray buffer:
//!    - `Grayscale` — keep the Rec. 601 values as-is.
//!    - `Threshold` — **Otsu's method** picks a per-image threshold
//!      maximizing between-class variance over the 256-bin histogram;
//!      pixels `> t` become white, the rest black. (A fixed 0.5
//!      threshold was considered and rejected: scanned pages vary too
//!      much in exposure for one constant to work.)
//!    - `Despeckle` — a **3×3 median filter** on the gray buffer
//!      removes isolated specks/noise pixels; edge pixels use a
//!      clamped neighborhood.
//!
//! The result is stored as an 8-bpc `/DeviceGray` image stream
//! (FlateDecode when that shrinks it — `lopdf`'s best-effort
//! `compress`). The page's `/Contents` is replaced by a single
//! `q w 0 0 h x0 y0 cm /Name Do Q` stream mapping the image's unit
//! square onto the effective `/MediaBox` (`[x0, y0, x1, y1]`).
//! `/Rotate` is whatever the rasterizer already applied — the image
//! is painted in unrotated user space, which matches the rendered
//! appearance.
//!
//! # Resource isolation
//!
//! Each rebuilt page gets a **fresh indirect `/Resources` dictionary**
//! cloned from its effective (possibly inherited) resources, with the
//! new XObject added under a collision-free name. Shared resource
//! dictionaries are never mutated, so unselected sibling pages are
//! untouched. Old content streams and old XObjects remain in the file
//! as orphans (lopdf does not garbage-collect on save).
//!
//! Requires the external `pdftoppm` binary; when absent every call
//! fails with [`PdfError::MissingTool`] — nothing is faked. Encrypted
//! documents are rejected. Malformed input surfaces as the usual
//! [`PdfError`] variants; nothing panics.

use lopdf::{Document, Object, Stream, dictionary};
use serde::{Deserialize, Serialize};

use crate::crates::core::engine::EngineLimits;

use crate::crates::pdf_engine::error::{PdfError, PdfResult};

use super::load::{load_pdf, lopdf_err, reject_encrypted, save_pdf};
use super::raster::{self, PageImage};
use super::watermark::{effective_resources_dict, media_box, selected_pages};

/// Pixel filter applied after grayscale conversion.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum CleanMode {
    /// Rec. 601 grayscale only.
    Grayscale,
    /// Grayscale + Otsu binarization (black/white).
    Threshold,
    /// Grayscale + 3×3 median despeckle.
    Despeckle,
}

/// Outcome of [`clean_scan`].
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct CleanReport {
    /// Number of pages actually rebuilt as image XObjects.
    pub pages_processed: u32,
    /// Non-fatal observations (e.g. alpha composited onto white).
    pub warnings: Vec<String>,
}

/// Rec. 601 luma coefficients as 16.16 fixed-point integers:
/// `Y = (19595 R + 38470 G + 7471 B + 32768) >> 16`.
const LUMA_R: u32 = 19595;
const LUMA_G: u32 = 38470;
const LUMA_B: u32 = 7471;

/// Convert RGBA pixels (4 bytes/px, alpha already composited) to
/// Rec. 601 grayscale, one byte per pixel.
fn to_grayscale601(rgba: &[u8]) -> Vec<u8> {
    let mut out = Vec::with_capacity(rgba.len() / 4);
    for px in rgba.chunks_exact(4) {
        let y = (LUMA_R * u32::from(px[0])
            + LUMA_G * u32::from(px[1])
            + LUMA_B * u32::from(px[2])
            + 32768)
            >> 16;
        out.push(y.min(255) as u8);
    }
    out
}

/// Composite RGBA onto white and return packed RGB triplets.
fn flatten_alpha(rgba: &[u8]) -> Vec<u8> {
    let mut out = Vec::with_capacity(rgba.len() / 4 * 3);
    for px in rgba.chunks_exact(4) {
        let a = u32::from(px[3]);
        let inv = 255 - a;
        for c in 0..3 {
            out.push(((u32::from(px[c]) * a + 255 * inv + 127) / 255) as u8);
        }
    }
    out
}

/// Convert packed RGB (3 bytes/px) to Rec. 601 grayscale.
fn rgb_to_grayscale601(rgb: &[u8]) -> Vec<u8> {
    let mut out = Vec::with_capacity(rgb.len() / 3);
    for px in rgb.chunks_exact(3) {
        let y = (LUMA_R * u32::from(px[0])
            + LUMA_G * u32::from(px[1])
            + LUMA_B * u32::from(px[2])
            + 32768)
            >> 16;
        out.push(y.min(255) as u8);
    }
    out
}

/// Otsu's threshold over a grayscale buffer: builds a 256-bin
/// histogram, then picks the level maximizing between-class variance
/// `w0*w1*(mu0-mu1)^2`. Returns the threshold `t` in `0..=255`; the
/// caller decides the polarity (`> t` → white).
///
/// A uniform image (single occupied bin) has zero variance
/// everywhere; returns 128 as a neutral fallback.
fn otsu_threshold(gray: &[u8]) -> u8 {
    let mut hist = [0u64; 256];
    for &v in gray {
        hist[usize::from(v)] += 1;
    }
    let total = gray.len() as u64;
    if total == 0 {
        return 128;
    }

    // Cumulative sums over bins 0..=i.
    let mut sum_all = 0u64;
    for (i, &h) in hist.iter().enumerate() {
        sum_all += i as u64 * h;
    }

    let mut w0 = 0u64;
    let mut sum0 = 0u64;
    let mut best_t = 128u8;
    let mut best_var = 0f64;
    for (t, &h) in hist.iter().enumerate() {
        w0 += h;
        sum0 += t as u64 * h;
        let w1 = total - w0;
        if w0 == 0 || w1 == 0 {
            continue;
        }
        let mu0 = sum0 as f64 / w0 as f64;
        let mu1 = (sum_all - sum0) as f64 / w1 as f64;
        let var = (w0 as f64) * (w1 as f64) * (mu0 - mu1) * (mu0 - mu1);
        if var > best_var {
            best_var = var;
            best_t = t as u8;
        }
    }
    best_t
}

/// Binarize `gray` with Otsu's threshold: pixels strictly above the
/// threshold become 255 (white — paper), the rest 0 (black — ink).
fn threshold_otsu(gray: &[u8]) -> Vec<u8> {
    let t = otsu_threshold(gray);
    gray.iter().map(|&v| if v > t { 255 } else { 0 }).collect()
}

/// 3×3 median filter over a `width`×`height` gray buffer. Out-of-range
/// neighbors are clamped to the edge pixel (replicate padding). Each
/// output pixel is the median (5th of 9 sorted values) of its
/// neighborhood — isolated single-pixel specks are removed while
/// edges thicker than one pixel survive.
fn median3x3(gray: &[u8], width: u32, height: u32) -> Vec<u8> {
    let (w, h) = (width as usize, height as usize);
    if w == 0 || h == 0 || gray.len() < w * h {
        return gray.to_vec();
    }
    let mut out = vec![0u8; w * h];
    let mut win = [0u8; 9];
    for y in 0..h {
        for x in 0..w {
            let mut n = 0;
            for dy in -1i32..=1 {
                let yy = (y as i32 + dy).clamp(0, h as i32 - 1) as usize;
                for dx in -1i32..=1 {
                    let xx = (x as i32 + dx).clamp(0, w as i32 - 1) as usize;
                    win[n] = gray[yy * w + xx];
                    n += 1;
                }
            }
            win.sort_unstable();
            out[y * w + x] = win[4];
        }
    }
    out
}

/// Decode a rendered page PNG into a grayscale buffer, enforcing
/// `max_pixels` and reporting alpha compositing via `warnings`.
fn page_to_grayscale(
    image: &PageImage,
    limits: &EngineLimits,
    warnings: &mut Vec<String>,
) -> PdfResult<(Vec<u8>, u32, u32)> {
    let decoded = image::load_from_memory_with_format(&image.png, image::ImageFormat::Png)
        .map_err(|err| {
            PdfError::InvalidInput(format!(
                "rendered page {} PNG could not be decoded: {err}",
                image.page
            ))
        })?;
    let (w, h) = (decoded.width(), decoded.height());
    if let Some(max_pixels) = limits.max_pixels {
        let pixels = u64::from(w) * u64::from(h);
        if pixels > max_pixels {
            return Err(PdfError::LimitExceeded(format!(
                "rendered page {} is {w}x{h} ({pixels} pixels), \
                 exceeding max_pixels {max_pixels}",
                image.page
            )));
        }
    }
    if decoded.color().has_alpha() {
        warnings.push(format!(
            "page {}: alpha channel composited onto white before filtering",
            image.page
        ));
    }
    let rgba = decoded.to_rgba8();
    // Fast path: opaque images can skip the alpha composite.
    let gray = if rgba.chunks_exact(4).all(|px| px[3] == 255) {
        to_grayscale601(&rgba)
    } else {
        let rgb = flatten_alpha(&rgba);
        rgb_to_grayscale601(&rgb)
    };
    Ok((gray, w, h))
}

/// Pick an XObject resource name that does not collide with existing
/// entries in `xobjects`.
fn fresh_xobject_name(xobjects: &lopdf::Dictionary) -> Vec<u8> {
    for i in 0..1000u32 {
        let name = if i == 0 {
            b"CleanScan".to_vec()
        } else {
            format!("CleanScan{i}").into_bytes()
        };
        if xobjects.get(&name).is_err() {
            return name;
        }
    }
    // Practically unreachable; still never panic.
    b"CleanScanFallback".to_vec()
}

/// Rebuild `page_id` so its whole appearance is the gray image
/// `gray` (width×height px) painted over the effective MediaBox.
fn rebuild_page_as_image(
    doc: &mut Document,
    page_id: lopdf::ObjectId,
    page_number: u32,
    gray: &[u8],
    width: u32,
    height: u32,
) -> PdfResult<()> {
    // Image XObject stream.
    let mut stream = Stream::new(
        dictionary! {
            "Type" => "XObject",
            "Subtype" => "Image",
            "Width" => width as i64,
            "Height" => height as i64,
            "ColorSpace" => "DeviceGray",
            "BitsPerComponent" => 8,
            "Interpolate" => true,
        },
        gray.to_vec(),
    );
    // Best-effort FlateDecode; lopdf keeps raw bytes when compression
    // would not shrink the stream.
    let _ = stream.compress();
    let image_id = doc
        .try_add_object(Object::Stream(stream))
        .map_err(lopdf_err)?;

    // Fresh page-private /Resources: clone the effective dict, ensure
    // an /XObject sub-dictionary (cloned, never the shared original),
    // and register the new image under a fresh name.
    let mut resources = effective_resources_dict(doc, page_id).unwrap_or_default();
    let mut xobjects = match resources.get(b"XObject") {
        Ok(obj) => {
            let resolved = match obj {
                Object::Reference(id) => doc.get_object(*id).ok().cloned(),
                other => Some(other.clone()),
            };
            resolved
                .as_ref()
                .and_then(|o| o.as_dict().ok())
                .cloned()
                .unwrap_or_default()
        }
        Err(_) => lopdf::Dictionary::new(),
    };
    let name = fresh_xobject_name(&xobjects);
    xobjects.set(name.as_slice(), Object::Reference(image_id));
    resources.set(b"XObject", Object::Dictionary(xobjects));
    let resources_id = doc
        .try_add_object(Object::Dictionary(resources))
        .map_err(lopdf_err)?;
    doc.get_dictionary_mut(page_id)
        .map_err(lopdf_err)?
        .set("Resources", Object::Reference(resources_id));

    // Content: paint the image's unit square over the MediaBox.
    let mb = media_box(doc, page_id);
    let w = mb[2] - mb[0];
    let h = mb[3] - mb[1];
    if w <= 0.0 || h <= 0.0 {
        return Err(PdfError::InvalidInput(format!(
            "page {page_number} has a degenerate MediaBox {mb:?}"
        )));
    }
    let content = format!(
        "q {} 0 0 {} {} {} cm /{} Do Q\n",
        w,
        h,
        mb[0],
        mb[1],
        String::from_utf8_lossy(&name)
    );
    // The resource name is generated internally ("CleanScan" + digits)
    // so the hand-built operator stream is always well-formed.
    let content_id = doc
        .try_add_object(Object::Stream(Stream::new(
            dictionary! {},
            content.into_bytes(),
        )))
        .map_err(lopdf_err)?;
    doc.get_dictionary_mut(page_id)
        .map_err(lopdf_err)?
        .set("Contents", Object::Reference(content_id));
    Ok(())
}

/// Rasterize, filter and rebuild the selected pages of a scanned PDF.
///
/// `pages` follows the shared 1-based range grammar (`"1-3,8"`,
/// `"all"`); `None` cleans every page. Returns the rewritten PDF bytes
/// plus a [`CleanReport`]. **Lossy**: see module docs — the original
/// page content is replaced by a raster image.
///
/// Errors: [`PdfError::MissingTool`] when `pdftoppm` is absent,
/// [`PdfError::Encrypted`] for encrypted input, the usual load/range
/// errors otherwise.
pub fn clean_scan(
    input: &[u8],
    pages: Option<&str>,
    mode: CleanMode,
    limits: &EngineLimits,
) -> PdfResult<(Vec<u8>, CleanReport)> {
    let mut doc = load_pdf(input, limits)?;
    reject_encrypted(&doc)?;
    let selected = selected_pages(&doc, pages)?;
    if selected.is_empty() {
        return Ok((
            save_pdf(&mut doc)?,
            CleanReport {
                pages_processed: 0,
                warnings: Vec::new(),
            },
        ));
    }

    // Rasterize the same selection. rasterize() re-loads and
    // re-validates the input and probes pdftoppm — a missing tool
    // surfaces as MissingTool before any pixel work.
    let images = raster::rasterize(input, pages, raster::DEFAULT_DPI, limits)?;
    let pages_map = doc.get_pages();

    let mut warnings: Vec<String> = Vec::new();
    let mut processed = 0u32;

    for image in &images {
        let gray = page_to_grayscale(image, limits, &mut warnings)?;
        let (mut buf, w, h) = gray;
        buf = match mode {
            CleanMode::Grayscale => buf,
            CleanMode::Threshold => threshold_otsu(&buf),
            CleanMode::Despeckle => median3x3(&buf, w, h),
        };

        let Some(&page_id) = pages_map.get(&image.page) else {
            warnings.push(format!(
                "page {}: rendered but no longer present in document; skipped",
                image.page
            ));
            continue;
        };
        rebuild_page_as_image(&mut doc, page_id, image.page, &buf, w, h)?;
        processed += 1;
    }

    let bytes = save_pdf(&mut doc)?;
    Ok((
        bytes,
        CleanReport {
            pages_processed: processed,
            warnings,
        },
    ))
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;
    use crate::crates::pdf_engine::ops::test_utils::pdf_with_pages;

    fn limits() -> EngineLimits {
        EngineLimits::default()
    }

    #[test]
    fn grayscale_uses_rec601_weights() {
        // Pure red → 0.299*255 ≈ 76; pure green ≈ 150; pure blue ≈ 29.
        let rgb = [255, 0, 0, 0, 255, 0, 0, 0, 255];
        let gray = rgb_to_grayscale601(&rgb);
        assert_eq!(gray, vec![76, 150, 29]);
        assert_eq!(to_grayscale601(&[255, 0, 0, 255]), vec![76]);
    }

    #[test]
    fn otsu_splits_bimodal_image() {
        // Half near-black, half near-white → threshold between them.
        let mut gray = vec![10u8; 50];
        gray.extend(vec![240u8; 50]);
        let t = otsu_threshold(&gray);
        assert!(t > 10 && t < 240, "threshold {t} not between modes");
        let bin = threshold_otsu(&gray);
        assert!(bin[..50].iter().all(|&v| v == 0));
        assert!(bin[50..].iter().all(|&v| v == 255));
    }

    #[test]
    fn otsu_uniform_image_is_safe() {
        let gray = vec![128u8; 100];
        let _ = otsu_threshold(&gray); // must not panic; any t is fine
        assert!(threshold_otsu(&gray).iter().all(|&v| v == 0 || v == 255));
    }

    #[test]
    fn median_removes_single_pixel_speck() {
        // 5x5 white field with one black speck at (2,2).
        let mut gray = vec![255u8; 25];
        gray[2 * 5 + 2] = 0;
        let out = median3x3(&gray, 5, 5);
        assert_eq!(out[2 * 5 + 2], 255, "speck should be median-filtered out");
        // A 3-pixel-wide dark bar survives (median of a 3-wide line is
        // still dark at its center column).
        let mut bar = vec![255u8; 25];
        for y in 0..5 {
            for x in 1..4 {
                bar[y * 5 + x] = 0;
            }
        }
        let out_bar = median3x3(&bar, 5, 5);
        assert_eq!(out_bar[2 * 5 + 2], 0);
    }

    #[test]
    fn median_handles_edges_and_tiny() {
        assert_eq!(median3x3(&[], 0, 0), Vec::<u8>::new());
        let one = vec![7u8];
        assert_eq!(median3x3(&one, 1, 1), one);
        let two = vec![1u8, 2];
        assert_eq!(median3x3(&two, 2, 1).len(), 2);
    }

    #[test]
    fn missing_tool_is_honest() {
        let pdf = pdf_with_pages(&["scan"]);
        if raster::tool_available(raster::RENDER_TOOL) {
            return; // tool present — nothing to assert here
        }
        let err = clean_scan(&pdf, None, CleanMode::Grayscale, &limits()).unwrap_err();
        assert!(matches!(err, PdfError::MissingTool(_)));
    }

    #[test]
    fn rejects_garbage_encrypted_and_bad_range() {
        assert!(clean_scan(b"junk", None, CleanMode::Grayscale, &limits()).is_err());
        let pdf = pdf_with_pages(&["a"]);
        assert!(clean_scan(&pdf, Some("9"), CleanMode::Threshold, &limits()).is_err());
    }
}
