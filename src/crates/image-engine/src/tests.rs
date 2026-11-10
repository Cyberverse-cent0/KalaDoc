//! Integration tests for [`ImageEngine`] and the `ops` functions.
//!
//! All fixtures are generated in-test with the `image` crate itself —
//! no fixture files needed.

use image::{DynamicImage, ImageBuffer, ImageFormat, Rgb, Rgba};

use crate::crates::core::engine::{
    EngineHealthStatus, EngineKind, EngineLimits, EngineOperation, ProcessingEngine,
};
use crate::crates::image_engine::error::ImageError;
use crate::crates::image_engine::{ImageEngine, OcrOptions, ResizeOptions, ScannerMode};

fn engine() -> ImageEngine {
    ImageEngine::new()
}

fn unlimited() -> EngineLimits {
    EngineLimits::default()
}

/// A 4×2 RGB test image with a distinct color per pixel.
fn test_rgb() -> DynamicImage {
    let mut img = ImageBuffer::new(4, 2);
    for (x, y, px) in img.enumerate_pixels_mut() {
        *px = Rgb([(x * 60) as u8, (y * 120) as u8, 200]);
    }
    DynamicImage::ImageRgb8(img)
}

/// Encode a [`DynamicImage`] to bytes in `format`.
fn encode(image: &DynamicImage, format: ImageFormat) -> Vec<u8> {
    let mut buf = std::io::Cursor::new(Vec::new());
    image.write_to(&mut buf, format).unwrap();
    buf.into_inner()
}

fn png_bytes() -> Vec<u8> {
    encode(&test_rgb(), ImageFormat::Png)
}

fn jpeg_bytes() -> Vec<u8> {
    encode(&test_rgb(), ImageFormat::Jpeg)
}

#[test]
fn trait_metadata() {
    let engine = engine();
    assert_eq!(engine.name(), "image");
    assert_eq!(engine.kind(), EngineKind::Image);
    assert!(!engine.version().is_empty());

    let caps = engine.capabilities();
    for op in [
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
        EngineOperation::Overlay,
        EngineOperation::Validate,
        EngineOperation::ExtractMetadata,
        EngineOperation::Ocr,
        EngineOperation::Deskew,
        EngineOperation::CleanScan,
    ] {
        assert!(caps.operations.contains(&op), "missing {op:?}");
    }
    // Operations we do NOT implement must not be advertised.
    for op in [
        EngineOperation::Merge,
        EngineOperation::Split,
        EngineOperation::ExtractPages,
        EngineOperation::ExtractText,
        EngineOperation::Encrypt,
        EngineOperation::Decrypt,
    ] {
        assert!(!caps.operations.contains(&op), "unexpected {op:?}");
    }
    for format in ["png", "jpeg", "gif", "webp", "bmp", "tiff"] {
        assert!(caps.input_formats.contains(&format.to_string()));
        assert!(caps.output_formats.contains(&format.to_string()));
    }
}

#[tokio::test]
async fn health_reflects_tesseract_availability() {
    let health = engine().health().await;
    // image itself is embedded, so the only degradation source is the
    // external `tesseract` binary backing `ocr`.
    if crate::crates::image_engine::ops::tesseract_available() {
        assert_eq!(health.status, EngineHealthStatus::Healthy);
        assert!(health.message.is_none());
    } else {
        assert_eq!(health.status, EngineHealthStatus::Degraded);
        assert!(health.message.is_some());
    }
}

#[test]
fn inspect_reports_dimensions_and_format() {
    let info = engine().inspect(&png_bytes()).unwrap();
    assert_eq!(info.format, "png");
    assert_eq!(info.width, 4);
    assert_eq!(info.height, 2);
    assert_eq!(info.pixel_count, 8);
    assert_eq!(info.file_size_bytes, png_bytes().len() as u64);
    assert_eq!(info.color_type.as_deref(), Some("Rgb8"));
    assert!(!info.has_alpha);
}

