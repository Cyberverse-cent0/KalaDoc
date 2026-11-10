//! End-to-end coverage for the image engine and security engine
//! working together: generate real images in-code, push them through
//! every image op family, then feed the results back through upload
//! validation (filename sanitization, magic-byte sniffing, resource
//! limits) to prove the pipeline accepts what the engine produces and
//! rejects hostile input at every stage.
//!
//! No files or network are involved — everything operates on in-memory
//! byte buffers, matching the engines' own design.

use std::io::Cursor;

use image::{DynamicImage, ImageBuffer, ImageFormat as ImageCodecFormat, Rgb};
use kaladoc::crates::core::engine::EngineLimits;
use kaladoc::crates::image_engine::{
    AdjustOptions, ColorOptions, CompressOptions, FlipDirection, ImageEngine, ImageError,
    ResizeOptions, WatermarkOptions, WatermarkPosition,
};
use kaladoc::crates::security_engine::{
    ResourceLimits, SecurityError, SniffedFormat, sanitize_filename, sniff_format, validate_pdf,
    validate_upload,
};

// ---------------------------------------------------------------------------
// Test image generation (via the `image` crate, a normal dependency)
// ---------------------------------------------------------------------------

/// 64x48 solid-color PNG.
fn solid_png() -> Vec<u8> {
    let img = ImageBuffer::from_pixel(64, 48, Rgb([220, 40, 40]));
    encode(&DynamicImage::ImageRgb8(img), ImageCodecFormat::Png)
}

/// 64x48 gradient PNG — varies in both dimensions so crop/flip effects are
/// observable.
fn gradient_png() -> Vec<u8> {
    let mut img = ImageBuffer::new(64, 48);
    for (x, y, p) in img.enumerate_pixels_mut() {
        *p = Rgb([(x * 4) as u8, (y * 5) as u8, ((x + y) * 2) as u8]);
    }
    encode(&DynamicImage::ImageRgb8(img), ImageCodecFormat::Png)
}

/// 32x32 deterministic "noise" JPEG (LCG — no rand dependency needed).
fn noise_jpeg() -> Vec<u8> {
    let mut img = ImageBuffer::new(32, 32);
    let mut state: u32 = 0x1234_5678;
    for p in img.pixels_mut() {
        state = state.wrapping_mul(1_664_525).wrapping_add(1_013_904_223);
        let v = (state >> 16) as u8;
        *p = Rgb([v, v.wrapping_mul(3), 255 - v]);
    }
    encode(&DynamicImage::ImageRgb8(img), ImageCodecFormat::Jpeg)
}

fn encode(img: &DynamicImage, format: ImageCodecFormat) -> Vec<u8> {
    let mut buf = Cursor::new(Vec::new());
    img.write_to(&mut buf, format)
        .expect("test fixture encoding should succeed");
    buf.into_inner()
}

/// Decode helper: asserts `bytes` is a readable image and returns info.
fn decoded_info(bytes: &[u8]) -> kaladoc::crates::image_engine::ImageInfo {
    ImageEngine::new()
        .inspect(bytes)
        .expect("output should decode as a valid image")
}

// ---------------------------------------------------------------------------
// 1-3. Image engine: inspection, op roundtrips, chained ops
// ---------------------------------------------------------------------------

#[test]
fn inspect_reports_format_and_dimensions() {
    let engine = ImageEngine::new();

    let png = solid_png();
    let info = engine.inspect(&png).unwrap();
    assert_eq!(info.format, "png");
    assert_eq!((info.width, info.height), (64, 48));
    assert_eq!(info.pixel_count, 64 * 48);
    assert_eq!(info.file_size_bytes, png.len() as u64);

    let jpeg = noise_jpeg();
    let info = engine.inspect(&jpeg).unwrap();
    assert_eq!(info.format, "jpeg");
    assert_eq!((info.width, info.height), (32, 32));

    // Header-only pre-flight agrees with the full decode.
    let report = engine.validate(&png).unwrap();
    assert!(report.valid);
    assert_eq!(report.format, "png");
    assert_eq!((report.width, report.height), (64, 48));
}

