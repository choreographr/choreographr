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
//! Supported sources, all normalized to provider-allowlisted PNG (alpha) or
//! JPEG (opaque):
//!   - every raster format the `image` crate decodes (JPEG, PNG, GIF, WebP,
//!     BMP, TIFF, TGA, DDS, ICO, PNM, HDR/Radiance, OpenEXR, Farbfeld, QOI);
//!   - AVIF — only when the gated `avif` feature is enabled (`image/avif-native`,
//!     dav1d). Recognized but rejected otherwise.
//!   - HEIC/HEIF — decoded via the pure-Rust `heif-oxide` crate (built in);
//!   - SVG — rasterized via `resvg`.
//!
//! EXIF orientation baking: raster formats (JPEG, WebP, and PNG's `eXIf`
//! chunk) are rotated/flipped in place to the orientation the header declares,
//! so phone/camera photos reach the model upright. HEIC carries its own
//! orientation and is already applied by `heif-oxide`; SVG has no orientation.
//!
//! Fixed constants (the vision plan chose fixed limits over configurable
//! ones): images are downscaled to fit within [`MAX_IMAGE_DIMENSION`] px on
//! the longest edge, decoded under a decompression-bomb guard (the shared
//! [`choreo_image::decode_raster_oriented`] uses [`image::Limits`];
//! [`choreo_image::decode_heic`] gates hostile declared geometry pre-decode),
//! and re-encoded to PNG (when the image has alpha) or JPEG (opaque) so the
//! wire bytes are always in a provider-allowlisted format.

use std::io::{Cursor, Read};
use std::path::Path;
use std::sync::Arc;

use image::{DynamicImage, GenericImageView, ImageFormat, RgbaImage};
use resvg::{tiny_skia, usvg};
use tracing::{debug, warn};

/// Longest-edge cap after normalization (px). Matches the common 2000px
/// default across the surveyed agents and comfortably fits every provider's
/// per-image limits.
pub const MAX_IMAGE_DIMENSION: u32 = 2000;
/// Hard cap on the source file size we are willing to read (MiB). Larger
/// inputs are rejected before any decode attempt.
pub const MAX_SOURCE_BYTES: usize = 20 * 1024 * 1024;
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
        warn!(
            len = buf.len(),
            max = MAX_SOURCE_BYTES,
            "image exceeds the maximum source size",
        );
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
    if is_heic(bytes) {
        normalize_heic(bytes)
    } else if is_svg(bytes) {
        normalize_svg(bytes)
    } else {
        normalize_raster(bytes)
    }
}

/// The raster formats the `image` crate can decode in this build.
///
/// AVIF is gated behind the `avif` feature (`image/avif-native`): recognized
/// by magic even without it, but only decodable/rejected-there-after when on.
fn is_supported_raster(format: ImageFormat) -> bool {
    matches!(
        format,
        ImageFormat::Jpeg
            | ImageFormat::Png
            | ImageFormat::Gif
            | ImageFormat::WebP
            | ImageFormat::Pnm
            | ImageFormat::Tiff
            | ImageFormat::Tga
            | ImageFormat::Dds
            | ImageFormat::Bmp
            | ImageFormat::Ico
            | ImageFormat::Hdr
            | ImageFormat::OpenExr
            | ImageFormat::Farbfeld
            | ImageFormat::Qoi
    ) || (format == ImageFormat::Avif && cfg!(feature = "avif"))
}

/// Normalize a raster image via the `image` crate, baking EXIF orientation.
fn normalize_raster(bytes: &[u8]) -> std::io::Result<PreparedVisionImage> {
    let format = image::guess_format(bytes)
        .map_err(|e| std::io::Error::new(std::io::ErrorKind::InvalidData, e.to_string()))?;
    if !is_supported_raster(format) {
        warn!(?format, "unsupported image format");
        return Err(std::io::Error::new(
            std::io::ErrorKind::InvalidData,
            format!("unsupported image format: {format:?}"),
        ));
    }

    // The shared decoder applies the decompression-bomb `image::Limits` guard,
    // bakes EXIF orientation (JPEG/WebP/PNG-eXIf) in one pass, and rejects the
    // source on failure (genuinely undecodable, or the guard firing).
    let img = choreo_image::decode_raster_oriented(bytes).map_err(|e| {
        warn!(error = %e, "failed to decode image (unsupported or decompression-bomb source)");
        std::io::Error::new(std::io::ErrorKind::InvalidData, e)
    })?;
    finalize(img, &format!("{format:?}"))
}

