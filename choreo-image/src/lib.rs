//! Shared image decode helpers, used by both the daemon (vision normalization
//! for `read_image`, and `display_image`) and the TUI (client display decode).
//!
//! Centralizes the decode paths that would otherwise be duplicated between
//! `choreo-daemon::image_prep` and `choreo-tui::image_worker`: the raster decode
//! with EXIF orientation baked in ([`decode_raster_oriented`], the `image`-crate
//! path under a decompression-bomb guard — a total-pixel budget checked against
//! the declared size, plus `image::Limits` as defense-in-depth — in a single
//! pass), the header-only dimension probes ([`probe_dimensions`] for rasters and
//! [`probe_heic_dimensions`] for HEIC, both enforcing the same guard without
//! decoding pixels), and the pure-Rust `heifer` HEIC/HEIF decode
//! ([`decode_heic`]).
//!
//! Keeping these in one leaf crate means an orientation bug fix or a security
//! guard change lands in the model path and the UI path together, so they can
//! never drift apart.
//!
//! # Colour management
//!
//! Both decode paths convert an embedded ICC profile to sRGB with the pure-Rust
//! `moxcms` CMM before the pixels leave this crate, so a `Display-P3` or
//! `AdobeRGB` source reaches the model and the terminal as sRGB values instead
//! of having its numbers reinterpreted as sRGB (which reads visibly desaturated).
//! See `colour` for the scope (RGB profiles on RGB buffers only) and the
//! fail-soft contract — a metadata tag never fails an image.
//!
//! # HEIC
//!
//! `heifer` is the pure-Rust HEIC decoder. It applies the container's own
//! orientation transforms (rotate/mirror/crop), so nothing here re-applies an
//! `Orientation` tag from the retained Exif box — doing so would rotate the
//! picture a second time. The decompression-bomb bound is enforced in two
//! places: `heifer` itself rejects a picture larger than its `max_pixels` option
//! at the allocation site, and [`decode_heic`] additionally pre-checks the
//! container's *declared* geometry (see [`probe_heic_dimensions`]) so a hostile
//! file is rejected before any decoding work starts.
//!
//! An embedded ICC profile (`rICC`/`prof`) read from the container is applied
//! through `colour::to_srgb`, so an ICC-tagged HEIC is colour-managed like any
//! raster source. When the container instead (or additionally) signals colour via
//! its `nclx` description, that conversion is part of the decode.

// Part of the ARCHITECTURE.md → rustdoc migration (see AGENTS.md → Documentation):
// every public item carries docs, enforced as a hard error by clippy-strict's
// `-D warnings`.
#![warn(missing_docs)]

mod colour;

use heifer_isobmff::boxes::Property;
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

/// Decode a HEIC/HEIF image to an RGBA [`DynamicImage`], colour-managed to sRGB.
///
/// The decode runs under the decompression-bomb guard: the container's
/// *declared* geometry is checked first (see [`probe_heic_dimensions`]), and the
/// decoder enforces [`MAX_DECODE_PIXELS`] itself against the picture it is about
/// to allocate, so a hostile file is rejected from its header and a
/// maliciously-amplified grid cannot allocate past the budget either.
///
/// `heifer` applies the container's own orientation transforms, so nothing here
/// re-applies an Exif `Orientation` tag — that would rotate the picture twice.
/// An embedded ICC profile is then converted to sRGB through `colour`; the
/// profile is a metadata tag, so a failure to read it is fail-soft and leaves
/// the pixels as decoded.
///
/// # Errors
///
/// Returns a human-readable error string when the declared HEIC geometry
/// exceeds the decompression-bomb guard or the decode itself fails.
pub fn decode_heic(data: &[u8]) -> Result<DynamicImage, String> {
    // Defence in depth: reject a hostile declared geometry from the header,
    // before the decoder does any work. `heifer` enforces the same budget at the
    // allocation site; this pre-check just fails faster and with a clearer
    // message.
    probe_heic_dimensions(data)?;
    decode_heic_with_budget(data, MAX_DECODE_PIXELS)
}