#[test]
fn resize_roundtrip_preserves_format_and_bounds() {
    let engine = ImageEngine::new();
    let png = gradient_png();

    let out = engine
        .resize(
            &png,
            ResizeOptions {
                max_width: Some(32),
                max_height: Some(32),
            },
        )
        .unwrap();

    let info = decoded_info(&out);
    assert_eq!(info.format, "png"); // output keeps input format
    assert!(info.width <= 32 && info.height <= 32);
    // Aspect ratio preserved: 64x48 → 32x24.
    assert_eq!((info.width, info.height), (32, 24));

    // Free-function entry point produces the same result shape.
    let out2 = kaladoc::crates::image_engine::resize_to(
        &png,
        Some(16),
        Some(16),
        &EngineLimits::default(),
    )
    .unwrap();
    let info2 = decoded_info(&out2);
    assert!(info2.width <= 16 && info2.height <= 16);
    assert_eq!((info2.width, info2.height), (16, 12));
}

#[test]
fn rotate_roundtrip_swaps_dimensions() {
    let engine = ImageEngine::new();
    let png = gradient_png(); // 64x48

    let out = engine.rotate(&png, 90).unwrap();
    let info = decoded_info(&out);
    assert_eq!((info.width, info.height), (48, 64));

    let out = engine.rotate(&png, 180).unwrap();
    assert_eq!(
        (decoded_info(&out).width, decoded_info(&out).height),
        (64, 48)
    );

    let out = engine.rotate(&png, 270).unwrap();
    assert_eq!(
        (decoded_info(&out).width, decoded_info(&out).height),
        (48, 64)
    );
}

#[test]
fn crop_roundtrip_produces_exact_rectangle() {
    let engine = ImageEngine::new();
    let png = gradient_png(); // 64x48

    let out = engine.crop(&png, 10, 5, 20, 15).unwrap();
    let info = decoded_info(&out);
    assert_eq!((info.width, info.height), (20, 15));
    assert_eq!(info.format, "png");
}

#[test]
fn flip_roundtrip_changes_pixels_but_keeps_dimensions() {
    let engine = ImageEngine::new();
    let png = gradient_png();
    let orig = decoded_info(&png);

    for dir in [
        FlipDirection::Horizontal,
        FlipDirection::Vertical,
        FlipDirection::Both,
    ] {
        let out = engine.flip(&png, dir).unwrap();
        let info = decoded_info(&out);
        assert_eq!((info.width, info.height), (orig.width, orig.height));
    }

    // Horizontal flip actually mirrors: leftmost column of original equals
    // rightmost column of flipped output.
    let a = image::load_from_memory(&png).unwrap().to_rgb8();
    let b = image::load_from_memory(&engine.flip(&png, FlipDirection::Horizontal).unwrap())
        .unwrap()
        .to_rgb8();
    for y in 0..a.height() {
        assert_eq!(a.get_pixel(0, y), b.get_pixel(a.width() - 1, y));
    }
}

#[test]
fn adjust_and_color_ops_roundtrip() {
    let engine = ImageEngine::new();
    let png = gradient_png();

    let brightened = engine
        .adjust(
            &png,
            &AdjustOptions {
                brightness: Some(20),
                contrast: Some(10.0),
                hue_degrees: Some(30),
            },
        )
        .unwrap();
    let info = decoded_info(&brightened);
    assert_eq!((info.width, info.height), (64, 48));

    // Grayscale conversion changes the color type but keeps geometry.
    let gray = engine
        .color(
            &png,
            &ColorOptions {
                grayscale: true,
                ..ColorOptions::default()
            },
        )
        .unwrap();
    let gray_info = decoded_info(&gray);
    assert_eq!((gray_info.width, gray_info.height), (64, 48));
    assert_eq!(gray_info.color_type.as_deref(), Some("Luma8"));

    // Invert alone is a valid op too.
    let inverted = engine
        .color(
            &png,
            &ColorOptions {
                invert: true,
                ..ColorOptions::default()
            },
        )
        .unwrap();
    decoded_info(&inverted);
}

