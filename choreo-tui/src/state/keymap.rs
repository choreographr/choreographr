//! The Chat page's logical keymap: one `Alt+<key>` shortcut per command,
//! lowered from a crossterm `KeyEvent`.
//!
//! **Every app command lives on an `Alt+` chord.**  Emacs/readline owns the
//! `Ctrl+<letter>` space (the editing kernel's movement and kill bindings — see
//! `state/input.rs`), so the command layer sits on `Alt+`, which is also
//! unambiguous on *both* kitty and legacy terminals: no terminal folds
//! `Alt+letter` into a control byte, so there is no per-terminal rebinding to
//! resolve (the old `Ctrl+M` ↔ `Ctrl+O` duality is gone).
//!
//! The table is the single source of truth for both Chat-page shortcut dispatch
//! (`binding_for`) and the command palette's right-aligned shortcut labels
//! (`shortcut_label_for`), so a shortcut and its typed `/command` spelling can
//! never drift.  `help` is deliberately NOT in the table — it toggles the help
//! overlay rather than running a catalog command — and `quit` is listed for
//! DISPLAY only (it stays the global pre-dispatch special case in
//! `connection/mod.rs`, so it quits from any page or open modal).

use crossterm::event::{KeyCode, KeyEvent, KeyModifiers};

/// A single logical key within a [`Chord`].
///
/// `Esc`/`PageUp`/`PageDown` (and some `Char`s) are not bound by the current
/// shortcut table, but the type models the full logical-key space so extending
/// the table is a data change, not a type change.
#[allow(dead_code)] // some variants are unbound today (see above)
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Key {
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
/// Constructed with the `alt` helper so the shortcut table reads as the logical
/// binding; `label` renders the human-facing form (e.g. `Alt+Enter`, `Alt+M`).
/// The `ctrl`/`shift` flags are still recorded from the incoming event so a
/// `Ctrl+<letter>` chord can never be mistaken for its `Alt+<letter>` command
/// (and vice-versa) — that is the whole point of moving commands off `Ctrl`.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
struct Chord {
    ctrl: bool,
    alt: bool,
    shift: bool,
    key: Key,
}

impl Chord {
    /// `Alt+<key>`.
    const fn alt(key: Key) -> Self {
        Self {
            ctrl: false,
            alt: true,
            shift: false,
            key,
        }
    }

    /// Human-facing label, e.g. `"Alt+Enter"`, `"Alt+M"`.
    fn label(self) -> String {
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
        // Letters render as the uppercase glyph (`Alt+M`, not `Alt+m`).
        Key::Char(c) => c.to_ascii_uppercase().to_string(),
        Key::Up => "↑".to_string(),
        Key::Down => "↓".to_string(),
        Key::Enter => "Enter".to_string(),
        Key::Esc => "Esc".to_string(),
        Key::PageUp => "PageUp".to_string(),
        Key::PageDown => "PageDown".to_string(),
    }
}

/// One logical shortcut per command, all on `Alt+<key>`.
///
/// The bindings are chosen to avoid readline's `Meta` (Alt) set — `b f d u l c
/// t y . < > { } ~ $ @ ! # & * / ? = \ ^` and the digits — so every `Alt+`
/// command chord is free of the editing kernel's word/case/history bindings.
const SHORTCUTS: &[(Chord, &str)] = &[
    (Chord::alt(Key::Char('m')), "model"),
    (Chord::alt(Key::Char('r')), "reasoning"),
    (Chord::alt(Key::Enter), "continue"),
    (Chord::alt(Key::Up), "undo"),
    (Chord::alt(Key::Down), "redo"),
    (Chord::alt(Key::Char('s')), "session"),
    (Chord::alt(Key::Char('a')), "account"),
    (Chord::alt(Key::Char('q')), "quit"),
];

/// Lower a crossterm `KeyEvent` to a logical [`Chord`], or `None` for keys the
/// shortcut table never binds.
fn chord_from_event(event: &KeyEvent) -> Option<Chord> {
    let key = match event.code {
        // Letters fold to lowercase so a kitty Shift-reporting terminal's
        // `Char('A')+ALT` still matches the lowercase table entry.
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
/// This is the single-source-of-truth lookup that Chat-page shortcut dispatch
/// routes through (`handle_chat_event`), so every shortcut runs the same
/// command path as its typed spelling.
pub(crate) fn binding_for(event: &KeyEvent) -> Option<&'static str> {
    let chord = chord_from_event(event)?;
    SHORTCUTS
        .iter()
        .find(|(logical, _)| *logical == chord)
        .map(|(_, name)| *name)
}

/// The display label for a command's shortcut.  `None` when the command has no
/// shortcut.
pub(crate) fn shortcut_label_for(name: &str) -> Option<String> {
    SHORTCUTS
        .iter()
        .find(|(_, n)| *n == name)
        .map(|(chord, _)| chord.label())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn binding_for_covers_the_shortcut_table() {
        let alt = |c| KeyEvent::new(KeyCode::Char(c), KeyModifiers::ALT);
        assert_eq!(binding_for(&alt('m')), Some("model"));
        assert_eq!(binding_for(&alt('r')), Some("reasoning"));
        assert_eq!(binding_for(&alt('s')), Some("session"));
        assert_eq!(binding_for(&alt('a')), Some("account"));
        assert_eq!(binding_for(&alt('q')), Some("quit"));
        assert_eq!(
            binding_for(&KeyEvent::new(KeyCode::Enter, KeyModifiers::ALT)),
            Some("continue")
        );
        assert_eq!(
            binding_for(&KeyEvent::new(KeyCode::Up, KeyModifiers::ALT)),
            Some("undo")
        );
        assert_eq!(
            binding_for(&KeyEvent::new(KeyCode::Down, KeyModifiers::ALT)),
            Some("redo")
        );
    }

    #[test]
    fn ctrl_letters_are_not_command_shortcuts() {
        // The whole point of the Alt+ move: readline's Ctrl+<letter> editing
        // chords must never resolve to an app command.
        let ctrl = |c| KeyEvent::new(KeyCode::Char(c), KeyModifiers::CONTROL);
        for c in ['a', 'e', 'b', 'f', 'd', 'h', 'k', 's', 'r', 'm', 'q', 't'] {
            assert_eq!(binding_for(&ctrl(c)), None, "Ctrl+{c} is an editing key");
        }
    }

    #[test]
    fn alt_letters_are_never_confused_with_ctrl_letters() {
        // `Ctrl+A` (beginning-of-line) and `Alt+A` (accounts) are distinct.
        let ctrl_a = KeyEvent::new(KeyCode::Char('a'), KeyModifiers::CONTROL);
        let alt_a = KeyEvent::new(KeyCode::Char('a'), KeyModifiers::ALT);
        assert_eq!(binding_for(&ctrl_a), None);
        assert_eq!(binding_for(&alt_a), Some("account"));
    }

    #[test]
    fn unbound_keys_resolve_to_nothing() {
        // A plain letter (no modifier) is typed text, not a shortcut.
        assert_eq!(
            binding_for(&KeyEvent::new(KeyCode::Char('m'), KeyModifiers::NONE)),
            None
        );
        // Alt+<key> with no table entry is unbound too.
        assert_eq!(
            binding_for(&KeyEvent::new(KeyCode::Char('z'), KeyModifiers::ALT)),
            None
        );
    }

    #[test]
    fn shortcut_labels_render_the_alt_chords() {
        assert_eq!(shortcut_label_for("model").as_deref(), Some("Alt+M"));
        assert_eq!(shortcut_label_for("continue").as_deref(), Some("Alt+Enter"));
        assert_eq!(shortcut_label_for("undo").as_deref(), Some("Alt+↑"));
        assert_eq!(shortcut_label_for("redo").as_deref(), Some("Alt+↓"));
        assert_eq!(shortcut_label_for("quit").as_deref(), Some("Alt+Q"));
        // `help` toggles the overlay, not a catalog command — no palette label.
        assert_eq!(shortcut_label_for("help"), None);
        assert_eq!(shortcut_label_for("nope"), None);
    }
}
