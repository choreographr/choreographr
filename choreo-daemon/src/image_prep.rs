//! Vision image normalization: turn a raw image file on disk into a
//! well-formed image the provider serializers can send.
//!
//! The `read_image` tool calls this to produce metadata (MIME + dimensions)
//! for its text handle and the durable `ImageReference`, and the request
//! builder calls it again at request time to produce the actual bytes
//! (pass-through design: no artifact store, so the file is re-read and
//! re-normalized on every request). Keeping both paths on one function means
//! the handle text reports the same dimensions the model actually sees.
//!
//! Supported sources, all normalized to provider-allowlisted PNG (alpha) or
//! JPEG (opaque):
//!   - every raster format the `image` crate decodes (JPEG, PNG, GIF, WebP,
//!     BMP, TIFF, TGA, DDS, ICO, PNM, HDR/Radiance, `OpenEXR`, Farbfeld, QOI);
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
//!
//! Optional crop: both [`load_and_normalize`] and [`normalize_bytes`] accept a
//! [`CropRegion`] expressed as fractions of the displayed extent. Raster and
//! HEIC sources are cropped from the decoded pixels (after EXIF orientation is
//! baked); SVG sources pass the region into the rasterizer so only that
//! rectangle of the vector tree is drawn — so a crop keeps full source detail
//! instead of downscaling the whole image first. With no region the pipeline is
//! byte-for-byte unchanged, and the crop lands *before* `finalize`, so a
//! region smaller than [`MAX_IMAGE_DIMENSION`] bypasses the downscale entirely.

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
    /// The re-encoded image bytes in the provider-allowlisted format below.
    pub data: Vec<u8>,
    /// `image/png` (alpha) or `image/jpeg` (opaque) after re-encode.
    pub mime_type: &'static str,
    /// Width in pixels after downscale/crop — the same value the `read_image`
    /// tool reports in its handle and the model actually sees.
    pub width: u32,
    /// Height in pixels after downscale/crop.
    pub height: u32,
}

/// A sub-rectangle of an image, given as fractions of its displayed extent.
///
/// Fractions (rather than pixels) keep a crop request independent of the source
/// file's resolution — a Retina screenshot is twice its CSS size — and of the
/// normalization downscale, so the same `region` selects the same visual area
/// whatever the file's pixel dimensions. The caller validates the fractions;
/// [`CropRegion::to_pixels`] still clamps defensively.
#[derive(Debug, Clone, Copy)]
pub struct CropRegion {
    /// Left edge as a fraction of the displayed width (0.0 = left).
    pub x: f32,
    /// Top edge as a fraction of the displayed height (0.0 = top).
    pub y: f32,
    /// Region width as a fraction of the displayed width.
    pub width: f32,
    /// Region height as a fraction of the displayed height.
    pub height: f32,
}

/// A pixel rectangle resolved from a [`CropRegion`] against a concrete image
/// size, in the decoded image's coordinate space (after EXIF orientation is
/// baked).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct PixelCrop {
    /// Left edge in decoded-image pixels.
    pub x: u32,
    /// Top edge in decoded-image pixels.
    pub y: u32,
    /// Crop width in pixels (non-zero — an empty crop is rejected).
    pub width: u32,
    /// Crop height in pixels (non-zero — an empty crop is rejected).
    pub height: u32,
}

impl CropRegion {
    /// The region clamped to the unit square as `(x0, y0, x1, y1)`, or `None`
    /// when it is empty (zero area) after clamping.
    ///
    /// The single source of the "empty region is rejected" rule: both the
    /// pixel-crop path ([`CropRegion::to_pixels`]) and the SVG rasterizer
    /// (`rasterize_svg`, which needs the fractional edges directly) resolve the
    /// edges here, so the two can never disagree about what counts as empty.
    fn normalized_bounds(self) -> Option<(f32, f32, f32, f32)> {
        let x0 = self.x.clamp(0.0, 1.0);
        let y0 = self.y.clamp(0.0, 1.0);
        let x1 = (self.x + self.width).clamp(0.0, 1.0);
        let y1 = (self.y + self.height).clamp(0.0, 1.0);
        if x1 <= x0 || y1 <= y0 {
            return None;
        }
        Some((x0, y0, x1, y1))
    }

