//! Image plumbing: `sync_turn_images`, per-turn image-layout ranges, the
//! on-demand fetch path (`request_image_fetch`/`flush_image_fetches`/
//! `handle_image_reply`), `submit_image_job`, and `apply_image_result` failure
//! recording. These reach the code through `crate::state`/`crate::…` paths, so
//! per `crate::tests`'s split they live here rather than inline in
//! `state::images`.

use crate::markdown_render::{lines_height, render_turn_lines};
use crate::state::{ImageJobRequest, ImageSlot, turn_image_count, turn_image_slots};
use crate::tests::support::test_app;
use choreo_proto::{ClientMessage, ClientMessageType, ImageKey, Turn};
use ratatui::layout::Size;
use std::sync::Arc;

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

// ── on-demand fetch plumbing ──

#[test]
fn request_image_fetch_queues_only_stripped_nonempty_images() {
    // After protocol v6 the client receives displayed images WITHOUT their
    // bytes (only metadata). The render path queues a fetch for each image
    // that actually has bytes (`byte_len > 0`) — never for one whose bytes
    // are already present, nor for a genuinely zero-byte image.
    let mut app = test_app();
    let (tx, rx) = crossbeam_channel::unbounded::<ClientMessage>();
    let meta = |byte_len| choreo_proto::ImageMetadata {
        mime_type: "image/png".to_string(),
        width: 4,
        height: 4,
        byte_len,
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
            // 0: already has bytes — nothing to fetch.
            choreo_proto::DisplayedImageRecord {
                metadata: meta(4),
                data: b"AAAA".to_vec(),
                tool_call_id: None,
            },
            // 1: stripped (kept metadata, byte_len>0) — must be fetched.
            choreo_proto::DisplayedImageRecord {
                metadata: meta(8),
                data: Vec::new(),
                tool_call_id: None,
            },
            // 2: stripped AND empty (byte_len==0) — nothing to fetch.
            choreo_proto::DisplayedImageRecord {
                metadata: meta(0),
                data: Vec::new(),
                tool_call_id: None,
            },
        ],
        reasoning_artifact: None,
        reasoning_producer: None,
    };
    app.sync_turn_images(3, 9, &turn);
    app.request_image_fetch(3, 9, ImageSlot::Displayed(0));
    app.request_image_fetch(3, 9, ImageSlot::Displayed(1));
    app.request_image_fetch(3, 9, ImageSlot::Displayed(2));
    // A repeat call for the queued image must not double-queue (deduped via
    // the `fetching` flag until the reply arrives).
    app.request_image_fetch(3, 9, ImageSlot::Displayed(1));

    app.flush_image_fetches(&tx);
    let sent: Vec<ClientMessageType> = rx.try_iter().map(|m| m.inner).collect();
    assert_eq!(
        sent,
        vec![ClientMessageType::GetImage {
            session_id: 3,
            turn_id: 9,
            key: ImageKey::Displayed { index: 1 },
        }]
    );
}

#[test]
fn turn_image_count_matches_slots_len() {
    // The height path reserves one block per image from `turn_image_count`;
    // the render path walks the identities from `turn_image_slots`.  Both
    // must agree on the count for a turn mixing displayed images and
    // tool-result vision images (and a tool result with no image, which is
    // not counted).
    let turn = Turn {
        created_at: choreo_proto::TimestampMs::now(),
        undone: false,
        error: None,
        user_text: None,
        assistant_text: None,
        assistant_reasoning: None,
        tool_calls: vec![],
        token_usage: None,
        tool_results: vec![
            choreo_proto::ToolResultRecord {
                call_id: "call_a".into(),
                name: "read_image".into(),
                content: "image".into(),
                is_error: false,
                invocation_description: "read_image".into(),
                image: Some(choreo_proto::ImageReference {
                    path: "/tmp/a.png".into(),
                    mime_type: "image/png".into(),
                    width: 8,
                    height: 6,
                    data: Vec::new(),
                }),
            },
            // No image → must not be counted.
            choreo_proto::ToolResultRecord {
                call_id: "call_b".into(),
                name: "read_file".into(),
                content: "text".into(),
                is_error: false,
                invocation_description: "read_file".into(),
                image: None,
            },
        ],
        displayed_images: vec![choreo_proto::DisplayedImageRecord {
            metadata: choreo_proto::ImageMetadata {
                mime_type: "image/png".to_string(),
                width: 4,
                height: 4,
                byte_len: 4,
                alt: None,
            },
            data: b"AAAA".to_vec(),
            tool_call_id: None,
        }],
        reasoning_artifact: None,
        reasoning_producer: None,
    };
    assert_eq!(
        turn_image_count(&turn),
        2,
        "one displayed + one vision image"
    );
    assert_eq!(
        turn_image_count(&turn),
        turn_image_slots(&turn).len(),
        "the count must match the slot list the render path walks"
    );
}

