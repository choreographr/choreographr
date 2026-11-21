//! The overlay and page-layout geometry the TUI draws and hit-tests against:
//! popup sizing, the overlay band layouts with their click mapping, the
//! full-page list rows, and the Chat page's own vertical split.
//!
//! Each subject changes for its own reason, so each gets a file:
//!
//! - `popup` — how large a box is and where it sits: [`PopupSize`],
//!   [`DialogWidth`], [`centered_popup`], and the text-measurement helpers
//!   ([`wrapped_text_rows`] / [`wrapped_lines_rows`]) whose only callers size
//!   boxes.
//! - `overlay` — the modal overlays' internal bands and their click mapping:
//!   [`SelectorLayout`]/[`selector_list_layout`] for the two LIST pickers (the
//!   new-account wizard's provider picker and the model selector),
//!   [`DialogLayout`]/[`dialog_layout`] for a content-sized no-filter dialog,
//!   and the left-click hit-testing that maps a click onto a list row or the
//!   filter cursor ([`selector_click_target`] /
//!   [`apply_selector_left_click`] / [`selector_position_filter_cursor`]).
//! - `hit` — the full-page list click hit-testing for the AI-providers accounts
//!   list and the session-manager list ([`page_list_content_rect`],
//!   [`ai_providers_list_click_index`], [`session_list_click_index`]).
//! - `chat` — the Chat page's vertical layout maths (`chat_page_layout` and the
//!   heights it is built from), which every consumer of the page derives from.
//!
//! Everything here is pure geometry — the renderers, the connection-layer mouse
//! handlers, and the viewport cache all consume the *same* functions, so what is
//! drawn is exactly what is clickable and exactly the height navigation caches.
//! It lives in `state/` rather than `render/` so the connection-layer handlers
//! can use it without an import cycle.
//!
//! Every item is re-exported from `state` (`pub(crate) use layout::*` in
//! `state/mod.rs`), so `crate::state::PopupSize` and friends keep resolving
//! crate-wide.

mod chat;
mod hit;
mod overlay;
mod popup;

// Compatibility layer: every item in the children is re-exported here, and
// `state/mod.rs` re-exports this module wholesale, so `crate::state::X` keeps
// resolving exactly as it did when the whole geometry was one file.
// `chat` declares only inherent `App` methods, so there is nothing to re-export
// from it: its `mod` declaration above is the whole wiring (the same shape as
// `pointer` in `state/mod.rs`).
pub(crate) use hit::*;
pub(crate) use overlay::*;
pub(crate) use popup::*;
