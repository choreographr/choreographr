//! `choreographr migrate` — relocate an existing platform-default install into
//! a base dir.
//!
//! The default config and data roots are two *separate* platform dirs, so they
//! cannot both be expressed as one base. This subcommand is the one place that
//! reads them together — through `choreo_shared::paths::default_*`, which
//! deliberately ignore any base override — and copies (or moves) them under
//! `{base}/config` and `{base}/data`. Copying the keystore files verbatim
//! (`identity.pk`, `transport.sec`/`.pub`) is what preserves the instance
//! identity, so a client's pinned server key keeps verifying after the move
//! (no re-pair) and the DB's keystore binding still holds.

use std::fs;
use std::io;
use std::path::{Path, PathBuf};

use anyhow::{Context, bail};
use tracing::info;

/// Directory name under a base that holds the config root (mirrors
/// `choreo_shared::paths`).
const CONFIG_SUBDIR: &str = "config";
/// Directory name under a base that holds the data root.
const DATA_SUBDIR: &str = "data";

/// Run the migration of the platform-default layout into `base`.
///
/// `do_move` moves instead of copying (the default copy leaves the old layout
/// intact and reversible); `dry_run` only reports; `force` allows merging into
/// a non-empty destination.
///
/// # Errors
///
/// Returns Err when the platform-default roots cannot be resolved, the
/// destination is non-empty and `force` was not given, or a filesystem
/// operation fails.
pub fn run(base: &Path, do_move: bool, dry_run: bool, force: bool) -> anyhow::Result<()> {
    let config_src = choreo_shared::paths::default_config_dir()
        .context("could not resolve the platform-default config directory")?;
    let data_src = choreo_shared::paths::default_data_dir()
        .context("could not resolve the platform-default data directory")?;

    let pairs = [
        (config_src, base.join(CONFIG_SUBDIR)),
        (data_src, base.join(DATA_SUBDIR)),
    ];
    let total = migrate(&pairs, do_move, dry_run, force)?;

    if !dry_run {
        println!("migrate complete: {total} file(s) into {}", base.display());
        println!(
            "start the instance with --base-dir {} (or export CHOREOGRAPHR_BASE_DIR={})",
            base.display(),
            base.display()
        );
    }
    Ok(())
}

/// The testable core: relocate each `(source, destination)` pair, returning the
/// number of files transferred.
fn migrate(
    pairs: &[(PathBuf, PathBuf)],
    do_move: bool,
    dry_run: bool,
    force: bool,
) -> anyhow::Result<usize> {
    let mut total = 0usize;
    for (src, dst) in pairs {
        if !src.exists() {
            info!(src = %src.display(), "nothing to migrate (source absent)");
            continue;
        }
        if dir_non_empty(dst) && !force {
            bail!(
                "destination {} already exists and is not empty; pass --force to merge into it, \
                 or choose an empty --base-dir",
                dst.display()
            );
        }
        if dry_run {
            println!(
                "would {} {} -> {} ({} file(s))",
                if do_move { "move" } else { "copy" },
                src.display(),
                dst.display(),
                count_files(src)
            );
            continue;
        }
        let files = copy_dir_all(src, dst).with_context(|| {
            format!("failed to transfer {} -> {}", src.display(), dst.display())
        })?;
        if do_move {
            fs::remove_dir_all(src)
                .with_context(|| format!("failed to remove migrated source {}", src.display()))?;
        }
        info!(files, src = %src.display(), dst = %dst.display(), "migrated layout");
        total += files;
    }
    Ok(total)
}

/// Recursively copy `src` into `dst`, preserving file permission bits
/// (`fs::copy` carries them on Unix), and return the file count.
fn copy_dir_all(src: &Path, dst: &Path) -> io::Result<usize> {
    fs::create_dir_all(dst)?;
    let mut count = 0;
    for entry in fs::read_dir(src)? {
        let entry = entry?;
        let to = dst.join(entry.file_name());
        if entry.file_type()?.is_dir() {
            count += copy_dir_all(&entry.path(), &to)?;
        } else {
            fs::copy(entry.path(), &to)?;
            count += 1;
        }
    }
    Ok(count)
}

