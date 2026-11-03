//! Unit tests for the raster decode path and its header-only probe.

use super::*;
use image::{ExtendedColorType, GenericImageView, ImageEncoder, Rgb, RgbImage, RgbaImage};

/// A minimal little-endian TIFF/Exif payload whose only IFD0 entry is the
/// orientation tag, holding `orientation`.
///
/// The body is the layout a camera writes, trimmed to the single field these
/// tests need: the `image` crate reads it through the same `set_exif_metadata`
/// path a real photo's APP1/`eXIf` chunk arrives on.
fn exif_orientation(orientation: u16) -> Vec<u8> {
    let mut tiff = Vec::new();
    tiff.extend_from_slice(b"II"); // little-endian
    tiff.extend_from_slice(&42u16.to_le_bytes());
    tiff.extend_from_slice(&8u32.to_le_bytes()); // IFD0 starts right after the header
    tiff.extend_from_slice(&1u16.to_le_bytes()); // one entry
    tiff.extend_from_slice(&0x0112u16.to_le_bytes()); // Orientation
    tiff.extend_from_slice(&3u16.to_le_bytes()); // SHORT
    tiff.extend_from_slice(&1u32.to_le_bytes()); // count
    tiff.extend_from_slice(&u32::from(orientation).to_le_bytes());
    tiff.extend_from_slice(&0u32.to_le_bytes()); // no next IFD
    tiff
}

/// Encode a `width`×`height` JPEG carrying `orientation` in its Exif block.
fn jpeg_with_orientation(width: u32, height: u32, orientation: u16) -> Vec<u8> {
    let img = RgbImage::from_pixel(width, height, Rgb([20, 40, 60]));
    let mut out = Cursor::new(Vec::new());
    let mut encoder = image::codecs::jpeg::JpegEncoder::new(&mut out);
    encoder
        .set_exif_metadata(exif_orientation(orientation))
        .expect("the JPEG encoder supports Exif metadata");
    encoder
        .write_image(img.as_raw(), width, height, ExtendedColorType::Rgb8)
        .expect("encode jpeg");
    out.into_inner()
}

/// Encode an image as PNG.
fn png(width: u32, height: u32) -> Vec<u8> {
    let img = RgbaImage::from_fn(width, height, |x, y| {
        // Channel values wrap modulo 256 by design — only the dimensions matter.
        #[expect(clippy::cast_possible_truncation)]
        image::Rgba([(x * 60) as u8, (y * 80) as u8, 0, 255])
    });
    let mut out = Cursor::new(Vec::new());
    DynamicImage::ImageRgba8(img)
        .write_to(&mut out, image::ImageFormat::Png)
        .expect("encode png");
    out.into_inner()
}

#[test]
fn decode_raster_oriented_preserves_dimensions() {
    // A valid PNG with no EXIF orientation decodes to the same dimensions (the
    // orientation path must be a no-op for the default orientation).
    let out = decode_raster_oriented(&png(4, 3)).expect("valid PNG should decode");
    assert_eq!(out.dimensions(), (4, 3));
}

#[test]
fn decode_raster_oriented_rejects_bytes_with_no_format() {
    let error = decode_raster_oriented(&[1, 2, 3]).unwrap_err();
    assert!(
        matches!(error, DecodeError::UnrecognizedFormat),
        "expected an unrecognized-format failure, got: {error}"
    );
}

#[test]
fn decode_raster_oriented_accepts_tall_in_budget_image() {
    // The guard bounds total pixels, not a per-side dimension: a tall strip
    // whose height exceeds the old 8192 per-side cap still decodes, because
    // 64 × 9000 = 576_000 px is far under the pixel budget. This is the
    // tall-screenshot (full-page capture) case a per-side cap wrongly rejected.
    let out = decode_raster_oriented(&png(64, 9000)).expect("tall in-budget image should decode");
    assert_eq!(out.dimensions(), (64, 9000));
}

#[test]
fn decode_raster_oriented_rejects_over_budget_pixels() {
    // A header declaring more than the pixel budget is rejected from its
    // *declared* size, before any allocation, even though each side is far
    // under the generous per-side sanity cap. A PNM (whose decoder honors no
    // `image::Limits` allocation limit) proves the rejection is the area guard
    // itself, not `max_alloc`.
    let error = decode_raster_oriented(b"P6\n40000 40000\n255\n").unwrap_err();
    match error {
        DecodeError::OverBudget {
            width,
            height,
            pixels,
            budget,
        } => {
            assert_eq!((width, height), (40_000, 40_000));
            assert_eq!(pixels, 1_600_000_000);
            assert_eq!(budget, MAX_DECODE_PIXELS);
        }
        other => panic!("expected the pixel-budget guard, got: {other}"),
    }
}

#[test]
fn probe_dimensions_reports_the_header_size() {
    assert_eq!(probe_dimensions(&png(7, 5)).expect("probe the png"), (7, 5));
}

#[test]
fn probe_dimensions_enforces_the_same_guard_as_the_decode() {
    // The probe must never become a way around the decompression-bomb bound that
    // the full decode enforces.
    assert!(matches!(
        probe_dimensions(b"P6\n40000 40000\n255\n"),
        Err(DecodeError::OverBudget { .. })
    ));
    assert!(matches!(
        probe_dimensions(&[1, 2, 3]),
        Err(DecodeError::UnrecognizedFormat)
    ));
}

#[test]
fn probe_dimensions_reports_the_size_the_decode_produces() {
    // A quarter-turned photo decodes with its sides swapped, so the probe must
    // answer with the *post-rotation* extent: an image is shown at the size its
    // pixels decode to, and Exif 5-8 are the turning orientations.
    for orientation in [5, 6, 7, 8] {
        let bytes = jpeg_with_orientation(4, 3, orientation);
        assert_eq!(
            probe_dimensions(&bytes).expect("probe the tagged jpeg"),
            (3, 4),
            "orientation {orientation} turns the picture"
        );
        assert_eq!(
            decode_raster_oriented(&bytes)
                .expect("decode the tagged jpeg")
                .dimensions(),
            (3, 4),
            "orientation {orientation} must agree with the probe"
        );
    }
    // The non-turning orientations keep the file's own extent, which is what
    // pins the swap to exactly the four quarter-turn cases.
    for orientation in [1, 2, 3, 4] {
        let bytes = jpeg_with_orientation(4, 3, orientation);
        assert_eq!(
            probe_dimensions(&bytes).expect("probe the tagged jpeg"),
            (4, 3),
            "orientation {orientation} does not turn the picture"
        );
        assert_eq!(
            decode_raster_oriented(&bytes)
                .expect("decode the tagged jpeg")
                .dimensions(),
            (4, 3)
        );
    }
}
