//! The inline command palette (Chat page).
//!
//! The logical keymap (the one-shortcut-per-command table and its `KeyEvent`
//! resolution) lives in [`crate::state::keymap`].
//!
//! There is **no separate "command mode" flag**: a line is a COMMAND LINE iff
//! [`is_command_line`] says so — the buffer starts with `/` and its first token
//! is not a filesystem path — and the palette is shown exactly when it is (and
//! no higher-priority overlay — the model selector — is open).  The `/`
//! therefore lives IN the buffer, verbatim as the user typed it and exactly as
//! `parse_input_line` expects (it strips one leading `/` and parses the
//! remainder as a command).  The palette thus appears the moment a `/` starts
//! the line and disappears when the user deletes it — there is no latched flag
//! that can fall out of sync with the buffer.
//!
//! The predicate is the shared [`command_remainder`] (and its `is_some()`
//! wrapper `is_command_line` in the same module), not a bare `starts_with('/')`
//! test, so a path — which is what a file dragged onto the terminal pastes, and
//! which always begins with `/` — stays a prompt: the drop lands in the composer
//! as text the user can wrap a question around, and Enter sends it to the model
//! instead of trying to run `/home/me/notes.md` as a command.
//!
//! A leading run of slashes (`//acl`) is the marker plus a typing slip: it is
//! still a command line (not a path, so it is never sent as a prompt), but it is
//! kept verbatim — the palette filters on the token after ONE slash, so
//! `//acl` matches nothing and the parser reports `unknown command: //acl`,
//! naming exactly what the user typed.
//!
//! While the palette is shown the buffer holds the command line WITH its
//! leading slash (`/model`, `/model gpt-4o`), `Enter` RUNS the line (see
//! `command_palette_enter_line`), `Esc` discards it, `Tab` completes the
//! highlighted name into the buffer, and `↑`/`↓` move the palette highlight.

use crate::state::App;
use choreo_client_core::{CommandMatch, command_catalog, command_remainder, match_commands};

/// Transient selection state for the inline command palette.
pub(crate) struct CommandPaletteState {
    /// Index into the current filtered match list of the highlighted row.
    focused: usize,
    /// First row of the visible window — a hint corrected at render time by
    /// `picker_window`, mirroring the picker popups.
    scroll: usize,
}

impl CommandPaletteState {
    pub(crate) fn new() -> Self {
        Self {
            focused: 0,
            scroll: 0,
        }
    }

    /// Reset the highlight and scroll window to the top.  Called when a command
    /// line is discarded, so the next command line starts on the first row.
    fn reset(&mut self) {
        self.focused = 0;
        self.scroll = 0;
    }
}

impl App {
    /// The current command line's remainder — the text after its leading `/` —
    /// or `None` when the buffer is not a command line.
    ///
    /// Command-line-ness is derived here — there is NO `command_mode` flag — so
    /// the palette tracks the buffer exactly: typing the leading `/` shows it,
    /// deleting the `/` hides it, and a paste of a `/…` line shows it for free.
    /// The test itself is the shared [`command_remainder`], the same primitive
    /// the parser splits with, so the palette can never label a line a command
    /// that the parser would then send as a prompt (a dropped file's path is the
    /// case that matters: it starts with `/` but is not a command).
    fn command_line(&self) -> Option<&str> {
        command_remainder(&self.input.text)
    }

    /// The command line split into its first token and the argument tail that
    /// follows it, both taken from the remainder after the single marker slash
    /// ([`command_remainder`]): `"/model gpt-4o"` → `("model", " gpt-4o")`,
    /// `"/"` → `("", "")`.  `None` when the buffer is not a command line.
    ///
    /// Every reader of a command line goes through this one split (the filter
    /// query below, the exact-match test and the completion offsets), so they
    /// can never disagree about which token is the command.  The split is
    /// literal: only the one marker slash comes off, so a doubled slash
    /// (`//acl`) yields the token `/acl`, matches no command name, and is
    /// reported by the parser verbatim rather than silently rewritten.
    fn command_split(&self) -> Option<(&str, &str)> {
        let body = self.command_line()?;
        let first = body.split(char::is_whitespace).next().unwrap_or("");
        // `first` is a whitespace-split prefix of `body`, so its length is a
        // char boundary at or before the end; `get` keeps the slice total.
        Some((first, body.get(first.len()..).unwrap_or("")))
    }

