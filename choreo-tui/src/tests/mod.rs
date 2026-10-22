//! The crate's unit-test suite, split by subject into the child modules below.
//!
//! Every child is compiled only under `cfg(test)` because this parent is
//! declared `#[cfg(test)] mod tests;` in `lib.rs`, so no child needs its own
//! gate. `support` is the shared harness (`test_app`, `add_user_text`,
//! `make_session`) that the children reach as `crate::tests::support`.
//!
//! What belongs here: a test that reaches the code under test through
//! `crate::…` paths — so the item is public or `pub(crate)` and the crate
//! suite can see it — organized by **subject**. To add one, create
//! `tests/<subject>.rs` and add one `mod <subject>;` line here; do not declare
//! test modules from `lib.rs`, which keeps that file's module list free of
//! test-only entries. A subject too large for one file becomes a directory
//! module (`tests/<subject>/mod.rs` declaring one child per subject, as
//! `tests/state/` does), and the parent then also holds the fixtures those
//! children share.
//!
//! What does not: a test that needs a module's *private* items. That is a
//! white-box test and cannot live here — a private item is not reachable from
//! `crate::tests` at all — so it stays in its own module's `#[cfg(test)] mod
//! tests` (`render/tests.rs`, `selection/tests.rs`, `markdown_render/tests/`,
//! and the inline blocks throughout `src/`).
//!
//! The split itself is deliberate: a monolithic suite file per area
//! (`app_tests.rs`, `session_tests.rs`, …) is one extra relink and one extra
//! clippy re-check, and a file name says nothing about which subject a test
//! covers. One module per subject keeps both costs proportionate to the
//! subject and makes a test's home obvious from its subject.

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
