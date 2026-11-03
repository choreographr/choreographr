//! Where and how to reach the hosting workspace manager's reporting API.
//!
//! Detection is a pure function over injected data — an environment lookup and
//! this program's own file name — so the opt-in rule is unit-testable without
//! touching the real process. The marker names and the "an empty value is not a
//! claim" lookup come from [`crate::terminal::profile`], the same set the
//! launch-time terminal profile uses to decide this process is in a managed
//! pane, so the two can never disagree about it.

use crate::terminal::profile::{HERDR_BIN_PATH, HERDR_ENV, HERDR_PANE_ID, value};

/// Where and how to reach the hosting workspace manager's reporting API.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ReporterConfig {
    /// The host's own binary, invoked as `<bin> pane …`.
    bin: String,
    /// The pane this program runs in.
    pane: String,
    /// This program's own file name (no directory): the first word of the
    /// resume command. Empty when it could not be read, which withholds the
    /// resume command rather than sending one the host would refuse.
    binary_name: String,
}

impl ReporterConfig {
    /// Build a configuration directly (the seam the integration test uses to
    /// point the reporter at a stand-in host binary).
    #[must_use]
    pub fn new(
        bin: impl Into<String>,
        pane: impl Into<String>,
        binary_name: impl Into<String>,
    ) -> Self {
        Self {
            bin: bin.into(),
            pane: pane.into(),
            binary_name: binary_name.into(),
        }
    }

    /// Detect the hosting workspace manager this process runs in.
    ///
    /// The only place the real environment and this program's own path are
    /// read; everything testable takes them injected.
    #[must_use]
    pub(crate) fn detect_from_env() -> Option<Self> {
        Self::from_env(&|name| std::env::var(name).ok(), &own_binary_name())
    }

    /// Detect the hosted environment from injected data, or `None` when this
    /// process is not in a pane that exposes the reporting API.
    ///
    /// Every marker of the shared Herdr set is required, read through the
    /// profile's own [`value`] lookup and named by the profile's own constants:
    /// the profile calls a pane hosted only when that same full set is present,
    /// so the pane this client believes it is in is the pane it reports to. A
    /// host that sets only some of them (or a process carrying a stale
    /// inherited slice) never starts the reporter, which is what keeps the
    /// whole feature opt-in.
    fn from_env(env: &impl Fn(&str) -> Option<String>, binary_name: &str) -> Option<Self> {
        let bin = value(env, HERDR_BIN_PATH)?;
        let pane = value(env, HERDR_PANE_ID)?;
        // The opt-in switch itself carries no value of its own; its presence is
        // the whole claim.
        value(env, HERDR_ENV)?;
        Some(Self {
            bin,
            pane,
            binary_name: binary_name.to_string(),
        })
    }

    /// The host binary one report invokes.
    pub(super) fn bin(&self) -> &str {
        &self.bin
    }

    /// The pane id a report addresses.
    pub(super) fn pane(&self) -> &str {
        &self.pane
    }

    /// This program's own file name, the first word of the resume command.
    pub(super) fn binary_name(&self) -> &str {
        &self.binary_name
    }

    /// The length of the pane id, for a payload-free log line.
    pub(super) fn pane_len(&self) -> usize {
        self.pane.len()
    }

    /// Whether a resume command can be formed at all, for the same log line.
    pub(super) fn has_resume(&self) -> bool {
        !self.binary_name.is_empty()
    }
}

/// This program's own file name (no directory), for the resume command.
///
/// Empty when it cannot be read: a resume command is optional, so a program
/// that cannot name itself still reports state.
fn own_binary_name() -> String {
    std::env::current_exe()
        .ok()
        .as_deref()
        .and_then(std::path::Path::file_name)
        .and_then(std::ffi::OsStr::to_str)
        .unwrap_or_default()
        .to_string()
}

#[cfg(test)]
mod tests {
    use std::collections::HashMap;

    use super::*;

    /// An injected environment built from literal pairs.
    fn env_of(pairs: &[(&str, &str)]) -> impl Fn(&str) -> Option<String> {
        let map: HashMap<String, String> = pairs
            .iter()
            .map(|(name, value)| ((*name).to_string(), (*value).to_string()))
            .collect();
        move |name| map.get(name).cloned()
    }

    fn config() -> ReporterConfig {
        ReporterConfig::new("/usr/bin/host", "pane-7", "choreo-tui")
    }

    #[test]
    fn no_reporter_without_every_marker() {
        let chained = [
            ("HERDR_ENV", "1"),
            ("HERDR_BIN_PATH", "/usr/bin/host"),
            ("HERDR_PANE_ID", "pane-7"),
        ];
        // Any missing marker means no config, hence no thread and no argv — the
        // same full-set predicate the terminal profile applies before it calls
        // a pane hosted.
        for missing in 0..chained.len() {
            let partial: Vec<(&str, &str)> = chained
                .iter()
                .enumerate()
                .filter(|(index, _)| *index != missing)
                .map(|(_, pair)| *pair)
                .collect();
            assert_eq!(
                ReporterConfig::from_env(&env_of(&partial), "choreo-tui"),
                None,
                "{partial:?} must not start a reporter"
            );
        }
        // An empty value is not a marker claim either.
        let empty = [
            ("HERDR_ENV", ""),
            ("HERDR_BIN_PATH", ""),
            ("HERDR_PANE_ID", ""),
        ];
        assert_eq!(
            ReporterConfig::from_env(&env_of(&empty), "choreo-tui"),
            None
        );

        assert_eq!(
            ReporterConfig::from_env(&env_of(&chained), "choreo-tui"),
            Some(config())
        );
    }

    #[test]
    fn the_config_hands_the_argv_builder_its_parts() {
        let config = config();
        assert_eq!(config.bin(), "/usr/bin/host");
        assert_eq!(config.pane(), "pane-7");
        assert_eq!(config.binary_name(), "choreo-tui");
        assert_eq!(config.pane_len(), "pane-7".len());
        assert!(config.has_resume());
        // A program that cannot name itself still reports state.
        assert!(!ReporterConfig::new("/usr/bin/host", "pane-7", "").has_resume());
    }
}
