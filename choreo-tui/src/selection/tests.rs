use super::*;
use crate::markdown_render::LineChrome;
use crate::state::{App, find_turn_at_row};
use crate::test_util::test_app;
use choreo_proto::{ToolResultRecord, Turn};
use ratatui::text::{Line, Span};
use unicode_width::UnicodeWidthStr;

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
    finish_selection(app)
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
                // `char_off` is a `find` byte offset, hence a char boundary.
                let col_start = UnicodeWidthStr::width(text.get(..char_off).unwrap_or(""));
                let col_end = col_start + UnicodeWidthStr::width(needle);
                let row_lo = cached
                    .rendered
                    .visual_offsets
                    .get(line_idx.wrapping_sub(1))
                    .copied()
                    .unwrap_or(0);
                // Every rendered line occupies one visual row in practice.
                let screen_row = content_to_screen_row(app, turn_start + row_lo)
                    .expect("needle must be on screen");
                // Display columns of a rendered row always fit u16 (the
                // viewport is u16-bounded); the cast is exact in practice.
                #[expect(clippy::cast_possible_truncation)] // viewport-bounded columns
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
    assert_eq!(finish_selection(&mut app), None);
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
    assert!(finish_selection(&mut app).is_some());
    assert!(app.text_selection.is_none());
}

// ── extraction ──

#[test]
fn extract_single_row_mid_line() {
    let mut app = app_with_turns(&[(0, "hello world")], 30);
    let (start, end) = locate(&app, "hello");
    start_selection(&mut app, start.0, start.1);
    update_selection(&mut app, end.0, end.1);
    let text = finish_selection(&mut app).expect("selection should extract");
    assert_eq!(text, "hello");
}

#[test]
fn extract_skips_box_chrome_across_turns() {
    // Selecting across boxed turn blocks must copy ONLY the content text:
    // the `┃` gutter, leading box padding, and trailing fill of every
    // selected row are excluded, and pure-chrome rows (separators,
    // padding) contribute nothing.  Regression for the box chrome being
    // dragged into the copied text.  (Also covers the multi-row case:
    // both turns' words arrive in render order joined by newlines.)
    let mut app = app_with_turns(&[(0, "first"), (1, "second")], 30);
    let (start, _) = locate(&app, "first");
    let (_, end) = locate(&app, "second");
    let text = drag_and_finish(&mut app, start, end).expect("selection should extract");
    assert!(
        !text.contains('┃'),
        "box gutter must not be copied: {text:?}"
    );
    let first_pos = text.find("first").expect("first word copied");
    let second_pos = text.find("second").expect("second word copied");
    assert!(first_pos < second_pos, "turns copied in render order");
    assert!(text.contains('\n'), "rows are joined with newlines");
    for line in text.lines() {
        assert_eq!(
            line.trim_end().len(),
            line.len(),
            "no trailing box fill spaces copied: {line:?}"
        );
    }
}

#[test]
fn extract_mid_line_start_and_end() {
    let mut app = app_with_turns(&[(0, "abcdefghij")], 30);
    let (start, end) = locate(&app, "cdef");
    start_selection(&mut app, start.0, start.1);
    update_selection(&mut app, end.0, end.1);
    let text = finish_selection(&mut app).expect("selection should extract");
    assert_eq!(text, "cdef");
}

#[test]
fn reverse_diagonal_drag_keeps_columns_anchored() {
    // Terminal-native anchor semantics: the anchor row extends from the
    // anchor column to end-of-line and the head row from start-of-line to
    // the head column — so a bottom-to-top drag that also moves
    // horizontally *mirrors* the columns instead of swapping them (the
    // old lexicographic normalize swapped them, extracting a mirror-image
    // rectangle: for this drag it would have yielded "one\nline ").
    let mut app = app_with_turns(&[(0, "line one"), (1, "line two")], 30);
    // Head at the end of "lin" on the top turn's content row; anchor at
    // the start of "wo" on the bottom turn's content row (a reverse
    // diagonal drag).
    let (_, head_end) = locate(&app, "lin");
    let (anchor_start, _) = locate(&app, "wo");
    let text = drag_and_finish(&mut app, anchor_start, head_end).expect("selection should extract");
    // Head row: [0, head_col) → "lin"; anchor row: [anchor_col, EOL) → "wo".
    assert_eq!(text, "lin\nwo");
}

#[test]
fn selection_bounds_follow_anchor_semantics() {
    // Anchor row: [anchor_col, EOL); head row: [0, head_col); middle
    // rows: full width — regardless of which endpoint is on top.
    let anchor = (5, 30);
    let head = (2, 4);
    assert_eq!(selection_bounds_for_line(anchor, head, 2), (0, 4));
    assert_eq!(selection_bounds_for_line(anchor, head, 3), (0, usize::MAX));
    assert_eq!(selection_bounds_for_line(anchor, head, 5), (30, usize::MAX));
    // Same-row drags are just the span between the two columns.
    assert_eq!(selection_bounds_for_line((3, 8), (3, 2), 3), (2, 8));
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
    update_selection(&mut app, end.0, end.1);
    let text = finish_selection(&mut app).expect("selection should extract");
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
    update_selection(&mut app, end.0, end.1);
    let text = finish_selection(&mut app).expect("selection should extract");
    assert_eq!(text, "text");
}