#[test]
fn inspect_jpeg() {
    let info = engine().inspect(&jpeg_bytes()).unwrap();
    assert_eq!(info.format, "jpeg");
    assert_eq!(info.width, 4);
    assert_eq!(info.height, 2);
}

#[test]
fn inspect_rejects_garbage() {
    let err = engine().inspect(b"this is not an image").unwrap_err();
    // Unrecognized bytes surface as a decode failure (format guessing
    // itself failed); a sniffed-but-unsupported codec would be
    // `UnsupportedFormat`. Either way it must be a hard error.
    assert!(
        matches!(
            err,
            ImageError::Decode(_) | ImageError::UnsupportedFormat(_)
        ),
        "{err:?}"
    );
}

#[test]
fn inspect_rejects_empty_input() {
    assert!(engine().inspect(b"").is_err());
}

#[test]
fn resize_fits_within_box_preserving_aspect() {
    // 4×2 source into a 2×2 box → 2×1.
    let out = engine()
        .resize(
            &png_bytes(),
            ResizeOptions {
                max_width: Some(2),
                max_height: Some(2),
            },
        )
        .unwrap();
    let info = engine().inspect(&out).unwrap();
    assert_eq!(info.format, "png");
    assert_eq!((info.width, info.height), (2, 1));
}

#[test]
fn resize_unbounded_axis() {
    // Only width bound: 4×2 → 2×1? No — aspect preserved: width 2 → height 1.
    let out = engine().resize_to(&png_bytes(), Some(2), None).unwrap();
    let info = engine().inspect(&out).unwrap();
    assert_eq!((info.width, info.height), (2, 1));
}

#[test]
fn resize_no_upscale_constraint_documented() {
    // Upscale is allowed: 4×2 into 8×8 box → 8×4.
    let out = engine().resize_to(&png_bytes(), Some(8), Some(8)).unwrap();
    let info = engine().inspect(&out).unwrap();
    assert_eq!((info.width, info.height), (8, 4));
}

#[test]
fn resize_rejects_empty_box() {
    let err = engine()
        .resize(
            &png_bytes(),
            ResizeOptions {
                max_width: None,
                max_height: None,
            },
        )
        .unwrap_err();
    assert!(matches!(err, ImageError::InvalidInput(_)), "{err:?}");

    let err = engine()
        .resize_to(&png_bytes(), Some(0), Some(4))
        .unwrap_err();
    assert!(matches!(err, ImageError::InvalidInput(_)), "{err:?}");
}

#[test]
fn resize_output_same_format() {
    let jpeg = jpeg_bytes();
    let out = engine().resize_to(&jpeg, Some(2), Some(2)).unwrap();
    let info = engine().inspect(&out).unwrap();
    assert_eq!(info.format, "jpeg");
    assert_eq!((info.width, info.height), (2, 1));
}

#[test]
fn convert_png_to_jpeg() {
    let out = engine().convert(&png_bytes(), "jpeg").unwrap();
    let info = engine().inspect(&out).unwrap();
    assert_eq!(info.format, "jpeg");
    assert_eq!((info.width, info.height), (4, 2));
}

#[test]
fn convert_accepts_jpg_alias_and_case() {
    let out = engine().convert(&png_bytes(), " JPG ").unwrap();
    assert_eq!(engine().inspect(&out).unwrap().format, "jpeg");
}

#[test]
fn convert_to_bmp_and_gif() {
    for target in ["bmp", "gif"] {
        let out = engine().convert(&png_bytes(), target).unwrap();
        assert_eq!(engine().inspect(&out).unwrap().format, target);
    }
}

#[test]
fn convert_to_tiff_and_back() {
    let out = engine().convert(&png_bytes(), "tiff").unwrap();
    let info = engine().inspect(&out).unwrap();
    assert_eq!(info.format, "tiff");
    assert_eq!((info.width, info.height), (4, 2));
    // The TIFF output is itself a supported input format.
    let back = engine().convert(&out, "png").unwrap();
    assert_eq!(engine().inspect(&back).unwrap().format, "png");
}