/// [`decode_heic`]'s pixel decode, with the budget supplied by the caller.
///
/// The budget is a parameter rather than the constant so the in-decoder limit
/// can be exercised by a test with a small value; `decode_heic` passes
/// [`MAX_DECODE_PIXELS`]. The declared-geometry pre-check lives in the caller:
/// this function's bound is `heifer`'s own `max_pixels`, enforced against the
/// picture it is about to allocate.
fn decode_heic_with_budget(data: &[u8], max_pixels: u64) -> Result<DynamicImage, String> {
    let decoded = heifer::decode_with_options(
        data,
        &heifer::Options {
            max_pixels,
            max_threads: 0,
        },
    )
    .map_err(|e| format!("failed to decode heic: {e}"))?;
    let rgba = RgbaImage::from_raw(decoded.width, decoded.height, decoded.to_rgba8())
        .ok_or_else(|| "heic decoded to a buffer that does not match its size".to_string())?;
    let img = DynamicImage::ImageRgba8(rgba);

    // Colour-manage to sRGB when the container carries an ICC profile. This is
    // fail-soft by design: a metadata tag must never fail an otherwise good
    // photograph, so an unreadable profile leaves the decoded pixels alone.
    match heifer::read_metadata(data) {
        Ok(metadata) => match metadata.icc_profile {
            Some(icc) => Ok(colour::to_srgb(img, &icc)),
            None => Ok(img),
        },
        Err(error) => {
            debug!(%error, "the heic colour profile could not be read; assuming sRGB");
            Ok(img)
        }
    }
}

/// The container-declared geometry of a HEIC's primary item.
struct DeclaredGeometry {
    /// The extent the decoder works at — the picture extent of a single coded
    /// image, or a `grid`'s canvas. This is the size the pixel budget is checked
    /// against, because it is what the decoder allocates before the container's
    /// crop shrinks it.
    canvas: (u32, u32),
    /// The size of the picture the decode produces, after the container's crop
    /// (`clap`) and rotation/mirror (`irot`/`imir`) transforms.
    display: (u32, u32),
}

/// Report a HEIC/HEIF image's declared dimensions without decoding pixels.
///
/// This is **header-only** — it parses the container's box structure (no HEVC
/// decode, no pixel allocation), so it costs microseconds rather than the tens
/// to hundreds of milliseconds a full decode takes. A caller that needs only the
/// geometry (a size probe before displaying, a layout calculation) uses this
/// instead of paying for a decode.
///
/// The returned size is the one a decode would produce: the primary item's
/// `grid` canvas (the assembled tiles, already cropped) or its `ispe` extent,
/// with the container's crop and rotation/mirror transforms applied. Because it
/// trusts those declarations rather than inspecting the coded picture, the probe
/// is **conservative** — a file declaring an oversized extent is rejected even
/// if the coded picture would turn out to be small.
///
/// # Errors
///
/// Returns a human-readable error string when the container does not parse, has
/// no size for its primary item, or declares more than [`MAX_DECODE_PIXELS`] —
/// the probe never reports a size the decode would refuse.
pub fn probe_heic_dimensions(data: &[u8]) -> Result<(u32, u32), String> {
    let geometry = declared_heic_geometry(data)?;
    let (width, height) = geometry.canvas;
    check_heic_budget(data, width, height, MAX_DECODE_PIXELS)?;
    Ok(geometry.display)
}

