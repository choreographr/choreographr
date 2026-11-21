//! The Polkadot-account import modal (AI providers page, `p`): its three steps
//! ([`PolkadotImportStep`]) and the state each carries
//! ([`PolkadotImportState`]).

use crate::state::DisplayLine;
use crossterm::event::KeyEvent;
use tui_prompts::{State, TextState};
use zeroize::Zeroize;

/// Steps of the Polkadot-account import modal (AI providers page, `p`).
/// The wizard walks name → keystore path → password, each a single text
/// field that Enter confirms.  Esc on the password/path steps backs out one
/// step; Esc on the name step cancels the whole flow.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum PolkadotImportStep {
    /// Step 1: the account/credential name the Substrate credential is stored
    /// under (the daemon's credential service key).
    Name,
    /// Step 2: the path to the Polkadot-JS `.json` keystore export.
    Path,
    /// Step 3: the account's password (masked while typed, never echoed or
    /// logged; the TUI decrypts client-side and never sends it to the daemon).
    Password,
}

/// The Polkadot-account import modal (AI providers page, `p`).  Open
/// ⇔ `open` is `true`.  Reached from the `p` binding on the accounts page.
/// Three sequential steps capture the name, the keystore export path and the
/// account password; on the final step the TUI reads the file, decrypts it
/// with `choreo_keystore::substrate::import_from_json` and sends the
/// resulting Substrate credential over the same encrypted
/// `ClientMessageType::AddCredential` path the API-key modal uses.
pub(crate) struct PolkadotImportState {
    pub(crate) open: bool,
    pub(crate) step: PolkadotImportStep,
    /// Account/credential name (step 1).
    pub(crate) name: TextState<'static>,
    /// Keystore export path (step 2).
    pub(crate) path: TextState<'static>,
    /// Account password (step 3).  Wiped on close.
    pub(crate) password: TextState<'static>,
    /// Modal-scoped error text (validation, file-read, decryption failures),
    /// stored normalized for display (see [`DisplayLine`]); it is only ever
    /// drawn.
    pub(crate) error: DisplayLine,
}

impl PolkadotImportState {
    pub(crate) fn new() -> Self {
        Self {
            open: false,
            step: PolkadotImportStep::Name,
            name: TextState::default(),
            path: TextState::default(),
            password: TextState::default(),
            error: DisplayLine::none(),
        }
    }

    pub(crate) fn is_open(&self) -> bool {
        self.open
    }

    /// Open the wizard fresh: step 1 (name), every field reset, no error.
    pub(crate) fn open(&mut self) {
        self.open = true;
        self.reset_all();
        self.focus_current();
    }

    /// Dismiss the wizard entirely (Esc on the name step), discarding any
    /// partial state and wiping the password.
    pub(crate) fn close(&mut self) {
        self.open = false;
        self.reset_all();
    }

    /// Reset every field the wizard owns.  Shared by the open (fresh start)
    /// and close (discard) paths so the two can never drift apart.
    fn reset_all(&mut self) {
        self.step = PolkadotImportStep::Name;
        self.name = TextState::default();
        self.path = TextState::default();
        self.wipe_password();
        self.error.clear();
    }

    /// Discard the typed password and wipe its bytes from the heap before the
    /// `String` is dropped.  Mirror of `CredentialModalState::wipe_input`:
    /// the password never leaves the TUI, but the transient copy in the input
    /// buffer is still secret material, so it is zeroized on close/reset.
    pub(crate) fn wipe_password(&mut self) {
        // `TextState.value_mut()` hands back the underlying `String`; take it
        // out and convert it to its backing `Vec<u8>` so the WHOLE allocation
        // — including spare capacity that may hold remnants of a longer
        // password deleted while editing — is zeroized, not just `len` bytes.
        let mut bytes = std::mem::take(self.password.value_mut()).into_bytes();
        bytes.resize(bytes.capacity(), 0);
        bytes.zeroize();
        self.password = TextState::default();
    }

    /// Advance to the next step, focusing its input field.  Called by the
    /// Enter handler after a step validates.
    pub(crate) fn advance(&mut self) {
        self.error.clear();
        self.step = match self.step {
            PolkadotImportStep::Name => PolkadotImportStep::Path,
            PolkadotImportStep::Path => PolkadotImportStep::Password,
            // The password step's Enter is handled by the submit path, never
            // here — it is not a validation-then-advance step.
            PolkadotImportStep::Password => PolkadotImportStep::Password,
        };
        self.focus_current();
    }

