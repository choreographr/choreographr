//! Shared image decode helpers, used by both the daemon (vision normalization
//! for `read_image`, and `display_image`) and the TUI (client display decode).
//!
//! Centralizes the two decode paths that were previously duplicated between
//! `choreo-daemon::image_prep` and `choreo-tui::image_worker`: the raster
//! decode with EXIF orientation baked in (`decode_raster_oriented`, the
//! `image`-crate path under a decompression-bomb `image::Limits` guard in a
//! single pass), and the pure-Rust `heif-oxide` HEIC/HEIF decode
//! (`decode_heic`, which applies the container's orientation and runs a
//! pre-decode allocation guard so a hostile container cannot drive a huge
//! allocation before we reject it).
//!
//! Keeping these in one leaf crate means an orientation bug fix or a security
//! guard change lands in the model path and the UI path together, so they can
//! never drift apart.

mod heif;

use image::metadata::Orientation;
use image::{DynamicImage, ImageDecoder, ImageReader, RgbaImage};
use std::io::Cursor;
use tracing::warn;

/// Cap on source image dimensions for the decompression-bomb guard (px per
/// side). Matched by the raster decode's `image::Limits` and the HEIC
/// pre-decode geometry check so an in-limits source is never rejected but a
/// hostile one cannot allocate gigabytes.
pub const MAX_SOURCE_DIMENSION: u32 = 8192;
/// Cap on total decoder allocation (bytes) — the decompression-bomb guard we
/// pass to the `image` crate. Derived from [`MAX_SOURCE_DIMENSION`] so the two
/// cannot drift: the worst case for an in-limits source is an RGBA8 image
/// (four bytes per pixel) at the source dimension — the square of
/// [`MAX_SOURCE_DIMENSION`] multiplied by four. Because the guard is derived
/// from (rather than independent of) the dimension cap, a source that passes
/// the dimension limit always fits the allocation guard.
pub const MAX_DECODE_ALLOC: u64 = (MAX_SOURCE_DIMENSION as u64).pow(2) * 4;

/// Cap on total decoded pixels — the pixel budget passed to the HEIC pre-decode
/// geometry guard. Derived from [`MAX_SOURCE_DIMENSION`] the same way
/// [`MAX_DECODE_ALLOC`] is derived (an in-limits source is at most
/// [`MAX_SOURCE_DIMENSION`]² pixels), so the raster `image::Limits` guard and
/// the HEIC geometry guard enforce the *same* allocation ceiling: a square
/// image at the source dimension is exactly at the cap, and pixel = 4 bytes
/// for the RGBA output `heif-oxide` produces.
pub const MAX_DECODE_PIXELS: u64 = (MAX_SOURCE_DIMENSION as u64).pow(2);

/// Decode a raster image via the `image` crate, baking EXIF orientation.
///
/// JPEG/WebP/PNG-`eXIf` orientation is applied in place after a single decode
/// pass, so phone/camera photos come out upright. The decode runs under a
/// decompression-bomb `image::Limits` guard derived from [`MAX_SOURCE_DIMENSION`].
pub fn decode_raster_oriented(data: &[u8]) -> Result<DynamicImage, String> {
    let mut reader = ImageReader::new(Cursor::new(data))
        .with_guessed_format()
        .map_err(|e| format!("failed to guess raster format: {e}"))?;
    // Decompression-bomb guard: bound the decode before any large allocation.
    // `Limits` is `#[non_exhaustive]`, so start from the default and set the
    // public fields via mutation (construction is forbidden).
    let mut limits = image::Limits::default();
    limits.max_image_width = Some(MAX_SOURCE_DIMENSION);
    limits.max_image_height = Some(MAX_SOURCE_DIMENSION);
    limits.max_alloc = Some(MAX_DECODE_ALLOC);
    reader.limits(limits);

    let mut decoder = reader
        .into_decoder()
        .map_err(|e| format!("failed to open raster decoder: {e}"))?;
    // Read the EXIF orientation from the header (JPEG/WebP/PNG-eXIf) before
    // decoding pixels, then rotate/flip in place. One decode pass total.
    let orientation = decoder.orientation().unwrap_or(Orientation::NoTransforms);
    let mut img = DynamicImage::from_decoder(decoder)
        .map_err(|e| format!("failed to decode raster image: {e}"))?;
    if orientation != Orientation::NoTransforms {
        img.apply_orientation(orientation);
    }
    Ok(img)
}

/// Decode a HEIC/HEIF image to an RGBA [`DynamicImage`].
///
/// `heif-oxide` applies the container's orientation transforms and delivers
/// display-ready sRGB, so no further rotation is needed. A *pre-decode*
/// allocation guard rejects hostile declared geometry before the decoder runs
/// (see [`heic_geometry_within_limits`]).
pub fn decode_heic(data: &[u8]) -> Result<DynamicImage, String> {
    if !heic_geometry_within_limits(data) {
        warn!(
            size = data.len(),
            "rejecting heic: declared geometry exceeds the decompression-bomb guard"
        );
        return Err(
            "heic image declares dimensions beyond the decompression-bomb guard".to_string(),
        );
    }
    let decoded =
        heif_oxide::decode_bytes(data).map_err(|e| format!("failed to decode heic: {e}"))?;
    let rgba = RgbaImage::from_raw(decoded.width, decoded.height, decoded.to_rgba8())
        .ok_or_else(|| "heic decoded to a buffer that does not match its size".to_string())?;
    Ok(DynamicImage::ImageRgba8(rgba))
}

