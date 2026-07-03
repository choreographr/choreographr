use std::{
    path::Path,
    process::Command,
    sync::atomic::{AtomicU64, Ordering},
    time::{SystemTime, UNIX_EPOCH},
};
use tai_daemon::{
    execute_git_add_tool,
    execute_git_commit_tool,
    execute_git_diff_tool,
    execute_git_log_tool,
    execute_git_push_tool,
    execute_git_status_tool,
};

static NEXT_UNIQUE_ID: AtomicU64 = AtomicU64::new(1);

fn unique_repo_dir(name: &str) -> std::path::PathBuf {
    let unique = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .expect("time")
        .as_nanos();
    let counter = NEXT_UNIQUE_ID.fetch_add(1, Ordering::Relaxed);
    let pid = std::process::id();
    std::env::temp_dir().join(format!("tai-git-tool-{name}-{pid}-{unique}-{counter}"))
}

fn git(repo: &Path, args: &[&str]) {
    let status = Command::new("git")
        .args(args)
        .current_dir(repo)
        .status()
        .expect("run git");
    assert!(status.success(), "git {:?} failed with {status}", args);
}

fn git_output(repo: &Path, args: &[&str]) -> String {
    let output = Command::new("git")
        .args(args)
        .current_dir(repo)
        .output()
        .expect("run git");
    assert!(
        output.status.success(),
        "git {:?} failed: {}",
        args,
        String::from_utf8_lossy(&output.stderr)
    );
    String::from_utf8_lossy(&output.stdout).into_owned()
}

fn init_repo() -> std::path::PathBuf {
    let dir = unique_repo_dir("repo");
    std::fs::create_dir_all(&dir).expect("create repo dir");
    git(&dir, &["init", "-b", "main"]);
    git(&dir, &["config", "user.name", "Tai Test"]);
    git(&dir, &["config", "user.email", "tai@example.com"]);
    dir
}

fn init_bare_remote() -> std::path::PathBuf {
    let dir = unique_repo_dir("remote");
    std::fs::create_dir_all(&dir).expect("create remote dir");
    git(&dir, &["init", "--bare", "--initial-branch=main"]);
    dir
}

fn git_output_result(repo: &Path, args: &[&str]) -> std::process::Output {
    Command::new("git")
        .args(args)
        .current_dir(repo)
        .output()
        .expect("run git")
}

#[ignore]
#[tokio::test]
async fn git_status_reports_staged_unstaged_and_untracked_changes() {
    let repo = init_repo();
    std::fs::write(repo.join("tracked.txt"), "one\n").expect("write tracked");
    git(&repo, &["add", "tracked.txt"]);
    git(&repo, &["commit", "-m", "initial commit"]);

    std::fs::write(repo.join("tracked.txt"), "two\n").expect("modify tracked");
    std::fs::write(repo.join("staged.txt"), "stage me\n").expect("write staged");
    std::fs::write(repo.join("untracked.txt"), "new\n").expect("write untracked");
    git(&repo, &["add", "staged.txt"]);

    let result =
        execute_git_status_tool(&serde_json::json!({ "repo_path": repo }).to_string(), None).await;

    assert!(!result.is_error, "{}", result.content);
    assert!(result.content.contains("head: main"));
    assert!(result.content.contains("staged:"));
    assert!(result.content.contains("A staged.txt"));
    assert!(result.content.contains("unstaged:"));
    assert!(result.content.contains("M tracked.txt"));
    assert!(result.content.contains("untracked:"));
    assert!(result.content.contains("?? untracked.txt"));

    let _ = std::fs::remove_dir_all(repo);
}

#[ignore]
#[tokio::test]
async fn git_diff_reports_worktree_and_cached_changes() {
    let repo = init_repo();
    std::fs::write(repo.join("file.txt"), "one\n").expect("write file");
    git(&repo, &["add", "file.txt"]);
    git(&repo, &["commit", "-m", "initial commit"]);

    std::fs::write(repo.join("file.txt"), "two\n").expect("modify file");
    std::fs::write(repo.join("added.txt"), "new\n").expect("write added");
    git(&repo, &["add", "added.txt"]);

    let worktree = execute_git_diff_tool(
        &serde_json::json!({ "repo_path": repo, "cached": false }).to_string(), None,
    )
    .await;
    assert!(!worktree.is_error, "{}", worktree.content);
    assert!(worktree.content.contains("mode: working tree"));
    assert!(worktree.content.contains("M file.txt"));

    let cached = execute_git_diff_tool(
        &serde_json::json!({ "repo_path": repo, "cached": true }).to_string(), None,
    )
    .await;
    assert!(!cached.is_error, "{}", cached.content);
    assert!(cached.content.contains("mode: staged"));
    assert!(cached.content.contains("A added.txt"));

    let _ = std::fs::remove_dir_all(repo);
}

