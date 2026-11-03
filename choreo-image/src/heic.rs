//! The HEIC/HEIF path on the pure-Rust `heifer` codec: the decode and the
//! header-only geometry probe.
//!
//! `heifer` owns the container parsing, the HEVC decode and the container's own
//! orientation transforms (rotate/mirror/crop), so nothing here re-applies an Exif
//! `Orientation` tag from the retained Exif box — doing so would rotate the
//! picture a second time.
//!
//! # Colour
//!
//! [`decode_heic`] decodes through `heifer`'s sRGB entry point, which converts
//! from the container's embedded ICC profile when it has one and otherwise from
//! its `nclx` colour description. That is why the buffer this module returns is
//! already sRGB and is deliberately **not** passed through `crate::colour`: a
//! second conversion would double-apply the source space. A file whose colour
//! `heifer` cannot convert (a non-RGB input profile, or a PQ/HLG transfer) still
//! decodes — the coded samples are kept and logged, because a colour tag must
//! never fail an otherwise good photograph.
//!
//! # Budgets
//!
//! The pixel budget is enforced twice, on purpose:
//!
//! - A declared-geometry pre-check rejects a hostile *measurable* geometry from
//!   the container header, before any decoding work starts. [`probe_heic_dimensions`]
//!   applies it for a size-only caller (the daemon's `display_image` probe) and
//!   must therefore fail on any container it cannot measure; [`decode_heic`]'s own
//!   pre-check is narrower — it fires only on a measurable over-budget geometry
//!   and otherwise defers to the decoder, so a container the probe cannot size but
//!   `heifer` can assemble is still decoded (the limit below bounds it).
//! - `heifer` enforces the same ceiling itself, at the allocation site, against
//!   the picture it is about to produce — including a `grid` item's tile
//!   amplification. That is the bound that holds when a container's declaration
//!   and its coded picture disagree, which is exactly why the pre-check is
//!   defence in depth rather than the load-bearing guard.

use crate::{DecodeError, MAX_DECODE_PIXELS};
use heifer_isobmff::boxes::Property;
use image::{DynamicImage, RgbImage, RgbaImage};
use tracing::warn;

/// Decode a HEIC/HEIF image to sRGB [`DynamicImage`].
///
/// The decode runs under the decompression-bomb guard: the container's
/// *measurable* declared geometry is checked first (see `check_declared_budget`),
/// and the decoder enforces [`MAX_DECODE_PIXELS`] itself against the picture it is
/// about to allocate — so a hostile file is rejected from its header and a
/// maliciously-amplified grid cannot allocate past the budget either. A container
/// whose geometry cannot be measured is still decoded: `heifer`'s own limit holds
/// the allocation, so a file the size-only [`probe_heic_dimensions`] would refuse
/// as unmeasurable is not lost here.
///
/// The returned image is `Rgb8` when the container carries no alpha plane and
/// `Rgba8` when it does. Preserving that distinction matters downstream: the
/// vision path picks PNG over JPEG from the colour type, so an always-RGBA
/// buffer would re-encode every ordinary photo as PNG.
///
/// # Errors
///
/// Returns [`DecodeError::OverBudget`] when the declared geometry exceeds the
/// decompression-bomb guard, and [`DecodeError::Heic`] when the container or the
/// HEVC bitstream does not decode.
pub fn decode_heic(data: &[u8]) -> Result<DynamicImage, DecodeError> {
    // Defence in depth: reject a hostile *measurable* declared geometry from the
    // header, before the decoder does any work. `heifer` enforces the same budget
    // at the allocation site; this pre-check just fails faster, with a clearer
    // message. An unmeasurable geometry is not a rejection — see the helper.
    check_declared_budget(data)?;
    decode_heic_with_budget(data, MAX_DECODE_PIXELS)
}

