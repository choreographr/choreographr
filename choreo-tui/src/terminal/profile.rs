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

use std::fmt;

use ratatui_image::picker::ProtocolType;

/// One dotted version triple, parsed from an environment value that carries
/// one.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) struct Version {
    major: u32,
    minor: u32,
    patch: u32,
}

impl Version {
    /// Build a triple directly (used for the capability table's version
    /// floors).
    #[must_use]
    pub(crate) const fn new(major: u32, minor: u32, patch: u32) -> Self {
        Self {
            major,
            minor,
            patch,
        }
    }

    /// Parse a dotted `MAJOR[.MINOR[.PATCH]]` value.
    ///
    /// A component's trailing non-digit suffix is dropped, so `3.5a` (tmux's
    /// style) and `1.2.0` both parse. A value whose first component has no
    /// leading digits (`unknown`, `a.b`) is malformed and yields `None`, so the
    /// caller records "version unknown" instead of inventing one; a malformed
    /// *later* component only truncates the triple (`3.x` is 3.0.0), which is
    /// still a truthful lower bound for a version floor.
    #[must_use]
    pub(crate) fn parse(text: &str) -> Option<Self> {
        let mut components = text.trim().split('.');
        let major = leading_number(components.next()?)?;
        let minor = components.next().and_then(leading_number);
        // A malformed component ends the triple: reading a later component past
        // it would turn `3.x.1` into `3.0.1`, claiming a patch release the value
        // never named.
        let patch = minor
            .and_then(|_| components.next())
            .and_then(leading_number);
        Some(Self::new(major, minor.unwrap_or(0), patch.unwrap_or(0)))
    }

    /// Decode the packed decimal integer some terminals export instead of a
    /// dotted string: the last two digits are the patch, the two before those
    /// the minor, and the rest the major — so `KONSOLE_VERSION=260400` is 26.04
    /// and `VTE_VERSION=7400` is 0.74.
    #[must_use]
    pub(crate) const fn from_packed(packed: u32) -> Self {
        Self::new(packed / 10_000, (packed / 100) % 100, packed % 100)
    }

    /// Whether this version is at or above `floor`.
    ///
    /// Spelled out rather than derived from `PartialOrd` so the comparison is
    /// usable from the `const` literals of the capability table and states the
    /// ordering exactly once.
    #[must_use]
    pub(crate) const fn at_least(self, floor: Self) -> bool {
        if self.major != floor.major {
            return self.major > floor.major;
        }
        if self.minor != floor.minor {
            return self.minor > floor.minor;
        }
        self.patch >= floor.patch
    }
}

impl fmt::Display for Version {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{}.{}.{}", self.major, self.minor, self.patch)
    }
}

/// The leading decimal run of one dotted component, or `None` when it has none
/// or does not fit a `u32` (not a version we can compare, so no answer is
/// better than a wrapped one).
fn leading_number(component: &str) -> Option<u32> {
    component
        .chars()
        .take_while(char::is_ascii_digit)
        .collect::<String>()
        .parse()
        .ok()
}

/// Which terminal emulator this process talks to.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum TerminalKind {
    /// Ghostty — identified by `TERM_PROGRAM=ghostty` or `TERM=xterm-ghostty`.
    Ghostty,
    /// Kitty — identified by `KITTY_WINDOW_ID`.
    Kitty,
    /// `WezTerm` — identified by `TERM_PROGRAM=WezTerm`.
    WezTerm,
    /// Alacritty — identified by a `TERM` of `alacritty*`.
    Alacritty,
    /// Foot — identified by a `TERM` of `foot*`.
    Foot,
    /// iTerm2 — identified by `TERM_PROGRAM=iTerm.app`.
    ItTerm,
    /// Windows Terminal — identified by `WT_SESSION` (a per-pane GUID).
    WindowsTerminal,
    /// Konsole — identified by `KONSOLE_VERSION` (a packed integer).
    Konsole,
    /// Ptyxis — identified by `PTYXIS_VERSION`.
    Ptyxis,
    /// A bare VTE-based terminal — identified by `VTE_VERSION`.
    Vte,
    /// Apple's Terminal.app — identified by `TERM_PROGRAM=Apple_Terminal`.
    AppleTerminal,
    /// Termux (Android) — identified by `TERMUX_VERSION`.
    Termux,
    /// Rex, reached only as the terminal a Rex pane provides.
    Rex,
    /// No marker matched, or a layer above us hides the real terminal.
    Unknown,
}

impl fmt::Display for TerminalKind {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        let name = match self {
            Self::Ghostty => "ghostty",
            Self::Kitty => "kitty",
            Self::WezTerm => "wezterm",
            Self::Alacritty => "alacritty",
            Self::Foot => "foot",
            Self::ItTerm => "iterm2",
            Self::WindowsTerminal => "windows-terminal",
            Self::Konsole => "konsole",
            Self::Ptyxis => "ptyxis",
            Self::Vte => "vte",
            Self::AppleTerminal => "apple-terminal",
            Self::Termux => "termux",
            Self::Rex => "rex",
            Self::Unknown => "unknown",
        };
        f.write_str(name)
    }
}

/// The operating system this client runs on.
///
/// Android is separated from Linux because the two have different terminal
/// stacks and different platform conventions for keys and clipboard access,
/// even though a Termux build compiles under a `linux` target triple.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum Platform {
    /// Linux (including a non-Termux Android userland).
    Linux,
    /// macOS.
    MacOs,
    /// Windows.
    Windows,
    /// Android, as Termux presents it.
    Android,
}

