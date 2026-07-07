use crate::diff_render::build_diff_panes;
use crate::markdown_render::{
    display_width, lines_height, session_message_lines, streaming_text_lines,
};
use tai_proto::SessionMessage;
use crate::state::{
    App, HistoryItem, INPUT_BAR_HEIGHT, Page, RenderedCache, SessionManagerView,
    history_text_height,
};
use ratatui::{
    Frame,
    layout::{Constraint, Direction, Layout, Rect},
    style::{Color, Style},
    text::Line,
    widgets::{Block, Borders, Paragraph, Wrap},
};
use tai_client_core::{DiffLineKind, FileDiff};
use ratatui_image::{Resize, StatefulImage};
use tai_proto::SessionStatus;

pub(crate) fn mouse_in_history_box(column: u16, row: u16) -> bool {
    let Ok((width, height)) = crossterm::terminal::size() else {
        return false;
    };

    if width == 0 || height == 0 || height <= 3 {
        return false;
    }

    let input_height = 3;
    let history_height = height.saturating_sub(input_height);
    column < width && row < history_height
}

pub(crate) fn render(frame: &mut Frame<'_>, app: &mut App) {
    match app.page {
        Page::Chat => render_chat(frame, app),
        Page::SessionManager => render_session_manager(frame, app),
    }
}

fn render_chat(frame: &mut Frame<'_>, app: &mut App) {
    let chunks = Layout::default()
        .direction(Direction::Vertical)
        .constraints([Constraint::Min(1), Constraint::Length(INPUT_BAR_HEIGHT)])
        .split(frame.area());

    render_history(frame, chunks[0], app);

    let input = Paragraph::new(app.input.as_str())
        .block(Block::default().borders(Borders::ALL).title("command"))
        .wrap(Wrap { trim: false });
    frame.render_widget(input, chunks[1]);

    let cursor_x = chunks[1]
        .x
        .saturating_add(1 + display_width(app.input.text.get(..app.input.cursor).unwrap_or("")) as u16);
    let cursor_y = chunks[1].y.saturating_add(1);
    frame.set_cursor_position((cursor_x, cursor_y));
}

fn render_history(frame: &mut Frame<'_>, area: Rect, app: &mut App) {
    if area.width == 0 || area.height == 0 {
        return;
    }

    // Ensure the render cache is aligned with the history vector before
    // we start iterating.  If items were pushed/inserted/trimmed since the
    // last frame, this rebuilds the cache from scratch (all Nones).
    app.ensure_cache_synced();

    let mut rows_remaining = area.height as usize;
    let mut y = area.y + area.height;
    let mut rows_to_skip = app.effective_scroll();

    // Iterate by index so we can borrow the history and render_cache
    // independently (they are separate fields of App).
    let len = app.client.history.len();
    for raw_i in 0..len {
        let i = len - 1 - raw_i;
        let item = &mut app.client.history[i];

        if rows_remaining == 0 {
            break;
        }

        match item {
            HistoryItem::Text(text) => {
                render_history_text(
                    frame,
                    area,
                    text.as_str(),
                    &mut rows_remaining,
                    &mut y,
                    &mut rows_to_skip,
                );
            }
            HistoryItem::SessionMessage(message) => {
                let lines = cached_or_compute_lines(
                    &mut app.render_cache,
                    i,
                    |msg| session_message_lines(msg, area.width),
                    message,
                    area.width,
                );
                render_history_lines(
                    frame,
                    area,
                    lines,
                    &mut rows_remaining,
                    &mut y,
                    &mut rows_to_skip,
                );
            }
            HistoryItem::Streaming(text) => {
                let lines = streaming_text_lines(text, area.width);
                render_history_lines(
                    frame,
                    area,
                    lines,
                    &mut rows_remaining,
                    &mut y,
                    &mut rows_to_skip,
                );
            }
            HistoryItem::Image(image) => {
                let rendered = image.protocol.size_for(
                    Resize::Scale(None),
                    ratatui::layout::Size::new(area.width, (area.height / 2).max(1)),
                );
                let full_height = rendered.height.max(1) as usize;
                if rows_to_skip >= full_height {
                    rows_to_skip -= full_height;
                    continue;
                }

                // Clip the item's full height by the number of rows
                // already scrolled past (rows_to_skip) *and* by the
                // remaining space in the viewport.  The old code used
                // `image_block_height(rows_remaining)` directly, which
                // ignored rows_to_skip and caused layout jumps at item
                // boundaries when partially scrolled past an image.
                let visible_height = (full_height.saturating_sub(rows_to_skip)).min(rows_remaining);
                let height = visible_height as u16;
                if height == 0 {
                    break;
                }
                y = y.saturating_sub(height);
                let block = Block::default().title(format!(
                    "image {} ({} {}x{})",
                    image.metadata.image_id,
                    image.metadata.mime_type,
                    image.metadata.width,
                    image.metadata.height
                ));
                let rect = Rect {
                    x: area.x,
                    y,
                    width: area.width,
                    height,
                };
                let inner = block.inner(rect);
                frame.render_widget(block, rect);

                // Only render the image when fully visible — the
                // image is not clipped by scroll offset or viewport
                // space, so its rect is stable and ratatui_image
                // never rescales during scrolling.
                if visible_height == full_height {
                    frame.render_stateful_widget(
                        StatefulImage::new().resize(Resize::Scale(None)),
                        inner,
                        &mut image.protocol,
                    );
                }
                rows_remaining = rows_remaining.saturating_sub(height as usize);
                rows_to_skip = 0;
            }
            HistoryItem::Diff(diffs) => {
                render_history_diff(
                    frame,
                    area,
                    diffs,
                    &mut rows_remaining,
                    &mut y,
                    &mut rows_to_skip,
                );
            }
        }
    }
}

