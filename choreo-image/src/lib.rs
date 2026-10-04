//! Shared image decode helpers, used by both the daemon (vision normalization
//! for `read_image`, and `display_image`) and the TUI (client display decode).
//!
//! Centralizes the two decode paths that were previously duplicated between
//! `choreo-daemon::image_prep` and `choreo-tui::image_worker`: the raster
//! decode with EXIF orientation baked in (`decode_raster_oriented`, the
//! `image`-crate path under a decompression-bomb guard — a total-pixel budget
//! checked against the declared size, plus `image::Limits` as defense-in-depth —
//! in a single pass), and the pure-Rust `heif-oxide` HEIC/HEIF decode
//! (`decode_heic`, which applies the container's orientation and runs a
//! pre-decode allocation guard so a hostile container cannot drive a huge
//! allocation before we reject it).
//!
//! Keeping these in one leaf crate means an orientation bug fix or a security
//! guard change lands in the model path and the UI path together, so they can
//! never drift apart.

// Part of the ARCHITECTURE.md → rustdoc migration (see AGENTS.md → Documentation):
// every public item carries docs, enforced as a hard error by clippy-strict's
// `-D warnings`.
#![warn(missing_docs)]

mod heif;

use image::metadata::Orientation;
use image::{DynamicImage, ImageDecoder, ImageReader, RgbaImage};
use std::io::Cursor;
use tracing::warn;

/// Primary decompression-bomb guard: the total decoded-pixel budget (px).
///
/// This is the bound that matters — the worst-case allocation is this many
/// pixels at four bytes each (RGBA8), i.e. [`MAX_DECODE_ALLOC`]. It is a
/// *total* budget, not a per-side one, so a tall screenshot or wide panorama
/// (a large single side but modest area) is admitted while a source of any
/// shape with more pixels is rejected before the decoder allocates. Both the
/// raster decode (checked against the declared size in
/// [`decode_raster_oriented`]) and the HEIC pre-decode geometry guard enforce
/// this same ceiling.
pub const MAX_DECODE_PIXELS: u64 = 8192 * 8192;

/// Cap on total decoder allocation (bytes) — derived from (and consistent with)
/// [`MAX_DECODE_PIXELS`]: an RGBA8 image is four bytes per pixel, so the byte
/// budget is the pixel budget times four. Passed to the `image` crate as
/// [`image::Limits::max_alloc`] as defense-in-depth for decoders that honor it;
/// the authoritative raster bound is the declared-pixel check in
/// [`decode_raster_oriented`], which holds even for a codec that ignores it.
pub const MAX_DECODE_ALLOC: u64 = MAX_DECODE_PIXELS * 4;

/// Generous sanity cap on a single declared side (px).
///
/// The pixel budget ([`MAX_DECODE_PIXELS`]) is the memory guard; this only
/// rejects a nonsensical single dimension (e.g. a corrupt header declaring a
/// side near `u32::MAX`) before a decoder sees it. It is set far above any real
/// image side so it never rejects a legitimate tall screenshot — aspect ratio
/// must not be what a bounded-area guard keys on.
pub const MAX_SOURCE_DIMENSION: u32 = 1 << 16;

