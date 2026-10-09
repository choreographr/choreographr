//! Terminal backend wrapper that works around a ratatui VS16 rendering bug.
//!
//! `ratatui` 0.30.2 (via `ratatui-core` 0.1.2 and `ratatui-crossterm` 0.1.2)
//! draws any row containing an emoji-presentation sequence that carries a
//! variation selector (U+FE0F — `❤️`, `♀️`, `♂️`, `1️⃣`, `⚕️`, `❄️`, `🚀️`, …)
//! one column to the right of where it should be, and can leave a glyph from
//! the previous frame behind on the vacated columns.  Two upstream defects
//! combine:
//!
//! 1. `ratatui-core`'s buffer diff deliberately re-emits a wide VS16 cluster's
//!    reserved trailing cell (to clear terminals that render the cluster one
//!    column wide).
//! 2. `ratatui-crossterm`'s `draw` decides whether to emit a `MoveTo` by
//!    comparing the next cell coordinate with `last_pos.x + 1`, ignoring the
//!    width of the symbol it just printed.  After printing a two-column
//!    grapheme the real cursor sits at `x + 2`, so the reserved cell's write
//!    lands one column too far right, and every later write on the row
//!    inherits the offset.
//!
//! Both are fixed on ratatui `main` (PRs #2686 and #2721, milestone v0.30.3),
//! but no released crate carries the fix yet.  Until one does, wrapping the
//! crossterm backend and dropping that single reserved cell restores correct
//! cursor positioning: with the reserved cell gone, the following cell is no
//! longer judged contiguous, so the backend emits the `MoveTo` it needs.
//!
//! The one behavioural trade-off, inherited by dropping the reserved cell, is
//! that a terminal which renders these clusters one column wide (rather than
//! two) may keep a stale glyph in the column the reserved cell would have
//! cleared.  That is the pre-#2686 behaviour for that narrow terminal class,
//! and far less disruptive than the universal one-column row shift this wrapper
//! removes.  Delete this module — and its wiring in `connection` — when the
//! workspace resolves `ratatui` 0.30.3 / `ratatui-crossterm` 0.1.3.

use ratatui::backend::{Backend, ClearType, CrosstermBackend, WindowSize};
use ratatui::buffer::{Cell, CellWidth};
use ratatui::layout::{Position, Size};
use std::io::{self, Write};

/// The [`Backend`] the TUI drives: a crossterm backend writing to stdout,
/// wrapped to suppress the VS16 reserved-cell artifact (see the module docs).
pub(crate) type TuiBackend = WideGlyphSafeBackend<io::Stdout>;

/// A [`CrosstermBackend`] wrapper that filters the reserved trailing cell of a
/// VS16 emoji cluster out of each frame's diff stream.
///
/// Implements [`Backend`] by delegating everything to the inner backend and
/// only rewriting the cell iterator [`Backend::draw`] receives; also forwards
/// [`Write`] so the caller's `crossterm::execute!` calls on `backend_mut()`
/// keep working unchanged.  Generic over the writer so the filtering can be
/// unit-tested against a capturing `Vec<u8>`.
pub(crate) struct WideGlyphSafeBackend<W: Write>(CrosstermBackend<W>);

impl<W: Write> WideGlyphSafeBackend<W> {
    /// Wrap a crossterm backend over `writer`.
    pub(crate) fn new(writer: W) -> Self {
        Self(CrosstermBackend::new(writer))
    }
}

impl<W: Write> Write for WideGlyphSafeBackend<W> {
    fn write(&mut self, buf: &[u8]) -> io::Result<usize> {
        self.0.write(buf)
    }

    fn flush(&mut self) -> io::Result<()> {
        // Disambiguate from `Backend::flush` (which both `CrosstermBackend` and
        // this wrapper implement).
        Write::flush(&mut self.0)
    }
}

impl<W: Write> Backend for WideGlyphSafeBackend<W> {
    type Error = io::Error;

