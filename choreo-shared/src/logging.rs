//! Shared `-v`/`-q` verbosity flags, log-level resolution, and the one
//! subscriber initializer every binary in the suite installs.
//!
//! Every CLI binary exposes the same two flags and resolves the same way.
//! Precedence follows the Unix convention — **explicit CLI flags win over the
//! ambient environment**:
//!
//! 1. `-v`/`-q` given → they select the level, and `RUST_LOG` is ignored (the
//!    caller reports the override *after* the subscriber is installed);
//! 2. otherwise `RUST_LOG`, if set, supplies the filter directives verbatim
//!    (it is a per-target directive language, richer than a single level);
//! 3. otherwise the level defaults to `info`.
//!
//! Beyond the flag parsing and the level decision, this module owns the one
//! [`init`] entry point that installs the suite's subscriber: diagnostics go to
//! a **file** (always) and, where a console exists, are **mirrored to stderr**.
//! The file is pid-keyed (`<binary>-<pid>.log`) so every process gets a fresh
//! file — "the pid is the rotation" — and stale logs (its own and captured MCP
//! `mcp-<…>.log` server logs alike) older than a week are pruned on startup.
//! Which binaries mirror to stderr, and whether that mirror is unconditional or
//! terminal-gated, is chosen per binary via [`ConsoleSink`].

use crate::paths;
use clap::Args;
use std::io;
use std::path::{Path, PathBuf};
use std::time::{Duration, SystemTime};
use tracing_subscriber::EnvFilter;

/// Where the console copy of the logs goes, in addition to the always-on file
/// sink.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ConsoleSink {
    /// No console copy — the TUI, which owns the terminal (the alternate
    /// screen), so any stderr write would corrupt the display.
    None,
    /// Always mirror to stderr — the daemon and the IM bridge, whose stderr is
    /// the console *or* the platform's log (journald, launchd, a redirection),
    /// so a `--base-dir` (or any file sink) must never silence the console.
    Stderr,
    /// Mirror to stderr only when it is a terminal — the GUI and the ACP
    /// adapter, launched by a desktop icon or an editor where stderr is usually
    /// not a terminal; a human running the binary by hand still sees the logs.
    StderrIfTty,
}

/// The per-binary inputs to [`init`].
#[derive(Clone, Copy)]
pub struct LogOptions<'a> {
    /// The binary's log-name stem (`"daemon"`, `"tui"`, …): the default file is
    /// `<binary>-<pid>.log`, and the pruner only ever touches that prefix.
    pub binary: &'a str,
    /// The shared `-v`/`-q` verbosity (explicit flags win over `RUST_LOG`).
    pub verbosity: Verbosity,
    /// An explicit `--log-file` path (used verbatim, with no pid key), if given.
    pub log_file: Option<&'a str>,
    /// Where the console copy goes, if anywhere.
    pub console: ConsoleSink,
    /// Show the module target on each line (off for the terse bridges).
    pub with_target: bool,
    /// Extra filter directives added after the level policy (e.g. a crate kept
    /// at `debug` regardless of the resolved level).
    pub extra_directives: &'a [&'a str],
    /// Whether a failed file open is fatal (`true`, the daemon) or degrades to
    /// the console sink (`false`, every other binary).
    pub require_file: bool,
}

/// How long a log file is kept in the log directory before the startup pruner
/// removes it. Retention is time-based, not count-based: a quiet instance keeps
/// a full week of history, and a busy one does not hold an unbounded number of
/// recent files.
const LOG_RETENTION: Duration = Duration::from_hours(7 * 24);