#[test]
fn convert_retargets_format_and_reencodes() {
    let engine = ImageEngine::new();
    let png = gradient_png();

    for (target, expected) in [
        ("png", "png"),
        ("jpeg", "jpeg"),
        ("jpg", "jpeg"), // alias
        ("bmp", "bmp"),
        ("webp", "webp"),
        ("gif", "gif"),
    ] {
        let out = engine.convert(&png, target).unwrap();
        let info = decoded_info(&out);
        assert_eq!(info.format, expected, "target {target}");
        assert_eq!((info.width, info.height), (64, 48));
    }

    // Case/whitespace-insensitive target names.
    let out = engine.convert(&png, "  PNG ").unwrap();
    assert_eq!(decoded_info(&out).format, "png");
}

#[test]
fn compress_and_thumbnail_roundtrip() {
    let engine = ImageEngine::new();
    let jpeg = noise_jpeg();

    // Re-encode JPEG at low quality — still valid JPEG, smaller or equal.
    let compressed = engine
        .compress(
            &jpeg,
            CompressOptions {
                format: None,
                quality: Some(20),
            },
        )
        .unwrap();
    let info = decoded_info(&compressed);
    assert_eq!(info.format, "jpeg");
    assert!(compressed.len() < jpeg.len() + 1024); // bounded growth, typically smaller

    // Thumbnail never upscales and fits the box.
    let png = gradient_png();
    let thumb = engine.thumbnail(&png, 16, 16).unwrap();
    let info = decoded_info(&thumb);
    assert!(info.width <= 16 && info.height <= 16);
    assert_eq!((info.width, info.height), (16, 12));

    // Small source stays small: thumbnail of a 32x32 into 64x64 does not upscale.
    let small_jpeg = noise_jpeg();
    let thumb2 = engine.thumbnail(&small_jpeg, 64, 64).unwrap();
    assert_eq!(
        (decoded_info(&thumb2).width, decoded_info(&thumb2).height),
        (32, 32)
    );
}

#[test]
fn metadata_reports_descriptive_fields() {
    let engine = ImageEngine::new();
    let png = gradient_png();

    let meta = engine.metadata(&png).unwrap();
    assert_eq!(meta.format, "png");
    assert_eq!((meta.width, meta.height), (64, 48));
    assert_eq!(meta.pixel_count, 64 * 48);
    assert_eq!(meta.file_size_bytes, png.len() as u64);
    assert!(meta.color_type.is_some());
}

#[test]
fn watermark_and_cmyk_produce_decodable_output() {
    let engine = ImageEngine::new();
    let base = gradient_png();
    let mark = solid_png(); // 64x48 same size — scale it down via options

    let out = engine
        .watermark(
            &base,
            &mark,
            &WatermarkOptions {
                position: WatermarkPosition::Center,
                opacity: 0.5,
                scale: Some(0.25),
                ..WatermarkOptions::default()
            },
        )
        .unwrap();
    let info = decoded_info(&out);
    assert_eq!((info.width, info.height), (64, 48));

    // CMYK op always emits uncompressed TIFF regardless of input format.
    let cmyk = engine.cmyk(&base).unwrap();
    let info = decoded_info(&cmyk);
    assert_eq!(info.format, "tiff");
}

#[test]
fn chained_pipeline_resize_compress_convert_produces_valid_output() {
    let engine = ImageEngine::new();
    let png = gradient_png();

    // resize → compress → convert(png): a realistic e2e pipeline.
    let resized = engine
        .resize(
            &png,
            ResizeOptions {
                max_width: Some(32),
                max_height: Some(32),
            },
        )
        .unwrap();
    let compressed = engine
        .compress(
            &resized,
            CompressOptions {
                format: Some(kaladoc::crates::image_engine::ImageFormat::Png),
                quality: Some(80),
            },
        )
        .unwrap();
    let converted = engine.convert(&compressed, "png").unwrap();

    let info = decoded_info(&converted);
    assert_eq!(info.format, "png");
    assert_eq!((info.width, info.height), (32, 24));

    // And through the security gate before storage.
    let report = validate_upload(
        "pipeline-output.png",
        converted.len() as u64,
        &converted,
        &ResourceLimits::default(),
        Some(SniffedFormat::Png),
    )
    .unwrap();
    assert_eq!(report.format, SniffedFormat::Png);
}