#[test]
fn tool_result_vision_image_is_fetched_by_call_id() {
    // A `read_image` vision image rides a byte-less `ImageReference` on the
    // client view; the render path must fetch it under
    // `ImageKey::ToolResult { call_id }` and store the reply in the same
    // `RenderedImage` map, keyed by the call id.
    let mut app = test_app();
    let (tx, rx) = crossbeam_channel::unbounded::<ClientMessage>();
    let turn = Turn {
        created_at: choreo_proto::TimestampMs::now(),
        undone: false,
        error: None,
        user_text: None,
        assistant_text: None,
        assistant_reasoning: None,
        tool_calls: vec![],
        token_usage: None,
        tool_results: vec![choreo_proto::ToolResultRecord {
            call_id: "call_v".into(),
            name: "read_image".into(),
            content: "image".into(),
            is_error: false,
            invocation_description: "read_image".into(),
            // Client view: metadata present, bytes stripped.
            image: Some(choreo_proto::ImageReference {
                path: "/tmp/a.png".into(),
                mime_type: "image/jpeg".into(),
                width: 8,
                height: 6,
                data: Vec::new(),
            }),
        }],
        displayed_images: vec![],
        reasoning_artifact: None,
        reasoning_producer: None,
    };
    app.sync_turn_images(2, 5, &turn);
    let slot = ImageSlot::ToolResult("call_v".into());
    // A placeholder exists with the reference's metadata (path as alt).
    let img = &app.rendered_images[&2][&5][&slot];
    assert_eq!(img.metadata.mime_type, "image/jpeg");
    assert_eq!(img.metadata.alt.as_deref(), Some("/tmp/a.png"));
    assert!(img.data.is_empty());

    app.request_image_fetch(2, 5, slot.clone());
    app.flush_image_fetches(&tx);
    let sent: Vec<ClientMessageType> = rx.try_iter().map(|m| m.inner).collect();
    assert_eq!(
        sent,
        vec![ClientMessageType::GetImage {
            session_id: 2,
            turn_id: 5,
            key: ImageKey::ToolResult {
                call_id: "call_v".into()
            },
        }]
    );

    // The reply routes back to the call-id slot.
    app.handle_image_reply(
        2,
        5,
        ImageKey::ToolResult {
            call_id: "call_v".into(),
        },
        Some(b"JPEG!".to_vec()),
    );
    assert_eq!(app.rendered_images[&2][&5][&slot].data.as_ref(), b"JPEG!");
    assert!(!app.rendered_images[&2][&5][&slot].fetching);
}

