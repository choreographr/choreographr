use crate::tools::{ToolError, truncate_tool_output};
use gix::bstr::ByteSlice;
use serde::Deserialize;
use std::{fmt::Write as _, io};

use super::{describe_head, open_repo, repo_work_dir_display};

#[derive(Debug, Deserialize)]
pub struct GitLogArgs {
    pub repo_path: Option<String>,
    pub limit: Option<usize>,
}

pub fn execute_git_log_tool(
    args: &GitLogArgs,
    working_dir: Option<&std::path::Path>,
) -> Result<String, ToolError> {
    let output = git_log_impl(
        args.repo_path.as_deref(),
        args.limit.unwrap_or(10).clamp(1, 100),
        working_dir,
    )?;
    Ok(truncate_tool_output(&output))
}

pub(crate) fn git_log_impl(
    repo_path: Option<&str>,
    limit: usize,
    working_dir: Option<&std::path::Path>,
) -> Result<String, ToolError> {
    let repo = open_repo(repo_path, working_dir)?;
    let head = match repo.head_id() {
        Ok(head) => head,
        Err(_) => return Ok("repository has no commits yet".to_string()),
    };

    let mut walk = repo
        .rev_walk([head.detach()])
        .all()
        .map_err(io::Error::other)?;

    let mut out = String::new();
    writeln!(&mut out, "repository: {}", repo_work_dir_display(&repo)).ok();
    writeln!(&mut out, "head: {}", describe_head(&repo)?).ok();

    let mut count = 0usize;
    for info in (&mut walk).take(limit) {
        let info = info.map_err(io::Error::other)?;
        let commit = info.object().map_err(io::Error::other)?;
        let short_id = commit.short_id().map_err(io::Error::other)?;
        let decoded = commit.decode().map_err(io::Error::other)?;
        let author = commit.author().map_err(io::Error::other)?;
        let title = decoded
            .message
            .lines()
            .next()
            .map(|line| String::from_utf8_lossy(line.trim()).into_owned())
            .unwrap_or_default();
        writeln!(
            &mut out,
            "{} {} <{}> {}",
            short_id, author.name, author.email, title
        )
        .ok();
        count += 1;
    }

    if count == 0 {
        writeln!(&mut out, "repository has no commits yet").ok();
    }

    Ok(out.trim_end().to_string())
}

pub(crate) struct GitLog;

define_tool!(
    GitLog,
    "git_log",
    "Show recent Git commits for the repository containing the given path.",
    GitLogArgs,
    execute_git_log_tool,
    serde_json::json!({"type":"object","properties":{"path":{"type":"string","description":"Relative or absolute path inside a Git repository","default":"."},"max_count":{"type":"integer","minimum":1,"maximum":100,"default":10}},"additionalProperties":false}),
    "git"
);
