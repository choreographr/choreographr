//! Launch-time identification of the terminal, the layer hosting it, and what
//! each of them can actually do.
//!
//! [`TerminalProfile::detect`] is a pure function over injected data — an
//! environment lookup, the compile-time platform, and the image protocol
//! `ratatui_image`'s picker negotiated — so every rule below is unit-testable
//! without touching the real process environment. `run_app` calls it once,
//! beside the picker, and stores the immutable result on `App`.
//!
//! **Host versus terminal.** A multiplexer (tmux, zellij, screen) or a hosting
//! workspace manager sits between this program and the emulator, rewrites
//! `TERM`, and passes the outer program's markers through — so a terminal
//! identity read underneath one names the wrong terminal. Identity is therefore
//! reported only for [`Host::Direct`], plus Rex, which is by definition the
//! terminal its own `xterm-rex` entry describes. Under any other layer the
//! terminal is [`TerminalKind::Unknown`] and the layer is what is reported.
//!
//! **Capabilities are three-state.** A [`Capabilities`] field is `Some(true)`
//! or `Some(false)` only where the terminal's own source or documentation
//! settles the question, and `None` everywhere else. An unidentified terminal —
//! and Rex, whose non-status behaviour is unverified — answers `None` to every
//! question rather than `Some(false)`: a later phase acting on a false negative
//! (skipping OSC 7501 on a terminal that honours it) is worse than acting on no
//! answer at all.
//!
//! **Layout.** The item groups live in submodules under this directory: [`types`]
//! holds the plain data and its `Display`/`From` impls, [`version`] the dotted
//! and packed version parsing, [`detect`] the environment-marker rules, and
//! [`capabilities`] the per-terminal capability table — one row per terminal, in
//! a file of its own, so the next terminal lands as a focused diff. This module
//! owns [`TerminalProfile`] and re-exports every item, so callers keep naming
//! `crate::terminal::profile::<Item>` exactly as before.

mod capabilities;
mod detect;
mod types;
mod version;

#[cfg(test)]
mod tests;

pub(crate) use capabilities::table_capabilities;
pub(crate) use detect::{HERDR_BIN_PATH, HERDR_ENV, HERDR_PANE_ID, value};
pub(crate) use types::*;
pub(crate) use version::*;

use capabilities::capabilities_of;
use detect::{detect_host, detect_terminal};

/// The terminal, its host layer, and the evidence that identified them.
///
/// Built once per process (see [`TerminalProfile::detect`]), immutable
/// afterwards, and stored on `App` so later phases — the welcome notice, the
/// Terminal page, the progress/status gates — read one consistent answer
/// instead of re-deriving it from a possibly changed environment.
#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct TerminalProfile {
    /// The identified terminal, or [`TerminalKind::Unknown`].
    pub(crate) name: TerminalKind,
    /// The terminal's version, where an environment value carried one.
    pub(crate) version: Option<Version>,
    /// The operating system, with a Termux userland reported as Android.
    pub(crate) platform: Platform,
    /// The image protocol the picker negotiated.
    pub(crate) images: ImageSupport,
    /// The layer immediately hosting this program.
    pub(crate) host: Host,
    /// Which marker variables matched, in the order they were checked — the
    /// whole chain for a nested session, not only the winning layer's, so the
    /// audit line explains the verdict.
    pub(crate) evidence: Vec<&'static str>,
}

impl Default for TerminalProfile {
    /// The un-identified profile: `Unknown` terminal, `Direct` host, the real
    /// platform, and half-block images. This is what `App::new` starts from, so
    /// a caller that never runs detection (every test) still holds a valid
    /// value.
    fn default() -> Self {
        Self {
            name: TerminalKind::Unknown,
            version: None,
            platform: Platform::current(),
            images: ImageSupport::Halfblocks,
            host: Host::Direct,
            evidence: Vec::new(),
        }
    }
}

impl TerminalProfile {
    /// Identify the terminal and its host layer in this process.
    ///
    /// The only place the real environment is read; everything below takes it
    /// injected.
    #[must_use]
    pub(crate) fn detect_process(images: ImageSupport) -> Self {
        Self::detect(|name| std::env::var(name).ok(), Platform::current(), images)
    }

    /// Identify the terminal and its host layer from injected data.
    ///
    /// `env` returns `Some(value)` for a set, non-empty variable, `platform` is
    /// the compile target's platform (refined to Android when the environment
    /// says Termux), and `images` is the protocol the picker negotiated.
    #[must_use]
    pub(crate) fn detect(
        env: impl Fn(&str) -> Option<String>,
        platform: Platform,
        images: ImageSupport,
    ) -> Self {
        let mut evidence = Vec::new();
        let host = detect_host(&env, &mut evidence);
        // Terminal identity is only trustworthy with nothing between us and the
        // emulator — see the module docs. Rex is the exception because the
        // terminal in a Rex pane *is* Rex's own.
        let (name, version) = match &host {
            Host::Direct => detect_terminal(&env, &mut evidence),
            Host::Hosted(HostedKind::Rex) => (TerminalKind::Rex, None),
            Host::Mux(_) | Host::Hosted(_) => (TerminalKind::Unknown, None),
        };
        Self {
            name,
            version,
            platform: platform.refine_android(&env),
            images,
            host,
            evidence,
        }
    }

    /// What this terminal supports, per the capability table.
    ///
    /// The table itself lives in [`capabilities_of`], which also applies the
    /// picker's negotiated image protocol and the hosting layer's own answers.
    #[must_use]
    pub(crate) fn capabilities(&self) -> Capabilities {
        capabilities_of(self.name, self.version, self.images, self.host)
    }

    /// The capability answers of the layer hosting this program, or the empty
    /// set when the program talks to a terminal directly.
    #[must_use]
    pub(crate) fn host_capabilities(&self) -> Capabilities {
        self.host.capabilities()
    }

    /// The version as a log/report string, `unknown` when the environment
    /// carried none.
    #[must_use]
    pub(crate) fn version_label(&self) -> String {
        self.version
            .map_or_else(|| "unknown".to_string(), |version| version.to_string())
    }
}
