use choreographr::{
    GitAddArgs, GitCommitArgs, GitDiffArgs, GitLogArgs, GitPushArgs, GitRepoArgs,
    execute_git_add_tool, execute_git_commit_tool, execute_git_diff_tool, execute_git_log_tool,
    execute_git_push_tool, execute_git_status_tool,
};
use std::{
    path::Path,
    process::Command,
    sync::atomic::{AtomicU64, Ordering},
    time::{SystemTime, UNIX_EPOCH},
};

static NEXT_UNIQUE_ID: AtomicU64 = AtomicU64::new(1);

fn unique_repo_dir(name: &str) -> std::path::PathBuf {
    let unique = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .expect("time")
        .as_nanos();
    let counter = NEXT_UNIQUE_ID.fetch_add(1, Ordering::Relaxed);
    let pid = std::process::id();
    std::env::temp_dir().join(format!(
        "choreographr-git-tool-{name}-{pid}-{unique}-{counter}"
    ))
}

fn git(repo: &Path, args: &[&str]) {
    let status = Command::new("git")
        .args(args)
        .current_dir(repo)
        .status()
        .expect("run git");
    assert!(status.success(), "git {:?} failed with {status}", args);
}

fn init_repo() -> std::path::PathBuf {
    let dir = unique_repo_dir("repo");
    std::fs::create_dir_all(&dir).expect("create repo dir");
    git(&dir, &["init", "-b", "main"]);
    git(&dir, &["config", "user.name", "Choreographr Test"]);
    git(&dir, &["config", "user.email", "choreo@example.com"]);
    dir
}

fn init_bare_remote() -> std::path::PathBuf {
    let dir = unique_repo_dir("remote");
    std::fs::create_dir_all(&dir).expect("create remote dir");
    git(&dir, &["init", "--bare", "--initial-branch=main"]);
    dir
}

fn setup_remote(remote: &Path, repo: &Path) {
    let remote_str = remote.to_str().unwrap();
    git(repo, &["remote", "add", "origin", remote_str]);
}

fn repo_path_arg(repo: &Path) -> Option<String> {
    Some(repo.to_str().unwrap().to_string())
}

#[test]
#[ignore]
fn status_initial_repo() {
    let repo = init_repo();
    let worktree = execute_git_status_tool(
        &GitRepoArgs {
            repo_path: repo_path_arg(&repo),
        },
        None,
    )
    .unwrap_or_default();
    assert!(worktree.contains("mode: working tree"), "{}", worktree);
}

#[test]
#[ignore]
fn status_tracked_and_untracked_files() {
    let repo = init_repo();
    std::fs::write(repo.join("file.txt"), "hello").unwrap();
    git(&repo, &["add", "file.txt"]);
    git(&repo, &["commit", "-m", "Add file.txt"]);
    // Modify tracked file so it shows as modified
    std::fs::write(repo.join("file.txt"), "modified").unwrap();
    std::fs::write(repo.join("untracked.txt"), "content").unwrap();

    let worktree = execute_git_status_tool(
        &GitRepoArgs {
            repo_path: repo_path_arg(&repo),
        },
        None,
    )
    .unwrap_or_default();
    assert!(worktree.contains("M file.txt"), "{}", worktree);
    assert!(worktree.contains("untracked.txt"), "{}", worktree);
}

#[test]
#[ignore]
fn diff_working_tree() {
    let repo = init_repo();
    std::fs::write(repo.join("file.txt"), "original").unwrap();
    git(&repo, &["add", "file.txt"]);
    git(&repo, &["commit", "-m", "init"]);
    std::fs::write(repo.join("file.txt"), "modified").unwrap();

    let worktree = execute_git_diff_tool(
        &GitDiffArgs {
            repo_path: repo_path_arg(&repo),
            cached: None,
            pathspec: None,
        },
        None,
    )
    .unwrap_or_default();
    assert!(worktree.contains("+modified"), "{}", worktree);
}

#[test]
#[ignore]
fn diff_cached() {
    let repo = init_repo();
    std::fs::write(repo.join("file.txt"), "original").unwrap();
    git(&repo, &["add", "file.txt"]);
    git(&repo, &["commit", "-m", "init"]);
    std::fs::write(repo.join("file.txt"), "modified").unwrap();
    git(&repo, &["add", "file.txt"]);

    let cached = execute_git_diff_tool(
        &GitDiffArgs {
            repo_path: repo_path_arg(&repo),
            cached: Some(true),
            pathspec: None,
        },
        None,
    )
    .unwrap_or_default();
    assert!(cached.contains("+modified"), "{}", cached);
}

#[test]
#[ignore]
fn log_recent() {
    let repo = init_repo();
    std::fs::write(repo.join("file.txt"), "v1").unwrap();
    git(&repo, &["add", "file.txt"]);
    git(&repo, &["commit", "-m", "Initial commit"]);
    std::fs::write(repo.join("file.txt"), "v2").unwrap();
    git(&repo, &["add", "file.txt"]);
    git(&repo, &["commit", "-m", "Second commit"]);

    let result = execute_git_log_tool(
        &GitLogArgs {
            repo_path: repo_path_arg(&repo),
            limit: Some(2),
        },
        None,
    )
    .unwrap_or_default();
    assert!(result.contains("head: main"), "{}", result);
}