/// Read the container-declared geometry of a HEIC's primary item.
///
/// A `grid` item's canvas wins: it is the assembled, cropped size the decoder
/// produces. A non-grid item falls back to its `ispe` extent, and a container
/// that declares neither size is an error rather than a guessed size.
fn declared_heic_geometry(data: &[u8]) -> Result<DeclaredGeometry, String> {
    let file = heifer_isobmff::HeifFile::parse(data)
        .map_err(|e| format!("failed to parse heic container: {e}"))?;
    let id = file.primary_id;
    // `grid` errors when the item is not a grid at all, which is the signal to
    // fall back to the plain image extent rather than a hard failure.
    let canvas = if let Ok(grid) = file.grid(id) {
        (grid.output_width, grid.output_height)
    } else {
        match file.image_size(id) {
            Ok(Some(size)) => size,
            Ok(None) => {
                return Err("heic container declares no size for its primary image".to_string());
            }
            Err(error) => return Err(format!("failed to read heic image size: {error}")),
        }
    };
    if canvas.0 == 0 || canvas.1 == 0 {
        return Err("heic container declares a zero-sized image".to_string());
    }
    let display = transformed_extent(&file, id, canvas)?;
    Ok(DeclaredGeometry { canvas, display })
}

/// Apply the container's transformative properties to a declared extent.
///
/// The decoder applies them in association order after decoding, so replaying
/// the same properties in the same order yields the size it will hand back: a
/// `clap` crop shrinks the extent, an odd `irot` quarter swaps the sides, and a
/// mirror changes neither.
fn transformed_extent(
    file: &heifer_isobmff::HeifFile<'_>,
    id: heifer_isobmff::ItemId,
    (width, height): (u32, u32),
) -> Result<(u32, u32), String> {
    let (mut width, mut height) = (width, height);
    let properties = file
        .item_properties(id)
        .map_err(|e| format!("failed to read heic image properties: {e}"))?;
    for property in properties {
        match *property {
            Property::CleanAperture {
                width: aperture_width,
                height: aperture_height,
                horiz_offset,
                vert_offset,
            } => {
                if aperture_width.1 == 0
                    || aperture_height.1 == 0
                    || horiz_offset.1 == 0
                    || vert_offset.1 == 0
                {
                    return Err("heic clean aperture has a zero denominator".to_string());
                }
                width = aperture_side(aperture_width, horiz_offset, width);
                height = aperture_side(aperture_height, vert_offset, height);
            }
            Property::Rotation(quarters) if quarters % 2 == 1 => {
                core::mem::swap(&mut width, &mut height);
            }
            _ => {}
        }
    }
    Ok((width, height))
}

/// One axis of a `clap` (clean aperture) crop: the aperture's declared size,
/// clamped to the remaining span the way the decoder's centred crop clamps it —
/// an aperture that runs past the edge yields what is left of the picture.
fn aperture_side(size: (u32, u32), offset: (i32, u32), extent: u32) -> u32 {
    let declared = f64::from(size.0) / f64::from(size.1);
    let centre = f64::from(offset.0) / f64::from(offset.1) + (f64::from(extent) - 1.0) / 2.0;
    let start = (centre - (declared - 1.0) / 2.0).round().max(0.0);
    let remaining = (f64::from(extent) - start).max(0.0);
    // Non-negative and bounded by `extent` (a `u32`), so the cast is exact for
    // every value this can produce.
    #[expect(clippy::cast_possible_truncation, clippy::cast_sign_loss)]
    {
        declared.round().min(remaining) as u32
    }
}