    /// The first whitespace-delimited token of the command line
    /// (`"/model gpt-4o"` → `"model"`, `"/"`/`"/  model"` → `""`).  The leading
    /// slash is stripped first, so the token is a bare command name (or empty) —
    /// exactly what `match_commands` matches against.  Only the first token
    /// filters the palette (an argument tail such as `gpt-4o` is ignored).  A
    /// non-command buffer yields `""` (the whole catalog); callers that need the
    /// active/inactive distinction use [`Self::command_palette_active`].
    fn command_query(&self) -> &str {
        self.command_split().map_or("", |(first, _)| first)
    }

    /// Whether a command line is active and the palette should be shown: the
    /// buffer is a command line and no higher-priority overlay (the model
    /// selector) is open.  This stays TRUE with zero matches, so a "no matches"
    /// palette still shows and `Enter` still submits the typed command line.
    pub(crate) fn command_palette_active(&self) -> bool {
        self.command_line().is_some() && !self.model_selector.is_open()
    }

    /// The commands matching the current command line's first token (empty when
    /// the buffer is not a command line).  An empty query returns the whole
    /// catalog.
    pub(crate) fn command_palette_matches(&self) -> Vec<CommandMatch> {
        if self.command_line().is_none() {
            return Vec::new();
        }
        match_commands(self.command_query())
    }

    /// Discard the command line, if the buffer holds one: clear the buffer and
    /// reset the palette.  Guarded — a no-op on a non-command buffer — so the
    /// many call sites that fire on page changes / session switches (which run
    /// with a real prompt draft in the buffer) never clobber that draft.
    pub(crate) fn discard_command_line(&mut self) {
        if self.command_line().is_none() {
            return;
        }
        self.input.clear();
        self.command_palette.reset();
    }

    /// Move the palette highlight by `delta` rows, wrapping at both ends.
    /// A no-op while there is nothing to select.
    // `len` is catalog-sized (a handful of commands) and `focused` is folded
    // into `[0, len)`, so the isize round-trip cannot actually wrap.
    #[expect(clippy::cast_possible_wrap)]
    pub(crate) fn command_palette_move(&mut self, delta: isize) {
        let len = self.command_palette_matches().len();
        if len == 0 {
            return;
        }
        // Fold a stale focus (left past the end by a filter change) back into
        // range before stepping, then wrap with `rem_euclid` so ↑ from the top
        // lands on the last row and ↓ from the bottom wraps to the first.
        let current = (self.command_palette.focused % len) as isize;
        self.command_palette.focused = (current + delta).rem_euclid(len as isize) as usize;
    }

    /// Complete the highlighted match into the command line, keeping the
    /// leading `/`: `"/mo"` → `"/model "` with the cursor at the end (never
    /// submits).  A no-op when there is nothing to complete.
    pub(crate) fn command_palette_complete(&mut self) {
        // Only a command line completes; the tail is whatever the user already
        // typed after the first token, preserved verbatim (so a trailing space
        // stays a trailing space) — see `command_split`.
        let Some((_, tail)) = self.command_split() else {
            return;
        };
        let matches = self.command_palette_matches();
        // Prefer the highlighted row; fall back to the first match when the
        // focus is stale.  Nothing to do on an empty list.
        let Some(found) = matches
            .get(self.command_palette.focused)
            .or_else(|| matches.first())
        else {
            return;
        };
        // A bare partial (`/mo`) completes to `"/model "` with a trailing space
        // so an argument can follow; an existing tail (`/mo gpt-4o`) keeps its
        // leading whitespace and is appended to the canonical name.
        let completed = if tail.is_empty() {
            format!("/{} ", found.spec.name)
        } else {
            format!("/{}{}", found.spec.name, tail)
        };
        // Built before the assignment so the borrow `tail` holds on the buffer
        // has ended (`format!` reads the buffer, the assignment writes it).
        self.input.text = completed;
        // Place the cursor at the end of the completed line via the buffer's
        // own accessor.  `InputBuffer::cursor` is a BYTE offset (every edit
        // advances it by `len_utf8`, and the buffer slices on it), so the end
        // of the text is its byte length — `cursor_end` sets exactly that.  A
        // char count would desync the cursor from its byte-indexed invariant
        // the moment the completed line ever held non-ASCII text.
        self.input.cursor_end();
        self.input.generation += 1;
        self.ensure_input_cursor_visible();
    }