    fn draw<'a, I>(&mut self, content: I) -> io::Result<()>
    where
        I: Iterator<Item = (u16, u16, &'a Cell)>,
    {
        self.0.draw(SuppressVs16Continuation {
            inner: content,
            prev: None,
        })
    }

    fn hide_cursor(&mut self) -> io::Result<()> {
        self.0.hide_cursor()
    }

    fn show_cursor(&mut self) -> io::Result<()> {
        self.0.show_cursor()
    }

    fn get_cursor_position(&mut self) -> io::Result<Position> {
        self.0.get_cursor_position()
    }

    fn set_cursor_position<P: Into<Position>>(&mut self, position: P) -> io::Result<()> {
        self.0.set_cursor_position(position)
    }

    fn clear(&mut self) -> io::Result<()> {
        self.0.clear()
    }

    fn clear_region(&mut self, clear_type: ClearType) -> io::Result<()> {
        self.0.clear_region(clear_type)
    }

    fn append_lines(&mut self, n: u16) -> io::Result<()> {
        self.0.append_lines(n)
    }

    fn size(&self) -> io::Result<Size> {
        self.0.size()
    }

    fn window_size(&mut self) -> io::Result<WindowSize> {
        self.0.window_size()
    }

    fn flush(&mut self) -> io::Result<()> {
        // Disambiguate from `Write::flush` (which both `CrosstermBackend` and
        // this wrapper implement).
        Backend::flush(&mut self.0)
    }
}

/// Iterator adapter over a backend's `(x, y, cell)` diff stream that drops the
/// reserved trailing cells emitted right after a wide grapheme.
///
/// A cell is dropped only when it is a *blank* cell lying strictly inside the
/// column span of a previously emitted cell that is at least two columns wide —
/// the exact shape of the VS16 reserved cell(s) (`x + 1 .. x + width`).  Any
/// other cell (including a genuine narrow cell that happens to follow a wide
/// one, as the diff's "shrinking wide glyph" path emits, and any cell off the
/// wide glyph's row) is passed through untouched, so nothing but a reserved cell
/// is ever removed.
struct SuppressVs16Continuation<I> {
    inner: I,
    /// `(x, y, width)` of the last cell that was actually yielded.
    prev: Option<(u16, u16, u16)>,
}

impl<'a, I> Iterator for SuppressVs16Continuation<I>
where
    I: Iterator<Item = (u16, u16, &'a Cell)>,
{
    type Item = (u16, u16, &'a Cell);

    fn next(&mut self) -> Option<Self::Item> {
        loop {
            let (x, y, cell) = self.inner.next()?;
            // A reserved cell of a wide grapheme: blank, on the same row as, and
            // strictly inside the column span of, a previously emitted wide
            // cell.  `prev` is deliberately left pointing at that wide cell, so
            // the first real cell past its span still compares against it and is
            // emitted (forcing the backend's `MoveTo`).
            if let Some((prev_x, prev_y, prev_width)) = self.prev
                && prev_width >= 2
                && y == prev_y
                && x > prev_x
                && x < prev_x.saturating_add(prev_width)
                && cell.symbol().chars().all(char::is_whitespace)
            {
                continue;
            }
            self.prev = Some((x, y, cell.cell_width()));
            return Some((x, y, cell));
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::{Arc, Mutex};

    /// Run a synthetic `(x, y, cell)` stream through the adapter and return the
    /// x-coordinates it yields.
    fn filtered_xs(cells: &[(&'static str, u16)]) -> Vec<u16> {
        // Own the cells so the iterator can borrow them; each entry is
        // `(symbol, x)` on row 0.
        let owned: Vec<Cell> = cells.iter().map(|(symbol, _)| Cell::new(symbol)).collect();
        let stream = owned
            .iter()
            .zip(cells)
            .map(|(cell, (_, x))| (*x, 0u16, cell));
        SuppressVs16Continuation {
            inner: stream,
            prev: None,
        }
        .map(|(x, _, _)| x)
        .collect()
    }

    #[test]
    fn drops_reserved_cell_after_a_vs16_wide_cluster() {
        // ❤️ (U+2764 U+FE0F) is a two-column VS16 cluster; the blank at x+1 is
        // its reserved trailing cell and must be dropped so the next cell is no
        // longer seen as contiguous by the crossterm backend.
        let xs = filtered_xs(&[("\u{2764}\u{fe0f}", 0), (" ", 1), ("x", 2)]);
        assert_eq!(xs, vec![0, 2]);
    }

    #[test]
    fn keeps_a_narrow_cell_that_follows_a_wide_glyph() {
        // The diff's "shrinking wide glyph" path emits a real narrow cell right
        // after a previous-frame wide cell; only *blank* cells are the reserved
        // continuation, so this one is kept.
        let xs = filtered_xs(&[("\u{2764}\u{fe0f}", 0), ("x", 1), ("y", 2)]);
        assert_eq!(xs, vec![0, 1, 2]);
    }

    #[test]
    fn keeps_a_blank_cell_that_is_not_adjacent_to_a_wide_glyph() {
        // A plain blank cell after a narrow cell is ordinary content.
        let xs = filtered_xs(&[("a", 0), (" ", 1)]);
        assert_eq!(xs, vec![0, 1]);
    }

    #[test]
    fn keeps_a_non_blank_cell_adjacent_to_a_wide_glyph() {
        // A wide non-VS16 glyph (e.g. CJK) followed by content two columns over
        // is the normal, already-correct case; nothing is dropped.
        let xs = filtered_xs(&[("字", 0), ("b", 2)]);
        assert_eq!(xs, vec![0, 2]);
    }

    #[test]
    fn keeps_a_blank_cell_on_a_different_row_from_a_wide_glyph() {
        // A blank at the head of the next row is not inside the wide glyph's
        // span, so it is content, not a reserved cell.
        let wide = Cell::new("\u{2764}\u{fe0f}");
        let blank = Cell::new(" ");
        let stream = vec![(0u16, 0u16, &wide), (0u16, 1u16, &blank)];
        let xs: Vec<u16> = SuppressVs16Continuation {
            inner: stream.into_iter(),
            prev: None,
        }
        .map(|(x, _, _)| x)
        .collect();
        assert_eq!(xs, vec![0, 0]);
    }

    /// A `Write` sink whose bytes are readable from every clone, so a backend
    /// built over it can be inspected after `draw`.
    #[derive(Clone, Default)]
    struct Capture(Arc<Mutex<Vec<u8>>>);

    impl Write for Capture {
        fn write(&mut self, buf: &[u8]) -> io::Result<usize> {
            self.0.lock().expect("capture lock").extend_from_slice(buf);
            Ok(buf.len())
        }

        fn flush(&mut self) -> io::Result<()> {
            Ok(())
        }
    }

    /// End-to-end guard over the *real* `CrosstermBackend` and a *real* buffer
    /// diff (the exact path `Terminal::draw` drives), reproducing the scenario
    /// from ratatui issue #2651: a row of `a`s replaced by a single `❤️`
    /// (U+2764 U+FE0F, a two-column VS16 cluster).  Without the workaround the
    /// reserved cell and every later cell are written with no cursor move, so the
    /// whole row lands one column too far right; with it, the first cell past the
    /// cluster is preceded by an explicit move to column 3 (1-based).
    #[test]
    fn emits_a_cursor_move_past_a_vs16_cluster_so_the_row_does_not_shift() {
        use ratatui::buffer::Buffer;
        use ratatui::layout::Rect;
        use ratatui::style::Style;

        let area = Rect::new(0, 0, 10, 1);
        let mut prev = Buffer::empty(area);
        prev.set_string(0, 0, "aaaaaaaaaa", Style::default());
        let mut next = Buffer::empty(area);
        next.set_string(0, 0, "\u{2764}\u{fe0f}", Style::default());

        let capture = Capture::default();
        let mut backend = WideGlyphSafeBackend::new(capture.clone());
        backend
            .draw(prev.diff_iter(&next))
            .expect("draw must succeed");

        let bytes = capture.0.lock().expect("capture lock").clone();
        let rendered = String::from_utf8_lossy(&bytes);
        assert!(
            rendered.contains("\u{1b}[1;3H"),
            "the cell after the cluster must be preceded by a cursor move to \
             column 3, otherwise the row shifts right; got {rendered:?}"
        );
    }
}