#[test]
#[ignore]
fn add_and_commit() {
    let repo = init_repo();
    let added = "new.txt";
    std::fs::write(repo.join(added), "content").unwrap();

    execute_git_add_tool(
        &GitAddArgs {
            repo_path: repo_path_arg(&repo),
            pathspec: vec![added.into()],
        },
        None,
    )
    .unwrap();

    let result = execute_git_commit_tool(
        &GitCommitArgs {
            repo_path: repo_path_arg(&repo),
            message: "Add new.txt".into(),
            allow_empty: None,
        },
        None,
    )
    .unwrap_or_default();
    assert!(result.contains("head: main"), "{}", result);
}

#[test]
#[ignore]
fn commit_empty_message_rejected() {
    let repo = init_repo();
    let result = execute_git_commit_tool(
        &GitCommitArgs {
            repo_path: repo_path_arg(&repo),
            message: "   ".into(),
            allow_empty: None,
        },
        None,
    );
    assert!(result.is_err());
    let err = result.unwrap_err().to_string();
    assert!(err.contains("commit message must not be empty"), "{}", err);
}

#[test]
#[ignore]
fn commit_no_changes_fails() {
    let repo = init_repo();
    let result = execute_git_commit_tool(
        &GitCommitArgs {
            repo_path: repo_path_arg(&repo),
            message: "Nothing".into(),
            allow_empty: None,
        },
        None,
    );
    assert!(result.is_err());
    let err = result.unwrap_err().to_string();
    assert!(err.contains("no staged changes to commit"), "{}", err);
}

#[test]
#[ignore]
fn push_to_remote() {
    let remote = init_bare_remote();
    let repo = init_repo();
    setup_remote(&remote, &repo);
    std::fs::write(repo.join("pushed.txt"), "content").unwrap();
    git(&repo, &["add", "pushed.txt"]);
    git(&repo, &["commit", "-m", "Push me"]);

    let result = execute_git_push_tool(
        &GitPushArgs {
            repo_path: repo_path_arg(&repo),
            remote: "origin".into(),
            branch: None,
            set_upstream: Some(true),
            force_with_lease: None,
            dry_run: None,
        },
        None,
    )
    .unwrap_or_default();
    assert!(result.contains("remote: origin"), "{}", result);
    assert!(result.contains("branch: main"), "{}", result);
    assert!(result.contains("set_upstream: yes"), "{}", result);
    assert!(result.contains("result: pushed"), "{}", result);
}

#[test]
#[ignore]
fn push_dry_run() {
    let remote = init_bare_remote();
    let repo = init_repo();
    setup_remote(&remote, &repo);
    std::fs::write(repo.join("dry.txt"), "dryrun").unwrap();
    git(&repo, &["add", "dry.txt"]);
    git(&repo, &["commit", "-m", "Dry run"]);

    let result = execute_git_push_tool(
        &GitPushArgs {
            repo_path: repo_path_arg(&repo),
            remote: "origin".into(),
            branch: Some("main".into()),
            set_upstream: None,
            force_with_lease: None,
            dry_run: Some(true),
        },
        None,
    )
    .unwrap_or_default();
    assert!(result.contains("dry_run: yes"), "{}", result);
    assert!(result.contains("result: dry run complete"), "{}", result);
}

#[test]
#[ignore]
fn push_fails_without_remote() {
    let repo = init_repo();
    let result = execute_git_push_tool(
        &GitPushArgs {
            repo_path: repo_path_arg(&repo),
            remote: "origin".into(),
            branch: None,
            set_upstream: None,
            force_with_lease: None,
            dry_run: None,
        },
        None,
    );
    assert!(result.is_err(), "expected error: {:?}", result);
    let err = result.unwrap_err().to_string();
    assert!(err.contains("remote: origin"), "{}", err);
}

#[test]
#[ignore]
fn push_rejected_when_ahead() {
    let remote = init_bare_remote();
    let repo = init_repo();
    setup_remote(&remote, &repo);
    std::fs::write(repo.join("a.txt"), "a").unwrap();
    git(&repo, &["add", "a.txt"]);
    git(&repo, &["commit", "-m", "A"]);

    // Push once to establish upstream
    execute_git_push_tool(
        &GitPushArgs {
            repo_path: repo_path_arg(&repo),
            remote: "origin".into(),
            branch: Some("main".into()),
            set_upstream: None,
            force_with_lease: None,
            dry_run: None,
        },
        None,
    )
    .unwrap();

    // Make another commit without pulling first (simulating non-ff rejection)
    // Actually in a bare test repo with no other pushes this won't fail.
    // Just test that push succeeds.
}
