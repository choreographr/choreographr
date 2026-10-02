//! Filesystem layout resolution for the Choreographr instance.
//!
//! Every suite crate resolves its on-disk locations through this module, so a
//! single override relocates the whole instance — config, data, socket, and
//! logs together. Two layouts exist:
//!
//! - **Default.** The platform dirs: `dirs::config_dir()/choreographr` for
//!   config, `dirs::data_dir()/choreographr` for data, and the platform temp
//!   dir for the socket. With no override, these are byte-for-byte the
//!   historical locations, so existing installs keep working untouched.
//! - **Base dir.** When `CHOREOGRAPHR_BASE_DIR` is set — the `--base-dir` flag
//!   the binaries accept writes it (see [`set_base_dir_from_cli`]) — every
//!   per-instance location lives under it:
//!
//!   ```text
//!   {base}/config/choreographr/
//!   {base}/data/choreographr/
//!   {base}/run/choreographr.sock
//!   {base}/log/<binary>.log
//!   ```
//!
//! Precedence is **base dir over the platform dirs**; the file-specific
//! `CHOREOGRAPHR_DB_PATH` / `CHOREOGRAPHR_SOCKET_PATH` overrides still win over
//! both, so one file can be pinned independently of the rest.
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
//! The default config and data roots are two separate platform dirs, so they
//! cannot both be expressed as one base. [`default_config_dir`] and
//! [`default_data_dir`] always return the *platform-default* locations
//! (ignoring any base override); `choreographr migrate` uses them as the source
//! when moving an existing install into a base dir.

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

/// The one subdirectory every location hangs off, matching the historical
/// `choreographr` segment under the platform dirs.
const APP_SUBDIR: &str = "choreographr";

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

/// The config root: `{base}/config/choreographr` when a base is set, else the
/// platform default.
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

/// The data root: `{base}/data/choreographr` when a base is set, else the
/// platform default.
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

/// The runtime directory (`{base}/run`), or `None` when no base is set.
///
/// The default layout keeps the socket in the platform temp dir; only a base
/// gives it a stable, instance-owned home.
#[must_use]
pub fn run_dir() -> Option<PathBuf> {
    base_dir().map(|b| run_dir_under(&b))
}

/// The base-derived socket path (`{base}/run/choreographr.sock`), or `None`
/// when no base is set (the caller falls back to the platform temp dir).
#[must_use]
pub fn base_socket_path() -> Option<PathBuf> {
    base_dir().map(|b| socket_path_under(&b))
}

/// The base-derived log directory (`{base}/log`), or `None` when no base is
/// set.
#[must_use]
pub fn log_dir() -> Option<PathBuf> {
    base_dir().map(|b| b.join(LOG_SUBDIR))
}

/// The default log file for `binary` under a base dir
/// (`{base}/log/<binary>.log`), creating the log directory if needed.
///
/// Returns `None` when no base is set (the caller keeps its own default, such
/// as a pid-keyed temp file or stderr). A create failure is not fatal here:
/// the caller's log-file open will fail too and degrade to no file logging.
#[must_use]
pub fn log_file_default(binary: &str) -> Option<PathBuf> {
    let dir = log_dir()?;
    let _ = std::fs::create_dir_all(&dir);
    Some(dir.join(format!("{binary}.log")))
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

fn config_dir_under(base: &Path) -> PathBuf {
    base.join(CONFIG_SUBDIR).join(APP_SUBDIR)
}

fn data_dir_under(base: &Path) -> PathBuf {
    base.join(DATA_SUBDIR).join(APP_SUBDIR)
}

fn run_dir_under(base: &Path) -> PathBuf {
    base.join(RUN_SUBDIR)
}

fn socket_path_under(base: &Path) -> PathBuf {
    run_dir_under(base).join("choreographr.sock")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn config_dir_under_nests_config_and_app() {
        let path = config_dir_under(Path::new("/inst"));
        assert_eq!(path, PathBuf::from("/inst/config/choreographr"));
    }

    #[test]
    fn data_dir_under_nests_data_and_app() {
        let path = data_dir_under(Path::new("/inst"));
        assert_eq!(path, PathBuf::from("/inst/data/choreographr"));
    }

    #[test]
    fn socket_path_is_under_run() {
        let path = socket_path_under(Path::new("/inst"));
        assert_eq!(path, PathBuf::from("/inst/run/choreographr.sock"));
    }

    #[test]
    fn log_file_name_is_binary_keyed() {
        let dir = Path::new("/inst").join(LOG_SUBDIR);
        assert_eq!(
            dir.join("daemon.log"),
            PathBuf::from("/inst/log/daemon.log")
        );
    }

    #[test]
    fn default_roots_ignore_a_base_override() {
        // default_config_dir / default_data_dir are the platform locations by
        // definition; setting the env var must not consume them. We assert the
        // Ok shape without asserting a specific OS path.
        let cfg = default_config_dir().unwrap();
        assert!(cfg.ends_with(APP_SUBDIR));
        let data = default_data_dir().unwrap();
        assert!(data.ends_with(APP_SUBDIR));
    }
}