    /// Resolve the command line to RUN when the user presses Enter — the bridge
    /// from "the highlighted palette row" to an executable line, so Enter alone
    /// runs the selected command without a preceding `Tab`.
    ///
    /// The typed line's FIRST whitespace-delimited token (after the one marker
    /// slash) is what filters the palette.  When that token already names a
    /// command EXACTLY, the line is returned in that command's CANONICAL
    /// spelling with its argument tail byte-for-byte intact (`/model gpt-4o` →
    /// `/model gpt-4o`, `/MoDel gpt-4o` → `/model gpt-4o`), so what the palette
    /// highlighted above the input is what runs; only the name's spelling is
    /// ever normalized, never an argument.  Otherwise the token is empty or a
    /// still-partial name: complete it to the highlighted match and keep any
    /// argument tail (`/mo gpt-4o` → `/model gpt-4o`), which is exactly what
    /// running the selected row means.  With no highlighted match (a typo'd
    /// command, or a doubled `//slash`) the line is returned verbatim so the
    /// normal "unknown command" feedback fires naming what the user typed.  A
    /// non-command buffer is returned verbatim (its caller runs it as a prompt).
    pub(crate) fn command_palette_enter_line(&self) -> String {
        let line = &self.input.text;
        // Only a command line resolves to a command; a prompt — a dropped
        // file's path, say — runs untouched as prompt text.
        let Some((first, tail)) = self.command_split() else {
            return line.clone();
        };
        // A fully-typed command runs untouched as to its arguments — they belong
        // to the user, not to a palette completion — in canonical spelling (the
        // name's case is normalized so the highlight always matches what runs).
        if let Some(spec) = command_catalog()
            .iter()
            .find(|spec| spec.name.eq_ignore_ascii_case(first))
        {
            return format!("/{}{}", spec.name, tail);
        }
        // Empty or still-partial token: adopt the highlighted row.  Prefer the
        // focused match; fall back to the first match when the focus is stale.
        // Keep whatever followed the token (`/mo gpt-4o` → `/model gpt-4o`) so a
        // completed prefix does not drop an already-typed argument.
        let matches = self.command_palette_matches();
        let Some(found) = matches
            .get(self.command_palette.focused)
            .or_else(|| matches.first())
        else {
            // Nothing highlighted (no matches): run the line as typed so the
            // parser reports the unknown command.
            return line.clone();
        };
        format!("/{}{}", found.spec.name, tail)
    }

    /// The highlighted row index, clamped against the current match count — the
    /// renderer uses it to place the `>` marker (and never points past the end
    /// of a narrowed list).
    pub(crate) fn command_palette_focused(&self) -> usize {
        let len = self.command_palette_matches().len();
        if len == 0 {
            0
        } else {
            self.command_palette.focused.min(len - 1)
        }
    }

