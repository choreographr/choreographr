//! Publish-time image processing: turn a local image file into the fully
//! specified [`ImageSpec`] the content protocol expects.
//!
//! This is the daemon-side port of the reference `acuity-dioxus`
//! `build_image_mixin` pipeline: the file is decoded, a JPEG mipmap pyramid
//! (level 0 = full resolution, each next level halved, Lanczos3) is encoded at
//! the reference quality, every level is uploaded to IPFS as its own pinned
//! CID, and the sha2-256 digest of the *original* file bytes becomes the
//! mixin's `ipfs_hash`. Callers of the publish tools therefore only need to
//! pass a `path` — dimensions, digest, sizes, and mipmap levels are derived
//! here rather than supplied by the caller.

use crate::ContentError;
use crate::encode::{ImageSpec, MipmapLevel, bytes_to_hex};
use image::{DynamicImage, GenericImageView, codecs::jpeg::JpegEncoder, imageops::FilterType};
use sha2::{Digest, Sha256};
use std::io::Cursor;
use std::path::Path;

/// JPEG quality for every stored mipmap level (reference protocol value).
const JPEG_QUALITY: u8 = 82;
/// The pyramid stops once a level's width or height is at or below this.
const MIN_LEVEL_DIMENSION: u32 = 64;

/// Width/height of one mipmap level, rounded and clamped to >= 1 so tiny
/// images never produce zero-sized encodes (reference arithmetic).
fn level_dimensions(original: u32, level: u32) -> u32 {
    let scale = 2_u32.pow(level);
    ((original as f32) / (scale as f32)).round().max(1.0) as u32
}

/// The `(width, height)` of every level in the pyramid for a full-res
/// `width x height` image. Pure so the pyramid shape is unit-testable
/// without the image or IPFS dependencies.
fn plan_levels(width: u32, height: u32) -> Vec<(u32, u32)> {
    let mut plan = Vec::new();
    let mut level = 0_u32;
    loop {
        let (w, h) = (
            level_dimensions(width, level),
            level_dimensions(height, level),
        );
        plan.push((w, h));
        if w <= MIN_LEVEL_DIMENSION || h <= MIN_LEVEL_DIMENSION {
            break;
        }
        level += 1;
    }
    plan
}

/// Encode a decoded image as a quality-82 JPEG (the protocol's storage
/// format — every level, including level 0, is a JPEG re-encode).
pub(crate) fn encode_as_jpeg(image: &DynamicImage) -> Result<Vec<u8>, ContentError> {
    let mut bytes = Vec::new();
    let mut cursor = Cursor::new(&mut bytes);
    JpegEncoder::new_with_quality(&mut cursor, JPEG_QUALITY)
        .encode_image(image)
        .map_err(|e| ContentError::Image(format!("failed to encode JPEG level: {e}")))?;
    Ok(bytes)
}

/// Build the mipmap pyramid for a decoded image, handing each level's JPEG
/// bytes to `upload` (which pins them to IPFS and returns the CID). The
/// uploader is injected so the pyramid logic is unit-testable without a live
/// IPFS daemon; the production caller passes a closure over
/// [`crate::ipfs::add`] + digest->CID conversion.
fn build_levels(
    image: &DynamicImage,
    mut upload: impl FnMut(&[u8]) -> Result<String, ContentError>,
) -> Result<Vec<MipmapLevel>, ContentError> {
    let (width, height) = image.dimensions();
    let mut levels = Vec::new();
    for (level, (out_width, out_height)) in plan_levels(width, height).into_iter().enumerate() {
        // Level 0 is the full-resolution encode; deeper levels are exact-size
        // Lanczos3 downscales so the (width, height) plan is honored exactly.
        let resized = if level == 0 {
            image.clone()
        } else {
            image.resize_exact(out_width, out_height, FilterType::Lanczos3)
        };
        let jpeg_bytes = encode_as_jpeg(&resized)?;
        let cid = upload(&jpeg_bytes)?;
        levels.push(MipmapLevel {
            filesize: jpeg_bytes.len() as u64,
            cid,
        });
    }
    Ok(levels)
}

/// Read `path`, build the full mipmap pyramid (uploading each level to IPFS),
/// and assemble the complete [`ImageSpec`] for the image mixin.
///
/// `filename_override` replaces the stored filename when set; otherwise the
/// path's own file name is used. The mixin's digest is the sha2-256 of the
/// original file bytes (matching what `acuity-dioxus` hashes into
/// `ipfs_hash`), while the mipmap levels reference the JPEG re-encodes.
pub fn build_image_spec(
    path: &Path,
    filename_override: Option<&str>,
) -> Result<ImageSpec, ContentError> {
    let bytes = std::fs::read(path).map_err(|e| {
        ContentError::Image(format!("failed to read image {}: {e}", path.display()))
    })?;
    let image = image::load_from_memory(&bytes).map_err(|e| {
        ContentError::Image(format!("failed to decode image {}: {e}", path.display()))
    })?;
    let (width, height) = image.dimensions();

    // Each level is pinned to IPFS under its own CID; the digest returned by
    // `ipfs::add` is converted back to the CID form the spec stores.
    let filename = filename_override
        .map(str::to_owned)
        .unwrap_or_else(|| fallback_filename(path));
    let upload = |jpeg_bytes: &[u8]| -> Result<String, ContentError> {
        let digest_hex = crate::ipfs::add(jpeg_bytes, &filename)?;
        crate::encode::digest_hex_to_cid(&digest_hex)
    };
    let mipmap_levels = build_levels(&image, upload)?;

    // The mixin-level digest covers the *original* file bytes, not the
    // level-0 JPEG re-encode — it identifies the file the publisher attached.
    let mut hasher = Sha256::new();
    hasher.update(&bytes);
    let digest: [u8; 32] = hasher.finalize().into();

    Ok(ImageSpec {
        filename,
        filesize: bytes.len() as u64,
        digest_hex: bytes_to_hex(&digest),
        width,
        height,
        mipmap_levels,
    })
}

