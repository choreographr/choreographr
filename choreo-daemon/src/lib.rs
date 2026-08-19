pub mod accounts;
pub mod broadcast;
pub mod catalog;
pub mod cli;
pub use cli::main;
pub mod config;
pub mod context;
pub mod daemon;
pub mod db;
pub mod diff_util;
pub mod mcp;
pub mod metrics;
pub mod providers;
mod reasoning;
mod requests;
pub mod server;
mod sessions;
pub mod tools;

pub use crate::daemon::{DaemonCommand, DaemonState};
#[cfg(feature = "test-utils")]
pub use crate::reasoning::build_chat_request_messages;
pub use crate::requests::{
    REQUEST_IMAGE_BYTES, REQUEST_IMAGE_HEIGHT, REQUEST_IMAGE_MIME_TYPE, REQUEST_IMAGE_WIDTH,
};
pub use crate::server::run_server;
#[cfg(feature = "test-utils")]
pub use crate::sessions::join_session_shutdown_with_grace_for_test;
pub use crate::sessions::{
    ActiveSessionEntry, AssistantResponse, ChildResult, RequestContext, SessionCommand,
    SessionMetadata, SessionState, session_main,
};
pub use crate::tools::exec::{ExecArgs, execute_exec_tool};
pub use crate::tools::find::{FindArgs, execute_find_tool};
pub use crate::tools::fish::{FishArgs, execute_fish_tool};
pub use crate::tools::fs::{
    EditFileArgs, ListFilesArgs, TextEditArgs, WriteFileArgs, execute_edit_file_tool,
    execute_list_files_tool, execute_write_file_tool,
};
pub use crate::tools::git::{
    GitAddArgs, GitCommitArgs, GitDiffArgs, GitLogArgs, GitPushArgs, GitRepoArgs, GitShowArgs,
    execute_git_add_tool, execute_git_commit_tool, execute_git_diff_tool, execute_git_log_tool,
    execute_git_push_tool, execute_git_show_tool, execute_git_status_tool,
};
pub use crate::tools::grep::{GrepArgs, GrepOutputMode, execute_grep_tool};
pub use crate::tools::nu::{NuArgs, execute_nu_tool};
pub use crate::tools::pdf::{
    PdfClassifyArgs, PdfToMarkdownArgs, execute_pdf_classify, execute_pdf_to_markdown,
};
pub use crate::tools::sh::{ShArgs, Shell, execute_sh_tool};
#[cfg(test)]
pub(crate) use crate::tools::sha256_hex;
pub use crate::tools::vm::{RunRiscVInput, execute_run_riscv_tool};

#[cfg(test)]
mod tests;