impl Platform {
    /// The compile target's platform.
    #[must_use]
    pub(crate) const fn current() -> Self {
        if cfg!(target_os = "macos") {
            Self::MacOs
        } else if cfg!(target_os = "windows") {
            Self::Windows
        } else if cfg!(target_os = "android") {
            Self::Android
        } else {
            Self::Linux
        }
    }

    /// Refine a Linux host into Android when the environment is a Termux-style
    /// userland.
    ///
    /// The compile target cannot answer this on its own: a build for Android's
    /// bionic target does report `target_os = "android"`, but the Termux
    /// musl build (the one this project ships for Android) compiles under a
    /// `linux` triple. Termux exports `TERMUX_VERSION` and `PREFIX` (its
    /// app-private install prefix), so the environment decides in that case.
    #[must_use]
    fn refine_android(self, env: &impl Fn(&str) -> Option<String>) -> Self {
        let termux = value(env, "TERMUX_VERSION").is_some() || value(env, "PREFIX").is_some();
        if self == Self::Linux && termux {
            Self::Android
        } else {
            self
        }
    }
}

impl fmt::Display for Platform {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        let name = match self {
            Self::Linux => "linux",
            Self::MacOs => "macos",
            Self::Windows => "windows",
            Self::Android => "android",
        };
        f.write_str(name)
    }
}

/// What the image picker actually negotiated with this terminal at launch.
///
/// This is the observed result of `ratatui_image`'s startup capability query —
/// including its fallback to [`ImageSupport::Halfblocks`] when the query goes
/// unanswered — not a claim from the capability table.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum ImageSupport {
    /// Half-block (Unicode block) rendering: the universal fallback.
    Halfblocks,
    /// Sixel graphics.
    Sixel,
    /// The Kitty graphics protocol.
    Kitty,
    /// The iTerm2 inline-image protocol.
    Iterm2,
}

impl From<ProtocolType> for ImageSupport {
    fn from(protocol: ProtocolType) -> Self {
        match protocol {
            ProtocolType::Halfblocks => Self::Halfblocks,
            ProtocolType::Sixel => Self::Sixel,
            ProtocolType::Kitty => Self::Kitty,
            ProtocolType::Iterm2 => Self::Iterm2,
        }
    }
}

impl fmt::Display for ImageSupport {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        let name = match self {
            Self::Halfblocks => "halfblocks",
            Self::Sixel => "sixel",
            Self::Kitty => "kitty",
            Self::Iterm2 => "iterm2",
        };
        f.write_str(name)
    }
}

/// Which image protocols a terminal renders natively, or `Unknown` when the
/// table asserts nothing (a half-block fallback is always available, so the
/// absence of an answer never blocks rendering).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum NativeImages {
    /// No graphics protocol at all.
    None,
    /// Only half-block rendering — sixel exists but renders incorrectly, so it
    /// is not worth offering.
    Halfblocks,
    /// Sixel graphics.
    Sixel,
    /// The Kitty graphics protocol.
    Kitty,
    /// The iTerm2 inline-image protocol.
    Iterm2,
    /// Not asserted — the picker's query is the only evidence.
    Unknown,
}

impl fmt::Display for NativeImages {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        let name = match self {
            Self::None => "none",
            Self::Halfblocks => "halfblocks",
            Self::Sixel => "sixel",
            Self::Kitty => "kitty",
            Self::Iterm2 => "iterm2",
            Self::Unknown => "unknown",
        };
        f.write_str(name)
    }
}

/// A multiplexer sitting between this program and its terminal.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum Mux {
    /// tmux. `version` is present only when tmux itself reported it (tmux 3.2+
    /// exports `TERM_PROGRAM=tmux` and `TERM_PROGRAM_VERSION` into a pane).
    Tmux {
        /// tmux's version, when the pane environment carried one.
        version: Option<Version>,
    },
    /// Zellij.
    Zellij,
    /// GNU screen.
    Screen,
}

impl fmt::Display for Mux {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Tmux { version: Some(v) } => write!(f, "tmux {v}"),
            Self::Tmux { version: None } => f.write_str("tmux"),
            Self::Zellij => f.write_str("zellij"),
            Self::Screen => f.write_str("screen"),
        }
    }
}

/// A workspace manager hosting this program in one of its panes.
///
/// These set markers for their children and expose their own reporting
/// integration, which is a different relationship from a multiplexer's: the
/// layer is a first-class host, not a session wrapper.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum HostedKind {
    /// The markers `HERDR_ENV` / `HERDR_PANE_ID` / `HERDR_BIN_PATH`.
    Herdr,
    /// The markers `REX_SESSION` / `REX_BLOCK` / `REX_SERVER`, or a `TERM` of
    /// `xterm-rex`.
    Rex,
    /// Reserved for a further hosting workspace manager; nothing detects this
    /// yet, so it is never the product of [`detect`].
    // `dead_code`: nothing detects this layer yet, so no rule constructs the
    // variant. It stays in the vocabulary so a later detection rule does not have
    // to reshape the enum every consumer matches on.
    #[expect(dead_code, reason = "reserved for a host layer no rule identifies yet")]
    Other,
}

