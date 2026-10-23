use crate::tools::{ToolError, truncate_tool_output};
use schemars::JsonSchema;
use serde::Deserialize;
use std::fmt::Write as _;

use super::{
    append_command_output, current_branch_name, describe_head, normalize_nonempty_argument,
    open_repo, repo_work_dir_display, run_git_command, yes_no,
};

/// Arguments for the `git_push` tool ([`execute_git_push_tool`]).
///
/// The repository is discovered from `repo_path` (or the session working
/// directory), and the push is invoked as `git push`.
///
/// `remote` and `branch` are model-supplied, so they are placed after a
/// literal `--` separator: everything after it is positional, and a value
/// beginning with `-` can never be parsed as a git option.
#[derive(Debug, Deserialize, JsonSchema)]
pub struct GitPushArgs {
    /// Path to the git repository. Defaults to the session working directory.
    pub repo_path: Option<String>,
    /// Remote to push to, such as `origin`. Must be non-empty after trimming.
    pub remote: String,
    /// Branch to push. Defaults to the current branch; an error when HEAD is
    /// detached and no branch is supplied.
    pub branch: Option<String>,
    /// When true, set the upstream tracking branch (`--set-upstream`).
    /// Defaults to false.
    pub set_upstream: Option<bool>,
    /// When true, push with `--force-with-lease`, which refuses to overwrite
    /// remote work the local ref does not already know about. Defaults to
    /// false.
    pub force_with_lease: Option<bool>,
    /// When true, run `--dry-run` — report what would be pushed without
    /// pushing. Defaults to false.
    pub dry_run: Option<bool>,
}

/// Push to a Git remote branch.
///
/// # Errors
///
/// Returns Err if the repository cannot be opened, the remote/branch names
/// are invalid, or the push command fails.
pub fn execute_git_push_tool(
    args: &GitPushArgs,
    working_dir: Option<&std::path::Path>,
) -> Result<String, ToolError> {
    let output = git_push_impl(
        args.repo_path.as_deref(),
        &args.remote,
        args.branch.as_deref(),
        args.set_upstream.unwrap_or(false),
        args.force_with_lease.unwrap_or(false),
        args.dry_run.unwrap_or(false),
        working_dir,
    )?;
    Ok(truncate_tool_output(&output))
}

/// Build the argument vector for `git push`.
///
/// All option flags come first, then a literal `--` that ends option parsing,
/// then the positional `remote` and `branch`. The separator is what makes the
/// push safe against argument injection: without it, a model-supplied remote
/// or branch beginning with `-` (for example `--upload-pack=...`) would be
/// read by git as an option rather than as a positional argument. Placing the
/// separator after the flags — never before them — keeps the flags effective.
fn build_push_args(
    remote: &str,
    branch: &str,
    dry_run: bool,
    set_upstream: bool,
    force_with_lease: bool,
) -> Vec<String> {
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
    args.push("--".to_string());
    args.push(remote.to_string());
    args.push(branch.to_string());
    args
}

fn git_push_impl(
    repo_path: Option<&str>,
    remote: &str,
    branch: Option<&str>,
    set_upstream: bool,
    force_with_lease: bool,
    dry_run: bool,
    working_dir: Option<&std::path::Path>,
) -> Result<String, ToolError> {
    let repo = open_repo(repo_path, working_dir)?;
    let remote = normalize_nonempty_argument(remote, "remote")?;
    let branch = match branch {
        Some(branch) => normalize_nonempty_argument(branch, "branch")?.to_string(),
        None => current_branch_name(&repo)?,
    };

    let args = build_push_args(remote, &branch, dry_run, set_upstream, force_with_lease);

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

pub fn describe_git_push_invocation(args: &GitPushArgs) -> String {
    let mut parts = vec![format!("Pushing to `{}`.", args.remote)];
    if let Some(ref branch) = args.branch {
        parts.push(format!(" Branch: `{branch}`."));
    }
    if args.set_upstream.unwrap_or(false) {
        parts.push(" Setting upstream.".to_string());
    }
    if args.force_with_lease.unwrap_or(false) {
        parts.push(" Force-with-lease.".to_string());
    }
    if args.dry_run.unwrap_or(false) {
        parts.push(" Dry run.".to_string());
    }
    parts.concat()
}

pub(crate) struct GitPush;

define_tool!(
    GitPush,
    "git_push",
    "Push to a Git remote branch.",
    GitPushArgs,
    execute_git_push_tool,
    "git",
    describe_git_push_invocation
);

#[cfg(test)]
mod tests {
    use super::*;

    /// Index of the literal `--` separator, or a panic when it is missing.
    fn separator_index(args: &[String]) -> usize {
        args.iter()
            .position(|arg| arg == "--")
            .expect("push args must contain a `--` separator")
    }

    #[test]
    fn test_build_push_args_places_positionals_after_separator() {
        let args = build_push_args("origin", "main", false, false, false);
        assert_eq!(args, vec!["push", "--", "origin", "main"]);
    }

    #[test]
    fn test_build_push_args_rejects_option_shaped_positionals() {
        // Both values begin with `-`, so without the separator git would read
        // them as options. They must appear only after `--`, never before it.
        let args = build_push_args("--upload-pack=evil", "-x", false, false, false);
        let separator = separator_index(&args);
        assert_eq!(args, vec!["push", "--", "--upload-pack=evil", "-x"]);
        assert_eq!(&args[separator + 1..], ["--upload-pack=evil", "-x"]);
        // Nothing option-shaped may precede the separator.
        assert!(args[..separator].iter().all(|arg| !arg.starts_with('-')));
    }

    #[test]
    fn test_build_push_args_orders_flags_before_separator() {
        let args = build_push_args("origin", "main", true, true, true);
        let separator = separator_index(&args);
        assert_eq!(
            &args[..separator],
            ["push", "--dry-run", "--set-upstream", "--force-with-lease"]
        );
        assert_eq!(&args[separator + 1..], ["origin", "main"]);
    }

    #[test]
    fn test_build_push_args_separator_always_present() {
        // The separator is structural: it appears even with every flag off.
        let args = build_push_args("upstream", "feature", false, false, false);
        assert_eq!(separator_index(&args), 1);
    }
}