/// Normalize a HEIC/HEIF image via the shared pure-Rust decoder.
///
/// [`choreo_image::decode_heic`] applies a pre-decode allocation guard (the
/// container's declared `ispe` extents) so a hostile HEIC cannot drive a huge
/// allocation, then applies the container's orientation and delivers
/// display-ready sRGB.
fn normalize_heic(bytes: &[u8]) -> std::io::Result<PreparedVisionImage> {
    let img = choreo_image::decode_heic(bytes)
        .map_err(|e| std::io::Error::new(std::io::ErrorKind::InvalidData, e))?;
    finalize(img, "heic")
}

/// Normalize an SVG by rasterizing it to an RGBA bitmap via `resvg`.
fn normalize_svg(bytes: &[u8]) -> std::io::Result<PreparedVisionImage> {
    let img = rasterize_svg(bytes)?;
    finalize(img, "svg")
}

/// Resize to [`MAX_IMAGE_DIMENSION`] and re-encode to PNG (alpha) or JPEG
/// (opaque). Shared by every source so the wire bytes are consistent.
fn finalize(img: DynamicImage, source_label: &str) -> std::io::Result<PreparedVisionImage> {
    let (source_width, source_height) = img.dimensions();

    // Resize the longest edge down to MAX_IMAGE_DIMENSION, preserving aspect.
    let resized = if source_width > MAX_IMAGE_DIMENSION || source_height > MAX_IMAGE_DIMENSION {
        img.resize(
            MAX_IMAGE_DIMENSION,
            MAX_IMAGE_DIMENSION,
            image::imageops::FilterType::Lanczos3,
        )
    } else {
        img
    };

    // `ColorType::has_alpha` (image 0.25.6) rather than `DynamicImage::has_alpha`
    // (added in 0.25.8): blitz-dom pins image to =0.25.6 workspace-wide.
    let (data, mime_type) = if resized.color().has_alpha() {
        (encode_png(&resized)?, "image/png")
    } else {
        (encode_jpeg(&resized)?, "image/jpeg")
    };
    debug!(
        source = source_label,
        source_width,
        source_height,
        mime = mime_type,
        output_bytes = data.len(),
        "normalized image",
    );
    let (width, height) = resized.dimensions();

    Ok(PreparedVisionImage {
        data,
        mime_type,
        width,
        height,
    })
}

/// Rasterize SVG bytes to an RGBA bitmap. Faces are rendered at the SVG's
/// intrinsic size, capped to [`MAX_IMAGE_DIMENSION`]; `finalize` downscales
/// anything larger with Lanczos3.
fn rasterize_svg(bytes: &[u8]) -> std::io::Result<DynamicImage> {
    let mut options = usvg::Options::default();
    // Load system fonts so `<text>` elements render (matching the TUI's SVG
    // rasterizer). Failure to load is non-fatal — missing glyphs are skipped.
    Arc::make_mut(&mut options.fontdb).load_system_fonts();
    let tree = usvg::Tree::from_data(bytes, &options)
        .map_err(|e| std::io::Error::new(std::io::ErrorKind::InvalidData, e.to_string()))?;

    let size = tree.size();
    let intrinsic_w = size.width();
    let intrinsic_h = size.height();
    let longest = intrinsic_w.max(intrinsic_h);
    let scale = if longest > MAX_IMAGE_DIMENSION as f32 {
        MAX_IMAGE_DIMENSION as f32 / longest
    } else {
        1.0
    };
    let out_w = (intrinsic_w * scale).ceil().max(1.0) as u32;
    let out_h = (intrinsic_h * scale).ceil().max(1.0) as u32;

    let mut pixmap = tiny_skia::Pixmap::new(out_w, out_h).ok_or_else(|| {
        std::io::Error::new(
            std::io::ErrorKind::InvalidData,
            "svg dimensions are too large to rasterize",
        )
    })?;
    resvg::render(
        &tree,
        tiny_skia::Transform::from_scale(scale, scale),
        &mut pixmap.as_mut(),
    );
    let rgba = RgbaImage::from_raw(out_w, out_h, pixmap.take()).ok_or_else(|| {
        std::io::Error::new(
            std::io::ErrorKind::InvalidData,
            "failed to build raster image from svg",
        )
    })?;
    Ok(DynamicImage::ImageRgba8(rgba))
}

