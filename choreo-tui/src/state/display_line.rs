//! [`DisplayLine`]: one piece of display-only message text, normalized for
//! display as it is stored.
//!
//! Every text field the TUI holds purely in order to draw it — the chat
//! status/error line, the AI-providers sub-modal error lines, the model
//! selector's error row — has the same lifetime: daemon-authored bytes come in
//! once and are read by several widgets (and, potentially, by a non-ratatui
//! consumer with no widget sink to route through).  Wrapping such a field in
//! this type makes ingest the choke point *structurally*: the only way to put
//! text in is [`DisplayLine::set`], which normalizes it, so no call site can
//! store raw control characters by accident and no reader has to re-run the
//! normalizer.
//!
//! Identity-bearing values (model id, account slug, session title, tool name,
//! tool-call id) are deliberately NOT held in this type: they must stay byte
//! exact in state because they are sent back to the daemon or compared, so they
//! are normalized at the draw sink instead (see `crate::render::display_text`).

use super::normalized_display_text;

/// Optional display-only message text, always stored normalized.
#[derive(Debug, Default, Clone, PartialEq, Eq)]
pub(crate) struct DisplayLine(Option<String>);

impl DisplayLine {
    /// An empty line.
    pub(crate) fn none() -> Self {
        Self(None)
    }

    /// Replace the text, normalizing it for display as it is stored (see
    /// [`normalized_display_text`]).  An empty string is stored as a present,
    /// empty line — "there is a message, it is blank" is distinguishable from
    /// "there is no message".
    pub(crate) fn set(&mut self, text: impl Into<String>) {
        self.0 = Some(normalized_display_text(text));
    }

    /// Drop the text entirely.
    pub(crate) fn clear(&mut self) {
        self.0 = None;
    }

    /// The stored text, already normalized.
    pub(crate) fn get(&self) -> Option<&str> {
        self.0.as_deref()
    }
}
