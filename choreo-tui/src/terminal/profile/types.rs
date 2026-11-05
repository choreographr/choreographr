//! The profile's plain data — terminal, platform, image support, host layer —
//! the capability record, and the `Display`/`From` impls that render them for
//! the log and the Terminal page.
//!
//! Nothing here reads the environment or holds a table: these are the vocabulary
//! types that [`super::detect`] produces and [`super::capabilities`] fills in.

use std::fmt;

use ratatui_image::picker::ProtocolType;

use super::detect::value;
use super::version::Version;

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

impl TerminalKind {
    /// The terminal's name as a person reads it: the user-facing spelling the
    /// welcome notice and the Terminal page print.
    ///
    /// Distinct from the machine [`Display`](fmt::Display) name used in logs and
    /// reports (e.g. `Windows Terminal` here versus `windows-terminal` there).
    /// `Unknown` reads as the neutral "this terminal" so a caller can name it
    /// without inventing an identity.
    #[must_use]
    pub(crate) const fn display_name(self) -> &'static str {
        match self {
            Self::Ghostty => "Ghostty",
            Self::Kitty => "kitty",
            Self::WezTerm => "WezTerm",
            Self::Alacritty => "Alacritty",
            Self::Foot => "foot",
            Self::ItTerm => "iTerm2",
            Self::WindowsTerminal => "Windows Terminal",
            Self::Konsole => "Konsole",
            Self::Ptyxis => "Ptyxis",
            Self::Vte => "a VTE terminal",
            Self::AppleTerminal => "Terminal.app",
            Self::Termux => "Termux",
            Self::Rex => "Rex",
            Self::Unknown => "this terminal",
        }
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
    ///
    /// `TERMUX_VERSION` is the marker; `PREFIX` only counts when its value names
    /// Termux's own `/data/data/<app>/files/usr` layout, because `PREFIX` on its
    /// own is an ordinary variable that build and development environments
    /// export. A false positive here is not harmless: it switches the platform
    /// advice to Android, which offers no alternative terminal and no
    /// Option-as-Meta setting.
    ///
    /// Called by [`super::TerminalProfile::detect`], which owns the injected
    /// environment.
    #[must_use]
    pub(crate) fn refine_android(self, env: &impl Fn(&str) -> Option<String>) -> Self {
        let termux = value(env, "TERMUX_VERSION").is_some()
            || value(env, "PREFIX").is_some_and(|prefix| prefix.starts_with("/data/data/"));
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
    /// yet, so it is never the product of [`super::TerminalProfile::detect`].
    // `dead_code`: nothing detects this layer yet, so no rule constructs the
    // variant. It stays in the vocabulary so a later detection rule does not have
    // to reshape the enum every consumer matches on.
    #[expect(dead_code, reason = "reserved for a host layer no rule identifies yet")]
    Other,
}

impl HostedKind {
    /// What the hosting layer itself provides to a program in one of its panes.
    ///
    /// A workspace manager owns the terminal, so these answers come from its own
    /// emulator rather than from the user's terminal: a feature it handles is one
    /// the client can use no matter what the outer terminal is. Only behaviour
    /// verified in the host's own implementation is asserted here.
    #[must_use]
    pub(crate) fn capabilities(self) -> Capabilities {
        match self {
            // Its panes are parsed by a vendored libghostty core: OSC 52 writes
            // are captured and re-emitted to the viewer's clipboard, kitty
            // graphics are re-emitted to the host terminal, and synchronized
            // output is consumed so the host composes its own frames. Program
            // status is deliberately left unasserted — its terminal app does not
            // read OSC 7501 (a program reports through the pane API instead).
            Self::Herdr => Capabilities {
                synchronized_output: YES,
                clipboard_write: YES,
                ..Capabilities::UNASSERTED
            },
            // Implements the program-status protocol it publishes, so OSC 7501
            // reaches the host's own status view.
            Self::Rex => Capabilities {
                program_status: YES,
                ..Capabilities::UNASSERTED
            },
            Self::Other => Capabilities::UNASSERTED,
        }
    }
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

impl Host {
    /// The capability answers of the hosting layer, or the empty set when the
    /// program talks to a terminal directly.
    ///
    /// A multiplexer asserts nothing here: it is a session wrapper, not an
    /// emulator, so the terminal's own row is the only evidence about features.
    #[must_use]
    pub(crate) fn capabilities(self) -> Capabilities {
        match self {
            Self::Hosted(kind) => kind.capabilities(),
            Self::Direct | Self::Mux(_) => Capabilities::UNASSERTED,
        }
    }
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
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
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
    pub(crate) const UNASSERTED: Self = Self {
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
    ///
    /// A terminal identified only by a version-less marker — a `TERM` prefix
    /// (`alacritty*`, `foot*`) or `KITTY_WINDOW_ID` — always takes the `None`
    /// arm at runtime: the marker names the terminal but carries no version, so
    /// there is no evidence to place against the floor. The gate itself stays,
    /// because an explicit version (a test's, or a future version source's) is
    /// evaluated normally. At runtime such a row stays unasserted, which is the
    /// honest answer for a terminal whose version never reached us — and for the
    /// `progress` row that still means the bar is drawn, because the OSC 9;4
    /// gate emits on `None` (an unsupported 9;4 is inert).
    pub(crate) fn at_least(version: Option<Version>, floor: Version) -> Option<bool> {
        version.map(|v| v.at_least(floor))
    }
}

/// Shorthand for an asserted "yes" in a capability row.
pub(crate) const YES: Option<bool> = Some(true);
/// Shorthand for a settled "no" in a capability row.
pub(crate) const NO: Option<bool> = Some(false);
