//! The inline command palette (Chat page), the logical shortcut table (one
//! shortcut per command), and the chord / `KeyEvent` resolution that rebinds
//! `Ctrl+M` to `Ctrl+O` on legacy terminals.
//!
//! There is **no separate "command mode" flag**: a line is a COMMAND LINE iff
//! the shared composer buffer (`App::input`) starts with `/`, and the palette
//! is shown exactly when it is (and no higher-priority overlay — the model
//! selector — is open).  The `/` therefore lives IN the buffer, verbatim as
//! the user typed it and exactly as `parse_input_line` expects (it strips one
//! leading `/` and parses the remainder as a command).  The palette thus
//! appears the moment a `/` starts the line and disappears when the user
//! deletes it — there is no latched flag that can fall out of sync with the
//! buffer (the historical bug where a second `/` produced `//acl`).
//!
//! While the palette is shown the buffer holds the command line WITH its
//! leading slash (`/model`, `/model gpt-4o`), `Enter` RUNS the line (see
//! `command_palette_enter_line`), `Esc` discards it, `Tab` completes the
//! highlighted name into the buffer, and `↑`/`↓` move the palette highlight.

use crate::state::App;
use choreo_client_core::{CommandMatch, command_catalog, match_commands};
use crossterm::event::{KeyCode, KeyEvent, KeyModifiers};

/// A single logical key within a [`Chord`].
///
/// `Esc`/`PageUp`/`PageDown` (and some `Char`s) are not bound by the current
/// shortcut table, but the type models the full logical-key space so extending
/// the table is a data change, not a type change.
#[allow(dead_code)] // some variants are unbound today (see above)
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum Key {
    Char(char),
    Up,
    Down,
    Enter,
    Esc,
    PageUp,
    PageDown,
}

/// A logical key chord (modifiers + key).
///
/// Constructed with the `ctrl`/`alt` helpers so the shortcut table reads as the
/// logical binding; `label` renders the human-facing form (e.g. `Ctrl+M`,
/// `Alt+Enter`, `Ctrl+↑`).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) struct Chord {
    ctrl: bool,
    alt: bool,
    shift: bool,
    key: Key,
}

impl Chord {
    /// `Ctrl+<key>`.
    pub(crate) const fn ctrl(key: Key) -> Self {
        Self {
            ctrl: true,
            alt: false,
            shift: false,
            key,
        }
    }

    /// `Alt+<key>`.
    pub(crate) const fn alt(key: Key) -> Self {
        Self {
            ctrl: false,
            alt: true,
            shift: false,
            key,
        }
    }

    /// Human-facing label, e.g. `"Ctrl+M"`, `"Alt+Enter"`, `"Ctrl+↑"`.
    pub(crate) fn label(self) -> String {
        let mut label = String::new();
        if self.ctrl {
            label.push_str("Ctrl+");
        }
        if self.alt {
            label.push_str("Alt+");
        }
        if self.shift {
            label.push_str("Shift+");
        }
        label.push_str(&key_label(self.key));
        label
    }
}

/// Human-facing label for a bare [`Key`] (the part after the modifiers).
fn key_label(key: Key) -> String {
    match key {
        // Letters render as the uppercase glyph (`Ctrl+M`, not `Ctrl+m`).
        Key::Char(c) => c.to_ascii_uppercase().to_string(),
        Key::Up => "↑".to_string(),
        Key::Down => "↓".to_string(),
        Key::Enter => "Enter".to_string(),
        Key::Esc => "Esc".to_string(),
        Key::PageUp => "PageUp".to_string(),
        Key::PageDown => "PageDown".to_string(),
    }
}

/// One logical shortcut per command: the *initial* binding, before
/// [`resolve_chord`] applies any terminal-specific rebinding.  The table is the
/// single source of truth for both key dispatch hints and the palette's
/// right-aligned shortcut labels.
///
/// `Ctrl+Q` is not dispatched through `run_named` — it stays the global
/// pre-dispatch special case in `connection/mod.rs` (so it quits even while
/// modals are open).  It is listed here for DISPLAY only.
const SHORTCUTS: &[(Chord, &str)] = &[
    (Chord::ctrl(Key::Char('m')), "model"),
    (Chord::ctrl(Key::Char('r')), "reasoning"),
    (Chord::alt(Key::Enter), "continue"),
    (Chord::ctrl(Key::Up), "undo"),
    (Chord::ctrl(Key::Down), "redo"),
    (Chord::ctrl(Key::Char('s')), "session"),
    (Chord::ctrl(Key::Char('a')), "account"),
    (Chord::ctrl(Key::Char('q')), "quit"),
];

/// Apply the terminal-specific rebinding for a logical chord.
///
/// This is the single place legacy rebinding lives: `Ctrl+M` is byte 0x0D on a
/// legacy terminal (indistinguishable from Enter), so the model selector is
/// reached with `Ctrl+O` there (see `App::keyboard_enhanced`).  Extending the
/// legacy rebinding is a matter of adding another arm here.
fn resolve_chord(chord: Chord, keyboard_enhanced: bool) -> Chord {
    if !keyboard_enhanced && chord == Chord::ctrl(Key::Char('m')) {
        Chord::ctrl(Key::Char('o'))
    } else {
        chord
    }
}

