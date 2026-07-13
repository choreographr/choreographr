pub mod accounts;
pub mod anthropic;
pub mod context;
pub mod daemon;
pub mod db;
pub mod diff_util;
pub mod google;
pub mod mcp;
pub mod metrics;
pub mod mistral;
pub mod openai;
pub mod providers;
mod requests;
pub mod retry;
pub mod server;
mod sessions;
pub mod tools;

pub use crate::daemon::{DaemonCommand, DaemonState};
pub use crate::requests::{
    REQUEST_IMAGE_BYTES, REQUEST_IMAGE_HEIGHT, REQUEST_IMAGE_MIME_TYPE, REQUEST_IMAGE_WIDTH,
};
pub use crate::server::run_server;
pub use crate::sessions::{
    ActiveSessionEntry, ChildResult, RequestContext, SessionCommand, SessionMetadata, SessionState,
    session_main,
};
pub use crate::tools::exec::{ExecArgs, execute_exec_tool};
pub use crate::tools::find::{FindArgs, execute_find_tool};
pub use crate::tools::fish::{FishArgs, execute_fish_tool};
pub use crate::tools::git::{
    GitAddArgs, GitCommitArgs, GitDiffArgs, GitLogArgs, GitPushArgs, GitRepoArgs,
    execute_git_add_tool, execute_git_commit_tool, execute_git_diff_tool, execute_git_log_tool,
    execute_git_push_tool, execute_git_status_tool,
};
pub use crate::tools::grep::{GrepArgs, execute_grep_tool};
pub use crate::tools::notify::{NotifySendArgs, execute_notify_send};
pub use crate::tools::nu::{NuArgs, execute_nu_tool};
pub use crate::tools::sh::{ShArgs, execute_sh_tool};
#[cfg(test)]
pub(crate) use crate::tools::sha256_hex;
pub use crate::tools::vm::{RunRiscVInput, execute_run_riscv_tool};

#[cfg(test)]
pub(crate) use crate::tools::fs::{
    execute_edit_file_tool, execute_read_file_range_tool, execute_write_file_tool,
};

#[cfg(test)]
mod tests;
