pub mod context;
pub mod daemon;
pub mod db;
pub mod openai;
mod requests;
mod server;
mod sessions;
pub mod tools;

pub use crate::daemon::{DaemonCommand, DaemonState};
pub use crate::requests::{
    REQUEST_IMAGE_BYTES, REQUEST_IMAGE_HEIGHT, REQUEST_IMAGE_MIME_TYPE, REQUEST_IMAGE_WIDTH,
};
pub use crate::server::run_server;
pub use crate::sessions::{
    ActiveSessionEntry, SessionCommand, SessionMetadata, SessionState, session_main,
};
pub use crate::tools::bash::execute_bash_tool;
pub use crate::tools::nu::execute_nu_tool;
pub use crate::tools::git::{
    execute_git_add_tool, execute_git_commit_tool, execute_git_diff_tool, execute_git_log_tool,
    execute_git_push_tool, execute_git_status_tool,
};
pub use tai_keystore::Keystore;

#[cfg(test)]
pub(crate) use crate::tools::sha256_hex;

#[cfg(test)]
pub(crate) use crate::tools::fs::{
    execute_edit_file_tool, execute_read_file_range_tool, execute_write_file_tool,
};

#[cfg(test)]
pub(crate) use crate::tools::http::execute_http_request_tool;

#[cfg(test)]
mod tests;