#[test]
fn convert_accepts_tif_alias() {
    let out = engine().convert(&png_bytes(), " TIF ").unwrap();
    assert_eq!(engine().inspect(&out).unwrap().format, "tiff");
}

#[test]
fn rotate_quarter_turn_swaps_dimensions() {
    // 4x2 source rotated 90° becomes 2x4.
    let out = engine().rotate(&png_bytes(), 90).unwrap();
    let info = engine().inspect(&out).unwrap();
    assert_eq!(info.format, "png");
    assert_eq!((info.width, info.height), (2, 4));
    assert!(engine().rotate(&png_bytes(), 45).is_err());
}

#[test]
fn cmyk_output_is_a_decodable_cmyk_tiff() {
    let out = engine().cmyk(&png_bytes()).unwrap();
    // Little-endian TIFF magic.
    assert_eq!(&out[..4], b"II*\0");
    // The engine's own TIFF decoder reads it back (separated CMYK
    // maps to RGB8), proving the hand-rolled writer is spec-compliant.
    let info = engine().inspect(&out).unwrap();
    assert_eq!(info.format, "tiff");
    assert_eq!((info.width, info.height), (4, 2));
}

#[test]
fn convert_rejects_unknown_format() {
    let err = engine().convert(&png_bytes(), "heic").unwrap_err();
    assert!(matches!(err, ImageError::InvalidInput(_)), "{err:?}");
}

#[test]
fn convert_rejects_garbage_input() {
    assert!(engine().convert(b"nope", "png").is_err());
}

#[test]
fn alpha_png_to_jpeg_flattens() {
    // RGBA source → JPEG has no alpha; must not error.
    let mut img = ImageBuffer::new(2, 2);
    for px in img.pixels_mut() {
        *px = Rgba([255, 0, 0, 128]);
    }
    let rgba_png = encode(&DynamicImage::ImageRgba8(img), ImageFormat::Png);
    let out = engine().convert(&rgba_png, "jpeg").unwrap();
    let info = engine().inspect(&out).unwrap();
    assert_eq!(info.format, "jpeg");
    assert!(!info.has_alpha);
}

#[test]
fn max_input_bytes_enforced_before_decode() {
    let limits = EngineLimits {
        max_input_bytes: Some(10),
        ..unlimited()
    };
    let engine = ImageEngine::with_limits(limits);
    let err = engine.inspect(&png_bytes()).unwrap_err();
    assert!(matches!(err, ImageError::LimitExceeded(_)), "{err:?}");
    assert!(engine.convert(&png_bytes(), "bmp").is_err());
    assert!(engine.resize_to(&png_bytes(), Some(1), Some(1)).is_err());
}

#[test]
fn max_pixels_enforced_on_header_before_decode() {
    // 4×2 = 8 pixels; cap at 7.
    let limits = EngineLimits {
        max_pixels: Some(7),
        ..unlimited()
    };
    let engine = ImageEngine::with_limits(limits);
    let err = engine.inspect(&png_bytes()).unwrap_err();
    assert!(matches!(err, ImageError::LimitExceeded(_)), "{err:?}");
    assert!(engine.convert(&png_bytes(), "bmp").is_err());
    assert!(engine.resize_to(&png_bytes(), Some(1), Some(1)).is_err());
}

#[test]
fn limits_at_boundary_pass() {
    let limits = EngineLimits {
        max_pixels: Some(8),
        ..unlimited()
    };
    let engine = ImageEngine::with_limits(limits);
    assert!(engine.inspect(&png_bytes()).is_ok());
}

#[test]
fn denoise_produces_decodable_output() {
    let out = engine().denoise(&png_bytes(), 2).unwrap();
    let info = engine().inspect(&out).unwrap();
    assert_eq!(info.format, "png");
    assert_eq!((info.width, info.height), (4, 2));
    // strength is validated 1..=5.
    assert!(engine().denoise(&png_bytes(), 0).is_err());
    assert!(engine().denoise(&png_bytes(), 6).is_err());
}

