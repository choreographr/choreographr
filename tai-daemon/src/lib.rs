pub mod db;
pub mod openai;
pub mod context;
pub mod daemon;
mod requests;
mod server;
mod sessions;
pub mod tools;

pub use crate::server::run_server;
pub use crate::daemon::{DaemonCommand, DaemonState};
pub use crate::sessions::{SessionCommand, SessionMetadata, ActiveSessionEntry, SessionState, session_main};
pub use crate::requests::{REQUEST_IMAGE_BYTES, REQUEST_IMAGE_HEIGHT, REQUEST_IMAGE_MIME_TYPE, REQUEST_IMAGE_WIDTH};
pub use tai_keystore::Keystore;
pub use crate::tools::git::{
    execute_git_add_tool,
    execute_git_commit_tool,
    execute_git_diff_tool,
    execute_git_log_tool,
    execute_git_push_tool,
    execute_git_status_tool,
};

#[cfg(test)]
pub(crate) use crate::tools::{
    sha256_hex,
};

#[cfg(test)]
pub(crate) use crate::tools::fs::{
    execute_read_file_range_tool,
    execute_write_file_tool,
    execute_edit_file_tool,
};

#[cfg(test)]
pub(crate) use crate::tools::http::execute_http_request_tool;

#[cfg(test)]
mod tests;