/// Install the process-wide subscriber: the shared level policy, a file sink,
/// and an optional console sink. Returns the resolved log-file path when a file
/// sink was installed, or `None` when the file could not be opened and the
/// process degraded to its console sink (or to no subscriber, when the console
/// sink is [`ConsoleSink::None`]).
///
/// # Errors
///
/// Returns the file-open error when `require_file` is set and the file cannot
/// be opened; with `require_file` unset a failed open degrades instead of
/// failing.
pub fn init(opts: LogOptions<'_>) -> io::Result<Option<PathBuf>> {
    let LoggingConfig {
        filter,
        effective_level,
        rust_log_ignored,
    } = LoggingConfig::resolve(opts.verbosity);
    let mut filter = filter;
    for directive in opts.extra_directives {
        if let Ok(directive) = directive.parse::<tracing_subscriber::filter::Directive>() {
            filter = filter.add_directive(directive);
        }
    }

    // Prune stale suite logs (older than one week) up front. The log directory
    // is shared housekeeping, so this runs whether or not this run uses the
    // default file, and it covers every binary's own log plus captured MCP
    // server logs.
    if let Some(dir) = paths::log_dir_default() {
        prune_logs(&dir);
    }

    // Resolve the file path. The default is pid-keyed; an explicit `--log-file`
    // is used verbatim.
    let path = match opts.log_file {
        Some(p) => PathBuf::from(p),
        None => paths::log_file(opts.binary, std::process::id()),
    };

    // Open the file (hardened). A failure degrades to the console sink unless
    // the caller made the file mandatory.
    let file = match open_log_file(&path) {
        Ok(file) => Some(file),
        Err(e) => {
            if opts.require_file {
                return Err(e);
            }
            None
        }
    };
    let log_path = file.as_ref().map(|_| path);

    // Install the subscriber only when it has at least one sink; with none,
    // there is nothing to receive the startup banner, so skip it.
    if log_path.is_some() || opts.console != ConsoleSink::None {
        install(filter, file, opts.console, opts.with_target);
        emit_startup_banner(effective_level, rust_log_ignored, log_path.as_deref());
    }
    Ok(log_path)
}

/// Emit the post-install startup diagnostics: the "flags take precedence"
/// warning (only when explicit flags overrode a set `RUST_LOG`) and the
/// effective-level banner, naming the resolved log file when a file sink is
/// installed. MUST run AFTER the subscriber is installed — an event logged
/// before `init()` has no subscriber and is silently dropped.
fn emit_startup_banner(
    effective_level: &'static str,
    rust_log_ignored: bool,
    log_path: Option<&Path>,
) {
    if rust_log_ignored {
        tracing::warn!("RUST_LOG is set; -v/-q CLI flags take precedence");
    }
    if let Some(path) = log_path {
        tracing::info!(
            effective_level,
            log_file = %path.display(),
            "logging initialized"
        );
    } else {
        tracing::info!(effective_level, "logging initialized");
    }
}

/// Install the subscriber: a file sink plus an optional console sink, both
/// rendered from one `filter`. One `fmt` layer per sink gives each its own
/// ANSI policy — a file is never colorized, the console is colored only when it
/// is a terminal.
fn install(
    filter: EnvFilter,
    file: Option<std::fs::File>,
    console: ConsoleSink,
    with_target: bool,
) {
    use tracing_subscriber::layer::SubscriberExt as _;
    use tracing_subscriber::util::SubscriberInitExt as _;

    let file_layer = file.map(|file| {
        tracing_subscriber::fmt::layer()
            .with_ansi(false)
            .with_target(with_target)
            .with_writer(std::sync::Mutex::new(file))
    });

    // The console ANSI setting is terminal-gated for BOTH console modes: color
    // only on a real terminal, and never when `NO_COLOR` is set. Whether the
    // console sink EXISTS at all is the one difference between the two modes.
    let ansi = console_wants_ansi();
    let console_layer = match console {
        ConsoleSink::None => None,
        ConsoleSink::Stderr => Some(
            tracing_subscriber::fmt::layer()
                .with_ansi(ansi)
                .with_target(with_target)
                .with_writer(std::io::stderr),
        ),
        ConsoleSink::StderrIfTty => {
            if stderr_is_tty() {
                Some(
                    tracing_subscriber::fmt::layer()
                        .with_ansi(ansi)
                        .with_target(with_target)
                        .with_writer(std::io::stderr),
                )
            } else {
                None
            }
        }
    };

    tracing_subscriber::registry()
        .with(filter)
        .with(file_layer)
        .with(console_layer)
        .init();
}

/// Whether stderr is a terminal.
fn stderr_is_tty() -> bool {
    use std::io::IsTerminal as _;
    std::io::stderr().is_terminal()
}

