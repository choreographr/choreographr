//! The `image`-crate raster decode path: EXIF-orientation baking, the
//! decompression-bomb guard, and the header-only size probe.
//!
//! One decode pass reads the header, the EXIF orientation and the embedded ICC
//! profile, then decodes the pixels, rotates/flips them in place and converts the
//! profile to sRGB — so phone/camera photos arrive upright and correctly
//! coloured. The probe shares the same guard but reads the header only, so a
//! caller that needs nothing but the geometry never pays for a pixel decode.

use crate::{DecodeError, MAX_DECODE_ALLOC, MAX_SOURCE_DIMENSION};
use image::metadata::Orientation;
use image::{DynamicImage, ImageDecoder, ImageReader};
use std::io::Cursor;
use tracing::debug;

/// Open a raster decoder under the decompression-bomb guard.
///
/// Shared by [`decode_raster_oriented`] and [`probe_dimensions`] so the guard
/// can never be enforced on one path and bypassed on the other. The returned
/// decoder has consumed the header only; no pixel buffer has been allocated.
fn open_guarded(data: &[u8]) -> Result<impl ImageDecoder, DecodeError> {
    let mut reader = ImageReader::new(Cursor::new(data))
        .with_guessed_format()
        // A failure here is an I/O error reading the bytes, not an unrecognized
        // format — surface it as a decode error rather than mislabelling it.
        .map_err(|error| DecodeError::Raster(image::ImageError::IoError(error)))?;
    // `with_guessed_format` leaves the format unset (rather than failing) when no
    // signature matched, and `into_decoder` would then reject it as an
    // `image`-crate "unsupported"; report it as what it is instead.
    if reader.format().is_none() {
        return Err(DecodeError::UnrecognizedFormat);
    }
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

    let decoder = reader.into_decoder().map_err(DecodeError::Raster)?;
    // The real guard: reject an over-budget image from the size the header
    // declares, *before* any pixel allocation. It keys on total pixels (not a
    // per-side dimension) so a tall screenshot is admitted while a bomb of any
    // shape stays bounded, and it holds even for a decoder that enforces no
    // `max_alloc` (the PNM decoder is one). The bound and its `OverBudget` error
    // are shared with the HEIC path (see `crate::check_pixel_budget`).
    let (width, height) = decoder.dimensions();
    crate::check_pixel_budget(width, height)?;
    Ok(decoder)
}

/// True for the orientations that turn the picture a quarter turn, swapping its
/// sides — Exif 5, 6, 7 and 8.
///
/// Baking the orientation in changes the decoded extent, so both paths that
/// report a size must apply the same swap or they will disagree about an image's
/// aspect ratio.
fn swaps_sides(orientation: Orientation) -> bool {
    matches!(orientation.to_exif(), 5..=8)
}

/// Report a raster image's dimensions as the decode will produce them, without
/// decoding its pixels.
///
/// This is the cheap path for a caller that needs only the geometry — a size
/// probe that shows the image elsewhere, a layout calculation: it sniffs the
/// format, enforces the same decompression-bomb guard as
/// [`decode_raster_oriented`], and returns the header's dimensions **with the
/// EXIF orientation applied**, because a quarter-turned photo decodes to swapped
/// sides and a probe that answered otherwise would misreport the aspect ratio.
/// Nothing is colour-managed, because no pixels are read.
///
/// # Errors
///
/// Returns [`DecodeError::UnrecognizedFormat`] when the data is not a raster
/// format at all, [`DecodeError::Raster`] when the header will not parse, and
/// [`DecodeError::OverBudget`] when it declares more pixels than the
/// [`crate::MAX_DECODE_PIXELS`] budget. A *header* that parses does not guarantee the
/// pixel data decodes; a caller that needs that guarantee should call
/// [`decode_raster_oriented`] instead (or leave it to the consumer that actually
/// displays the pixels).
pub fn probe_dimensions(data: &[u8]) -> Result<(u32, u32), DecodeError> {
    let mut decoder = open_guarded(data)?;
    let (width, height) = decoder.dimensions();
    let orientation = decoder.orientation().unwrap_or(Orientation::NoTransforms);
    Ok(if swaps_sides(orientation) {
        (height, width)
    } else {
        (width, height)
    })
}

/// Decode a raster image via the `image` crate, baking EXIF orientation.
///
/// JPEG/WebP/PNG-`eXIf` orientation is applied in place after a single decode
/// pass, so phone/camera photos come out upright, and any embedded ICC profile
/// is then converted to sRGB (see the crate-level colour notes) — orientation is
/// a pixel reordering and colour a per-pixel value remap, so the two are
/// independent. The decode runs under a decompression-bomb guard whose
/// authoritative bound is the total-pixel budget [`crate::MAX_DECODE_PIXELS`], checked
/// against the image's declared size before any pixel allocation — so aspect
/// ratio never decides the limit.
///
/// # Errors
///
/// Returns [`DecodeError::UnrecognizedFormat`] when the data is not a raster
/// format at all, [`DecodeError::Raster`] when it is corrupt or the decoder
/// refuses it, and [`DecodeError::OverBudget`] when it declares more pixels than
/// the decompression-bomb budget.
pub fn decode_raster_oriented(data: &[u8]) -> Result<DynamicImage, DecodeError> {
    let mut decoder = open_guarded(data)?;
    // Read the EXIF orientation and the embedded ICC profile from the header
    // (JPEG/WebP/PNG-eXIf, JPEG/PNG/WebP/TIFF/GIF ICC) before decoding pixels,
    // then rotate/flip and colour-manage in place. One decode pass total.
    let orientation = decoder.orientation().unwrap_or(Orientation::NoTransforms);
    let icc = decoder.icc_profile().unwrap_or_else(|error| {
        debug!(%error, "the embedded colour profile could not be read");
        None
    });
    let mut img = DynamicImage::from_decoder(decoder).map_err(DecodeError::Raster)?;
    if orientation != Orientation::NoTransforms {
        img.apply_orientation(orientation);
    }
    // Colour-manage to sRGB after orientation. Only an image that carries a
    // profile *and* whose profile is not already sRGB pays for the transform; a
    // profile-less source is left as the decoder produced it, so the common
    // sRGB-photo path is unchanged.
    if let Some(icc) = icc {
        img = crate::colour::to_srgb(img, &icc);
    }
    Ok(img)
}

#[cfg(test)]
mod tests;