impl fmt::Display for HostedKind {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        let name = match self {
            Self::Herdr => "herdr",
            Self::Rex => "rex",
            Self::Other => "other",
        };
        f.write_str(name)
    }
}

/// The layer immediately hosting this program.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum Host {
    /// Nothing between the program and its terminal.
    Direct,
    /// A multiplexer.
    Mux(Mux),
    /// A workspace manager.
    Hosted(HostedKind),
}

impl fmt::Display for Host {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Direct => f.write_str("direct"),
            Self::Mux(mux) => write!(f, "{mux}"),
            Self::Hosted(kind) => write!(f, "{kind}"),
        }
    }
}

/// What the identified terminal supports, per capability.
///
/// Every field is `Some(true)` (supported), `Some(false)` (settled as not
/// supported), or `None` (not asserted).
#[derive(Clone, Copy, Debug)]
pub(crate) struct Capabilities {
    /// The Kitty keyboard protocol (disambiguated keys, key release/repeat).
    pub(crate) kitty_keyboard: Option<bool>,
    /// Synchronized output, DEC private mode 2026 (a frame is presented
    /// atomically).
    pub(crate) synchronized_output: Option<bool>,
    /// OSC 52 clipboard *writes*.
    pub(crate) clipboard_write: Option<bool>,
    /// OSC 9;4 progress reporting.
    pub(crate) progress: Option<bool>,
    /// OSC 7501 program-status reporting.
    pub(crate) program_status: Option<bool>,
    /// OSC 22 pointer-shape selection.
    pub(crate) pointer_shape: Option<bool>,
    /// Which image protocol the terminal renders natively.
    pub(crate) images: NativeImages,
}

impl Capabilities {
    /// The answer "nothing is asserted": used by an unidentified terminal and by
    /// any terminal whose remaining behaviour is unverified.
    const UNASSERTED: Self = Self {
        kitty_keyboard: None,
        synchronized_output: None,
        clipboard_write: None,
        progress: None,
        program_status: None,
        pointer_shape: None,
        images: NativeImages::Unknown,
    };

    /// A capability gated on a version floor: `Some(true)` at or above the
    /// floor, `Some(false)` below a *known* older version, and `None` when the
    /// version is unknown — the conservative direction, since only a version we
    /// actually read can rule a feature out.
    fn at_least(version: Option<Version>, floor: Version) -> Option<bool> {
        version.map(|v| v.at_least(floor))
    }
}

const YES: Option<bool> = Some(true);
const NO: Option<bool> = Some(false);

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
    /// The table is a `match` rather than a static array because the version-gated
    /// entries depend on the parsed version, and keeping each terminal's row in
    /// one block is what makes a row auditable against its source.
    #[must_use]
    pub(crate) fn capabilities(&self) -> Capabilities {
        let version = self.version;
        match self.name {
            // Ghostty 1.2 added OSC 9;4, and it is the only terminal that
            // implements OSC 7501.
            TerminalKind::Ghostty => Capabilities {
                kitty_keyboard: YES,
                synchronized_output: YES,
                clipboard_write: YES,
                progress: Capabilities::at_least(version, Version::new(1, 2, 0)),
                program_status: YES,
                pointer_shape: YES,
                images: NativeImages::Kitty,
            },
            // Kitty: graphics from 0.28, OSC 9;4 from 0.47.
            TerminalKind::Kitty => Capabilities {
                kitty_keyboard: YES,
                synchronized_output: YES,
                clipboard_write: YES,
                progress: Capabilities::at_least(version, Version::new(0, 47, 0)),
                program_status: NO,
                pointer_shape: YES,
                images: NativeImages::Kitty,
            },
            // WezTerm speaks the Kitty keyboard protocol only once
            // `enable_kitty_keyboard = true` is in its config, which we cannot
            // observe — so the claim is made and the user can turn it on. No
            // OSC 22 and no OSC 7501.
            TerminalKind::WezTerm => Capabilities {
                kitty_keyboard: YES,
                synchronized_output: YES,
                clipboard_write: YES,
                progress: YES,
                program_status: NO,
                pointer_shape: NO,
                images: NativeImages::Iterm2,
            },
            // Alacritty: keyboard protocol, synchronized output and OSC 52
            // arrived together in 0.13. No graphics, no progress, no OSC 22.
            TerminalKind::Alacritty => Capabilities {
                kitty_keyboard: Capabilities::at_least(version, Version::new(0, 13, 0)),
                synchronized_output: Capabilities::at_least(version, Version::new(0, 13, 0)),
                clipboard_write: Capabilities::at_least(version, Version::new(0, 13, 0)),
                progress: NO,
                program_status: NO,
                pointer_shape: NO,
                images: NativeImages::None,
            },
            // Foot: the keyboard protocol from 1.10.3, plus sixel. No OSC 9;4.
            TerminalKind::Foot => Capabilities {
                kitty_keyboard: Capabilities::at_least(version, Version::new(1, 10, 3)),
                synchronized_output: YES,
                clipboard_write: YES,
                progress: NO,
                program_status: NO,
                pointer_shape: YES,
                images: NativeImages::Sixel,
            },
            // Konsole: OSC 9;4 from 26.04. Its sixel support is known-buggy, so
            // images are advertised as half-block rather than sixel.
            TerminalKind::Konsole => Capabilities {
                kitty_keyboard: YES,
                synchronized_output: YES,
                clipboard_write: YES,
                progress: Capabilities::at_least(version, Version::new(26, 4, 0)),
                program_status: NO,
                pointer_shape: NO,
                images: NativeImages::Halfblocks,
            },
            // A bare VTE: its mode-2026 handler is an unimplemented TODO, it has
            // no keyboard-protocol support and no OSC 52, but it does render
            // OSC 9;4.
            TerminalKind::Vte => Capabilities {
                kitty_keyboard: NO,
                synchronized_output: NO,
                clipboard_write: NO,
                progress: YES,
                program_status: NO,
                pointer_shape: NO,
                images: NativeImages::None,
            },
            // iTerm2: everything but OSC 22 and OSC 7501, plus its own image
            // protocol.
            TerminalKind::ItTerm => Capabilities {
                kitty_keyboard: YES,
                synchronized_output: YES,
                clipboard_write: YES,
                progress: YES,
                program_status: NO,
                pointer_shape: NO,
                images: NativeImages::Iterm2,
            },
            // Terminal.app supports none of these.
            TerminalKind::AppleTerminal => Capabilities {
                kitty_keyboard: NO,
                synchronized_output: NO,
                clipboard_write: NO,
                progress: NO,
                program_status: NO,
                pointer_shape: NO,
                images: NativeImages::None,
            },
            // Windows Terminal: ConPTY does not answer the graphics capability
            // query, so images usually do not negotiate even though the terminal
            // can draw them — hence "not asserted" rather than "none". No OSC 22
            // (a console has no shape concept) and no OSC 7501.
            TerminalKind::WindowsTerminal => Capabilities {
                kitty_keyboard: YES,
                synchronized_output: YES,
                clipboard_write: YES,
                progress: YES,
                program_status: NO,
                pointer_shape: NO,
                images: NativeImages::Unknown,
            },
            // Termux: OSC 52 and sixel only.
            TerminalKind::Termux => Capabilities {
                kitty_keyboard: NO,
                synchronized_output: NO,
                clipboard_write: YES,
                progress: NO,
                program_status: NO,
                pointer_shape: NO,
                images: NativeImages::Sixel,
            },
            // Rex authored and implements the Program Status Protocol, so OSC
            // 7501 is the one thing its row asserts. Nothing else about its
            // terminal is verified.
            TerminalKind::Rex => Capabilities {
                program_status: YES,
                ..Capabilities::UNASSERTED
            },
            // Nothing verified about Ptyxis's own support, so nothing is
            // asserted — its VTE base is not evidence for its build's features.
            TerminalKind::Ptyxis | TerminalKind::Unknown => Capabilities::UNASSERTED,
        }
    }

    /// The version as a log/report string, `unknown` when the environment
    /// carried none.
    #[must_use]
    pub(crate) fn version_label(&self) -> String {
        self.version
            .map_or_else(|| "unknown".to_string(), |version| version.to_string())
    }
}

