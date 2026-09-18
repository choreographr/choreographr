//! Shared `-v`/`-q` verbosity flags and log-level resolution.
//!
//! Every CLI binary in the suite exposes the same two flags and resolves the
//! same way. Precedence follows the Unix convention — **explicit CLI flags win
//! over the ambient environment**:
//!
//! 1. `-v`/`-q` given → they select the level, and `RUST_LOG` is ignored (the
//!    caller reports the override *after* the subscriber is installed);
//! 2. otherwise `RUST_LOG`, if set, supplies the filter directives verbatim
//!    (it is a per-target directive language, richer than a single level);
//! 3. otherwise the level defaults to `info`.
//!
//! Only the flag *parsing* and the level *decision* live here. Subscriber
//! *construction* (stderr vs. a log file, ANSI, `.init()`) stays in each
//! binary, because the destinations genuinely differ (the daemon logs to stderr
//! or `--log-file`; the TUI and ACP adapter are file-only; the GUI is a
//! windowed app with no terminal).

use clap::Args;
use tracing_subscriber::EnvFilter;

/// Create a pid-keyed diagnostics log file at `path`, owner-only and refusing
/// to follow a symlink planted at the path.
///
/// Shared by the file-only binaries — `choreo-tui`, `choreo-gui`, and
/// `choreo-acp` — so the temp-dir hardening lives in one place instead of
/// being re-derived per crate. On unix the file is created 0600 (owner-only:
/// the platform temp dir is frequently world-readable/writable) and a symlink
/// at the predictable, pid-keyed name is refused, so another local user cannot
/// redirect our diagnostics into a file of their choosing or read them. The
/// symlink guard is a pre-open `symlink_metadata` check because std exposes no
/// portable `O_NOFOLLOW`; a small TOCTOU window therefore remains, which the
/// daemon's `--log-file` open closes with `O_NOFOLLOW` plus an owner/regular
/// file check. On Windows the file inherits the parent directory's ACLs.
///
/// Returns `None` when the file cannot be created — the caller degrades to no
/// file logging, because diagnostics are never a startup precondition.
#[must_use]
pub fn create_log_file(path: &std::path::Path) -> Option<std::fs::File> {
    #[cfg(unix)]
    {
        use std::os::unix::fs::{OpenOptionsExt as _, PermissionsExt as _};

        // Refuse to follow or replace a symlink planted at the path.
        if std::fs::symlink_metadata(path).is_ok_and(|meta| meta.file_type().is_symlink()) {
            return None;
        }
        // `mode` applies only on creation; the explicit set_permissions below
        // also tightens a file left behind by an earlier run.
        let file = std::fs::OpenOptions::new()
            .write(true)
            .create(true)
            .truncate(true)
            .mode(0o600)
            .open(path)
            .ok()?;
        let _ = file.set_permissions(std::fs::Permissions::from_mode(0o600));
        Some(file)
    }
    #[cfg(not(unix))]
    {
        std::fs::File::create(path).ok()
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

    /// Emit the post-install startup diagnostics: the "flags take precedence"
    /// warning (only when explicit flags overrode a set `RUST_LOG`) and the
    /// effective-level banner.
    ///
    /// MUST be called AFTER the tracing subscriber is installed — an event
    /// logged before `init()` has no subscriber and is silently dropped (the
    /// bug this ordering exists to avoid). Centralised here so every binary
    /// emits identical wording exactly once.
    pub fn emit_startup_logs(&self) {
        if self.rust_log_ignored {
            tracing::warn!("RUST_LOG is set; -v/-q CLI flags take precedence");
        }
        tracing::info!(
            effective_level = self.effective_level,
            "logging initialized"
        );
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

    // ── create_log_file hardening (unix) ─────────────────────────────

    /// A freshly created log file must be owner-only (0600), not the
    /// umask-derived 0644 — the platform temp dir is often shared.
    #[cfg(unix)]
    #[test]
    fn create_log_file_is_owner_only() {
        use std::os::unix::fs::PermissionsExt as _;
        let dir = tempfile::tempdir().expect("temp dir");
        let path = dir.path().join("tui-123.log");
        let file = create_log_file(&path).expect("a writable temp dir yields a log");
        drop(file);
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
    fn create_log_file_refuses_a_symlink() {
        let dir = tempfile::tempdir().expect("temp dir");
        let target = dir.path().join("victim");
        std::fs::write(&target, b"do not touch").expect("write target");
        let link = dir.path().join("tui-123.log");
        std::os::unix::fs::symlink(&target, &link).expect("symlink");
        assert!(
            create_log_file(&link).is_none(),
            "a symlink at the log path must be refused"
        );
        assert_eq!(
            std::fs::read(&target).expect("read target"),
            b"do not touch",
            "the symlink target must be untouched"
        );
    }

    /// A file left group/world-readable by an earlier run is tightened to
    /// 0600 on the next open (the create mode applies only on creation).
    #[cfg(unix)]
    #[test]
    fn create_log_file_tightens_a_preexisting_loose_file() {
        use std::os::unix::fs::PermissionsExt as _;
        let dir = tempfile::tempdir().expect("temp dir");
        let path = dir.path().join("tui-123.log");
        std::fs::write(&path, b"old").expect("write");
        std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o644)).expect("chmod");
        let file = create_log_file(&path).expect("open");
        drop(file);
        let mode = std::fs::metadata(&path)
            .expect("metadata")
            .permissions()
            .mode()
            & 0o777;
        assert_eq!(mode, 0o600, "an existing loose log must be tightened");
    }
}