/// True when the declared image geometry in an ISOBMFF/HEIF container stays
/// within the decompression-bomb guard, so `heif-oxide` won't allocate from a
/// hostile size.
///
/// `heif-oxide` exposes no decoder limit, so we pre-parse the container for
/// the geometry it allocates: every `ispe` (ImageSpatialExtentsProperty)
/// extent (the per-item frame size a single coded image or grid tile is
/// decoded from) and every `grid` derived item's canvas (tile extent ×
/// rows/cols, read from the grid item payload located via `iinf`/`iloc`).
/// See [`heif`] for the details; parsing is bounds-checked and a container
/// whose size we cannot prove is rejected rather than decoded (the safe
/// default — a valid HEIF still image always carries `ispe` geometry).
fn heic_geometry_within_limits(data: &[u8]) -> bool {
    heif::geometry_within_limits(data, MAX_SOURCE_DIMENSION, MAX_DECODE_PIXELS)
}

#[cfg(test)]
mod tests {
    use super::*;
    use image::GenericImageView;

    /// Build a box header + the given content. `size` is written as the box
    /// size (content + header).
    fn box_(box_type: &[u8; 4], content: &[u8]) -> Vec<u8> {
        let size = (8 + content.len()) as u32;
        let mut b = Vec::with_capacity(size as usize);
        b.extend_from_slice(&size.to_be_bytes());
        b.extend_from_slice(box_type);
        b.extend_from_slice(content);
        b
    }

    /// A minimal `meta` (full box, version/flags prefix) > `iprp` > `ipco` >
    /// `ispe` container wrapping one image extent of `w`×`h`.
    fn heic_container(w: u32, h: u32) -> Vec<u8> {
        let mut ispe = vec![0u8; 4]; // version/flags
        ispe.extend_from_slice(&w.to_be_bytes());
        ispe.extend_from_slice(&h.to_be_bytes());
        let ispe = box_(b"ispe", &ispe);
        let ipco = box_(b"ipco", &ispe);
        let iprp = box_(b"iprp", &ipco);
        let mut meta = vec![0u8; 4]; // meta full-box version/flags
        meta.extend_from_slice(&iprp);
        box_(b"meta", &meta)
    }

    #[test]
    fn rejects_oversized_declared_heic_extent() {
        // A single coded item declaring an 8193×100 frame is rejected. Even
        // one byte over the limit must not reach the decoder.
        assert!(!heic_geometry_within_limits(&heic_container(8193, 100)));
        assert!(!heic_geometry_within_limits(&heic_container(100, 8193)));
        assert!(!heic_geometry_within_limits(&heic_container(16000, 16000)));
    }

    #[test]
    fn accepts_in_limits_declared_heic_extent() {
        assert!(heic_geometry_within_limits(&heic_container(4000, 3000)));
        assert!(heic_geometry_within_limits(&heic_container(8192, 8192)));
    }

    #[test]
    fn rejects_when_no_ispe_geometry_is_found() {
        // No image extent declared → the safe default is to reject rather than
        // decode a container whose size we cannot prove.
        assert!(!heic_geometry_within_limits(
            b"\0\0\0\0ftypheic\0\0\0\0heic"
        ));
        assert!(!heic_geometry_within_limits(b""));
    }

    #[test]
    fn ignores_geometry_inside_mdat() {
        // Bytes in `mdat` (raw media data) must NOT be parsed as an `ispe` —
        // otherwise arbitrary payload bytes could cause a false rejection. Build
        // a valid in-limits container plus an `mdat` whose payload is a
        // would-be oversized `ispe` lookalike; the real geometry (in `meta`)
        // wins and the mdat junk is ignored.
        let mut container = heic_container(4000, 3000); // valid, in-limits
        let mut lookalike = Vec::new();
        lookalike.extend_from_slice(&[0; 4]); // version/flags
        lookalike.extend_from_slice(&0xFFFF_FFFFu32.to_be_bytes()); // hostile width
        lookalike.extend_from_slice(&0xFFFF_FFFFu32.to_be_bytes()); // hostile height
        container.extend_from_slice(&box_(b"mdat", &lookalike));
        assert!(heic_geometry_within_limits(&container));
    }

    #[test]
    fn decode_raster_oriented_preserves_dimensions() {
        // A valid PNG with no EXIF orientation decodes to the same dimensions
        // (the orientation path must be a no-op for the default orientation).
        let img = RgbaImage::from_fn(4, 3, |x, y| {
            image::Rgba([x as u8 * 60, y as u8 * 80, 0, 255])
        });
        let mut png = Cursor::new(Vec::new());
        image::DynamicImage::ImageRgba8(img)
            .write_to(&mut png, image::ImageFormat::Png)
            .expect("encode png");
        let out = decode_raster_oriented(&png.into_inner()).expect("valid PNG should decode");
        assert_eq!(out.dimensions(), (4, 3));
    }

    #[test]
    fn decode_raster_oriented_rejects_bytes_with_no_format() {
        assert!(decode_raster_oriented(&[1, 2, 3]).is_err());
    }

    #[test]
    fn decode_heic_rejects_non_heif_bytes() {
        assert!(
            decode_heic(&[1, 2, 3]).unwrap_err().contains("heic"),
            "non-HEIF bytes should fail via the guarded heif path"
        );
    }
}