#[ignore]
#[tokio::test]
async fn git_log_reports_recent_commits() {
    let repo = init_repo();
    std::fs::write(repo.join("file.txt"), "one\n").expect("write file");
    git(&repo, &["add", "file.txt"]);
    git(&repo, &["commit", "-m", "first commit"]);
    std::fs::write(repo.join("file.txt"), "two\n").expect("rewrite file");
    git(&repo, &["commit", "-am", "second commit"]);

    let result =
        execute_git_log_tool(&serde_json::json!({ "repo_path": repo, "limit": 2 }).to_string(), None)
            .await;

    assert!(!result.is_error, "{}", result.content);
    assert!(result.content.contains("head: main"));
    assert!(
        result
            .content
            .contains("Tai Test <tai@example.com> second commit")
    );
    assert!(
        result
            .content
            .contains("Tai Test <tai@example.com> first commit")
    );

    let _ = std::fs::remove_dir_all(repo);
}

#[ignore]
#[tokio::test]
async fn git_add_stages_modified_untracked_and_deleted_paths() {
    let repo = init_repo();
    std::fs::write(repo.join("tracked.txt"), "one\n").expect("write tracked");
    std::fs::write(repo.join("delete-me.txt"), "gone\n").expect("write delete me");
    git(&repo, &["add", "."]);
    git(&repo, &["commit", "-m", "initial commit"]);

    std::fs::write(repo.join("tracked.txt"), "two\n").expect("modify tracked");
    std::fs::write(repo.join("new.txt"), "brand new\n").expect("write new");
    std::fs::remove_file(repo.join("delete-me.txt")).expect("remove file");

    let result = execute_git_add_tool(
        &serde_json::json!({
            "repo_path": repo,
            "pathspec": ["tracked.txt", "new.txt", "delete-me.txt"]
        })
        .to_string(), None,
    )
    .await;

    assert!(!result.is_error, "{}", result.content);
    let cached = git_output(&repo, &["diff", "--cached", "--name-status"]);
    assert!(cached.contains("M\ttracked.txt"), "{cached}");
    assert!(cached.contains("A\tnew.txt"), "{cached}");
    assert!(cached.contains("D\tdelete-me.txt"), "{cached}");

    let _ = std::fs::remove_dir_all(repo);
}

#[ignore]
#[tokio::test]
async fn git_add_accepts_clean_tracked_paths_as_noop() {
    let repo = init_repo();
    std::fs::write(repo.join("tracked.txt"), "one\n").expect("write tracked");
    git(&repo, &["add", "tracked.txt"]);
    git(&repo, &["commit", "-m", "initial commit"]);

    let result = execute_git_add_tool(
        &serde_json::json!({ "repo_path": repo, "pathspec": ["tracked.txt"] }).to_string(), None,
    )
    .await;

    assert!(!result.is_error, "{}", result.content);
    assert!(result.content.contains("index_changed: no"));
    assert!(result.content.contains("no changes"));

    let _ = std::fs::remove_dir_all(repo);
}

#[ignore]
#[tokio::test]
async fn git_add_works_from_subdirectory_repo_path() {
    let repo = init_repo();
    std::fs::create_dir_all(repo.join("src")).expect("create src");
    std::fs::write(repo.join("src/lib.rs"), "pub fn one() {}\n").expect("write file");

    let subdir = repo.join("src");
    let result = execute_git_add_tool(
        &serde_json::json!({ "repo_path": subdir, "pathspec": ["lib.rs"] }).to_string(), None,
    )
    .await;

    assert!(!result.is_error, "{}", result.content);
    let cached = git_output(&repo, &["diff", "--cached", "--name-status"]);
    assert!(cached.contains("A\tsrc/lib.rs"), "{cached}");

    let _ = std::fs::remove_dir_all(repo);
}