/// Whether the console copy should use ANSI styling: only on a real terminal,
/// and never when `NO_COLOR` is set — the opt-out `tracing-subscriber` honors
/// by default, which an explicit `with_ansi` would otherwise bypass.
fn console_wants_ansi() -> bool {
    stderr_is_tty() && std::env::var_os("NO_COLOR").is_none_or(|v| v.is_empty())
}

/// Open a fresh, owner-only log file at `path`, hardened against a shared or
/// world-writable parent directory.
///
/// The open is create-new (`O_CREAT | O_EXCL`) with `O_NOFOLLOW`, mode `0600`.
/// When the path already exists it must be a regular file owned by this
/// process's euid — a stale file left by a previous pid-owner — which is
/// removed and recreated; anything else (a symlink, a non-regular file, another
/// user's file) is refused, so another local user can never redirect or corrupt
/// our diagnostics. On Windows the file is created new and inherits the parent
/// directory's ACLs.
///
/// # Errors
///
/// Returns the underlying I/O error when the file cannot be created, or the
/// refusal error when an entry we must not replace sits at the path.
pub fn open_log_file(path: &Path) -> io::Result<std::fs::File> {
    #[cfg(unix)]
    {
        match create_new(path) {
            Ok(file) => Ok(file),
            Err(e) if e.kind() == io::ErrorKind::AlreadyExists => {
                // A reused pid: replace the (older) file only when it is a
                // regular file we own; refuse a symlink, a device/FIFO, or
                // another user's file outright.
                replace_if_owned(path)?;
                create_new(path)
            }
            Err(e) => Err(e),
        }
    }
    #[cfg(not(unix))]
    {
        match std::fs::OpenOptions::new()
            .write(true)
            .create_new(true)
            .open(path)
        {
            Ok(file) => Ok(file),
            Err(e) if e.kind() == io::ErrorKind::AlreadyExists => {
                std::fs::remove_file(path)?;
                std::fs::OpenOptions::new()
                    .write(true)
                    .create_new(true)
                    .open(path)
            }
            Err(e) => Err(e),
        }
    }
}

/// Open a log at `path` for append (creating it if absent), hardened like
/// [`open_log_file`]: `O_NOFOLLOW`, mode `0600`, and a post-open check that the
/// result is a regular file owned by this euid.
///
/// For a log that is *appended* across reconnects rather than recreated per
/// open — the captured stderr of a stdio MCP server, whose per-server file
/// (`mcp-<slug>-<hash>.log`) is stable. Refusing a symlink or a foreign file
/// keeps a server's diagnostics from being diverted.
///
/// # Errors
///
/// Returns the underlying I/O error, or the refusal when a non-regular / foreign
/// entry sits at the path.
pub fn open_log_append(path: &Path) -> io::Result<std::fs::File> {
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt as _;
        let file = std::fs::OpenOptions::new()
            .create(true)
            .append(true)
            // 0600 applies on creation; `verify_owned_regular` re-applies it.
            .mode(0o600)
            // O_NOFOLLOW: fail rather than follow a symlink at the path.
            .custom_flags(rustix::fs::OFlags::NOFOLLOW.bits().cast_signed())
            .open(path)?;
        verify_owned_regular(&file, path)?;
        Ok(file)
    }
    #[cfg(not(unix))]
    {
        std::fs::OpenOptions::new()
            .create(true)
            .append(true)
            .open(path)
    }
}

/// Remove a stale log at `path`, but only when it is a regular file we own;
/// refuse anything else.
#[cfg(unix)]
fn replace_if_owned(path: &Path) -> io::Result<()> {
    use std::os::unix::fs::MetadataExt as _;
    let meta = std::fs::symlink_metadata(path)?;
    if !meta.is_file() || meta.uid() != rustix::process::geteuid().as_raw() {
        return Err(io::Error::new(
            io::ErrorKind::AlreadyExists,
            format!(
                "refusing to replace {}: not a regular file owned by the current user",
                path.display()
            ),
        ));
    }
    std::fs::remove_file(path)
}

