use super::*;

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