/// Lower a crossterm `KeyEvent` to a logical [`Chord`], or `None` for keys the
/// shortcut table never binds.
///
/// Used by [`binding_for`], the single-source-of-truth lookup that Chat-page
/// shortcut dispatch routes through.
fn chord_from_event(event: &KeyEvent) -> Option<Chord> {
    let key = match event.code {
        // Letters fold to lowercase so a kitty Shift-reporting terminal's
        // `Char('M')+CONTROL` still matches the lowercase table entry.
        KeyCode::Char(c) => Key::Char(c.to_ascii_lowercase()),
        KeyCode::Up => Key::Up,
        KeyCode::Down => Key::Down,
        KeyCode::Enter => Key::Enter,
        KeyCode::Esc => Key::Esc,
        KeyCode::PageUp => Key::PageUp,
        KeyCode::PageDown => Key::PageDown,
        _ => return None,
    };
    Some(Chord {
        ctrl: event.modifiers.contains(KeyModifiers::CONTROL),
        alt: event.modifiers.contains(KeyModifiers::ALT),
        shift: event.modifiers.contains(KeyModifiers::SHIFT),
        key,
    })
}

/// Resolve a crossterm event to the name of the command whose shortcut it is,
/// or `None` when it matches no shortcut.
///
/// Logical chords are resolved for the current terminal, so a `Ctrl+O` event
/// maps to `"model"` on a legacy terminal while `Ctrl+M` maps to `"model"` on a
/// kitty terminal.
///
/// This is the single-source-of-truth lookup that Chat-page shortcut dispatch
/// routes through (`handle_chat_ctrl_key`), so every shortcut runs the same
/// command path as its typed spelling and the terminal-specific rebinding
/// lives in exactly one place.
pub(crate) fn binding_for(event: &KeyEvent, keyboard_enhanced: bool) -> Option<&'static str> {
    let chord = chord_from_event(event)?;
    SHORTCUTS
        .iter()
        .find(|(logical, _)| resolve_chord(*logical, keyboard_enhanced) == chord)
        .map(|(_, name)| *name)
}