/// Verify an opened log is a regular file owned by this euid and tighten it to
/// 0600; refuse otherwise.
#[cfg(unix)]
fn verify_owned_regular(file: &std::fs::File, path: &Path) -> io::Result<()> {
    use std::os::unix::fs::{MetadataExt as _, PermissionsExt as _};
    let meta = file.metadata()?;
    if !meta.is_file() || meta.uid() != rustix::process::geteuid().as_raw() {
        return Err(io::Error::new(
            io::ErrorKind::PermissionDenied,
            format!(
                "refusing {}: not a regular file owned by the current user",
                path.display()
            ),
        ));
    }
    let _ = file.set_permissions(std::fs::Permissions::from_mode(0o600));
    Ok(())
}

/// Create a brand-new 0600 file at `path`, refusing to follow a symlink there.
#[cfg(unix)]
fn create_new(path: &Path) -> io::Result<std::fs::File> {
    use std::os::unix::fs::{OpenOptionsExt as _, PermissionsExt as _};

    let file = std::fs::OpenOptions::new()
        .write(true)
        // O_CREAT|O_EXCL: fail (`AlreadyExists`) rather than reuse an existing
        // file, so a planted file is never written into.
        .create_new(true)
        // The create mode; `set_permissions` below re-applies it uniformly.
        .mode(0o600)
        // O_NOFOLLOW: refuse to follow a symlink planted at the path.
        .custom_flags(rustix::fs::OFlags::NOFOLLOW.bits().cast_signed())
        .open(path)?;
    file.set_permissions(std::fs::Permissions::from_mode(0o600))?;
    Ok(file)
}

/// Whether `name` is a log file this suite writes, and therefore prunes: the
/// suite's own pid-keyed `<binary>-<pid>.log`, or a captured MCP server stderr
/// `mcp-<…>.log`. A user's explicit `--log-file` under any other name is left
/// alone.
fn is_suite_log_name(name: &str) -> bool {
    let Some(stem) = name.strip_suffix(".log") else {
        return false;
    };
    if stem.starts_with("mcp-") {
        return true;
    }
    // `<binary>-<pid>`: split at the last '-', the tail must be all digits.
    match stem.rsplit_once('-') {
        Some((prefix, pid)) => {
            !prefix.is_empty() && !pid.is_empty() && pid.bytes().all(|b| b.is_ascii_digit())
        }
        None => false,
    }
}

/// Best-effort startup prune: remove every suite log in `dir` older than the
/// retention window (one week).
///
/// Only files this suite names are considered (see `is_suite_log_name`) — the
/// suite's own `<binary>-<pid>.log` and the captured `mcp-<…>.log` server logs
/// alike. A user's explicit `--log-file` under another name, an unrelated file,
/// or a non-regular file (a symlink or directory) is never touched. Never
/// fails: an unreadable directory or an undeletable file is silently left
/// alone — retention is housekeeping, never a startup precondition.
pub fn prune_logs(dir: &Path) {
    let Ok(entries) = std::fs::read_dir(dir) else {
        return;
    };
    let Some(cutoff) = SystemTime::now().checked_sub(LOG_RETENTION) else {
        return;
    };
    for entry in entries.flatten() {
        let name = entry.file_name();
        let Some(name) = name.to_str() else { continue };
        if !is_suite_log_name(name) {
            continue;
        }
        // `symlink_metadata` reports the entry itself, so a symlink (whose
        // `is_file` is false) and a directory are both skipped.
        let Ok(meta) = std::fs::symlink_metadata(entry.path()) else {
            continue;
        };
        if !meta.is_file() {
            continue;
        }
        let Ok(modified) = meta.modified() else {
            continue;
        };
        if modified < cutoff {
            let _ = std::fs::remove_file(entry.path());
        }
    }
}

/// The shared `-v`/`-q` verbosity flags.
///
/// Flatten into a CLI struct with `#[command(flatten)]` so every binary spells
/// these identically:
///
/// ```ignore
/// #[derive(Parser)]
/// struct Cli {
///     #[command(flatten)]
///     verbosity: choreo_shared::logging::Verbosity,
///     // …
/// }
/// ```
#[derive(Args, Debug, Clone, Copy)]
pub struct Verbosity {
    /// Increase logging verbosity (-v debug, -vv trace)
    #[arg(short = 'v', long = "verbose", action = clap::ArgAction::Count)]
    pub verbose: u8,

