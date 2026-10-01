//! LaTeX math handling: deciding whether a pulldown-cmark `$…$` / `$$…$$` span
//! is real mathematics or prose, and pretty-printing a real expression to a
//! best-effort Unicode approximation for terminal display.
//!
//! The classifier ([`looks_like_math`]) exists because pulldown-cmark's math
//! rule keys only on whitespace adjacency, so `$`-heavy prose yields spurious
//! spans; [`normalize_math_event`] re-emits those as literal text. The printer
//! ([`render_math_pretty`]) is a total, depth-bounded, table-driven parser that
//! falls back to raw source for anything it cannot map.

mod detect;
mod pretty;

pub(crate) use detect::normalize_math_event;
pub use pretty::render_math_pretty;
