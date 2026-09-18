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
    /// the CLI-over-environment precedence.
    #[must_use]
    pub fn resolve(verbosity: Verbosity) -> Self {
        let rust_log_set = std::env::var_os("RUST_LOG").is_some();
        if verbosity.is_explicit() {
            // CLI flags are the most explicit expression of intent, so they win
            // over the ambient RUST_LOG (the Unix precedence convention).
            let level = verbosity.level();
            Self {
                filter: EnvFilter::new(level),
                effective_level: level,
                rust_log_ignored: rust_log_set,
            }
        } else if rust_log_set {
            // No flags: RUST_LOG supplies the directive language verbatim.
            Self {
                filter: EnvFilter::try_from_default_env()
                    .unwrap_or_else(|_| EnvFilter::new("info")),
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
}
