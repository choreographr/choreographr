//! Colour management: convert a decoded buffer from its embedded ICC profile to
//! sRGB.
//!
//! The `image` crate decodes a source's samples without interpreting the colour
//! space they are encoded in, so an `AdobeRGB`, `Display-P3` or `ProPhoto` photo
//! reaches the model and the terminal as if it were sRGB — visibly desaturated
//! or duller than the file intends. This module remaps those samples to sRGB
//! with the pure-Rust `moxcms` colour-management engine (C-free, so the
//! static-musl release build needs no C toolchain — the reason `lcms2` is not
//! used).
//!
//! # Scope
//!
//! Only an **RGB profile on an RGB buffer** is handled:
//!
//! - A luma/luma-alpha buffer has no RGB primaries to reinterpret and is left as
//!   the decoder produced it.
//! - Only a profile whose own data colour space is RGB is applied. A Gray, Lab
//!   or CMYK profile describes a buffer we do not have: `image`'s decoders hand
//!   back RGB (a CMYK JPEG is converted by the decoder itself), and `moxcms`
//!   would read an `Rgba` layout as `C/M/Y/K` for a CMYK source, which mangles
//!   the colours *and* forces the alpha channel opaque. Such a profile is
//!   ignored rather than misapplied.
//! - 8-bit and 16-bit integer buffers keep their depth: the transform is built
//!   at the buffer's own depth, so colour management never quantises a 16-bit
//!   source down to 8 bits. Float (HDR/EXR) buffers are returned untouched —
//!   their consumers reduce to 8-bit anyway, and no float transform is built.
//!
//! # Cost and failure
//!
//! The transform is a per-pixel pass bounded by the decode's own pixel budget,
//! and it is skipped whenever it would be a no-op: an image with no profile is
//! returned unchanged, and a profile that *is* sRGB (the most common tag in the
//! wild) is detected by its primaries and transfer curves so no buffer is
//! allocated or rewritten for an identity conversion.
//!
//! Everything is **fail-soft**: an unparsable profile, an unusable layout, or a
//! transform that will not build or run leaves the pixels exactly as decoded
//! (assumed sRGB). A metadata tag is never a reason to refuse a photograph.

use image::{DynamicImage, ImageBuffer, Pixel};
use moxcms::{
    CmsError, ColorProfile, DataColorSpace, Layout, Matrix3d, ToneReprCurve, TransformExecutor,
    TransformOptions,
};
use std::sync::Arc;
use tracing::debug;

/// How close two colourant matrices must be to count as the same primaries.
///
/// Real-world sRGB profiles round their colourants to four decimals, so the
/// derived matrix differs from the reference by ~1e-4. A genuine different
/// space is orders of magnitude further away (Display-P3 differs by ~0.19),
/// so this tolerance identifies sRGB without ever misclassifying P3/AdobeRGB.
const MATRIX_EPSILON: f64 = 1.0e-3;

/// How close two transfer curves must be at every sample point to count as the
/// same curve. One 8-bit code value is 1/255 ≈ 3.9e-3, so this is well inside a
/// single quantisation step across the whole domain.
const TRC_EPSILON: f32 = 1.0e-3;

/// Code values at which two transfer curves are compared. Spread across the
/// shadow/mid/highlight range so a curve that only differs in one of them is
/// still caught.
const TRC_SAMPLE_POINTS: [f32; 9] = [0.0, 0.01, 0.05, 0.1, 0.25, 0.5, 0.75, 0.9, 1.0];

/// Convert `img` from its embedded ICC profile to sRGB.
///
/// `icc` is the profile exactly as the container carried it. Returns `img`
/// untouched in every case where a conversion is not applied, so a caller never
/// has to distinguish "converted" from "left alone" — see the module docs for
/// the cases and the fail-soft contract.
pub(crate) fn to_srgb(img: DynamicImage, icc: &[u8]) -> DynamicImage {
    // A luma/luma-alpha buffer carries no colour primaries to reinterpret.
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
    // Only an RGB profile can describe this RGB buffer — see the module docs for
    // why applying a CMYK/Gray/Lab profile would be worse than ignoring it.
    if source.color_space != DataColorSpace::Rgb {
        debug!(
            space = ?source.color_space,
            "the embedded profile does not describe an RGB buffer; assuming sRGB"
        );
        return img;
    }
    let target = ColorProfile::new_srgb();
    // An sRGB-tagged source converts to itself. Detecting that up front skips a
    // full per-pixel pass and a second full-size buffer for no pixel change —
    // and an sRGB tag is by far the most common one on real photos.
    if is_srgb(&source, &target) {
        debug!("the embedded profile is sRGB; nothing to convert");
        return img;
    }

    // The daemon's vision path and the TUI both reduce to 8-bit before use, but
    // the crate's contract is to hand back what it was given at the depth it was
    // given, so the 16-bit arm transforms in `u16` rather than quantising.
    match img {
        DynamicImage::ImageRgb8(buf) => {
            DynamicImage::ImageRgb8(convert(buf, &source, &target, Layout::Rgb))
        }
        DynamicImage::ImageRgba8(buf) => {
            DynamicImage::ImageRgba8(convert(buf, &source, &target, Layout::Rgba))
        }
        DynamicImage::ImageRgb16(buf) => {
            DynamicImage::ImageRgb16(convert(buf, &source, &target, Layout::Rgb))
        }
        DynamicImage::ImageRgba16(buf) => {
            DynamicImage::ImageRgba16(convert(buf, &source, &target, Layout::Rgba))
        }
        // Float (HDR/EXR/Radiance) buffers: no integer transform applies, and the
        // pipeline downstream is 8-bit, so keep the decoded samples rather than
        // truncating them here.
        other => {
            debug!(
                color = ?other.color(),
                "no colour transform for this buffer depth; keeping the decoded samples"
            );
            other
        }
    }
}

