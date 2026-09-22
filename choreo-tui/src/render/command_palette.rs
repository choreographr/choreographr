//! The inline command palette overlay (Chat page): a keyboard-only popup that
//! floats immediately ABOVE the command input box while the user types a
//! `/`-query.  It lists the matching commands from the shared catalog (see
//! `choreo_client_core::command_catalog`), highlighting the matched prefix and
//! showing each command's logical shortcut on the right.
//!
//! The palette is deliberately anchored to the input box (`input_box_rect`) so
//! it can never cover the input box or the bottom status bar — it only ever
//! overlaps the help/status rows above them, erased with `Clear`.
use crate::markdown_render::display_width;
use crate::state::{App, picker_window, shortcut_label_for};
use choreo_client_core::CommandMatch;
use ratatui::{
    Frame,
    layout::Rect,
    style::{Color, Modifier, Style},
    text::{Line, Span},
    widgets::{Clear, Paragraph},
};

/// Maximum number of rows the palette draws.  Keeps it a floating hint rather
/// than a full-screen list; the catalog is longer, so the tail is summarized
/// with a `+N more` row.
pub(super) const COMMAND_PALETTE_MAX_ROWS: usize = 8;

/// Draw the inline command palette above the command input box.
///
/// `input` is the command input box `Rect` the caller (`render_chat`) already
/// laid out — passed in so the palette never recomputes the Chat-page layout.
///
/// No-op (draws nothing) when the palette has no matches, when there is no room
/// above the input box (`input.y == 0`), or on a zero-area box.
pub(super) fn render_command_palette(frame: &mut Frame<'_>, app: &mut App, input: Rect) {
    let matches = app.command_palette_matches();
    if matches.is_empty() {
        return;
    }

    // Anchor to the command input box: the palette's bottom edge sits on the
    // box's top border and shares its left edge and width, so it never covers
    // the input box or the status bar beneath it.  `input` is the same shared
    // geometry `render_chat` laid out, so it tracks the box even on tiny
    // terminals where the layout solver relocates it.
    if input.y == 0 {
        // The box is at the very top of the screen: drawing the palette above
        // it would either cover the box or fall off-screen, so draw nothing.
        return;
    }

    let len = matches.len();
    let height = len.min(COMMAND_PALETTE_MAX_ROWS);
    if height == 0 {
        return;
    }
    // `height` is at most `COMMAND_PALETTE_MAX_ROWS` (8), so the narrowing
    // casts to the `u16` rect dimensions cannot truncate.
    #[allow(clippy::cast_possible_truncation)]
    let rect = Rect {
        x: input.x,
        y: input.y.saturating_sub(height as u16),
        width: input.width,
        height: height as u16,
    };
    if rect.width == 0 || rect.height == 0 {
        return;
    }

    // Erase the region beneath so the palette reads as a solid overlay rather
    // than text drawn on top of the help/history rows.
    frame.render_widget(Clear, rect);

    // When the list is truncated the final row is a `+N more` hint, leaving
    // one fewer row for items; the window arithmetic (shared with the picker
    // popups) then keeps the highlighted item visible within that smaller
    // window.
    let truncated = len > height;
    let item_rows = if truncated { height - 1 } else { height };
    let (scroll, count) = picker_window(
        app.command_palette_scroll(),
        app.command_palette_focused(),
        len,
        item_rows,
    );
    let focused = app.command_palette_focused();

    let mut lines: Vec<Line> = Vec::with_capacity(height);
    for (i, command_match) in matches.iter().enumerate().skip(scroll).take(count) {
        lines.push(palette_row(command_match, i == focused, rect.width));
    }
    if truncated {
        let hidden = len - (scroll + count);
        lines.push(Line::from(Span::styled(
            format!(" +{hidden} more"),
            Style::default().fg(Color::DarkGray),
        )));
    }

    frame.render_widget(Paragraph::new(lines), rect);
}

/// Build one palette row: `"{marker}/{name}  {summary}"` with the matched
/// prefix characters emphasized, plus a right-aligned dim shortcut label when
/// the command has one.
///
/// `marker` is `>` on the focused row (else a space), matching the picker
/// popups' highlight convention.
fn palette_row(command_match: &CommandMatch, focused: bool, width: u16) -> Line<'static> {
    // Focused rows read Yellow+BOLD (the picker-popup convention); other rows
    // are White.  Matched prefix characters are emphasized brighter (Cyan+BOLD)
    // so the leading characters the query matched stand out from the rest of
    // the name.
    let base = if focused {
        Style::default()
            .fg(Color::Yellow)
            .add_modifier(Modifier::BOLD)
    } else {
        Style::default().fg(Color::White)
    };
    let matched = Style::default()
        .fg(Color::Cyan)
        .add_modifier(Modifier::BOLD);
    let summary_style = Style::default().fg(Color::DarkGray);

    let name = command_match.spec.name;
    let marker = if focused { '>' } else { ' ' };
    let mut spans: Vec<Span> = Vec::with_capacity(name.len() + 3);
    spans.push(Span::styled(marker.to_string(), base));
    spans.push(Span::styled("/", base));
    for (i, ch) in name.chars().enumerate() {
        let style = if command_match.name_positions.contains(&i) {
            matched
        } else {
            base
        };
        spans.push(Span::styled(ch.to_string(), style));
    }
    spans.push(Span::styled(
        format!("  {}", command_match.spec.summary),
        summary_style,
    ));

    // Right-align the dim shortcut label against the palette's right edge.  The
    // content width is measured from the same text the spans emit so the label
    // lands flush right (and is simply omitted when the row is too narrow).
    let content = format!("{marker}/{name}  {}", command_match.spec.summary);
    if let Some(label) = shortcut_label_for(name) {
        let used = display_width(&content);
        let label_width = display_width(&label);
        if (width as usize) > used + label_width + 1 {
            let pad = width as usize - used - label_width;
            spans.push(Span::raw(" ".repeat(pad)));
            spans.push(Span::styled(label, summary_style));
        }
    }

    Line::from(spans)
}