/// Whether `path` is a directory with at least one entry (a missing path is
/// treated as empty).
fn dir_non_empty(path: &Path) -> bool {
    fs::read_dir(path).is_ok_and(|mut it| it.next().is_some())
}

/// Count regular files under `path` (best-effort; used only for the dry-run
/// report).
fn count_files(path: &Path) -> usize {
    fn walk(dir: &Path, n: &mut usize) {
        let Ok(entries) = fs::read_dir(dir) else {
            return;
        };
        for entry in entries.flatten() {
            if entry.file_type().is_ok_and(|t| t.is_dir()) {
                walk(&entry.path(), n);
            } else {
                *n += 1;
            }
        }
    }
    let mut n = 0;
    walk(path, &mut n);
    n
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn copies_a_layout_preserving_files() {
        let tmp = tempfile::tempdir().unwrap();
        let src = tmp.path().join("config-src");
        fs::create_dir_all(src.join("nested")).unwrap();
        fs::write(src.join("config.toml"), b"max_turns = 3").unwrap();
        fs::write(src.join("nested").join("inner"), b"x").unwrap();
        let dst = tmp.path().join("base/config/choreographr");

        let n = migrate(&[(src.clone(), dst.clone())], false, false, false).unwrap();
        assert_eq!(n, 2, "both files are counted");
        assert!(src.join("config.toml").exists(), "copy leaves the source");
        assert_eq!(fs::read(dst.join("config.toml")).unwrap(), b"max_turns = 3");
        assert!(dst.join("nested").join("inner").exists());
    }

    #[test]
    fn move_removes_the_source() {
        let tmp = tempfile::tempdir().unwrap();
        let src = tmp.path().join("src");
        fs::create_dir_all(&src).unwrap();
        fs::write(src.join("state.redb"), b"db").unwrap();
        let dst = tmp.path().join("base/data/choreographr");

        migrate(&[(src.clone(), dst.clone())], true, false, false).unwrap();
        assert!(!src.exists(), "move must remove the source tree");
        assert!(dst.join("state.redb").exists());
    }

    #[test]
    fn refuses_a_non_empty_destination_without_force() {
        let tmp = tempfile::tempdir().unwrap();
        let src = tmp.path().join("src");
        fs::create_dir_all(&src).unwrap();
        fs::write(src.join("a"), b"a").unwrap();
        let dst = tmp.path().join("dst");
        fs::create_dir_all(&dst).unwrap();
        fs::write(dst.join("existing"), b"b").unwrap();

        assert!(migrate(&[(src.clone(), dst.clone())], false, false, false).is_err());
        // --force merges instead.
        migrate(&[(src, dst.clone())], false, false, true).unwrap();
        assert!(dst.join("existing").exists());
        assert!(dst.join("a").exists());
    }

    #[test]
    fn dry_run_writes_nothing() {
        let tmp = tempfile::tempdir().unwrap();
        let src = tmp.path().join("src");
        fs::create_dir_all(&src).unwrap();
        fs::write(src.join("a"), b"a").unwrap();
        let dst = tmp.path().join("dst");

        let n = migrate(&[(src.clone(), dst.clone())], false, true, false).unwrap();
        assert_eq!(n, 0, "dry-run transfers nothing");
        assert!(!dst.exists(), "dry-run must not create the destination");
        assert!(src.exists());
    }

    #[test]
    fn absent_source_is_a_noop() {
        let tmp = tempfile::tempdir().unwrap();
        let src = tmp.path().join("does-not-exist");
        let dst = tmp.path().join("dst");
        assert_eq!(
            migrate(&[(src, dst.clone())], false, false, false).unwrap(),
            0
        );
        assert!(!dst.exists());
    }

    #[test]
    fn dir_non_empty_detects_content() {
        let tmp = tempfile::tempdir().unwrap();
        assert!(!dir_non_empty(&tmp.path().join("missing")));
        assert!(!dir_non_empty(tmp.path()));
        fs::write(tmp.path().join("f"), b"x").unwrap();
        assert!(dir_non_empty(tmp.path()));
    }
}
