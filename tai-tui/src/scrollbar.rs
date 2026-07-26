use std::collections::HashSet;

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
///
/// User-text markers (green indicator dots on the track) are rendered
/// from pre-computed virtual-slot positions passed via [`with_markers`].
#[derive(Debug, Clone)]
pub(crate) struct SmoothScrollbar {
    thumb_fg: Option<Color>,
    track_bg: Option<Color>,
    marker_fg: Option<Color>,
    /// Pre-computed virtual-slot positions where user-text markers
    /// appear on the track.  Computed at render time from content-line
    /// positions and the virtual-track scale.
    markers: HashSet<usize>,
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
    pub(crate) fn new() -> Self {
        Self {
            thumb_fg: None,
            track_bg: None,
            marker_fg: None,
            markers: HashSet::new(),
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

    /// Set the foreground color for user-text markers on the track.
    pub(crate) const fn marker_fg(mut self, color: Color) -> Self {
        self.marker_fg = Some(color);
        self
    }

    /// Attach pre-computed virtual-slot positions where user-text
    /// markers should appear on the track.
    pub(crate) fn with_markers(mut self, markers: &[usize]) -> Self {
        self.markers.clear();
        self.markers.extend(markers.iter().copied());
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
        // to at least 1 half-block (1 slot).
        let thumb_slots = (state.viewport_content_length * virtual_track / state.content_length)
            .clamp(1, virtual_track);

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

        // Marker style: marker_fg on the filled half, track_bg on the
        // unfilled half so the un-filled portion inherits the track.
        let marker_fg = self.marker_fg;
        let track_bg = self.track_bg;

        for i in 0..track_height {
            let top_slot = 2 * i;
            let bot_slot = 2 * i + 1;

            let thumb_end = thumb_start + thumb_slots;
            let top_in_thumb = top_slot >= thumb_start && top_slot < thumb_end;
            let bot_in_thumb = bot_slot >= thumb_start && bot_slot < thumb_end;

            let y = area.y + i as u16;
            let x = area.x;

            let marker_top = self.markers.contains(&top_slot);
            let marker_bot = self.markers.contains(&bot_slot);

            match (marker_top, marker_bot) {
                // Both halves are markers.
                (true, true) => {
                    let s = style_from_opts(marker_fg, track_bg);
                    buf.set_string(x, y, "█", s);
                }
                // Only the top half is a marker.
                (true, false) => {
                    if bot_in_thumb {
                        let s = style_from_opts(marker_fg, self.thumb_fg);
                        buf.set_string(x, y, "▀", s);
                    } else {
                        let s = style_from_opts(marker_fg, track_bg);
                        buf.set_string(x, y, "▀", s);
                    }
                }
                // Only the bottom half is a marker.
                (false, true) => {
                    if top_in_thumb {
                        let s = style_from_opts(marker_fg, self.thumb_fg);
                        buf.set_string(x, y, "▄", s);
                    } else {
                        let s = style_from_opts(marker_fg, track_bg);
                        buf.set_string(x, y, "▄", s);
                    }
                }
                // No marker — original thumb/track logic.
                (false, false) => match (top_in_thumb, bot_in_thumb) {
                    (true, true) => {
                        buf.set_string(x, y, "█", full_style);
                    }
                    (true, false) => {
                        buf.set_string(x, y, "▀", half_style);
                    }
                    (false, true) => {
                        buf.set_string(x, y, "▄", half_style);
                    }
                    (false, false) => {
                        buf.set_string(x, y, " ", track_style);
                    }
                },
            }
        }
    }
}

/// Build a Style with an optional foreground and background.
/// When an option is `None` the default (inherited) value is used,
/// avoiding hardcoded fallbacks that could diverge from builder-set
/// colors elsewhere.
fn style_from_opts(fg: Option<Color>, bg: Option<Color>) -> Style {
    let mut s = Style::default();
    if let Some(fg) = fg {
        s = s.fg(fg);
    }
    if let Some(bg) = bg {
        s = s.bg(bg);
    }
    s
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
        // H=5, content=10, viewport=1 → thumb_slots=1 (min),
        //   virtual_scroll_range=9, scroll_range=9
        // position=4 → thumb_start = 4*9/9 = 4
        // virtual [4,5) → row 2: top half (slot 4) → "▀"
        let symbols = render_to_symbols(5, 10, 1, 4);
        assert_eq!(symbols[2], "▀", "row 2 should be upper half (slot 4)");
        for i in 0..5 {
            if i != 2 {
                assert_eq!(symbols[i], " ", "row {i} should be track");
            }
        }
    }

    #[test]
    fn subcell_precision_halfway() {
        // H=5, content=10, viewport=1 → thumb_slots=1 (min),
        //   virtual_scroll_range=9, scroll_range=9
        // position=3 → thumb_start = 3*9/9 = 3
        // virtual [3,4) → row 1: bottom half (slot 3) → "▄"
        let symbols = render_to_symbols(5, 10, 1, 3);
        assert_eq!(symbols[1], "▄", "row 1 should be lower half (slot 3)");
        for i in 0..5 {
            if i != 1 {
                assert_eq!(symbols[i], " ", "row {i} should be track");
            }
        }
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

    // ── Marker tests ─────────────────────────────────────────────

    /// Like `render_to_symbols` but with markers and marker_fg set.
    /// Markers are pre-computed virtual-slot positions.
    fn render_to_symbols_with_markers(
        height: u16,
        content_length: usize,
        viewport_content_length: usize,
        position: usize,
        marker_slots: &[usize],
    ) -> Vec<String> {
        let mut buf = Buffer::empty(Rect::new(0, 0, 1, height));
        let scrollbar = SmoothScrollbar::new()
            .thumb_fg(Color::Gray)
            .track_bg(Color::DarkGray)
            .marker_fg(Color::Green)
            .with_markers(marker_slots);
        let mut state = SmoothScrollbarState::new(content_length)
            .position(position)
            .viewport_content_length(viewport_content_length);
        scrollbar.render(buf.area, &mut buf, &mut state);
        (0..height)
            .map(|i| buf.cell((0, i)).unwrap().symbol().to_string())
            .collect()
    }

    #[test]
    fn marker_shows_on_track_when_not_covered_by_thumb() {
        // H=5, content=10, viewport=3, position=0
        //   virtual_track=10, thumb_slots=3, virtual_scroll_range=7,
        //   scroll_range=7, thumb_start=0 → thumb covers [0,3) → rows 0-1
        // Marker at line 5 → virtual slot 5*10/10=5 → row 2 bottom half.
        let symbols = render_to_symbols_with_markers(5, 10, 3, 0, &[5]);
        assert_eq!(symbols[0], "█", "row 0 thumb");
        assert_eq!(symbols[1], "▀", "row 1 thumb upper half");
        assert_eq!(symbols[2], "▄", "row 2 should be marker lower half");
        assert_eq!(symbols[3], " ", "row 3 track");
        assert_eq!(symbols[4], " ", "row 4 track");
    }

    #[test]
    fn marker_visible_when_covered_by_thumb() {
        // Same layout — marker at line 0 falls in virtual slot 0
        // which is inside the thumb range [0,3).
        //   Row 0: top_slot=0 (marker + top thumb), bot_slot=1 (thumb only)
        //     → marker_top=true → "▀" fg=green bg=thumb (marker over thumb)
        //   Row 1: top_slot=2 (thumb only), bot_slot=3 (track)
        //     → "▀" half_style (original thumb behavior)
        let symbols = render_to_symbols_with_markers(5, 10, 3, 0, &[0]);
        assert_eq!(symbols[0], "▀", "row 0 marker top half over thumb");
        assert_eq!(symbols[1], "▀", "row 1 thumb upper half");
    }

    #[test]
    fn multiple_markers_across_track() {
        // H=6, content=12, viewport=2, position=0
        //   virtual_track=12, thumb_slots=2*12/12=2, virtual_scroll_range=10,
        //   scroll_range=10, thumb_start=0 → thumb covers [0,2) → row 0
        // virtual slot = line*12/12 = line
        // Marker at line 2  → slot 2  → row 1 top   → "▀"
        // Marker at line 5  → slot 5  → row 2 bot   → "▄"
        // Marker at line 9  → slot 9  → row 4 bot   → "▄"
        // Marker at line 11 → slot 11 → row 5 bot   → "▄"
        let symbols = render_to_symbols_with_markers(6, 12, 2, 0, &[2, 5, 9, 11]);
        assert_eq!(symbols[0], "█", "row 0 thumb");
        assert_eq!(symbols[1], "▀", "row 1 marker top half");
        assert_eq!(symbols[2], "▄", "row 2 marker bottom half");
        assert_eq!(symbols[3], " ", "row 3 track");
        assert_eq!(symbols[4], "▄", "row 4 marker bottom half");
        assert_eq!(symbols[5], "▄", "row 5 marker bottom half (slot 11)");
    }

    #[test]
    fn empty_markers_unchanged() {
        // No markers → regular thumb rendering.
        let symbols = render_to_symbols_with_markers(5, 10, 3, 0, &[]);
        assert_eq!(symbols[0], "█", "row 0 thumb");
        assert_eq!(symbols[1], "▀", "row 1 thumb upper half");
        for i in 2..5 {
            assert_eq!(symbols[i], " ", "row {i} track (no markers)");
        }
    }

    #[test]
    fn marker_uses_green_fg_color() {
        let mut buf = Buffer::empty(Rect::new(0, 0, 1, 5));
        let scrollbar = SmoothScrollbar::new()
            .thumb_fg(Color::Gray)
            .track_bg(Color::DarkGray)
            .marker_fg(Color::Green)
            .with_markers(&[6]);
        // Position=0, content=10, viewport=3 → thumb covers rows 0-1.
        // Marker at line 6 → virtual slot 6 → row 3 top half.
        let mut state = SmoothScrollbarState::new(10)
            .position(0)
            .viewport_content_length(3);
        scrollbar.render(buf.area, &mut buf, &mut state);
        let cell = buf.cell((0, 3)).unwrap();
        assert_eq!(
            cell.fg,
            Color::Green,
            "marker cell should use green foreground"
        );
    }

    #[test]
    fn marker_mixed_with_thumb_has_green_fg_and_thumb_bg() {
        let mut buf = Buffer::empty(Rect::new(0, 0, 1, 5));
        let scrollbar = SmoothScrollbar::new()
            .thumb_fg(Color::Gray)
            .track_bg(Color::DarkGray)
            .marker_fg(Color::Green)
            .with_markers(&[0]);
        // Position=0, content=10, viewport=3 → thumb covers rows 0-1.
        // Marker at line 0 → virtual slot 0 → row 0 top half, which is
        // also inside the thumb range → mixed marker+thumb rendering.
        let mut state = SmoothScrollbarState::new(10)
            .position(0)
            .viewport_content_length(3);
        scrollbar.render(buf.area, &mut buf, &mut state);
        // Row 0: top marker (slot 0) + bottom thumb (slot 1).
        let cell = buf.cell((0, 0)).unwrap();
        assert_eq!(cell.symbol(), "▀", "marker top half over thumb");
        assert_eq!(cell.fg, Color::Green, "marker half should be green");
        assert_eq!(cell.bg, Color::Gray, "thumb half should use thumb fg as bg");
    }
}
