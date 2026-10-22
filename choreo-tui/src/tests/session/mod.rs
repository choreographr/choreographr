//! `App`-level session tests, one child module per subject — the boundaries
//! follow the banner sections the monolithic file was already divided into.
//!
//! `manager` covers `SessionManagerState`'s list/detail model plus the Session
//! Manager page's key, mouse and scroll behaviour; `streaming` pins the
//! multi-session streaming rule that switching keeps the accumulated content
//! (and the viewport anchoring that goes with it); `background_meta` pins the
//! rule that metadata for a background session never writes the global
//! status/error line; `lifecycle` covers which `SessionCreated` a client may
//! attach to — the connection-level `session_id: None` gating, the refusal to
//! auto-attach to an agent-spawned sub-session, and the broadcast
//! `SessionCreated` that must never hijack the view; `subsession` covers the
//! sub-session↔parent relationship once a sub-session runs (finishing
//! auto-switches back to the parent) together with the `handle_sessions`
//! auto-attach rules that prefer a top-level session over a child.
//!
//! The last four banner sections are one broad subject — what a client may
//! attach to — but they are split across `lifecycle` and `subsession` because
//! the trailing section also carries the generic `handle_sessions` attach rules
//! (empty list, archived-only, child fallback), which are about the
//! parent/child relationship rather than about `SessionCreated` gating; keeping
//! them beside the sub-session finish tests leaves `lifecycle` a single
//! coherent subject instead of a 900-line catalogue.

mod background_meta;
mod lifecycle;
mod manager;
mod streaming;
mod subsession;