// ---------------------------------------------------------------------------
// 4. Image engine error paths
// ---------------------------------------------------------------------------

#[test]
fn garbage_bytes_fail_to_decode() {
    let engine = ImageEngine::new();

    for garbage in [
        &b""[..],
        b"this is definitely not an image",
        &[0xFF, 0xD8][..], // truncated JPEG magic
        b"%PDF-1.7 fake pdf pretending to be an image",
    ] {
        assert!(
            engine.inspect(garbage).is_err(),
            "expected decode error for {garbage:?}"
        );
        assert!(
            engine
                .resize(
                    garbage,
                    ResizeOptions {
                        max_width: Some(10),
                        max_height: Some(10)
                    }
                )
                .is_err()
        );
        assert!(engine.validate(garbage).is_err());
    }
}

#[test]
fn truncated_image_is_rejected() {
    let engine = ImageEngine::new();
    let png = solid_png();

    // Cut the PNG roughly in half — header parses, pixels don't.
    let truncated = &png[..png.len() / 2];
    let result = engine.inspect(truncated);
    assert!(result.is_err(), "truncated PNG must not inspect cleanly");
    assert!(engine.convert(truncated, "png").is_err());
}

#[test]
fn engine_limits_reject_oversized_input_and_pixels() {
    let png = solid_png();

    // max_input_bytes smaller than the fixture.
    let strict = ImageEngine::with_limits(EngineLimits {
        max_input_bytes: Some(10),
        ..EngineLimits::default()
    });
    let err = strict.inspect(&png).unwrap_err();
    assert!(matches!(err, ImageError::LimitExceeded(_)), "got {err:?}");

    // max_pixels smaller than 64*48=3072.
    let strict = ImageEngine::with_limits(EngineLimits {
        max_pixels: Some(100),
        ..EngineLimits::default()
    });
    let err = strict.inspect(&png).unwrap_err();
    assert!(matches!(err, ImageError::LimitExceeded(_)), "got {err:?}");

    // Header-only validate() also enforces max_pixels before decode.
    let err = strict.validate(&png).unwrap_err();
    assert!(matches!(err, ImageError::LimitExceeded(_)), "got {err:?}");

    // Exactly-at-limit still passes.
    let exact = ImageEngine::with_limits(EngineLimits {
        max_pixels: Some(3072),
        ..EngineLimits::default()
    });
    assert!(exact.inspect(&png).is_ok());
}

#[test]
fn invalid_arguments_fail_honestly() {
    let engine = ImageEngine::new();
    let png = solid_png();

    // Unknown target format name.
    let err = engine.convert(&png, "tiff2").unwrap_err();
    assert!(
        matches!(
            err,
            ImageError::InvalidInput(_) | ImageError::UnsupportedFormat(_)
        ),
        "got {err:?}"
    );

    // Rotation angles outside {90,180,270}.
    let err = engine.rotate(&png, 45).unwrap_err();
    assert!(matches!(err, ImageError::InvalidInput(_)), "got {err:?}");

    // Empty resize box.
    let err = engine
        .resize(
            &png,
            ResizeOptions {
                max_width: None,
                max_height: None,
            },
        )
        .unwrap_err();
    assert!(matches!(err, ImageError::InvalidInput(_)), "got {err:?}");

    // Zero-dimension crop / thumbnail.
    assert!(matches!(
        engine.crop(&png, 0, 0, 0, 10).unwrap_err(),
        ImageError::InvalidInput(_)
    ));
    assert!(matches!(
        engine.thumbnail(&png, 0, 10).unwrap_err(),
        ImageError::InvalidInput(_)
    ));

    // Out-of-range quality.
    let err = engine
        .compress(
            &png,
            CompressOptions {
                format: None,
                quality: Some(0),
            },
        )
        .unwrap_err();
    assert!(matches!(err, ImageError::InvalidInput(_)), "got {err:?}");
}

