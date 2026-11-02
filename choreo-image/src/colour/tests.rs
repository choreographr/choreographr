//! Unit tests for the ICC → sRGB conversion.

use super::*;
use image::{ExtendedColorType, ImageEncoder, Rgb, RgbImage, Rgba, RgbaImage};

/// A real Display-P3 profile, encoded by `moxcms` itself.
fn display_p3() -> Vec<u8> {
    ColorProfile::new_display_p3()
        .encode()
        .expect("encode a Display P3 profile")
}

/// A real sRGB profile, encoded by `moxcms` itself.
fn srgb() -> Vec<u8> {
    ColorProfile::new_srgb()
        .encode()
        .expect("encode an sRGB profile")
}

/// A profile whose *data colour space* is CMYK — the shape a CMYK-tagged raster
/// carries. Matrix data is irrelevant: the point is that the guard must reject
/// it by colour space alone.
fn cmyk() -> Vec<u8> {
    let mut profile = ColorProfile::new_srgb();
    profile.color_space = DataColorSpace::Cmyk;
    profile.encode().expect("encode a CMYK-space profile")
}

/// The single pixel of a solid 1×1 RGBA image after conversion.
fn rgba_pixel(mut img: DynamicImage, icc: &[u8]) -> [u8; 4] {
    img = to_srgb(img, icc);
    img.to_rgba8().get_pixel(0, 0).0
}

/// A saturated, non-neutral colour that a P3 → sRGB conversion must change.
const SATURATED: Rgb<u8> = Rgb([0, 128, 255]);

#[test]
fn unparsable_profile_leaves_the_image_unchanged() {
    // A broken ICC tag must never fail the decode: the pixels are assumed sRGB
    // and handed back exactly as they were.
    let img = DynamicImage::ImageRgb8(RgbImage::from_pixel(2, 2, Rgb([10, 20, 30])));
    let out = to_srgb(img.clone(), b"not an ICC profile");
    assert_eq!(out.to_rgb8(), img.to_rgb8());
}

