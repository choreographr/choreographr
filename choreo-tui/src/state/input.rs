//! Text-input machinery shared by every text field in the TUI (command
//! input, model-selector filter, new-account wizard filter/slug, credential
//! entry): the `InputBuffer` editing kernel plus the visual-line helpers
//! that wrap text for display and cursor movement.

use crossterm::event::{KeyCode, KeyEvent, KeyModifiers};
use unicode_segmentation::UnicodeSegmentation;
use unicode_width::UnicodeWidthStr;

pub(crate) struct InputBuffer {
    pub(crate) text: String,
    pub(crate) cursor: usize,
    /// Index of the first visual line shown in the visible window.
    /// Adjusted by `ensure_cursor_visible` after each mutation to keep
    /// the cursor in view.
    pub(crate) scroll_offset: usize,
    /// Monotonically increasing counter bumped on every text mutation.
    /// Used by `cached_visual_lines` to detect stale cache entries.
    pub(crate) generation: u64,
    /// Lazily computed visual lines, keyed by `(generation, max_width)`.
    pub(crate) lines_cache: Option<(u64, usize, Vec<VisualLineInfo>)>,
}

impl InputBuffer {
    pub(crate) fn new() -> Self {
        Self {
            text: String::new(),
            cursor: 0,
            scroll_offset: 0,
            generation: 0,
            lines_cache: None,
        }
    }

    #[cfg(test)]
    pub(crate) fn is_empty(&self) -> bool {
        self.text.is_empty()
    }

    pub(crate) fn clear(&mut self) {
        self.text.clear();
        self.cursor = 0;
        self.scroll_offset = 0;
        self.generation += 1;
    }

    pub(crate) fn cursor_left(&mut self) {
        let prefix = &self.text[..self.cursor];
        if let Some((start, _)) = prefix.grapheme_indices(true).next_back() {
            self.cursor = start;
        }
    }

    pub(crate) fn cursor_right(&mut self) {
        let suffix = &self.text[self.cursor..];
        if suffix.is_empty() {
            return;
        }
        if let Some((offset, grapheme)) = suffix.grapheme_indices(true).next() {
            self.cursor += offset + grapheme.len();
        }
    }

    pub(crate) fn cursor_home(&mut self) {
        self.cursor = 0;
    }

    pub(crate) fn cursor_end(&mut self) {
        self.cursor = self.text.len();
    }

    fn word_left_boundary(&self) -> usize {
        let s = &self.text[..self.cursor];
        let trimmed = s.trim_end();
        if trimmed.is_empty() {
            return 0;
        }
        trimmed
            .rfind(|c: char| c.is_whitespace())
            .map(|i| i + 1)
            .unwrap_or(0)
    }

    fn word_right_boundary(&self) -> usize {
        let s = &self.text[self.cursor..];
        if s.is_empty() {
            return self.cursor;
        }
        let mut chars = s.char_indices().peekable();
        if chars.peek().is_some_and(|&(_, c)| !c.is_whitespace()) {
            for (_, c) in chars.by_ref() {
                if c.is_whitespace() {
                    break;
                }
            }
        }
        while chars.peek().is_some_and(|&(_, c)| c.is_whitespace()) {
            chars.next();
        }
        self.cursor + chars.next().map(|(pos, _)| pos).unwrap_or(s.len())
    }

    pub(crate) fn word_left(&mut self) {
        self.cursor = self.word_left_boundary();
    }

    pub(crate) fn word_right(&mut self) {
        self.cursor = self.word_right_boundary();
    }

    pub(crate) fn insert_char_at_cursor(&mut self, c: char) {
        self.text.insert(self.cursor, c);
        self.cursor += c.len_utf8();
        self.generation += 1;
    }

    /// Insert a string at the cursor position.
    ///
    /// Used for paste events where a block of text (potentially
    /// containing newlines) is inserted all at once rather than
    /// character-by-character.
    pub(crate) fn insert_str_at_cursor(&mut self, s: &str) {
        if s.is_empty() {
            return;
        }
        self.text.insert_str(self.cursor, s);
        self.cursor += s.len();
        self.generation += 1;
    }

    pub(crate) fn backspace_at_cursor(&mut self) {
        if self.cursor == 0 {
            return;
        }
        let prefix = &self.text[..self.cursor];
        if let Some((start, _)) = prefix.grapheme_indices(true).next_back() {
            self.text.drain(start..self.cursor);
            self.cursor = start;
        }
        self.generation += 1;
    }