#[test]
fn clean_background_produces_decodable_output() {
    let out = engine().clean_background(&png_bytes()).unwrap();
    let info = engine().inspect(&out).unwrap();
    assert_eq!(info.format, "png");
    assert_eq!((info.width, info.height), (4, 2));
}

/// A page-like image: white background with several horizontal black
/// bars — the structure deskew's projection-variance sweep keys on.
fn text_like_png() -> Vec<u8> {
    let mut img = ImageBuffer::new(64, 48);
    for px in img.pixels_mut() {
        *px = Rgb([255, 255, 255]);
    }
    for y in [8, 16, 24, 32, 40] {
        for x in 4..60 {
            img.put_pixel(x, y, Rgb([0, 0, 0]));
        }
    }
    encode(&DynamicImage::ImageRgb8(img), ImageFormat::Png)
}

#[test]
fn deskew_returns_result_with_report() {
    let result = engine().deskew(&text_like_png(), None).unwrap();
    // Output stays in the input's sniffed format and dimensions.
    let info = engine().inspect(&result.image).unwrap();
    assert_eq!(info.format, "png");
    assert_eq!((info.width, info.height), (64, 48));
    assert!(result.report.angle_deg.is_finite());
    assert!((0.0..=1.0).contains(&result.report.confidence));
    // The sweep is bounded by the default ±5° search range.
    assert!(
        result.report.angle_deg.abs() <= crate::crates::image_engine::DEFAULT_DESKEW_ANGLE + 1e-9,
        "{result:?}"
    );
}

#[test]
fn deskew_rejects_bad_max_angle() {
    for bad in [Some(0.0), Some(-1.0), Some(f64::NAN), Some(46.0)] {
        assert!(
            engine().deskew(&png_bytes(), bad).is_err(),
            "max_angle {bad:?} must be rejected"
        );
    }
}

#[test]
fn scanner_preset_modes_produce_bytes() {
    let input = png_bytes();
    for mode in [
        ScannerMode::Clean,
        ScannerMode::Photocopy,
        ScannerMode::HighContrast,
    ] {
        let out = engine().scanner_preset(&input, mode).unwrap();
        let info = engine().inspect(&out).unwrap();
        assert_eq!(info.format, "png", "mode {mode:?}");
        assert_eq!((info.width, info.height), (4, 2), "mode {mode:?}");
    }
}

#[test]
fn ocr_image_is_honest_when_tesseract_absent() {
    let result = engine().ocr_image(&png_bytes(), &OcrOptions::default());
    match result {
        // tesseract installed: exercise the happy path shape.
        Ok(report) => {
            assert!(!report.language.is_empty());
            assert!(report.words.iter().all(|w| !w.text.is_empty()));
        }
        // tesseract absent: must be the honest ExternalTool error —
        // never a fabricated report.
        Err(ImageError::ExternalTool { tool, .. }) => assert_eq!(tool, "tesseract"),
        Err(e) => panic!("unexpected OCR error: {e:?}"),
    }
    // ocr_languages degrades the same way.
    match engine().ocr_languages() {
        Ok(langs) => assert!(!langs.is_empty()),
        Err(ImageError::ExternalTool { tool, .. }) => assert_eq!(tool, "tesseract"),
        Err(e) => panic!("unexpected ocr_languages error: {e:?}"),
    }
}

#[test]
fn truncated_png_is_decode_error_not_panic() {
    let full = png_bytes();
    let truncated = &full[..full.len() / 2];
    let err = engine().inspect(truncated).unwrap_err();
    // Either the header probe or decode fails — never a panic, never a
    // silent success.
    assert!(
        matches!(
            err,
            ImageError::Decode(_) | ImageError::UnsupportedFormat(_)
        ),
        "{err:?}"
    );
}
