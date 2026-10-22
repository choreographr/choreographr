//! The chat history pane: the visible window of turns and their text blocks.
//!
//! The pane is drawn bottom-up. Turns are walked from newest to oldest and
//! [`clipped_area`] consumes the scroll offset from *that* end, so a turn that
//! starts below the viewport is skipped before any of its content is touched
//! and the loop stops as soon as the viewport is full. Every block — an image
//! block, a turn's text — reserves its rows the same way, which is why the
//! caller must render images before text: images sit *below* their turn's text
//! on screen, and the shared `y` cursor is what keeps the two in order.
//!
//! Two widths are threaded separately. Message blocks lose nine columns of
//! chrome (a two-column margin, the `┃` gutter and the shading on both sides)
//! while tool rows lose one, and both budgets must stay in lockstep with the
//! line builder in `markdown_render` or rows will not fill the viewport.
//!
//! Per-turn lines come from the render cache keyed by every input that can
//! change them; the selection highlight is applied to the *cached* lines at
//! draw time rather than baked into the cache, so the cache stays pure and the
//! highlighted bytes are exactly the bytes a copy takes.

use crate::markdown_render::{reasoning_expanded_default, render_turn_lines};
use crate::selection;
use crate::state::{App, RenderCacheKey, cached_or_compute_lines, turn_image_slots};
use ratatui::{Frame, layout::Rect, style::Style, text::Line, widgets::Paragraph};

use super::image::render_turn_image;

pub(super) fn render_history(frame: &mut Frame<'_>, area: Rect, app: &mut App) {
    if area.width == 0 || area.height == 0 {
        return;
    }

    // Text-wrap budgets: message blocks lose 9 columns of chrome (a 2-col
    // margin + `┃` gutter + 2-col shading on the left; 2 shaded + 2 blank on
    // the right) and tool rows lose 1 (their single right-margin column).
    // These must stay in lockstep with add_margin_lines / the tool-result
    // row loop in markdown_render.rs so rows fill exactly the viewport width.
    let content_width = area.width.saturating_sub(9);
    let tool_content_width = area.width.saturating_sub(1);

    app.ensure_cache_synced();

    let mut rows_remaining = area.height as usize;
    let mut y = area.y + area.height;
    let mut rows_to_skip = app.effective_scroll();

    // Clone visible turn IDs upfront to avoid borrow conflicts when
    // accessing display state via app.display_for inside the loop.
    let Some(session_id) = app.active_session_id else {
        return;
    };
    let visible_turn_ids: Vec<u32> = app.display_for(session_id).visible_turn_ids.clone();
    let len = visible_turn_ids.len();

    // Build the whole-selection table highlight ONCE per frame, before the
    // visible-turn loop: it scans the selection's content lines (viewport-
    // independent), so recomputing it inside the loop would cost
    // O(visible_turns × selection_span) for nothing.  Empty when no selection
    // is active.
    let table_highlight = selection::table_highlight(app);

    // Iterate visible turns from newest to oldest.  clipped_area consumes
    // rows_to_skip from the bottom (newest end) so that turns fully below
    // the viewport are skipped before any content is rendered.
    for raw_i in 0..len {
        let i = len - 1 - raw_i;
        // `i < len == visible_turn_ids.len()`, so the lookup is always in
        // bounds; `.get()` keeps it total.
        let Some(turn_id) = visible_turn_ids.get(i).copied() else {
            continue;
        };

        if rows_remaining == 0 {
            break;
        }

        // Get cached lines (Arc clone is O(1)), the pre-computed height,
        // cumulative visual-row offsets for O(log n) row→line lookups, the
        // per-line content column ranges (for selection clamping), and the
        // per-line chrome intervals the highlight subtracts from them.
        let (text_lines_arc, text_height, text_offsets, content_ranges, chrome_ranges, img_slots) = {
            let display = app.display_for(session_id);
            let Some(turn) = display.view.turns.get(&turn_id) else {
                continue;
            };
            if turn.undone {
                continue;
            }
            let img_slots = turn_image_slots(turn);
            // Effective reasoning visibility: explicit override (header click)
            // wins, else the streaming-derived default.  The default is read
            // from the precomputed turn layout — rebuilt in lockstep with
            // `visible_turn_ids` before every render — keeping this per-frame
            // path free of string scanning; the trim-based derivation is only
            // a defensive fallback for a missing layout.
            let reasoning_expanded = {
                let default = display.turn_layouts.get(i).map_or_else(
                    || reasoning_expanded_default(turn),
                    |l| l.reasoning_default_expanded,
                );
                display.effective_reasoning_expanded(turn_id, default)
            };
            // Effective per-result collapse state (aligned with
            // `turn.tool_results`), part of the render-cache key like
            // `reasoning_expanded`.  Built only for turns that actually
            // have tool results — the common case allocates nothing and the
            // key comparison short-circuits on the empty slice.
            let tool_results_collapsed: Vec<bool> = if turn.tool_results.is_empty() {
                Vec::new()
            } else {
                turn.tool_results
                    .iter()
                    .map(|r| display.effective_tool_result_collapsed(turn_id, r))
                    .collect()
            };
            let key = RenderCacheKey {
                turn_id,
                width: content_width,
                viewport_width: area.width,
                reasoning_expanded,
                tool_results_collapsed,
                content_version: display.turn_content_version(turn_id),
            };
            let rendered = cached_or_compute_lines(&mut display.render_cache, i, &key, || {
                render_turn_lines(
                    turn,
                    content_width,
                    tool_content_width,
                    key.reasoning_expanded,
                    &key.tool_results_collapsed,
                )
            });
            (
                rendered.lines,
                rendered.height,
                rendered.visual_offsets,
                rendered.content_ranges,
                rendered.chrome_ranges,
                img_slots,
            )
        };

        // ── Images (rendered first so they sit below text) ──
        let full_img_height = app.image_block_height() as usize;
        for slot in img_slots.iter().rev() {
            if let Some((_top_line, visible_height)) = clipped_area(
                full_img_height,
                &mut rows_to_skip,
                &mut rows_remaining,
                &mut y,
            ) {
                let fully_visible = visible_height >= full_img_height;
                // `visible_height` is clamped to the viewport height (a `u16`
                // area dimension), so the cast never truncates.
                #[expect(clippy::cast_possible_truncation)]
                let img_rect = Rect {
                    x: area.x,
                    y,
                    width: area.width,
                    height: visible_height as u16,
                };
                render_turn_image(
                    frame,
                    img_rect,
                    session_id,
                    turn_id,
                    slot.clone(),
                    app,
                    fully_visible,
                );
            }
        }

        // ── Text content (render above images) ──
        if let Some((top_line, visible_height)) =
            clipped_area(text_height, &mut rows_to_skip, &mut rows_remaining, &mut y)
        {
            // Clone only the visible slice from the Arc — O(visible_lines)
            // instead of O(total_lines_in_turn).
            // Binary-search the precomputed cumulative offsets to find which
            // semantic lines the visible visual rows span — O(log n).
            let row_start = top_line;
            let row_end = top_line + visible_height;
            let line_start = text_offsets.partition_point(|&o| o <= row_start);
            let line_end = text_offsets.partition_point(|&o| o <= row_end);
            // `partition_point` returns offsets within `0..=text_lines.len()`,
            // so the range is always a valid slice of `text_lines_arc`.
            let mut visible_lines = text_lines_arc
                .get(line_start..line_end)
                .unwrap_or(&[])
                .to_vec();
            // Apply the in-progress text-selection highlight to the visible
            // slice at draw time — the render cache stays pure, and the same
            // cached lines drive both the highlight and the copy, so what is
            // highlighted is exactly what gets copied.  `i` is the
            // visible-turn index; the turn's first content row is
            // `height_prefix[i-1]` (0 for the first turn).
            let turn_start = i
                .checked_sub(1)
                .and_then(|prev| {
                    app.active_display_ref()
                        .and_then(|d| d.height_prefix.get(prev))
                        .copied()
                })
                .unwrap_or(0);
            selection::apply_selection_to_lines(
                app,
                &table_highlight,
                &selection::TurnSlice {
                    turn_start,
                    text_offsets: &text_offsets[..],
                    content_ranges: &content_ranges[..],
                    chrome_ranges: &chrome_ranges[..],
                },
                line_start,
                &mut visible_lines,
            );
            render_text_block(
                frame,
                area,
                visible_lines,
                visible_height,
                &mut y,
                Style::default(),
            );
        }
    }
}

