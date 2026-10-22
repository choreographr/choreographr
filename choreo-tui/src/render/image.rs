//! Image drawing: the fullscreen viewer and the per-turn inline blocks.
//!
//! Both surfaces are placeholders-until-encoded. A frame never blocks on an
//! encode: it draws a titled placeholder for the block's whole height and asks
//! the state layer (through the daemon's image-job channel) to produce the
//! protocol for the cell size it wants. That constant height is why scroll
//! positions stay stable while an image is still encoding — the block occupies
//! the same rows whether or not a protocol exists yet.
//!
//! Bytes can be absent in two different ways, and they take the same path here:
//! a tool-result vision image and a displayed image differ only in their slot
//! key, and a snapshot whose bytes were stripped (`data` empty, `byte_len` set)
//! is fetched on demand before any job is submitted. The render path holds no
//! client sender, so it queues a deduped fetch request rather than sending.
//!
//! Everything user-visible that comes from outside the TUI — a MIME type, a
//! model-chosen tool-call id in a slot label — goes through
//! [`display_text`] here, at the sink, so a title measures exactly the columns
//! it draws and a future caller inherits the rule instead of repeating it.

use crate::RenderedImage;
use crate::state::{App, ImageSlot, slot_source, turn_image_slots};
use ratatui::{
    Frame,
    layout::{Alignment, Rect, Size},
    widgets::Block,
};
use ratatui_image::StatefulImage;
use std::sync::Arc;

use super::display_text;

/// Look up the fullscreen image by (`turn_id`, [`ImageSlot`]) and render it.
pub(crate) fn render_fullscreen_only(frame: &mut Frame<'_>, app: &mut App) -> bool {
    let Some((session_id, turn_id, slot)) = app.fullscreen_image_target.clone() else {
        return false;
    };
    // De Morgan: the guard reads clearer as a single negation of "there is a
    // rendered image for this turn OR the turn still exposes image slots".
    let has_image = app
        .rendered_images
        .get(&session_id)
        .is_some_and(|m| m.contains_key(&turn_id))
        || app
            .display_for(session_id)
            .view
            .turns
            .get(&turn_id)
            .is_some_and(|t| !turn_image_slots(t).is_empty());
    if !has_image {
        app.fullscreen_image_target = None;
        return false;
    }
    render_fullscreen_image(frame, session_id, turn_id, slot, app);
    true
}

fn render_fullscreen_placeholder(frame: &mut Frame<'_>) {
    let area = frame.area();
    let block = Block::bordered()
        .title(" Loading image … ")
        .title_alignment(Alignment::Center);
    frame.render_widget(block, area);
}

fn render_fullscreen_image(
    frame: &mut Frame<'_>,
    session_id: u64,
    turn_id: u32,
    slot: ImageSlot,
    app: &mut App,
) {
    let area = frame.area();
    let full = Size::new(area.width, area.height);

    // Ensure the rendered_images entry exists — create from turn data if missing.
    if !app.rendered_images.contains_key(&session_id) {
        let Some((metadata, data)) = app
            .display_for(session_id)
            .view
            .turns
            .get(&turn_id)
            .and_then(|turn| slot_source(turn, &slot))
        else {
            return;
        };
        let placeholder = RenderedImage::new_placeholder(metadata, Arc::from(data));
        app.rendered_images
            .entry(session_id)
            .or_default()
            .entry(turn_id)
            .or_default()
            .insert(slot.clone(), placeholder);
    }

    // Fast path — already encoded at full size.
    let should_submit = match app
        .rendered_images
        .get_mut(&session_id)
        .and_then(|imgs| imgs.get_mut(&turn_id))
        .and_then(|images| images.get_mut(&slot))
    {
        Some(img) => {
            if let Some(protocol) = img.protocols.get_mut(&full) {
                let target = protocol.size_for(crate::IMAGE_RESIZE, full);
                let centered = Rect {
                    x: area.x + (area.width.saturating_sub(target.width)) / 2,
                    y: area.y + (area.height.saturating_sub(target.height)) / 2,
                    width: target.width.min(area.width),
                    height: target.height.min(area.height),
                };
                frame.render_stateful_widget(
                    StatefulImage::new().resize(crate::IMAGE_RESIZE),
                    centered,
                    protocol,
                );
                return;
            }
            // Submit job if not pending/failed/cached.
            img.pending_job.is_none()
                && !img.failed_sizes.contains(&full)
                && !img.protocols.contains_key(&full)
        }
        None => false,
    };

    if should_submit {
        // Extract the bytes/metadata while the immutable borrow is live, then
        // drop it before touching `app` mutably (to fetch or submit).
        let payload = app
            .rendered_images
            .get(&session_id)
            .and_then(|imgs| imgs.get(&turn_id))
            .and_then(|images| images.get(&slot))
            .map(|img| (img.data.clone(), img.metadata.clone()));
        if let Some((data, meta)) = payload {
            if data.is_empty() && meta.byte_len > 0 {
                // Bytes stripped from the snapshot: fetch on demand (see
                // `render_turn_image` for the full rationale).
                app.request_image_fetch(session_id, turn_id, slot);
            } else {
                app.submit_image_job(crate::state::ImageJobRequest {
                    session_id,
                    turn_id,
                    slot,
                    data,
                    metadata: meta,
                    cell_size: full,
                    resize: crate::IMAGE_RESIZE,
                });
            }
        }
    }

    render_fullscreen_placeholder(frame);
}