#[test]
fn anchor_stays_pinned_to_text_while_head_tracks_the_cursor() {
    // Scrolling mid-gesture: the anchor stays on the text it was placed
    // on (content coordinates), while the live drag head re-resolves to
    // the content now under the (stationary) cursor — so the selection
    // tracks the cursor as the viewport moves, and the highlight reflects
    // it on the scroll event itself rather than waiting for the next
    // drag.
    let mut app = test_app();
    app.history_viewport.width = 80;
    app.history_viewport.height = 10;
    for i in 0..20u32 {
        app.display_for(0)
            .view
            .insert_or_replace(i, turn(&format!("turn {i}")));
    }
    app.rebuild_height_prefix();
    assert!(
        app.total_history_height() > app.history_viewport.height as usize,
        "history must overflow the viewport"
    );

    // Drag rows 2..7 at scroll 0: anchor = content line 92, head = line 97.
    start_selection(&mut app, 2, 3);
    update_selection(&mut app, 7, 80);
    assert!(app.text_selection.unwrap().active);
    let ((a0, _), (h0, _)) = selection_range(&app).unwrap();
    assert_eq!((a0, h0), (92, 97));

    // Simulate the selection arm's scroll handling: apply the scroll,
    // then re-resolve the head at the cursor (the wheel event position).
    app.scroll_accumulator = 3;
    app.apply_scroll_delta();
    update_selection(&mut app, 7, 80);
    let ((a1, _), (h1, _)) = selection_range(&app).unwrap();
    assert_eq!(
        a1, 92,
        "the anchor stays pinned to the text it was placed on"
    );
    assert_eq!(h1, 94, "the head tracks the content now under the cursor");
    assert!(
        app.text_selection.is_some_and(|s| s.active),
        "scrolling must not cancel the selection"
    );
}

#[test]
fn release_uses_head_from_last_sync_without_rewinding() {
    // The release event never moves the head: after a mid-gesture scroll
    // the draw-time `follow_cursor` sync re-resolves it to the content
    // under the cursor, and releasing must preserve exactly that —
    // re-resolving the release *screen* position would point at different
    // content than the cursor sat on at the last draw (the viewport moved
    // under it).
    let mut app = test_app();
    app.history_viewport.width = 80;
    app.history_viewport.height = 10;
    for i in 0..20u32 {
        app.display_for(0)
            .view
            .insert_or_replace(i, turn(&format!("turn {i}")));
    }
    app.rebuild_height_prefix();

    // Drag rows 2..4 at scroll 0, then scroll up 3 and let the draw-time
    // sync re-resolve the head (a wheel mid-gesture does both of these).
    start_selection(&mut app, 2, 3);
    update_selection(&mut app, 4, 10);
    app.scroll_accumulator = 3;
    app.apply_scroll_delta();
    follow_cursor(&mut app);

    let expected = extract_selection_text(&app).expect("selection text after the sync");
    // Release: the copied text is whatever the last sync left — the
    // release position is never consulted, so it can neither extend nor
    // rewind the selection.
    let actual = finish_selection(&mut app).expect("selection text on release");
    assert_eq!(actual, expected, "release must not move the head");
}

#[test]
fn follow_cursor_reanchors_head_after_content_scrolls() {
    // New content appended at the bottom while a selection is active (the
    // auto-following bottom of a streaming session): the content under
    // the stationary cursor changes, so the draw-time sync must re-resolve
    // the head to it — without any mouse movement.  The anchor stays
    // pinned to the text it was placed on.
    let mut app = test_app();
    app.history_viewport.width = 80;
    app.history_viewport.height = 10;
    for i in 0..20u32 {
        app.display_for(0)
            .view
            .insert_or_replace(i, turn(&format!("turn {i}")));
    }
    app.rebuild_height_prefix();

    // Drag rows 2..5 at scroll 0 (bottom-anchored: content lines
    // total−vh+2 ..= total−vh+5).
    start_selection(&mut app, 2, 3);
    update_selection(&mut app, 5, 40);
    let ((anchor0, _), (head0, _)) = selection_range(&app).unwrap();
    assert_eq!((anchor0, head0), (92, 95));

    // A new turn streams in below, growing the history exactly as a
    // daemon message would (the viewport is bottom-anchored, so the old
    // content shifts up and the content under the stationary cursor moves
    // down).
    app.display_for(0)
        .view
        .insert_or_replace(20, turn("turn 20"));
    app.rebuild_height_prefix();
    follow_cursor(&mut app);

    let ((anchor1, _), (head1, _)) = selection_range(&app).unwrap();
    assert_eq!(anchor1, 92, "the anchor stays pinned to its text");
    assert_eq!(head1, 100, "the head tracks the content under the cursor");
    assert!(
        app.text_selection.is_some_and(|s| s.active),
        "content scroll must keep the selection active"
    );
}

#[test]
fn follow_cursor_does_not_activate_an_armed_click() {
    // A plain click (armed but never dragged) must not silently become a
    // selection when the content scrolls under it — only an explicit drag
    // activates the gesture.
    let mut app = app_with_turns(&[(0, "hello")], 30);
    let ((r, c), _) = locate(&app, "hello");
    start_selection(&mut app, r, c);
    follow_cursor(&mut app);
    assert!(
        !app.text_selection.unwrap().active,
        "a plain click + content scroll must not start a selection"
    );
    assert_eq!(finish_selection(&mut app), None, "nothing to copy");
}

