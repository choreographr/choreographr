//! Displayed-image plumbing: `sync_turn_images`, per-turn image layout ranges,
//! and `apply_image_result` failure recording.

use crate::markdown_render::{lines_height, render_turn_lines};
use crate::state::ImageSlot;
use crate::test_util::test_app;
use choreo_proto::Turn;
use ratatui::layout::Size;

#[test]
fn sync_turn_images_populates_rendered_images() {
    let mut app = test_app();
    let metadata = choreo_proto::ImageMetadata {
        mime_type: "image/svg+xml".to_string(),
        width: 100,
        height: 200,
        byte_len: 50,
        alt: None,
    };
    let turn = Turn {
        created_at: choreo_proto::TimestampMs::now(),
        undone: false,
        error: None,
        user_text: None,
        assistant_text: None,
        assistant_reasoning: None,
        tool_calls: vec![],
        token_usage: None,
        tool_results: vec![],
        displayed_images: vec![
            choreo_proto::DisplayedImageRecord {
                metadata: metadata.clone(),
                data: b"svg-data".to_vec(),
                tool_call_id: Some("call-1".into()),
            },
            choreo_proto::DisplayedImageRecord {
                metadata: metadata.clone(),
                data: b"more-svg".to_vec(),
                tool_call_id: None,
            },
        ],
        reasoning_artifact: None,
        reasoning_producer: None,
    };
    app.sync_turn_images(0, 42, &turn);

    let images = app.rendered_images.get(&0).unwrap().get(&42).unwrap();
    assert_eq!(images.len(), 2);
    assert_eq!(images[&ImageSlot::Displayed(0)].data.as_ref(), b"svg-data");
    assert_eq!(images[&ImageSlot::Displayed(1)].data.as_ref(), b"more-svg");
    // Second call is idempotent — preserves existing entries
    app.sync_turn_images(0, 42, &turn);
    assert_eq!(
        app.rendered_images.get(&0).unwrap().get(&42).unwrap().len(),
        2
    );
}

// ── TurnImageLayout image_ranges ──

#[test]
#[expect(clippy::assert_is_empty)] // clearer than assert_eq! against []
fn turn_layout_empty_when_no_images() {
    let mut app = test_app();
    app.history_viewport.width = 80;
    app.history_viewport.height = 20;
    let turn = Turn {
        created_at: choreo_proto::TimestampMs::now(),
        undone: false,
        error: None,
        user_text: Some("hello".into()),
        assistant_text: Some("world".into()),
        assistant_reasoning: None,
        tool_calls: vec![],
        token_usage: None,
        tool_results: vec![],
        displayed_images: vec![],
        reasoning_artifact: None,
        reasoning_producer: None,
    };
    app.active_display()
        .unwrap()
        .view
        .insert_or_replace(1, turn);
    app.rebuild_height_prefix();

    assert_eq!(app.active_display().unwrap().turn_layouts.len(), 1);
    assert!(
        app.active_display().unwrap().turn_layouts[0]
            .image_ranges
            .is_empty()
    );
}

#[test]
fn turn_layout_populates_image_ranges_with_fallback_height() {
    let mut app = test_app();
    app.history_viewport.width = 80;
    app.history_viewport.height = 20;
    let metadata = choreo_proto::ImageMetadata {
        mime_type: "image/png".to_string(),
        width: 100,
        height: 100,
        byte_len: 500,
        alt: None,
    };
    let turn = Turn {
        created_at: choreo_proto::TimestampMs::now(),
        undone: false,
        error: None,
        user_text: None,
        assistant_text: Some("short".into()),
        assistant_reasoning: None,
        tool_calls: vec![],
        token_usage: None,
        tool_results: vec![],
        displayed_images: vec![
            choreo_proto::DisplayedImageRecord {
                metadata: metadata.clone(),
                data: vec![0u8; 10],
                tool_call_id: None,
            },
            choreo_proto::DisplayedImageRecord {
                metadata: metadata.clone(),
                data: vec![1u8; 10],
                tool_call_id: None,
            },
        ],
        reasoning_artifact: None,
        reasoning_producer: None,
    };
    let turn_clone = turn.clone();
    app.active_display()
        .unwrap()
        .view
        .insert_or_replace(2, turn);
    app.sync_turn_images(0, 2, &turn_clone);
    app.rebuild_height_prefix();

    assert_eq!(app.active_display().unwrap().turn_layouts.len(), 1);
    // Mutable borrow dropped.

    // Capture needed values before taking another mutable borrow for layout.
    let fallback_h = app.image_block_height() as usize;
    let vp_width = app.history_viewport.width;
    let text_h = {
        let display = app.active_display().unwrap();
        let turn = &display.view.turns[&2];
        lines_height(
            &render_turn_lines(turn, 71, vp_width, false, &[]).lines,
            vp_width,
        )
        .max(1)
    };

    let layout = &app.active_display().unwrap().turn_layouts[0];
    assert_eq!(layout.image_ranges.len(), 2);

    let (s0, e0) = layout.image_ranges[0];
    assert_eq!(s0, text_h);
    assert_eq!(e0, text_h + fallback_h);

    let (s1, e1) = layout.image_ranges[1];
    assert_eq!(s1, text_h + fallback_h);
    assert_eq!(e1, text_h + 2 * fallback_h);
}
// ── apply_image_result ──