/// Return the cached rendered lines for a history item, or compute, cache,
/// and return them.  Only caches items with stable content (not Streaming).
fn cached_or_compute_lines(
    cache: &mut [Option<RenderedCache>],
    index: usize,
    compute: impl FnOnce(&SessionMessage) -> Vec<Line<'static>>,
    message: &SessionMessage,
    width: u16,
) -> Vec<Line<'static>> {
    // Fast path: cache hit at the current width.
    if let Some(Some(cached)) = cache.get(index) {
        if cached.width == width {
            return cached.lines.clone();
        }
    }

    // Cache miss: compute, store, and return.
    let lines = compute(message);
    let height = lines_height(&lines, width);
    if let Some(slot) = cache.get_mut(index) {
        *slot = Some(RenderedCache {
            lines: lines.clone(),
            height,
            width,
        });
    }
    lines
}

fn render_history_text(
    frame: &mut Frame<'_>,
    area: Rect,
    text: &str,
    rows_remaining: &mut usize,
    y: &mut u16,
    rows_to_skip: &mut usize,
) {
    let wrapped = history_text_height(text, area.width).max(1);
    if *rows_to_skip >= wrapped {
        *rows_to_skip -= wrapped;
        return;
    }

    // The visible portion of this text item is the wrapped height
    // minus any rows already skipped past (rows_to_skip), further
    // clamped to the remaining viewport space.  Previously this was
    // `wrapped.min(*rows_remaining)` — omitting rows_to_skip meant
    // the view would jump at item boundaries during partial-scroll.
    let visible_height = (wrapped.saturating_sub(*rows_to_skip)).min(*rows_remaining);
    if visible_height == 0 {
        return;
    }

    let bottom_line = wrapped.saturating_sub(*rows_to_skip);
    let top_line = bottom_line.saturating_sub(visible_height);

    *y = (*y).saturating_sub(visible_height as u16);
    let rect = Rect {
        x: area.x,
        y: *y,
        width: area.width,
        height: visible_height as u16,
    };
    frame.render_widget(
        Paragraph::new(text)
            .wrap(Wrap { trim: false })
            .scroll((top_line as u16, 0)),
        rect,
    );
    *rows_remaining -= visible_height;
    *rows_to_skip = 0;
}

fn render_history_lines(
    frame: &mut Frame<'_>,
    area: Rect,
    lines: Vec<Line<'static>>,
    rows_remaining: &mut usize,
    y: &mut u16,
    rows_to_skip: &mut usize,
) {
    let wrapped = lines_height(&lines, area.width).max(1);
    if *rows_to_skip >= wrapped {
        *rows_to_skip -= wrapped;
        return;
    }

    // Same pattern as render_history_text: the visible height is the
    // wrapped content height minus rows already skipped, clamped to
    // remaining viewport space.  Without accounting for rows_to_skip,
    // a partial scroll past a session-message boundary would cause a
    // visible jump on the next frame.
    let visible_height = (wrapped.saturating_sub(*rows_to_skip)).min(*rows_remaining);
    if visible_height == 0 {
        return;
    }

    let bottom_line = wrapped.saturating_sub(*rows_to_skip);
    let top_line = bottom_line.saturating_sub(visible_height);

    *y = (*y).saturating_sub(visible_height as u16);
    let rect = Rect {
        x: area.x,
        y: *y,
        width: area.width,
        height: visible_height as u16,
    };
    frame.render_widget(
        Paragraph::new(lines)
            .wrap(Wrap { trim: false })
            .scroll((top_line as u16, 0)),
        rect,
    );
    *rows_remaining -= visible_height;
    *rows_to_skip = 0;
}