/// Reject a declared HEIC geometry that exceeds `max_pixels`.
///
/// The bound is on *area*, not a per-side dimension, matching the raster path's
/// guard: a tall or wide HEIC with few enough pixels is admitted while a bomb of
/// any shape is rejected. `u64` arithmetic cannot overflow a pair of `u32` sides.
fn check_heic_budget(data: &[u8], width: u32, height: u32, max_pixels: u64) -> Result<(), String> {
    let pixels = u64::from(width) * u64::from(height);
    if pixels > max_pixels {
        warn!(
            size = data.len(),
            width, height, "rejecting heic: declared geometry exceeds the decompression-bomb guard"
        );
        return Err(
            "heic image declares dimensions beyond the decompression-bomb guard".to_string(),
        );
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use image::{ColorType, GenericImageView};

    /// A 16×16 HEIC with no embedded colour profile.
    const FIXTURE: &[u8] = include_bytes!("../tests/fixtures/heic-16x16.heic");
    /// The same 16×16 picture tagged with an embedded `AdobeRGB` ICC profile.
    const FIXTURE_ICC: &[u8] = include_bytes!("../tests/fixtures/heic-16x16-icc.heic");

    /// Rewrite the first `ispe` extent's width and height in place, producing a
    /// container that *declares* a bomb-sized image.
    ///
    /// The box layout is: `ispe` type at `idx` (a 4-byte size precedes it), then
    /// the full-box `version/flags` at `idx + 4`, and the two big-endian `u32`
    /// extents at `idx + 8` and `idx + 12`.
    fn patch_ispe_extent(heic: &[u8], width: u32, height: u32) -> Vec<u8> {
        let mut bytes = heic.to_vec();
        let idx = bytes
            .windows(4)
            .position(|w| w == b"ispe")
            .expect("the fixture carries an ispe box");
        bytes[idx + 8..idx + 12].copy_from_slice(&width.to_be_bytes());
        bytes[idx + 12..idx + 16].copy_from_slice(&height.to_be_bytes());
        bytes
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
            "non-HEIF bytes should fail via the guarded heic path"
        );
    }

    #[test]
    fn decode_heic_decodes_the_plain_fixture() {
        let img = decode_heic(FIXTURE).expect("the 16x16 fixture should decode");
        assert_eq!(img.dimensions(), (16, 16));
        assert_eq!(img.color(), ColorType::Rgba8);
    }

    #[test]
    fn decode_heic_colour_manages_an_icc_tagged_fixture() {
        // An ICC-tagged HEIC decodes *and* carries a profile the colour path can
        // consume. The assertion stops at "the profile parses as a colour
        // profile": the converted pixel values depend on the CMM's numerics,
        // which is not something this crate should pin.
        let img = decode_heic(FIXTURE_ICC).expect("the icc fixture should decode");
        assert_eq!(img.dimensions(), (16, 16));

        let metadata = heifer::read_metadata(FIXTURE_ICC).expect("metadata should parse");
        let icc = metadata
            .icc_profile
            .expect("the fixture carries an embedded ICC profile");
        assert!(
            moxcms::ColorProfile::new_from_slice(&icc).is_ok(),
            "the embedded profile must be usable by the colour path"
        );
    }

    #[test]
    fn probe_heic_dimensions_reports_the_declared_size() {
        assert_eq!(probe_heic_dimensions(FIXTURE), Ok((16, 16)));
        assert_eq!(probe_heic_dimensions(FIXTURE_ICC), Ok((16, 16)));
        assert!(probe_heic_dimensions(b"not a heif file").is_err());
    }

    #[test]
    fn declared_geometry_bomb_is_rejected_by_both_probe_and_decode() {
        // Patching the declared `ispe` extent turns an in-budget picture into one
        // that *claims* 60_000 x 60_000 pixels (3.6e9, far over the budget) while
        // its coded picture stays tiny. Both entry points must reject it: the probe
        // (so a size caller never sees a bomb dimension) and the decode (before the
        // decoder does any work). The guard keys on the *declared allocation*
        // extent, which the container's crop does not shrink — this fixture's crop
        // stays 16x16, so a guard that looked at the cropped size would let the
        // over-budget extent through.
        let bomb = patch_ispe_extent(FIXTURE, 60_000, 60_000);
        for error in [
            probe_heic_dimensions(&bomb).unwrap_err(),
            decode_heic(&bomb).unwrap_err(),
        ] {
            assert!(
                error.contains("decompression-bomb guard"),
                "expected the over-budget error, got: {error}"
            );
        }
    }

    #[test]
    fn decode_heic_enforces_the_decoders_own_pixel_limit() {
        // The declared geometry (16x16) fits any sane budget, so this reaches
        // `heifer` and is rejected by *its* `max_pixels` bound against the
        // padded picture it would allocate — proving the in-decoder limit is
        // actually wired, not just our header pre-check.
        let error = decode_heic_with_budget(FIXTURE, 100).unwrap_err();
        assert!(
            error.contains("failed to decode heic"),
            "expected the decoder's size-limit rejection, got: {error}"
        );
    }
}
