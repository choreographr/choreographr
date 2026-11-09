//! The Chat page's logical keymap: one `Alt+[Shift+]<key>` shortcut per command,
//! lowered from a crossterm `KeyEvent`.
//!
//! **Every app command lives on an `Alt+` chord.**  Emacs/readline owns the
//! `Ctrl+<letter>` space (the editing kernel's movement and kill bindings — see
//! `state/input.rs`), so the command layer sits on `Alt+`, which is also
//! unambiguous on *both* kitty and legacy terminals: no terminal folds
//! `Alt+letter` into a control byte, so there is no per-terminal rebinding to
//! resolve (the old `Ctrl+M` ↔ `Ctrl+O` duality is gone).
//!
//! **Shift is part of the chord.**  `Alt+Shift+R` and `Alt+R` are different
//! commands, and a chord matches only the exact modifier set it was declared
//! with.  Both terminal encodings can express the distinction, by different
//! means: a kitty-protocol terminal reports `CSI 114;4 u` (SHIFT in the modifier
//! field), while a legacy terminal sends `ESC R` — shift lives in the *glyph's
//! case*.  [`chord_from_event`] therefore infers SHIFT from an uppercase glyph
//! as well as from the modifier bit, so one chord binds identically on both.
//! The consequence of reading case is that Caps Lock is indistinguishable from
//! Shift on a legacy terminal (kitty terminals do not report lock modifiers for
//! text keys), which is accepted: a caps-lock user's `Alt+R` reads as
//! `Alt+Shift+R` there, and a shifted chord the table does not bind (e.g.
//! `Alt+Shift+M`) is simply unbound.
//!
//! The table is the single source of truth for both Chat-page shortcut dispatch
//! (`binding_for`), the command palette's right-aligned shortcut labels
//! (`shortcut_label_for`), and the Help modal's listed chords, so a shortcut
//! and its typed `/command` spelling can never drift.  `quit` is listed for
//! DISPLAY only (it stays the global pre-dispatch special case in
//! `connection/mod.rs`, so it quits from any page or open modal).

use crossterm::event::{KeyCode, KeyEvent, KeyModifiers};

/// A single logical key within a [`Chord`].
///
/// `Esc`/`PageUp`/`PageDown` (and some `Char`s) are not bound by the current
/// shortcut table, but the type models the full logical-key space so extending
/// the table is a data change, not a type change.
// Some variants are unbound today (see above); which ones are read varies by
// build target, so `dead_code` cannot be an `expect` — kept as an `allow`.
#[allow(clippy::allow_attributes)]
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
/// Constructed with the `alt` / `alt_shift` helpers so the shortcut table reads
/// as the logical binding; `label` renders the human-facing form (e.g.
/// `Alt+Enter`, `Alt+Shift+R`).  The `ctrl`/`alt`/`shift` flags are all recorded
/// from the incoming event, so a chord matches only its exact modifier set: a
/// `Ctrl+<letter>` chord can never be mistaken for its `Alt+<letter>` command
/// (that is the whole point of moving commands off `Ctrl`), and a shifted chord
/// can never be mistaken for its unshifted sibling.
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

    /// `Alt+Shift+<key>`.
    const fn alt_shift(key: Key) -> Self {
        Self {
            ctrl: false,
            alt: true,
            shift: true,
            key,
        }
    }

    /// Human-facing label, e.g. `"Alt+Enter"`, `"Alt+Shift+R"`.
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

/// One logical shortcut per command, on `Alt+[Shift+]<key>`.
///
/// The bindings are chosen to avoid readline's `Meta` (Alt) set — `b f d u l c
/// y . < > { } ~ $ @ ! # & * / ? = \ ^` and the digits — so every `Alt+`
/// command chord is free of the editing kernel's word/case/history bindings.
/// `alt+t` is the one exception: readline binds `Meta-t` to `transpose-chars`,
/// but the shortcut table is consulted *before* the editing arm in
/// `connection/chat.rs`, so the command chord wins and `Ctrl+T` (the editing
/// transpose) is untouched.
///
/// `Alt+Shift+R` (`reasoning-toggle`) sits next to `Alt+R` (the reasoning-effort
/// cycle) on purpose: the two are the reasoning pair, and Shift is what tells
/// them apart.  Note the editing kernel stays shift-INSENSITIVE (`Ctrl+Shift+D`
/// is still `Ctrl+D`): legacy terminals cannot express that distinction for
/// control chords, and the kernel's contract is legacy equivalence.
const SHORTCUTS: &[(Chord, &str)] = &[
    (Chord::alt(Key::Char('m')), "model"),
    (Chord::alt(Key::Char('r')), "reasoning"),
    (Chord::alt_shift(Key::Char('r')), "reasoning-toggle"),
    (Chord::alt(Key::Enter), "continue"),
    (Chord::alt(Key::Up), "undo"),
    (Chord::alt(Key::Down), "redo"),
    (Chord::alt(Key::Char('s')), "session"),
    (Chord::alt(Key::Char('a')), "account"),
    (Chord::alt(Key::Char('t')), "terminal"),
    (Chord::alt(Key::Char('h')), "help"),
    (Chord::alt(Key::Char('q')), "quit"),
];