/// Reject a *measurable* declared geometry that is over the decode budget,
/// without failing on a container the header cannot measure.
///
/// [`probe_heic_dimensions`] treats "cannot measure" as an error because a
/// size-only caller needs a real size. The decode does not: a container whose
/// primary item declares no extent of its own (an `iovl`/`iden` derived image
/// without an `ispe`) is one `heifer` can still assemble, and its own `max_pixels`
/// bound holds the allocation at the decoder's allocation site. So this pre-check
/// fires only on a measurable over-budget geometry and otherwise returns `Ok`,
/// letting the decoder (and its limit) take over.
fn check_declared_budget(data: &[u8]) -> Result<(), DecodeError> {
    match declared_heic_geometry(data) {
        Ok(geometry) => crate::check_pixel_budget(geometry.canvas.0, geometry.canvas.1),
        // Could not measure the declared geometry: not a rejection here — the
        // decoder's own pixel limit is the backstop.
        Err(_) => Ok(()),
    }
}

/// [`decode_heic`]'s pixel decode, with the budget supplied by the caller.
///
/// The budget is a parameter rather than the constant so the in-decoder limit
/// can be exercised by a test with a small value; `decode_heic` passes
/// [`MAX_DECODE_PIXELS`]. The declared-geometry pre-check lives in the caller:
/// this function's bound is `heifer`'s own `max_pixels`, enforced against the
/// picture it is about to allocate.
fn decode_heic_with_budget(data: &[u8], max_pixels: u64) -> Result<DynamicImage, DecodeError> {
    let options = heifer::Options {
        max_pixels,
        // `0` lets `heifer` use the machine's cores for a `grid` item's tiles,
        // which is what makes a phone photo's tiled decode bearable.
        max_threads: 0,
    };
    // The sRGB entry point: the embedded ICC profile when the container has one,
    // otherwise the `nclx` description (or the HEVC VUI's primaries/transfer).
    // Using the plain decode here would hand back samples still in the source
    // space — a Display-P3 or BT.2020 photo read as sRGB, visibly desaturated.
    let decoded = match heifer::decode_srgb_rgba8(data, &options) {
        Ok(decoded) => decoded,
        // `Unsupported` is broader than colour: `heifer` also reports an
        // unsupported item type, an uncompressed item, or an undersized alpha
        // plane this way. A colour space it cannot convert must not fail the
        // photo, so retry the plain decode (which skips the colour conversion)
        // and keep the coded samples; a genuinely unsupported file fails again
        // there with the same reason, so the retry is wasted only on that rare
        // path and never on the common one.
        Err(heifer::Error::Unsupported(reason)) => {
            warn!(
                %reason,
                "heifer declined the file (unconvertible colour or an unsupported feature); \
                 retrying without colour management"
            );
            heifer::decode_rgba8(data, &options).map_err(DecodeError::Heic)?
        }
        Err(error) => return Err(DecodeError::Heic(error)),
    };
    let (width, height, has_alpha) = (decoded.width, decoded.height, decoded.has_alpha);
    // `into_rgb8` drops the (fully opaque) alpha plane in place, so an alpha-less
    // photo costs no second buffer to narrow.
    let img = if has_alpha {
        RgbaImage::from_raw(width, height, decoded.data).map(DynamicImage::ImageRgba8)
    } else {
        RgbImage::from_raw(width, height, decoded.into_rgb8()).map(DynamicImage::ImageRgb8)
    };
    img.ok_or(DecodeError::BufferSize { width, height })
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

/// Report a HEIC/HEIF image's dimensions as the decode will produce them,
/// without decoding pixels.
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
/// Returns [`DecodeError::HeicGeometry`] when the container does not parse or has
/// no size for its primary item, and [`DecodeError::OverBudget`] when it declares
/// more than [`MAX_DECODE_PIXELS`] — the probe never reports a size over the
/// decode budget. (The decode itself tolerates an unmeasurable geometry; see
/// [`decode_heic`].)
pub fn probe_heic_dimensions(data: &[u8]) -> Result<(u32, u32), DecodeError> {
    let geometry = declared_heic_geometry(data)?;
    let (width, height) = geometry.canvas;
    crate::check_pixel_budget(width, height)?;
    Ok(geometry.display)
}

/// Read the container-declared geometry of a HEIC's primary item.
///
/// A `grid` item's canvas wins: it is the assembled, cropped size the decoder
/// produces. A non-grid item falls back to its `ispe` extent, and a container
/// that declares neither size is an error rather than a guessed size.
///
/// A derived item type `heifer` resolves by reference (`iovl`, `iden`) has no
/// extent of its own beyond its `ispe`, so a container that omits one is
/// rejected here even though the decoder could assemble it. That is deliberate:
/// a probe that cannot prove the size must not report one, while [`decode_heic`]'s
/// [`check_declared_budget`] ignores a geometry it cannot measure and lets
/// `heifer`'s own limit bound the allocation.
fn declared_heic_geometry(data: &[u8]) -> Result<DeclaredGeometry, DecodeError> {
    let file = heifer_isobmff::HeifFile::parse(data)
        .map_err(|e| DecodeError::HeicGeometry(format!("the container does not parse: {e}")))?;
    let id = file.primary_id;
    // `grid` errors when the item is not a grid at all, which is the signal to
    // fall back to the plain image extent rather than a hard failure.
    let canvas = if let Ok(grid) = file.grid(id) {
        (grid.output_width, grid.output_height)
    } else {
        match file.image_size(id) {
            Ok(Some(size)) => size,
            Ok(None) => {
                return Err(DecodeError::HeicGeometry(
                    "it declares no size for its primary image".to_string(),
                ));
            }
            Err(error) => {
                return Err(DecodeError::HeicGeometry(format!(
                    "its image size cannot be read: {error}"
                )));
            }
        }
    };
    if canvas.0 == 0 || canvas.1 == 0 {
        return Err(DecodeError::HeicGeometry(
            "it declares a zero-sized image".to_string(),
        ));
    }
    let display = transformed_extent(&file, id, canvas)?;
    Ok(DeclaredGeometry { canvas, display })
}

/// Apply the container's transformative properties to a declared extent.
///
/// This re-derives arithmetic that lives in `heifer`, and it has to stay in step
/// with it: `heifer`'s `decode_item_with` walks `item_properties` in association
/// order and applies each one, so replaying the same properties in the same
/// order yields the extent it will hand back. Keep this in step with:
///
/// - `heifer::clean_aperture` for `clap` — the same rational-to-`f64` declared
///   size, the same `(extent - 1) / 2` centre, and the same `.round()`, because
///   a half-pixel difference in that convention moves the result by one;
/// - `heifer::Image::crop` for the clamp — an aperture that runs past the edge
///   yields what is left of the picture (see [`aperture_side`]);
/// - `heifer::Image::rotate_ccw` for `irot` — only an odd quarter count swaps
///   the sides, and `imir` never changes either side.
///
/// `probe_dimensions_agree_with_the_decode` is what makes a divergence loud
/// rather than silent: it encodes containers through `heifer`'s own encoder and
/// asserts this prediction equals the decoded extent.
fn transformed_extent(
    file: &heifer_isobmff::HeifFile<'_>,
    id: heifer_isobmff::ItemId,
    (width, height): (u32, u32),
) -> Result<(u32, u32), DecodeError> {
    let (mut width, mut height) = (width, height);
    let properties = file.item_properties(id).map_err(|e| {
        DecodeError::HeicGeometry(format!("its image properties cannot be read: {e}"))
    })?;
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
                    return Err(DecodeError::HeicGeometry(
                        "it declares a clean aperture with a zero denominator".to_string(),
                    ));
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
/// clamped to the remaining span the way `heifer::Image::crop` clamps it — an
/// aperture that runs past the edge yields what is left of the picture.
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

#[cfg(test)]
mod tests;