// ── Session Manager ──────────────────────────────────────────

fn render_session_manager(frame: &mut Frame<'_>, app: &mut App) {
    match app.session_mgr.view {
        SessionManagerView::List => render_session_list_view(frame, app),
        SessionManagerView::Detail => render_session_detail_view(frame, app),
    }
}

fn render_session_list_view(frame: &mut Frame<'_>, app: &mut App) {
    let area = frame.area();
    let chunks = Layout::default()
        .direction(Direction::Vertical)
        .constraints([Constraint::Min(1), Constraint::Length(1)])
        .split(area);

    let block = Block::default()
        .title(" Session Manager ")
        .borders(Borders::ALL);
    let inner = block.inner(chunks[0]);
    frame.render_widget(block, chunks[0]);

    let scroll = app.session_mgr.scroll;
    let max_rows = inner.height as usize;

    if app.session_mgr.sessions.is_empty() {
        let msg = Paragraph::new("No sessions. Press 'n' to create one.");
        frame.render_widget(msg, inner);
    } else {
        let mut lines: Vec<Line> = Vec::new();
        for i in scroll..app.session_mgr.sessions.len() {
            if lines.len() >= max_rows {
                break;
            }
            let session = &app.session_mgr.sessions[i];
            let is_selected = Some(i) == app.session_mgr.selection;
            let is_attached = Some(session.session_id) == app.attached_session_id;

            let sel = if is_selected { ">" } else { " " };
            let att = if is_attached { "*" } else { " " };
            let title = session.title.as_deref().unwrap_or("untitled");
            let model = session.selected_model.as_deref().unwrap_or("-");
            let status_str = match &session.status {
                SessionStatus::Sleeping => "sleep",
                SessionStatus::Inactive => "idle",
                SessionStatus::Inference => "infer",
                SessionStatus::ToolCall(name) => &name,
            };
            let status_style = match &session.status {
                SessionStatus::Sleeping => Color::DarkGray,
                SessionStatus::Inactive => Color::Green,
                SessionStatus::Inference => Color::Yellow,
                SessionStatus::ToolCall(_) => Color::Cyan,
            };
            let row = format!(
                "{sel}{att} {:>4}  \"{title}\"  ({model})  — {} messages  [",
                session.session_id, session.message_count,
            );

            let style = if is_selected {
                Style::default()
                    .bg(Color::Blue)
                    .fg(Color::White)
            } else {
                Style::default()
            };
            let status_label = format!("{}]", status_str);
            lines.push(Line::from(vec![
                ratatui::text::Span::styled(row, style),
                ratatui::text::Span::styled(status_label, Style::default().fg(status_style)),
            ]));
        }

        let paragraph = Paragraph::new(lines);
        frame.render_widget(paragraph, inner);
    }

    let status = Paragraph::new(Line::from(format!(
        " <j/k nav>  <Enter switch>  <i details>  <n new>  <Esc back>  —  {} sessions",
        app.session_mgr.sessions.len()
    )));
    frame.render_widget(status, chunks[1]);
}

fn render_session_detail_view(frame: &mut Frame<'_>, app: &mut App) {
    let area = frame.area();
    let chunks = Layout::default()
        .direction(Direction::Vertical)
        .constraints([Constraint::Min(1), Constraint::Length(1)])
        .split(area);

    let block = Block::default()
        .title(" Session Details ")
        .borders(Borders::ALL);
    let inner = block.inner(chunks[0]);
    frame.render_widget(block, chunks[0]);

    if let Some(ref detail) = app.session_mgr.detail_data {
        let lines = vec![
            Line::from(format!("Session ID:    {}", detail.session_id)),
            Line::from(format!("Title:         {}", detail.title)),
            Line::from(format!("Model:         {}", detail.selected_model)),
            Line::from(format!(
                "Parent:        {}",
                detail
                    .parent_session_id
                    .map_or("none".to_string(), |id| id.to_string())
            )),
            Line::from(format!("CWD:           {}", detail.cwd)),
            Line::from(format!("Created:       {}", format_timestamp(detail.created_at))),
            Line::from(format!("Message Count: {}", detail.message_count)),
            Line::from(format!(
                "Max Turns:     {}",
                detail
                    .max_turns
                    .map_or("unlimited".to_string(), |mt| mt.to_string())
            )),
            Line::from(format!("Status:        {}", format_status(&detail.status))),
            Line::from(format!("Tool Groups:   {}", detail.active_tool_groups.join(", "))),
        ];
        let paragraph = Paragraph::new(lines);
        frame.render_widget(paragraph, inner);
    }

    let status =
        Paragraph::new(Line::from(" <b back>  <Enter switch to this session>"));
    frame.render_widget(status, chunks[1]);
}

