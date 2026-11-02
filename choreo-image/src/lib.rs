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
//!
//! The raster path also performs **colour management**: an embedded ICC profile
//! is converted to sRGB with the pure-Rust `moxcms` CMM before the pixels leave
//! this crate, so a `Display-P3` or `AdobeRGB` source reaches the model and the
//! terminal as sRGB values instead of having its numbers reinterpreted as sRGB
//! (which reads visibly desaturated). The HEIC path is already sRGB from
//! `heif-oxide`'s `nclx` handling, so it needs no second conversion. A profile
//! that will not parse or convert is assumed sRGB and decoding carries on — a
//! metadata tag never fails an image.

// Part of the ARCHITECTURE.md → rustdoc migration (see AGENTS.md → Documentation):
// every public item carries docs, enforced as a hard error by clippy-strict's
// `-D warnings`.
#![warn(missing_docs)]

mod heif;

use image::metadata::Orientation;
use image::{DynamicImage, ImageDecoder, ImageReader, RgbImage, RgbaImage};
use moxcms::{ColorProfile, Layout, TransformOptions};
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
    // Read the EXIF orientation and the embedded ICC profile from the header
    // (JPEG/WebP/PNG-eXIf, JPEG/PNG/WebP/TIFF ICC) before decoding pixels, then
    // rotate/flip and colour-manage in place. One decode pass total.
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
    // Colour-manage to sRGB after orientation (the two are independent: one is a
    // pixel reordering, the other a per-pixel value remap). Only an image that
    // carries a profile pays for the transform; a profile-less source is left as
    // the decoder produced it, so the common sRGB-photo path is unchanged.
    if let Some(icc) = icc {
        img = to_srgb(img, &icc);
    }
    Ok(img)
}

