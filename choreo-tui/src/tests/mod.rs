//! The crate's unit-test suite, split by subject into the child modules below.
//!
//! Every child is compiled only under `cfg(test)` because this parent is
//! declared `#[cfg(test)] mod tests;` in `lib.rs`, so no child needs its own
//! gate. `support` is the shared harness (`test_app`, `add_user_text`,
//! `make_session`) that the children and the `state` tests reach as
//! `crate::tests::support`.
//!
//! To add a test module, create `tests/<subject>.rs` and add one `mod
//! <subject>;` line here — do not declare test modules from `lib.rs`, which
//! keeps that file's module list free of test-only entries.

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