/// The display label for a command's shortcut, resolved for the current
/// terminal (so the model selector advertises `Ctrl+O` on legacy terminals).
/// `None` when the command has no shortcut.
pub(crate) fn shortcut_label_for(name: &str, keyboard_enhanced: bool) -> Option<String> {
    SHORTCUTS
        .iter()
        .find(|(_, n)| *n == name)
        .map(|(chord, _)| resolve_chord(*chord, keyboard_enhanced).label())
}

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
    /// The current command line's text AFTER its leading `/`, or `None` when
    /// the buffer is not a command line.
    ///
    /// Command-line-ness is derived here — there is NO `command_mode` flag — so
    /// the palette tracks the buffer exactly: typing the leading `/` shows it,
    /// deleting the `/` hides it, and a paste of a `/…` line shows it for free.
    fn command_line(&self) -> Option<&str> {
        self.input.text.strip_prefix('/')
    }

    /// The first whitespace-delimited token of the command line
    /// (`"/model gpt-4o"` → `"model"`, `"/"`/`"/  model"` → `""`).  The leading
    /// slash is stripped first, so the token is a bare command name (or empty) —
    /// exactly what `match_commands` matches against.  Only the first token
    /// filters the palette (an argument tail such as `gpt-4o` is ignored).  A
    /// non-command buffer yields `""` (the whole catalog); callers that need the
    /// active/inactive distinction use [`Self::command_palette_active`].
    fn command_query(&self) -> &str {
        self.command_line()
            .unwrap_or("")
            .split(char::is_whitespace)
            .next()
            .unwrap_or("")
    }

    /// Whether a command line is active and the palette should be shown: the
    /// buffer starts with `/` and no higher-priority overlay (the model
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
    #[allow(clippy::cast_possible_wrap, clippy::cast_sign_loss)]
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
        let matches = self.command_palette_matches();
        // Prefer the highlighted row; fall back to the first match when the
        // focus is stale.  Nothing to do on an empty list.
        let Some(found) = matches
            .get(self.command_palette.focused)
            .or_else(|| matches.first())
        else {
            return;
        };
        self.input.text = format!("/{} ", found.spec.name);
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
    /// The typed line's FIRST whitespace-delimited token (after the leading `/`)
    /// is what filters the palette.  When that token already names a command
    /// EXACTLY the line is returned verbatim — a fully-typed command must never
    /// be rewritten, and its argument tail (`/model gpt-4o`, `/session new foo`)
    /// has to survive.  Otherwise the token is empty or a still-partial name:
    /// complete it to the highlighted match and keep any argument tail
    /// (`/mo gpt-4o` → `/model gpt-4o`), which is exactly what running the
    /// selected row means.  With no highlighted match (a typo'd command) the
    /// line is returned verbatim so the normal "unknown command" feedback still
    /// fires.  A non-command buffer is returned verbatim (its caller runs it as
    /// a prompt).
    pub(crate) fn command_palette_enter_line(&self) -> String {
        let line = &self.input.text;
        // Only a `/`-leading line is a command; anything else runs untouched.
        let Some(rest) = line.strip_prefix('/') else {
            return line.clone();
        };
        // Split off the leading token the same way `command_query` does (first
        // whitespace-delimited token of the post-slash remainder) so the
        // exact-match test and the palette filter always agree on the query.
        let first = rest.split(char::is_whitespace).next().unwrap_or("");
        // A fully-typed command runs untouched — its arguments belong to the
        // user, not to a palette completion.
        if command_catalog()
            .iter()
            .any(|spec| spec.name.eq_ignore_ascii_case(first))
        {
            return line.clone();
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
        // `first` is a whitespace-split prefix of `rest`, so `first.len()` is a
        // valid char boundary at or before the end; `get` keeps the slice
        // total (clippy's `string_slice` denies a bare `&rest[..]`).
        let tail = rest.get(first.len()..).unwrap_or("");
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
    use crate::test_util::test_app;
    use crossterm::event::{KeyCode, KeyEvent, KeyModifiers};

    /// Set the composer to a command line (leading slash included) the way
    /// typing it would — the buffer holds the `/`, so a command line is just a
    /// buffer that starts with one.
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

    #[test]
    fn binding_for_resolves_legacy_rebinding() {
        let ctrl_m = KeyEvent::new(KeyCode::Char('m'), KeyModifiers::CONTROL);
        let ctrl_o = KeyEvent::new(KeyCode::Char('o'), KeyModifiers::CONTROL);

        assert_eq!(binding_for(&ctrl_m, true), Some("model"));
        assert_eq!(
            binding_for(&ctrl_m, false),
            None,
            "Ctrl+M is unreachable on a legacy terminal"
        );
        assert_eq!(binding_for(&ctrl_o, false), Some("model"));
    }

    #[test]
    fn one_shortcut_per_command_model_selector() {
        // There is exactly ONE shortcut per command: the model selector is
        // Ctrl+M on a kitty terminal and Ctrl+O on a legacy one — never both,
        // and Ctrl+O is unbound on kitty (it is NOT an alias there).
        let ctrl_m = KeyEvent::new(KeyCode::Char('m'), KeyModifiers::CONTROL);
        let ctrl_o = KeyEvent::new(KeyCode::Char('o'), KeyModifiers::CONTROL);

        assert_eq!(binding_for(&ctrl_m, true), Some("model"));
        assert_eq!(
            binding_for(&ctrl_o, true),
            None,
            "Ctrl+O is unbound on a kitty terminal"
        );
        assert_eq!(binding_for(&ctrl_o, false), Some("model"));
    }

    #[test]
    fn binding_for_covers_the_shortcut_table() {
        let ctrl = |c| KeyEvent::new(KeyCode::Char(c), KeyModifiers::CONTROL);
        assert_eq!(binding_for(&ctrl('r'), true), Some("reasoning"));
        assert_eq!(binding_for(&ctrl('s'), true), Some("session"));
        assert_eq!(binding_for(&ctrl('a'), true), Some("account"));
        assert_eq!(binding_for(&ctrl('q'), true), Some("quit"));
        assert_eq!(
            binding_for(&KeyEvent::new(KeyCode::Enter, KeyModifiers::ALT), true),
            Some("continue")
        );
        assert_eq!(
            binding_for(&KeyEvent::new(KeyCode::Up, KeyModifiers::CONTROL), true),
            Some("undo")
        );
        assert_eq!(
            binding_for(&KeyEvent::new(KeyCode::Down, KeyModifiers::CONTROL), true),
            Some("redo")
        );
        // Unbound keys resolve to nothing.
        assert_eq!(
            binding_for(
                &KeyEvent::new(KeyCode::Char('z'), KeyModifiers::CONTROL),
                true
            ),
            None
        );
    }

    #[test]
    fn shortcut_labels_resolve_for_the_terminal() {
        assert_eq!(shortcut_label_for("model", true).as_deref(), Some("Ctrl+M"));
        assert_eq!(
            shortcut_label_for("model", false).as_deref(),
            Some("Ctrl+O")
        );
        assert_eq!(
            shortcut_label_for("continue", true).as_deref(),
            Some("Alt+Enter")
        );
        assert_eq!(shortcut_label_for("undo", true).as_deref(), Some("Ctrl+↑"));
        assert_eq!(shortcut_label_for("redo", true).as_deref(), Some("Ctrl+↓"));
        assert_eq!(shortcut_label_for("quit", true).as_deref(), Some("Ctrl+Q"));
        assert_eq!(shortcut_label_for("nope", true), None);
    }
}
