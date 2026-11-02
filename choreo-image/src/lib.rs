//! Shared image decode helpers, used by both the daemon (vision normalization
//! for `read_image`, and `display_image`) and the TUI (client display decode).
//!
//! Centralizes the decode paths that would otherwise be duplicated between
//! `choreo-daemon::image_prep` and `choreo-tui::image_worker`: the raster decode
//! with EXIF orientation baked in ([`decode_raster_oriented`], the `image`-crate
//! path under a decompression-bomb guard — a total-pixel budget checked against
//! the declared size, plus `image::Limits` as defense-in-depth — in a single
//! pass), the header-only dimension probe ([`probe_dimensions`], which enforces
//! the same guard without decoding pixels), and the pure-Rust `heif-oxide`
//! HEIC/HEIF decode ([`decode_heic`], which applies the container's orientation
//! and runs a pre-decode allocation guard so a hostile container cannot drive a
//! huge allocation before we reject it).
//!
//! Keeping these in one leaf crate means an orientation bug fix or a security
//! guard change lands in the model path and the UI path together, so they can
//! never drift apart.
//!
//! The raster path also performs **colour management**: an embedded ICC profile
//! is converted to sRGB with the pure-Rust `moxcms` CMM before the pixels leave
//! this crate, so a `Display-P3` or `AdobeRGB` source reaches the model and the
//! terminal as sRGB values instead of having its numbers reinterpreted as sRGB
//! (which reads visibly desaturated). See `colour` for the scope (RGB profiles
//! on RGB buffers only) and the fail-soft contract — a metadata tag never fails
//! an image.
//!
//! The HEIC path is already sRGB when the container carries an `nclx` colour
//! description, which `heif-oxide` converts (Display-P3/BT.2020 → sRGB) as part
//! of the decode. A HEIC whose only colour tag is an embedded ICC profile
//! (`rICC`/`prof`) is *not* colour-managed: `heif-oxide` reports the profile's
//! presence but does not hand back its bytes, so there is nothing to apply
//! through the crate's colour path. That gap is upstream and covers only the
//! ICC-tagged HEIC case; the same applies to PNG `cICP`/`sRGB`-chunk colour,
//! which is not an ICC profile at all.

// Part of the ARCHITECTURE.md → rustdoc migration (see AGENTS.md → Documentation):
// every public item carries docs, enforced as a hard error by clippy-strict's
// `-D warnings`.
#![warn(missing_docs)]

mod colour;
mod heif;

use image::metadata::Orientation;
use image::{DynamicImage, ImageDecoder, ImageReader, RgbaImage};
use std::io::Cursor;
use tracing::{debug, warn};

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
///
/// Colour management then needs a second buffer of the decoded sample size, so
/// the peak for an image at this ceiling is about twice the budget, not the
/// budget itself. That is still bounded by the pixel guard, and the transform is
/// skipped entirely for a profile-less or sRGB-tagged source.
pub const MAX_DECODE_ALLOC: u64 = MAX_DECODE_PIXELS * 4;

/// Generous sanity cap on a single declared side (px).
///
/// The pixel budget ([`MAX_DECODE_PIXELS`]) is the memory guard; this only
/// rejects a nonsensical single dimension (e.g. a corrupt header declaring a
/// side near `u32::MAX`) before a decoder sees it. It is set far above any real
/// image side so it never rejects a legitimate tall screenshot — aspect ratio
/// must not be what a bounded-area guard keys on.
pub const MAX_SOURCE_DIMENSION: u32 = 1 << 16;

/// Open a raster decoder under the decompression-bomb guard.
///
/// Shared by [`decode_raster_oriented`] and [`probe_dimensions`] so the guard
/// can never be enforced on one path and bypassed on the other. The returned
/// decoder has consumed the header only; no pixel buffer has been allocated.
fn open_guarded(data: &[u8]) -> Result<impl ImageDecoder, String> {
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

    let decoder = reader
        .into_decoder()
        .map_err(|e| format!("failed to open raster decoder: {e}"))?;
    // The real guard: reject an over-budget image from the size the header
    // declares, *before* any pixel allocation. Keying on total pixels (not a
    // per-side dimension) admits a tall screenshot — a large height with few
    // enough pixels — while a decompression bomb of any shape stays bounded, and
    // it holds even for a decoder that enforces no `max_alloc` (the PNM decoder
    // is one). `u64` arithmetic cannot overflow a pair of `u32` sides.
    let (width, height) = decoder.dimensions();
    let pixels = u64::from(width) * u64::from(height);
    if pixels > MAX_DECODE_PIXELS {
        return Err(format!(
            "image is {width}x{height} ({pixels} pixels), over the {MAX_DECODE_PIXELS} pixel decode budget"
        ));
    }
    Ok(decoder)
}