/// Converts an ICC-tagged image to sRGB, preserving its dimensions and alpha.
///
/// The source's embedded profile is transformed into sRGB with the pure-Rust
/// `moxcms` colour-management engine (no `lcms2`/C dependency — see the crate
/// docs for why the release build stays C-free). Alpha is a separate channel a
/// colour transform never touches, so an `Rgba8` buffer is transformed in the
/// `Rgba8` layout and its alpha channel is carried through untouched.
///
/// This is deliberately **fail-soft**: a profile that will not parse, or a
/// transform that will not build or run, leaves the pixels as they are (assumed
/// sRGB) rather than failing the decode. A metadata tag is not worth refusing a
/// photograph over, and the source is still a perfectly decodable image. Only
/// RGB-family images are transformed — a luma/LA buffer has no RGB primaries to
/// reinterpret and is returned as the decoder produced it.
fn to_srgb(img: DynamicImage, icc: &[u8]) -> DynamicImage {
    if !img.color().has_color() {
        return img;
    }
    let source = match ColorProfile::new_from_slice(icc) {
        Ok(source) => source,
        Err(error) => {
            debug!(%error, "the embedded colour profile does not parse; assuming sRGB");
            return img;
        }
    };
    let target = ColorProfile::new_srgb();
    let has_alpha = img.color().has_alpha();
    let layout = if has_alpha { Layout::Rgba } else { Layout::Rgb };
    let transform =
        match source.create_transform_8bit(layout, &target, layout, TransformOptions::default()) {
            Ok(transform) => transform,
            Err(error) => {
                debug!(%error, "no transform to sRGB could be built; assuming sRGB");
                return img;
            }
        };

    if has_alpha {
        let src = img.to_rgba8();
        // The transform rewrites a same-length buffer, so the destination is the
        // source's byte length exactly.
        let mut dst = vec![0u8; src.as_raw().len()];
        if let Err(error) = transform.transform(src.as_raw(), &mut dst) {
            debug!(%error, "the colour conversion failed; assuming sRGB");
            return DynamicImage::ImageRgba8(src);
        }
        match RgbaImage::from_raw(src.width(), src.height(), dst) {
            Some(converted) => DynamicImage::ImageRgba8(converted),
            // The transform cannot change the buffer length, but keep the source
            // if it somehow does rather than hand back a mismatched image.
            None => DynamicImage::ImageRgba8(src),
        }
    } else {
        let src = img.to_rgb8();
        let mut dst = vec![0u8; src.as_raw().len()];
        if let Err(error) = transform.transform(src.as_raw(), &mut dst) {
            debug!(%error, "the colour conversion failed; assuming sRGB");
            return DynamicImage::ImageRgb8(src);
        }
        match RgbImage::from_raw(src.width(), src.height(), dst) {
            Some(converted) => DynamicImage::ImageRgb8(converted),
            None => DynamicImage::ImageRgb8(src),
        }
    }
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
    use image::ImageEncoder;

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

    /// Encode an RGB image as a PNG, embedding `icc` when it is non-empty.
    fn png_with_icc(img: &RgbImage, icc: &[u8]) -> Vec<u8> {
        let mut out = Cursor::new(Vec::new());
        let mut encoder = image::codecs::png::PngEncoder::new(&mut out);
        if !icc.is_empty() {
            encoder
                .set_icc_profile(icc.to_vec())
                .expect("the PNG encoder supports ICC profiles");
        }
        encoder
            .write_image(
                img.as_raw(),
                img.width(),
                img.height(),
                image::ExtendedColorType::Rgb8,
            )
            .expect("encode png");
        out.into_inner()
    }

    #[test]
    fn to_srgb_leaves_the_image_unchanged_on_an_unparsable_profile() {
        // A broken ICC tag must never fail the decode: the pixels are assumed
        // sRGB and handed back exactly as they were.
        let img = DynamicImage::ImageRgb8(RgbImage::from_pixel(2, 2, image::Rgb([10, 20, 30])));
        let out = to_srgb(img.clone(), b"not an ICC profile");
        assert_eq!(out.to_rgb8(), img.to_rgb8());
    }

    #[test]
    fn to_srgb_converts_display_p3_and_keeps_neutral_in_the_steps() {
        let icc = ColorProfile::new_display_p3()
            .encode()
            .expect("encode a Display P3 profile");

        // A saturated, non-neutral colour is remapped when its P3 numbers are
        // reinterpreted in sRGB (the whole point of the conversion).
        let colour = image::Rgb([0u8, 128, 255]);
        let iimg = DynamicImage::ImageRgb8(RgbImage::from_pixel(2, 2, colour));
        let got = to_srgb(iimg, &icc).to_rgb8().get_pixel(0, 0).0;
        assert!(
            got.iter()
                .zip(colour.0.iter())
                .any(|(a, b)| a.abs_diff(*b) > 0),
            "a saturated P3 colour should change under sRGB conversion, got {got:?}"
        );

        // A neutral grey is D65 in both spaces, so it stays put (sanity that the
        // transform is not mangling the neutral axis).
        let grey = image::Rgb([128u8, 128, 128]);
        let gimg = DynamicImage::ImageRgb8(RgbImage::from_pixel(1, 1, grey));
        let gout = to_srgb(gimg, &icc).to_rgb8().get_pixel(0, 0).0;
        assert!(
            gout.iter()
                .zip(grey.0.iter())
                .all(|(a, b)| a.abs_diff(*b) <= 2),
            "neutral grey should stay neutral, got {gout:?}"
        );
    }

    #[test]
    fn decode_raster_oriented_applies_an_embedded_profile() {
        let icc = ColorProfile::new_display_p3()
            .encode()
            .expect("encode a Display P3 profile");
        let colour = image::Rgb([0u8, 128, 255]);
        let img = RgbImage::from_pixel(3, 2, colour);

        // With a P3 profile embedded, the decode colour-manages to sRGB, so the
        // pixel comes back changed.
        let with =
            decode_raster_oriented(&png_with_icc(&img, &icc)).expect("decode a P3-tagged png");
        assert_eq!(with.dimensions(), (3, 2));
        let got = with.to_rgb8().get_pixel(0, 0).0;
        assert!(
            got.iter()
                .zip(colour.0.iter())
                .any(|(a, b)| a.abs_diff(*b) > 0),
            "the embedded profile should be applied, got {got:?}"
        );

        // The same pixels with no profile are returned byte-for-byte unchanged,
        // so the common sRGB path pays nothing and is not perturbed.
        let plain =
            decode_raster_oriented(&png_with_icc(&img, &[])).expect("decode an untagged png");
        assert_eq!(plain.to_rgb8(), img);
    }
}
