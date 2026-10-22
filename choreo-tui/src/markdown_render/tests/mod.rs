mod ansi;
mod blockquote;
mod code_box;
mod copy_metadata;
mod incremental;
mod inline;
mod lists;
mod math;
mod punctuation;
mod reasoning;
mod syntax;
mod tables;
mod text;
mod tool_results;
mod tools;
mod turn;

/// Number of leading space characters in a rendered line (0 when it does
/// not start with spaces).
pub(super) fn leading_spaces(line: &str) -> usize {
    line.chars().take_while(|ch| *ch == ' ').count()
}

/// Column (byte index, ASCII-only test input) where the first non-marker
/// text of a rendered list line begins — i.e. where the content starts.
pub(super) fn first_content_column(line: &str) -> usize {
    line.char_indices()
        .find(|(_, ch)| ch.is_alphabetic())
        .map_or(0, |(idx, _)| idx)
}

/// Draw `lines` into a `TestBackend` through the same non-wrapping
/// `Paragraph` the chat history uses, returning one `String` per rendered row.
///
/// Drawing is what the code box's alignment actually depends on: the rows' own
/// `Line::width` values agree even when the *drawn* cells do not (a control
/// character measures as one column and draws as none), so a width-only
/// assertion cannot catch a desync between measurement and the screen. The
/// samples these tests feed it are ASCII, so a char index in a returned row is
/// also its display column.
pub(super) fn draw_rows(lines: &[ratatui::text::Line<'static>], width: u16) -> Vec<String> {
    use ratatui::Terminal;
    use ratatui::backend::TestBackend;
    use ratatui::widgets::Paragraph;

    let height = u16::try_from(lines.len()).expect("row count fits a viewport");
    let mut terminal = Terminal::new(TestBackend::new(width, height)).expect("test terminal");
    let lines = lines.to_vec();
    terminal
        .draw(|frame| frame.render_widget(Paragraph::new(lines), frame.area()))
        .expect("draw the paragraph");
    let buffer = terminal.backend().buffer();
    (0..height)
        .map(|y| {
            (0..width)
                .map(|x| buffer[(x, y)].symbol())
                .collect::<String>()
        })
        .collect()
}