fn format_timestamp(ts: i64) -> String {
    if ts <= 0 {
        return "-".to_string();
    }

    let mut t = ts as u64;
    let secs = t % 60;
    t /= 60;
    let mins = t % 60;
    t /= 60;
    let hours = t % 24;
    t /= 24;
    let days = t;

    let mut y = 1970i64;
    let mut d = days as i64;
    loop {
        let days_in_year = if is_leap(y) { 366 } else { 365 };
        if d < days_in_year {
            break;
        }
        d -= days_in_year;
        y += 1;
    }

    const MONTH_DAYS: [i64; 12] = [31, 28, 31, 30, 31, 30, 31, 31, 30, 31, 30, 31];
    let mut m = 0;
    for &md in &MONTH_DAYS {
        let adj = if m == 1 && is_leap(y) { 29 } else { md };
        if d < adj {
            break;
        }
        d -= adj;
        m += 1;
    }

    format!(
        "{:04}-{:02}-{:02} {:02}:{:02}:{:02}",
        y,
        m + 1,
        d + 1,
        hours,
        mins,
        secs
    )
}

fn is_leap(y: i64) -> bool {
    (y % 4 == 0 && y % 100 != 0) || y % 400 == 0
}

fn format_status(status: &SessionStatus) -> String {
    match status {
        SessionStatus::Sleeping => "sleeping".to_string(),
        SessionStatus::Inactive => "idle".to_string(),
        SessionStatus::Inference => "inferring".to_string(),
        SessionStatus::ToolCall(name) => format!("tool call: {name}"),
    }
}

// ── Diff rendering ─────────────────────────────────────────

/// Render a side-by-side diff into the terminal frame.
///
/// The diff is first converted into aligned left/right pane rows via
/// `build_diff_panes`, then clipped to the visible viewport through the
/// `rows_to_skip` / `rows_remaining` scrolling mechanism (shared with text
/// rendering). Each row is split into two panes separated by a `│` gutter.
/// Deletions appear highlighted in red on the left; additions in green on the right.
fn render_history_diff(
    frame: &mut Frame<'_>,
    area: Rect,
    diffs: &[FileDiff],
    rows_remaining: &mut usize,
    y: &mut u16,
    rows_to_skip: &mut usize,
) {
    use crate::diff_render::diff_display_height;

    let full_height = diff_display_height(diffs);
    if *rows_to_skip >= full_height {
        *rows_to_skip -= full_height;
        return;
    }

    // Same visible-height calculation as the text/lines renderers:
    // subtract the portion already scrolled past, then clamp to the
    // viewport rows that are still available.
    let visible_height = (full_height.saturating_sub(*rows_to_skip)).min(*rows_remaining);
    if visible_height == 0 {
        return;
    }

    let bottom_line = full_height.saturating_sub(*rows_to_skip);
    let top_line = bottom_line.saturating_sub(visible_height);

    *y = (*y).saturating_sub(visible_height as u16);
    let rect = Rect {
        x: area.x,
        y: *y,
        width: area.width,
        height: visible_height as u16,
    };

    let pane_width = if area.width > 4 {
        (area.width - 2) / 2
    } else {
        1
    };

    let rows = build_diff_panes(diffs);
    let visible_rows: Vec<_> = rows
        .iter()
        .skip(top_line)
        .take(visible_height)
        .collect();

    let mut left_spans: Vec<Vec<ratatui::text::Span>> = Vec::with_capacity(visible_rows.len());
    let mut right_spans: Vec<Vec<ratatui::text::Span>> = Vec::with_capacity(visible_rows.len());

    for row in &visible_rows {
        left_spans.push(diff_cell_spans(&row.left_spans, row.left_kind, pane_width, true));
        right_spans.push(diff_cell_spans(&row.right_spans, row.right_kind, pane_width, false));
    }

    let separator_style = Style::default().fg(Color::DarkGray);
    let separator_str = "│";

    let mut lines: Vec<Line> = Vec::with_capacity(visible_rows.len());
    for i in 0..visible_rows.len() {
        let mut spans = Vec::new();
        spans.extend(left_spans[i].clone());
        // separator
        spans.push(ratatui::text::Span::styled(
            separator_str.to_string(),
            separator_style,
        ));
        spans.extend(right_spans[i].clone());
        lines.push(Line::from(spans));
    }

    let paragraph = Paragraph::new(lines);
    frame.render_widget(paragraph, rect);

    *rows_remaining = rows_remaining.saturating_sub(visible_height);
    *rows_to_skip = 0;
}