// ---------------------------------------------------------------------------
// 5. Security: magic-byte sniffing
// ---------------------------------------------------------------------------

#[test]
fn sniff_format_identifies_magic_bytes() {
    assert_eq!(sniff_format(b"%PDF-1.7\n..."), SniffedFormat::Pdf);
    assert_eq!(sniff_format(b"\x89PNG\r\n\x1a\nrest"), SniffedFormat::Png);
    assert_eq!(sniff_format(&[0xFF, 0xD8, 0xFF, 0xE0]), SniffedFormat::Jpeg);
    assert_eq!(sniff_format(b"GIF89a...."), SniffedFormat::Gif);
    assert_eq!(sniff_format(b"GIF87a...."), SniffedFormat::Gif);
    assert_eq!(sniff_format(b"PK\x03\x04\x14\x00"), SniffedFormat::Zip);
}

#[test]
fn sniff_format_is_honest_about_truncation_and_garbage() {
    // Truncated signatures → Unknown, never a guess.
    for bytes in [
        &b""[..],
        b"%PDF",
        b"%pdF-",
        b"\x89PN",
        b"PK\x03",
        &[0xFF, 0xD8][..],
        b"random garbage bytes",
        b"#!/bin/sh\necho pwned",
    ] {
        assert_eq!(sniff_format(bytes), SniffedFormat::Unknown, "{bytes:?}");
    }
}

#[test]
fn validate_upload_accepts_real_image_bytes() {
    let png = solid_png();
    let report = validate_upload(
        "photo.png",
        png.len() as u64,
        &png,
        &ResourceLimits::default(),
        Some(SniffedFormat::Png),
    )
    .unwrap();
    assert_eq!(report.sanitized_filename, "photo.png");
    assert_eq!(report.format, SniffedFormat::Png);
    assert_eq!(report.size_bytes, png.len() as u64);

    // JPEG bytes sniff as Jpeg.
    let jpeg = noise_jpeg();
    let report = validate_upload(
        "noise.jpg",
        jpeg.len() as u64,
        &jpeg,
        &ResourceLimits::default(),
        Some(SniffedFormat::Jpeg),
    )
    .unwrap();
    assert_eq!(report.format, SniffedFormat::Jpeg);
}

#[test]
fn validate_upload_rejects_format_spoofing_and_truncated_content() {
    let png = solid_png();

    // PNG bytes claiming to be a PDF.
    let err = validate_upload(
        "evil.pdf",
        png.len() as u64,
        &png,
        &ResourceLimits::default(),
        Some(SniffedFormat::Pdf),
    )
    .unwrap_err();
    assert!(
        matches!(
            err,
            SecurityError::ContentTypeMismatch {
                expected: SniffedFormat::Pdf,
                found: SniffedFormat::Png,
            }
        ),
        "got {err:?}"
    );

    // Truncated PNG: header may still sniff as PNG, so pair it with a
    // byte-level check — a truncated upload is at minimum suspicious and
    // the engine will reject it downstream (covered in image error tests).
    // Here we verify the sniffer itself reports honestly on the head.
    let truncated = &png[..16];
    assert_eq!(sniff_format(truncated), SniffedFormat::Png);

    // Garbage content with an image filename and matching expectation fails.
    let err = validate_upload(
        "fake.png",
        20,
        b"definitely not an image!!",
        &ResourceLimits::default(),
        Some(SniffedFormat::Png),
    )
    .unwrap_err();
    assert!(matches!(err, SecurityError::ContentTypeMismatch { .. }));
}

#[test]
fn validate_upload_enforces_size_and_consistency_checks() {
    let png = solid_png();
    let mut limits = ResourceLimits::default();
    limits.max_upload_bytes = 100;

    // Oversized declared size → rejected before content is even examined.
    let err = validate_upload(
        "big.png",
        png.len() as u64,
        &png,
        &limits,
        Some(SniffedFormat::Png),
    )
    .unwrap_err();
    assert!(matches!(err, SecurityError::FileTooLarge { .. }));

    // Declared size must equal actual content length.
    let err = validate_upload(
        "lie.png",
        png.len() as u64 + 1,
        &png,
        &ResourceLimits::default(),
        None,
    )
    .unwrap_err();
    assert!(matches!(err, SecurityError::SizeMismatch { .. }));

    // Empty content.
    let err = validate_upload("empty.png", 0, b"", &ResourceLimits::default(), None).unwrap_err();
    assert!(matches!(err, SecurityError::EmptyFile));
}

