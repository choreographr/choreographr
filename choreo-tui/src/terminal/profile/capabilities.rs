//! The per-terminal capability table.
//!
//! One row per terminal, kept as a `match` rather than a static array because
//! the version-gated entries depend on the parsed version, and keeping each
//! terminal's row in one block is what makes a row auditable against its
//! source. A new terminal is a new arm here and nothing else.

use super::types::{Capabilities, Host, ImageSupport, NO, NativeImages, TerminalKind, YES};
use super::version::Version;

/// A hosting layer's asserted answer wins over the terminal's; anything else
/// leaves the terminal's answer in place.
fn host_preferred(host: Option<bool>, terminal: Option<bool>) -> Option<bool> {
    if host == Some(true) {
        Some(true)
    } else {
        terminal
    }
}

/// The capability table for the terminal identified by `name` at `version`,
/// from the table alone.
///
/// This is the raw row, with no correction from anything the runtime observed:
/// a caller that wants the answer for *this* launch (the negotiated image
/// protocol, the hosting layer's own capabilities) uses
/// [`TerminalProfile::capabilities`](super::TerminalProfile::capabilities). The
/// advice tables use this one, because a recommendation has to rest on what the
/// terminal's own documentation settles rather than on what one session
/// negotiated.
pub(crate) fn table_capabilities(name: TerminalKind, version: Option<Version>) -> Capabilities {
    match name {
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
        // Kitty: Kitty graphics throughout, OSC 9;4 from 0.47. The floor is a
        // statement of when the support arrived, and it is unreachable at
        // runtime: `KITTY_WINDOW_ID` carries no version, so the row resolves to
        // `None` there. That is the intended outcome — the 9;4 gate emits on an
        // unasserted row (an unsupported sequence is inert), so kitty gets its
        // progress bar without the version ever being known.
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
        // arrived together in 0.13. A `TERM=alacritty*` marker carries no
        // version, so these three floors yield `None` at runtime and the
        // rows stay unasserted. No graphics, no progress, no OSC 22.
        TerminalKind::Alacritty => Capabilities {
            kitty_keyboard: Capabilities::at_least(version, Version::new(0, 13, 0)),
            synchronized_output: Capabilities::at_least(version, Version::new(0, 13, 0)),
            clipboard_write: Capabilities::at_least(version, Version::new(0, 13, 0)),
            progress: NO,
            program_status: NO,
            pointer_shape: NO,
            images: NativeImages::None,
        },
        // Foot: the keyboard protocol from 1.10.3, plus sixel. A
        // `TERM=foot*` marker carries no version, so the floor yields `None`
        // at runtime and keyboard support stays unasserted. No OSC 9;4.
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
        // protocol. OSC 9;4 arrived in 3.6.6 (advertised through the `P`
        // feature of its `TERM_FEATURES` report), and `TERM_PROGRAM_VERSION`
        // carries the version, so the row is version-gated rather than asserted
        // outright.
        TerminalKind::ItTerm => Capabilities {
            kitty_keyboard: YES,
            synchronized_output: YES,
            clipboard_write: YES,
            progress: Capabilities::at_least(version, Version::new(3, 6, 6)),
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
        // Ptyxis: OSC 9;4 from 48.0, and `PTYXIS_VERSION` carries the version.
        // Nothing else about its build is asserted — its VTE base is not
        // evidence for the features the build actually implements.
        TerminalKind::Ptyxis => Capabilities {
            progress: Capabilities::at_least(version, Version::new(48, 0, 0)),
            ..Capabilities::UNASSERTED
        },
        TerminalKind::Unknown => Capabilities::UNASSERTED,
    }
}

/// The terminal's capability row, corrected by the picker's negotiated protocol
/// (`images`) and by anything the hosting layer `host` provides itself.
pub(crate) fn capabilities_of(
    name: TerminalKind,
    version: Option<Version>,
    images: ImageSupport,
    host: Host,
) -> Capabilities {
    let mut caps = table_capabilities(name, version);
    // The picker negotiated the image protocol with this terminal, and a
    // negotiated protocol is a measurement, so it outranks the table. It is
    // also the only capability a hosted pane can report at all: the
    // workspace manager's emulator answers the query while no terminal
    // identity reaches us.
    match images {
        ImageSupport::Kitty => caps.images = NativeImages::Kitty,
        ImageSupport::Sixel => caps.images = NativeImages::Sixel,
        ImageSupport::Iterm2 => caps.images = NativeImages::Iterm2,
        // No native protocol negotiated, which several terminals answer
        // whether or not they can draw images (a console's ConPTY drops the
        // query), so the table's own answer stands.
        ImageSupport::Halfblocks => {}
    }
    // A hosting layer answers for the features it provides itself, which
    // outranks a table answer about a terminal whose identity never reached
    // us. A host that asserts nothing leaves the terminal's answer alone.
    let host = host.capabilities();
    caps.kitty_keyboard = host_preferred(host.kitty_keyboard, caps.kitty_keyboard);
    caps.synchronized_output = host_preferred(host.synchronized_output, caps.synchronized_output);
    caps.clipboard_write = host_preferred(host.clipboard_write, caps.clipboard_write);
    caps.progress = host_preferred(host.progress, caps.progress);
    caps.program_status = host_preferred(host.program_status, caps.program_status);
    caps.pointer_shape = host_preferred(host.pointer_shape, caps.pointer_shape);
    if !matches!(host.images, NativeImages::Unknown) {
        caps.images = host.images;
    }
    caps
}