#[ignore]
#[tokio::test]
async fn git_add_rejects_empty_and_unmatched_pathspecs() {
    let repo = init_repo();

    let empty = execute_git_add_tool(
        &serde_json::json!({ "repo_path": repo, "pathspec": ["", "  "] }).to_string(), None,
    )
    .await;
    assert!(empty.is_error);
    assert!(
        empty
            .content
            .contains("pathspec must contain at least one non-empty entry")
    );

    let unmatched = execute_git_add_tool(
        &serde_json::json!({ "repo_path": repo, "pathspec": ["missing.txt"] }).to_string(), None,
    )
    .await;
    assert!(unmatched.is_error);
    assert!(
        unmatched
            .content
            .contains("pathspec did not match any tracked or untracked paths")
    );

    let _ = std::fs::remove_dir_all(repo);
}

#[ignore]
#[tokio::test]
async fn git_commit_creates_commit_from_staged_index() {
    let repo = init_repo();
    std::fs::write(repo.join("file.txt"), "one\n").expect("write file");
    execute_git_add_tool(
        &serde_json::json!({ "repo_path": repo, "pathspec": ["file.txt"] }).to_string(), None,
    )
    .await;

    let result = execute_git_commit_tool(
        &serde_json::json!({ "repo_path": repo, "message": "Add file" }).to_string(), None,
    )
    .await;

    assert!(!result.is_error, "{}", result.content);
    assert!(result.content.contains("head: main"));
    assert!(
        result
            .content
            .contains("Tai Test <tai@example.com> Add file")
    );
    let log = git_output(&repo, &["log", "--format=%s", "-1"]);
    assert_eq!(log.trim(), "Add file");

    let _ = std::fs::remove_dir_all(repo);
}

#[ignore]
#[tokio::test]
async fn git_commit_supports_multiline_messages_and_allow_empty() {
    let repo = init_repo();

    let empty_commit = execute_git_commit_tool(
        &serde_json::json!({
            "repo_path": repo,
            "message": "Initial empty\n\nBody",
            "allow_empty": true
        })
        .to_string(), None,
    )
    .await;
    assert!(!empty_commit.is_error, "{}", empty_commit.content);
    assert!(empty_commit.content.contains("Initial empty"));

    let body = git_output(&repo, &["log", "--format=%B", "-1"]);
    assert!(body.starts_with("Initial empty\n\nBody"), "{body}");

    let _ = std::fs::remove_dir_all(repo);
}

#[ignore]
#[tokio::test]
async fn git_commit_rejects_blank_message_and_missing_staged_changes() {
    let repo = init_repo();

    let blank = execute_git_commit_tool(
        &serde_json::json!({ "repo_path": repo, "message": "   " }).to_string(), None,
    )
    .await;
    assert!(blank.is_error);
    assert!(blank.content.contains("commit message must not be empty"));

    let no_changes = execute_git_commit_tool(
        &serde_json::json!({ "repo_path": repo, "message": "Nothing" }).to_string(), None,
    )
    .await;
    assert!(no_changes.is_error);
    assert!(no_changes.content.contains("no staged changes to commit"));

    let _ = std::fs::remove_dir_all(repo);
}

#[ignore]
#[tokio::test]
async fn git_commit_rejects_conflicted_index() {
    let repo = init_repo();
    std::fs::write(repo.join("file.txt"), "one\n").expect("write file");
    git(&repo, &["add", "file.txt"]);
    git(&repo, &["commit", "-m", "base"]);

    git(&repo, &["checkout", "-b", "feature"]);
    std::fs::write(repo.join("file.txt"), "feature\n").expect("write feature");
    git(&repo, &["commit", "-am", "feature change"]);

    git(&repo, &["checkout", "main"]);
    std::fs::write(repo.join("file.txt"), "main\n").expect("write main");
    git(&repo, &["commit", "-am", "main change"]);

    let output = Command::new("git")
        .args(["merge", "feature"])
        .current_dir(&repo)
        .output()
        .expect("run git merge");
    assert!(!output.status.success(), "merge unexpectedly succeeded");

    let result = execute_git_commit_tool(
        &serde_json::json!({ "repo_path": repo, "message": "should fail" }).to_string(), None,
    )
    .await;
    assert!(result.is_error);
    assert!(result.content.contains("unresolved index conflicts"));

    let _ = std::fs::remove_dir_all(repo);
}