    /// Decrease logging verbosity (only errors and warnings)
    #[arg(short = 'q', long = "quiet", action = clap::ArgAction::Count)]
    pub quiet: u8,
}

impl Verbosity {
    /// Map the counts to a `tracing` level directive: `-q` → warn, `-v` →
    /// debug, `-vv` (or more) → trace, and info by default. `-q` wins when both
    /// are given.
    #[must_use]
    pub fn level(self) -> &'static str {
        match (self.verbose, self.quiet) {
            (0, 0) => "info",
            (_, q) if q > 0 => "warn",
            (1, 0) => "debug",
            _ => "trace",
        }
    }

    /// Whether the user explicitly passed `-v` or `-q` (which take precedence
    /// over `RUST_LOG`).
    #[must_use]
    pub fn is_explicit(self) -> bool {
        self.verbose > 0 || self.quiet > 0
    }
}

/// The resolved logging configuration: the `EnvFilter` to install, plus the
/// facts the caller reports once the subscriber exists.
pub struct LoggingConfig {
    /// The filter to hand to the subscriber builder.
    pub filter: EnvFilter,
    /// The effective level directive for the startup banner — a level string,
    /// or `"from RUST_LOG"` when the environment supplied it.
    pub effective_level: &'static str,
    /// `true` when `RUST_LOG` is set but explicit `-v`/`-q` flags overrode it.
    /// The caller should emit the "flags take precedence" warning *after*
    /// installing the subscriber: an event logged before `init()` has no
    /// subscriber and is silently dropped.
    pub rust_log_ignored: bool,
}

impl LoggingConfig {
    /// Resolve the log configuration from the shared verbosity flags, applying
    /// the CLI-over-environment precedence. Reads the ambient `RUST_LOG`;
    /// [`resolve_with`](Self::resolve_with) is the pure, testable core.
    #[must_use]
    pub fn resolve(verbosity: Verbosity) -> Self {
        // `var` (not `var_os`): a non-UTF-8 `RUST_LOG` is treated as unset, and
        // the level falls back to `info` — the same practical outcome as the
        // previous lossy parse, with precedence still under our control.
        let rust_log = std::env::var("RUST_LOG").ok();
        Self::resolve_with(verbosity, rust_log.as_deref())
    }