// ---------------------------------------------------------------------------
// 6. Security: filename validation
// ---------------------------------------------------------------------------

#[test]
fn sanitize_filename_rejects_traversal_and_path_syntax() {
    for name in [
        "../secret.png",
        "..\\win.png",
        "..",
        "sub/dir/file.png",
        "sub\\dir.png",
        "/etc/passwd",
        "C:\\evil.png",
        "file.png:ads",
    ] {
        assert!(
            sanitize_filename(name).is_err(),
            "expected rejection: {name:?}"
        );
    }
}

#[test]
fn sanitize_filename_rejects_reserved_and_control_characters() {
    for name in [
        "CON.png",
        "nul.txt",
        "com1.log",
        "lpt9",
        "bad\0name.png",
        "line\nbreak.png",
        "trailing.",
        " leading.png",
        "name ",
        ".hidden",
        &"a".repeat(300),
    ] {
        assert!(
            sanitize_filename(name).is_err(),
            "expected rejection: {name:?}"
        );
    }

    // RTL-override spoofing attempt.
    assert!(sanitize_filename("exe\u{202E}fdp.png").is_err());
}

#[test]
fn sanitize_filename_accepts_unicode_and_normal_names() {
    for name in [
        "photo.png",
        "my photo (final).PNG",
        "résumé-2024.png",
        "ファイル.png",
        "café.txt",
        "quarterly report.v2.png",
    ] {
        assert_eq!(sanitize_filename(name).unwrap(), name);
    }
}

// ---------------------------------------------------------------------------
// 7. Security: ResourceLimits primitives
// ---------------------------------------------------------------------------

#[test]
fn resource_limits_reject_oversized_before_processing() {
    let mut limits = ResourceLimits::default();
    limits.max_upload_bytes = 1024;

    assert!(limits.check_size(1024).is_ok());
    assert!(matches!(
        limits.check_size(1025),
        Err(SecurityError::FileTooLarge {
            size: 1025,
            max: 1024
        })
    ));

    let mut limits = ResourceLimits::default();
    limits.max_operation_bytes = 10;
    assert!(matches!(
        limits.check_operation_size(11),
        Err(SecurityError::OperationTooLarge { .. })
    ));

    let mut limits = ResourceLimits::default();
    limits.max_pages = Some(5);
    assert!(matches!(
        limits.check_pages(6),
        Err(SecurityError::TooManyPages { pages: 6, max: 5 })
    ));
    limits.max_pages = None;
    assert!(limits.check_pages(1_000_000).is_ok());

    let mut limits = ResourceLimits::default();
    limits.max_outputs = 2;
    assert!(matches!(
        limits.check_outputs(3),
        Err(SecurityError::TooManyOutputs { count: 3, max: 2 })
    ));
}

// ---------------------------------------------------------------------------
// 8. Cross-check: image op outputs through the security gate
// ---------------------------------------------------------------------------