#[ignore]
#[tokio::test]
async fn git_push_pushes_branch_to_remote_and_sets_upstream() {
    let repo = init_repo();
    let remote = init_bare_remote();
    git(
        &repo,
        &[
            "remote",
            "add",
            "origin",
            remote.to_str().expect("utf8 remote"),
        ],
    );
    std::fs::write(repo.join("file.txt"), "one\n").expect("write file");
    git(&repo, &["add", "file.txt"]);
    git(&repo, &["commit", "-m", "initial commit"]);

    let result = execute_git_push_tool(
        &serde_json::json!({
            "repo_path": repo,
            "remote": "origin",
            "set_upstream": true
        })
        .to_string(), None,
    )
    .await;

    assert!(!result.is_error, "{}", result.content);
    assert!(result.content.contains("remote: origin"));
    assert!(result.content.contains("branch: main"));
    assert!(result.content.contains("set_upstream: yes"));
    assert!(result.content.contains("result: pushed"));
    let remote_head = git_output(&remote, &["rev-parse", "main"]);
    let local_head = git_output(&repo, &["rev-parse", "HEAD"]);
    assert_eq!(remote_head.trim(), local_head.trim());
    let upstream = git_output(
        &repo,
        &["rev-parse", "--abbrev-ref", "--symbolic-full-name", "@{u}"],
    );
    assert_eq!(upstream.trim(), "origin/main");

    let _ = std::fs::remove_dir_all(repo);
    let _ = std::fs::remove_dir_all(remote);
}

#[ignore]
#[tokio::test]
async fn git_push_supports_dry_run() {
    let repo = init_repo();
    let remote = init_bare_remote();
    git(
        &repo,
        &[
            "remote",
            "add",
            "origin",
            remote.to_str().expect("utf8 remote"),
        ],
    );
    std::fs::write(repo.join("file.txt"), "one\n").expect("write file");
    git(&repo, &["add", "file.txt"]);
    git(&repo, &["commit", "-m", "initial commit"]);

    let result = execute_git_push_tool(
        &serde_json::json!({
            "repo_path": repo,
            "remote": "origin",
            "branch": "main",
            "dry_run": true
        })
        .to_string(), None,
    )
    .await;

    assert!(!result.is_error, "{}", result.content);
    assert!(result.content.contains("dry_run: yes"));
    assert!(result.content.contains("result: dry run complete"));
    let remote_lookup = git_output_result(&remote, &["rev-parse", "main"]);
    assert!(
        !remote_lookup.status.success(),
        "dry run should not update remote"
    );

    let _ = std::fs::remove_dir_all(repo);
    let _ = std::fs::remove_dir_all(remote);
}

#[ignore]
#[tokio::test]
async fn git_push_rejects_detached_head_without_branch() {
    let repo = init_repo();
    let remote = init_bare_remote();
    git(
        &repo,
        &[
            "remote",
            "add",
            "origin",
            remote.to_str().expect("utf8 remote"),
        ],
    );
    std::fs::write(repo.join("file.txt"), "one\n").expect("write file");
    git(&repo, &["add", "file.txt"]);
    git(&repo, &["commit", "-m", "initial commit"]);
    let head = git_output(&repo, &["rev-parse", "HEAD"]);
    git(&repo, &["checkout", head.trim()]);

    let result = execute_git_push_tool(
        &serde_json::json!({ "repo_path": repo, "remote": "origin" }).to_string(), None,
    )
    .await;

    assert!(result.is_error);
    assert!(
        result
            .content
            .contains("branch must be provided when HEAD is detached")
    );

    let _ = std::fs::remove_dir_all(repo);
    let _ = std::fs::remove_dir_all(remote);
}

#[ignore]
#[tokio::test]
async fn git_push_reports_push_failure() {
    let repo = init_repo();
    std::fs::write(repo.join("file.txt"), "one\n").expect("write file");
    git(&repo, &["add", "file.txt"]);
    git(&repo, &["commit", "-m", "initial commit"]);

    let result = execute_git_push_tool(
        &serde_json::json!({
            "repo_path": repo,
            "remote": "origin",
            "branch": "main"
        })
        .to_string(), None,
    )
    .await;

    assert!(result.is_error);
    assert!(result.content.contains("result: push failed"));
    assert!(result.content.contains("remote: origin"));

    let _ = std::fs::remove_dir_all(repo);
}
