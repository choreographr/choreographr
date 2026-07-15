use ratatui::buffer::Buffer;
use ratatui::layout::Rect;
use ratatui::style::{Color, Style};
use ratatui::widgets::StatefulWidget;

/// A vertical scrollbar with a fixed 1-cell-high thumb rendered at
/// half-block sub-cell precision.
///
/// The thumb is always 1 terminal cell tall but can start at any
/// half-cell boundary, producing smooth sub-cell movement using
/// `▀`/`▄`/`█` Unicode block characters.
///
/// Clicking on the track jumps to that position. Dragging the thumb
/// is supported via the `scrollbar_dragging` field on `App`.
#[derive(Debug, Clone)]
pub(crate) struct FixedScrollbar {
    thumb_fg: Option<Color>,
    track_bg: Option<Color>,
}

/// State for [`FixedScrollbar`].
///
/// Mirrors ratatui's `ScrollbarState` in API so the two can be
/// swapped with minimal diff.
#[derive(Debug, Default, Clone, Copy, Eq, PartialEq, Hash)]
pub(crate) struct FixedScrollbarState {
    content_length: usize,
    position: usize,
    viewport_content_length: usize,
}

impl FixedScrollbar {
    pub(crate) const fn new() -> Self {
        Self {
            thumb_fg: None,
            track_bg: None,
        }
    }

    /// Set the foreground color for the thumb (including half-block
    /// characters where visible).
    pub(crate) const fn thumb_fg(mut self, color: Color) -> Self {
        self.thumb_fg = Some(color);
        self
    }

    /// Set the background color for the track (the area behind the
    /// thumb).
    pub(crate) const fn track_bg(mut self, color: Color) -> Self {
        self.track_bg = Some(color);
        self
    }
}

impl FixedScrollbarState {
    pub(crate) const fn new(content_length: usize) -> Self {
        Self {
            content_length,
            position: 0,
            viewport_content_length: 0,
        }
    }

    pub(crate) const fn position(mut self, position: usize) -> Self {
        self.position = position;
        self
    }

    pub(crate) const fn viewport_content_length(mut self, len: usize) -> Self {
        self.viewport_content_length = len;
        self
    }
}

impl StatefulWidget for FixedScrollbar {
    type State = FixedScrollbarState;