/// Detect the HEIC/HEIF `ftyp` container: an ISO-BMFF file whose major or
/// compatible brand is an HEVC/HEIF brand. An explicit AVIF brand (`avif`/`avis`)
/// disqualifies the file so AVIF routes to the (gated) `image` decoder instead
/// — AVIF and HEIC are both HEIF container brands, so the generic `mif1`/`msf1`
/// brands alone are ambiguous and cannot be trusted to pick the HEIC path.
fn is_heic(bytes: &[u8]) -> bool {
    if bytes.len() < 12 || &bytes[4..8] != b"ftyp" {
        return false;
    }
    // The first four bytes are the box size (big-endian). 0 = to EOF; 1 =
    // extended size (size in a following 8-byte field) — both mean "scan to
    // the end of what we have" for brand detection.
    let size = u32::from_be_bytes([bytes[0], bytes[1], bytes[2], bytes[3]]);
    let box_end = match size {
        0 | 1 => bytes.len(),
        n => (n as usize).min(bytes.len()),
    };
    let mut has_heif_brand = false;
    let mut off = 8;
    while off + 4 <= box_end {
        let brand = &bytes[off..off + 4];
        if matches!(brand, b"avif" | b"avis") {
            return false;
        }
        if matches!(
            brand,
            b"heic" | b"heix" | b"hevc" | b"hevx" | b"heif" | b"heim" | b"heis" | b"mif1" | b"msf1"
        ) {
            has_heif_brand = true;
        }
        off += 4;
    }
    has_heif_brand
}

/// Detect SVG by content: skip leading whitespace (and any XML declaration),
/// then look for an `<svg` root tag within the first 512 bytes. `resvg` does
/// full validation on parse; this is only a cheap pre-routing heuristic so
/// SVG never goes through the raster sniff. The search window is bounded and
/// case-insensitive, and a genuine raster never starts with `<`, so real
/// images are never misrouted here.
fn is_svg(bytes: &[u8]) -> bool {
    let trimmed = bytes
        .iter()
        .position(|b| !b.is_ascii_whitespace())
        .map(|i| &bytes[i..])
        .unwrap_or(bytes);
    if trimmed.first() != Some(&b'<') {
        return false;
    }
    let window = &trimmed[..trimmed.len().min(512)];
    let lower = window.to_ascii_lowercase();
    lower.windows(4).any(|w| w == b"<svg")
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
    use image::{ImageBuffer, Rgb, Rgba};

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

    #[cfg(not(feature = "avif"))]
    #[test]
    fn gated_avif_is_rejected_when_feature_disabled() {
        // An AVIF `ftyp` header is recognized by magic regardless, but is only
        // accepted when the gated `avif` feature is enabled.
        let err = normalize_bytes(b"\0\0\0\x18ftypavif").unwrap_err();
        assert!(
            err.to_string().contains("unsupported image format"),
            "{err}"
        );
    }

    #[test]
    fn bmp_is_now_supported() {
        // BMP is a guessable raster format and is in the supported set.
        // (This is a decode that will fail on truncation, not an unsupported
        // format — assert it is *not* the unsupported-format rejection.)
        let err = normalize_bytes(b"BM\0\0\0\0\0\0\0\0").unwrap_err();
        assert!(
            !err.to_string().contains("unsupported image format"),
            "{err}"
        );
    }

    #[test]
    fn empty_bytes_are_rejected() {
        assert!(normalize_bytes(&[]).is_err());
    }

    #[test]
    fn heic_is_detected_by_ftyp_brand() {
        assert!(is_heic(b"\0\0\0\x18ftypheic\x00\x00\x00\x00heicmif1"));
        assert!(is_heic(b"\0\0\0\x18ftypheix\x00\x00\x00\x00mif1heix"));
        // AVIF must NOT be routed to the HEIC decoder.
        assert!(!is_heic(b"\0\0\0\x18ftypavif\x00\x00\x00\x00avifmif1"));
        assert!(!is_heic(b"not a box at all"));
    }

    #[test]
    fn svg_is_detected_by_content() {
        assert!(is_svg(b"<svg xmlns='http://www.w3.org/2000/svg'></svg>"));
        assert!(is_svg(b"  \n<?xml version='1.0'?><svg></svg>"));
        assert!(!is_svg(b"\x89PNG\r\n\x1a\n"));
        assert!(!is_svg(b"plain text"));
    }
}