#[test]
fn handle_image_reply_fills_bytes_and_marks_missing() {
    // A `Some` reply stores the bytes (clearing the in-flight flag and any
    // stale decode-failure state); a `None` reply marks the image failed so
    // it is not re-requested every frame.
    let mut app = test_app();
    let meta = choreo_proto::ImageMetadata {
        mime_type: "image/png".to_string(),
        width: 4,
        height: 4,
        byte_len: 4,
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
                metadata: meta.clone(),
                data: Vec::new(),
                tool_call_id: None,
            },
            choreo_proto::DisplayedImageRecord {
                metadata: meta.clone(),
                data: Vec::new(),
                tool_call_id: None,
            },
        ],
        reasoning_artifact: None,
        reasoning_producer: None,
    };
    app.sync_turn_images(1, 2, &turn);

    app.handle_image_reply(
        1,
        2,
        ImageKey::Displayed { index: 0 },
        Some(b"PNG!".to_vec()),
    );
    let img0 = &app.rendered_images[&1][&2][&ImageSlot::Displayed(0)];
    assert_eq!(img0.data.as_ref(), b"PNG!");
    assert!(!img0.fetching);
    assert!(!img0.fetch_failed);

    app.handle_image_reply(1, 2, ImageKey::Displayed { index: 1 }, None);
    let img1 = &app.rendered_images[&1][&2][&ImageSlot::Displayed(1)];
    assert!(img1.fetch_failed);
    assert!(!img1.fetching);
    assert!(img1.data.is_empty());

    // A failed image is never re-queued by a later render pass.
    app.request_image_fetch(1, 2, ImageSlot::Displayed(1));
    assert_eq!(app.pending_image_fetch.len(), 0);
}

#[test]
fn handle_image_reply_empty_for_nonempty_image_latches_failure() {
    // A malformed reply — `Some(vec![])` for an image advertised with
    // `byte_len > 0` — must NOT spin the fetch loop: the empty payload is
    // latched as a (recoverable) failure so the render path stops, exactly
    // like a `None` reply. `sync_turn_images` still clears the latch on a
    // later re-advertisement, so recovery is preserved.
    let mut app = test_app();
    let meta = choreo_proto::ImageMetadata {
        mime_type: "image/png".to_string(),
        width: 4,
        height: 4,
        byte_len: 8,
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
            metadata: meta,
            data: Vec::new(),
            tool_call_id: None,
        }],
        reasoning_artifact: None,
        reasoning_producer: None,
    };
    app.sync_turn_images(4, 1, &turn);

    // The daemon returns an empty payload despite `byte_len > 0`.
    app.handle_image_reply(4, 1, ImageKey::Displayed { index: 0 }, Some(Vec::new()));
    let img = &app.rendered_images[&4][&1][&ImageSlot::Displayed(0)];
    assert!(img.fetch_failed);
    assert!(img.data.is_empty());
    assert!(!img.fetching);

    // No re-request on the next render pass (the loop is broken).
    app.request_image_fetch(4, 1, ImageSlot::Displayed(0));
    assert_eq!(app.pending_image_fetch.len(), 0);

    // A later re-advertisement clears the latch (recovery still works).
    app.sync_turn_images(4, 1, &turn);
    assert!(!app.rendered_images[&4][&1][&ImageSlot::Displayed(0)].fetch_failed);
    app.request_image_fetch(4, 1, ImageSlot::Displayed(0));
    assert_eq!(
        app.pending_image_fetch,
        vec![(4, 1, ImageSlot::Displayed(0))]
    );
}

#[test]
fn sync_turn_images_recovers_transient_fetch_failure() {
    // A transient `None` (e.g. the emit-time storage write failed) latches
    // `fetch_failed`, which normally stops re-requesting. When the daemon
    // later finalizes and re-persists the image it re-broadcasts the turn,
    // which lands here; that re-advertisement must clear the latch so the
    // next render pass re-requests the image instead of hiding it forever.
    let mut app = test_app();
    let meta = choreo_proto::ImageMetadata {
        mime_type: "image/png".to_string(),
        width: 4,
        height: 4,
        byte_len: 8,
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
            metadata: meta.clone(),
            data: Vec::new(),
            tool_call_id: None,
        }],
        reasoning_artifact: None,
        reasoning_producer: None,
    };
    app.sync_turn_images(7, 3, &turn);

    // The daemon had no bytes yet: the fetch comes back `None`.
    app.handle_image_reply(7, 3, ImageKey::Displayed { index: 0 }, None);
    assert!(app.rendered_images[&7][&3][&ImageSlot::Displayed(0)].fetch_failed);
    // While latched, a render pass leaves the image alone.
    app.request_image_fetch(7, 3, ImageSlot::Displayed(0));
    assert_eq!(app.pending_image_fetch.len(), 0);

    // Finalize re-broadcasts the turn (image still advertised with
    // byte_len > 0 and no bytes inline): the latch must clear...
    app.sync_turn_images(7, 3, &turn);
    assert!(!app.rendered_images[&7][&3][&ImageSlot::Displayed(0)].fetch_failed);

    // ...so the next render pass re-queues the fetch.
    app.request_image_fetch(7, 3, ImageSlot::Displayed(0));
    assert_eq!(
        app.pending_image_fetch,
        vec![(7, 3, ImageSlot::Displayed(0))]
    );
}

