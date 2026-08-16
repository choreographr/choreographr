//! Mouse text selection over the chat history pane.
//!
//! Selecting text with the mouse in a raw-mode TUI is an *app-level* feature:
//! once `EnableMouseCapture` is on, the terminal forwards drag events to the
//! app instead of selecting natively, so the app must (a) track the drag,
//! (b) map the screen rectangle back to the text it covers, and (c) hand the
//! text to the clipboard itself.  This mirrors opencode's select-to-copy.
//!
//! Scope (v1): the chat *history pane* only — the input box and overlay
//! popups are out.  The selection is stored in *viewport* coordinates (row in
//! `[0, viewport height)`, column in `[0, viewport width)`), the same space
//! mouse events arrive in, and resolved to content at extraction/render time
//! through the existing `find_turn_at_row` + render-cache machinery — the
//! exact inverse of the click hit-testing the TUI already does.  Resolving
//! fresh at gesture end also means streaming that lands mid-drag cannot
//! corrupt the result: each row maps to whatever content is current then.
//!
//! Coordinates: the history pane's lines are pre-wrapped at `content_width`
//! (viewport width − 9) and drawn in a non-wrapping `Paragraph`, so every
//! semantic line occupies exactly one visual row; the code still walks the
//! cached `visual_offsets` so a hypothetical multi-row line maps correctly.

use crate::state::{
    App, RenderedTurn, SessionDisplayState, find_turn_at_row, grapheme_offset_at_column,
};
use ratatui::style::Modifier;
use ratatui::text::{Line, Span};
use unicode_width::UnicodeWidthStr;

/// An in-progress mouse text selection over the history pane.
///
/// Both endpoints are viewport-relative screen coordinates (row ∈ `[0,
/// viewport height)`, column ∈ `[0, viewport width)`).  `active` flips to
/// true once the drag has actually moved — that is what distinguishes a
/// selection gesture from a plain click (a click still performs its existing
/// toggle/cursor actions; only a real drag copies text on release).
#[derive(Debug, Clone, Copy)]
pub(crate) struct TextSelection {
    /// Mouse-down position.
    pub anchor: (u16, u16),
    /// Live drag head; updated on every drag event.
    pub head: (u16, u16),
    /// True once `head != anchor` (a real selection, not a click).
    pub active: bool,
}

/// Arm a potential selection at the mouse-down position.  A selection only
/// becomes real (highlighted, copied) once the drag moves.
pub(crate) fn start_selection(app: &mut App, row: u16, column: u16) {
    app.text_selection = Some(TextSelection {
        anchor: (row, column),
        head: (row, column),
        active: false,
    });
}

/// Whether a selection gesture is in progress (between mouse-down and
/// mouse-up in the history pane).
pub(crate) fn is_selecting(app: &App) -> bool {
    app.text_selection.is_some()
}

/// Extend the selection to the current drag position.
pub(crate) fn update_selection(app: &mut App, row: u16, column: u16) {
    if let Some(sel) = &mut app.text_selection {
        sel.head = (row, column);
        if sel.head != sel.anchor {
            sel.active = true;
        }
    }
}

/// Abandon the in-progress selection (scroll, right-click, page switch…).
pub(crate) fn cancel_selection(app: &mut App) {
    app.text_selection = None;
}

/// The normalized (start, end) endpoints of the selection, or `None` when
/// there is no active selection (including a plain click that never dragged).
pub(crate) fn selection_range(app: &App) -> Option<((u16, u16), (u16, u16))> {
    let sel = app.text_selection?;
    if !sel.active {
        return None;
    }
    Some(normalize(sel.anchor, sel.head))
}

/// Order two endpoints so `start <= end` in (row, column) space, making a
/// bottom-to-top drag (or a right-to-left drag on the end row) select the
/// same rectangle as the equivalent top-to-bottom one.
fn normalize(a: (u16, u16), b: (u16, u16)) -> ((u16, u16), (u16, u16)) {
    if (b.0, b.1) < (a.0, a.1) {
        (b, a)
    } else {
        (a, b)
    }
}