/// Remap one buffer through the source→sRGB transform at the buffer's depth.
///
/// Generic over the sample depth (`u8`/`u16`) and the pixel layout, so the
/// four 8/16-bit × RGB/RGBA combinations share one implementation: the transform
/// is built at the buffer's own depth by [`Depth::to_srgb_transform`], and the
/// alpha channel is carried through untouched (the transform runs in the
/// alpha-bearing layout, where `moxcms` copies it rather than converting it).
///
/// Returns `buf` unchanged when the transform cannot be built or run, and when
/// rebuilding the image from the transformed bytes fails — the decode still
/// succeeds, the colour is just left as the decoder produced it.
fn convert<S, P>(
    buf: ImageBuffer<P, Vec<S>>,
    source: &ColorProfile,
    target: &ColorProfile,
    layout: Layout,
) -> ImageBuffer<P, Vec<S>>
where
    S: Depth,
    P: Pixel<Subpixel = S> + 'static,
{
    let transform = match S::to_srgb_transform(source, target, layout) {
        Ok(transform) => transform,
        Err(error) => {
            debug!(%error, "no colour transform to sRGB could be built; assuming sRGB");
            return buf;
        }
    };
    // The transform rewrites a same-length buffer, so the destination is the
    // source's sample count exactly.
    let mut dst = vec![S::default(); buf.as_raw().len()];
    if let Err(error) = transform.transform(buf.as_raw(), &mut dst) {
        debug!(%error, "the colour conversion failed; assuming sRGB");
        return buf;
    }
    match ImageBuffer::from_raw(buf.width(), buf.height(), dst) {
        Some(converted) => converted,
        // The transform cannot change the sample count, but keep the source if it
        // somehow does rather than hand back a mismatched image.
        None => buf,
    }
}

/// An integer sample depth `moxcms` can transform, paired with the transform
/// builder for that depth.
///
/// This is what lets [`convert`] stay depth-generic: the only depth-specific
/// step is which `create_transform_*` constructor to call.
trait Depth: Copy + Default {
    /// Build the source→`target` transform for this depth, in `layout` on both
    /// sides (a colour transform never changes the channel count).
    fn to_srgb_transform(
        source: &ColorProfile,
        target: &ColorProfile,
        layout: Layout,
    ) -> Result<Arc<dyn TransformExecutor<Self> + Send + Sync>, CmsError>;
}

impl Depth for u8 {
    fn to_srgb_transform(
        source: &ColorProfile,
        target: &ColorProfile,
        layout: Layout,
    ) -> Result<Arc<dyn TransformExecutor<Self> + Send + Sync>, CmsError> {
        source.create_transform_8bit(layout, target, layout, TransformOptions::default())
    }
}

impl Depth for u16 {
    fn to_srgb_transform(
        source: &ColorProfile,
        target: &ColorProfile,
        layout: Layout,
    ) -> Result<Arc<dyn TransformExecutor<Self> + Send + Sync>, CmsError> {
        source.create_transform_16bit(layout, target, layout, TransformOptions::default())
    }
}

/// True when `source` describes the same space as `reference` (callers pass
/// sRGB): matching primaries and matching transfer curves.
///
/// Compared numerically rather than by profile bytes, because the same space is
/// written many ways — a parametric curve, a sampled `curv` table, or the same
/// numbers in a v2 or v4 profile — and bytes would only recognise the exact
/// encoding this crate's own encoder emits.
fn is_srgb(source: &ColorProfile, reference: &ColorProfile) -> bool {
    if source.pcs != reference.pcs {
        return false;
    }
    if !matrix_close(source.rgb_to_xyz_matrix(), reference.rgb_to_xyz_matrix()) {
        return false;
    }
    trc_close(source.red_trc.as_ref(), reference.red_trc.as_ref())
        && trc_close(source.green_trc.as_ref(), reference.green_trc.as_ref())
        && trc_close(source.blue_trc.as_ref(), reference.blue_trc.as_ref())
}

/// True when the two colourant matrices agree entry-for-entry within
/// [`MATRIX_EPSILON`].
fn matrix_close(a: Matrix3d, b: Matrix3d) -> bool {
    a.v.iter()
        .flatten()
        .zip(b.v.iter().flatten())
        .all(|(x, y)| (x - y).abs() <= MATRIX_EPSILON)
}

/// True when the two transfer curves agree at every [`TRC_SAMPLE_POINTS`] sample
/// within [`TRC_EPSILON`].
///
/// The curves are compared through their evaluators, which is what makes a
/// parametric curve and an equivalent sampled table compare equal. A curve that
/// either side cannot evaluate is treated as different (the caller then converts,
/// which is the safe direction).
fn trc_close(a: Option<&ToneReprCurve>, b: Option<&ToneReprCurve>) -> bool {
    let (Some(a), Some(b)) = (a, b) else {
        return false;
    };
    let (Ok(a), Ok(b)) = (a.make_linear_evaluator(), b.make_linear_evaluator()) else {
        return false;
    };
    TRC_SAMPLE_POINTS
        .iter()
        .all(|&point| (a.evaluate_value(point) - b.evaluate_value(point)).abs() <= TRC_EPSILON)
}

#[cfg(test)]
mod tests;