    fn render(self, area: Rect, buf: &mut Buffer, state: &mut Self::State) {
        let track_height = area.height as usize;
        if track_height == 0 || state.content_length == 0 {
            return;
        }

        let scroll_range = state
            .content_length
            .saturating_sub(state.viewport_content_length)
            .max(1);

        // 2× virtual resolution — each terminal cell has two
        // half-cell slots (top and bottom). The thumb occupies
        // exactly 2 virtual units (1 cell) and can start at any
        // half-cell boundary for smooth sub-cell motion.
        let max_virtual = 2 * track_height - 2;
        let thumb_start = (state.position * 2 * track_height / scroll_range).min(max_virtual);

        // Build the three style variants from the optional colors.
        let mut full_style = Style::default();
        let mut half_style = Style::default();
        let mut track_style = Style::default();
        if let Some(fg) = self.thumb_fg {
            full_style = full_style.fg(fg);
            half_style = half_style.fg(fg);
        }
        if let Some(bg) = self.track_bg {
            half_style = half_style.bg(bg);
            track_style = track_style.bg(bg);
        }

        for i in 0..track_height {
            let top_slot = 2 * i;
            let bot_slot = 2 * i + 1;

            let top_in_thumb = top_slot >= thumb_start && top_slot < thumb_start + 2;
            let bot_in_thumb = bot_slot >= thumb_start && bot_slot < thumb_start + 2;

            let y = area.y + i as u16;
            let x = area.x;

            match (top_in_thumb, bot_in_thumb) {
                (true, true) => {
                    // Entire cell covered by thumb.
                    buf.set_string(x, y, "█", full_style);
                }
                (true, false) => {
                    // Only the top half is covered.
                    buf.set_string(x, y, "▀", half_style);
                }
                (false, true) => {
                    // Only the bottom half is covered.
                    buf.set_string(x, y, "▄", half_style);
                }
                (false, false) => {
                    // No thumb coverage — plain track cell.
                    buf.set_string(x, y, " ", track_style);
                }
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Create a 1-wide × H-tall buffer, render the scrollbar, and
    /// return each row's symbol as a `Vec<&str>`.
    fn render_to_symbols(
        height: u16,
        content_length: usize,
        viewport_content_length: usize,
        position: usize,
    ) -> Vec<String> {
        let mut buf = Buffer::empty(Rect::new(0, 0, 1, height));
        let scrollbar = FixedScrollbar::new()
            .thumb_fg(Color::Gray)
            .track_bg(Color::DarkGray);
        let mut state = FixedScrollbarState::new(content_length)
            .position(position)
            .viewport_content_length(viewport_content_length);
        scrollbar.render(buf.area, &mut buf, &mut state);
        (0..height)
            .map(|i| buf.cell((0, i)).unwrap().symbol().to_string())
            .collect()
    }

    #[test]
    fn thumb_at_top_when_position_zero() {
        // H=5, content=10, viewport=3 → scroll_range=7
        // position=0 → thumb_start = 0*2*5/7 = 0
        // virtual [0,2) → row 0: both halves → "█"
        //              → rows 1..4: neither → " "
        let symbols = render_to_symbols(5, 10, 3, 0);
        assert_eq!(symbols[0], "█", "thumb should be at row 0");
        for i in 1..5 {
            assert_eq!(symbols[i], " ", "row {i} should be track");
        }
    }

    #[test]
    fn thumb_at_bottom_when_position_at_max() {
        // H=5, content=10, viewport=3 → scroll_range=7
        // position=7 → thumb_start = min(7*2*5/7=10, 8) = 8
        // virtual [8,10) → row 4: both halves → "█"
        let symbols = render_to_symbols(5, 10, 3, 7);
        assert_eq!(symbols[4], "█", "thumb should be at row 4");
        for i in 0..4 {
            assert_eq!(symbols[i], " ", "row {i} should be track");
        }
    }

    #[test]
    fn thumb_mid_position() {
        // H=5, content=10, viewport=1 → scroll_range=9
        // position=4 → thumb_start = 4*2*5/9 = 4
        // virtual [4,6) → row 2 covers slots 4,5 → "█"
        let symbols = render_to_symbols(5, 10, 1, 4);
        assert_eq!(symbols[2], "█", "thumb should be at row 2");
    }

    #[test]
    fn subcell_precision_halfway() {
        // H=5, content=10, viewport=1 → scroll_range=9
        // position=3 → thumb_start = 3*2*5/9 = 3 (truncated)
        // virtual [3,5) → row 1: bottom half (slot 3)  → "▄"
        //              → row 2: top half (slot 4)     → "▀"
        let symbols = render_to_symbols(5, 10, 1, 3);
        assert_eq!(symbols[1], "▄", "row 1 should be lower half (slot 3)");
        assert_eq!(symbols[2], "▀", "row 2 should be upper half (slot 4)");
    }

    #[test]
    fn empty_content_renders_nothing() {
        let mut buf = Buffer::empty(Rect::new(0, 0, 1, 5));
        let mut state = FixedScrollbarState::new(0);
        FixedScrollbar::new().render(buf.area, &mut buf, &mut state);
        for i in 0..5u16 {
            assert_eq!(buf.cell((0, i)).unwrap().symbol(), " ");
        }
    }

    #[test]
    fn zero_height_does_not_panic() {
        let mut buf = Buffer::empty(Rect::new(0, 0, 1, 0));
        let mut state = FixedScrollbarState::new(10)
            .position(5)
            .viewport_content_length(3);
        FixedScrollbar::new().render(buf.area, &mut buf, &mut state);
    }

    #[test]
    fn content_fits_viewport() {
        // When content_length == viewport_content_length, scroll_range == 1.
        // position=0 → thumb_start = 0 → row 0: full thumb.
        let symbols = render_to_symbols(5, 10, 10, 0);
        assert_eq!(symbols[0], "█");
    }
}
