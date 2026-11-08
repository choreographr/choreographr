//! Widget-rendering tests, one child module per subject — the boundaries
//! follow the banner sections the monolithic file was already divided into.
//!
//! `format` holds the pure formatters (status line, token readouts and
//! timestamp); `model_selector` covers the model-selection popup; `wizard`
//! covers the account wizard and the credential modal; `session_list` covers
//! the session-list table as one subject — its columns, the selected-row
//! highlight, the three scroll/highlight sections and the archived/pin view;
//! `palette` covers the inline command palette; `tool_rows` pins where a tool
//! row's text ends relative to the scrollbar column; `sinks` pins that
//! untrusted daemon text is normalized at every widget sink it reaches.

mod format;
mod model_selector;
mod palette;
mod session_list;
mod sinks;
mod tool_rows;
mod wizard;

use ratatui::buffer::{Buffer, Cell};

/// Draw one whole frame through `render` into a `TestBackend` and return the
/// drawn cells as a flat string.
///
/// A test reads back the cells the backend actually holds, so the assertion is
/// on what reaches the screen rather than on what a widget was asked to draw.
/// The returned string preserves the buffer's row-major order, so `contains`
/// and `find` splice it exactly as a reader sees the frame; a test that needs
/// cell *styling* or cell *positions* (a background-colour span, a scrollbar
/// column) reads [`drawn_buffer`] instead.
///
/// Shared by nearly every submodule (they all assert on drawn symbols), so it
/// lives here rather than in any one consumer.
pub(super) fn drawn_cells(app: &mut crate::state::App, w: u16, h: u16) -> String {
    drawn_buffer(app, w, h)
        .content()
        .iter()
        .map(Cell::symbol)
        .collect()
}

/// Draw one whole frame through `render` into a `TestBackend` and return the
/// resulting [`Buffer`] for tests that need cell coordinates (a row's rightmost
/// inked column, a specific `(x, y)` symbol) rather than the flat symbol string
/// [`drawn_cells`] hands back.  Both paths draw through the same backend and
/// `render`, so a positional assertion and a `contains` assertion always agree.
pub(super) fn drawn_buffer(app: &mut crate::state::App, w: u16, h: u16) -> Buffer {
    use ratatui::Terminal;
    use ratatui::backend::TestBackend;

    let mut terminal = Terminal::new(TestBackend::new(w, h)).unwrap();
    terminal
        .draw(|frame| crate::render::render(frame, app))
        .expect("render a frame");
    terminal.backend().buffer().clone()
}