/// Finish the selection at the release point: return the selected text (if
/// the gesture was a real drag over copyable rows) and always clear the
/// selection state.  Returns `None` for a plain click.
pub(crate) fn finish_selection(app: &mut App, row: u16, column: u16) -> Option<String> {
    // The release event's position is authoritative for the final head.
    update_selection(app, row, column);
    let text = if app.text_selection.is_some_and(|s| s.active) {
        extract_selection_text(app)
    } else {
        None
    };
    app.text_selection = None;
    text
}

/// Extract the plain text covered by the active selection rectangle.
///
/// Rows are resolved one at a time through `find_turn_at_row` + the render
/// cache.  Rows that do not resolve — the blank band above short content,
/// drags past the bottom edge of the pane, image blocks — are skipped, so
/// the copyable region is exactly the region the highlight covers.
fn extract_selection_text(app: &App) -> Option<String> {
    let ((start_row, start_col), (end_row, end_col)) = selection_range(app)?;
    let vp_width = app.history_viewport.width as usize;
    let mut out = String::new();
    for row in start_row..=end_row {
        let col_lo = if row == start_row {
            start_col as usize
        } else {
            0
        };
        let col_hi = if row == end_row {
            end_col as usize
        } else {
            usize::MAX
        };
        if let Some(text) = text_for_row(app, row, col_lo, col_hi, vp_width) {
            if !out.is_empty() {
                out.push('\n');
            }
            out.push_str(&text);
        }
    }
    if out.is_empty() { None } else { Some(out) }
}

/// Resolve one viewport row of the selection to the text slice it covers.
fn text_for_row(
    app: &App,
    row: u16,
    col_start: usize,
    col_end: usize,
    vp_width: usize,
) -> Option<String> {
    let (turn_idx, visual_row) = find_turn_at_row(app, row)?;
    let display = app.active_display_ref()?;
    let rendered = cached_rendered_turn(display, turn_idx, vp_width)?;
    // Map the turn-local visual row to a semantic line, then to the visual
    // row *within* that line (0 for every line in practice: lines are
    // pre-wrapped narrower than the viewport).
    let line_idx = rendered
        .visual_offsets
        .partition_point(|&o| o <= visual_row);
    if line_idx >= rendered.lines.len() {
        return None;
    }
    let line_start_row = line_idx
        .checked_sub(1)
        .and_then(|i| rendered.visual_offsets.get(i))
        .copied()
        .unwrap_or(0);
    let within_line = visual_row.saturating_sub(line_start_row);
    // Translate viewport columns into the semantic line's own column space:
    // visual row `within_line` of a line shows columns
    // `[within_line * vp_width, (within_line + 1) * vp_width)`.
    let base = within_line.saturating_mul(vp_width);
    let line_col_lo = base.saturating_add(col_start);
    let line_col_hi = if col_end == usize::MAX {
        usize::MAX
    } else {
        base.saturating_add(col_end)
    };
    Some(slice_line_columns(
        &rendered.lines[line_idx],
        line_col_lo,
        line_col_hi,
    ))
}

/// Read-only render-cache lookup for a turn's rendered lines.
///
/// Mirrors the key the renderer computes (see `render_history`): only an
/// entry with matching turn id and widths is reused, so a stale entry (e.g.
/// from before a resize) is treated as a miss and the row is skipped rather
/// than extracting text from the wrong wrapping.
fn cached_rendered_turn(
    display: &SessionDisplayState,
    turn_idx: usize,
    vp_width: usize,
) -> Option<&RenderedTurn> {
    let cached = display.render_cache.get(turn_idx)?.as_ref()?;
    let turn_id = display.visible_turn_ids.get(turn_idx).copied()?;
    if cached.key.turn_id != turn_id
        || cached.key.width as usize != vp_width.saturating_sub(9)
        || cached.key.viewport_width as usize != vp_width
    {
        return None;
    }
    Some(&cached.rendered)
}