    /// Pure core of [`resolve`](Self::resolve): resolve from an explicit
    /// `RUST_LOG` value (`None` = unset), so the precedence logic is
    /// unit-testable without touching process-global environment state.
    #[must_use]
    pub fn resolve_with(verbosity: Verbosity, rust_log: Option<&str>) -> Self {
        if verbosity.is_explicit() {
            // CLI flags are the most explicit expression of intent, so they win
            // over the ambient RUST_LOG (the Unix precedence convention).
            let level = verbosity.level();
            Self {
                filter: EnvFilter::new(level),
                effective_level: level,
                rust_log_ignored: rust_log.is_some(),
            }
        } else if let Some(raw) = rust_log {
            // No flags: RUST_LOG supplies the directive language verbatim; an
            // unparseable value degrades to `info` rather than failing startup.
            Self {
                filter: EnvFilter::try_new(raw).unwrap_or_else(|_| EnvFilter::new("info")),
                effective_level: "from RUST_LOG",
                rust_log_ignored: false,
            }
        } else {
            Self {
                filter: EnvFilter::new("info"),
                effective_level: "info",
                rust_log_ignored: false,
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn verbosity(verbose: u8, quiet: u8) -> Verbosity {
        Verbosity { verbose, quiet }
    }

    /// Pins the exact mapping every binary shares: default info, `-v` debug,
    /// `-vv` (and up) trace, `-q` warn — with `-q` winning whenever both are
    /// present.
    #[test]
    fn level_matches_the_suite_mapping() {
        assert_eq!(verbosity(0, 0).level(), "info");
        assert_eq!(verbosity(1, 0).level(), "debug");
        assert_eq!(verbosity(2, 0).level(), "trace");
        assert_eq!(verbosity(9, 0).level(), "trace");
        assert_eq!(verbosity(0, 1).level(), "warn");
        assert_eq!(verbosity(2, 1).level(), "warn");
    }

    #[test]
    fn is_explicit_is_false_only_without_flags() {
        assert!(!verbosity(0, 0).is_explicit());
        assert!(verbosity(1, 0).is_explicit());
        assert!(verbosity(0, 1).is_explicit());
    }

    /// Explicit flags win over `RUST_LOG`, and the override is reported so the
    /// caller's post-install warning fires. This is the suite's headline
    /// precedence rule — pinned directly on the pure resolver.
    #[test]
    fn explicit_flags_take_precedence_over_rust_log() {
        let c = LoggingConfig::resolve_with(verbosity(1, 0), Some("trace"));
        assert_eq!(c.effective_level, "debug", "the flag level must win");
        assert!(c.rust_log_ignored, "the override must be reported");
    }

    /// With no flags, `RUST_LOG` supplies the directives verbatim and is NOT
    /// reported as overridden.
    #[test]
    fn rust_log_is_used_when_no_flags() {
        let c = LoggingConfig::resolve_with(verbosity(0, 0), Some("warn"));
        assert_eq!(c.effective_level, "from RUST_LOG");
        assert!(!c.rust_log_ignored);
    }

    /// With neither flags nor `RUST_LOG`, the level defaults to `info`.
    #[test]
    fn default_is_info_without_flags_or_rust_log() {
        let c = LoggingConfig::resolve_with(verbosity(0, 0), None);
        assert_eq!(c.effective_level, "info");
        assert!(!c.rust_log_ignored);
    }

    /// Flags with no `RUST_LOG` set must not raise the override warning.
    #[test]
    fn flags_without_rust_log_do_not_report_an_override() {
        let c = LoggingConfig::resolve_with(verbosity(0, 1), None);
        assert_eq!(c.effective_level, "warn");
        assert!(!c.rust_log_ignored);
    }

    /// An unparseable `RUST_LOG` degrades to `info` without failing startup.
    #[test]
    fn invalid_rust_log_degrades_to_info() {
        let c = LoggingConfig::resolve_with(verbosity(0, 0), Some("=,not a directive"));
        // The banner still says it came from RUST_LOG; the point is that it
        // did not panic and produced a usable filter.
        assert_eq!(c.effective_level, "from RUST_LOG");
        assert!(!c.rust_log_ignored);
    }

    // ── open_log_file hardening (unix) ───────────────────────────────

    /// A freshly created log file must be owner-only (0600), not the
    /// umask-derived 0644 — the platform temp dir is often shared.
    #[cfg(unix)]
    #[test]
    fn open_log_file_is_owner_only() {
        use std::os::unix::fs::PermissionsExt as _;
        let dir = tempfile::tempdir().expect("temp dir");
        let path = dir.path().join("daemon-123.log");
        open_log_file(&path).expect("a writable temp dir yields a log");
        let mode = std::fs::metadata(&path)
            .expect("metadata")
            .permissions()
            .mode()
            & 0o777;
        assert_eq!(mode, 0o600, "log files must be owner-only");
    }

    /// A symlink at the (predictable, pid-keyed) log path must be refused, so
    /// another local user cannot redirect or corrupt our diagnostics.
    #[cfg(unix)]
    #[test]
    fn open_log_file_refuses_a_symlink() {
        let dir = tempfile::tempdir().expect("temp dir");
        let target = dir.path().join("victim");
        std::fs::write(&target, b"do not touch").expect("write target");
        let link = dir.path().join("daemon-123.log");
        std::os::unix::fs::symlink(&target, &link).expect("symlink");
        assert!(
            open_log_file(&link).is_err(),
            "a symlink at the log path must be refused"
        );
        assert_eq!(
            std::fs::read(&target).expect("read target"),
            b"do not touch",
            "the symlink target must be untouched"
        );
    }

    /// A stale pid-keyed file that we own (a reused pid) is replaced with a
    /// fresh, empty 0600 file rather than appended to.
    #[cfg(unix)]
    #[test]
    fn open_log_file_replaces_a_stale_owned_file() {
        use std::os::unix::fs::PermissionsExt as _;
        let dir = tempfile::tempdir().expect("temp dir");
        let path = dir.path().join("daemon-123.log");
        std::fs::write(&path, b"stale run").expect("write stale");
        std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o644)).expect("chmod");
        open_log_file(&path).expect("open replaces the stale file");
        assert_eq!(
            std::fs::read(&path).expect("read").as_slice(),
            b"",
            "the replaced file must be empty"
        );
        let mode = std::fs::metadata(&path)
            .expect("metadata")
            .permissions()
            .mode()
            & 0o777;
        assert_eq!(mode, 0o600, "the replacement must be owner-only");
    }

    /// The append opener (`open_log_append`, used for a stdio MCP server's
    /// captured stderr) hardens the same way: a symlink is refused and a fresh
    /// file is owner-only, while an existing file is appended to.
    #[cfg(unix)]
    #[test]
    fn open_log_append_hardens_and_appends() {
        use std::os::unix::fs::PermissionsExt as _;
        let dir = tempfile::tempdir().expect("temp dir");

        // A symlink at the path is refused (O_NOFOLLOW).
        let target = dir.path().join("victim");
        std::fs::write(&target, b"do not touch").expect("write target");
        let link = dir.path().join("mcp-link.log");
        std::os::unix::fs::symlink(&target, &link).expect("symlink");
        assert!(open_log_append(&link).is_err(), "a symlink must be refused");

        // A fresh file is created owner-only, and a second open appends.
        let path = dir.path().join("mcp-fresh.log");
        let mut file = open_log_append(&path).expect("open");
        std::io::Write::write_all(&mut file, b"one\n").expect("write");
        drop(file);
        let mode = std::fs::metadata(&path)
            .expect("metadata")
            .permissions()
            .mode()
            & 0o777;
        assert_eq!(mode, 0o600, "a fresh per-server log must be owner-only");
        let mut file = open_log_append(&path).expect("reopen");
        std::io::Write::write_all(&mut file, b"two\n").expect("append");
        drop(file);
        assert_eq!(std::fs::read(&path).expect("read"), b"one\ntwo\n");
    }

    // ── prune_logs retention ─────────────────────────────────────────

    /// Write a one-byte log file and set its mtime.
    fn seed_log(dir: &std::path::Path, name: &str, mtime: SystemTime) {
        let path = dir.join(name);
        std::fs::write(&path, b"x").expect("write");
        std::fs::File::options()
            .write(true)
            .open(&path)
            .expect("open")
            .set_modified(mtime)
            .expect("set mtime");
    }

    /// Pruning removes suite logs older than the retention window — both the
    /// suite's own `<binary>-<pid>.log` and the captured `mcp-<…>.log` server
    /// logs — and keeps newer ones, while never touching a foreign file name.
    #[test]
    fn prune_removes_logs_older_than_the_retention_window() {
        let dir = tempfile::tempdir().expect("temp dir");
        let now = SystemTime::now();
        let old = now - Duration::from_hours(8 * 24); // 8 days
        let fresh = now - Duration::from_mins(1); // a minute

        for name in ["daemon-1000.log", "tui-1000.log", "mcp-docs-abc123.log"] {
            seed_log(dir.path(), name, old);
        }
        for name in ["daemon-2000.log", "mcp-filesystem-def456.log"] {
            seed_log(dir.path(), name, fresh);
        }
        // Foreign names must never be pruned, however old.
        seed_log(dir.path(), "custom.log", old);
        seed_log(dir.path(), "notes.txt", old);

        prune_logs(dir.path());

        for name in ["daemon-1000.log", "tui-1000.log", "mcp-docs-abc123.log"] {
            assert!(!dir.path().join(name).exists(), "{name} should be pruned");
        }
        for name in [
            "daemon-2000.log",
            "mcp-filesystem-def456.log",
            "custom.log",
            "notes.txt",
        ] {
            assert!(dir.path().join(name).exists(), "{name} should be kept");
        }
    }

    /// Retention is time-based, not count-based: any number of recent logs
    /// survives, however many there are.
    #[test]
    fn prune_keeps_many_recent_logs() {
        let dir = tempfile::tempdir().expect("temp dir");
        let fresh = SystemTime::now() - Duration::from_mins(1);
        for i in 0..20 {
            seed_log(dir.path(), &format!("tui-{i}.log"), fresh);
        }
        prune_logs(dir.path());
        assert_eq!(std::fs::read_dir(dir.path()).expect("read_dir").count(), 20);
    }
}
