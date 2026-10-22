//! `handle_daemon_message`/key-handling tests for the chat page, one child
//! module per subject — the boundaries follow the banner sections the
//! monolithic file was already divided into.
//!
//! `progress` covers the progress bar and token readouts fed from
//! `SessionState`/`TokenUsage` (including the rule that a background session
//! never pollutes the attached display); `entries` covers the Continue / Stop
//! / Undo / Redo key paths; `reasoning_cycle` covers the reasoning-effort
//! cycling key and the model/reasoning/account messages it reads back;
//! `flags` covers the per-session pin/archive flags from `SessionFlagsChanged`;
//! `first_run` follows the first-run chain from a saved key through account
//! selection to the model picker; `model_list` covers the warming-model-list
//! retry that keeps a not-yet-ready list out of the error line.

mod entries;
mod first_run;
mod flags;
mod model_list;
mod progress;
mod reasoning_cycle;
