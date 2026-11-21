//! The API-key modal (AI providers page): the account the key is being entered
//! for and the masked input buffer it is typed into.

use crate::state::{DisplayLine, InputBuffer};
use zeroize::Zeroize;

/// The API-key modal (AI providers page).  Open ⇔ `target` is `Some(account
/// name)`.  Reached from `c` on an existing account or auto-opened right after
/// the new-account wizard creates one.  The key is masked while typing and
/// sent as typed plaintext on save (the daemon encrypts it to its own binding
/// key; see `build_add_credential_message`).
pub(crate) struct CredentialModalState {
    /// The account the key is for; `Some` means the modal is open.
    pub(crate) target: Option<String>,
    pub(crate) input: InputBuffer,
    /// Modal-scoped error text, stored normalized for display (see
    /// [`DisplayLine`]); it is only ever drawn.
    pub(crate) error: DisplayLine,
}

impl CredentialModalState {
    pub(crate) fn new() -> Self {
        Self {
            target: None,
            input: InputBuffer::new(),
            error: DisplayLine::none(),
        }
    }

    pub(crate) fn is_open(&self) -> bool {
        self.target.is_some()
    }

    pub(crate) fn open(&mut self, account_name: String) {
        self.target = Some(account_name);
        self.wipe_input();
        self.error.clear();
    }

    pub(crate) fn close(&mut self) {
        self.target = None;
        self.wipe_input();
        self.error.clear();
    }

    /// Discard the typed key and wipe its bytes from the input buffer's heap
    /// allocation before the `String` is dropped.  The daemon already zeroizes
    /// its own stored `ServiceCredential` copies (choreo-keystore's
    /// `#[zeroize(drop)]` on `ServiceCredential` plus the daemon's explicit key
    /// wipes); this covers the TUI's transient copy of a pasted/typed API key,
    /// which the daemon never sees.  Called by the modal's own open/close and
    /// by the connection layer's Enter handler.
    pub(crate) fn wipe_input(&mut self) {
        // Take the typed key's String out and convert it to its backing
        // `Vec<u8>` so the WHOLE heap allocation can be zeroized — including
        // the spare capacity beyond `len`, which may hold remnants of a longer
        // key deleted while editing (`String::zeroize()` only covers the
        // current length).  Growing to `capacity` with zeros first makes the
        // subsequent `zeroize()` reach every byte, so the freed allocation is
        // all zeros rather than a mix of zeros and stale key material.
        let mut bytes = std::mem::take(&mut self.input.text).into_bytes();
        bytes.resize(bytes.capacity(), 0);
        bytes.zeroize();
        self.input = InputBuffer::new();
    }
}
