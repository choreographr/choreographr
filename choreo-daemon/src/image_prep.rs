//! Vision image normalization: turn a raw image file on disk into a
//! well-formed image the provider serializers can send.
//!
//! The `read_image` tool calls this to produce metadata (MIME + dimensions)
//! for its text handle and the durable [`ImageReference`], and the request
//! builder calls it again at request time to produce the actual bytes
//! (pass-through design: no artifact store, so the file is re-read and
//! re-normalized on every request). Keeping both paths on one function means
//! the handle text reports the same dimensions the model actually sees.
//!
//! Fixed constants (the vision plan chose fixed limits over configurable
//! ones): images are downscaled to fit within [`MAX_IMAGE_DIMENSION`] px on
//! the longest edge, decoded under a decompression-bomb [`image::Limits`],
//! and re-encoded to PNG (when the image has alpha) or JPEG (opaque) so the
//! wire bytes are always in a provider-allowlisted format.
//!
//! EXIF orientation baking is deliberately **deferred** (a future
//! enhancement): the `image` crate exposes `ImageDecoder::orientation` and
//! `DynamicImage::apply_orientation`, but applying it would require a second
//! decode pass; the common case (images without an EXIF orientation tag) is
//! a no-op, and providers tolerate untransposed EXIF for most inputs.

use std::io::{Cursor, Read};
use std::path::Path;

use image::{DynamicImage, GenericImageView, ImageFormat, ImageReader};

/// Longest-edge cap after normalization (px). Matches the common 2000px
/// default across the surveyed agents and comfortably fits every provider's
/// per-image limits.
pub const MAX_IMAGE_DIMENSION: u32 = 2000;
/// Hard cap on the source file size we are willing to read (MiB). Larger
/// inputs are rejected before any decode attempt.
pub const MAX_SOURCE_BYTES: usize = 20 * 1024 * 1024;
/// Cap on the source image dimensions used to size the decoder's
/// decompression-bomb guard (px per side).
pub const MAX_SOURCE_DIMENSION: u32 = 8192;
/// Cap on total decoder allocation (bytes) — the decompression-bomb guard
/// that fires before a hostile image can allocate gigabytes.
pub const MAX_DECODE_ALLOC: u64 = 256 * 1024 * 1024;
/// JPEG re-encode quality for opaque images.
const JPEG_QUALITY: u8 = 85;

/// A normalized, ready-to-send image.
#[derive(Debug)]
pub struct PreparedVisionImage {
    pub data: Vec<u8>,
    /// `image/png` (alpha) or `image/jpeg` (opaque) after re-encode.
    pub mime_type: &'static str,
    pub width: u32,
    pub height: u32,
}

/// Read `path`, normalize it, and return the prepared image.
///
/// Fails (never panics) on: an oversized/unreadable file, an unsupported or
/// undecodable image format, or a decompression-bomb allocation. The caller
/// surfaces the error as a tool error or a placeholder text, never a crash.
pub fn load_and_normalize(path: &Path) -> std::io::Result<PreparedVisionImage> {
    let bytes = read_bounded(path)?;
    normalize_bytes(&bytes)
}

/// Read a file with a hard [`MAX_SOURCE_BYTES`] bound, guarding against a
/// growing/FIFO source by reading cap+1 and detecting the overflow.
fn read_bounded(path: &Path) -> std::io::Result<Vec<u8>> {
    let file = std::fs::File::open(path)?;
    let mut buf = Vec::with_capacity(MAX_SOURCE_BYTES.min(1 << 20));
    // Read at most cap+1 bytes; `take` on a `Read` returns early once the
    // limit is reached, so an over-limit file is detected by the length check
    // below rather than being buffered whole.
    let mut capped = file.take((MAX_SOURCE_BYTES + 1) as u64);
    capped.read_to_end(&mut buf)?;
    if buf.len() > MAX_SOURCE_BYTES {
        return Err(std::io::Error::new(
            std::io::ErrorKind::InvalidData,
            format!(
                "image exceeds the maximum source size of {}",
                humfmt::bytes(MAX_SOURCE_BYTES as u64)
            ),
        ));
    }
    Ok(buf)
}

/// Normalize raw image bytes: sniff the format, decode under limits, resize
/// to [`MAX_IMAGE_DIMENSION`], and re-encode to PNG (alpha) or JPEG (opaque).
pub fn normalize_bytes(bytes: &[u8]) -> std::io::Result<PreparedVisionImage> {
    let format = image::guess_format(bytes)
        .map_err(|e| std::io::Error::new(std::io::ErrorKind::InvalidData, e.to_string()))?;
    // Restrict to the vision-capable raster formats (JPEG/PNG/GIF/WebP). The
    // animated GIF/WebP first-frame fallback is acceptable for v1; the
    // decoder reads only the first frame.
    if !matches!(
        format,
        ImageFormat::Jpeg | ImageFormat::Png | ImageFormat::Gif | ImageFormat::WebP
    ) {
        return Err(std::io::Error::new(
            std::io::ErrorKind::InvalidData,
            format!("unsupported image format: {format:?}"),
        ));
    }

    let mut reader = ImageReader::new(Cursor::new(bytes)).with_guessed_format()?;
    // Decompression-bomb guard: bound the decode before any large allocation.
    // `Limits` is `#[non_exhaustive]`, so start from the default and set the
    // public fields via mutation (construction is forbidden).
    let mut limits = image::Limits::default();
    limits.max_image_width = Some(MAX_SOURCE_DIMENSION);
    limits.max_image_height = Some(MAX_SOURCE_DIMENSION);
    limits.max_alloc = Some(MAX_DECODE_ALLOC);
    reader.limits(limits);
    let img = reader.decode().map_err(io_err)?;
    let (width, height) = img.dimensions();

    // Resize the longest edge down to MAX_IMAGE_DIMENSION, preserving aspect.
    let resized = if width > MAX_IMAGE_DIMENSION || height > MAX_IMAGE_DIMENSION {
        img.resize(
            MAX_IMAGE_DIMENSION,
            MAX_IMAGE_DIMENSION,
            image::imageops::FilterType::Lanczos3,
        )
    } else {
        img
    };

    let (data, mime_type) = if resized.has_alpha() {
        (encode_png(&resized)?, "image/png")
    } else {
        (encode_jpeg(&resized)?, "image/jpeg")
    };
    let (width, height) = resized.dimensions();

    Ok(PreparedVisionImage {
        data,
        mime_type,
        width,
        height,
    })
}

