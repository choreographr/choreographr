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
//! file — "the pid is the rotation" — and stale siblings are pruned on startup.
//! Which binaries mirror to stderr, and whether that mirror is unconditional or
//! terminal-gated, is chosen per binary via [`ConsoleSink`].

use crate::paths;
use clap::Args;
use std::io;
use std::path::{Path, PathBuf};
use std::time::SystemTime;
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

/// The number of most-recent pid-keyed logs kept per binary. Older siblings are
/// pruned on startup so a long-lived log directory stays small.
const KEEP_LOG_FILES: usize = 5;

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

    // Resolve the file path. For the default (pid-keyed) name, prune stale
    // siblings first so retention stays small; an explicit `--log-file` is used
    // verbatim and never pruned.
    let path = if let Some(p) = opts.log_file {
        PathBuf::from(p)
    } else {
        if let Some(dir) = paths::log_dir_default() {
            prune_logs(&dir, opts.binary);
        }
        paths::log_file(opts.binary, std::process::id())
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
        use std::os::unix::fs::MetadataExt as _;

        match create_new(path) {
            Ok(file) => Ok(file),
            Err(e) if e.kind() == io::ErrorKind::AlreadyExists => {
                // A reused pid: replace the (older) file only when it is a
                // regular file we own; refuse a symlink, a device/FIFO, or
                // another user's file outright.
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
                std::fs::remove_file(path)?;
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

/// Best-effort startup prune of stale pid-keyed logs for `binary` in `dir`,
/// keeping the newest `KEEP_LOG_FILES` and removing the rest.
///
/// Only files named exactly `<binary>-<digits>.log` are considered, so a user's
/// explicit `--log-file` (any other name) and the MCP per-server `mcp-*.log`
/// captures (a different prefix) are never touched. Never fails: an unreadable
/// directory or an undeletable file is silently left alone — retention is
/// housekeeping, never a startup precondition.
pub fn prune_logs(dir: &Path, binary: &str) {
    let prefix = format!("{binary}-");
    let Ok(entries) = std::fs::read_dir(dir) else {
        return;
    };

    let mut files: Vec<(SystemTime, PathBuf)> = Vec::new();
    for entry in entries.flatten() {
        let name = entry.file_name();
        let Some(name) = name.to_str() else { continue };
        let Some(rest) = name.strip_prefix(prefix.as_str()) else {
            continue;
        };
        let Some(digits) = rest.strip_suffix(".log") else {
            continue;
        };
        if digits.is_empty() || !digits.bytes().all(|b| b.is_ascii_digit()) {
            continue;
        }
        let Ok(meta) = entry.metadata() else { continue };
        if !meta.is_file() {
            continue;
        }
        let mtime = meta.modified().unwrap_or(SystemTime::UNIX_EPOCH);
        files.push((mtime, entry.path()));
    }

    if files.len() <= KEEP_LOG_FILES {
        return;
    }
    // Oldest first: the leading `len - KEEP` entries are the ones to drop.
    files.sort_by_key(|(mtime, _)| *mtime);
    let excess = files.len() - KEEP_LOG_FILES;
    for (_, path) in files.into_iter().take(excess) {
        let _ = std::fs::remove_file(path);
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

    // ── prune_logs retention ─────────────────────────────────────────

    /// Pruning keeps the newest `KEEP_LOG_FILES` pid-keyed logs and removes the
    /// older ones, and leaves unrelated names (an explicit `--log-file`, an
    /// `mcp-*.log` capture) untouched.
    #[test]
    fn prune_keeps_the_newest_and_spares_foreign_names() {
        let dir = tempfile::tempdir().expect("temp dir");
        // Create KEEP+2 daemon logs with strictly increasing mtimes.
        let total = KEEP_LOG_FILES + 2;
        for i in 0..total {
            let path = dir.path().join(format!("daemon-{}.log", 1000 + i));
            std::fs::write(&path, b"x").expect("write");
            // File mtimes have coarse resolution on some filesystems; nudge
            // each one forward so the newest is unambiguous.
            let file = std::fs::File::options()
                .write(true)
                .open(&path)
                .expect("open");
            let mtime = std::time::SystemTime::UNIX_EPOCH
                + std::time::Duration::from_secs(1_700_000_000 + i as u64);
            file.set_modified(mtime).expect("set mtime");
        }
        // Foreign names that must never be pruned.
        std::fs::write(dir.path().join("custom.log"), b"x").expect("write custom");
        std::fs::write(dir.path().join("mcp-docs-abc.log"), b"x").expect("write mcp");

        prune_logs(dir.path(), "daemon");

        let remaining = std::fs::read_dir(dir.path())
            .expect("read_dir")
            .flatten()
            .filter_map(|e| e.file_name().to_str().map(str::to_owned))
            .collect::<Vec<_>>();
        let daemon_logs = remaining
            .iter()
            .filter(|n| n.starts_with("daemon-"))
            .count();
        assert_eq!(daemon_logs, KEEP_LOG_FILES, "only the newest are kept");
        assert!(remaining.iter().any(|n| n == "custom.log"));
        assert!(remaining.iter().any(|n| n == "mcp-docs-abc.log"));
        // The two oldest daemon logs (1000, 1001) are gone.
        assert!(!dir.path().join("daemon-1000.log").exists());
        assert!(!dir.path().join("daemon-1001.log").exists());
        assert!(
            dir.path()
                .join(format!("daemon-{}.log", 1000 + total - 1))
                .exists()
        );
    }

    /// With no more than `KEEP_LOG_FILES` logs, pruning removes nothing.
    #[test]
    fn prune_is_a_no_op_below_the_cap() {
        let dir = tempfile::tempdir().expect("temp dir");
        for i in 0..KEEP_LOG_FILES {
            std::fs::write(dir.path().join(format!("tui-{i}.log")), b"x").expect("write");
        }
        prune_logs(dir.path(), "tui");
        assert_eq!(
            std::fs::read_dir(dir.path()).expect("read_dir").count(),
            KEEP_LOG_FILES
        );
    }
}