#[test]
fn display_p3_is_converted_and_neutral_grey_stays_put() {
    let icc = display_p3();

    // A saturated P3 colour is remapped when its numbers are reinterpreted in
    // sRGB (the whole point of the conversion).
    let iimg = DynamicImage::ImageRgb8(RgbImage::from_pixel(2, 2, SATURATED));
    let got = to_srgb(iimg, &icc).to_rgb8().get_pixel(0, 0).0;
    assert!(
        got.iter()
            .zip(SATURATED.0.iter())
            .any(|(a, b)| a.abs_diff(*b) > 0),
        "a saturated P3 colour should change under sRGB conversion, got {got:?}"
    );

    // A neutral grey is D65 in both spaces, so it stays put (sanity that the
    // transform is not mangling the neutral axis).
    let grey = Rgb([128u8, 128, 128]);
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
fn sixteen_bit_conversion_keeps_the_bit_depth() {
    // A 16-bit source carrying a profile must be transformed *in 16-bit*: the
    // conversion must not be the reason a 16-bit photo comes back as 8-bit.
    let mut img = ImageBuffer::<image::Rgb<u16>, Vec<u16>>::from_pixel(
        2,
        2,
        image::Rgb([0u16, 32_896, 65_535]),
    );
    img.put_pixel(0, 1, image::Rgb([1_000, 2_000, 3_000]));
    let before = DynamicImage::ImageRgb16(img);

    let out = to_srgb(before.clone(), &display_p3());
    assert_eq!(
        out.color(),
        image::ColorType::Rgb16,
        "a 16-bit buffer must stay 16-bit through colour management"
    );
    assert_ne!(
        out.to_rgb16().as_raw(),
        before.to_rgb16().as_raw(),
        "the P3 samples should have been remapped"
    );
}

#[test]
fn alpha_is_carried_through_untouched() {
    let img = DynamicImage::ImageRgba8(RgbaImage::from_pixel(
        2,
        2,
        Rgba([SATURATED.0[0], SATURATED.0[1], SATURATED.0[2], 37]),
    ));
    let got = rgba_pixel(img, &display_p3());
    assert_eq!(
        got[3], 37,
        "alpha must not be touched by a colour transform"
    );
    assert!(
        got[..3]
            .iter()
            .zip(SATURATED.0.iter())
            .any(|(a, b)| a.abs_diff(*b) > 0),
        "the colour channels should still be converted, got {got:?}"
    );
}

#[test]
fn a_non_rgb_profile_is_ignored_rather_than_misapplied() {
    // `moxcms` accepts an `Rgba` layout for a CMYK source and would read R/G/B/A
    // as C/M/Y/K, mangling the colour and forcing alpha opaque. The guard must
    // skip such a profile entirely.
    let img = Rgba([SATURATED.0[0], SATURATED.0[1], SATURATED.0[2], 200]);
    let got = rgba_pixel(
        DynamicImage::ImageRgba8(RgbaImage::from_pixel(1, 1, img)),
        &cmyk(),
    );
    assert_eq!(got, img.0, "a non-RGB profile must leave the buffer alone");
}

#[test]
fn a_luma_buffer_is_left_as_decoded() {
    // No RGB primaries to reinterpret, whatever the tag says.
    let luma = image::GrayImage::from_pixel(2, 2, image::Luma([77]));
    let out = to_srgb(DynamicImage::ImageLuma8(luma.clone()), &display_p3());
    assert_eq!(out.to_luma8(), luma);
}

/// The sRGB EOTF sampled to a 16-bit table — the sampled-table encoding real
/// sRGB profiles use, and the counterpart of the parametric curve above.
fn sampled_srgb_curve() -> ToneReprCurve {
    /// One sample; the value is clamped to `[0, 1]` before scaling, so the
    /// truncation and sign-loss casts cannot lose anything.
    #[expect(clippy::cast_possible_truncation, clippy::cast_sign_loss)]
    fn sample(y: f32) -> u16 {
        (y.clamp(0.0, 1.0) * 65_535.0).round() as u16
    }

    ToneReprCurve::Lut(
        (0..256u16)
            .map(|i| {
                let x = f32::from(i) / 255.0;
                let y = if x <= 0.04045 {
                    x / 12.92
                } else {
                    ((x + 0.055) / 1.055).powf(2.4)
                };
                sample(y)
            })
            .collect(),
    )
}

#[test]
fn srgb_is_recognised_as_already_srgb() {
    let reference = ColorProfile::new_srgb();
    let parsed = ColorProfile::new_from_slice(&srgb()).expect("parse the encoded sRGB profile");
    assert!(
        is_srgb(&parsed, &reference),
        "an sRGB profile must be recognised so the identity pass is skipped"
    );

    // The spaces that motivated the conversion must not be mistaken for sRGB.
    for other in [
        ColorProfile::new_display_p3(),
        ColorProfile::new_adobe_rgb(),
        ColorProfile::new_pro_photo_rgb(),
    ] {
        assert!(!is_srgb(&other, &reference));
    }
}

#[test]
fn a_different_transfer_curve_is_not_srgb() {
    // Same primaries, different transfer curve (gamma 1.8): the conversion is
    // still needed, so the primaries alone must not decide.
    let mut gamma = ColorProfile::new_srgb();
    let curve = moxcms::curve_from_gamma(1.8);
    gamma.red_trc = Some(curve.clone());
    gamma.green_trc = Some(curve.clone());
    gamma.blue_trc = Some(curve);
    assert!(!is_srgb(&gamma, &ColorProfile::new_srgb()));
}

#[test]
fn matrix_close_is_not_fooled_by_a_near_miss_space() {
    // Guard the tolerance itself: Display-P3 shares D65 and the sRGB green-ish
    // ordering, so it is the closest well-known space to sRGB.
    let srgb = ColorProfile::new_srgb();
    let p3 = ColorProfile::new_display_p3();
    assert!(matrix_close(
        srgb.rgb_to_xyz_matrix(),
        srgb.rgb_to_xyz_matrix()
    ));
    assert!(!matrix_close(
        p3.rgb_to_xyz_matrix(),
        srgb.rgb_to_xyz_matrix()
    ));
}

#[test]
fn trc_close_compares_by_value_not_by_encoding() {
    // The same curve written as a sampled table and as a parametric curve must
    // compare equal: real sRGB profiles in the wild use either encoding.
    let reference = ColorProfile::new_srgb();
    let mut sampled = ColorProfile::new_srgb();
    let parametric =
        ToneReprCurve::Parametric(vec![2.4, 1. / 1.055, 0.055 / 1.055, 1. / 12.92, 0.04045]);
    sampled.red_trc = Some(sampled_srgb_curve());
    assert!(
        trc_close(sampled.red_trc.as_ref(), reference.red_trc.as_ref()),
        "an equivalent sampled curve must match the parametric one"
    );
    assert!(trc_close(Some(&parametric), reference.red_trc.as_ref()));
    // A missing curve is never a match.
    assert!(!trc_close(None, reference.red_trc.as_ref()));
}

#[test]
fn the_guard_does_not_depend_on_the_container() {
    // The same profile must behave identically whether it arrived in a PNG's
    // `iCCP` chunk or any other container: the conversion is a function of the
    // profile bytes and the buffer, not the file format.
    let mut out = std::io::Cursor::new(Vec::new());
    let mut encoder = image::codecs::png::PngEncoder::new(&mut out);
    encoder
        .set_icc_profile(display_p3())
        .expect("the PNG encoder supports ICC profiles");
    encoder
        .write_image(
            RgbImage::from_pixel(2, 2, SATURATED).as_raw(),
            2,
            2,
            ExtendedColorType::Rgb8,
        )
        .expect("encode png");
    let decoded = crate::decode_raster_oriented(&out.into_inner()).expect("decode a P3-tagged png");
    let got = decoded.to_rgb8().get_pixel(0, 0).0;
    assert!(
        got.iter()
            .zip(SATURATED.0.iter())
            .any(|(a, b)| a.abs_diff(*b) > 0),
        "the embedded profile should be applied, got {got:?}"
    );
}