/// A short human label for an image slot, used in placeholder titles so the
/// reader can tell a displayed image from a tool-result vision image (and which
/// tool call it belongs to).
///
/// The `ToolResult` arm embeds a model-chosen `call_id`, so the label is
/// normalized here rather than at each call site: every title built from a
/// slot label is inert and measures exactly what it draws, and a future caller
/// inherits that without repeating the rule.
///
/// `pub(crate)` so the sink test can pin the label without drawing a full image
/// block.
pub(crate) fn slot_label(slot: &ImageSlot) -> String {
    match slot {
        ImageSlot::Displayed(index) => format!("[{index}]"),
        ImageSlot::ToolResult(call_id) => format!("[{}]", display_text(call_id)),
    }
}

/// Render a single turn image block (displayed or tool-result vision).
///
/// Height is always `image_block_height()` regardless of encoding state,
/// so scroll positions remain stable.
///
/// When `fully_visible` is true the image is centered within its block
/// using the protocol's actual rendered dimensions (via `size_for`). When
/// only a slice of the block is visible it is rendered without centering
/// to prevent visual reflow during scrolling.
pub(super) fn render_turn_image(
    frame: &mut Frame<'_>,
    area: Rect,
    session_id: u64,
    turn_id: u32,
    slot: ImageSlot,
    app: &mut App,
    fully_visible: bool,
) {
    let inline_size = Size::new(area.width, app.image_block_height());

    // Extract data we need while the borrow is active.
    let (needs_job, data, meta) = if let Some(img) = app
        .rendered_images
        .get_mut(&session_id)
        .and_then(|imgs| imgs.get_mut(&turn_id))
        .and_then(|images| images.get_mut(&slot))
    {
        if let Some(protocol) = img.protocols.get_mut(&inline_size) {
            // The mime type comes from the protocol, so it is normalized like
            // any other displayed value before it lands in a block title.
            let title = format!(
                "image {} ({} {}x{})",
                turn_id,
                display_text(&img.metadata.mime_type),
                img.metadata.width,
                img.metadata.height,
            );
            let block = Block::default().title(title);
            let inner = block.inner(area);
            frame.render_widget(block, area);
            if fully_visible {
                // Center the image within the block using the protocol's actual
                // rendered dimensions, preventing visual reflow when only part
                // of the block is visible.
                let rendered_at = protocol.size_for(crate::IMAGE_RESIZE, inline_size);
                let centered = Rect {
                    x: inner.x + (inner.width.saturating_sub(rendered_at.width)) / 2,
                    y: inner.y + (inner.height.saturating_sub(rendered_at.height)) / 2,
                    width: rendered_at.width.min(inner.width),
                    height: rendered_at.height.min(inner.height),
                };
                frame.render_stateful_widget(
                    StatefulImage::new().resize(crate::IMAGE_RESIZE),
                    centered,
                    protocol,
                );
            }
            return;
        }
        (
            img.pending_job.is_none()
                && !img.failed_sizes.contains(&inline_size)
                && !img.protocols.contains_key(&inline_size),
            img.data.clone(),
            img.metadata.clone(),
        )
    } else {
        let block =
            Block::default().title(format!("image {turn_id}{} (pending)", slot_label(&slot)));
        frame.render_widget(block, area);
        return;
    };

    if needs_job {
        if data.is_empty() && meta.byte_len > 0 {
            // The bytes were stripped from the turn snapshot: fetch them on
            // demand. The render path has no client sender, so
            // `request_image_fetch` queues it (deduped) and the UI loop sends
            // the `GetImage`; the reply stores the bytes and clears the failed
            // state so a later frame submits the decode job. This is the SAME
            // path for a displayed image and a tool-result vision image — only
            // the slot key differs.
            app.request_image_fetch(session_id, turn_id, slot);
        } else {
            app.submit_image_job(crate::state::ImageJobRequest {
                session_id,
                turn_id,
                slot,
                data,
                metadata: meta.clone(),
                cell_size: inline_size,
                resize: crate::IMAGE_RESIZE,
            });
        }
    }

    // Render placeholder frame while encoding is pending.
    // Use metadata from the RenderedImage entry (already populated by
    // sync_turn_images), not from the session turns.
    let placeholder_title = format!(
        "image {} ({} {}x{})",
        turn_id,
        display_text(&meta.mime_type),
        meta.width,
        meta.height,
    );
    let block = Block::default().title(placeholder_title);
    frame.render_widget(block, area);
}