/// Rough token estimate for the copied-selection status line.
///
/// Not a tokenizer: it reuses the same UTF-8 bytes ÷ 4 heuristic the daemon
/// documents for non-decodable reasoning artifacts (`choreo-daemon`'s
/// `estimate_prompt_tokens`), which lands near the ~4 chars/token density of
/// English prose.  Good enough for a "how much context did I just grab"
/// status; exact counts would need tiktoken in the TUI.
pub(crate) fn approx_tokens(text: &str) -> usize {
    text.len().div_ceil(4)
}

/// Apply the selection highlight to the visible slice of one turn's lines.
///
/// Called from `render_history` for the visible semantic-line slice of each
/// turn.  For every line occupying any selected screen row, the covered
/// column range (translated into the line's own column space) is restyled
/// with `REVERSED` — the terminal-native selection look.  The render cache
/// is never mutated: lines are restyled at draw time only.
pub(crate) fn apply_selection_to_lines(
    app: &App,
    turn_start: usize,
    text_offsets: &[usize],
    line_start: usize,
    lines: &mut [Line<'static>],
) {
    let Some((start, end)) = selection_range(app) else {
        return;
    };
    let vp = app.history_viewport;
    let vh = vp.height as usize;
    if vh == 0 {
        return;
    }
    let Some(display) = app.active_display_ref() else {
        return;
    };
    let total = display.total_history_height();
    let scroll = display.effective_scroll(&vp);
    // The renderer draws the content bottom-anchored inside the viewport:
    // content row `c` maps to screen row `c + scroll + vh - total` (see
    // `find_turn_at_row` for the inverse).  For visible rows this is never
    // negative, so the saturating math below is exact.
    let row_base = scroll.saturating_add(vh).saturating_sub(total);
    for (k, line) in lines.iter_mut().enumerate() {
        let li = line_start + k;
        let row_lo = li
            .checked_sub(1)
            .and_then(|i| text_offsets.get(i))
            .copied()
            .unwrap_or(0);
        let Some(&row_hi) = text_offsets.get(li) else {
            continue;
        };
        // Every line occupies exactly one visual row in practice (pre-wrapped
        // at content_width < viewport width); the inner loop generalizes to
        // multi-row lines defensively.
        for vr in row_lo..row_hi {
            let screen_row = turn_start.saturating_add(vr).saturating_add(row_base);
            let start_row = start.0 as usize;
            let end_row = end.0 as usize;
            if screen_row < start_row || screen_row > end_row {
                continue;
            }
            let col_lo = if screen_row == start_row {
                start.1 as usize
            } else {
                0
            };
            let col_hi = if screen_row == end_row {
                end.1 as usize
            } else {
                usize::MAX
            };
            let base = vr.saturating_mul(vp.width as usize);
            let line_col_lo = base.saturating_add(col_lo);
            let line_col_hi = if col_hi == usize::MAX {
                usize::MAX
            } else {
                base.saturating_add(col_hi)
            };
            *line = style_line_selection(line, line_col_lo, line_col_hi);
            // A real (single-visual-row) line is fully covered by its one row.
            if row_hi - row_lo <= 1 {
                break;
            }
        }
    }
}

/// Restyle the display-column range `[col_lo, col_hi)` of a line with the
/// selection highlight, splitting spans at grapheme boundaries so a selection
/// can never split a ZWJ emoji or combining sequence.  `col_hi` of
/// `usize::MAX` means "to the end of the line".
pub(crate) fn style_line_selection(
    line: &Line<'static>,
    col_lo: usize,
    col_hi: usize,
) -> Line<'static> {
    if col_lo >= col_hi {
        return line.clone();
    }
    let mut out: Vec<Span<'static>> = Vec::with_capacity(line.spans.len() + 2);
    let mut col = 0usize;
    for span in &line.spans {
        let span_text = span.content.as_ref();
        let span_w = UnicodeWidthStr::width(span_text);
        if span_w == 0 {
            // Zero-width span (e.g. a control-char placeholder) — no cells to
            // highlight, keep it untouched.
            out.push(span.clone());
            continue;
        }
        let span_lo = col;
        let span_hi = col.saturating_add(span_w);
        col = span_hi;
        if span_hi <= col_lo || span_lo >= col_hi {
            // Entirely before or after the selection — keep as-is.
            out.push(span.clone());
            continue;
        }
        // Overlap: split this span at the selection boundaries, snapping both
        // cuts to grapheme boundaries.
        let before_w = col_lo.saturating_sub(span_lo);
        let sel_hi_col = col_hi.saturating_sub(span_lo).min(span_w);
        let mut sel_lo = grapheme_offset_at_column(span_text, before_w);
        let mut sel_hi = grapheme_offset_at_column(span_text, sel_hi_col);
        // Defensive monotonicity (columns are ordered; the snap can't invert
        // them, but a malformed range must never panic on the slice below).
        if sel_lo > sel_hi {
            std::mem::swap(&mut sel_lo, &mut sel_hi);
        }
        let before = &span_text[..sel_lo];
        let selected = &span_text[sel_lo..sel_hi];
        let after = &span_text[sel_hi..];
        if !before.is_empty() {
            out.push(Span::styled(before.to_owned(), span.style));
        }
        out.push(Span::styled(
            selected.to_owned(),
            span.style.add_modifier(Modifier::REVERSED),
        ));
        if !after.is_empty() {
            out.push(Span::styled(after.to_owned(), span.style));
        }
    }
    Line::from(out)
}

