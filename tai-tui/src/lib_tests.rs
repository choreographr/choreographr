use super::*;
use image::GenericImageView;
use image::ImageEncoder;

#[test]
fn image_assembler_tracks_lifecycle() {
    let mut assembler = ImageAssembler::new();
    let metadata = ImageMetadata {
        image_id: 11,
        mime_type: "image/png".to_string(),
        width: 1,
        height: 1,
        byte_len: 4,
        alt: Some("tiny".to_string()),
    };

    assembler.start(7, metadata.clone()).expect("start");
    assembler.push_chunk(7, 11, &[1, 2]).expect("chunk1");
    assembler.push_chunk(7, 11, &[3, 4]).expect("chunk2");
    let (actual_metadata, data) = assembler.finish(7, 11).expect("finish");

    assert_eq!(actual_metadata, metadata);
    assert_eq!(data, vec![1, 2, 3, 4]);
}

#[test]
fn image_assembler_rejects_unknown_chunk() {
    let mut assembler = ImageAssembler::new();
    let error = assembler.push_chunk(1, 2, &[3]).expect_err("should fail");
    assert!(matches!(
        error,
        ClientError::UnknownImage {
            image_id: 2,
            request_id: 1
        }
    ));
}

#[test]
fn image_assembler_rejects_duplicate_start() {
    let mut assembler = ImageAssembler::new();
    let metadata = ImageMetadata {
        image_id: 2,
        mime_type: "image/png".to_string(),
        width: 1,
        height: 1,
        byte_len: 1,
        alt: None,
    };

    assembler.start(1, metadata.clone()).expect("first start");
    let error = assembler.start(1, metadata).expect_err("should fail");
    assert!(matches!(
        error,
        ClientError::DuplicateImage {
            image_id: 2,
            request_id: 1
        }
    ));
}

#[test]
fn image_assembler_rejects_wrong_final_size() {
    let mut assembler = ImageAssembler::new();
    assembler
        .start(
            1,
            ImageMetadata {
                image_id: 9,
                mime_type: "image/png".to_string(),
                width: 1,
                height: 1,
                byte_len: 3,
                alt: None,
            },
        )
        .expect("start");
    assembler.push_chunk(1, 9, &[1, 2]).expect("chunk");

    let error = assembler.finish(1, 9).expect_err("should fail");
    assert!(matches!(
        error,
        ClientError::ImageSizeMismatch {
            image_id: 9,
            request_id: 1,
            expected: 3,
            actual: 2
        }
    ));
}

#[test]
fn image_assembler_drop_request_clears_pending_images() {
    let mut assembler = ImageAssembler::new();
    assembler
        .start(
            4,
            ImageMetadata {
                image_id: 7,
                mime_type: "image/png".to_string(),
                width: 1,
                height: 1,
                byte_len: 1,
                alt: None,
            },
        )
        .expect("start");

    assembler.drop_request(4);
    let error = assembler.finish(4, 7).expect_err("should fail");
    assert!(matches!(
        error,
        ClientError::UnknownImage {
            image_id: 7,
            request_id: 4
        }
    ));
}

#[test]
fn build_rendered_image_rejects_invalid_bytes() {
    let picker = Picker::halfblocks();
    let result = build_rendered_image(
        &picker,
        ImageMetadata {
            image_id: 1,
            mime_type: "image/png".to_string(),
            width: 1,
            height: 1,
            byte_len: 3,
            alt: None,
        },
        vec![1, 2, 3],
    );
    let error = match result {
        Ok(_) => panic!("should fail"),
        Err(error) => error,
    };
    assert_eq!(error.kind(), io::ErrorKind::Other);
}