/// Decode a raster image via the `image` crate, baking EXIF orientation.
///
/// JPEG/WebP/PNG-`eXIf` orientation is applied in place after a single decode
/// pass, so phone/camera photos come out upright. The decode runs under a
/// decompression-bomb guard whose authoritative bound is the total-pixel
/// budget [`MAX_DECODE_PIXELS`], checked against the image's declared size
/// before any pixel allocation — so aspect ratio never decides the limit.
///
/// # Errors
///
/// Returns a human-readable error string when the data is not a supported
/// raster format, is corrupt, or declares more pixels than the
/// decompression-bomb budget.
pub fn decode_raster_oriented(data: &[u8]) -> Result<DynamicImage, String> {
    let mut reader = ImageReader::new(Cursor::new(data))
        .with_guessed_format()
        .map_err(|e| format!("failed to guess raster format: {e}"))?;
    // Decompression-bomb guard: bound the decode before any large allocation.
    // `Limits` is `#[non_exhaustive]`, so start from the default and set the
    // public fields via mutation (construction is forbidden). The per-side
    // fields are a sanity cap only and `max_alloc` is defense-in-depth for
    // decoders that honor it; the authoritative bound is the total-pixel check
    // on the decoder's declared size just below.
    let mut limits = image::Limits::default();
    limits.max_image_width = Some(MAX_SOURCE_DIMENSION);
    limits.max_image_height = Some(MAX_SOURCE_DIMENSION);
    limits.max_alloc = Some(MAX_DECODE_ALLOC);
    reader.limits(limits);

    let mut decoder = reader
        .into_decoder()
        .map_err(|e| format!("failed to open raster decoder: {e}"))?;
    // The real guard: reject an over-budget image from the size the header
    // declares, *before* `from_decoder` allocates any pixels. Keying on total
    // pixels (not a per-side dimension) admits a tall screenshot — a large
    // height with few enough pixels — while a decompression bomb of any shape
    // stays bounded, and it holds even for a decoder that enforces no
    // `max_alloc` (the PNM decoder is one). `u64` arithmetic cannot overflow a
    // pair of `u32` sides.
    let (width, height) = decoder.dimensions();
    let pixels = u64::from(width) * u64::from(height);
    if pixels > MAX_DECODE_PIXELS {
        return Err(format!(
            "image is {width}x{height} ({pixels} pixels), over the {MAX_DECODE_PIXELS} pixel decode budget"
        ));
    }
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
/// (see `heic_geometry_within_limits`).
///
/// # Errors
///
/// Returns a human-readable error string when the declared HEIC geometry
/// exceeds the pre-decode guard or the decode itself fails.
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
/// the geometry it allocates: every `ispe` (`ImageSpatialExtentsProperty`)
/// extent (the per-item frame size a single coded image or grid tile is
/// decoded from) and every `grid` derived item's canvas (tile extent ×
/// rows/cols, read from the grid item payload located via `iinf`/`iloc`).
/// See [`heif`] for the details; parsing is bounds-checked and a container
/// whose size we cannot prove is rejected rather than decoded (the safe
/// default — a valid HEIF still image always carries `ispe` geometry).
fn heic_geometry_within_limits(data: &[u8]) -> bool {
    heif::geometry_within_limits(data, MAX_DECODE_PIXELS)
}

#[cfg(test)]
mod tests {
    use super::*;
    use image::GenericImageView;

    /// Build a box header + the given content. `size` is written as the box
    /// size (content + header).
    fn box_(box_type: [u8; 4], content: &[u8]) -> Vec<u8> {
        let size = u32::try_from(8 + content.len()).expect("test boxes fit u32");
        let mut b = Vec::with_capacity(size as usize);
        b.extend_from_slice(&size.to_be_bytes());
        b.extend_from_slice(&box_type);
        b.extend_from_slice(content);
        b
    }

    /// A minimal `meta` (full box, version/flags prefix) > `iprp` > `ipco` >
    /// `ispe` container wrapping one image extent of `w`×`h`.
    fn heic_container(w: u32, h: u32) -> Vec<u8> {
        let mut ispe = vec![0u8; 4]; // version/flags
        ispe.extend_from_slice(&w.to_be_bytes());
        ispe.extend_from_slice(&h.to_be_bytes());
        let ispe = box_(*b"ispe", &ispe);
        let ipco = box_(*b"ipco", &ispe);
        let iprp = box_(*b"iprp", &ipco);
        let mut meta = vec![0u8; 4]; // meta full-box version/flags
        meta.extend_from_slice(&iprp);
        box_(*b"meta", &meta)
    }

    #[test]
    fn rejects_declared_heic_extent_over_the_pixel_budget() {
        // The guard bounds total pixels, not a per-side dimension, so an
        // extent whose area exceeds the budget is rejected before the decoder
        // runs — whatever its shape.
        assert!(!heic_geometry_within_limits(&heic_container(16000, 16000))); // 256M px, square
        assert!(!heic_geometry_within_limits(&heic_container(2000, 40000))); // 80M px, tall
        assert!(!heic_geometry_within_limits(&heic_container(40000, 2000))); // 80M px, wide
    }

    #[test]
    fn accepts_in_budget_declared_heic_extent() {
        assert!(heic_geometry_within_limits(&heic_container(4000, 3000)));
        // Exactly at the budget (a square at the derived side) is accepted.
        assert!(heic_geometry_within_limits(&heic_container(8192, 8192)));
        // A tall extent over the old 8192 per-side cap but well under the area
        // budget is accepted — the aspect ratio does not reject it.
        assert!(heic_geometry_within_limits(&heic_container(1000, 12000)));
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
        container.extend_from_slice(&box_(*b"mdat", &lookalike));
        assert!(heic_geometry_within_limits(&container));
    }

    #[test]
    fn decode_raster_oriented_preserves_dimensions() {
        // A valid PNG with no EXIF orientation decodes to the same dimensions
        // (the orientation path must be a no-op for the default orientation).
        // Pixel values wrap modulo 256 by design — only dimensions matter.
        #[expect(clippy::cast_possible_truncation)]
        let img = RgbaImage::from_fn(4, 3, |x, y| {
            image::Rgba([(x * 60) as u8, (y * 80) as u8, 0, 255])
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
    fn decode_raster_oriented_accepts_tall_in_budget_image() {
        // The guard bounds total pixels, not a per-side dimension: a tall
        // strip whose height exceeds the old 8192 per-side cap still decodes,
        // because 64 × 9000 = 576_000 px is far under the pixel budget. This
        // is the tall-screenshot (full-page capture) case the per-side cap
        // wrongly rejected.
        let img = RgbaImage::from_fn(64, 9000, |_x, _y| image::Rgba([0, 0, 0, 255]));
        let mut png = Cursor::new(Vec::new());
        image::DynamicImage::ImageRgba8(img)
            .write_to(&mut png, image::ImageFormat::Png)
            .expect("encode png");
        let out =
            decode_raster_oriented(&png.into_inner()).expect("tall in-budget image should decode");
        assert_eq!(out.dimensions(), (64, 9000));
    }

    #[test]
    fn decode_raster_oriented_rejects_over_budget_pixels() {
        // A header declaring more than the pixel budget is rejected from its
        // *declared* size, before any allocation, even though each side is far
        // under the generous per-side sanity cap. A PNM (whose decoder honors
        // no `image::Limits` allocation limit) proves the rejection is the
        // area guard itself, not `max_alloc`.
        let pnm = b"P6\n40000 40000\n255\n";
        let err = decode_raster_oriented(pnm).unwrap_err();
        assert!(
            err.contains("pixel"),
            "expected the pixel-budget guard, got: {err}"
        );
    }

    #[test]
    fn decode_heic_rejects_non_heif_bytes() {
        assert!(
            decode_heic(&[1, 2, 3]).unwrap_err().contains("heic"),
            "non-HEIF bytes should fail via the guarded heif path"
        );
    }
}