/// Concatenate a rendered line's spans into its plain text.
fn line_text(line: &Line<'_>) -> String {
    let mut text = String::new();
    for span in &line.spans {
        text.push_str(span.content.as_ref());
    }
    text
}

/// Slice a rendered line's text by display columns, snapping to grapheme
/// boundaries (a selection can never split a ZWJ emoji or combining mark).
fn slice_line_columns(line: &Line<'_>, col_lo: usize, col_hi: usize) -> String {
    let text = line_text(line);
    let width = UnicodeWidthStr::width(text.as_str());
    let lo = grapheme_offset_at_column(&text, col_lo.min(width));
    let hi = grapheme_offset_at_column(&text, col_hi.min(width));
    text[lo.min(hi)..lo.max(hi)].to_string()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::test_util::test_app;
    use choreo_proto::Turn;

    /// Build a turn with a single assistant text line (and no user text) so
    /// the rendered layout is exactly one line per turn — the easiest canvas
    /// for asserting exact extracted text.  `turns` is a `BTreeMap` keyed by
    /// turn id, so iteration order (and thus render order) is deterministic.
    fn turn(assistant: &str) -> Turn {
        Turn {
            created_at: choreo_proto::TimestampMs::now(),
            undone: false,
            error: None,
            user_text: None,
            assistant_text: Some(assistant.into()),
            assistant_reasoning: None,
            tool_calls: vec![],
            token_usage: None,
            tool_results: vec![],
            displayed_images: vec![],
            reasoning_artifact: None,
            reasoning_producer: None,
        }
    }

    /// Build an app with `turns` rendered, viewport wide enough that the
    /// whole history is visible (so no row is scrolled out), with the render
    /// cache populated.  `vh` must be >= total height (tests pass a large
    /// value); the selection coordinates below are derived from the actual
    /// layout via [`locate`], never assumed.
    fn app_with_turns(turns: &[(u32, &str)], vh: u16) -> App {
        let mut app = test_app();
        app.history_viewport.width = 80;
        app.history_viewport.height = vh;
        for (id, text) in turns {
            app.display_for(0).view.insert_or_replace(*id, turn(text));
        }
        app.rebuild_height_prefix();
        app
    }

    fn drag_and_finish(app: &mut App, from: (u16, u16), to: (u16, u16)) -> Option<String> {
        start_selection(app, from.0, from.1);
        update_selection(app, to.0, to.1);
        finish_selection(app, to.0, to.1)
    }

    /// Locate `needle` in the rendered history and return its exact selection
    /// rectangle in viewport coordinates, derived from the actual render
    /// cache + `height_prefix`.  Assistant turns render inside a boxed,
    /// shaded block (a `┃` gutter + separator rows), so the tests must not
    /// hardcode row/column offsets — this helper turns the layout upside
    /// down: it finds where the text *actually* is and feeds that back in,
    /// verifying the row→turn→line→column mapping end to end.
    fn locate(app: &App, needle: &str) -> ((u16, u16), (u16, u16)) {
        let display = app.active_display_ref().expect("active display");
        let vp = app.history_viewport;
        let total = display.total_history_height();
        let scroll = display.effective_scroll(&vp);
        // Content row c maps to screen row c + scroll + vh - total (bottom
        // anchored; see find_turn_at_row).
        let row_base = scroll + vp.height as usize - total;
        for (turn_idx, _turn_id) in display.visible_turn_ids.iter().enumerate() {
            let cached = display.render_cache[turn_idx]
                .as_ref()
                .expect("render cache populated by rebuild_height_prefix");
            let turn_start = display
                .height_prefix
                .get(turn_idx.wrapping_sub(1))
                .copied()
                .unwrap_or(0);
            for (line_idx, line) in cached.rendered.lines.iter().enumerate() {
                let text = line_text(line);
                if let Some(char_off) = text.find(needle) {
                    let col_start = UnicodeWidthStr::width(&text[..char_off]);
                    let col_end = col_start + UnicodeWidthStr::width(needle);
                    let row_lo = cached
                        .rendered
                        .visual_offsets
                        .get(line_idx.wrapping_sub(1))
                        .copied()
                        .unwrap_or(0);
                    // Every rendered line occupies one visual row in practice.
                    let screen_row = (turn_start + row_lo + row_base) as u16;
                    return ((screen_row, col_start as u16), (screen_row, col_end as u16));
                }
            }
        }
        panic!("needle {needle:?} not found in rendered history");
    }

    // ── state machine ──

    #[test]
    fn click_without_drag_is_not_a_selection() {
        let mut app = app_with_turns(&[(0, "hello"), (1, "world")], 30);
        let ((r, c), _) = locate(&app, "hello");
        start_selection(&mut app, r, c);
        update_selection(&mut app, r, c); // no movement
        assert!(!app.text_selection.unwrap().active);
        assert_eq!(finish_selection(&mut app, r, c), None);
        assert!(
            app.text_selection.is_none(),
            "selection cleared after finish"
        );
    }

    #[test]
    fn drag_activates_and_finish_clears() {
        let mut app = app_with_turns(&[(0, "hello"), (1, "world")], 30);
        let (start, _) = locate(&app, "hello");
        let (_, end) = locate(&app, "world");
        start_selection(&mut app, start.0, start.1);
        update_selection(&mut app, end.0, end.1);
        assert!(app.text_selection.unwrap().active);
        assert!(finish_selection(&mut app, end.0, end.1).is_some());
        assert!(app.text_selection.is_none());
    }

    // ── extraction ──

    #[test]
    fn extract_single_row_mid_line() {
        let mut app = app_with_turns(&[(0, "hello world")], 30);
        let (start, end) = locate(&app, "hello");
        start_selection(&mut app, start.0, start.1);
        let text = finish_selection(&mut app, end.0, end.1).expect("selection should extract");
        assert_eq!(text, "hello");
    }

    #[test]
    fn extract_multi_row_joins_turns() {
        // Selecting across the boxed turn blocks copies the visible gutter
        // rows in between (terminal-native semantics: what you see is what
        // you copy), so assert both words appear in render order rather than
        // asserting an exact string.
        let mut app = app_with_turns(&[(0, "first"), (1, "second")], 30);
        let (start, _) = locate(&app, "first");
        let (_, end) = locate(&app, "second");
        let text = drag_and_finish(&mut app, start, end).expect("selection should extract");
        let first_pos = text.find("first").expect("first word copied");
        let second_pos = text.find("second").expect("second word copied");
        assert!(first_pos < second_pos, "turns copied in render order");
        assert!(text.contains('\n'), "rows are joined with newlines");
    }

    #[test]
    fn extract_mid_line_start_and_end() {
        let mut app = app_with_turns(&[(0, "abcdefghij")], 30);
        let (start, end) = locate(&app, "cdef");
        start_selection(&mut app, start.0, start.1);
        let text = finish_selection(&mut app, end.0, end.1).expect("selection should extract");
        assert_eq!(text, "cdef");
    }

    #[test]
    fn extract_reverse_drag_normalizes() {
        let mut app = app_with_turns(&[(0, "first"), (1, "second")], 30);
        let (start, _) = locate(&app, "first");
        let (_, end) = locate(&app, "second");
        // Bottom-to-top drag must select the same rectangle.
        let text = drag_and_finish(&mut app, end, start).expect("selection should extract");
        assert!(text.contains("first") && text.contains("second"));
    }

    #[test]
    fn extract_wide_chars_use_display_columns() {
        // 日本語 is 6 display columns wide; selecting the rectangle locate()
        // reports for 日本 must copy exactly those two CJK chars — if the
        // column math counted chars instead of display columns, the slice
        // would land inside the wide characters.
        let mut app = app_with_turns(&[(0, "日本語")], 30);
        let (start, end) = locate(&app, "日本");
        start_selection(&mut app, start.0, start.1);
        let text = finish_selection(&mut app, end.0, end.1).expect("selection should extract");
        assert_eq!(text, "日本");
    }

    #[test]
    fn extract_rows_outside_content_are_skipped() {
        // Viewport taller than content: rows above the content (the blank
        // band) resolve to no turn and must be skipped, not cancel the copy.
        let mut app = app_with_turns(&[(0, "hello")], 30);
        let (_, end) = locate(&app, "hello");
        let text = drag_and_finish(&mut app, (0, 0), end).expect("selection should extract");
        assert!(text.contains("hello"), "blank band skipped, content copied");
    }

    #[test]
    fn extract_via_render_cache_matches_highlight_source() {
        // Extraction reads the same cached lines the highlight styles, so the
        // copied text is exactly what the user sees highlighted.
        let mut app = app_with_turns(&[(0, "styled text")], 30);
        let (start, end) = locate(&app, "text");
        start_selection(&mut app, start.0, start.1);
        let text = finish_selection(&mut app, end.0, end.1).expect("selection should extract");
        assert_eq!(text, "text");
    }

    // ── style_line_selection ──

    #[test]
    fn style_selection_splits_single_span() {
        let line = Line::from("hello world");
        let styled = style_line_selection(&line, 6, 11);
        // ["hello ", "world"(REVERSED)] — the selected slice gets REVERSED.
        assert_eq!(styled.spans.len(), 2);
        assert_eq!(styled.spans[0].content, "hello ");
        assert_eq!(styled.spans[1].content, "world");
        assert!(
            styled.spans[1]
                .style
                .add_modifier
                .contains(Modifier::REVERSED)
        );
    }

    #[test]
    fn style_selection_middle_of_span() {
        let line = Line::from("abcdefgh");
        let styled = style_line_selection(&line, 2, 5);
        assert_eq!(styled.spans.len(), 3);
        assert_eq!(styled.spans[0].content, "ab");
        assert_eq!(styled.spans[1].content, "cde");
        assert!(
            styled.spans[1]
                .style
                .add_modifier
                .contains(Modifier::REVERSED)
        );
        assert_eq!(styled.spans[2].content, "fgh");
    }

    #[test]
    fn style_selection_full_line_keeps_spans() {
        let line = Line::from("hello");
        let styled = style_line_selection(&line, 0, usize::MAX);
        assert_eq!(styled.spans.len(), 1);
        assert!(
            styled.spans[0]
                .style
                .add_modifier
                .contains(Modifier::REVERSED)
        );
    }

    #[test]
    fn style_selection_no_overlap_returns_unchanged() {
        let line = Line::from("hello");
        let styled = style_line_selection(&line, 5, 10);
        assert_eq!(styled, line);
    }

    #[test]
    fn style_selection_does_not_split_zwj_emoji() {
        // The selection boundary at column 1 falls inside the first emoji
        // (2 columns wide); the cut must snap to a grapheme boundary, so the
        // emoji stays whole in one span and the selected slice starts after
        // it.  The property under test is *no grapheme is split*, not where
        // the boundary lands.
        let line = Line::from("😀abc");
        let styled = style_line_selection(&line, 1, 3);
        // Emoji (cols 0..2) stays whole and unselected; 'a' (col 2) is the
        // selected cell; 'bc' follows unselected.
        let texts: Vec<String> = styled.spans.iter().map(|s| s.content.to_string()).collect();
        assert_eq!(
            texts,
            vec!["😀".to_string(), "a".to_string(), "bc".to_string()]
        );
        assert!(
            styled.spans[1]
                .style
                .add_modifier
                .contains(Modifier::REVERSED)
        );
        assert!(
            !styled.spans[0]
                .style
                .add_modifier
                .contains(Modifier::REVERSED)
        );
    }

    #[test]
    fn style_selection_preserves_span_style() {
        let styled = Line::from(Span::styled(
            "abcdef",
            ratatui::style::Style::default().fg(ratatui::style::Color::Cyan),
        ));
        let styled = style_line_selection(&styled, 1, 3);
        assert_eq!(styled.spans.len(), 3);
        // The unselected pieces keep Cyan; the selected piece is Cyan + REVERSED.
        assert_eq!(styled.spans[0].style.fg, Some(ratatui::style::Color::Cyan));
        assert_eq!(styled.spans[1].style.fg, Some(ratatui::style::Color::Cyan));
        assert!(
            styled.spans[1]
                .style
                .add_modifier
                .contains(Modifier::REVERSED)
        );
        assert_eq!(styled.spans[2].style.fg, Some(ratatui::style::Color::Cyan));
    }

    // ── approx_tokens ──

    #[test]
    fn approx_tokens_empty_is_zero() {
        assert_eq!(approx_tokens(""), 0);
    }

    #[test]
    fn approx_tokens_basic() {
        // 20 bytes / 4 = 5 tokens.
        assert_eq!(approx_tokens("hello world hello!"), 5);
    }

    #[test]
    fn approx_tokens_rounds_up() {
        // 5 bytes → ceil(5/4) = 2 tokens.
        assert_eq!(approx_tokens("hello"), 2);
    }

    // ── slice_line_columns ──

    #[test]
    fn slice_columns_basic() {
        let line = Line::from("hello world");
        assert_eq!(slice_line_columns(&line, 0, 5), "hello");
        assert_eq!(slice_line_columns(&line, 6, usize::MAX), "world");
    }

    #[test]
    fn slice_columns_clamps_past_end() {
        let line = Line::from("hi");
        assert_eq!(slice_line_columns(&line, 0, usize::MAX), "hi");
        assert_eq!(slice_line_columns(&line, 5, 10), "");
    }

    #[test]
    fn slice_columns_wide_chars() {
        let line = Line::from("日本語x");
        // Columns 0..3 → the first two CJK chars.
        assert_eq!(slice_line_columns(&line, 0, 3), "日本");
    }
}
