//! Unit tests for the HEIC decode and its header-only geometry probe.

use super::*;
use image::{ColorType, GenericImageView};

/// A 16×16 HEIC with no embedded colour profile and no alpha plane.
const FIXTURE: &[u8] = include_bytes!("../../tests/fixtures/heic-16x16.heic");
/// The same 16×16 picture tagged with an embedded `AdobeRGB` ICC profile.
const FIXTURE_ICC: &[u8] = include_bytes!("../../tests/fixtures/heic-16x16-icc.heic");

/// Encode a mixed-colour image as a HEIC, through `heifer`'s own encoder.
///
/// The encoder is the only way to obtain a container carrying the box structure
/// these tests need (`grid` for a tiled picture, `clap` for the padding crop)
/// without hand-assembling boxes, and going through the real encoder means the
/// probe is checked against the geometry a real file declares.
fn encode(width: u32, height: u32, format: heifer::EncodeFormat) -> Vec<u8> {
    let mut rgba = Vec::with_capacity((width * height * 4) as usize);
    for y in 0..height {
        for x in 0..width {
            // A saturated, non-neutral ramp: a colour transform must move it.
            #[expect(clippy::cast_possible_truncation)]
            rgba.extend_from_slice(&[(x % 256) as u8, (y % 256) as u8, 200, 255]);
        }
    }
    let options = heifer::EncodeOptions {
        max_threads: 1,
        ..Default::default()
    };
    heifer::encode_rgba8_with_format(width, height, &rgba, format, &options).expect("encode a heic")
}

/// Rewrite the container's `nclx` colour description in place.
///
/// The `colr` box holds `nclx`, then the primaries and transfer characteristics
/// as big-endian `u16`s, then the matrix and range. Patching the two code points
/// is how a Display-P3/BT.2020 (or an HDR) camera file is simulated without
/// shipping a second multi-megabyte fixture.
fn patch_nclx(heic: &[u8], primaries: u16, transfer: u16) -> Vec<u8> {
    let mut bytes = heic.to_vec();
    let idx = bytes
        .windows(4)
        .position(|w| w == b"nclx")
        .expect("the encoder writes an nclx colour description");
    bytes[idx + 4..idx + 6].copy_from_slice(&primaries.to_be_bytes());
    bytes[idx + 6..idx + 8].copy_from_slice(&transfer.to_be_bytes());
    bytes
}

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

/// The first pixel of a decoded image, as 8-bit RGB.
fn first_pixel(img: &DynamicImage) -> [u8; 3] {
    let px = img.to_rgba8().get_pixel(0, 0).0;
    [px[0], px[1], px[2]]
}

#[test]
fn decode_heic_rejects_non_heif_bytes() {
    let error = decode_heic(&[1, 2, 3]).unwrap_err();
    assert!(
        matches!(error, DecodeError::HeicGeometry(_)),
        "non-HEIF bytes should fail in the container parse, got: {error}"
    );
}

#[test]
fn decode_heic_decodes_the_plain_fixture() {
    let img = decode_heic(FIXTURE).expect("the 16x16 fixture should decode");
    assert_eq!(img.dimensions(), (16, 16));
    // A container with no alpha plane must come back as RGB, not RGBA with an
    // opaque alpha channel: the vision path chooses PNG over JPEG from the
    // colour type, so an always-RGBA buffer would re-encode every photo as PNG.
    assert_eq!(img.color(), ColorType::Rgb8);
}

#[test]
fn decode_heic_colour_manages_an_icc_tagged_fixture() {
    // An ICC-tagged HEIC decodes, and `heifer` applies the profile during the
    // decode. The assertion stops at "the profile is usable by a CMM": the
    // converted pixel values depend on the CMM's numerics, which is not
    // something this crate should pin.
    let img = decode_heic(FIXTURE_ICC).expect("the icc fixture should decode");
    assert_eq!(img.dimensions(), (16, 16));

    let metadata = heifer::read_metadata(FIXTURE_ICC).expect("metadata should parse");
    let icc = metadata
        .icc_profile
        .expect("the fixture carries an embedded ICC profile");
    assert!(
        moxcms::ColorProfile::new_from_slice(&icc).is_ok(),
        "the embedded profile must be usable by a colour engine"
    );
}

