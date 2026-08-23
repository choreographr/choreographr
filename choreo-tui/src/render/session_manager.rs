// The session-manager page's rendering, extracted from the former monolithic
// render.rs.  Shared helpers (status/timestamp formatting, the scrollbar
// widget) live in the parent render/mod.rs and are imported below.
use crate::diff_render::truncate_str;
use crate::scrollbar::SmoothScrollbarState;
use crate::state::{App, SessionManagerView};
use ratatui::{
    Frame,
    layout::{Constraint, Direction, Layout, Rect},
    style::{Color, Modifier, Style},
    text::{Line, Span},
    widgets::{Block, Borders, Cell, Paragraph, Row, Table},
};

use super::{
    format_status, format_timestamp, session_detail_tokens_line, status_display, vertical_scrollbar,
};

// ── Session Manager ──────────────────────────────────────────

// Dispatched from the top-level `render()` in render/mod.rs.
pub(super) fn render_session_manager(frame: &mut Frame<'_>, app: &mut App) {
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

    let list_chunks = Layout::default()
        .direction(Direction::Horizontal)
        .constraints([Constraint::Min(1), Constraint::Length(1)])
        .split(inner);

    let max_rows = list_chunks[0].height as usize;
    let total_items = app.session_mgr.sessions.len();
    // The table header occupies one of `max_rows` rows, so only `list_rows`
    // session rows fit below it.  The window and scrollbar math must use
    // this content height — otherwise the highlighted row can sit one row
    // below the last drawn session row.  `window()` is pure (no state
    // mutation during `draw()`), and the returned start doubles as the
    // scrollbar position.
    let list_rows = max_rows.saturating_sub(1);
    let (scroll, _count) = app.session_mgr.window(list_rows);

    if let Some(ref err) = app.session_mgr.error {
        let err_style = Style::default().fg(Color::Red);
        let err_text = format!("Error: {err}");
        let err_para = Paragraph::new(Line::from(Span::styled(err_text, err_style)));
        let err_area = Rect {
            x: list_chunks[0].x + 1,
            y: list_chunks[0].y + 1,
            width: list_chunks[0].width.saturating_sub(2),
            height: 1,
        };
        frame.render_widget(err_para, err_area);
    }

    if total_items == 0 {
        let msg = Paragraph::new("No sessions. Press 'n' to create one.");
        frame.render_widget(msg, list_chunks[0]);
    } else {
        // ── Column layout ────────────────────────────────────────────────
        // The title column is LAST so it absorbs the remaining width via
        // Constraint::Fill(1) — long titles truncate (with an ellipsis)
        // instead of squeezing the fixed columns.  Session and parent ids are
        // fixed-width numeric columns; long ids truncate with an ellipsis.
        let session_w = 8u16;
        let parent_w = 8u16;
        let marker_w = 2u16; // ">" selection + "*" attached markers
        let status_w = 14u16;
        let model_w = 16u16;
        let turns_w = 5u16;
        let modified_w = 11u16;
        let fixed_w = session_w + parent_w + marker_w + status_w + model_w + turns_w + modified_w;
        // The Table adds `column_spacing(1)` between the 8 columns (7 gaps),
        // so the title column gets the remaining width minus those gaps.
        let title_w = list_chunks[0].width.saturating_sub(fixed_w + 7).max(1) as usize;

        let header = Row::new(vec![
            Cell::from(""),
            Cell::from("Session"),
            Cell::from("Parent"),
            Cell::from("Status"),
            Cell::from("Model"),
            Cell::from("Turns"),
            Cell::from("Modified"),
            Cell::from("Title"),
        ])
        .style(
            Style::default()
                .fg(Color::DarkGray)
                .add_modifier(Modifier::BOLD),
        );

        // Only render the visible slice; the scrollbar below reflects the
        // full list length.
        let end = (scroll + list_rows).min(total_items);
        let mut rows = Vec::with_capacity(end.saturating_sub(scroll));
        for i in scroll..end {
            let session = &app.session_mgr.sessions[i];
            let is_selected = Some(i) == app.session_mgr.selection;
            let is_attached = Some(session.session_id) == app.attached_session_id;
            let row_style = if is_selected {
                Style::default().bg(Color::Blue).fg(Color::White)
            } else {
                Style::default()
            };

            let sel = if is_selected { ">" } else { " " };
            let att = if is_attached { "*" } else { " " };
            // Child sessions show their parent's id; top-level sessions get a
            // dash so the column stays readable at a glance.
            let parent = session
                .parent_session_id
                .map_or_else(|| "-".to_string(), |id| id.to_string());
            let (status_text, status_color) = status_display(&session.status);
            let model = session.selected_model.as_deref().unwrap_or("-");
            let model_display =
                if let Some(effort) = session.reasoning_effort.as_deref().filter(|e| *e != "off") {
                    format!("{model} ({effort})")
                } else {
                    model.to_string()
                };
            let modified = format_timestamp(session.last_modified);
            let title = session.title.as_deref().unwrap_or("untitled");

            // Apply the selection style to the whole Row (not to each cell's
            // span) so the highlight background is solid across the entire
            // content width: ratatui paints `Row::style` over the full row
            // area, whereas span-level styles only cover the characters they
            // render.
            let status_cell_style = row_style.fg(status_color);
            rows.push(
                Row::new(vec![
                    Cell::from(format!("{sel}{att}")),
                    Cell::from(truncate_str(
                        &session.session_id.to_string(),
                        session_w as usize,
                    )),
                    Cell::from(truncate_str(&parent, parent_w as usize)),
                    // Keep the status colour even on the highlighted row so
                    // active/inferring sessions stay recognisable at a glance.
                    Cell::from(truncate_str(&status_text, status_w as usize))
                        .style(status_cell_style),
                    Cell::from(truncate_str(&model_display, model_w as usize)),
                    Cell::from(format!(
                        "{:>width$}",
                        session.turn_count,
                        width = turns_w as usize
                    )),
                    Cell::from(truncate_str(&modified, modified_w as usize)),
                    Cell::from(truncate_str(title, title_w)),
                ])
                .style(row_style),
            );
        }

        let table = Table::new(
            rows,
            [
                Constraint::Length(marker_w),
                Constraint::Length(session_w),
                Constraint::Length(parent_w),
                Constraint::Length(status_w),
                Constraint::Length(model_w),
                Constraint::Length(turns_w),
                Constraint::Length(modified_w),
                Constraint::Fill(1),
            ],
        )
        .header(header)
        .column_spacing(1);
        frame.render_widget(table, list_chunks[0]);
    }

    if total_items > list_rows {
        frame.render_stateful_widget(
            vertical_scrollbar(),
            list_chunks[1],
            &mut SmoothScrollbarState::new(total_items)
                .position(scroll)
                .viewport_content_length(list_rows),
        );
    }

    let status = if let Some((_id, title)) = &app.session_mgr.confirm_delete {
        Paragraph::new(Line::from(format!(" Delete \"{title}\"? (y/N)  ")))
    } else {
        Paragraph::new(Line::from(format!(
            " <j/k nav>  <Enter switch>  <i details>  <n new>  <d delete>  <Esc back>  —  {} sessions",
            total_items
        )))
    };
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
                "Reasoning:     {}",
                detail
                    .reasoning_effort
                    .as_deref()
                    .filter(|e| *e != "off")
                    .unwrap_or("off")
            )),
            Line::from(format!(
                "Parent:        {}",
                detail
                    .parent_session_id
                    .map_or("none".to_string(), |id| id.to_string())
            )),
            Line::from(format!("Working Dir:   {}", detail.working_dir)),
            Line::from(format!(
                "Created:       {}",
                format_timestamp(detail.created_at)
            )),
            Line::from(format!(
                "Last Modified: {}",
                format_timestamp(detail.last_modified)
            )),
            Line::from(format!("Turn Count:    {}", detail.turn_count)),
            Line::from(format!(
                "Account:       {}",
                detail.account_name.as_deref().unwrap_or("-")
            )),
            Line::from(match &detail.accumulated_usage {
                Some(usage) => session_detail_tokens_line(usage),
                None => "Tokens:        -".to_string(),
            }),
            Line::from(match (detail.context_window, detail.last_prompt_tokens) {
                (Some(limit), Some(current)) => {
                    let ratio = if limit > 0 {
                        current as f64 / limit as f64
                    } else {
                        0.0
                    };
                    format!(
                        "Context:       {} / {} ({})",
                        humfmt::number(current),
                        humfmt::number(limit),
                        humfmt::percent(ratio),
                    )
                }
                (Some(limit), None) => {
                    format!("Context:       ? / {}", humfmt::number(limit))
                }
                (None, Some(current)) => format!("Context:       {} / ?", humfmt::number(current)),
                (None, None) => "Context:       unknown".to_string(),
            }),
            Line::from(format!("Status:        {}", format_status(&detail.status))),
            Line::from(format!(
                "Tool Groups:   {}",
                humfmt::list(&detail.active_tool_groups)
            )),
        ];
        let paragraph = Paragraph::new(lines);
        frame.render_widget(paragraph, inner);
    }

    let status = Paragraph::new(Line::from(" <b back>  <Enter switch to this session>"));
    frame.render_widget(status, chunks[1]);
}
