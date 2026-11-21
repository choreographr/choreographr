//! The TUI's full application state: the `App` struct and its accessors, the
//! input plumbing, and the page/overlay states.  The per-session display model
//! (`SessionDisplayState` and `SessionNote`, with the per-turn visibility
//! overrides), the history pane's render model (a directory module: viewport,
//! render cache, height prefix-sum and markers, scroll, plus the
//! viewport-invalidation policy that drives them), the streaming response
//! auto-pin, the provider catalog, text-input machinery, page states, picker and
//! page geometry (a directory module: popup sizing, overlay bands, list
//! hit-testing and the Chat page's split), displayed-image fetch plumbing, the
//! turn-event handlers, the model selector, the staged
//! account/model/reasoning flow and the reasoning dialog, the pointer-shape
//! selection, and the session lifecycle live in sibling modules (`app`,
//! `display`, `history`, `pin`, `providers`, `input`, `pages`, `pointer`,
//! `model_selector`, `setup_flow`, `reasoning_dialog`, `layout`, `images`,
//! `turn`, `session`) and are re-exported here so the rest of the crate keeps
//! referring to `crate::state::*` unchanged.
//!
//! What stays in this file is the facade: the module wiring, the cross-cutting
//! chrome constants (the status-bar row and the input box's line/width budget),
//! the two text helpers every wrapping consumer shares
//! ([`normalized_display_text`] and [`input_inner_width`]), and the
//! [`UiEvent`] the UI loop receives from the connection task.

use crate::text_filter::normalize_display_text;

mod app;
mod command_palette;
mod display;
mod display_line;
mod draft;
mod history;
mod images;
mod input;
mod keymap;
mod layout;
mod login;
mod model_selector;
mod onboarding;
mod pages;
mod pin;
mod pointer;
mod providers;
mod reasoning_dialog;
mod session;
mod session_manager;
mod setup_flow;
mod streaming;
mod subsession;
mod terminal_advice;
mod terminal_notice;
mod terminal_status;
mod turn;

// The hosting-workspace-manager reporter lives beside the OSC 7501 derivation
// it shares (`terminal_status`), and is re-exported here because `run_app` and
// the UI loop reach it through `state`.
pub use terminal_status::host_report;

// Compatibility layer: every item moved into the sibling modules is
// re-exported here so `crate::state::X` references (in this crate and in the
// crate's `crate::tests` suite) keep resolving exactly as before.
pub(crate) use app::*;
pub(crate) use command_palette::*;
pub(crate) use display::*;
pub(crate) use display_line::DisplayLine;
pub(crate) use history::*;
pub(crate) use images::{
    ImageJobRequest, ImageSlot, slot_source, turn_image_count, turn_image_slots,
};
pub(crate) use input::*;
pub(crate) use keymap::*;
pub(crate) use layout::*;
pub(crate) use login::*;
pub(crate) use model_selector::*;
// The welcome panel's terminal advice needs its type and derivation at the
// launch site (`run_app`) and in the renderer; `onboarding` stays purely setup
// glue.
pub(crate) use pages::*;
pub(crate) use pin::*;
pub(crate) use terminal_notice::{TerminalNotice, terminal_notice};
// `pointer` declares only inherent `App` methods, so there is nothing to
// re-export: its `mod` declaration above is the whole wiring.
pub(crate) use providers::*;
pub(crate) use reasoning_dialog::ReasoningDialogState;
pub(crate) use session::*;
pub(crate) use session_manager::*;
pub(crate) use setup_flow::*;
pub(crate) use streaming::StreamingResponseCache;
pub(crate) use subsession::*;
// The terminal advice tables are read by the welcome notice derivation
// (`state/terminal_notice.rs`) and by the Terminal page renderer
// (`render/terminal_help.rs`), so they are re-exported here like every other
// item that moved into a sibling module.
pub(crate) use terminal_advice::{
    gap_phrases, hosted_pane_sentence, join_phrases, option_key_setting, recommendations,
    screen_sentence, tmux_lines,
};

pub(crate) const STATUS_BAR_HEIGHT: u16 = 1;
pub(crate) const MIN_INPUT_CONTENT_LINES: u16 = 1;
pub(crate) const MAX_INPUT_CONTENT_LINES: u16 = 10;
pub(crate) const PAGE_SCROLL_LINES: usize = 3;

/// Horizontal padding (columns) on each side of the command input box.
///
/// The box draws only top/bottom borders, so the content area loses `INPUT_PAD`
/// columns on each side and nothing more.  Every code path that wraps input
/// text (height estimation, cursor movement, rendering) must use
/// [`input_inner_width`] so they all agree on where word-wrap happens;
/// otherwise a wrapped line can be computed in one path but not another.
pub(crate) const INPUT_PAD: u16 = 2;

/// Store a display-only string, normalizing it exactly once as it enters the
/// state.
///
/// This is the ingest half of the display policy (see
/// [`crate::render::display_text`] for the sink half): text the daemon authors
/// that is only ever *displayed* is made inert here, in the one place it is
/// stored, rather than re-scanned at every widget that draws it.  The normalizer
/// strips control characters, escape sequences and bidi overrides and expands
/// tabs, so a stored value can never occupy a measured column that does not
/// draw, and can never carry a live escape to any later reader — including a
/// non-ratatui consumer that has no sink to route through.
///
/// The normalizer borrows already-normalized text, so the common case (clean
/// text, or text already normalized by an earlier pass) costs one scan and —
/// because the caller hands over an owned `String` anyway — no second
/// allocation.
pub(crate) fn normalized_display_text(text: impl Into<String>) -> String {
    let text = text.into();
    match normalize_display_text(&text) {
        // Already normalized: keep the caller's allocation instead of copying.
        std::borrow::Cow::Borrowed(_) => text,
        std::borrow::Cow::Owned(normalized) => normalized,
    }
}

/// Inner content width (columns) of the command input box for a given terminal
/// width: terminal width minus the horizontal padding on both sides.
pub(crate) fn input_inner_width(term_width: u16) -> usize {
    term_width.saturating_sub(INPUT_PAD * 2) as usize
}

pub(crate) enum UiEvent {
    Daemon(Box<choreo_proto::DaemonMessage>),
    ReaderClosed,
    /// A transient status-line message from the connection task, used while
    /// the connection is not yet established (the daemon-autostart wait) —
    /// the reader thread cannot paint the UI itself, so progress feedback
    /// travels to the UI loop as an event. Unlike daemon messages this does
    /// NOT scroll or mutate any view: it just sets the status line. Marked
    /// transient ([`App::status_is_transient`]) so the first real daemon
    /// message clears it — the "daemon started" reassurance must not outlive
    /// the connection it describes.
    Status(String),
}
