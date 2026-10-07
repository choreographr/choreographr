//! Mouse text selection over the chat history pane.
//!
//! Selecting text with the mouse in a raw-mode TUI is an *app-level* feature:
//! once `EnableMouseCapture` is on, the terminal forwards drag events to the
//! app instead of selecting natively, so the app must (a) track the drag,
//! (b) map the screen rectangle back to the text it covers, and (c) hand the
//! text to the clipboard itself.  This mirrors opencode's select-to-copy.
//!
//! Scope (v1): the chat *history pane* only — the input box and overlay
//! popups are out.  The selection is stored in *content* coordinates — a
//! global content line in `[0, total history height)` plus a viewport column
//! — NOT screen coordinates: mouse events arrive in viewport space, so each
//! is mapped to the content it covers the moment it is processed
//! ([`screen_to_content`], the exact inverse of the click hit-testing the
//! TUI already does).  Storing content coordinates is what lets the
//! selection survive scrolling: the anchor stays pinned to the text it was
//! placed on, while the live drag head re-resolves to the content under the
//! cursor — on wheel events immediately, and on content-induced scrolls
//! (streaming growth, appended turns) at draw time via [`follow_cursor`] —
//! and the draw-time highlight re-evaluates against the current scroll every
//! frame, so what is highlighted is exactly what gets copied.  Resolving
//! fresh at gesture end also means streaming that lands mid-drag cannot
//! corrupt the result: each row maps to whatever content is current then.
//!
//! Coordinates: the history pane's lines are pre-wrapped at `content_width`
//! (viewport width − 9) and drawn in a non-wrapping `Paragraph`, so every
//! semantic line occupies exactly one visual row; the code still walks the
//! cached `visual_offsets` so a hypothetical multi-row line maps correctly.
//!
//! The module is split by concern (mirroring `markdown_render/`) into a
//! façade (this file, which re-exports every crate-internal item so callers
//! reach them through `selection::…` unchanged) and four submodules:
//! [`gesture`] (screen↔content mapping and the mouse state machine),
//! [`extract`] (content-line resolution and per-line text extraction),
//! [`table`] (the data-table cell model, reading-order fill, and the
//! once-per-frame table highlight), and [`highlight`] (the draw-time
//! restyling).  [`text`] holds the shared column-slicing helpers.  The unit
//! tests live in the [`tests`] submodule.

mod extract;
mod gesture;
mod highlight;
mod table;
mod text;

// Re-export every submodule item so siblings, the test module, and the rest of
// the crate reach them through `selection::…` — the paths external callers
// (`state/`, `render/`, `connection/`) already use.  Single-segment glob
// re-exports (`pub(crate) use gesture::*`) are the idiomatic way to hoist a
// private submodule's crate-internal API into its parent.
pub(crate) use extract::*;
pub(crate) use gesture::*;
pub(crate) use highlight::*;
pub(crate) use table::*;
pub(crate) use text::*;

#[cfg(test)]
mod tests;
