//! Widget-rendering tests, one child module per subject — the boundaries
//! follow the banner sections the monolithic file was already divided into.
//!
//! `format` holds the pure formatters (status line, token readouts and
//! timestamp); `model_selector` covers the model-selection popup; `wizard`
//! covers the account wizard and the credential modal; `session_list` covers
//! the session-list table as one subject — its columns, the selected-row
//! highlight, the three scroll/highlight sections and the archived/pin view;
//! `palette` covers the inline command palette; `sinks` pins that untrusted
//! daemon text is normalized at every widget sink it reaches.

mod format;
mod model_selector;
mod palette;
mod session_list;
mod sinks;
mod wizard;

use ratatui::buffer::Cell;

/// Draw one whole frame through `render` into a `TestBackend` and return the
/// drawn cells as a flat string.
///
/// A test reads back the cells the backend actually holds, so the assertion is
/// on what reaches the screen rather than on what a widget was asked to draw.
/// The returned string preserves the buffer's row-major order, so `contains`
/// and `find` splice it exactly as a reader sees the frame; a test that needs
/// cell *styling* rather than symbols (a background-colour span) reads the
/// buffer directly instead.
///
/// Shared by nearly every submodule (they all assert on drawn symbols), so it
/// lives here rather than in any one consumer.
pub(super) fn drawn_cells(app: &mut crate::state::App, w: u16, h: u16) -> String {
    use ratatui::Terminal;
    use ratatui::backend::TestBackend;

    let mut terminal = Terminal::new(TestBackend::new(w, h)).unwrap();
    terminal
        .draw(|frame| crate::render::render(frame, app))
        .expect("render a frame");
    terminal
        .backend()
        .buffer()
        .content()
        .iter()
        .map(Cell::symbol)
        .collect()
}
