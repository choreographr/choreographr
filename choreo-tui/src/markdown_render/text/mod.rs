//! Text utilities for the renderers, split by subject:
//!
//! - [`wrap`] — the wrappers that turn text into rows of at most a given display
//!   width: plain-text wrapping (verbatim, whitespace preserved) and styled
//!   word-wrapping, both recording the per-row `LineJoin` copy metadata.
//! - [`measure`] — the display-width and visual-height math the wrappers, the
//!   code-box padding, and the history viewport all measure with.
//! - [`ansi`] — ANSI-escape-coded text parsed into styled lines, then wrapped.
//!
//! `wrap` builds on `measure` (a wrap decision is a width comparison) and `ansi`
//! builds on both; nothing here reaches back into `markdown_render`'s block
//! renderers. The items are re-exported below so `markdown_render`'s single
//! `pub(crate) use text::*;` keeps hoisting them for its own callers.

mod ansi;
mod measure;
mod wrap;

pub(crate) use ansi::*;
pub(crate) use measure::*;
pub(crate) use wrap::*;