/// Re-encode a `DynamicImage` as PNG (lossless — used when the image has an
/// alpha channel so transparency survives).
fn encode_png(img: &DynamicImage) -> std::io::Result<Vec<u8>> {
    let mut out = Cursor::new(Vec::new());
    img.write_to(&mut out, ImageFormat::Png).map_err(io_err)?;
    Ok(out.into_inner())
}

/// Re-encode an opaque `DynamicImage` as JPEG at [`JPEG_QUALITY`] (smaller
/// than PNG for photographic content).
fn encode_jpeg(img: &DynamicImage) -> std::io::Result<Vec<u8>> {
    use image::ExtendedColorType;
    use image::codecs::jpeg::JpegEncoder;
    let rgb = img.to_rgb8();
    let mut out = Cursor::new(Vec::new());
    let mut encoder = JpegEncoder::new_with_quality(&mut out, JPEG_QUALITY);
    encoder
        .encode(&rgb, rgb.width(), rgb.height(), ExtendedColorType::Rgb8)
        .map_err(io_err)?;
    Ok(out.into_inner())
}

fn io_err(e: image::ImageError) -> std::io::Error {
    std::io::Error::new(std::io::ErrorKind::InvalidData, e.to_string())
}

#[cfg(test)]
mod tests {
    use super::*;
    use image::{DynamicImage, ImageBuffer, Rgb, Rgba};

    /// A tiny opaque image (3×2) to exercise the JPEG re-encode path.
    fn opaque_rgb() -> DynamicImage {
        let buf: ImageBuffer<Rgb<u8>, Vec<u8>> =
            ImageBuffer::from_fn(3, 2, |x, y| Rgb([(x * 80) as u8, (y * 90) as u8, 40]));
        DynamicImage::ImageRgb8(buf)
    }

    fn as_png(img: &DynamicImage) -> Vec<u8> {
        let mut out = Cursor::new(Vec::new());
        img.write_to(&mut out, ImageFormat::Png).unwrap();
        out.into_inner()
    }

    #[test]
    fn opaque_image_reencodes_to_jpeg() {
        let bytes = as_png(&opaque_rgb());
        let out = normalize_bytes(&bytes).unwrap();
        assert_eq!(out.mime_type, "image/jpeg");
        assert_eq!(out.width, 3);
        assert_eq!(out.height, 2);
        // Re-encodes to a decodable JPEG.
        let decoded = image::load_from_memory(&out.data).unwrap();
        assert_eq!(decoded.dimensions(), (3, 2));
    }

    #[test]
    fn transparent_image_reencodes_to_png() {
        let buf: ImageBuffer<Rgba<u8>, Vec<u8>> = ImageBuffer::from_fn(4, 4, |x, y| {
            Rgba([x as u8, y as u8, 0, if x % 2 == 0 { 0 } else { 255 }])
        });
        let img = DynamicImage::ImageRgba8(buf);
        let bytes = as_png(&img);
        let out = normalize_bytes(&bytes).unwrap();
        assert_eq!(out.mime_type, "image/png");
        assert_eq!(out.width, 4);
        assert_eq!(out.height, 4);
    }

    #[test]
    fn oversized_image_is_downscaled() {
        // A 4000×2000 image (longest edge 4000 > 2000) is downscaled to fit.
        let buf: ImageBuffer<Rgba<u8>, Vec<u8>> =
            ImageBuffer::from_fn(4000, 2000, |x, y| Rgba([x as u8, y as u8, 100, 255]));
        let img = DynamicImage::ImageRgba8(buf);
        let bytes = as_png(&img);
        let out = normalize_bytes(&bytes).unwrap();
        assert!(out.width <= MAX_IMAGE_DIMENSION);
        assert!(out.height <= MAX_IMAGE_DIMENSION);
        // Aspect ratio preserved (2000×1000).
        assert_eq!(out.width, 2000);
        assert_eq!(out.height, 1000);
    }

    #[test]
    fn unsupported_format_is_rejected() {
        // BMP is a guessable format but not in the vision allowlist → rejected
        // with the dedicated message (not a guess failure).
        let err = normalize_bytes(b"BM\0\0\0\0\0\0\0\0").unwrap_err();
        assert!(err.to_string().contains("unsupported image format"));
    }

    #[test]
    fn empty_bytes_are_rejected() {
        assert!(normalize_bytes(&[]).is_err());
    }
}
