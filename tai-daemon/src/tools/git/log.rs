use crate::tools::{ToolError, truncate_tool_output};
use gix::bstr::ByteSlice;
use schemars::JsonSchema;
use serde::Deserialize;
use std::{fmt::Write as _, io};

use super::{describe_head, open_repo, repo_work_dir_display};

#[derive(Debug, Deserialize, JsonSchema)]
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

pub fn describe_git_log_invocation(args: &GitLogArgs) -> String {
    let limit = args.limit.unwrap_or(10).clamp(1, 100);
    match &args.repo_path {
        Some(p) => format!("Showing git log for `{}` (last {} commits).", p, limit),
        None => format!("Showing git log (last {} commits).", limit),
    }
}

pub(crate) struct GitLog;

define_tool!(
    GitLog,
    "git_log",
    "Show recent Git commits for the repository containing the given path.",
    GitLogArgs,
    execute_git_log_tool,
    "git",
    describe_git_log_invocation
);
