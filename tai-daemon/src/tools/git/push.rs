use crate::tools::{ToolError, ToolResult, tool_ok, truncate_tool_output};
use serde::Deserialize;
use std::fmt::Write as _;

use super::{
    append_command_output, current_branch_name, describe_head, normalize_nonempty_argument,
    open_repo, repo_work_dir_display, run_git_command, yes_no,
};

#[derive(Debug, Deserialize)]
struct GitPushArgs {
    repo_path: Option<String>,
    remote: String,
    branch: Option<String>,
    set_upstream: Option<bool>,
    force_with_lease: Option<bool>,
    dry_run: Option<bool>,
}

pub fn execute_git_push_tool(arguments_json: &str, cwd: Option<&std::path::Path>) -> ToolResult {
    match execute_git_push_inner(arguments_json, cwd) {
        Ok(content) => tool_ok(content),
        Err(error) => error.into(),
    }
}

fn execute_git_push_inner(arguments_json: &str, cwd: Option<&std::path::Path>) -> Result<String, ToolError> {
    let args: GitPushArgs = serde_json::from_str(arguments_json)?;
    let output = git_push_impl(
        args.repo_path.as_deref(),
        &args.remote,
        args.branch.as_deref(),
        args.set_upstream.unwrap_or(false),
        args.force_with_lease.unwrap_or(false),
        args.dry_run.unwrap_or(false),
        cwd,
    )?;
    Ok(truncate_tool_output(&output))
}

fn git_push_impl(
    repo_path: Option<&str>,
    remote: &str,
    branch: Option<&str>,
    set_upstream: bool,
    force_with_lease: bool,
    dry_run: bool,
    cwd: Option<&std::path::Path>,
) -> Result<String, ToolError> {
    let repo = open_repo(repo_path, cwd)?;
    let remote = normalize_nonempty_argument(remote, "remote")?;
    let branch = match branch {
        Some(branch) => normalize_nonempty_argument(branch, "branch")?.to_string(),
        None => current_branch_name(&repo)?,
    };

    let mut args = vec!["push".to_string()];
    if dry_run {
        args.push("--dry-run".to_string());
    }
    if set_upstream {
        args.push("--set-upstream".to_string());
    }
    if force_with_lease {
        args.push("--force-with-lease".to_string());
    }
    args.push(remote.to_string());
    args.push(branch.clone());

    let output = run_git_command(&repo, &args)?;
    let stdout = String::from_utf8_lossy(&output.stdout).trim().to_string();
    let stderr = String::from_utf8_lossy(&output.stderr).trim().to_string();

    if !output.status.success() {
        let mut out = String::new();
        writeln!(&mut out, "repository: {}", repo_work_dir_display(&repo)).ok();
        writeln!(&mut out, "head: {}", describe_head(&repo)?).ok();
        writeln!(&mut out, "remote: {remote}").ok();
        writeln!(&mut out, "branch: {branch}").ok();
        writeln!(&mut out, "dry_run: {}", yes_no(dry_run)).ok();
        writeln!(&mut out, "set_upstream: {}", yes_no(set_upstream)).ok();
        writeln!(&mut out, "force_with_lease: {}", yes_no(force_with_lease)).ok();
        writeln!(&mut out, "result: push failed").ok();
        append_command_output(&mut out, "stdout", &stdout);
        append_command_output(&mut out, "stderr", &stderr);
        return Err(ToolError::Other(out.trim_end().to_string()));
    }

    let mut out = String::new();
    writeln!(&mut out, "repository: {}", repo_work_dir_display(&repo)).ok();
    writeln!(&mut out, "head: {}", describe_head(&repo)?).ok();
    writeln!(&mut out, "remote: {remote}").ok();
    writeln!(&mut out, "branch: {branch}").ok();
    writeln!(&mut out, "dry_run: {}", yes_no(dry_run)).ok();
    writeln!(&mut out, "set_upstream: {}", yes_no(set_upstream)).ok();
    writeln!(&mut out, "force_with_lease: {}", yes_no(force_with_lease)).ok();
    writeln!(
        &mut out,
        "result: {}",
        if dry_run {
            "dry run complete"
        } else {
            "pushed"
        }
    )
    .ok();
    append_command_output(&mut out, "stdout", &stdout);
    append_command_output(&mut out, "stderr", &stderr);
    Ok(out.trim_end().to_string())
}

define_tool_with_cwd!(GitPush, "git_push",
    "Push to a Git remote branch.",
    execute_git_push_tool,
    serde_json::json!({"type":"object","properties":{"path":{"type":"string","description":"Relative or absolute path inside a Git repository","default":"."},"remote":{"type":"string","description":"Remote name","default":"origin"},"branch":{"type":"string","description":"Remote branch name"},"set_upstream":{"type":"boolean","description":"Set upstream tracking reference","default":false},"force_with_lease":{"type":"boolean","description":"Force push with lease (safe force push)","default":false},"dry_run":{"type":"boolean","description":"Simulate push without sending data","default":false}},"required":[],"additionalProperties":false})
);