    /// Resolve the fractional region against an image of `width` × `height`
    /// pixels, clamped to the image bounds.
    ///
    /// Returns `None` when the region is empty (zero area after rounding) or
    /// lies entirely outside the image, so callers reject it rather than crop
    /// to nothing.
    #[must_use]
    pub fn to_pixels(self, width: u32, height: u32) -> Option<PixelCrop> {
        let (x0, y0, x1, y1) = self.normalized_bounds()?;
        // Round each edge to the nearest pixel; `scaled_edge` clamps to
        // `[0, extent]`, so the resulting rectangle is always in bounds.
        let left = scaled_edge(f64::from(x0), width);
        let top = scaled_edge(f64::from(y0), height);
        let right = scaled_edge(f64::from(x1), width);
        let bottom = scaled_edge(f64::from(y1), height);
        let crop_width = right.saturating_sub(left);
        let crop_height = bottom.saturating_sub(top);
        if crop_width == 0 || crop_height == 0 {
            return None;
        }
        Some(PixelCrop {
            x: left,
            y: top,
            width: crop_width,
            height: crop_height,
        })
    }
}

/// Map a `[0, 1]` fraction to a pixel edge along an `extent`-pixel axis,
/// rounding to the nearest pixel and clamping to `[0, extent]`.
// The clamped value is within `[0, extent]` (≤ u32::MAX), so the cast is exact
// and never negative — the lints cannot fire here by construction.
#[expect(clippy::cast_possible_truncation, clippy::cast_sign_loss)]
fn scaled_edge(fraction: f64, extent: u32) -> u32 {
    (fraction * f64::from(extent))
        .round()
        .clamp(0.0, f64::from(extent)) as u32
}

