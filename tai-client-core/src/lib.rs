pub mod connection;
pub mod diff;
pub mod dispatch;
pub mod error;
pub mod history;
pub mod image;
pub mod shell;

pub use connection::{run_daemon_connection, run_daemon_reader};
pub use diff::{DiffHunk, DiffLine, DiffLineKind, FileDiff};
pub use dispatch::{DaemonMessageHandler, dispatch_daemon_message};
pub use error::{ClientError, broken_pipe};
pub use history::{ClientHistory, HistoryItem, MAX_HISTORY_ITEMS};
pub use image::{ImageAssembler, PendingImage};
pub use shell::{ShellCommand, StreamingText, parse_input_line, shell_command_echo};

#[cfg(test)]
mod tests;
