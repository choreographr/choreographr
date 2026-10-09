//! Filesystem layout resolution for the Choreographr instance.
//!
//! Every suite crate resolves its on-disk locations through this module, so a
//! single override relocates the whole instance. Two layouts exist:
//!
//! - **Default (XDG).** Each category resolves through its own XDG base
//!   directory, exactly as the specification mandates — there is deliberately
//!   no single XDG base:
//!
//!   ```text
//!   $XDG_CONFIG_HOME/choreographr/   config.toml, accounts.toml, transport.*, …
//!   $XDG_DATA_HOME/choreographr/     state.redb, catalog.bin
//!   $XDG_RUNTIME_DIR/choreographr.sock
//!   $XDG_STATE_HOME/choreographr/    <binary>.log
//!   ```
//!
//!   `dirs::{runtime,state}_dir()` are `None` off Linux (and `$XDG_RUNTIME_DIR`
//!   can be unset in a bare environment), so the socket and logs fall back to
//!   the platform temp dir there — the spec's "replacement directory" posture.
//!
//! - **Base dir.** When `CHOREOGRAPHR_BASE_DIR` is set — the `--base-dir` flag
//!   the binaries accept writes it (see [`set_base_dir_from_cli`]) — the
//!   instance is relocated under one private root. Because that root plays the
//!   role of the whole XDG home, the `choreographr` segment is dropped (the
//!   parent is already app-private), unlike the default where each parent is
//!   shared:
//!
//!   ```text
//!   {base}/config/   config.toml, accounts.toml, transport.*, …
//!   {base}/data/     state.redb, catalog.bin
//!   {base}/run/      choreographr.sock
//!   {base}/log/      <binary>.log
//!   ```
//!
//! The file-specific `CHOREOGRAPHR_DB_PATH` / `CHOREOGRAPHR_SOCKET_PATH`
//! overrides still win over both layouts, so one file can be pinned
//! independently of the rest.
//!
//! The override travels by environment rather than in-process state: the
//! socket path is resolved in `choreo-proto` (which has no reason to know about
//! the config dir), the daemon is a *separate process* spawned by the TUI, and
//! the ACP/IM bridges are launched by editors/operators — an env var is the one
//! carrier every one of those already reads, and it is inherited by the
//! child processes without any explicit forwarding. See
//! [`set_base_dir_from_cli`] for the single, contained `set_var`.
//!
//! # Legacy layout note
//!
//! [`default_config_dir`] / [`default_data_dir`] always return the
//! *platform-default* locations (ignoring any base override); `choreographr
//! migrate` uses them as the source when copying an existing install into a
//! base dir.

use std::cell::RefCell;
use std::io;
use std::path::{Path, PathBuf};

/// Environment variable carrying the instance base directory.
///
/// Set by the `--base-dir` flag on any binary (see [`set_base_dir_from_cli`]),
/// or directly by an operator / service unit; read by every path resolver in
/// the suite, in every process.
pub const BASE_DIR_ENV: &str = "CHOREOGRAPHR_BASE_DIR";

/// Directory (inside the base) holding the config root.
const CONFIG_SUBDIR: &str = "config";
/// Directory (inside the base) holding the data root.
const DATA_SUBDIR: &str = "data";
/// Directory (inside the base) holding the runtime socket.
const RUN_SUBDIR: &str = "run";
/// Directory (inside the base) holding the per-binary log files.
const LOG_SUBDIR: &str = "log";

/// The app namespace segment used under the SHARED platform dirs (the default
/// XDG layout). Deliberately absent under a base, whose parent is app-private.
const APP_SUBDIR: &str = "choreographr";

/// The socket file name, used under both layouts.
const SOCKET_NAME: &str = "choreographr.sock";

/// Apply a `--base-dir` value from a parsed CLI, if any.
///
/// This is the one place the base override is written into the environment.
/// It MUST be called during single-threaded startup, before any other thread
/// is spawned or any path is resolved.
pub fn set_base_dir_from_cli(base: Option<PathBuf>) {
    if let Some(base) = base {
        // SAFETY: this runs on the main thread before the process spawns any
        // other thread and before any library reads the environment, so the
        // process-global environment is not concurrently observed or mutated.
        // (In edition 2024 `set_var` is unsafe for exactly that reason.)
        unsafe { std::env::set_var(BASE_DIR_ENV, &base) };
    }
}

/// The configured base directory, or `None` when the override is unset/empty.
///
/// An empty value is treated as unset, so an exported-but-empty
/// `CHOREOGRAPHR_BASE_DIR=` does not silently relocate everything to an empty
/// path.
#[must_use]
pub fn base_dir() -> Option<PathBuf> {
    match std::env::var_os(BASE_DIR_ENV) {
        Some(v) if !v.is_empty() => Some(PathBuf::from(v)),
        _ => None,
    }
}