// ── submit_image_job ──

#[test]
fn submit_image_job_fails_the_slot_when_the_worker_is_gone() {
    // The worker thread is a single point of failure for the whole process:
    // if it is gone, a job handed to its closed channel must not leave the
    // slot latched `pending_job` — the render path's `needs_job` requires
    // that marker to be clear, so the image would never be submitted again.
    // The failure latches the SIZE instead, exactly like a worker-reported
    // encode failure, so a later resize still retries.
    let mut app = test_app();
    let (tx, rx) = crossbeam_channel::unbounded();
    app.image_job_tx = Some(tx);
    // Dropping the receiver closes the channel — what a dead worker thread
    // looks like from the UI thread.
    drop(rx);

    let turn = turn_with_displayed_image();
    app.sync_turn_images(3, 9, &turn);
    let slot = ImageSlot::Displayed(0);
    let cell_size = Size::new(80, 20);

    let id = app.submit_image_job(ImageJobRequest {
        session_id: 3,
        turn_id: 9,
        slot: slot.clone(),
        data: Arc::from(b"PNG!".to_vec()),
        metadata: turn.displayed_images[0].metadata.clone(),
        cell_size,
        resize: crate::IMAGE_RESIZE,
    });

    assert!(id.is_none(), "a job that could not be queued yields no id");
    let img = &app.rendered_images[&3][&9][&slot];
    assert!(
        img.pending_job.is_none(),
        "the slot must not be left waiting for a result that cannot come"
    );
    assert!(
        img.failed_sizes.contains(&cell_size),
        "the size is latched so the render path stops re-submitting"
    );
    assert!(
        app.pending_job_idx.is_empty(),
        "no job-index entry may leak for a job that was never queued"
    );
}

#[test]
fn submit_image_job_queues_and_marks_the_slot_pending() {
    // The success path: the job reaches the worker, the slot carries the
    // pending marker, and the id is indexed so the result routes back to
    // this exact (session, turn, slot).
    let mut app = test_app();
    let (tx, rx) = crossbeam_channel::unbounded();
    app.image_job_tx = Some(tx);

    let turn = turn_with_displayed_image();
    app.sync_turn_images(3, 9, &turn);
    let slot = ImageSlot::Displayed(0);
    let cell_size = Size::new(80, 20);

    let id = app
        .submit_image_job(ImageJobRequest {
            session_id: 3,
            turn_id: 9,
            slot: slot.clone(),
            data: Arc::from(b"PNG!".to_vec()),
            metadata: turn.displayed_images[0].metadata.clone(),
            cell_size,
            resize: crate::IMAGE_RESIZE,
        })
        .expect("a live worker takes the job");

    assert_eq!(app.rendered_images[&3][&9][&slot].pending_job, Some(id));
    assert_eq!(
        app.pending_job_idx.get(&id),
        Some(&(3u64, 9u32, slot.clone()))
    );
    let queued = rx.try_recv().expect("the worker received the job");
    assert_eq!(queued.id, id);
    assert_eq!(queued.cell_size, cell_size);
}

/// A one-turn fixture exposing a single displayed image that still has its
/// bytes, for the `submit_image_job` tests.
fn turn_with_displayed_image() -> Turn {
    Turn {
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
            metadata: choreo_proto::ImageMetadata {
                mime_type: "image/png".to_string(),
                width: 4,
                height: 4,
                byte_len: 4,
                alt: None,
            },
            data: b"PNG!".to_vec(),
            tool_call_id: None,
        }],
        reasoning_artifact: None,
        reasoning_producer: None,
    }
}