#[test]
fn build_rendered_image_rasterizes_svg() {
    let picker = Picker::halfblocks();
    let svg = br#"<svg xmlns='http://www.w3.org/2000/svg' width='4' height='3'><rect width='4' height='3' fill='red'/></svg>"#;
    let result = build_rendered_image(
        &picker,
        ImageMetadata {
            image_id: 2,
            mime_type: "image/svg+xml".to_string(),
            width: 4,
            height: 3,
            byte_len: svg.len() as u64,
            alt: Some("red rectangle".to_string()),
        },
        svg.to_vec(),
    );

    let image = result.expect("svg should render");
    assert_eq!(image.metadata.mime_type, "image/svg+xml");
}

#[test]
fn rasterize_svg_at_size_produces_correct_dimensions() {
    let svg = br#"<svg xmlns='http://www.w3.org/2000/svg' width='100' height='50'><rect width='100' height='50' fill='blue'/></svg>"#;
    let image = rasterize_svg_at_size(svg, 200, 100).expect("should rasterize");
    let (w, h) = image.dimensions();
    assert_eq!(w, 200, "width constrained by target width");
    assert_eq!(h, 100, "height constrained proportionally");
}

#[test]
fn rasterize_svg_at_size_preserves_aspect_ratio() {
    let svg = br#"<svg xmlns='http://www.w3.org/2000/svg' width='100' height='50'><rect width='100' height='50' fill='green'/></svg>"#;
    // 100x100 target is narrower than 2:1 -> width-constrained, height stays 50
    let image = rasterize_svg_at_size(svg, 100, 100).expect("should rasterize");
    let (w, h) = image.dimensions();
    assert_eq!(w, 100, "width constrained by target width");
    assert_eq!(h, 50, "height preserves 2:1 aspect ratio");
}

#[test]
fn ensure_display_resolution_rasterizes_svg_at_target_cell_size() {
    let picker = Picker::halfblocks();
    let svg = br#"<svg xmlns='http://www.w3.org/2000/svg' width='4' height='3'><rect width='4' height='3' fill='red'/></svg>"#;
    let mut image = build_rendered_image(
        &picker,
        ImageMetadata {
            image_id: 10,
            mime_type: "image/svg+xml".to_string(),
            width: 4,
            height: 3,
            byte_len: svg.len() as u64,
            alt: None,
        },
        svg.to_vec(),
    )
    .expect("svg should render");

    assert!(image.svg_data.is_some(), "SVG data should be preserved");
    assert!(
        image.rasterized_cells.is_none(),
        "no cache entry before first call"
    );

    let cell_size = Size::new(20, 16);
    image
        .ensure_display_resolution(&picker, cell_size)
        .expect("re-rasterization should succeed");

    assert_eq!(
        image.rasterized_cells,
        Some(cell_size),
        "cache entry set after re-rasterization"
    );

    // Second call with same size is a cache hit (no error, cache unchanged)
    image
        .ensure_display_resolution(&picker, cell_size)
        .expect("cache hit should succeed");
    assert_eq!(image.rasterized_cells, Some(cell_size));
}

#[test]
fn ensure_display_resolution_skips_non_svg() {
    let picker = Picker::halfblocks();
    // Build a valid 1x1 white PNG in memory via the image crate
    let mut png_buf = std::io::Cursor::new(Vec::new());
    let encoder = image::codecs::png::PngEncoder::new(&mut png_buf);
    encoder
        .write_image(&[255u8; 4], 1, 1, image::ExtendedColorType::Rgba8)
        .expect("encode 1x1 png");
    let png_data = png_buf.into_inner();
    let mut image = build_rendered_image(
        &picker,
        ImageMetadata {
            image_id: 11,
            mime_type: "image/png".to_string(),
            width: 1,
            height: 1,
            byte_len: png_data.len() as u64,
            alt: None,
        },
        png_data,
    )
    .expect("png should render");

    assert!(image.svg_data.is_none(), "PNG has no SVG data");
    assert!(image.rasterized_cells.is_none());

    // ensure_display_resolution should be a no-op for non-SVG images
    image
        .ensure_display_resolution(&picker, Size::new(20, 16))
        .expect("non-SVG no-op should not error");
    assert!(
        image.rasterized_cells.is_none(),
        "rasterized_cells stays None for non-SVG"
    );
}
