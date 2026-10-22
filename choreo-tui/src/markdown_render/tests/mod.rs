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

/// Run `check` over every Unicode scalar value, sharded across the available
/// cores. The code-space sweep below is exhaustive by design (it pins the TUI's
/// per-char terminal keep policy against the shared spoofing predicate for
/// *every* char) and pure CPU; sharding keeps it exhaustive while cutting its
/// wall time to roughly one core's share. A panic in any shard propagates out
/// of `scope` once every thread has joined.
pub(super) fn sweep_code_space(check: impl Fn(char) + Sync) {
    const TOTAL: u32 = 0x11_0000; // Unicode scalar range, excluding surrogates.
    let threads = std::thread::available_parallelism().map_or(1, std::num::NonZero::get);
    let shard = TOTAL.div_ceil(u32::try_from(threads).unwrap_or(1)).max(1);
    std::thread::scope(|scope| {
        let mut start = 0u32;
        while start < TOTAL {
            let end = (start + shard).min(TOTAL);
            let check = &check;
            scope.spawn(move || {
                for cp in start..end {
                    if let Some(c) = char::from_u32(cp) {
                        check(c);
                    }
                }
            });
            start = end;
        }
    });
}

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