/// Stored filename fallback for paths with no final component (e.g. `/`).
/// Protocol mixins expect a non-empty filename, so we substitute a constant
/// rather than fail a publish that has perfectly good image bytes.
fn fallback_filename(path: &Path) -> String {
    path.file_name()
        .map(|n| n.to_string_lossy().into_owned())
        .unwrap_or_else(|| "image".to_string())
}

#[cfg(test)]
mod tests {
    use super::*;
    use image::{ImageBuffer, Rgba};

    /// A deterministic 300x200 test image (small enough that the pyramid is
    /// short: 300x200 -> 150x100 -> 75x50, stop at 75 <= 64? no — 75x50 stops
    /// on the height, giving 3 levels).
    fn test_image() -> DynamicImage {
        DynamicImage::ImageRgba8(ImageBuffer::from_fn(300, 200, |x, y| {
            Rgba([(x % 256) as u8, (y % 256) as u8, 128, 255])
        }))
    }

    #[test]
    fn level_dimensions_round_and_clamp_to_one() {
        assert_eq!(level_dimensions(1012, 0), 1012);
        assert_eq!(level_dimensions(1012, 1), 506);
        // 5 / 4 = 1.25 rounds to 1; 1 / 2 = 0.5 clamps to 1, never 0.
        assert_eq!(level_dimensions(5, 2), 1);
        assert_eq!(level_dimensions(1, 10), 1);
    }

    #[test]
    fn plan_levels_halves_until_min_dimension() {
        // 1012x1012: 1012 -> 506 -> 253 -> 127 -> 63 (stop, <= 64; the
        // reference rounds each halving, so 127/2 = 63.5 lands on 63).
        let plan = plan_levels(1012, 1012);
        assert_eq!(
            plan,
            vec![(1012, 1012), (506, 506), (253, 253), (127, 127), (63, 63)]
        );

        // A small image is a single full-res level when already at the floor.
        assert_eq!(plan_levels(64, 64), vec![(64, 64)]);

        // Non-square images stop as soon as EITHER dimension hits the floor.
        let wide = plan_levels(300, 200);
        assert_eq!(wide, vec![(300, 200), (150, 100), (75, 50)]);
    }

    #[test]
    fn encode_as_jpeg_produces_decodable_jpeg_at_dimensions() {
        let jpeg = encode_as_jpeg(&test_image()).unwrap();
        let decoded = image::load_from_memory(&jpeg).unwrap();
        assert_eq!(
            image::guess_format(&jpeg).unwrap(),
            image::ImageFormat::Jpeg
        );
        assert_eq!(decoded.dimensions(), (300, 200));
    }

    #[test]
    fn build_levels_uploads_every_level_and_records_sizes() {
        let image = test_image();

        // Fake uploader: no IPFS, just hand back a deterministic pseudo-CID so
        // the pyramid logic is exercised end-to-end offline.
        let mut calls = 0;
        let levels = build_levels(&image, |bytes| {
            calls += 1;
            assert!(!bytes.is_empty());
            Ok(format!("QmFake{calls}"))
        })
        .unwrap();

        assert_eq!(levels.len(), 3);
        assert_eq!(levels[0].cid, "QmFake1");
        assert_eq!(levels[2].cid, "QmFake3");
        // Full-res level is the largest; each deeper level shrinks.
        assert!(levels[0].filesize > levels[1].filesize);
        assert!(levels[1].filesize > levels[2].filesize);
        // `calls` is borrowed mutably inside the closure; read it after.
        assert_eq!(calls, 3);
    }

    #[test]
    fn fallback_filename_handles_rootless_paths() {
        assert_eq!(fallback_filename(Path::new("/tmp/pic.png")), "pic.png");
        assert_eq!(fallback_filename(Path::new("/")), "image");
    }

    #[test]
    fn build_image_spec_reports_missing_files() {
        let err = build_image_spec(Path::new("/nonexistent/img.png"), None).unwrap_err();
        assert!(err.to_string().contains("failed to read image"));
    }

    #[test]
    fn build_image_spec_rejects_non_image_bytes() {
        let dir = std::env::temp_dir().join(format!("choreo-content-image-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("not-an-image.bin");
        std::fs::write(&path, b"definitely not an image").unwrap();

        let err = build_image_spec(&path, None).unwrap_err();
        assert!(err.to_string().contains("failed to decode image"));

        std::fs::remove_dir_all(&dir).unwrap();
    }
}