/// Report a raster image's declared dimensions without decoding its pixels.
///
/// For a caller that needs only the geometry — a size probe that shows the image
/// elsewhere, a layout calculation — this is the cheap path: it sniffs the
/// format, enforces the same decompression-bomb guard as
/// [`decode_raster_oriented`], and returns the header's dimensions. Nothing is
/// colour-managed, because no pixels are read.
///
/// # Errors
///
/// Returns a human-readable error string when the data is not a supported raster
/// format, is corrupt enough that its header will not parse, or declares more
/// pixels than the [`MAX_DECODE_PIXELS`] budget. Note that a *header* that parses
/// does not guarantee the pixel data decodes; a caller that needs that guarantee
/// should call [`decode_raster_oriented`] instead (or leave it to the consumer
/// that actually displays the pixels).
pub fn probe_dimensions(data: &[u8]) -> Result<(u32, u32), String> {
    let decoder = open_guarded(data)?;
    Ok(decoder.dimensions())
}

/// Decode a raster image via the `image` crate, baking EXIF orientation.
///
/// JPEG/WebP/PNG-`eXIf` orientation is applied in place after a single decode
/// pass, so phone/camera photos come out upright, and any embedded ICC profile
/// is then converted to sRGB (see `colour`) — orientation is a pixel
/// reordering and colour a per-pixel value remap, so the two are independent.
/// The decode runs under a decompression-bomb guard whose authoritative bound is
/// the total-pixel budget [`MAX_DECODE_PIXELS`], checked against the image's
/// declared size before any pixel allocation — so aspect ratio never decides the
/// limit.
///
/// # Errors
///
/// Returns a human-readable error string when the data is not a supported
/// raster format, is corrupt, or declares more pixels than the
/// decompression-bomb budget.
pub fn decode_raster_oriented(data: &[u8]) -> Result<DynamicImage, String> {
    let mut decoder = open_guarded(data)?;
    // Read the EXIF orientation and the embedded ICC profile from the header
    // (JPEG/WebP/PNG-eXIf, JPEG/PNG/WebP/TIFF/GIF ICC) before decoding pixels,
    // then rotate/flip and colour-manage in place. One decode pass total.
    let orientation = decoder.orientation().unwrap_or(Orientation::NoTransforms);
    let icc = decoder.icc_profile().unwrap_or_else(|error| {
        debug!(%error, "the embedded colour profile could not be read");
        None
    });
    let mut img = DynamicImage::from_decoder(decoder)
        .map_err(|e| format!("failed to decode raster image: {e}"))?;
    if orientation != Orientation::NoTransforms {
        img.apply_orientation(orientation);
    }
    // Colour-manage to sRGB after orientation. Only an image that carries a
    // profile *and* whose profile is not already sRGB pays for the transform; a
    // profile-less source is left as the decoder produced it, so the common
    // sRGB-photo path is unchanged.
    if let Some(icc) = icc {
        img = colour::to_srgb(img, &icc);
    }
    Ok(img)
}

/// Decode a HEIC/HEIF image to an RGBA [`DynamicImage`].
///
/// `heif-oxide` applies the container's orientation transforms and converts an
/// `nclx` colour description to sRGB, so no further rotation or colour pass is
/// needed here (an ICC-tagged HEIC is the one gap — see the crate docs). A
/// *pre-decode* allocation guard rejects hostile declared geometry before the
/// decoder runs (see `heic_geometry_within_limits`).
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

    // The HEIC container fixtures and the pre-decode geometry guard's own tests
    // live with the parser (`heif::tests`); what belongs here is the public
    // decode contract, so the two cannot drift into testing the same parser
    // twice.

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
    fn probe_dimensions_reports_the_header_size() {
        let img = RgbaImage::from_fn(7, 5, |_x, _y| image::Rgba([0, 0, 0, 255]));
        let mut png = Cursor::new(Vec::new());
        image::DynamicImage::ImageRgba8(img)
            .write_to(&mut png, image::ImageFormat::Png)
            .expect("encode png");
        assert_eq!(probe_dimensions(&png.into_inner()), Ok((7, 5)));
    }

    #[test]
    fn probe_dimensions_enforces_the_same_guard_as_the_decode() {
        // The probe must never become a way around the decompression-bomb bound
        // that the full decode enforces.
        assert!(probe_dimensions(b"P6\n40000 40000\n255\n").is_err());
        assert!(probe_dimensions(&[1, 2, 3]).is_err());
    }

    #[test]
    fn decode_heic_rejects_non_heif_bytes() {
        assert!(
            decode_heic(&[1, 2, 3]).unwrap_err().contains("heic"),
            "non-HEIF bytes should fail via the guarded heif path"
        );
    }
}