#[test]
fn apply_image_result_clears_pending_job_and_records_failure() {
    use crate::image_worker::next_job_id;

    let mut app = test_app();
    app.history_viewport.width = 80;
    app.history_viewport.height = 20;
    let metadata = choreo_proto::ImageMetadata {
        mime_type: "image/png".to_string(),
        width: 100,
        height: 100,
        byte_len: 500,
        alt: None,
    };
    let turn = Turn {
        created_at: choreo_proto::TimestampMs::now(),
        undone: false,
        error: None,
        user_text: None,
        assistant_text: None,
        assistant_reasoning: None,
        tool_calls: vec![],
        token_usage: None,
        tool_results: vec![],
        displayed_images: vec![choreo_proto::DisplayedImageRecord {
            metadata: metadata.clone(),
            data: vec![3u8; 30],
            tool_call_id: None,
        }],
        reasoning_artifact: None,
        reasoning_producer: None,
    };
    let turn_clone = turn.clone();
    app.active_display()
        .unwrap()
        .view
        .insert_or_replace(4, turn);
    app.sync_turn_images(0, 4, &turn_clone);

    let img_id = next_job_id();
    app.pending_job_idx
        .insert(img_id, (0, 4, ImageSlot::Displayed(0)));
    let img = app
        .rendered_images
        .get_mut(&0)
        .unwrap()
        .get_mut(&4)
        .unwrap()
        .get_mut(&ImageSlot::Displayed(0))
        .unwrap();
    img.pending_job = Some(img_id);

    let inline_size = Size::new(app.history_viewport.width, app.image_block_height());
    let result = crate::image_worker::ImageResult {
        id: img_id,
        protocol: None,
        cell_size: inline_size,
    };
    app.apply_image_result(result);

    let img = app
        .rendered_images
        .get(&0)
        .unwrap()
        .get(&4)
        .unwrap()
        .get(&ImageSlot::Displayed(0))
        .unwrap();
    assert!(img.failed_sizes.contains(&inline_size));
    assert!(img.pending_job.is_none());
}

#[test]
fn apply_image_result_records_failure_at_any_size() {
    use crate::image_worker::next_job_id;

    let mut app = test_app();
    app.history_viewport.width = 80;
    app.history_viewport.height = 20;
    let metadata = choreo_proto::ImageMetadata {
        mime_type: "image/png".to_string(),
        width: 100,
        height: 100,
        byte_len: 500,
        alt: None,
    };
    let turn = Turn {
        created_at: choreo_proto::TimestampMs::now(),
        undone: false,
        error: None,
        user_text: None,
        assistant_text: None,
        assistant_reasoning: None,
        tool_calls: vec![],
        token_usage: None,
        tool_results: vec![],
        displayed_images: vec![choreo_proto::DisplayedImageRecord {
            metadata: metadata.clone(),
            data: vec![4u8; 40],
            tool_call_id: None,
        }],
        reasoning_artifact: None,
        reasoning_producer: None,
    };
    let turn_clone = turn.clone();
    app.active_display()
        .unwrap()
        .view
        .insert_or_replace(5, turn);
    app.sync_turn_images(0, 5, &turn_clone);

    let img_id = next_job_id();
    app.pending_job_idx
        .insert(img_id, (0, 5, ImageSlot::Displayed(0)));
    let img = app
        .rendered_images
        .get_mut(&0)
        .unwrap()
        .get_mut(&5)
        .unwrap()
        .get_mut(&ImageSlot::Displayed(0))
        .unwrap();
    img.pending_job = Some(img_id);

    // Use a cell_size that is NOT the inline size.
    let non_inline_size = Size::new(80, app.image_block_height() + 1);
    let result = crate::image_worker::ImageResult {
        id: img_id,
        protocol: None,
        cell_size: non_inline_size,
    };
    app.apply_image_result(result);

    let img = app
        .rendered_images
        .get(&0)
        .unwrap()
        .get(&5)
        .unwrap()
        .get(&ImageSlot::Displayed(0))
        .unwrap();
    assert!(img.failed_sizes.contains(&non_inline_size));
}