    pub(crate) fn delete_at_cursor(&mut self) {
        if self.cursor >= self.text.len() {
            return;
        }
        let suffix = &self.text[self.cursor..];
        if let Some((offset, grapheme)) = suffix.grapheme_indices(true).next() {
            self.text
                .drain(self.cursor + offset..self.cursor + offset + grapheme.len());
        }
        self.generation += 1;
    }

    pub(crate) fn delete_word_backward(&mut self) {
        if self.cursor == 0 {
            return;
        }
        let boundary = self.word_left_boundary();
        self.text.drain(boundary..self.cursor);
        self.cursor = boundary;
        self.generation += 1;
    }

    pub(crate) fn delete_word_forward(&mut self) {
        if self.cursor >= self.text.len() {
            return;
        }
        let boundary = self.word_right_boundary();
        self.text.drain(self.cursor..boundary);
        self.generation += 1;
    }

    pub(crate) fn delete_to_start(&mut self) {
        self.text.drain(..self.cursor);
        self.cursor = 0;
        self.generation += 1;
    }

    pub(crate) fn handle_key(&mut self, key: KeyEvent) -> bool {
        match key.code {
            KeyCode::Backspace if key.modifiers.contains(KeyModifiers::CONTROL) => {
                // Ctrl+Backspace clears the whole draft prompt (every text
                // field shares this kernel — chat input, model-selector
                // filter, credential/slug entry).  The buffer is emptied
                // wherever the cursor sits, unlike Ctrl+U which only clears
                // up to the cursor.  Ctrl+W stays the word-delete.
                self.clear();
                true
            }
            KeyCode::Backspace => {
                self.backspace_at_cursor();
                true
            }
            KeyCode::Delete if key.modifiers.contains(KeyModifiers::CONTROL) => {
                self.delete_word_forward();
                true
            }
            KeyCode::Delete => {
                self.delete_at_cursor();
                true
            }
            KeyCode::Left if key.modifiers.contains(KeyModifiers::CONTROL) => {
                self.word_left();
                true
            }
            KeyCode::Left => {
                self.cursor_left();
                true
            }
            KeyCode::Right if key.modifiers.contains(KeyModifiers::CONTROL) => {
                self.word_right();
                true
            }
            KeyCode::Right => {
                self.cursor_right();
                true
            }
            KeyCode::Home if key.modifiers.contains(KeyModifiers::CONTROL) => {
                self.cursor_home();
                true
            }
            KeyCode::End if key.modifiers.contains(KeyModifiers::CONTROL) => {
                self.cursor_end();
                true
            }
            KeyCode::Home => {
                self.cursor_home_line();
                true
            }
            KeyCode::End => {
                self.cursor_end_line();
                true
            }
            KeyCode::Char('w') if key.modifiers.contains(KeyModifiers::CONTROL) => {
                self.delete_word_backward();
                true
            }
            KeyCode::Char('u') if key.modifiers.contains(KeyModifiers::CONTROL) => {
                self.delete_to_start();
                true
            }
            KeyCode::Char(c)
                if !key
                    .modifiers
                    .intersects(KeyModifiers::CONTROL | KeyModifiers::ALT) =>
            {
                if c == '\0' {
                    // A NUL must never enter the text buffer.  crossterm 0.29
                    // parses kitty-protocol IME "text events"
                    // (`CSI 0;;<codepoints>u`, how terminals deliver composed
                    // text like Vietnamese) as `Char('\0')` because it drops
                    // the associated-text field.  We avoid that mode entirely
                    // (see KITTY_KEYBOARD_FLAGS in connection.rs), but if a
                    // terminal sends a NUL anyway (e.g. Ctrl+Space in legacy
                    // mode) dropping it is strictly better than inserting
                    // invisible control text.
                    return false;
                }
                self.insert_char_at_cursor(c);
                true
            }
            KeyCode::Enter | KeyCode::Tab | KeyCode::Esc => false,
            _ => false,
        }
    }

    /// Move cursor to the start of the current logical line (after `\n` or at offset 0).
    pub(crate) fn cursor_home_line(&mut self) {
        let prefix = &self.text[..self.cursor];
        self.cursor = prefix.rfind('\n').map(|p| p + 1).unwrap_or(0);
    }

    /// Move cursor to the end of the current logical line (at the `\n` or at text end).
    pub(crate) fn cursor_end_line(&mut self) {
        let suffix = &self.text[self.cursor..];
        self.cursor += suffix.find('\n').unwrap_or(suffix.len());
    }