#[test]
fn follow_cursor_is_a_noop_when_layout_is_unchanged() {
    // The draw-time sync is fingerprint-gated: when the screen→content
    // mapping inputs (total height, scroll, viewport height) have not
    // moved since the last head resolution, the head must stay exactly
    // where the last drag put it.  Here the remembered cursor position is
    // corrupted to a different spot WITHOUT touching the layout — an
    // un-gated follow_cursor would "helpfully" re-resolve the head to it,
    // but the gate must skip the re-resolution.  This also pins the
    // every-frame cost: an idle frame never re-derives the head.
    let mut app = test_app();
    app.history_viewport.width = 80;
    app.history_viewport.height = 10;
    for i in 0..20u32 {
        app.display_for(0)
            .view
            .insert_or_replace(i, turn(&format!("turn {i}")));
    }
    app.rebuild_height_prefix();

    start_selection(&mut app, 2, 3);
    update_selection(&mut app, 5, 40);
    let head_before = app.text_selection.unwrap().head;
    assert!(
        app.text_selection.is_some_and(|s| s.head_sync.is_some()),
        "every head resolution records its layout fingerprint"
    );

    // A stale cursor (simulating what the gate protects against: only a
    // real mouse event or a layout change may move the head).
    app.text_selection.as_mut().unwrap().cursor = (0, 1);
    follow_cursor(&mut app);
    assert_eq!(
        app.text_selection.unwrap().head,
        head_before,
        "unchanged layout must skip the head re-resolution"
    );
}

// ── style_line_selection ──

#[test]
fn style_selection_splits_single_span() {
    let line = Line::from("hello world");
    let styled = style_line_selection(&line, 6, 11);
    // ["hello ", "world"(SELECTION_BG)] — the selected slice gets the
    // selection background.
    assert_eq!(styled.spans.len(), 2);
    assert_eq!(styled.spans[0].content, "hello ");
    assert_eq!(styled.spans[1].content, "world");
    assert_eq!(styled.spans[1].style.bg, Some(SELECTION_BG));
}

#[test]
fn style_selection_middle_of_span() {
    let line = Line::from("abcdefgh");
    let styled = style_line_selection(&line, 2, 5);
    assert_eq!(styled.spans.len(), 3);
    assert_eq!(styled.spans[0].content, "ab");
    assert_eq!(styled.spans[1].content, "cde");
    assert_eq!(styled.spans[1].style.bg, Some(SELECTION_BG));
    assert_eq!(styled.spans[2].content, "fgh");
}

#[test]
fn style_selection_full_line_keeps_spans() {
    let line = Line::from("hello");
    let styled = style_line_selection(&line, 0, usize::MAX);
    assert_eq!(styled.spans.len(), 1);
    assert_eq!(styled.spans[0].style.bg, Some(SELECTION_BG));
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
    assert_eq!(styled.spans[1].style.bg, Some(SELECTION_BG));
    assert_ne!(styled.spans[0].style.bg, Some(SELECTION_BG));
}

#[test]
fn style_selection_preserves_span_style() {
    let styled = Line::from(Span::styled(
        "abcdef",
        ratatui::style::Style::default().fg(ratatui::style::Color::Cyan),
    ));
    let styled = style_line_selection(&styled, 1, 3);
    assert_eq!(styled.spans.len(), 3);
    // The unselected pieces keep Cyan; the selected piece is Cyan + the
    // selection background.
    assert_eq!(styled.spans[0].style.fg, Some(ratatui::style::Color::Cyan));
    assert_eq!(styled.spans[1].style.fg, Some(ratatui::style::Color::Cyan));
    assert_eq!(styled.spans[1].style.bg, Some(SELECTION_BG));
    assert_eq!(styled.spans[2].style.fg, Some(ratatui::style::Color::Cyan));
}

#[test]
fn style_selection_overrides_shaded_background() {
    // History turns render on the dark-gray BG_SHADE background (see
    // `add_margin_lines`); the selection must replace it with the visible
    // selection background, not stack on top of it.
    let shaded = Line::from(Span::styled(
        "hello",
        ratatui::style::Style::default().bg(crate::render::BG_SHADE),
    ));
    let styled = style_line_selection(&shaded, 0, 5);
    assert_eq!(styled.spans.len(), 1);
    assert_eq!(styled.spans[0].style.bg, Some(SELECTION_BG));
}

// ── apply_selection_to_lines (screen-row mapping) ──