    /// The stored window-start hint (see [`CommandPaletteState::scroll`]).
    pub(crate) fn command_palette_scroll(&self) -> usize {
        self.command_palette.scroll
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::tests::support::test_app;

    /// Set the composer to a command line (leading slash included) the way
    /// typing it would — a command line is any buffer `is_command_line` accepts
    /// (see `choreo_client_core::command_remainder`).
    fn set_command(app: &mut App, line: &str) {
        app.input.text = line.to_string();
        app.input.cursor = line.len();
    }

    #[test]
    fn a_slash_leading_buffer_is_a_command_line_and_activates() {
        let mut app = test_app();
        assert!(!app.command_palette_active(), "inactive on an empty buffer");
        set_command(&mut app, "/");
        assert!(app.command_palette_active());
        assert_eq!(app.command_line(), Some(""));
        assert_eq!(
            app.command_palette_matches().len(),
            choreo_client_core::command_catalog().len(),
            "an empty command line shows the whole catalog"
        );
    }

    #[test]
    fn a_non_slash_buffer_is_not_a_command_line() {
        let mut app = test_app();
        app.input.text = "model".to_string();
        assert!(!app.command_palette_active());
        assert_eq!(app.command_line(), None);
        assert!(app.command_palette_matches().is_empty());
    }

    #[test]
    fn discard_command_line_clears_a_command_but_spares_a_prompt_draft() {
        let mut app = test_app();
        set_command(&mut app, "/model");
        app.discard_command_line();
        assert!(app.input.text.is_empty(), "the command line is discarded");

        // Outside a command line, discarding must NOT clobber a real draft.
        app.input.text = "hello".to_string();
        app.discard_command_line();
        assert_eq!(app.input.text, "hello", "a real prompt draft is preserved");
    }

    #[test]
    fn active_is_false_while_model_selector_open() {
        let mut app = test_app();
        set_command(&mut app, "/");
        assert!(app.command_palette_active());
        app.model_selector.open();
        assert!(
            !app.command_palette_active(),
            "the modal overlay suppresses the palette"
        );
    }

    #[test]
    fn matches_use_first_token_only() {
        let mut app = test_app();
        set_command(&mut app, "/");
        // Empty command line → whole catalog.
        assert_eq!(
            app.command_palette_matches().len(),
            choreo_client_core::command_catalog().len()
        );

        set_command(&mut app, "/mo");
        let names: Vec<&str> = app
            .command_palette_matches()
            .iter()
            .map(|m| m.spec.name)
            .collect();
        assert_eq!(names, vec!["model"]);

        // An argument after a space still filters by the FIRST token.
        set_command(&mut app, "/model gpt-4o");
        let names: Vec<&str> = app
            .command_palette_matches()
            .iter()
            .map(|m| m.spec.name)
            .collect();
        assert_eq!(names, vec!["model"], "only the first token filters");
    }

    #[test]
    fn move_wraps_at_both_ends() {
        let mut app = test_app();
        set_command(&mut app, "/"); // empty query → whole catalog
        let len = app.command_palette_matches().len();
        assert!(len > 1, "catalog must have several commands");
        assert_eq!(app.command_palette_focused(), 0);

        app.command_palette_move(-1);
        assert_eq!(
            app.command_palette_focused(),
            len - 1,
            "↑ from the top wraps to the last row"
        );
        app.command_palette_move(1);
        assert_eq!(
            app.command_palette_focused(),
            0,
            "↓ from the bottom wraps to the first row"
        );
    }

    #[test]
    fn move_is_noop_when_empty() {
        let mut app = test_app();
        set_command(&mut app, "/zzz-no-such-command");
        assert!(app.command_palette_matches().is_empty());
        app.command_palette_move(1);
        assert_eq!(app.command_palette_focused(), 0);
    }

    #[test]
    fn complete_keeps_the_slash_and_puts_cursor_at_end() {
        let mut app = test_app();
        set_command(&mut app, "/mo");
        app.command_palette_complete();
        assert_eq!(app.input.text, "/model ");
        assert_eq!(app.input.cursor, "/model ".len());
    }

    #[test]
    fn complete_keeps_an_argument_tail() {
        // Tab completion must not drop an argument the user already typed —
        // `command_palette_enter_line` preserves it, so Tab must agree.
        let mut app = test_app();
        set_command(&mut app, "/mo gpt-4o");
        app.command_palette_complete();
        assert_eq!(app.input.text, "/model gpt-4o");
        assert_eq!(app.input.cursor, "/model gpt-4o".len());
    }

    #[test]
    fn complete_is_noop_when_nothing_matches() {
        let mut app = test_app();
        set_command(&mut app, "/zzz-no-such-command");
        app.command_palette_complete();
        assert_eq!(app.input.text, "/zzz-no-such-command");
    }

    #[test]
    fn enter_line_uses_the_highlighted_command_on_a_bare_slash() {
        let mut app = test_app();
        set_command(&mut app, "/"); // whole catalog, focus on row 0
        assert_eq!(
            app.command_palette_enter_line(),
            format!("/{}", choreo_client_core::command_catalog()[0].name),
            "Enter runs the highlighted row without a preceding Tab"
        );

        // Moving the highlight changes which command Enter runs.
        app.command_palette_move(1);
        assert_eq!(
            app.command_palette_enter_line(),
            format!("/{}", choreo_client_core::command_catalog()[1].name)
        );
    }

    #[test]
    fn enter_line_completes_a_partial_first_token() {
        let mut app = test_app();
        set_command(&mut app, "/mo");
        assert_eq!(app.command_palette_enter_line(), "/model");
    }

    #[test]
    fn enter_line_keeps_the_argument_tail_of_a_partial_token() {
        let mut app = test_app();
        set_command(&mut app, "/mo gpt-4o");
        assert_eq!(app.command_palette_enter_line(), "/model gpt-4o");
    }

    #[test]
    fn enter_line_keeps_a_fully_typed_command_verbatim() {
        let mut app = test_app();
        set_command(&mut app, "/model gpt-4o");
        assert_eq!(
            app.command_palette_enter_line(),
            "/model gpt-4o",
            "an exact command name runs untouched, arguments and all"
        );
    }

    #[test]
    fn enter_line_returns_verbatim_when_nothing_matches() {
        let mut app = test_app();
        set_command(&mut app, "/zzz-no-such-command");
        assert_eq!(
            app.command_palette_enter_line(),
            "/zzz-no-such-command",
            "no highlighted row → the parser still reports the unknown command"
        );
    }

    #[test]
    fn enter_line_returns_a_non_command_line_verbatim() {
        let mut app = test_app();
        app.input.text = "hello".to_string();
        assert_eq!(
            app.command_palette_enter_line(),
            "hello",
            "a plain prompt line is returned untouched"
        );
    }

    // ── a dropped file's path is a prompt, not a command ──────────────
    //
    // A path starts with `/` but its first token carries a path separator, so
    // it is NOT a command line: no palette, no `command` title, and Enter sends
    // it to the model as prompt text (see the end-to-end cases in
    // `crate::tests::app::palette`).

    #[test]
    fn a_dropped_path_is_not_a_command_line() {
        let mut app = test_app();
        set_command(&mut app, "/home/jbrown/veday_tiles.md");
        assert!(!app.command_palette_active(), "no palette for a path");
        assert_eq!(app.command_line(), None);
        assert!(app.command_palette_matches().is_empty());
        assert_eq!(
            app.command_palette_enter_line(),
            "/home/jbrown/veday_tiles.md",
            "the path runs verbatim, as prompt text"
        );
    }

    // ── a doubled slash is verbatim, not normalized into a command ────

    #[test]
    fn a_doubled_slash_is_reported_verbatim_rather_than_guessed_at() {
        // `//acl` is still a command line (it is not a path, so it must never be
        // sent to the model as a prompt), but the palette and the parser read it
        // literally: the filter is `/acl`, which matches no command name, so
        // nothing is highlighted and Enter runs the line as typed — the error
        // names exactly what the user saw (see
        // `crate::tests::app::palette::a_double_slash_is_run_verbatim_never_silently_prepended`).
        let mut app = test_app();
        set_command(&mut app, "//acl");
        assert!(app.command_palette_active(), "still a command line");
        assert_eq!(
            app.command_query(),
            "/acl",
            "only the one marker slash is stripped"
        );
        assert!(
            app.command_palette_matches().is_empty(),
            "no command name carries a slash, so nothing matches"
        );
        assert_eq!(
            app.command_palette_enter_line(),
            "//acl",
            "the line runs verbatim, so the error names what was typed"
        );
    }

    #[test]
    fn enter_line_normalizes_the_name_case_but_never_the_arguments() {
        // The filter matches case-insensitively, so what the palette highlights
        // must be what Enter runs; the argument tail — the user's text — is
        // never touched.
        let mut app = test_app();
        set_command(&mut app, "/MoDel gpt-4o");
        assert_eq!(app.command_palette_enter_line(), "/model gpt-4o");
    }
}
