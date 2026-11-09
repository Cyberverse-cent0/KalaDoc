//! CMYK conversion: RGB(A) input → CMYK output as TIFF.
//!
//! # Why a hand-rolled TIFF writer
//!
//! The `image` crate's TIFF codec (including CMYK photometric) is
//! behind its `tiff` cargo feature, which this crate does not enable
//! (see the root `Cargo.toml`). Rather than approximate CMYK as a
//! lossy RGB re-encode, [`cmyk`] writes a minimal, spec-compliant
//! **uncompressed** TIFF directly: baseline TIFF 6.0, little-endian,
//! `PhotometricInterpretation = 5` (Separated), `InkSet = 1` (CMYK),
//! `SamplesPerPixel = 4`, `BitsPerSample = 8` per sample, chunky
//! (`PlanarConfiguration = 1`) single-strip layout. No compression,
//! no tiling, no alpha — readers that support CMYK TIFF (most print
//! pipelines, `image` with the `tiff` feature, libtiff) will read it.
//!
//! # Conversion
//!
//! Standard naive RGB→CMYK per pixel on 8-bit sRGB:
//!
//! ```text
//! k = 1 - max(r, g, b)
//! c = (1 - r - k) / (1 - k)   (and likewise m, y)
//! ```
//!
//! with all channels scaled to 0..=255 and rounded. Alpha, which CMYK
//! cannot represent, is flattened over white before conversion.
//!
//! # Output contract
//!
//! Unlike every other op, [`cmyk`] does **not** preserve the input's
//! sniffed format — the output is always CMYK TIFF bytes regardless
//! of input codec. Input is still decoded and limit-checked exactly
//! like the other ops.

use image::GenericImageView;

use crate::crates::core::engine::EngineLimits;
use crate::crates::image_engine::error::ImageResult;

use super::load::{decode, normalize_8bit};

/// Convert `input` to CMYK, returning uncompressed CMYK TIFF bytes.
///
/// See the module docs for the conversion math and format notes.
/// Alpha is flattened over white. Fails with the usual
/// decode/limit errors for malformed or oversized inputs.
pub fn cmyk(input: &[u8], limits: &EngineLimits) -> ImageResult<Vec<u8>> {
    let decoded = decode(input, limits)?;
    let image = normalize_8bit(decoded.image);

    let (width, height) = image.dimensions();
    // CMYK cannot represent alpha; `to_rgba8` gives a uniform pixel
    // type (RGB sources gain opaque alpha) which `rgb_to_cmyk`
    // flattens over white.
    let rgba = image.to_rgba8();
    let mut pixels = Vec::with_capacity((width as usize) * (height as usize) * 4);
    for px in rgba.pixels() {
        pixels.extend_from_slice(&rgb_to_cmyk(px.0[0], px.0[1], px.0[2], px.0[3]));
    }

    Ok(encode_cmyk_tiff(width, height, &pixels))
}

/// Converts one RGBA pixel to `[c, m, y, k]` (0..=255 each), flattening
/// alpha over white first.
fn rgb_to_cmyk(r: u8, g: u8, b: u8, a: u8) -> [u8; 4] {
    // Composite over white: out = src * a + 255 * (1 - a).
    let flatten = |c: u8| -> f32 {
        let alpha = f32::from(a) / 255.0;
        (f32::from(c) * alpha + 255.0 * (1.0 - alpha)) / 255.0
    };
    let r = flatten(r);
    let g = flatten(g);
    let b = flatten(b);

    let k = 1.0 - r.max(g).max(b);
    if k >= 1.0 {
        return [0, 0, 0, 255];
    }
    // c = (1 - v - k) / (1 - k), m and y likewise.
    let chan = |v: f32| -> u8 {
        let cmy = (1.0 - v - k) / (1.0 - k);
        (cmy * 255.0).round().clamp(0.0, 255.0) as u8
    };
    [
        chan(r),
        chan(g),
        chan(b),
        (k * 255.0).round().clamp(0.0, 255.0) as u8,
    ]
}

// ------------------------------------------------------------------
// Minimal uncompressed CMYK TIFF writer (little-endian, TIFF 6.0).
// ------------------------------------------------------------------

/// TIFF field types used below.
const TIFF_SHORT: u16 = 3;
const TIFF_LONG: u16 = 4;

const TAG_IMAGE_WIDTH: u16 = 256;
const TAG_IMAGE_LENGTH: u16 = 257;
const TAG_BITS_PER_SAMPLE: u16 = 258;
const TAG_COMPRESSION: u16 = 259;
const TAG_PHOTOMETRIC: u16 = 262;
const TAG_STRIP_OFFSETS: u16 = 273;
const TAG_SAMPLES_PER_PIXEL: u16 = 277;
const TAG_ROWS_PER_STRIP: u16 = 278;
const TAG_STRIP_BYTE_COUNTS: u16 = 279;
const TAG_PLANAR_CONFIG: u16 = 284;
const TAG_INK_SET: u16 = 332;
const TAG_NUMBER_OF_INKS: u16 = 334;