/// Return the diff foreground colour for a given line kind and side.
/// Used as a fallback when no syntax highlighting is present.
fn diff_fg(kind: DiffLineKind, is_left: bool) -> Option<Color> {
    match (kind, is_left) {
        (DiffLineKind::Deletion, true) => Some(Color::Red),
        (DiffLineKind::Addition, false) => Some(Color::Green),
        _ => None,
    }
}

/// Apply diff background styling to pre-computed syntax-highlighted spans.
///
/// * Overlays the red (deletion) or green (addition) background on each span
///   while preserving the syntax foreground colour from syntect.
/// * Pads with spaces when the content is narrower than the pane so the
///   background colour fills the entire cell.
/// * Truncates content that exceeds the pane width, appending `…`.
fn diff_cell_spans(
    spans: &[ratatui::text::Span<'static>],
    kind: DiffLineKind,
    pane_width: u16,
    is_left: bool,
) -> Vec<ratatui::text::Span<'static>> {
    let pane_width = pane_width as usize;

    let diff_bg = match (kind, is_left) {
        (DiffLineKind::Deletion, true) => Some(Color::Rgb(80, 0, 0)),
        (DiffLineKind::Addition, false) => Some(Color::Rgb(0, 80, 0)),
        _ => None,
    };

    // Apply background to each span while keeping the syntax foreground.
    // When no syntax foreground is set (plain text), fall back to the diff
    // foreground colour (red for deletions, green for additions) so that
    // header / hunk-header rows and plain-text file types still get coloured.
    let styled: Vec<ratatui::text::Span<'static>> = spans
        .iter()
        .map(|s| {
            let style = match diff_bg {
                Some(bg) => ratatui::style::Style {
                    fg: s.style.fg.or_else(|| diff_fg(kind, is_left)),
                    ..ratatui::style::Style::default()
                }
                .bg(bg),
                None => s.style,
            };
            ratatui::text::Span::styled(s.content.clone(), style)
        })
        .collect();

    let total: usize = styled
        .iter()
        .map(|s| display_width(&s.content))
        .sum();

    if total > pane_width {
        // Truncate spans to fit pane_width, preserving syntax colours on
        // the remaining content and appending `…` at the end.
        let mut result = Vec::new();
        let mut remaining = pane_width.saturating_sub(1);
        for span in styled {
            let w = display_width(&span.content);
            if w <= remaining {
                result.push(span);
                remaining -= w;
            } else if remaining > 0 {
                // Walk characters by display width, not by scalar count,
                // so CJK/emoji (width 2) are handled correctly.
                let truncated: String = span.content
                    .chars()
                    .scan(remaining, |budget, c| {
                        let cw = unicode_width::UnicodeWidthChar::width(c).unwrap_or(0);
                        if *budget >= cw { *budget -= cw; Some(c) } else { None }
                    })
                    .collect();
                result.push(ratatui::text::Span::styled(truncated, span.style));
                break;
            } else {
                break;
            }
        }
        let ellipsis_style = diff_bg
            .map_or(Style::default(), |b| Style::default().bg(b));
        result.push(ratatui::text::Span::styled("…".to_string(), ellipsis_style));
        result
    } else {
        // Always pad to pane_width so the `│` separator stays at a fixed
        // column (every row's left and right cells must have the same byte
        // width).  The padding span uses the diff background when present,
        // otherwise default style.
        let mut result = styled;
        let pad_style = diff_bg
            .map_or(Style::default(), |b| {
                ratatui::style::Style {
                    fg: None,
                    ..ratatui::style::Style::default()
                }
                .bg(b)
            });
        result.push(ratatui::text::Span::styled(
            " ".repeat(pane_width.saturating_sub(total)),
            pad_style,
        ));
        result
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use ratatui::backend::TestBackend;
    use ratatui::Terminal;
    use tai_client_core::{DiffHunk, DiffLine};

    // ── render_history_text tests ──

    #[test]
    fn render_history_text_no_skip() {
        let backend = TestBackend::new(80, 30);
        let mut terminal = Terminal::new(backend).unwrap();
        let mut rows_remaining = 30;
        let mut y = 30;
        let mut rows_to_skip = 0;

        terminal
            .draw(|frame| {
                render_history_text(
                    frame,
                    Rect { x: 0, y: 0, width: 80, height: 30 },
                    "line1\nline2",
                    &mut rows_remaining,
                    &mut y,
                    &mut rows_to_skip,
                );
            })
            .unwrap();

        assert_eq!(rows_remaining, 28, "consumed 2 visible rows");
        assert_eq!(y, 28, "y moved up by 2");
        assert_eq!(rows_to_skip, 0, "rows_to_skip consumed completely");
    }

    #[test]
    fn render_history_text_partial_skip() {
        let backend = TestBackend::new(80, 30);
        let mut terminal = Terminal::new(backend).unwrap();
        let mut rows_remaining = 30;
        let mut y = 30;
        let mut rows_to_skip = 2;

        terminal
            .draw(|frame| {
                render_history_text(
                    frame,
                    Rect { x: 0, y: 0, width: 80, height: 30 },
                    "line1\nline2\nline3\nline4\nline5",
                    &mut rows_remaining,
                    &mut y,
                    &mut rows_to_skip,
                );
            })
            .unwrap();

        // wrapped=5, skip=2 → visible = (5-2).min(30) = 3 → remaining = 30-3 = 27
        assert_eq!(rows_remaining, 27);
        assert_eq!(y, 27);
        assert_eq!(rows_to_skip, 0);
    }

    #[test]
    fn render_history_text_full_skip() {
        let backend = TestBackend::new(80, 30);
        let mut terminal = Terminal::new(backend).unwrap();
        let mut rows_remaining = 30;
        let mut y = 30;
        let mut rows_to_skip = 10;

        terminal
            .draw(|frame| {
                render_history_text(
                    frame,
                    Rect { x: 0, y: 0, width: 80, height: 30 },
                    "line1\nline2\nline3\nline4\nline5",
                    &mut rows_remaining,
                    &mut y,
                    &mut rows_to_skip,
                );
            })
            .unwrap();

        // wrapped=5 <= skip=10 → fully skipped, skip reduced by 5
        assert_eq!(rows_remaining, 30, "no rows consumed");
        assert_eq!(y, 30, "y unchanged");
        assert_eq!(rows_to_skip, 5, "rows_to_skip decremented by 5");
    }

    #[test]
    fn render_history_text_exhausted_viewport() {
        let backend = TestBackend::new(80, 30);
        let mut terminal = Terminal::new(backend).unwrap();
        let mut rows_remaining = 2;
        let mut y = 30;
        let mut rows_to_skip = 2;

        terminal
            .draw(|frame| {
                render_history_text(
                    frame,
                    Rect { x: 0, y: 0, width: 80, height: 30 },
                    "line1\nline2\nline3\nline4\nline5",
                    &mut rows_remaining,
                    &mut y,
                    &mut rows_to_skip,
                );
            })
            .unwrap();

        // wrapped=5, skip=2 → visible = (5-2).min(2) = 2 → remaining = 0
        assert_eq!(rows_remaining, 0, "viewport exhausted");
        assert_eq!(y, 28);
        assert_eq!(rows_to_skip, 0);
    }

    #[test]
    fn render_history_text_zero_remaining() {
        let backend = TestBackend::new(80, 30);
        let mut terminal = Terminal::new(backend).unwrap();
        let mut rows_remaining = 0;
        let mut y = 0;
        let mut rows_to_skip = 0;

        terminal
            .draw(|frame| {
                render_history_text(
                    frame,
                    Rect { x: 0, y: 0, width: 80, height: 30 },
                    "content",
                    &mut rows_remaining,
                    &mut y,
                    &mut rows_to_skip,
                );
            })
            .unwrap();

        // visible = (1-0).min(0) = 0 → returns early
        assert_eq!(rows_remaining, 0);
        assert_eq!(y, 0);
        assert_eq!(rows_to_skip, 0);
    }

    // ── render_history_lines tests ──

    #[test]
    fn render_history_lines_no_skip() {
        let backend = TestBackend::new(80, 30);
        let mut terminal = Terminal::new(backend).unwrap();
        let mut rows_remaining = 30;
        let mut y = 30;
        let mut rows_to_skip = 0;

        terminal
            .draw(|frame| {
                render_history_lines(
                    frame,
                    Rect { x: 0, y: 0, width: 80, height: 30 },
                    vec![Line::from("a"), Line::from("b"), Line::from("c")],
                    &mut rows_remaining,
                    &mut y,
                    &mut rows_to_skip,
                );
            })
            .unwrap();

        assert_eq!(rows_remaining, 27, "3 rows consumed");
        assert_eq!(y, 27);
        assert_eq!(rows_to_skip, 0);
    }

    #[test]
    fn render_history_lines_partial_skip() {
        let backend = TestBackend::new(80, 30);
        let mut terminal = Terminal::new(backend).unwrap();
        let mut rows_remaining = 30;
        let mut y = 30;
        let mut rows_to_skip = 1;

        terminal
            .draw(|frame| {
                render_history_lines(
                    frame,
                    Rect { x: 0, y: 0, width: 80, height: 30 },
                    vec![Line::from("a"), Line::from("b"), Line::from("c")],
                    &mut rows_remaining,
                    &mut y,
                    &mut rows_to_skip,
                );
            })
            .unwrap();

        // wrapped=3, skip=1 → visible=2 → remaining=28
        assert_eq!(rows_remaining, 28);
        assert_eq!(y, 28);
        assert_eq!(rows_to_skip, 0);
    }

    #[test]
    fn render_history_lines_full_skip() {
        let backend = TestBackend::new(80, 30);
        let mut terminal = Terminal::new(backend).unwrap();
        let mut rows_remaining = 30;
        let mut y = 30;
        let mut rows_to_skip = 10;

        terminal
            .draw(|frame| {
                render_history_lines(
                    frame,
                    Rect { x: 0, y: 0, width: 80, height: 30 },
                    vec![Line::from("only")],
                    &mut rows_remaining,
                    &mut y,
                    &mut rows_to_skip,
                );
            })
            .unwrap();

        assert_eq!(rows_remaining, 30, "no rows consumed");
        assert_eq!(y, 30);
        assert_eq!(rows_to_skip, 9, "rows_to_skip decremented by 1");
    }

    #[test]
    fn render_history_lines_zero_remaining() {
        let backend = TestBackend::new(80, 30);
        let mut terminal = Terminal::new(backend).unwrap();
        let mut rows_remaining = 0;
        let mut y = 0;
        let mut rows_to_skip = 0;

        terminal
            .draw(|frame| {
                render_history_lines(
                    frame,
                    Rect { x: 0, y: 0, width: 80, height: 30 },
                    vec![Line::from("content")],
                    &mut rows_remaining,
                    &mut y,
                    &mut rows_to_skip,
                );
            })
            .unwrap();

        assert_eq!(rows_remaining, 0);
        assert_eq!(y, 0);
        assert_eq!(rows_to_skip, 0);
    }

    // ── render_history_diff tests ──

    #[test]
    fn render_history_diff_no_skip() {
        let backend = TestBackend::new(80, 30);
        let mut terminal = Terminal::new(backend).unwrap();
        let mut rows_remaining = 30;
        let mut y = 30;
        let mut rows_to_skip = 0;

        let diffs = vec![FileDiff {
            old_path: String::new(),
            new_path: String::new(),
            hunks: vec![DiffHunk {
                header: "header".to_string(),
                lines: vec![DiffLine {
                    kind: DiffLineKind::Context,
                    content: "unchanged".to_string(),
                }],
            }],
        }];

        terminal
            .draw(|frame| {
                render_history_diff(
                    frame,
                    Rect { x: 0, y: 0, width: 80, height: 30 },
                    &diffs,
                    &mut rows_remaining,
                    &mut y,
                    &mut rows_to_skip,
                );
            })
            .unwrap();

        // build_diff_panes always emits a file header row, so height = 1 (file) + 1 (hunk) + 1 (line) = 3
        assert_eq!(rows_remaining, 27, "3 diff rows consumed");
        assert_eq!(y, 27, "y moved up by 3");
        assert_eq!(rows_to_skip, 0);
    }

    #[test]
    fn render_history_diff_partial_skip() {
        let backend = TestBackend::new(80, 30);
        let mut terminal = Terminal::new(backend).unwrap();
        let mut rows_remaining = 30;
        let mut y = 30;
        let mut rows_to_skip = 1;

        let diffs = vec![FileDiff {
            old_path: String::new(),
            new_path: String::new(),
            hunks: vec![DiffHunk {
                header: "hdr".to_string(),
                lines: vec![DiffLine {
                    kind: DiffLineKind::Addition,
                    content: "added".to_string(),
                }],
            }],
        }];

        terminal
            .draw(|frame| {
                render_history_diff(
                    frame,
                    Rect { x: 0, y: 0, width: 80, height: 30 },
                    &diffs,
                    &mut rows_remaining,
                    &mut y,
                    &mut rows_to_skip,
                );
            })
            .unwrap();

        // full_height=3, skip=1 → visible=2 → remaining=28
        assert_eq!(rows_remaining, 28);
        assert_eq!(y, 28);
        assert_eq!(rows_to_skip, 0);
    }

    #[test]
    fn render_history_diff_full_skip() {
        let backend = TestBackend::new(80, 30);
        let mut terminal = Terminal::new(backend).unwrap();
        let mut rows_remaining = 30;
        let mut y = 30;
        let mut rows_to_skip = 10;

        let diffs = vec![FileDiff {
            old_path: String::new(),
            new_path: String::new(),
            hunks: vec![DiffHunk {
                header: "h".to_string(),
                lines: vec![DiffLine {
                    kind: DiffLineKind::Context,
                    content: "c".to_string(),
                }],
            }],
        }];

        terminal
            .draw(|frame| {
                render_history_diff(
                    frame,
                    Rect { x: 0, y: 0, width: 80, height: 30 },
                    &diffs,
                    &mut rows_remaining,
                    &mut y,
                    &mut rows_to_skip,
                );
            })
            .unwrap();

        // full_height=3 <= skip=10 → fully skipped, skip reduced by 3
        assert_eq!(rows_remaining, 30);
        assert_eq!(y, 30);
        assert_eq!(rows_to_skip, 7);
    }

    // ── diff_cell_spans tests ──

    fn span_from_text(text: &str) -> Vec<ratatui::text::Span<'static>> {
        vec![ratatui::text::Span::styled(
            text.to_string(),
            Style::default(),
        )]
    }

    #[test]
    fn diff_cell_spans_pads_short_content() {
        let spans = diff_cell_spans(&span_from_text("hi"), DiffLineKind::Context, 10, true);
        let text = spans[0].content.trim_end();
        assert!(text.starts_with("hi"), "content='{text}' should start with 'hi'");
    }

    #[test]
    fn diff_cell_spans_truncates_long_content() {
        let long = "a".repeat(20);
        let spans = diff_cell_spans(&span_from_text(&long), DiffLineKind::Context, 5, true);
        // truncated to 4 chars in the first span + '…' as a separate span = 2 spans
        assert_eq!(spans[0].content.chars().count(), 4);
        assert_eq!(spans[1].content, "…");
    }

    #[test]
    fn diff_cell_spans_left_deletion_has_red_style() {
        let spans = diff_cell_spans(&span_from_text("del"), DiffLineKind::Deletion, 10, true);
        let style = spans[0].style;
        assert_eq!(style.fg, Some(Color::Red));
        assert_eq!(style.bg, Some(Color::Rgb(80, 0, 0)));
    }

    #[test]
    fn diff_cell_spans_right_deletion_has_default_style() {
        let spans = diff_cell_spans(&span_from_text("del"), DiffLineKind::Deletion, 10, false);
        assert_eq!(spans[0].style, Style::default());
    }

    #[test]
    fn diff_cell_spans_right_addition_has_green_style() {
        let spans = diff_cell_spans(&span_from_text("add"), DiffLineKind::Addition, 10, false);
        let style = spans[0].style;
        assert_eq!(style.fg, Some(Color::Green));
        assert_eq!(style.bg, Some(Color::Rgb(0, 80, 0)));
    }

    #[test]
    fn diff_cell_spans_left_addition_has_default_style() {
        let spans = diff_cell_spans(&span_from_text("add"), DiffLineKind::Addition, 10, true);
        assert_eq!(spans[0].style, Style::default());
    }

    #[test]
    fn diff_cell_spans_context_has_default_style() {
        let spans = diff_cell_spans(&span_from_text("ctx"), DiffLineKind::Context, 10, true);
        assert_eq!(spans[0].style, Style::default());
        let spans = diff_cell_spans(&span_from_text("ctx"), DiffLineKind::Context, 10, false);
        assert_eq!(spans[0].style, Style::default());
    }

    #[test]
    fn diff_cell_spans_preserves_syntax_fg_on_deletion() {
        // Simulate a syntax-highlighted span with a non-default fg colour
        let input = vec![ratatui::text::Span::styled(
            "fn".to_string(),
            Style::default().fg(Color::Rgb(200, 100, 0)),
        )];
        let spans = diff_cell_spans(&input, DiffLineKind::Deletion, 10, true);
        assert_eq!(
            spans[0].style.fg,
            Some(Color::Rgb(200, 100, 0)),
            "syntax foreground should be preserved on deletion"
        );
        assert_eq!(
            spans[0].style.bg,
            Some(Color::Rgb(80, 0, 0)),
            "diff background should be applied"
        );
    }

    #[test]
    fn diff_cell_spans_preserves_syntax_fg_on_addition() {
        let input = vec![ratatui::text::Span::styled(
            "let".to_string(),
            Style::default().fg(Color::Rgb(0, 150, 200)),
        )];
        let spans = diff_cell_spans(&input, DiffLineKind::Addition, 10, false);
        assert_eq!(spans[0].style.fg, Some(Color::Rgb(0, 150, 200)));
        assert_eq!(spans[0].style.bg, Some(Color::Rgb(0, 80, 0)));
    }

    #[test]
    fn diff_cell_spans_pads_with_diff_background() {
        let spans = diff_cell_spans(&span_from_text("hi"), DiffLineKind::Deletion, 10, true);
        // There should be a padding span at the end with the diff background
        assert!(spans.len() > 1, "should have padding span");
        assert_eq!(
            spans.last().unwrap().style.bg,
            Some(Color::Rgb(80, 0, 0)),
        );
        // The text span should have the red bg too
        assert_eq!(
            spans[0].style.bg,
            Some(Color::Rgb(80, 0, 0)),
        );
    }
}