#[test]
fn decode_heic_applies_the_nclx_colour_description() {
    // A container that signals its colour through `nclx` (as Android and many
    // cameras do) must be converted to sRGB by the decode. `heifer`'s plain
    // entry point would hand back the coded samples, so comparing against it is
    // what pins the sRGB entry point being used.
    let heic = patch_nclx(
        &encode(32, 32, heifer::EncodeFormat::Lossless),
        9,  // BT.2020 primaries: wider than sRGB, so the numbers must move
        13, // sRGB transfer
    );
    let options = heifer::Options::default();
    let coded = heifer::decode_rgba8(&heic, &options).expect("decode the coded samples");
    let coded_pixel = [coded.data[0], coded.data[1], coded.data[2]];

    let ours = decode_heic(&heic).expect("the nclx-tagged heic should decode");
    let pixel = first_pixel(&ours);
    assert_ne!(
        pixel, coded_pixel,
        "BT.2020 samples re-read as sRGB is exactly the bug this pins"
    );
    assert_eq!(
        pixel,
        first_pixel(&DynamicImage::ImageRgba8(
            RgbaImage::from_raw(
                coded.width,
                coded.height,
                heifer::decode_srgb_rgba8(&heic, &options)
                    .expect("decode to sRGB")
                    .data,
            )
            .expect("the sRGB buffer matches its declared size"),
        )),
        "the decode must return the colour-managed samples"
    );
}

#[test]
fn decode_heic_keeps_the_coded_samples_when_the_colour_space_is_unsupported() {
    // PQ (`nclx` transfer 16) is HDR: `heifer` refuses to convert it to 8-bit
    // sRGB. That must not fail the photo — the coded samples come back, exactly
    // as an untagged source is handled.
    let heic = patch_nclx(
        &encode(32, 32, heifer::EncodeFormat::Lossless),
        9,
        16, // PQ / SMPTE ST 2084
    );
    assert!(
        heifer::decode_srgb_rgba8(&heic, &heifer::Options::default()).is_err(),
        "the fixture must actually exercise the unsupported-colour path"
    );

    let ours = decode_heic(&heic).expect("an unconvertible colour space must not fail the decode");
    let coded =
        heifer::decode_rgba8(&heic, &heifer::Options::default()).expect("decode the coded samples");
    assert_eq!(
        first_pixel(&ours),
        [coded.data[0], coded.data[1], coded.data[2]]
    );
}

#[test]
fn probe_heic_dimensions_reports_the_declared_size() {
    assert_eq!(
        probe_heic_dimensions(FIXTURE).expect("probe the plain fixture"),
        (16, 16)
    );
    assert_eq!(
        probe_heic_dimensions(FIXTURE_ICC).expect("probe the icc fixture"),
        (16, 16)
    );
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
            matches!(&error, DecodeError::HeicGeometry(reason) if reason.contains("decompression-bomb guard")),
            "expected the over-budget error, got: {error}"
        );
    }
}

#[test]
fn decode_heic_enforces_the_decoders_own_pixel_limit() {
    // The declared geometry (16x16) fits any sane budget, so this reaches
    // `heifer` and is rejected by *its* `max_pixels` bound against the picture
    // it would allocate — proving the in-decoder limit is actually wired, not
    // just our header pre-check.
    match decode_heic_with_budget(FIXTURE, 100).unwrap_err() {
        DecodeError::Heic(heifer::Error::Invalid(_) | heifer::Error::Hevc(_)) => {}
        other => panic!("expected the decoder's size-limit rejection, got: {other}"),
    }
}

#[test]
fn probe_dimensions_agree_with_the_decode() {
    // The probe re-derives the extent from the container's own transforms, so it
    // must predict exactly what the decoder produces for a cropped and for a
    // tiled picture. This is what makes a change in `heifer`'s crop/rotate
    // arithmetic a test failure here rather than a silently wrong size.
    // An odd size in 4:2:0 is rounded up to the chroma grid and then cropped
    // back with a `clap`, which is the transform the probe has to replay; the
    // grid below instead exercises the assembled-canvas path.
    let cropped = encode(65, 41, heifer::EncodeFormat::Ycbcr420);
    assert!(
        cropped.windows(4).any(|w| w == b"clap"),
        "an odd-sized 4:2:0 picture must be stored with a clap crop"
    );
    let tiled = encode(520, 100, heifer::EncodeFormat::Ycbcr420);
    assert!(
        tiled.windows(4).any(|w| w == b"grid"),
        "a picture wider than the tile size must be stored as a grid"
    );

    for (label, heic, expected) in [
        ("cropped", &cropped, (65, 41)),
        ("tiled", &tiled, (520, 100)),
    ] {
        let probe = probe_heic_dimensions(heic).unwrap_or_else(|e| panic!("{label}: probe: {e}"));
        let decode = decode_heic(heic).unwrap_or_else(|e| panic!("{label}: decode: {e}"));
        assert_eq!(probe, expected, "{label}: the probe must report the extent");
        assert_eq!(
            decode.dimensions(),
            expected,
            "{label}: the decoder must produce the extent the probe reported"
        );
    }
}