/// One IFD entry being assembled; `value` is the inline u32 field
/// (for SHORT counts ≤ 2 the value is left-justified in the low
/// bytes) or, when `offset` is `Some`, a byte offset to the real
/// data.
struct IfdEntry {
    tag: u16,
    ty: u16,
    count: u32,
    value: u32,
}

/// Serializes `pixels` (row-major CMYK, 4 bytes/pixel) as an
/// uncompressed little-endian TIFF. `pixels.len()` must equal
/// `width * height * 4`.
fn encode_cmyk_tiff(width: u32, height: u32, pixels: &[u8]) -> Vec<u8> {
    debug_assert_eq!(pixels.len(), width as usize * height as usize * 4);

    // Layout: header (8) | IFD count+entries+next (2 + 12n + 4)
    //         | BitsPerSample array (8) | pixel data.
    let n_entries = 12u32;
    let ifd_offset = 8u32;
    let ifd_size = 2 + n_entries * 12 + 4;
    let bps_offset = ifd_offset + ifd_size;
    let data_offset = bps_offset + 8;

    let strip_bytes = pixels.len() as u32;
    let entries = [
        IfdEntry {
            tag: TAG_IMAGE_WIDTH,
            ty: TIFF_LONG,
            count: 1,
            value: width,
        },
        IfdEntry {
            tag: TAG_IMAGE_LENGTH,
            ty: TIFF_LONG,
            count: 1,
            value: height,
        },
        IfdEntry {
            tag: TAG_BITS_PER_SAMPLE,
            ty: TIFF_SHORT,
            count: 4,
            value: bps_offset,
        },
        IfdEntry {
            tag: TAG_COMPRESSION,
            ty: TIFF_SHORT,
            count: 1,
            value: 1,
        },
        IfdEntry {
            tag: TAG_PHOTOMETRIC,
            ty: TIFF_SHORT,
            count: 1,
            value: 5,
        },
        IfdEntry {
            tag: TAG_STRIP_OFFSETS,
            ty: TIFF_LONG,
            count: 1,
            value: data_offset,
        },
        IfdEntry {
            tag: TAG_SAMPLES_PER_PIXEL,
            ty: TIFF_SHORT,
            count: 1,
            value: 4,
        },
        IfdEntry {
            tag: TAG_ROWS_PER_STRIP,
            ty: TIFF_LONG,
            count: 1,
            value: height,
        },
        IfdEntry {
            tag: TAG_STRIP_BYTE_COUNTS,
            ty: TIFF_LONG,
            count: 1,
            value: strip_bytes,
        },
        IfdEntry {
            tag: TAG_PLANAR_CONFIG,
            ty: TIFF_SHORT,
            count: 1,
            value: 1,
        },
        IfdEntry {
            tag: TAG_INK_SET,
            ty: TIFF_SHORT,
            count: 1,
            value: 1,
        },
        IfdEntry {
            tag: TAG_NUMBER_OF_INKS,
            ty: TIFF_SHORT,
            count: 1,
            value: 4,
        },
    ];

    let mut out = Vec::with_capacity(data_offset as usize + pixels.len());
    // Header: "II" (little-endian), magic 42, IFD offset.
    out.extend_from_slice(b"II");
    out.extend_from_slice(&42u16.to_le_bytes());
    out.extend_from_slice(&ifd_offset.to_le_bytes());

    // IFD: entry count, entries sorted by tag (they already are),
    // next-IFD offset = 0.
    out.extend_from_slice(&(n_entries as u16).to_le_bytes());
    for e in &entries {
        out.extend_from_slice(&e.tag.to_le_bytes());
        out.extend_from_slice(&e.ty.to_le_bytes());
        out.extend_from_slice(&e.count.to_le_bytes());
        out.extend_from_slice(&e.value.to_le_bytes());
    }
    out.extend_from_slice(&0u32.to_le_bytes());

    // BitsPerSample: four SHORTs of 8. For SHORT values ≤ 2 per
    // entry the value field holds them inline; here count = 4 so the
    // field is a real offset (already written above).
    for _ in 0..4 {
        out.extend_from_slice(&8u16.to_le_bytes());
    }

    out.extend_from_slice(pixels);
    out
}

#[cfg(test)]
mod tests {
    use std::io::Cursor;

    use image::{DynamicImage, Rgb, RgbaImage};

    use super::{cmyk, rgb_to_cmyk};
    use crate::crates::core::engine::EngineLimits;

    /// Encodes a 2x2 PNG with the given RGBA pixels (row-major).
    fn png_of(pixels: &[[u8; 4]]) -> Vec<u8> {
        let mut img = RgbaImage::new(2, 2);
        for (i, p) in pixels.iter().enumerate() {
            img.put_pixel((i % 2) as u32, (i / 2) as u32, image::Rgba(*p));
        }
        let mut buf = Vec::new();
        DynamicImage::ImageRgba8(img)
            .write_to(&mut Cursor::new(&mut buf), image::ImageFormat::Png)
            .expect("test PNG must encode");
        buf
    }