    /// Move cursor up one visual line (wrapping-aware).
    pub(crate) fn cursor_up(&mut self, max_width: usize) {
        if max_width < 1 {
            return;
        }
        let lines = cached_visual_lines(
            &self.text,
            max_width,
            self.generation,
            &mut self.lines_cache,
        );
        let (current_line, col) = find_cursor_pos(&self.text, self.cursor, lines);
        if current_line == 0 {
            return;
        }
        let target = &lines[current_line as usize - 1];
        let target_text = &self.text[target.start_byte..target.end_byte];
        let target_col = (col as usize).min(target.display_width);
        let byte_off = byte_offset_at_column(target_text, target_col);
        self.cursor = target.start_byte + byte_off;
    }

    /// Move cursor down one visual line (wrapping-aware).
    pub(crate) fn cursor_down(&mut self, max_width: usize) {
        if max_width < 1 {
            return;
        }
        let lines = cached_visual_lines(
            &self.text,
            max_width,
            self.generation,
            &mut self.lines_cache,
        );
        let (current_line, col) = find_cursor_pos(&self.text, self.cursor, lines);
        if current_line + 1 >= lines.len() as u16 {
            return;
        }
        let target = &lines[current_line as usize + 1];
        let target_text = &self.text[target.start_byte..target.end_byte];
        let target_col = (col as usize).min(target.display_width);
        let byte_off = byte_offset_at_column(target_text, target_col);
        self.cursor = target.start_byte + byte_off;
    }

    /// Return the (visual_row, visual_col) of the cursor within wrapped text.
    /// Both are 0-indexed.
    pub(crate) fn cursor_visual_pos(&mut self, max_width: usize) -> (u16, u16) {
        if max_width < 1 {
            return (0, 0);
        }
        let lines = cached_visual_lines(
            &self.text,
            max_width,
            self.generation,
            &mut self.lines_cache,
        );
        find_cursor_pos(&self.text, self.cursor, lines)
    }

    /// True when the cursor is on the first visual line of the input.
    pub(crate) fn is_on_first_visual_line(&mut self, max_width: usize) -> bool {
        self.cursor_visual_pos(max_width).0 == 0
    }

    /// True when the cursor is on the last visual line of the input.
    pub(crate) fn is_on_last_visual_line(&mut self, max_width: usize) -> bool {
        if max_width < 1 {
            return true;
        }
        let lines = cached_visual_lines(
            &self.text,
            max_width,
            self.generation,
            &mut self.lines_cache,
        );
        let (row, _) = find_cursor_pos(&self.text, self.cursor, lines);
        row + 1 >= lines.len() as u16
    }

    /// Adjust `scroll_offset` so the cursor's visual line is within the visible window.
    ///
    /// `max_width` is the inner width of the input box (terminal width minus borders).
    /// `visible_height` is the number of content rows available.
    pub(crate) fn ensure_cursor_visible(&mut self, max_width: usize, visible_height: usize) {
        if max_width < 1 || visible_height == 0 {
            self.scroll_offset = 0;
            return;
        }
        let lines = cached_visual_lines(
            &self.text,
            max_width,
            self.generation,
            &mut self.lines_cache,
        );
        if lines.len() <= visible_height {
            self.scroll_offset = 0;
            return;
        }
        let max_scroll = lines.len() - visible_height;
        let (cursor_row, _) = find_cursor_pos(&self.text, self.cursor, lines);
        let cursor_row = cursor_row as usize;

        // If cursor is above the visible area, scroll up.
        if cursor_row < self.scroll_offset {
            self.scroll_offset = cursor_row;
        }
        // If cursor is below the visible area, scroll down.
        if self.scroll_offset + visible_height <= cursor_row {
            self.scroll_offset = cursor_row + 1 - visible_height;
        }

        self.scroll_offset = self.scroll_offset.min(max_scroll);
    }