/// Lower a crossterm `KeyEvent` to a logical [`Chord`], or `None` for keys the
/// shortcut table never binds.
fn chord_from_event(event: &KeyEvent) -> Option<Chord> {
    // Whether SHIFT is set, including when it rides on the glyph's case rather
    // than in the modifier field.  A legacy terminal has no shift bit for a
    // printer key: `Alt+Shift+r` arrives as `ESC R`, which crossterm reports as
    // `Char('R')+ALT`.  Reading the case is what lets one chord bind identically
    // on legacy and kitty terminals (the latter sends SHIFT explicitly).  The
    // cost is that Caps Lock looks like Shift on a legacy terminal, since both
    // only change the byte's case.
    let mut shift = event.modifiers.contains(KeyModifiers::SHIFT);
    let key = match event.code {
        // Letters fold to lowercase so the table's logical key is case-free;
        // the case itself survives as the inferred SHIFT above.
        KeyCode::Char(c) => {
            shift |= c.is_ascii_uppercase();
            Key::Char(c.to_ascii_lowercase())
        }
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
        shift,
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

    /// Shift is part of the chord: the shifted form reaches its own command from
    /// BOTH terminal encodings — kitty's explicit SHIFT modifier, and the legacy
    /// shifted glyph (`Char('R')+ALT`, what an `ESC R` byte decodes to).
    #[test]
    fn shift_is_a_real_modifier_in_a_chord() {
        let alt_shift =
            |c| KeyEvent::new(KeyCode::Char(c), KeyModifiers::ALT | KeyModifiers::SHIFT);
        assert_eq!(binding_for(&alt_shift('r')), Some("reasoning-toggle"));

        // The legacy encoding of the same press: the glyph carries the shift.
        assert_eq!(
            binding_for(&KeyEvent::new(KeyCode::Char('R'), KeyModifiers::ALT)),
            Some("reasoning-toggle")
        );

        // …and the unshifted press is still its own command.
        assert_eq!(
            binding_for(&KeyEvent::new(KeyCode::Char('r'), KeyModifiers::ALT)),
            Some("reasoning")
        );
        // A shifted chord never aliases its unshifted sibling.
        assert_eq!(binding_for(&alt_shift('m')), None);
    }

    /// The label the Help page and palette render for the shifted chord.
    #[test]
    fn shifted_chord_label_reads_as_alt_shift() {
        assert_eq!(
            shortcut_label_for("reasoning-toggle").as_deref(),
            Some("Alt+Shift+R")
        );
        assert_eq!(shortcut_label_for("reasoning").as_deref(), Some("Alt+R"));
    }

    #[test]
    fn binding_for_covers_the_shortcut_table() {
        let alt = |c| KeyEvent::new(KeyCode::Char(c), KeyModifiers::ALT);
        assert_eq!(binding_for(&alt('m')), Some("model"));
        assert_eq!(binding_for(&alt('r')), Some("reasoning"));
        assert_eq!(binding_for(&alt('s')), Some("session"));
        assert_eq!(binding_for(&alt('a')), Some("account"));
        assert_eq!(binding_for(&alt('t')), Some("terminal"));
        assert_eq!(binding_for(&alt('h')), Some("help"));
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
        assert_eq!(shortcut_label_for("terminal").as_deref(), Some("Alt+T"));
        assert_eq!(shortcut_label_for("help").as_deref(), Some("Alt+H"));
        assert_eq!(shortcut_label_for("nope"), None);
    }
}
