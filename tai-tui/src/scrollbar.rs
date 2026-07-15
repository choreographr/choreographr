use ratatui::buffer::Buffer;
use ratatui::layout::Rect;
use ratatui::style::{Color, Style};
use ratatui::widgets::StatefulWidget;

/// A vertical scrollbar whose thumb height is proportional to the
/// fraction of content visible in the viewport.
///
/// The thumb is rendered at half-block sub-cell precision using
/// `▀`/`▄`/`█` Unicode block characters.  When almost all content fits
/// the viewport the thumb fills most of the track; when only a small
/// fraction is visible the thumb shrinks to a minimum of one cell.
///
/// Clicking on the track jumps to that position. Dragging the thumb
/// is supported via the `scrollbar_dragging` field on `App`.
#[derive(Debug, Clone)]
pub(crate) struct SmoothScrollbar {
    thumb_fg: Option<Color>,
    track_bg: Option<Color>,
}

/// State for [`SmoothScrollbar`].
///
/// Mirrors ratatui's `ScrollbarState` in API so the two can be
/// swapped with minimal diff.
#[derive(Debug, Default, Clone, Copy, Eq, PartialEq, Hash)]
pub(crate) struct SmoothScrollbarState {
    content_length: usize,
    position: usize,
    viewport_content_length: usize,
}

impl SmoothScrollbar {
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

impl SmoothScrollbarState {
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

impl StatefulWidget for SmoothScrollbar {
    type State = SmoothScrollbarState;

    fn render(self, area: Rect, buf: &mut Buffer, state: &mut Self::State) {
        let track_height = area.height as usize;
        if track_height == 0 || state.content_length == 0 {
            return;
        }

        let scroll_range = state
            .content_length
            .saturating_sub(state.viewport_content_length)
            .max(1);

        let virtual_track = 2 * track_height;

        // Thumb height in virtual half-cell slots, proportional to
        // the fraction of content visible in the viewport, clamped
        // to at least 1 terminal cell (2 slots).
        let thumb_slots = (state.viewport_content_length * virtual_track / state.content_length)
            .clamp(2, virtual_track);

        // The thumb can slide through the remaining virtual space.
        let virtual_scroll_range = virtual_track - thumb_slots;

        // Start position of the thumb (in virtual slots) mapped from
        // the content-scroll position.
        let thumb_start =
            (state.position * virtual_scroll_range / scroll_range).min(virtual_scroll_range);

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

            let thumb_end = thumb_start + thumb_slots;
            let top_in_thumb = top_slot >= thumb_start && top_slot < thumb_end;
            let bot_in_thumb = bot_slot >= thumb_start && bot_slot < thumb_end;

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
        let scrollbar = SmoothScrollbar::new()
            .thumb_fg(Color::Gray)
            .track_bg(Color::DarkGray);
        let mut state = SmoothScrollbarState::new(content_length)
            .position(position)
            .viewport_content_length(viewport_content_length);
        scrollbar.render(buf.area, &mut buf, &mut state);
        (0..height)
            .map(|i| buf.cell((0, i)).unwrap().symbol().to_string())
            .collect()
    }

    #[test]
    fn thumb_at_top_when_position_zero() {
        // H=5, content=10, viewport=3 → virtual_track=10,
        //   thumb_slots = 3*10/10 = 3, virtual_scroll_range=7,
        //   scroll_range=7
        // position=0 → thumb_start = 0*7/7 = 0
        // virtual [0,3) → row 0: slots [0,1] → "█"
        //              → row 1: slots [2,3) → top (2) in, bot (3) not → "▀"
        //              → rows 2..4: neither → " "
        let symbols = render_to_symbols(5, 10, 3, 0);
        assert_eq!(symbols[0], "█", "row 0 should be full thumb");
        assert_eq!(symbols[1], "▀", "row 1 should be upper half");
        for i in 2..5 {
            assert_eq!(symbols[i], " ", "row {i} should be track");
        }
    }

    #[test]
    fn thumb_at_bottom_when_position_at_max() {
        // H=5, content=10, viewport=3 → virtual_track=10,
        //   thumb_slots=3, virtual_scroll_range=7, scroll_range=7
        // position=7 → thumb_start = 7*7/7 = 7
        // virtual [7,10) → row 3: slot 6 not in, slot 7 in → "▄"
        //               → row 4: slots [8,9] → "█"
        let symbols = render_to_symbols(5, 10, 3, 7);
        assert_eq!(symbols[3], "▄", "row 3 should be lower half");
        assert_eq!(symbols[4], "█", "row 4 should be full thumb");
        for i in 0..3 {
            assert_eq!(symbols[i], " ", "row {i} should be track");
        }
    }

    #[test]
    fn thumb_mid_position() {
        // H=5, content=10, viewport=1 → thumb_slots=2 (min),
        //   virtual_scroll_range=8, scroll_range=9
        // position=4 → thumb_start = 4*8/9 = 3
        // virtual [3,5) → row 1: bottom half (slot 3)  → "▄"
        //              → row 2: top half (slot 4)     → "▀"
        let symbols = render_to_symbols(5, 10, 1, 4);
        assert_eq!(symbols[1], "▄", "row 1 should be lower half (slot 3)");
        assert_eq!(symbols[2], "▀", "row 2 should be upper half (slot 4)");
    }

    #[test]
    fn subcell_precision_halfway() {
        // H=5, content=10, viewport=1 → thumb_slots=2 (min),
        //   virtual_scroll_range=8, scroll_range=9
        // position=3 → thumb_start = 3*8/9 = 2
        // virtual [2,4) → row 1: both halves (slots 2,3) → "█"
        let symbols = render_to_symbols(5, 10, 1, 3);
        assert_eq!(symbols[1], "█", "row 1 should be full thumb");
    }

    #[test]
    fn empty_content_renders_nothing() {
        let mut buf = Buffer::empty(Rect::new(0, 0, 1, 5));
        let mut state = SmoothScrollbarState::new(0);
        SmoothScrollbar::new().render(buf.area, &mut buf, &mut state);
        for i in 0..5u16 {
            assert_eq!(buf.cell((0, i)).unwrap().symbol(), " ");
        }
    }

    #[test]
    fn zero_height_does_not_panic() {
        let mut buf = Buffer::empty(Rect::new(0, 0, 1, 0));
        let mut state = SmoothScrollbarState::new(10)
            .position(5)
            .viewport_content_length(3);
        SmoothScrollbar::new().render(buf.area, &mut buf, &mut state);
    }

    #[test]
    fn content_fills_track_when_viewport_matches_content() {
        // content=10, viewport=10 → thumb_slots = 10*10/10 = 10
        // (fills the whole 5-cell track).  scroll_range = max(1) = 1.
        let symbols = render_to_symbols(5, 10, 10, 0);
        for i in 0..5 {
            assert_eq!(symbols[i as usize], "█", "row {i} should be full thumb");
        }
    }

    #[test]
    fn thumb_size_scales_with_viewport_ratio() {
        // H=10, content=100, viewport=50 → 50% visible.
        //   thumb_slots = 50*20/100 = 10 (5 cells out of 10).
        let symbols = render_to_symbols(10, 100, 50, 0);
        // First 5 rows are thumb (10 virtual slots = 5 cells).
        for i in 0..5 {
            assert_eq!(symbols[i], "█", "row {i} should be full thumb");
        }
        for i in 5..10 {
            assert_eq!(symbols[i], " ", "row {i} should be track");
        }
    }
}