/// Reserve `full_height` rows of the pane for one block and return the part of
/// it that is actually on screen, advancing the bottom-up draw cursor.
///
/// `rows_to_skip` is the scroll offset still to be consumed, `rows_remaining`
/// the viewport rows still unfilled, and `y` the next row to draw into. The
/// block is skipped entirely (as `None`) when the offset swallows it, and the
/// returned `(top_line, visible_height)` names the slice to draw — counted from
/// the top of the block, since the block is clipped at its *top* when the
/// viewport cuts it.
pub(super) fn clipped_area(
    full_height: usize,
    rows_to_skip: &mut usize,
    rows_remaining: &mut usize,
    y: &mut u16,
) -> Option<(usize, usize)> {
    if *rows_to_skip >= full_height {
        *rows_to_skip -= full_height;
        return None;
    }

    let visible_height = (full_height.saturating_sub(*rows_to_skip)).min(*rows_remaining);
    if visible_height == 0 {
        return None;
    }

    let bottom_line = full_height.saturating_sub(*rows_to_skip);
    let top_line = bottom_line.saturating_sub(visible_height);

    // `visible_height` is clamped to `rows_remaining`, which starts at the
    // viewport height (a `u16`), so the cast never truncates.
    #[expect(clippy::cast_possible_truncation)]
    let new_y = (*y).saturating_sub(visible_height as u16);
    *y = new_y;
    *rows_remaining -= visible_height;
    *rows_to_skip = 0;

    Some((top_line, visible_height))
}

/// Render a text block into the given area with wrapping.
///
/// `lines` must already be clipped to the visible portion (no `scroll` offset
/// is applied since the slice starts at the correct position).
fn render_text_block(
    frame: &mut Frame<'_>,
    area: Rect,
    lines: Vec<Line<'static>>,
    visible_height: usize,
    y: &mut u16,
    paragraph_style: Style,
) {
    // `visible_height` comes from `clipped_area`, which clamps it to the
    // viewport height (a `u16`), so the cast never truncates.
    #[expect(clippy::cast_possible_truncation)]
    let height = visible_height as u16;
    let rect = Rect {
        x: area.x,
        y: *y,
        width: area.width,
        height,
    };

    frame.render_widget(
        Paragraph::new(lines).style(paragraph_style).scroll((0, 0)),
        rect,
    );
}
