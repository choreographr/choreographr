//! The `state` subject of the crate suite: the display-state plumbing, split by
//! subject into the child modules below.
//!
//! Each child owns one concern — input, the session switcher, the scroll/height
//! model, the reasoning/tool-result collapse state, image layout,
//! viewport-change classification, streaming, the streaming response auto-pin,
//! the request lifecycle, and the terminal-status records. This parent only
//! declares the modules; fixtures live in `crate::tests::support`, so a child
//! that needs one imports it from there.

mod content_version;
mod display_line;
mod find_turn;
mod height;
mod images;
mod input;
mod markers;
mod model_selector;
mod reasoning;
mod request_lifecycle;
mod response_pin;
mod scroll;
mod session_manager;
mod session_switch;
mod status_height;
mod streaming;
mod terminal_status;
mod tool_result;
mod undo_pruning;
mod viewport;
mod window;