/// Apply a full-width selection to `screen_row` and report whether that
/// row's line ended up with the selection background.
fn row_highlighted(app: &mut App, screen_row: u16) -> bool {
    let vp_width = app.history_viewport.width;
    // The selection is stored in content space, so the target screen row
    // is converted to its content line via the exact mapping
    // `start_selection` applies (`screen_to_content`), not a hand-rolled
    // copy of the formula.
    let (content_line, _) = screen_to_content(app, screen_row, 0);
    app.text_selection = Some(TextSelection {
        anchor: (content_line, 0),
        head: (content_line, vp_width),
        cursor: (screen_row, 0),
        active: true,
        head_sync: None,
    });
    let (turn_idx, visual_row) = find_turn_at_row(app, screen_row).expect("row maps");
    let (cached_lines, offsets, content_ranges, chrome_ranges, turn_start) = {
        let display = app.active_display_ref().unwrap();
        let cached = display.render_cache[turn_idx].as_ref().unwrap();
        (
            cached.rendered.lines.clone(),
            cached.rendered.visual_offsets.clone(),
            cached.rendered.content_ranges.clone(),
            cached.rendered.chrome_ranges.clone(),
            display
                .height_prefix
                .get(turn_idx.wrapping_sub(1))
                .copied()
                .unwrap_or(0),
        )
    };
    // The whole-selection table highlight is built once per frame in
    // production; the test builds it once and threads it in identically.
    let table = table_highlight(app);
    let turn = TurnSlice {
        turn_start,
        text_offsets: &offsets,
        content_ranges: &content_ranges,
        chrome_ranges: &chrome_ranges,
    };
    let mut lines = cached_lines.to_vec();
    apply_selection_to_lines(app, &table, &turn, 0, &mut lines);
    let line_idx = offsets
        .partition_point(|&o| o <= visual_row)
        .min(lines.len().saturating_sub(1));
    lines[line_idx]
        .spans
        .iter()
        .any(|s| s.style.bg == Some(SELECTION_BG))
}

#[test]
fn apply_selection_to_lines_overflowing_history_styles_visible_rows() {
    // Regression (kept as a behavior pin): when the history overflows
    // the viewport (the common long-conversation case), the content is
    // bottom-anchored — a saturating unsigned screen-row offset once
    // clamped to 0 there, so no screen row ever matched and the
    // highlight was never drawn.  The content-space selection has no
    // screen-row math to get wrong, but this still pins that content
    // rows highlight while chrome rows never do in the overflowing
    // layout.
    let mut app = test_app();
    app.history_viewport.width = 80;
    app.history_viewport.height = 10;
    for i in 0..6 {
        app.display_for(0)
            .view
            .insert_or_replace(i, turn(&format!("turn {i}")));
    }
    app.rebuild_height_prefix();
    let vh = app.history_viewport.height as usize;
    let total = app.total_history_height();
    assert!(total > vh, "history must overflow the viewport");

    // Find one visible content row and one visible chrome row (the box
    // separator/padding rows carry no content and must stay
    // unhighlighted).
    let (content_row, chrome_row) = {
        let display = app.active_display_ref().unwrap();
        let mut content_row = None;
        let mut chrome_row = None;
        // The viewport height is a u16 field read into usize; the cast
        // back is exact for any real terminal.
        #[expect(clippy::cast_possible_truncation)] // u16 viewport height round-trip
        for screen_row in 0..vh as u16 {
            let (turn_idx, visual_row) = find_turn_at_row(&app, screen_row).expect("row maps");
            let cached = display.render_cache[turn_idx].as_ref().unwrap();
            let line_idx = cached
                .rendered
                .visual_offsets
                .partition_point(|&o| o <= visual_row)
                .min(cached.rendered.lines.len().saturating_sub(1));
            let has_content = cached
                .rendered
                .content_ranges
                .get(line_idx)
                .copied()
                .flatten()
                .is_some_and(|(lo, hi)| lo < hi);
            if has_content {
                content_row.get_or_insert(screen_row);
            } else {
                chrome_row.get_or_insert(screen_row);
            }
            if content_row.is_some() && chrome_row.is_some() {
                break;
            }
        }
        (
            content_row.expect("a visible content row must exist"),
            chrome_row.expect("a visible chrome row must exist"),
        )
    };

    assert!(
        row_highlighted(&mut app, content_row),
        "a content row must be highlighted"
    );
    assert!(
        !row_highlighted(&mut app, chrome_row),
        "a chrome row (separator/padding) must not be highlighted"
    );
}