/// The platform-default config root (`dirs::config_dir()/choreographr`),
/// ignoring any base override.
///
/// # Errors
///
/// Returns `NotFound` when the OS provides no config directory.
pub fn default_config_dir() -> io::Result<PathBuf> {
    dirs::config_dir()
        .map(|d| d.join(APP_SUBDIR))
        .ok_or_else(|| {
            io::Error::new(
                io::ErrorKind::NotFound,
                "could not determine standard config directory",
            )
        })
}

/// The platform-default data root (`dirs::data_dir()/choreographr`), ignoring
/// any base override.
///
/// # Errors
///
/// Returns `NotFound` when the OS provides no data directory.
pub fn default_data_dir() -> io::Result<PathBuf> {
    dirs::data_dir().map(|d| d.join(APP_SUBDIR)).ok_or_else(|| {
        io::Error::new(
            io::ErrorKind::NotFound,
            "could not determine standard data directory",
        )
    })
}

/// The config root: `{base}/config` when a base is set, else the platform
/// default (`dirs::config_dir()/choreographr`).
///
/// # Errors
///
/// Returns `NotFound` when no base is set and the OS provides no config dir.
pub fn config_dir() -> io::Result<PathBuf> {
    match base_dir() {
        Some(base) => Ok(config_dir_under(&base)),
        None => default_config_dir(),
    }
}

/// The data root: `{base}/data` when a base is set, else the platform default
/// (`dirs::data_dir()/choreographr`).
///
/// # Errors
///
/// Returns `NotFound` when no base is set and the OS provides no data dir.
pub fn data_dir() -> io::Result<PathBuf> {
    match base_dir() {
        Some(base) => Ok(data_dir_under(&base)),
        None => default_data_dir(),
    }
}

/// A path inside the config root, e.g. `config_file("accounts.toml")`.
///
/// # Errors
///
/// Propagates the [`config_dir`] resolution error.
pub fn config_file(name: &str) -> io::Result<PathBuf> {
    Ok(config_dir()?.join(name))
}

/// A path inside the data root, e.g. `data_file("state.redb")`.
///
/// # Errors
///
/// Propagates the [`data_dir`] resolution error.
pub fn data_file(name: &str) -> io::Result<PathBuf> {
    Ok(data_dir()?.join(name))
}

/// The default socket path: `{base}/run/choreographr.sock` under a base, else
/// `$XDG_RUNTIME_DIR/choreographr.sock`.
///
/// Returns `None` when neither a base nor an XDG runtime dir is available
/// (macOS/Windows, or a bare environment with `$XDG_RUNTIME_DIR` unset), so the
/// caller falls back to the platform temp dir.
#[must_use]
pub fn default_socket_path() -> Option<PathBuf> {
    match base_dir() {
        Some(base) => Some(socket_path_under(&base)),
        None => dirs::runtime_dir().map(|d| d.join(SOCKET_NAME)),
    }
}

/// The log directory: `{base}/log` under a base, else
/// `$XDG_STATE_HOME/choreographr`, else `None` when no state dir exists
/// (macOS/Windows, and some Android environments), so the caller falls back to
/// the platform temp dir.
///
/// Public so the startup log pruner and the TUI's reconstruction of an
/// autostarted daemon's path resolve through the same one definition the
/// log-file default uses.
#[must_use]
pub fn log_dir_default() -> Option<PathBuf> {
    log_dir()
}

/// The pid-keyed log file for `binary`: `{logdir}/<binary>-<pid>.log`, where
/// `{logdir}` is [`log_dir_default`], else the platform temp dir
/// (`choreo-<binary>-<pid>.log` — respects `TMPDIR`, and is the only writable
/// choice where no XDG state dir exists).
///
/// The `-<pid>` key makes every process's log a distinct, fresh file ("the pid
/// is the rotation"): parallel instances never clobber one another and no run
/// appends to a predecessor's file. This is the single naming definition — the
/// daemon's own default and the TUI's reconstruction of an autostarted daemon's
/// path both go through it, so the two cannot drift.
#[must_use]
pub fn log_file(binary: &str, pid: u32) -> PathBuf {
    let stem = format!("{binary}-{pid}");
    match log_dir() {
        Some(dir) => log_file_in(&dir, &stem),
        None => std::env::temp_dir().join(format!("choreo-{stem}.log")),
    }
}

/// The default log file for a file-logging `binary`.
///
/// Under a base: `{base}/log/<binary>.log`. Otherwise the XDG state dir
/// (`$XDG_STATE_HOME/choreographr/<binary>.log`), or `None` when the state dir
/// is unavailable (macOS/Windows) so the caller keeps its own fallback.
#[must_use]
pub fn log_file_default(binary: &str) -> Option<PathBuf> {
    log_dir().map(|dir| log_file_in(&dir, binary))
}

/// The log/state directory: the test override, else `{base}/log`, else
/// `$XDG_STATE_HOME/choreographr`.
fn log_dir() -> Option<PathBuf> {
    if let Some(dir) = TEST_LOG_DIR.with(|c| c.borrow().clone()) {
        return Some(dir);
    }
    if let Some(base) = base_dir() {
        return Some(base.join(LOG_SUBDIR));
    }
    dirs::state_dir().map(|d| d.join(APP_SUBDIR))
}