    /// Back out one step (Esc on the path/password steps), keeping each
    /// previously-entered field so the user can correct it.
    pub(crate) fn back(&mut self) {
        self.error.clear();
        self.step = match self.step {
            PolkadotImportStep::Name => PolkadotImportStep::Name,
            PolkadotImportStep::Path => PolkadotImportStep::Name,
            PolkadotImportStep::Password => PolkadotImportStep::Path,
        };
        self.focus_current();
    }

    /// Route a key to the active step's text field (characters, backspace,
    /// arrow cursor movement, word deletes).
    pub(crate) fn handle_key(&mut self, key: KeyEvent) {
        let field = match self.step {
            PolkadotImportStep::Name => &mut self.name,
            PolkadotImportStep::Path => &mut self.path,
            PolkadotImportStep::Password => &mut self.password,
        };
        field.handle_key_event(key);
    }

    /// Trimmed value of the active step's text field.
    pub(crate) fn value(&self) -> String {
        match self.step {
            PolkadotImportStep::Name => self.name.value().trim().to_string(),
            PolkadotImportStep::Path => self.path.value().trim().to_string(),
            PolkadotImportStep::Password => self.password.value().to_string(),
        }
    }

    /// Focus the active step's text field so key events edit it.
    fn focus_current(&mut self) {
        let field = match self.step {
            PolkadotImportStep::Name => &mut self.name,
            PolkadotImportStep::Path => &mut self.path,
            PolkadotImportStep::Password => &mut self.password,
        };
        field.focus();
    }

    /// Insert pasted text into the active step's field (the keystore path is
    /// often pasted, and the password may be too), mirroring the credential
    /// modal's paste handling.  `TextState` exposes only the `State` trait
    /// (no `insert_str_at_cursor`), so the paste is done manually at the
    /// cursor position.
    pub(crate) fn handle_paste(&mut self, data: &str) {
        let field = match self.step {
            PolkadotImportStep::Name => &mut self.name,
            PolkadotImportStep::Path => &mut self.path,
            PolkadotImportStep::Password => &mut self.password,
        };
        let pos = field.position();
        let suffix = field.value().chars().skip(pos).collect::<String>();
        let truncated = field.value().chars().take(pos).collect::<String>();
        let mut new_value = truncated;
        new_value.push_str(data);
        new_value.push_str(&suffix);
        *field.value_mut() = new_value;
        *field.position_mut() = pos + data.chars().count();
    }

    /// Mutable reference to the active step's text field, for the renderer.
    pub(crate) fn field(&mut self) -> &mut TextState<'static> {
        match self.step {
            PolkadotImportStep::Name => &mut self.name,
            PolkadotImportStep::Path => &mut self.path,
            PolkadotImportStep::Password => &mut self.password,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::{PolkadotImportState, PolkadotImportStep};
    use tui_prompts::State;

    #[test]
    fn polkadot_import_wizard_steps_and_password_wipe() {
        let mut w = PolkadotImportState::new();
        assert!(!w.is_open());
        w.open();
        assert!(w.is_open());
        assert_eq!(w.step, PolkadotImportStep::Name);

        // Value is trimmed for the name/path, raw for password.
        w.name.value_mut().push_str("  main  ");
        assert_eq!(w.value(), "main");
        w.advance();
        assert_eq!(w.step, PolkadotImportStep::Path);
        w.path.value_mut().push_str("/tmp/k.json");
        assert_eq!(w.value(), "/tmp/k.json");
        w.advance();
        assert_eq!(w.step, PolkadotImportStep::Password);

        // Typing into the password step populates the password field.
        w.password.value_mut().push_str("secret");
        assert_eq!(w.value(), "secret");

        // Back goes to the path step, keeping the path.
        w.back();
        assert_eq!(w.step, PolkadotImportStep::Path);
        assert_eq!(w.path.value(), "/tmp/k.json");

        // Close wipes the password to empty.
        w.close();
        assert!(!w.is_open());
        assert_eq!(w.password.value(), "");
    }

    #[test]
    fn polkadot_import_paste_inserts_at_cursor() {
        let mut w = PolkadotImportState::new();
        w.open();
        w.name.value_mut().push_str("ac");
        *w.name.position_mut() = 2; // cursor at end
        w.handle_paste("count");
        assert_eq!(w.name.value(), "account");

        // Paste on the path step lands in the path field.
        w.advance();
        w.handle_paste("/tmp/x");
        assert_eq!(w.path.value(), "/tmp/x");
    }
}