#[test]
fn apply_selection_to_lines_short_history_styles_visible_rows() {
    // The short-history case (content fits the viewport, blank band on
    // top): content row c maps to screen row c + vh - total (positive).
    // The selection is built through the public API so the screen →
    // content conversion is exercised (`locate` returns viewport coords).
    let mut app = app_with_turns(&[(0, "hello"), (1, "world")], 30);
    let (start, _) = locate(&app, "hello");
    start_selection(&mut app, start.0, start.1);
    update_selection(&mut app, start.0, start.1.saturating_add(5));
    let (turn_idx, _) = find_turn_at_row(&app, start.0).expect("row maps");
    let (cached_lines, offsets, content_ranges, chrome_ranges, turn_start) = {
        let display = app.active_display_ref().unwrap();
        let cached = display.render_cache[turn_idx].as_ref().unwrap();
        (
            cached.rendered.lines.clone(),
            cached.rendered.visual_offsets.clone(),
            cached.rendered.content_ranges.clone(),
            cached.rendered.chrome_ranges.clone(),
            display
                .height_prefix
                .get(turn_idx.wrapping_sub(1))
                .copied()
                .unwrap_or(0),
        )
    };
    let mut lines = cached_lines.to_vec();
    let table = table_highlight(&app);
    let turn = TurnSlice {
        turn_start,
        text_offsets: &offsets,
        content_ranges: &content_ranges,
        chrome_ranges: &chrome_ranges,
    };
    apply_selection_to_lines(&app, &table, &turn, 0, &mut lines);
    assert!(
        lines
            .iter()
            .any(|l| l.spans.iter().any(|s| s.style.bg == Some(SELECTION_BG))),
        "the selected row must be highlighted"
    );
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

// ── selectable_intervals (content − chrome) ──

#[test]
fn selectable_intervals_empty_chrome_returns_base() {
    let out = selectable_intervals((2, 5), &LineChrome::default());
    assert_eq!(&out[..], &[(2, 5)]);
}

#[test]
fn selectable_intervals_blank_base_is_empty() {
    // A blank content row (`(lo, lo)`) has no cells to select.
    assert!(selectable_intervals((4, 4), &LineChrome::default()).is_empty());
}

#[test]
fn selectable_intervals_subtracts_chrome() {
    // A block quote nested in a list item: the bar (2..4) is cut out of the
    // base range, keeping both the marker before it and the text after.
    let mut chrome = LineChrome::default();
    chrome.push(2, 4);
    assert_eq!(
        &selectable_intervals((0, 9), &chrome)[..],
        &[(0, 2), (4, 9)]
    );
}

#[test]
fn selectable_intervals_clips_leading_chrome_to_base() {
    // Leading chrome already excluded from the base (`(2, 7)`) clips to an
    // empty interval, leaving the whole base selectable.
    let mut chrome = LineChrome::default();
    chrome.push(0, 2);
    assert_eq!(&selectable_intervals((2, 7), &chrome)[..], &[(2, 7)]);
}

// ── wrapped-text copy (unwrapping) ──

/// Screen row of the rendered row containing `needle` (see [`locate`]).
fn locate_row(app: &App, needle: &str) -> u16 {
    locate(app, needle).0.0
}

#[test]
fn wrapped_paragraph_copies_unwrapped_text() {
    // Regression: a long assistant response that the renderer wraps onto
    // several rows used to be copied with the display's wrap points as
    // newlines — the copy reproduced the wrapped text instead of the
    // original.  The renderer now records that the rows are continuations of
    // one paragraph, so the copy must rejoin them with a single space into
    // the exact original sentence.
    let text = "the quick brown fox jumps over the lazy dog and runs far away";
    let mut app = test_app();
    app.history_viewport.width = 30; // content width 21 → wraps to 3 rows
    app.history_viewport.height = 40;
    app.display_for(0).view.insert_or_replace(0, turn(text));
    app.rebuild_height_prefix();
    let first = locate_row(&app, "the quick");
    let last = locate_row(&app, "away");
    assert!(last > first, "paragraph must wrap to multiple rows");

    let copied =
        drag_and_finish(&mut app, (first, 0), (last, 200)).expect("selection should extract");
    assert!(
        !copied.contains('\n'),
        "wrapped rows must be re-joined, not newline-separated: {copied:?}"
    );
    assert_eq!(
        copied.trim(),
        text,
        "the exact original paragraph is copied"
    );
}

#[test]
fn wrapped_paragraphs_keep_paragraph_boundaries() {
    // Paragraph boundaries are real breaks: the copy joins the wrapped rows
    // of each paragraph, and the blank line the renderer inserts between
    // separate paragraphs survives as a blank line.
    let p1 = "first paragraph text that wraps around nicely and continues";
    let p2 = "second paragraph also wraps across the same narrow viewport";
    let md = format!("{p1}\n\n{p2}");
    let mut app = test_app();
    app.history_viewport.width = 30; // content width 21 → both wrap
    app.history_viewport.height = 50;
    app.display_for(0).view.insert_or_replace(0, turn(&md));
    app.rebuild_height_prefix();
    let first = locate_row(&app, "first");
    let last = locate_row(&app, "viewport");
    assert!(last > first, "the two paragraphs must span rows");

    let copied =
        drag_and_finish(&mut app, (first, 0), (last, 200)).expect("selection should extract");
    let mut lines = copied.lines();
    assert_eq!(lines.next(), Some(p1), "first paragraph unwrapped");
    assert_eq!(lines.next(), Some(""), "blank line between paragraphs kept");
    assert_eq!(lines.next(), Some(p2), "second paragraph unwrapped");
    assert_eq!(lines.next(), None, "no extra lines");
}

#[test]
fn blank_line_between_heading_and_paragraph_is_copied() {
    // Regression: the blank spacer row the renderer leaves between a heading
    // and the following paragraph is a *content* row (an empty `(5, 5)`
    // range, recorded with a `Break` join) — not chrome.  It used to be
    // dropped from the slots, so copying the heading and its paragraph
    // collapsed into "heading\nparagraph" and lost the source's blank line;
    // the empty slot plus its `Break` join must reproduce "heading\n\n...".
    let md = "## A Heading\n\nsome paragraph below the heading";
    let mut app = test_app();
    app.history_viewport.width = 80;
    app.history_viewport.height = 40;
    app.display_for(0).view.insert_or_replace(0, turn(md));
    app.rebuild_height_prefix();
    let first = locate_row(&app, "A Heading");
    let last = locate_row(&app, "paragraph");
    assert!(last > first, "heading and paragraph must span rows");

    let copied =
        drag_and_finish(&mut app, (first, 0), (last, 200)).expect("selection should extract");
    assert_eq!(
        copied, "A Heading\n\nsome paragraph below the heading",
        "the source's blank line between heading and paragraph survives"
    );
    // Sanity note: the heading renders at normalized level 1, whose prefix
    // is dropped entirely, so the source's "## " is neither displayed nor
    // copied — only the heading text itself.
}

#[test]
fn blank_lines_in_tool_output_are_copied() {
    // Same bug class as the markdown blank spacers: a genuinely blank line
    // inside verbatim tool output (a `\n\n` in the content) must survive
    // the copy as a blank line, not collapse into a single newline.
    let content = "line one\n\nline two";
    let mut app = test_app();
    app.history_viewport.width = 80;
    app.history_viewport.height = 50;
    let mut t = turn(""); // no assistant text; only the tool block
    t.tool_results = vec![ToolResultRecord {
        call_id: "c1".into(),
        name: "cat".into(),
        content: content.into(),
        is_error: false,
        invocation_description: "read a file".into(),
        image: None,
    }];
    app.display_for(0).view.insert_or_replace(0, t);
    app.rebuild_height_prefix();
    let first = locate_row(&app, "line one");
    let last = locate_row(&app, "line two");
    assert!(last > first, "the two lines must span rows");

    let copied =
        drag_and_finish(&mut app, (first, 0), (last, 200)).expect("selection should extract");
    assert_eq!(copied, content, "the tool output keeps its blank line");
}

#[test]
fn wrapped_plain_tool_output_copies_unwrapped_text() {
    // Long verbatim tool output wraps onto several rows (`plain_text_lines`,)
    // which keeps its whitespace on the previous row — so the copy must
    // rejoin the rows directly (no invented space), yielding the exact
    // original content line.
    let content = "alpha beta gamma delta epsilon zeta eta theta iota kappa lambda mu";
    let mut app = test_app();
    app.history_viewport.width = 30; // tool content width 26 → wraps
    app.history_viewport.height = 60;
    let mut t = turn(""); // no assistant text; only the tool block
    t.tool_results = vec![ToolResultRecord {
        call_id: "c1".into(),
        name: "cat".into(),
        content: content.into(),
        is_error: false,
        invocation_description: "read a long file".into(),
        image: None,
    }];
    app.display_for(0).view.insert_or_replace(0, t);
    app.rebuild_height_prefix();
    let first = locate_row(&app, "alpha");
    let last = locate_row(&app, "mu");
    assert!(last > first, "tool output must wrap to multiple rows");

    let copied =
        drag_and_finish(&mut app, (first, 0), (last, 200)).expect("selection should extract");
    assert!(
        !copied.contains('\n'),
        "wrapped tool rows must be re-joined: {copied:?}"
    );
    assert_eq!(copied.trim(), content, "the exact tool line is copied");
}

#[test]
fn wrapped_code_block_line_rejoins_with_space() {
    // A single source line of a code block wraps without losing anything; the
    // two wrapped rows rejoin with the single space the reflow consumed.
    let md = "```text\nfunction call(a, b) { return a + b; }\n```";
    let mut app = test_app();
    app.history_viewport.width = 30; // content width 21 → code line wraps
    app.history_viewport.height = 60;
    let t = turn(md);
    app.display_for(0).view.insert_or_replace(0, t);
    app.rebuild_height_prefix();
    let first = locate_row(&app, "function");
    // The closing brace is on the final wrapped row of the boxed code line.
    let last = locate_row(&app, "}");
    assert!(last > first, "code line must wrap to multiple rows");

    let copied =
        drag_and_finish(&mut app, (first, 0), (last, 200)).expect("selection should extract");
    assert!(
        !copied.contains('\n'),
        "wrapped code rows must be re-joined: {copied:?}"
    );
    assert_eq!(
        copied.trim(),
        "function call(a, b) { return a + b; }",
        "the code line reads exactly as written"
    );
}

#[test]
fn code_box_copy_is_label_blank_and_code() {
    // Dragging across a whole code box copies the language tag, the blank
    // padding row, and the code — never the `│`/`─` frame or the padding
    // around the code.
    let md = "```rust\nfn main() {}\n```";
    let mut app = test_app();
    app.history_viewport.width = 30; // content width 21 → box hugs the code
    app.history_viewport.height = 40;
    app.display_for(0).view.insert_or_replace(0, turn(md));
    app.rebuild_height_prefix();
    let (start, _) = locate(&app, "rust");
    let (_, end) = locate(&app, "fn main() {}");
    let copied = drag_and_finish(&mut app, start, end).expect("selection should extract");
    assert_eq!(
        copied, "rust\n\nfn main() {}",
        "label, blank padding row and code must be copied verbatim: {copied:?}"
    );
}

#[test]
fn blockquote_in_list_copies_without_bar_keeping_marker() {
    // The fix: a block quote nested directly inside a list item used to copy
    // its `│ ` bar — the old leading-run detector stopped at the list marker, so
    // the bar was neither recognised nor excluded.  The renderer now records the
    // bar as chrome (shifted past the marker), so the copy keeps the list marker
    // and drops only the bar.  The unordered marker renders as `• `.
    for (source, needle, expected) in [
        ("- > hello", "hello", "• hello"),
        ("- > > hi", "hi", "• hi"),
    ] {
        let mut app = app_with_turns(&[(0, source)], 20);
        let ((row, _), end) = locate(&app, needle);
        // Start at the line's first column so the list marker is inside the
        // selection: the marker must survive the copy.
        let copied = drag_and_finish(&mut app, (row, 0), end).expect("selection should extract");
        assert!(!copied.contains('│'), "bar must not be copied: {copied:?}");
        assert_eq!(copied, expected, "marker kept, bar dropped: {copied:?}");
    }
}

#[test]
fn literal_bar_in_prose_is_copied() {
    // A `│ ` in ordinary prose is text, not block-quote/box chrome: the
    // renderer records no chrome for it, so a copy keeps it verbatim.
    let mut app = app_with_turns(&[(0, "a │ b")], 20);
    let ((row, _), _) = locate(&app, "│");
    // Drag from the line's first column to past its end: the bar is in the
    // middle, so the whole row is selected and the bar must survive.
    let copied = drag_and_finish(&mut app, (row, 0), (row, 200)).expect("selection should extract");
    assert!(
        copied.contains('│'),
        "literal bar must be copied: {copied:?}"
    );
    assert_eq!(
        copied.trim(),
        "a │ b",
        "prose is copied verbatim: {copied:?}"
    );
}

#[test]
fn code_box_copy_preserves_interior_blank_line() {
    // Regression: a blank line *inside* a fence is code content, not box
    // padding, and must survive a copy.  It used to be classified as pure
    // chrome (the box's frame plus the pad covered the whole row) and dropped,
    // so copying a multi-line code block collapsed its blank lines away.
    let md = "```rust\nalpha\n\nbeta\n```";
    let mut app = test_app();
    app.history_viewport.width = 30;
    app.history_viewport.height = 40;
    app.display_for(0).view.insert_or_replace(0, turn(md));
    app.rebuild_height_prefix();
    let (start, _) = locate(&app, "alpha");
    let (_, end) = locate(&app, "beta");
    let copied = drag_and_finish(&mut app, start, end).expect("selection should extract");
    assert_eq!(
        copied, "alpha\n\nbeta",
        "the interior blank line must survive the copy: {copied:?}"
    );
}

// ── table selection ─────────────────────────────────────────────────────

#[test]
fn table_selection_excludes_borders() {
    // A normal selection over a table keeps the cell text and drops the `│`
    // borders and the `─` frame rules, one cell per line.
    let md = "| a | b |\n|---|---|\n| 1 | 2 |";
    let mut app = app_with_turns(&[(0, md)], 30);
    let (start, _) = locate(&app, "1");
    let (_, end) = locate(&app, "2");
    let copied = drag_and_finish(&mut app, start, end).expect("selection should extract");
    assert_eq!(
        copied, "1\n\n2",
        "cells copied on their own lines: {copied:?}"
    );
    assert!(!copied.contains('│'), "no cell border copied: {copied:?}");
    assert!(!copied.contains('─'), "no frame rule copied: {copied:?}");
}

#[test]
fn table_wrapped_cell_rejoins_to_original_text() {
    // Dragging down a wrapped cell's column copies the cell's *original* text:
    // the display's wrap rows are rejoined with the space the reflow consumed,
    // not copied as separate lines.  (The neighbouring column's cell is swept
    // in only where it shares a display row — plain line selection, not a cell
    // rectangle — but a blank neighbour contributes no text.)
    let md = "| Key | Value |\n|-----|-------|\n| k | alpha beta gamma delta epsilon |";
    // Content width 31: the Value column shrinks and its cell wraps.
    let mut app = test_app();
    app.history_viewport.width = 40;
    app.history_viewport.height = 40;
    app.display_for(0).view.insert_or_replace(0, turn(md));
    app.rebuild_height_prefix();
    let (start, _) = locate(&app, "alpha");
    let (_, end) = locate(&app, "epsilon");
    let copied = drag_and_finish(&mut app, start, end).expect("selection should extract");
    assert_eq!(
        copied, "alpha beta gamma delta epsilon",
        "the wrapped cell rejoins to its original text: {copied:?}"
    );
    assert!(!copied.contains('│'), "no cell border copied: {copied:?}");
}

#[test]
fn table_cells_are_copied_one_line_each_in_reading_order() {
    // Selecting table content copies each cell's text (its wrapped display rows
    // rejoined), one cell per line, cells separated by a blank line, in
    // row-major order — the table structure dissolves instead of the wrapped
    // rows spilling out line-by-line.
    let md = "| aaaa bbbb cccc dddd | eeee ffff gggg hhhh |\n|---|---|\n| iiii jjjj kkkk llll | mmmm nnnn oooo pppp |";
    // Content width 31: both columns shrink and their cells wrap.
    let mut app = test_app();
    app.history_viewport.width = 40;
    app.history_viewport.height = 40;
    app.display_for(0).view.insert_or_replace(0, turn(md));
    app.rebuild_height_prefix();
    let (start, _) = locate(&app, "iiii");
    let (_, end) = locate(&app, "pppp");
    let copied = drag_and_finish(&mut app, start, end).expect("selection should extract");
    assert_eq!(
        copied, "iiii jjjj kkkk llll\n\nmmmm nnnn oooo pppp",
        "each cell on its own line, blank-line separated: {copied:?}"
    );
    assert!(!copied.contains('│'), "no cell border copied: {copied:?}");
}

#[test]
fn table_selected_with_surrounding_prose_copies_cells_per_line() {
    // The common case: a drag that starts in the prose above a table and runs to
    // its last row.  The prose is copied normally; the table's rows each become
    // one blank-line-separated line per cell (the header is row 0, included
    // because the rectangle covers it).
    let md = "Intro paragraph before the table.\n\n| aaaa bbbb cccc dddd | eeee ffff gggg hhhh |\n|---|---|\n| iiii jjjj kkkk llll | mmmm nnnn oooo pppp |";
    let mut app = test_app();
    app.history_viewport.width = 40;
    app.history_viewport.height = 40;
    app.display_for(0).view.insert_or_replace(0, turn(md));
    app.rebuild_height_prefix();
    let (start, _) = locate(&app, "Intro");
    let (_, end) = locate(&app, "pppp");
    let copied = drag_and_finish(&mut app, start, end).expect("selection should extract");
    assert!(
        copied.starts_with("Intro paragraph before the table."),
        "prose copied first: {copied:?}"
    );
    assert!(
        copied.contains("iiii jjjj kkkk llll\n\nmmmm nnnn oooo pppp"),
        "table cells copied one per line: {copied:?}"
    );
    assert!(!copied.contains('│'), "no cell border copied: {copied:?}");
}

/// The text of every line span that currently carries the selection background.
fn selected_highlight_text(app: &App) -> String {
    let display = app.active_display_ref().expect("display");
    let cached = display
        .render_cache
        .first()
        .and_then(Option::as_ref)
        .expect("render cache");
    let mut lines = cached.rendered.lines.to_vec();
    let table = table_highlight(app);
    let turn = TurnSlice {
        turn_start: 0,
        text_offsets: &cached.rendered.visual_offsets,
        content_ranges: &cached.rendered.content_ranges,
        chrome_ranges: &cached.rendered.chrome_ranges,
    };
    apply_selection_to_lines(app, &table, &turn, 0, &mut lines);
    let mut out = String::new();
    for line in &lines {
        for span in &line.spans {
            if span.style.bg == Some(SELECTION_BG) {
                out.push_str(&span.content);
            }
        }
    }
    out
}

#[test]
fn table_reading_order_fill_copies_cells_with_partial_ends() {
    // Reading-order fill: the cells read row-major from the anchor's cell to the
    // head's cell (inclusive); every middle cell whole, the two end cells sliced
    // at the anchor/head point, one per line, blank-line separated.
    let md = "| ab cd | ef gh | ij kl |\n|---|---|---|\n| mn op | qr st | uv wx |";
    let mut app = test_app();
    app.history_viewport.width = 200;
    app.history_viewport.height = 40;
    app.display_for(0).view.insert_or_replace(0, turn(md));
    app.rebuild_height_prefix();
    // Anchor mid header cell 0 (at "cd"), head at the end of body cell 1's "st".
    let (start, _) = locate(&app, "cd");
    let (_, end) = locate(&app, "st");
    let copied = drag_and_finish(&mut app, start, end).expect("selection should extract");
    assert_eq!(
        copied, "cd\n\nef gh\n\nij kl\n\nmn op\n\nqr st",
        "header cell 0 sliced from 'cd', body cell 1 sliced to 'st': {copied:?}"
    );
}

#[test]
fn table_reading_order_fill_highlights_selected_cells() {
    // The highlight uses the same reading-order fill as the copy, so a cell
    // outside the run (here body column 3) is never highlighted, and neither is
    // the part of an end cell before the anchor.
    let md = "| ab cd | ef gh | ij kl |\n|---|---|---|\n| mn op | qr st | uv wx |";
    let mut app = test_app();
    app.history_viewport.width = 200;
    app.history_viewport.height = 40;
    app.display_for(0).view.insert_or_replace(0, turn(md));
    app.rebuild_height_prefix();
    let (start, _) = locate(&app, "cd");
    let (_, end) = locate(&app, "st");
    start_selection(&mut app, start.0, start.1);
    update_selection(&mut app, end.0, end.1);
    let selected = selected_highlight_text(&app);
    // Exactly the trimmed cell text is highlighted — no cell padding, and
    // neither the part of the first cell before the anchor (`ab`) nor the
    // out-of-range body cell (`uv wx`).
    assert_eq!(
        selected, "cdef ghij klmn opqr st",
        "only the copied text is highlighted: {selected:?}"
    );
    app.text_selection = None;
}

#[test]
fn table_zwj_emoji_highlight_matches_copy() {
    // A ZWJ emoji family in a table cell: the highlighted extent and the
    // copied text must agree.  The trim used to measure columns by summing
    // each char's width, while the copy and the highlight are grapheme-cluster
    // based — for a ZWJ family the char-width sum (2 per person glyph) drifts
    // well past the cluster's 2 columns, so the highlight spilled into the
    // cell padding while the copy kept only the emoji.  Pinning highlight ==
    // copy for a ZWJ cell is the invariant `selection.rs` depends on.
    let family = "\u{1F468}\u{200D}\u{1F469}\u{200D}\u{1F467}\u{200D}\u{1F466}";
    let md = format!("| x |\n|---|\n| {family} |");
    let mut app = test_app();
    app.history_viewport.width = 80;
    app.history_viewport.height = 40;
    app.display_for(0).view.insert_or_replace(0, turn(&md));
    app.rebuild_height_prefix();
    let (start, end) = locate(&app, family);
    start_selection(&mut app, start.0, start.1);
    update_selection(&mut app, end.0, end.1);
    let copied = extract_selection_text(&app).expect("selection should extract");
    let highlighted = selected_highlight_text(&app);
    assert_eq!(
        highlighted, copied,
        "the highlighted extent must equal the copied text"
    );
    assert_eq!(copied, family, "only the emoji is copied: {copied:?}");
    app.text_selection = None;
}