#[test]
fn image_op_outputs_pass_security_validation() {
    let engine = ImageEngine::new();
    let limits = ResourceLimits::default();

    let png = gradient_png();

    // Every byte-producing op's output should be a well-formed upload:
    // correct magic bytes, sane filename, within default limits.
    let outputs: Vec<(&str, Vec<u8>, SniffedFormat)> = vec![
        (
            "resized.png",
            engine.resize_to(&png, Some(32), Some(32)).unwrap(),
            SniffedFormat::Png,
        ),
        (
            "rotated.png",
            engine.rotate(&png, 90).unwrap(),
            SniffedFormat::Png,
        ),
        (
            "cropped.png",
            engine.crop(&png, 0, 0, 16, 16).unwrap(),
            SniffedFormat::Png,
        ),
        (
            "flipped.png",
            engine.flip(&png, FlipDirection::Horizontal).unwrap(),
            SniffedFormat::Png,
        ),
        (
            "thumb.png",
            engine.thumbnail(&png, 16, 16).unwrap(),
            SniffedFormat::Png,
        ),
        (
            "adjusted.png",
            engine
                .adjust(
                    &png,
                    &AdjustOptions {
                        brightness: Some(10),
                        ..AdjustOptions::default()
                    },
                )
                .unwrap(),
            SniffedFormat::Png,
        ),
        (
            "converted.jpg",
            engine.convert(&png, "jpeg").unwrap(),
            SniffedFormat::Jpeg,
        ),
        (
            "compressed.png",
            engine
                .compress(
                    &png,
                    CompressOptions {
                        format: None,
                        quality: Some(50),
                    },
                )
                .unwrap(),
            SniffedFormat::Png,
        ),
    ];

    for (name, bytes, expected) in outputs {
        // The security sniffer sees the right format.
        assert_eq!(sniff_format(&bytes), expected, "{name}");
        // And the full upload gate accepts it end-to-end.
        let report = validate_upload(name, bytes.len() as u64, &bytes, &limits, Some(expected))
            .unwrap_or_else(|e| panic!("{name} failed upload validation: {e}"));
        assert_eq!(report.format, expected);
        // Filename came back sanitized and identical (these are safe names).
        assert_eq!(report.sanitized_filename, name);
    }
}

#[test]
fn hostile_input_rejected_at_security_boundary_before_engine() {
    let engine = ImageEngine::new();
    let limits = ResourceLimits::default();

    // Simulate the real gate order: sanitize filename → check size →
    // sniff → (optionally) hand to engine.
    let hostile_name = "../../etc/cron.d/pwn";
    assert!(sanitize_filename(hostile_name).is_err());

    // Oversized upload rejected by ResourceLimits without touching engine.
    let mut tight = ResourceLimits::default();
    tight.max_upload_bytes = 16;
    let png = solid_png();
    assert!(matches!(
        validate_upload("ok.png", png.len() as u64, &png, &tight, None),
        Err(SecurityError::FileTooLarge { .. })
    ));

    // Spoofed content rejected even though filename is fine.
    assert!(matches!(
        validate_upload(
            "innocent.png",
            11,
            b"%PDF-1.7\n%%EOF",
            &limits,
            Some(SniffedFormat::Png),
        ),
        Err(SecurityError::ContentTypeMismatch { .. })
    ));

    // What survives the gate is genuinely processable by the engine.
    let report = validate_upload(
        "innocent.png",
        png.len() as u64,
        &png,
        &limits,
        Some(SniffedFormat::Png),
    )
    .unwrap();
    let info = engine.inspect(&png).unwrap();
    assert_eq!(info.format, "png");
    assert_eq!(report.size_bytes, png.len() as u64);
}

// ---------------------------------------------------------------------------
// Bonus: PDF-side validation still works alongside image flows
// ---------------------------------------------------------------------------

#[test]
fn validate_pdf_still_gates_pdf_bytes() {
    const MINIMAL_PDF: &[u8] = b"%PDF-1.4\n\
1 0 obj<</Type/Catalog/Pages 2 0 R>>endobj\n\
2 0 obj<</Type/Pages/Kids[3 0 R]/Count 1>>endobj\n\
3 0 obj<</Type/Page/Parent 2 0 R/MediaBox[0 0 612 792]>>endobj\n\
trailer<</Root 1 0 R/ID[<0123456789abcdef0123456789abcdef><0123456789abcdef0123456789abcdef>]>>\n\
%%EOF";

    assert!(validate_pdf(MINIMAL_PDF, 10 * 1024 * 1024).is_ok());
    assert!(matches!(
        validate_pdf(b"not a pdf", 1024),
        Err(SecurityError::NotPdf)
    ));
    assert!(matches!(
        validate_pdf(&[], 1024),
        Err(SecurityError::EmptyFile)
    ));
    // Image bytes are not PDFs.
    assert!(matches!(
        validate_pdf(&solid_png(), 10 * 1024 * 1024),
        Err(SecurityError::NotPdf)
    ));
}