/// Read `name`, treating an empty value as absent: a marker variable set to the
/// empty string is not a claim.
fn value(env: &impl Fn(&str) -> Option<String>, name: &str) -> Option<String> {
    env(name).filter(|raw| !raw.is_empty())
}

/// Read `name` and record it in `evidence` when it is set, handing the value
/// back.
fn marker(
    env: &impl Fn(&str) -> Option<String>,
    evidence: &mut Vec<&'static str>,
    name: &'static str,
) -> Option<String> {
    let raw = value(env, name);
    if raw.is_some() {
        evidence.push(name);
    }
    raw
}

/// Record every variable in `names` that the environment sets, reporting
/// whether any did.
fn note_markers(
    env: &impl Fn(&str) -> Option<String>,
    evidence: &mut Vec<&'static str>,
    names: &[&'static str],
) -> bool {
    // A plain loop, not `any()`: that would stop at the first match and leave the
    // layer's other markers out of the evidence, which is the whole point of
    // collecting them.
    let mut matched = false;
    for name in names {
        if marker(env, evidence, name).is_some() {
            matched = true;
        }
    }
    matched
}

/// Identify the layer immediately hosting this program.
///
/// Both a hosting workspace manager and a multiplexer are detected on every
/// call, even though the host is one value: the layer that wins is the hosted
/// one (a workspace manager claims a richer relationship with its panes than a
/// session wrapper does), and knowing what else matched is what lets the audit
/// line explain a nested session.
fn detect_host(env: &impl Fn(&str) -> Option<String>, evidence: &mut Vec<&'static str>) -> Host {
    let hosted = detect_hosted(env, evidence);
    let mux = detect_mux(env, evidence);
    match (hosted, mux) {
        (Some(kind), _) => Host::Hosted(kind),
        (None, Some(mux)) => Host::Mux(mux),
        (None, None) => Host::Direct,
    }
}

/// Identify a hosting workspace manager, its markers outranking a
/// multiplexer's.
fn detect_hosted(
    env: &impl Fn(&str) -> Option<String>,
    evidence: &mut Vec<&'static str>,
) -> Option<HostedKind> {
    let mut hosted: Option<HostedKind> = None;
    if note_markers(
        env,
        evidence,
        &["HERDR_ENV", "HERDR_PANE_ID", "HERDR_BIN_PATH"],
    ) {
        hosted.get_or_insert(HostedKind::Herdr);
    }
    // Rex ships its own terminfo entry under `xterm-rex`, so its panes export
    // that TERM even when none of the REX_* markers leak into the environment.
    let rex_term = value(env, "TERM").is_some_and(|term| term == "xterm-rex");
    if rex_term {
        evidence.push("TERM=xterm-rex");
    }
    if note_markers(env, evidence, &["REX_SESSION", "REX_BLOCK", "REX_SERVER"]) || rex_term {
        hosted.get_or_insert(HostedKind::Rex);
    }
    hosted
}

/// Identify a session multiplexer, recording every multiplexer marker that
/// matched so one nested inside another is visible in the audit line.
fn detect_mux(
    env: &impl Fn(&str) -> Option<String>,
    evidence: &mut Vec<&'static str>,
) -> Option<Mux> {
    let tmux = value(env, "TMUX").is_some();
    let zellij = value(env, "ZELLIJ").is_some();
    let screen = value(env, "STY").is_some();
    let mux = if tmux {
        Some(Mux::Tmux {
            version: tmux_version(env, evidence),
        })
    } else if zellij {
        Some(Mux::Zellij)
    } else if screen {
        Some(Mux::Screen)
    } else {
        None
    };
    for (set, name) in [(tmux, "TMUX"), (zellij, "ZELLIJ"), (screen, "STY")] {
        if set {
            evidence.push(name);
        }
    }
    mux
}

/// tmux's own version, read from the pair it exports into a pane (3.2+).
///
/// `TMUX` alone says nothing about which tmux it is, so an older or otherwise
/// silent tmux yields `None` rather than a guess.
fn tmux_version(
    env: &impl Fn(&str) -> Option<String>,
    evidence: &mut Vec<&'static str>,
) -> Option<Version> {
    if value(env, "TERM_PROGRAM").as_deref() != Some("tmux") {
        return None;
    }
    evidence.push("TERM_PROGRAM=tmux");
    value(env, "TERM_PROGRAM_VERSION").and_then(|raw| Version::parse(&raw))
}

/// Identify the terminal emulator, for a [`Host::Direct`] session only.
///
/// Checks run from the most distinctive marker to the least, so a variable only
/// one terminal exports wins over an inherited `TERM`.
fn detect_terminal(
    env: &impl Fn(&str) -> Option<String>,
    evidence: &mut Vec<&'static str>,
) -> (TerminalKind, Option<Version>) {
    // Per-pane / per-session identifiers: each is exported by exactly one
    // terminal, so it is unambiguous even when the environment carries an
    // inherited TERM.
    if marker(env, evidence, "WT_SESSION").is_some() {
        return (TerminalKind::WindowsTerminal, None);
    }
    if let Some(raw) = marker(env, evidence, "TERMUX_VERSION") {
        return (TerminalKind::Termux, Version::parse(&raw));
    }
    if marker(env, evidence, "KITTY_WINDOW_ID").is_some() {
        return (TerminalKind::Kitty, None);
    }

    if let Some(program) = marker(env, evidence, "TERM_PROGRAM") {
        let known = match program.as_str() {
            "ghostty" => Some(TerminalKind::Ghostty),
            "iTerm.app" => Some(TerminalKind::ItTerm),
            "WezTerm" => Some(TerminalKind::WezTerm),
            "Apple_Terminal" => Some(TerminalKind::AppleTerminal),
            _ => None,
        };
        if let Some(kind) = known {
            return (kind, terminal_program_version(env, evidence));
        }
        // A TERM_PROGRAM we recognize as an application but have no row for (a
        // code editor's embedded terminal, an Electron shell, mintty, rio) still
        // names the program the user is looking at — the toolkit variables below
        // would describe the engine underneath it, not that program. So its
        // presence ends identification as Unknown.
        return (TerminalKind::Unknown, None);
    }

    // Toolkit version variables, each exported only by terminals built on that
    // toolkit.
    if let Some(raw) = marker(env, evidence, "KONSOLE_VERSION") {
        return (TerminalKind::Konsole, parse_packed_or_dotted(&raw));
    }
    if let Some(raw) = marker(env, evidence, "PTYXIS_VERSION") {
        return (TerminalKind::Ptyxis, Version::parse(&raw));
    }
    if let Some(raw) = marker(env, evidence, "VTE_VERSION") {
        return (TerminalKind::Vte, parse_packed_or_dotted(&raw));
    }

    // TERM's prefix names the emulator, unless it only names a capability tier
    // (`xterm-256color`, `screen-256color`) shared by many — in which case no
    // marker is recorded and identification stays Unknown.
    if let Some(term) = value(env, "TERM") {
        if term.starts_with("foot") {
            evidence.push("TERM=foot");
            return (TerminalKind::Foot, None);
        }
        if term.starts_with("alacritty") {
            evidence.push("TERM=alacritty");
            return (TerminalKind::Alacritty, None);
        }
        if term.starts_with("xterm-ghostty") {
            evidence.push("TERM=xterm-ghostty");
            return (TerminalKind::Ghostty, None);
        }
    }
    (TerminalKind::Unknown, None)
}

/// The version a `TERM_PROGRAM`-identified terminal exports alongside its name.
fn terminal_program_version(
    env: &impl Fn(&str) -> Option<String>,
    evidence: &mut Vec<&'static str>,
) -> Option<Version> {
    marker(env, evidence, "TERM_PROGRAM_VERSION").and_then(|raw| Version::parse(&raw))
}

/// Parse a version value that may be dotted (`26.04.0`) or the packed decimal
/// integer `KONSOLE_VERSION` / `VTE_VERSION` use (`260400`, `7400`).
fn parse_packed_or_dotted(raw: &str) -> Option<Version> {
    if raw.contains('.') {
        Version::parse(raw)
    } else {
        raw.parse::<u32>().ok().map(Version::from_packed)
    }
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

    fn detect_linux(pairs: &[(&str, &str)]) -> TerminalProfile {
        TerminalProfile::detect(env_of(pairs), Platform::Linux, ImageSupport::Halfblocks)
    }

    #[test]
    fn version_parses_dotted_suffixes_and_rejects_malformed() {
        assert_eq!(Version::parse("1.2.0"), Some(Version::new(1, 2, 0)));
        assert_eq!(Version::parse("3.5a"), Some(Version::new(3, 5, 0)));
        assert_eq!(Version::parse("3"), Some(Version::new(3, 0, 0)));
        assert_eq!(Version::parse(" 0.47 "), Some(Version::new(0, 47, 0)));
        // A malformed later component truncates rather than discarding.
        assert_eq!(Version::parse("3.x.1"), Some(Version::new(3, 0, 0)));
        assert_eq!(Version::parse("unknown"), None);
        assert_eq!(Version::parse(""), None);
        assert_eq!(Version::parse("a.b"), None);
    }

    #[test]
    fn version_decodes_the_packed_integer_form() {
        assert_eq!(Version::from_packed(260_400), Version::new(26, 4, 0));
        assert_eq!(Version::from_packed(7_400), Version::new(0, 74, 0));
    }

    #[test]
    fn version_ordering_compares_major_then_minor_then_patch() {
        assert!(Version::new(1, 2, 0).at_least(Version::new(1, 2, 0)));
        assert!(Version::new(1, 3, 0).at_least(Version::new(1, 2, 9)));
        assert!(Version::new(2, 0, 0).at_least(Version::new(1, 99, 99)));
        assert!(!Version::new(1, 2, 0).at_least(Version::new(1, 2, 1)));
        assert!(!Version::new(1, 1, 9).at_least(Version::new(1, 2, 0)));
    }

    #[test]
    fn empty_environment_is_unknown_with_no_evidence() {
        let profile = detect_linux(&[]);
        assert_eq!(profile.name, TerminalKind::Unknown);
        assert_eq!(profile.version, None);
        assert_eq!(profile.host, Host::Direct);
        assert_eq!(profile.evidence, Vec::<&'static str>::new());
        assert_eq!(profile.capabilities().program_status, None);
    }

    #[test]
    fn identifies_each_terminal_from_its_own_marker() {
        let cases: &[(&[(&str, &str)], TerminalKind)] = &[
            (&[("WT_SESSION", "guid")], TerminalKind::WindowsTerminal),
            (&[("TERMUX_VERSION", "0.118.0")], TerminalKind::Termux),
            (&[("KITTY_WINDOW_ID", "1")], TerminalKind::Kitty),
            (&[("TERM_PROGRAM", "ghostty")], TerminalKind::Ghostty),
            (&[("TERM_PROGRAM", "WezTerm")], TerminalKind::WezTerm),
            (&[("TERM_PROGRAM", "iTerm.app")], TerminalKind::ItTerm),
            (
                &[("TERM_PROGRAM", "Apple_Terminal")],
                TerminalKind::AppleTerminal,
            ),
            (&[("KONSOLE_VERSION", "260400")], TerminalKind::Konsole),
            (&[("PTYXIS_VERSION", "47.1")], TerminalKind::Ptyxis),
            (&[("VTE_VERSION", "7400")], TerminalKind::Vte),
            (&[("TERM", "foot-extra")], TerminalKind::Foot),
            (&[("TERM", "alacritty-direct")], TerminalKind::Alacritty),
            (&[("TERM", "xterm-ghostty")], TerminalKind::Ghostty),
        ];
        for (env, expected) in cases {
            let profile = detect_linux(env);
            assert_eq!(profile.name, *expected, "env {env:?}");
            assert_eq!(profile.host, Host::Direct, "env {env:?}");
        }
    }

    #[test]
    fn a_capability_tier_term_is_not_an_identity() {
        let profile = detect_linux(&[("TERM", "xterm-256color")]);
        assert_eq!(profile.name, TerminalKind::Unknown);
        assert_eq!(profile.evidence, Vec::<&'static str>::new());
    }

    #[test]
    fn a_code_editors_terminal_ends_identification_as_unknown() {
        let profile = detect_linux(&[("TERM_PROGRAM", "vscode"), ("TERM", "xterm-256color")]);
        assert_eq!(profile.name, TerminalKind::Unknown);
        // The application was named, so the audit line records that it was seen.
        assert_eq!(profile.evidence, vec!["TERM_PROGRAM"]);
    }

    #[test]
    fn terminal_version_is_parsed_where_the_environment_carries_one() {
        let ghostty = detect_linux(&[
            ("TERM_PROGRAM", "ghostty"),
            ("TERM_PROGRAM_VERSION", "1.4.0"),
        ]);
        assert_eq!(ghostty.version, Some(Version::new(1, 4, 0)));
        assert_eq!(ghostty.version_label(), "1.4.0");

        let konsole = detect_linux(&[("KONSOLE_VERSION", "260400")]);
        assert_eq!(konsole.version, Some(Version::new(26, 4, 0)));

        let kitty = detect_linux(&[("KITTY_WINDOW_ID", "1")]);
        assert_eq!(kitty.version, None);
        assert_eq!(kitty.version_label(), "unknown");
    }

    #[test]
    fn detects_each_host_layer() {
        let herdr = detect_linux(&[("HERDR_PANE_ID", "pane-7")]);
        assert_eq!(herdr.host, Host::Hosted(HostedKind::Herdr));

        let rex = detect_linux(&[("REX_SESSION", "s-1")]);
        assert_eq!(rex.host, Host::Hosted(HostedKind::Rex));

        let zellij = detect_linux(&[("ZELLIJ", "0")]);
        assert_eq!(zellij.host, Host::Mux(Mux::Zellij));

        let screen = detect_linux(&[("STY", "1234.pts-0.host")]);
        assert_eq!(screen.host, Host::Mux(Mux::Screen));
    }

    #[test]
    fn rex_is_recognized_by_its_terminfo_entry_alone() {
        let profile = detect_linux(&[("TERM", "xterm-rex")]);
        assert_eq!(profile.host, Host::Hosted(HostedKind::Rex));
        // Rex is the terminal in its own panes, so its identity is known.
        assert_eq!(profile.name, TerminalKind::Rex);
    }

    #[test]
    fn a_hosted_layer_outranks_a_multiplexer() {
        let profile = detect_linux(&[("HERDR_ENV", "1"), ("TMUX", "/tmp/tmux-1000/default,1,0")]);
        assert_eq!(profile.host, Host::Hosted(HostedKind::Herdr));
        // The multiplexer was still seen, so the audit line explains the nesting.
        assert!(profile.evidence.contains(&"TMUX"));
        assert!(profile.evidence.contains(&"HERDR_ENV"));
        // A layer above the terminal hides which terminal it is.
        assert_eq!(profile.name, TerminalKind::Unknown);
    }

    #[test]
    fn a_multiplexer_hides_the_terminal_identity() {
        let profile = detect_linux(&[
            ("TMUX", "/tmp/tmux-1000/default,1,0"),
            ("TERM_PROGRAM", "ghostty"),
            ("TERM_PROGRAM_VERSION", "1.4.0"),
            ("KITTY_WINDOW_ID", "1"),
        ]);
        assert_eq!(profile.name, TerminalKind::Unknown);
        assert_eq!(profile.version, None);
    }

    #[test]
    fn tmux_version_is_read_where_present_absent_or_malformed() {
        let present = detect_linux(&[
            ("TMUX", "/tmp/tmux-1000/default,1,0"),
            ("TERM_PROGRAM", "tmux"),
            ("TERM_PROGRAM_VERSION", "3.5a"),
        ]);
        assert_eq!(
            present.host,
            Host::Mux(Mux::Tmux {
                version: Some(Version::new(3, 5, 0))
            })
        );

        // tmux older than 3.2 exports neither variable.
        let absent = detect_linux(&[("TMUX", "/tmp/tmux-1000/default,1,0")]);
        assert_eq!(absent.host, Host::Mux(Mux::Tmux { version: None }));

        let malformed = detect_linux(&[
            ("TMUX", "/tmp/tmux-1000/default,1,0"),
            ("TERM_PROGRAM", "tmux"),
            ("TERM_PROGRAM_VERSION", "not-a-version"),
        ]);
        assert_eq!(malformed.host, Host::Mux(Mux::Tmux { version: None }));
        assert_eq!(malformed.host.to_string(), "tmux");
    }

    #[test]
    fn termux_refines_linux_into_android() {
        let android = detect_linux(&[
            ("TERMUX_VERSION", "0.118.0"),
            ("PREFIX", "/data/data/com.termux/files/usr"),
        ]);
        assert_eq!(android.platform, Platform::Android);
        assert_eq!(android.name, TerminalKind::Termux);

        // The PREFIX alone is enough: it is Termux's own layout.
        let prefix_only = detect_linux(&[("PREFIX", "/data/data/com.termux/files/usr")]);
        assert_eq!(prefix_only.platform, Platform::Android);

        let plain = detect_linux(&[("TERM", "xterm-256color")]);
        assert_eq!(plain.platform, Platform::Linux);
    }

    #[test]
    fn an_explicitly_android_target_keeps_its_platform() {
        let profile = TerminalProfile::detect(env_of(&[]), Platform::Android, ImageSupport::Kitty);
        assert_eq!(profile.platform, Platform::Android);
    }

    #[test]
    fn unknown_asserts_nothing_rather_than_denying() {
        let caps = detect_linux(&[]).capabilities();
        assert_eq!(caps.kitty_keyboard, None);
        assert_eq!(caps.synchronized_output, None);
        assert_eq!(caps.clipboard_write, None);
        assert_eq!(caps.progress, None);
        assert_eq!(caps.program_status, None);
        assert_eq!(caps.pointer_shape, None);
        assert_eq!(caps.images, NativeImages::Unknown);
    }

    #[test]
    fn rex_asserts_its_program_status_and_nothing_else() {
        let caps = detect_linux(&[("REX_SESSION", "s-1")]).capabilities();
        assert_eq!(caps.program_status, Some(true));
        assert_eq!(caps.kitty_keyboard, None);
        assert_eq!(caps.synchronized_output, None);
        assert_eq!(caps.clipboard_write, None);
        assert_eq!(caps.progress, None);
        assert_eq!(caps.pointer_shape, None);
        assert_eq!(caps.images, NativeImages::Unknown);
    }

    #[test]
    fn ghostty_is_the_only_program_status_terminal_with_mode_2026() {
        let caps = detect_linux(&[
            ("TERM_PROGRAM", "ghostty"),
            ("TERM_PROGRAM_VERSION", "1.4.0"),
        ])
        .capabilities();
        assert_eq!(caps.program_status, Some(true));
        assert_eq!(caps.synchronized_output, Some(true));
        assert_eq!(caps.pointer_shape, Some(true));
        assert_eq!(caps.progress, Some(true));
        assert_eq!(caps.images, NativeImages::Kitty);
    }

    #[test]
    fn version_floors_gate_progress_and_images() {
        // Ghostty below the 1.2 floor has no OSC 9;4, and an unknown version
        // cannot rule it out.
        let old_ghostty = detect_linux(&[
            ("TERM_PROGRAM", "ghostty"),
            ("TERM_PROGRAM_VERSION", "1.1.0"),
        ]);
        assert_eq!(old_ghostty.capabilities().progress, Some(false));
        let unknown_version = detect_linux(&[("TERM", "xterm-ghostty")]);
        assert_eq!(unknown_version.capabilities().progress, None);

        // Kitty: OSC 9;4 needs 0.47.
        let kitty_old = detect_linux(&[("TERM_PROGRAM", "kitty")]);
        assert_eq!(kitty_old.capabilities().progress, None);
        let kitty = TerminalProfile {
            name: TerminalKind::Kitty,
            version: Some(Version::new(0, 46, 0)),
            ..detect_linux(&[])
        };
        assert_eq!(kitty.capabilities().progress, Some(false));
        let kitty_new = TerminalProfile {
            name: TerminalKind::Kitty,
            version: Some(Version::new(0, 47, 0)),
            ..detect_linux(&[])
        };
        assert_eq!(kitty_new.capabilities().progress, Some(true));
        assert_eq!(kitty_new.capabilities().images, NativeImages::Kitty);
    }

    #[test]
    fn vte_refuses_mode_2026_and_denies_osc_52() {
        let caps = detect_linux(&[("VTE_VERSION", "7400")]).capabilities();
        assert_eq!(caps.synchronized_output, Some(false));
        assert_eq!(caps.clipboard_write, Some(false));
        assert_eq!(caps.kitty_keyboard, Some(false));
        assert_eq!(caps.progress, Some(true));
        assert_eq!(caps.images, NativeImages::None);
    }

    #[test]
    fn konsole_reports_halfblocks_and_a_2604_progress_floor() {
        let old = detect_linux(&[("KONSOLE_VERSION", "240800")]);
        assert_eq!(old.capabilities().progress, Some(false));
        assert_eq!(old.capabilities().images, NativeImages::Halfblocks);
        let new = detect_linux(&[("KONSOLE_VERSION", "260400")]);
        assert_eq!(new.capabilities().progress, Some(true));
    }

    #[test]
    fn windows_terminal_leaves_images_unasserted() {
        let caps = detect_linux(&[("WT_SESSION", "guid")]).capabilities();
        assert_eq!(caps.images, NativeImages::Unknown);
        assert_eq!(caps.kitty_keyboard, Some(true));
        assert_eq!(caps.program_status, Some(false));
    }

    #[test]
    fn empty_values_are_not_markers() {
        let profile = detect_linux(&[("TMUX", ""), ("KITTY_WINDOW_ID", "")]);
        assert_eq!(profile.host, Host::Direct);
        assert_eq!(profile.name, TerminalKind::Unknown);
        assert_eq!(profile.evidence, Vec::<&'static str>::new());
    }

    #[test]
    fn display_renders_the_log_fields() {
        let profile = detect_linux(&[
            ("TMUX", "/tmp/tmux-1000/default,1,0"),
            ("TERM_PROGRAM", "tmux"),
            ("TERM_PROGRAM_VERSION", "3.5"),
        ]);
        assert_eq!(profile.host.to_string(), "tmux 3.5.0");
        assert_eq!(profile.platform.to_string(), "linux");
        assert_eq!(profile.images.to_string(), "halfblocks");
        assert_eq!(profile.name.to_string(), "unknown");
    }

    #[test]
    fn image_support_follows_the_picker() {
        assert_eq!(
            ImageSupport::from(ProtocolType::Halfblocks),
            ImageSupport::Halfblocks
        );
        assert_eq!(ImageSupport::from(ProtocolType::Sixel), ImageSupport::Sixel);
        assert_eq!(ImageSupport::from(ProtocolType::Kitty), ImageSupport::Kitty);
        assert_eq!(
            ImageSupport::from(ProtocolType::Iterm2),
            ImageSupport::Iterm2
        );
        assert_eq!(ImageSupport::Kitty.to_string(), "kitty");
    }

    #[test]
    fn the_default_profile_is_the_unidentified_one() {
        let profile = TerminalProfile::default();
        assert_eq!(profile.name, TerminalKind::Unknown);
        assert_eq!(profile.host, Host::Direct);
        assert_eq!(profile.images, ImageSupport::Halfblocks);
        assert_eq!(profile.evidence, Vec::<&'static str>::new());
    }
}