/// Whether a base-dir layout (config or data root) already exists under `base`.
#[must_use]
pub fn base_layout_present(base: &Path) -> bool {
    config_dir_under(base).exists() || data_dir_under(base).exists()
}

/// Whether the platform-default config or data root exists — used to warn when
/// a fresh base dir would silently shadow an existing install.
#[must_use]
pub fn legacy_layout_present() -> bool {
    default_config_dir().is_ok_and(|d| d.exists()) || default_data_dir().is_ok_and(|d| d.exists())
}

/// Warn, once at startup, when a base dir is configured but empty while the
/// platform-default locations still hold data — the "silent fresh instance"
/// footgun. A no-op without a base, or once the base has been populated.
pub fn warn_if_base_shadows_legacy_install() {
    let Some(base) = base_dir() else {
        return;
    };
    if !base_layout_present(&base) && legacy_layout_present() {
        tracing::warn!(
            base = %base.display(),
            "base dir is empty but an existing install was found in the default \
             locations; run `choreographr migrate --base-dir <dir>` to move it \
             (see docs), or the daemon will start as a fresh instance"
        );
    }
}

/// The config root under a base dir: `{base}/config`.
///
/// This is the canonical under-base config root — `choreo_daemon::migrate`
/// reuses it to compute its destination, so the layout has exactly one
/// definition and the two cannot drift.
#[must_use]
pub fn config_dir_under(base: &Path) -> PathBuf {
    base.join(CONFIG_SUBDIR)
}

/// The data root under a base dir: `{base}/data`.
///
/// This is the canonical under-base data root — `choreo_daemon::migrate`
/// reuses it to compute its destination, so the layout has exactly one
/// definition and the two cannot drift.
#[must_use]
pub fn data_dir_under(base: &Path) -> PathBuf {
    base.join(DATA_SUBDIR)
}

fn socket_path_under(base: &Path) -> PathBuf {
    base.join(RUN_SUBDIR).join(SOCKET_NAME)
}

/// Create `dir` and return `dir/<binary>.log`. A create failure is not fatal:
/// the caller's log-file open fails too and degrades to no file logging.
fn log_file_in(dir: &Path, binary: &str) -> PathBuf {
    let _ = std::fs::create_dir_all(dir);
    dir.join(format!("{binary}.log"))
}

thread_local! {
    /// Test-only override for the log directory. When set, `log_file_default`
    /// writes under it instead of the base/state dir, so tests never create a
    /// log in the developer's real `$XDG_STATE_HOME`.
    ///
    /// Deliberately NOT `#[cfg(test)]`-gated: integration tests compile the
    /// crate without `cfg(test)`, so the hook must exist in normal builds too
    /// (it is a no-op unless explicitly set).
    static TEST_LOG_DIR: RefCell<Option<PathBuf>> = const { RefCell::new(None) };
}

/// Set the log-directory test override (see `TEST_LOG_DIR`).
#[doc(hidden)]
pub fn set_test_log_dir(dir: Option<PathBuf>) {
    TEST_LOG_DIR.with(|c| c.replace(dir));
}

/// Guard that resets the log-directory test override on drop, even on panic.
#[doc(hidden)]
pub struct TestLogDirGuard;

#[doc(hidden)]
impl TestLogDirGuard {
    /// Set the log directory, returning a guard that resets it to `None`.
    #[must_use]
    pub fn set(dir: Option<PathBuf>) -> Self {
        set_test_log_dir(dir);
        TestLogDirGuard
    }
}

#[doc(hidden)]
impl Drop for TestLogDirGuard {
    fn drop(&mut self) {
        set_test_log_dir(None);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn config_dir_under_has_no_app_segment() {
        let path = config_dir_under(Path::new("/inst"));
        assert_eq!(path, PathBuf::from("/inst/config"));
    }

    #[test]
    fn data_dir_under_has_no_app_segment() {
        let path = data_dir_under(Path::new("/inst"));
        assert_eq!(path, PathBuf::from("/inst/data"));
    }

    #[test]
    fn socket_path_is_under_run() {
        let path = socket_path_under(Path::new("/inst"));
        assert_eq!(path, PathBuf::from("/inst/run/choreographr.sock"));
    }

    #[test]
    fn default_roots_keep_the_app_segment() {
        // The platform dirs are SHARED (e.g. ~/.config), so they keep the app
        // namespace; a base (whose parent is app-private) does not.
        let cfg = default_config_dir().unwrap();
        assert!(cfg.ends_with(APP_SUBDIR));
        let data = default_data_dir().unwrap();
        assert!(data.ends_with(APP_SUBDIR));
    }

    #[test]
    fn log_file_default_honors_the_test_override() {
        let temp = std::env::temp_dir().join("choreo-paths-log-override");
        let _guard = TestLogDirGuard::set(Some(temp.clone()));
        let path = log_file_default("tui-1").unwrap();
        assert_eq!(path, temp.join("tui-1.log"));
    }
}