/// Read `path`, normalize it, and return the prepared image.
///
/// Fails (never panics) on: an oversized/unreadable file, an unsupported or
/// undecodable image format, or a decompression-bomb allocation. The caller
/// surfaces the error as a tool error or a placeholder text, never a crash.
///
/// # Errors
///
/// Returns Err if the file cannot be read (missing, unreadable, or over
/// [`MAX_SOURCE_BYTES`]) or normalization fails (see [`normalize_bytes`]).
pub fn load_and_normalize(
    path: &Path,
    region: Option<CropRegion>,
) -> std::io::Result<PreparedVisionImage> {
    let bytes = read_bounded(path)?;
    normalize_bytes(&bytes, region)
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
///
/// # Errors
///
/// Returns Err on an unsupported or undecodable image format, a
/// decompression-bomb allocation, or a re-encode failure.
pub fn normalize_bytes(
    bytes: &[u8],
    region: Option<CropRegion>,
) -> std::io::Result<PreparedVisionImage> {
    if is_heic(bytes) {
        normalize_heic(bytes, region)
    } else if is_svg(bytes) {
        normalize_svg(bytes, region)
    } else {
        normalize_raster(bytes, region)
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
fn normalize_raster(
    bytes: &[u8],
    region: Option<CropRegion>,
) -> std::io::Result<PreparedVisionImage> {
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
    finalize_region(img, &format!("{format:?}"), region)
}

/// Normalize a HEIC/HEIF image via the shared pure-Rust decoder.
///
/// [`choreo_image::decode_heic`] applies a pre-decode allocation guard (the
/// container's declared `ispe` extents) so a hostile HEIC cannot drive a huge
/// allocation, then applies the container's orientation and delivers
/// display-ready sRGB.
fn normalize_heic(
    bytes: &[u8],
    region: Option<CropRegion>,
) -> std::io::Result<PreparedVisionImage> {
    let img = choreo_image::decode_heic(bytes)
        .map_err(|e| std::io::Error::new(std::io::ErrorKind::InvalidData, e))?;
    finalize_region(img, "heic", region)
}

/// Normalize an SVG by rasterizing it to an RGBA bitmap via `resvg`.
fn normalize_svg(bytes: &[u8], region: Option<CropRegion>) -> std::io::Result<PreparedVisionImage> {
    // The region is applied *during* rasterization (see `rasterize_svg`), so the
    // output here is already the crop and needs only the shared downscale/encode.
    let img = rasterize_svg(bytes, region)?;
    finalize(img, "svg")
}

/// Crop a decoded image to `region`, returning the sub-image.
///
/// Returns `Err` when the region resolves to no pixels of this image, so a
/// caller never silently receives the whole image for an empty region.
fn crop_decoded(img: &DynamicImage, region: CropRegion) -> std::io::Result<DynamicImage> {
    let (width, height) = img.dimensions();
    let Some(rect) = region.to_pixels(width, height) else {
        return Err(std::io::Error::new(
            std::io::ErrorKind::InvalidInput,
            "crop region does not overlap the image",
        ));
    };
    Ok(img.crop_imm(rect.x, rect.y, rect.width, rect.height))
}

/// Crop (when a region is given) and then resize/re-encode. Shared by the raster
/// and HEIC paths, which both start from a full-size decoded image.
fn finalize_region(
    img: DynamicImage,
    source_label: &str,
    region: Option<CropRegion>,
) -> std::io::Result<PreparedVisionImage> {
    let img = match region {
        Some(region) => crop_decoded(&img, region)?,
        None => img,
    };
    finalize(img, source_label)
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

/// Rasterize SVG bytes to an RGBA bitmap.
///
/// With no `region` the whole viewport is drawn at its intrinsic size, capped to
/// [`MAX_IMAGE_DIMENSION`] (`finalize` downscales anything larger with Lanczos3).
/// With a `region`, only that rectangle is drawn — the region is resolved into
/// viewport coordinates, sized to its own extent, and the transform translates
/// its top-left to the pixmap origin — so the crop keeps the vector detail the
/// full-tree render would have downscaled away.
fn rasterize_svg(bytes: &[u8], region: Option<CropRegion>) -> std::io::Result<DynamicImage> {
    let mut options = usvg::Options::default();
    // Load system fonts so `<text>` elements render (matching the TUI's SVG
    // rasterizer). Failure to load is non-fatal — missing glyphs are skipped.
    Arc::make_mut(&mut options.fontdb).load_system_fonts();
    let tree = usvg::Tree::from_data(bytes, &options)
        .map_err(|e| std::io::Error::new(std::io::ErrorKind::InvalidData, e.to_string()))?;

    // Resolve the fractional region into the viewport's own coordinate space
    // (the space `tree.size()` reports, after the viewBox is applied), or the
    // whole viewport when no crop is requested. Cropping by *rendering only the
    // region* keeps full vector detail: downscaling the whole tree first and
    // cutting afterward would discard the detail the crop is meant to recover.
    let size = tree.size();
    let view_w = size.width();
    let view_h = size.height();
    let (origin_x, origin_y, region_w, region_h) = match region {
        Some(region) => {
            let (x0, y0, x1, y1) = region.normalized_bounds().ok_or_else(|| {
                std::io::Error::new(
                    std::io::ErrorKind::InvalidInput,
                    "crop region does not overlap the image",
                )
            })?;
            (
                x0 * view_w,
                y0 * view_h,
                (x1 - x0) * view_w,
                (y1 - y0) * view_h,
            )
        }
        None => (0.0, 0.0, view_w, view_h),
    };

    // Cap the *rendered region* (not the whole tree) to MAX_IMAGE_DIMENSION, so
    // a small crop renders at native resolution and only an oversized region is
    // downscaled.
    let longest = region_w.max(region_h);
    // f64→f32: the region extent feeds a raster target capped at
    // MAX_IMAGE_DIMENSION (2000 px), where f32 precision is far beyond pixel
    // granularity.
    #[expect(clippy::cast_precision_loss)]
    let scale = if longest > MAX_IMAGE_DIMENSION as f32 {
        MAX_IMAGE_DIMENSION as f32 / longest
    } else {
        1.0
    };
    // f64→u32 raster dimensions: values are ceil()ed and clamped to >= 1
    // before the cast, and the Pixmap::new allocation below fails if they
    // exceed the rasterizer's limits — identical behavior, just lint-silenced.
    #[expect(clippy::cast_possible_truncation, clippy::cast_sign_loss)]
    let out_w = (region_w * scale).ceil().max(1.0) as u32;
    #[expect(clippy::cast_possible_truncation, clippy::cast_sign_loss)]
    let out_h = (region_h * scale).ceil().max(1.0) as u32;

    let mut pixmap = tiny_skia::Pixmap::new(out_w, out_h).ok_or_else(|| {
        std::io::Error::new(
            std::io::ErrorKind::InvalidData,
            "svg dimensions are too large to rasterize",
        )
    })?;
    // Scale by `scale` and shift the region's top-left corner to the pixmap
    // origin: a viewport point (vx, vy) lands at ((vx - origin_x) * scale,
    // (vy - origin_y) * scale). Content outside the pixmap is clipped.
    let transform = tiny_skia::Transform::from_row(
        scale,
        0.0,
        0.0,
        scale,
        -origin_x * scale,
        -origin_y * scale,
    );
    resvg::render(&tree, transform, &mut pixmap.as_mut());
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
    // Compose the length guard with the accesses so every index is proven in
    // bounds (clippy::indexing_slicing); `bytes.get(..8)` over the guarded
    // prefix keeps the same `b"ftyp"` comparison.
    let Some(head) = bytes.get(..12) else {
        return false;
    };
    if bytes.get(4..8) != Some(b"ftyp") {
        return false;
    }
    // The first four bytes are the box size (big-endian). 0 = to EOF; 1 =
    // extended size (size in a following 8-byte field) — both mean "scan to
    // the end of what we have" for brand detection.
    // `head` is at least 12 bytes, so the range is in bounds.
    let size = u32::from_be_bytes(
        head.get(..4)
            .and_then(|s| s.try_into().ok())
            .unwrap_or([0u8; 4]),
    );
    let box_end = match size {
        0 | 1 => bytes.len(),
        n => (n as usize).min(bytes.len()),
    };
    let mut has_heif_brand = false;
    let mut off = 8;
    while off + 4 <= box_end {
        // `off + 4 <= box_end <= bytes.len()`, so the range is always in bounds.
        let brand = bytes.get(off..off + 4).unwrap_or_default();
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
        // `position` returns a valid index; fallback keeps the original slice.
        .and_then(|i| bytes.get(i..))
        .unwrap_or(bytes);
    if trimmed.first() != Some(&b'<') {
        return false;
    }
    // Bounded window slice; the min() keeps the range in bounds.
    let window = trimmed.get(..trimmed.len().min(512)).unwrap_or(trimmed);
    let lower = window.to_ascii_lowercase();
    lower.windows(4).any(|w| w == b"<svg")
}

/// Re-encode a `DynamicImage` as PNG (lossless — used when the image has an
/// alpha channel so transparency survives).
fn encode_png(img: &DynamicImage) -> std::io::Result<Vec<u8>> {
    let mut out = Cursor::new(Vec::new());
    img.write_to(&mut out, ImageFormat::Png)
        .map_err(|e| io_err(&e))?;
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
        .map_err(|e| io_err(&e))?;
    Ok(out.into_inner())
}

fn io_err(e: &image::ImageError) -> std::io::Error {
    std::io::Error::new(std::io::ErrorKind::InvalidData, e.to_string())
}

#[cfg(test)]
mod tests {
    use super::*;
    use image::{ImageBuffer, Rgb, Rgba};

    /// A tiny opaque image (3×2) to exercise the JPEG re-encode path.
    fn opaque_rgb() -> DynamicImage {
        let buf: ImageBuffer<Rgb<u8>, Vec<u8>> = ImageBuffer::from_fn(3, 2, |x, y| {
            // u8 pixel coordinates (3×2 image): the arithmetic never exceeds u8.
            #[expect(clippy::cast_possible_truncation)]
            Rgb([(x * 80) as u8, (y * 90) as u8, 40])
        });
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
        let out = normalize_bytes(&bytes, None).unwrap();
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
            // u8 pixel coordinates (4×4 image): the arithmetic never exceeds u8.
            #[expect(clippy::cast_possible_truncation)]
            Rgba([x as u8, y as u8, 0, if x % 2 == 0 { 0 } else { 255 }])
        });
        let img = DynamicImage::ImageRgba8(buf);
        let bytes = as_png(&img);
        let out = normalize_bytes(&bytes, None).unwrap();
        assert_eq!(out.mime_type, "image/png");
        assert_eq!(out.width, 4);
        assert_eq!(out.height, 4);
    }

    #[test]
    fn fractional_crop_selects_the_sub_rectangle() {
        // 3×2 source; middle third by width, top half by height → 1×1 px.
        let bytes = as_png(&opaque_rgb());
        let region = CropRegion {
            x: 1.0 / 3.0,
            y: 0.0,
            width: 1.0 / 3.0,
            height: 0.5,
        };
        let out = normalize_bytes(&bytes, Some(region)).unwrap();
        assert_eq!((out.width, out.height), (1, 1));
        assert_eq!(out.mime_type, "image/jpeg");
    }

    #[test]
    fn empty_crop_region_is_rejected() {
        // A zero-width region resolves to no pixels — reject it rather than
        // silently returning the whole image.
        let bytes = as_png(&opaque_rgb());
        let region = CropRegion {
            x: 0.0,
            y: 0.0,
            width: 0.0,
            height: 1.0,
        };
        let err = normalize_bytes(&bytes, Some(region)).unwrap_err();
        assert!(err.to_string().contains("does not overlap"), "{err}");
    }

    #[test]
    fn full_region_matches_no_region() {
        // A full-image region must be a no-op: same dimensions as no crop.
        let bytes = as_png(&opaque_rgb());
        let region = CropRegion {
            x: 0.0,
            y: 0.0,
            width: 1.0,
            height: 1.0,
        };
        let cropped = normalize_bytes(&bytes, Some(region)).unwrap();
        assert_eq!((cropped.width, cropped.height), (3, 2));
    }

    #[test]
    fn svg_crop_renders_only_the_region() {
        // A 100×100 SVG cropped to the top-left quarter renders 50×25 at native
        // resolution: the region is drawn, not the whole tree downscaled.
        let svg = b"<svg xmlns='http://www.w3.org/2000/svg' width='100' height='100'>\
                    <rect width='100' height='100' fill='blue'/></svg>";
        let region = CropRegion {
            x: 0.0,
            y: 0.0,
            width: 0.5,
            height: 0.25,
        };
        let out = normalize_bytes(svg, Some(region)).unwrap();
        assert_eq!((out.width, out.height), (50, 25));
    }

    #[test]
    fn svg_crop_translates_to_the_requested_region() {
        // A left-black / right-white split SVG: cropping the right half must
        // show the white fill, proving the transform shifts the region to the
        // origin rather than merely resizing the output.
        let svg = b"<svg xmlns='http://www.w3.org/2000/svg' width='100' height='100'>\
                    <rect width='50' height='100' fill='black'/>\
                    <rect x='50' width='50' height='100' fill='white'/></svg>";
        let region = CropRegion {
            x: 0.5,
            y: 0.0,
            width: 0.5,
            height: 1.0,
        };
        let out = normalize_bytes(svg, Some(region)).unwrap();
        assert_eq!((out.width, out.height), (50, 100));
        let decoded = image::load_from_memory(&out.data).unwrap().to_rgb8();
        let pixel = decoded.get_pixel(25, 50).0;
        assert!(
            pixel[0] > 200 && pixel[1] > 200 && pixel[2] > 200,
            "expected the white right-hand fill, got {pixel:?}"
        );
    }

    #[test]
    fn oversized_image_is_downscaled() {
        // A 2002×1001 image (longest edge 2002 > the 2000 cap, exact 2:1
        // aspect) is downscaled to fit. Kept barely over the cap deliberately:
        // the unoptimized dev profile makes the Lanczos3 resample and BMP
        // decode the cost here, and the test only needs *some* longest edge
        // over the cap plus a clean 2:1 ratio (so the 2000×1000 output is
        // exact) — a huge source adds seconds of resampling for no extra
        // coverage.
        //
        // Encoded as UNCOMPRESSED BMP with an opaque (RGB, no-alpha) buffer:
        // the behavior under test is the *downscale*, and a PNG round-trip of a
        // multi-megapixel image costs seconds of deflate/inflate in the
        // unoptimized dev profile for zero added coverage (the transparent-PNG
        // re-encode path has its own tiny test below). Opaque input also keeps
        // the output on the JPEG re-encode path (no alpha), which is cheap.
        let buf: ImageBuffer<Rgb<u8>, Vec<u8>> = ImageBuffer::from_fn(2002, 1001, |x, y| {
            // u8 pixel coordinates: x wraps by design across the 2002-px width,
            // y (0..1001) is truncated mod 256 — the pattern is cosmetic.
            #[expect(clippy::cast_possible_truncation)]
            Rgb([x as u8, y as u8, 100])
        });
        let img = DynamicImage::ImageRgb8(buf);
        let mut bytes = Cursor::new(Vec::new());
        img.write_to(&mut bytes, ImageFormat::Bmp).unwrap();
        let out = normalize_bytes(&bytes.into_inner(), None).unwrap();
        assert!(out.width <= MAX_IMAGE_DIMENSION);
        assert!(out.height <= MAX_IMAGE_DIMENSION);
        // Aspect ratio preserved (2000×1000), opaque input → JPEG output.
        assert_eq!(out.width, 2000);
        assert_eq!(out.height, 1000);
        assert_eq!(out.mime_type, "image/jpeg");
    }

    #[cfg(not(feature = "avif"))]
    #[test]
    fn gated_avif_is_rejected_when_feature_disabled() {
        // An AVIF `ftyp` header is recognized by magic regardless, but is only
        // accepted when the gated `avif` feature is enabled.
        let err = normalize_bytes(b"\0\0\0\x18ftypavif", None).unwrap_err();
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
        let err = normalize_bytes(b"BM\0\0\0\0\0\0\0\0", None).unwrap_err();
        assert!(
            !err.to_string().contains("unsupported image format"),
            "{err}"
        );
    }

    #[test]
    fn empty_bytes_are_rejected() {
        assert!(normalize_bytes(&[], None).is_err());
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