    /// Map a mouse click at content `(row, col)` to a byte offset in the text.
    ///
    /// Coordinates are relative to the input box's text area (0-indexed),
    /// *excluding* the box borders and side padding: `row` is the visual line
    /// within the currently visible window and `col` is the display-width
    /// column.  `visible_height` is the number of content rows actually drawn
    /// (box height minus its two borders) — it is used to clamp `scroll_offset`
    /// exactly as the renderer does, so clicks land on the same lines that are
    /// drawn even when `scroll_offset` is stale (e.g. right after a resize
    /// shrank the box but before the next `ensure_cursor_visible` re-clamped
    /// it).  Clicks below the last text line resolve to the end of the buffer;
    /// clicks past the right edge of a line resolve to that line's end.
    /// Within a line the offset is grapheme-aware (see
    /// [`grapheme_offset_at_column`]).
    pub(crate) fn byte_offset_at_click(
        &mut self,
        max_width: usize,
        visible_height: usize,
        row: usize,
        col: usize,
    ) -> usize {
        if max_width < 1 {
            return 0;
        }
        let lines = cached_visual_lines(
            &self.text,
            max_width,
            self.generation,
            &mut self.lines_cache,
        );
        // Mirror the renderer's visible-window arithmetic: the window starts
        // at `scroll_offset` clamped so it never runs past the last visual
        // line.  Without this, a stale `scroll_offset` (left over from a wider
        // box) would map clicks to the end of the buffer instead of the line
        // under the pointer.
        let visible_count = visible_height.max(1).min(lines.len());
        let offset = self
            .scroll_offset
            .min(lines.len().saturating_sub(visible_count));
        let visual_idx = offset.saturating_add(row);
        match lines.get(visual_idx) {
            Some(vl) => {
                let line_text = &self.text[vl.start_byte..vl.end_byte];
                let target_col = col.min(vl.display_width);
                vl.start_byte + grapheme_offset_at_column(line_text, target_col)
            }
            // The click landed below the last visual line (e.g. after a resize
            // shrank the box); the cursor goes to the very end of the text.
            None => self.text.len(),
        }
    }
}

/// Return cached visual lines for `max_width`, recomputing only when
/// `max_width` or `text` has changed since the last call.
///
/// `generation` is a monotonically increasing counter from the owning
/// `InputBuffer` that is bumped on every text mutation.  The cache is
/// invalidated when either `generation` or `max_width` differs from
/// the values stored at the last computation.
///
/// Takes separate references to `text` and `cache` so callers can pass
/// field-level borrows and avoid borrow-checker conflicts with other
/// fields (e.g. `cursor`).
pub(crate) fn cached_visual_lines<'a>(
    text: &str,
    max_width: usize,
    generation: u64,
    cache: &'a mut Option<(u64, usize, Vec<VisualLineInfo>)>,
) -> &'a [VisualLineInfo] {
    let entry =
        cache.get_or_insert_with(|| (generation, max_width, compute_visual_lines(text, max_width)));
    if entry.0 != generation || entry.1 != max_width {
        entry.0 = generation;
        entry.1 = max_width;
        entry.2 = compute_visual_lines(text, max_width);
    }
    &entry.2
}

/// A single visual (wrapped) line derived from the input text.
#[derive(Debug)]
pub(crate) struct VisualLineInfo {
    /// Byte offset of the start of this visual line within the full input text.
    pub(crate) start_byte: usize,
    /// Byte offset of the end (exclusive) of this visual line.
    pub(crate) end_byte: usize,
    /// Display width of the text on this visual line.
    pub(crate) display_width: usize,
}

/// Find the cursor's (visual_row, visual_col) within pre-computed visual lines.
pub(crate) fn find_cursor_pos(text: &str, cursor: usize, lines: &[VisualLineInfo]) -> (u16, u16) {
    for (i, vl) in lines.iter().enumerate() {
        if cursor >= vl.start_byte && cursor <= vl.end_byte {
            let line_text = &text[vl.start_byte..cursor.min(vl.end_byte)];
            let col = UnicodeWidthStr::width(line_text);
            return (i as u16, col as u16);
        }
    }
    // Cursor past the last visual line — place at end.
    let last = match lines.last() {
        Some(vl) => vl,
        None => return (0, 0),
    };
    let col = UnicodeWidthStr::width(&text[last.start_byte..last.end_byte]);
    (lines.len().saturating_sub(1) as u16, col as u16)
}

