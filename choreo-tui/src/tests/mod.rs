//! The crate's unit-test suite, split by subject into the child modules below.
//!
//! Every child is compiled only under `cfg(test)` because this parent is
//! declared `#[cfg(test)] mod tests;` in `lib.rs`, so no child needs its own
//! gate. `support` is the shared harness (`test_app`, `add_user_text`,
//! `make_session`, `make_session_full`, `insert_turn`) that the children reach
//! as `crate::tests::support` — one definition site and one import path for the
//! whole suite, so a fixture never depends on which module a test lives in.
//!
//! What belongs here: a test that reaches the code under test through
//! `crate::…` paths — so the item is public or `pub(crate)` and the crate
//! suite can see it — organized by **subject**. To add one, create
//! `tests/<subject>.rs` and add one `mod <subject>;` line here; do not declare
//! test modules from `lib.rs`, which keeps that file's module list free of
//! test-only entries. A subject too large for one file becomes a directory
//! module (`tests/<subject>/mod.rs` declaring one child per subject, as
//! `tests/state/` does); fixtures shared across children live in `support`,
//! never in a subject's parent module.
//!
//! What does not: a test that needs a module's *private* items. That is a
//! white-box test and cannot live here — a private item is not reachable from
//! `crate::tests` at all — so it stays in its own module's `#[cfg(test)] mod
//! tests` (`render/tests.rs`, `selection/tests.rs`, `markdown_render/tests/`,
//! and the inline blocks throughout `src/`).
//!
//! The split itself is deliberate, and it is the standing reason for one
//! module per subject: each top-level test file is an extra relink and an
//! extra clippy re-check on every library change, and a file name says nothing
//! about which subject a test covers. One module per subject keeps both costs
//! proportionate to the subject and makes a test's home obvious from its
//! subject.

pub(crate) mod support;

mod ai_providers;
mod app;
mod cli;
mod daemon;
mod input;
mod lib_api;
mod login;
mod model_selector;
mod render;
mod session;
mod state;