    /// Minimal TIFF reader for the subset this writer emits: returns
    /// (width, height, photometric, pixels) by parsing the IFD.
    fn parse_cmyk_tiff(bytes: &[u8]) -> (u32, u32, u16, Vec<u8>) {
        assert_eq!(&bytes[0..4], b"II*\0", "must be little-endian TIFF");
        let u16at = |o: usize| u16::from_le_bytes([bytes[o], bytes[o + 1]]);
        let u32at =
            |o: usize| u32::from_le_bytes([bytes[o], bytes[o + 1], bytes[o + 2], bytes[o + 3]]);
        let ifd = u32at(4) as usize;
        let n = u16at(ifd) as usize;
        let mut width = 0;
        let mut height = 0;
        let mut photometric = 0;
        let mut offset = 0;
        let mut count = 0;
        for i in 0..n {
            let e = ifd + 2 + i * 12;
            let tag = u16at(e);
            let value = u32at(e + 8);
            match tag {
                256 => width = value,
                257 => height = value,
                262 => photometric = value as u16,
                273 => offset = value as usize,
                279 => count = value as usize,
                _ => {}
            }
        }
        (
            width,
            height,
            photometric,
            bytes[offset..offset + count].to_vec(),
        )
    }

    #[test]
    fn rgb_to_cmyk_primaries() {
        // Red: no cyan, full magenta+yellow, no black.
        assert_eq!(rgb_to_cmyk(255, 0, 0, 255), [0, 255, 255, 0]);
        // Green.
        assert_eq!(rgb_to_cmyk(0, 255, 0, 255), [255, 0, 255, 0]);
        // Blue.
        assert_eq!(rgb_to_cmyk(0, 0, 255, 255), [255, 255, 0, 0]);
        // White → all zeros; black → full black ink.
        assert_eq!(rgb_to_cmyk(255, 255, 255, 255), [0, 0, 0, 0]);
        assert_eq!(rgb_to_cmyk(0, 0, 0, 255), [0, 0, 0, 255]);
    }

    #[test]
    fn rgb_to_cmyk_flattens_alpha_over_white() {
        // Fully transparent red composites to white → no ink.
        assert_eq!(rgb_to_cmyk(255, 0, 0, 0), [0, 0, 0, 0]);
        // Half-transparent black ≈ mid gray → moderate black ink.
        let cmyk = rgb_to_cmyk(0, 0, 0, 128);
        assert!(cmyk[3] > 100 && cmyk[3] < 160, "{cmyk:?}");
    }

    #[test]
    fn cmyk_produces_valid_tiff() {
        let input = png_of(&[
            [255, 0, 0, 255],
            [0, 255, 0, 255],
            [0, 0, 255, 255],
            [255, 255, 255, 255],
        ]);
        let out = cmyk(&input, &EngineLimits::default()).expect("cmyk must succeed");

        let (w, h, photometric, pixels) = parse_cmyk_tiff(&out);
        assert_eq!((w, h), (2, 2));
        assert_eq!(photometric, 5, "Separated/CMYK photometric");
        assert_eq!(pixels.len(), 2 * 2 * 4);
        // Row-major: red, green / blue, white.
        assert_eq!(&pixels[0..4], &[0, 255, 255, 0]);
        assert_eq!(&pixels[4..8], &[255, 0, 255, 0]);
        assert_eq!(&pixels[8..12], &[255, 255, 0, 0]);
        assert_eq!(&pixels[12..16], &[0, 0, 0, 0]);
    }

    #[test]
    fn cmyk_enforces_limits() {
        let input = png_of(&[[0, 0, 0, 255]; 4]);
        let limits = EngineLimits {
            max_pixels: Some(3),
            ..EngineLimits::default()
        };
        assert!(cmyk(&input, &limits).is_err());
    }

    #[test]
    fn cmyk_input_comes_from_any_supported_format() {
        // JPEG input (lossy, but structure must survive).
        let mut img = image::RgbImage::new(4, 4);
        for p in img.pixels_mut() {
            *p = Rgb([200, 30, 30]);
        }
        let mut buf = Vec::new();
        DynamicImage::ImageRgb8(img)
            .write_to(&mut Cursor::new(&mut buf), image::ImageFormat::Jpeg)
            .expect("test JPEG must encode");
        let out = cmyk(&buf, &EngineLimits::default()).expect("cmyk must succeed");
        let (w, h, _, _) = parse_cmyk_tiff(&out);
        assert_eq!((w, h), (4, 4));
    }

    #[test]
    fn decode_still_rejects_garbage_for_cmyk() {
        assert!(cmyk(b"not an image", &EngineLimits::default()).is_err());
    }
}