/// Word-wrap `text` into visual lines that each fit within `max_width`.
/// Explicit `\n` characters always create line breaks.  Words longer than
/// `max_width` are placed on their own line and overflow — they are never
/// character-broken.  Returns at least one entry (for empty text).
pub(crate) fn compute_visual_lines(text: &str, max_width: usize) -> Vec<VisualLineInfo> {
    if max_width == 0 {
        return vec![VisualLineInfo {
            start_byte: 0,
            end_byte: 0,
            display_width: 0,
        }];
    }

    let text_ptr = text.as_ptr() as usize;
    let mut lines: Vec<VisualLineInfo> = Vec::new();

    for logical in text.split('\n') {
        let logical_offset = logical.as_ptr() as usize - text_ptr;

        if logical.is_empty() {
            lines.push(VisualLineInfo {
                start_byte: logical_offset,
                end_byte: logical_offset,
                display_width: 0,
            });
            continue;
        }

        // Collect word positions (non-whitespace runs) within this logical line.
        let mut words: Vec<(usize, usize)> = Vec::new(); // (start, end) byte offsets within `logical`
        let mut pos = 0;
        while pos < logical.len() {
            while pos < logical.len() && logical.as_bytes()[pos].is_ascii_whitespace() {
                pos += 1;
            }
            if pos >= logical.len() {
                break;
            }
            let w_start = pos;
            while pos < logical.len() && !logical.as_bytes()[pos].is_ascii_whitespace() {
                pos += 1;
            }
            words.push((w_start, pos));
        }

        if words.is_empty() {
            // Logical line contained only whitespace.
            lines.push(VisualLineInfo {
                start_byte: logical_offset,
                end_byte: logical_offset + logical.len(),
                display_width: UnicodeWidthStr::width(logical),
            });
            continue;
        }

        // Greedy word-wrap: accumulate words onto visual lines.
        let mut line_start_byte = logical_offset; // byte offset in full text
        let mut line_width: usize = 0;
        let mut last_word_end_byte = logical_offset; // end of last word placed

        for (i, &(w_start, w_end)) in words.iter().enumerate() {
            let word = &logical[w_start..w_end];
            let word_width = UnicodeWidthStr::width(word);

            // Whitespace between the previous word (or start of logical line) and this word.
            let preceding_ws = if i == 0 {
                &logical[0..w_start]
            } else {
                &logical[words[i - 1].1..w_start]
            };
            let ws_width = UnicodeWidthStr::width(preceding_ws);

            let space_needed = ws_width + word_width;

            if line_width > 0 && line_width + space_needed > max_width {
                // Flush current line (everything up to the end of the previous word).
                lines.push(VisualLineInfo {
                    start_byte: line_start_byte,
                    end_byte: last_word_end_byte,
                    display_width: line_width,
                });
                // Start new visual line with this word (leading whitespace trimmed).
                line_start_byte = logical_offset + w_start;
                line_width = word_width;
                last_word_end_byte = logical_offset + w_end;
            } else {
                line_width += space_needed;
                last_word_end_byte = logical_offset + w_end;
            }
        }

        // Flush the last visual line of this logical line,
        // including any trailing whitespace after the last word.
        let trailing_ws = words
            .last()
            .map(|&(_, w_end)| &logical[w_end..])
            .unwrap_or(logical);
        let trailing_ws_width = UnicodeWidthStr::width(trailing_ws);
        lines.push(VisualLineInfo {
            start_byte: line_start_byte,
            end_byte: last_word_end_byte + trailing_ws.len(),
            display_width: line_width + trailing_ws_width,
        });
    }

    if lines.is_empty() {
        lines.push(VisualLineInfo {
            start_byte: 0,
            end_byte: 0,
            display_width: 0,
        });
    }

    lines
}

/// Find the byte offset within `s` for the given display-width column,
/// without exceeding `target_col`.  Returns `s.len()` if `target_col` is
/// larger than the string's display width.
pub(crate) fn byte_offset_at_column(s: &str, target_col: usize) -> usize {
    let mut col = 0;
    for (byte_i, ch) in s.char_indices() {
        let ch_w = UnicodeWidthStr::width(&s[byte_i..byte_i + ch.len_utf8()]);
        if col + ch_w > target_col {
            return byte_i;
        }
        col += ch_w;
    }
    s.len()
}

/// Find the byte offset within `s` for a click at display column `target_col`.
///
/// A click is a direct placement intent, so unlike [`byte_offset_at_column`]
/// (which cursor up/down use to *preserve* a column "at or before" the target)
/// this is grapheme-cluster aware:
///
/// - The cursor is always placed on a grapheme boundary — it can never land
///   inside a ZWJ family emoji or a base+combining sequence.
/// - For a wide grapheme, a click on its leftmost cell places the cursor
///   before it and a click on any later cell places it after — matching how
///   editors treat wide characters.
/// - Clicks past the end of the string's display width resolve to the end.
///
/// Returns `s.len()` when `target_col` is larger than the string's display
/// width.
pub(crate) fn grapheme_offset_at_column(s: &str, target_col: usize) -> usize {
    let mut col = 0;
    for (byte_i, g) in s.grapheme_indices(true) {
        let g_w = UnicodeWidthStr::width(g);
        if col + g_w > target_col {
            // The click fell inside this grapheme's cells.  A 1-column
            // grapheme's only cell places the cursor before it; a wide
            // grapheme's first cell does the same, but its remaining cells
            // place the cursor after the whole cluster.
            if g_w >= 2 && target_col > col {
                return byte_i + g.len();
            }
            return byte_i;
        }
        col += g_w;
    }
    s.len()
}
