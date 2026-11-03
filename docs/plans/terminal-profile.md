# Plan: launch-time terminal and host profile (`choreo-tui`)

**Status:** research record for the shipped terminal profile; the reference it
backs lives in the rustdoc of `choreo-tui/src/terminal/profile.rs` and in
`ARCHITECTURE.md`'s "Terminal integration" section. This file keeps the
per-terminal evidence — with the source each row came from — and the open
questions the later phases (welcome notice, Terminal page, progress/status
gates) still have to settle.
**Date:** 2026-10-30
**Targets:** `choreo-tui` (the profile, and the phases that consume it).
**Origin:** deciding which escape-sequence families the client may emit needs an
evidence base, not a guess. Every fact below was taken from the project's own
source tree or its own documentation on the date above, and each row names which
project it came from.

> **TL;DR.** Twelve terminals plus a "know nothing" case are identified from a
> handful of environment markers; three multiplexers and two hosting workspace
> managers are identified as the layer nearest the program; and a three-state
> capability matrix (supported / not supported / unknown) covers the seven
> escape-sequence families the client uses. Nothing is emitted differently yet —
> the profile is logged once at launch and stored on `App` for the phases that
> follow.

---

## 1. Why an explicit profile at all

The client already writes OSC 9;4 progress, OSC 7501 program status, OSC 52
clipboard, OSC 2 title, OSC 22 pointer shape, mode 2026 synchronized output and
the Kitty keyboard protocol (see `docs/plans/terminal-protocols.md` for the
sequence-level inventory). Every one of those is a *write*: a terminal that does
not understand it either ignores it or, worse, renders the sequence's payload as
text. So "is this supported?" is a question worth answering once, and answering
it wrongly in the negative direction costs a feature the user could have had.

That is why the table is **three-state**, not boolean. `Some(false)` is reserved
for a terminal whose own source or documentation settles the question; `None`
means "not asserted" and is the answer for every terminal the client cannot
identify. A later phase must treat `None` as "may work" and let the sequence
through, because a false negative silently disables a feature.

## 2. Identifiers

All values are ordinary environment variables. Nothing is probed over the wire
in this phase, so the profile costs no round-trip and cannot hang on a mute
terminal.

### 2.1 Terminal markers (evaluated in this order, first match wins)

| Marker | Terminal | Notes |
|---|---|---|
| `WT_SESSION` | Windows Terminal | A per-pane GUID; unambiguous. |
| `TERMUX_VERSION` | Termux | Termux's own package version; also selects the Android platform. |
| `KITTY_WINDOW_ID` | kitty | Per-OS-window id. kitty exports no version variable, so the version stays unknown. |
| `TERM_PROGRAM` = `ghostty` | Ghostty | Version from `TERM_PROGRAM_VERSION`. |
| `TERM_PROGRAM` = `iTerm.app` | iTerm2 | Version from `TERM_PROGRAM_VERSION`. |
| `TERM_PROGRAM` = `WezTerm` | WezTerm | Version from `TERM_PROGRAM_VERSION`. |
| `TERM_PROGRAM` = `Apple_Terminal` | Apple Terminal | Version from `TERM_PROGRAM_VERSION`. |
| `TERM_PROGRAM` = anything else | unknown | A named host application we have no row for (an editor's embedded terminal, an Electron shell, mintty, rio, Tabby, Hyper). Its presence ends identification: the toolkit variables below describe the engine underneath it, not the program the user sees. |
| `KONSOLE_VERSION` | Konsole | Packed decimal (`260400` = 26.04). |
| `PTYXIS_VERSION` | Ptyxis | Dotted. |
| `VTE_VERSION` | a bare VTE terminal | Packed decimal (`7400` = 0.74). |
| `TERM` prefix `foot` | foot | |
| `TERM` prefix `alacritty` | Alacritty | |
| `TERM` prefix `xterm-ghostty` | Ghostty | Version unknown on this path. |
| `TERM` = `xterm-rex` | (host) | Treated as the Rex host layer, not as a terminal marker; see §2.2. |

`TERM` values that only name a capability tier — `xterm-256color`,
`screen-256color`, `linux` — identify nothing and are deliberately not matched.

### 2.2 Host layers (hosted outranks a multiplexer; every match is recorded)

| Marker | Layer | Notes |
|---|---|---|
| `HERDR_ENV` (`1`), `HERDR_PANE_ID`, `HERDR_BIN_PATH` | hosted: Herdr | Markers a hosting workspace manager sets for its children. |
| `REX_SESSION`, `REX_BLOCK`, `REX_SERVER` | hosted: Rex | |
| `TERM` = `xterm-rex` | hosted: Rex | Rex ships its own terminfo entry under that name, so its panes export the TERM even when no `REX_*` variable survives. |
| `TMUX` | mux: tmux | Version only from `TERM_PROGRAM=tmux` + `TERM_PROGRAM_VERSION` (tmux ≥ 3.2 exports both into a pane). |
| `ZELLIJ` | mux: zellij | |
| `STY` | mux: screen | |

**Identity is suppressed under a layer.** A multiplexer rewrites `TERM` and
passes the outer program's markers straight through, so reading identity there
names the terminal behind the layer. The one exception is Rex: the terminal in a
Rex pane *is* Rex's own, described by Rex's own terminfo entry.

## 3. Capability matrix

Seven families, abbreviated below: **kb** = Kitty keyboard protocol, **2026** =
synchronized output, **52** = OSC 52 clipboard *write*, **9;4** = progress
reporting, **7501** = program status, **22** = pointer shape, **img** = native
image protocol.

| Terminal | kb | 2026 | 52 | 9;4 | 7501 | 22 | img | Source of the row |
|---|---|---|---|---|---|---|---|---|
| Ghostty | ✔ | ✔ | ✔ | ✔ (≥ 1.2) | ✔ | ✔ | Kitty graphics | Ghostty's own source tree and changelog. It is the only terminal in this set that implements the Program Status Protocol. |
| kitty | ✔ | ✔ | ✔ | ✔ (≥ 0.47) | ✘ | ✔ | Kitty graphics (≥ 0.28) | kitty's own documentation and source tree. |
| WezTerm | ✔ (needs `enable_kitty_keyboard = true`) | ✔ | ✔ (write) | ✔ | ✘ | ✘ | iTerm2 inline images | WezTerm's own documentation and source tree. |
| Alacritty | ✔ (≥ 0.13) | ✔ (≥ 0.13) | ✔ (≥ 0.13) | ✘ | ✘ | ✘ | none | Alacritty's own changelog and source tree. |
| foot | ✔ (≥ 1.10.3) | ✔ | ✔ | ✘ | ✘ | ✔ | sixel | foot's own documentation and source tree. |
| Konsole | ✔ | ✔ | ✔ | ✔ (≥ 26.04) | ✘ | ✘ | half-blocks (sixel exists but is buggy) | Konsole's own changelog and source tree. |
| VTE (bare) | ✘ | ✘ (handler is an unimplemented TODO) | ✘ | ✔ | ✘ | ✘ | none | VTE's own source tree. |
| iTerm2 | ✔ | ✔ | ✔ | ✔ | ✘ | ✘ | iTerm2 inline images | iTerm2's own documentation and source tree. |
| Apple Terminal | ✘ | ✘ | ✘ | ✘ | ✘ | ✘ | none | Apple's documentation and Terminal's observed behaviour. |
| Windows Terminal | ✔ | ✔ | ✔ | ✔ | ✘ | ✘ | not asserted — ConPTY does not answer the graphics capability query | Windows Terminal's own documentation and source tree. |
| Termux | ✘ | ✘ | ✔ | ✘ | ✘ | ✘ | sixel | Termux's own packages/documentation. |
| Rex | not asserted | not asserted | not asserted | not asserted | ✔ | not asserted | not asserted | Rex's own implementation of the Program Status Protocol. Everything else about its terminal is unverified, so nothing else is claimed. |
| unknown | — | — | — | — | — | — | — | Nothing asserted, by construction. |

Two rows are deliberately weaker than they could look. **Windows Terminal's
images** is `unknown` rather than `none`: the terminal can draw sixel, but the
ConPTY layer does not answer `ratatui_image`'s capability query, so the picker
falls back to half-blocks and the profile reports "not asserted". **Konsole's
images** is `halfblocks` because its sixel support renders incorrectly; offering
it would produce visibly broken output, which is worse than the fallback.

## 4. macOS Option-as-Alt

A large share of macOS users run a terminal where the Option key composes a
character (`Option+e` → `´`) instead of sending Alt, which silently changes what
every `Alt+` chord in the client does. Every terminal makes this a *setting*,
and every setting can be overridden by the user, so this cannot be detected — it
can only be advised.

| Terminal | Setting |
|---|---|
| Apple Terminal | Profiles → Keyboard → "Use Option as Meta key" |
| iTerm2 | Profiles → Keys → Left/Right Option key → "Esc+" |
| kitty | `macos_option_as_alt yes` (`no` / `left` / `right` / `yes`) |
| WezTerm | `send_composed_key_when_left_alt_is_pressed` / `..._right_alt_is_pressed` |
| Alacritty | `option_as_alt = "Both"` (`None` / `OnlyLeft` / `OnlyRight` / `Both`) |
| Ghostty | `macos-option-as-alt = true` (also accepts `left` / `right`) |
| foot, Konsole, Ptyxis, VTE terminals, Windows Terminal, Termux | not applicable / not verified |

**The default value of each setting is deliberately not recorded here.** It
differs across releases, is overridden by most users' existing dotfiles, and is
invisible to us either way — recording a default would invite a warning message
that is wrong for the user in front of us. The durable fact is the *setting
name*, which is what any future advice has to name.

## 5. Multiplexers

A multiplexer is between the program and the terminal, so it decides what
reaches the outer terminal:

- **`extended-keys`** (tmux ≥ 3.2, `off` / `on` / `always`) — matters for the
  Kitty keyboard protocol: with the default, tmux re-encodes keys itself and the
  outer terminal's protocol never reaches the program. `always` forwards escape
  sequences unmodified.
- **`allow-passthrough`** (`off` / `on` / `all`) — whether a pane's program may
  send escape sequences straight through to the outer terminal. This is what
  graphics protocols and other OSC traffic need in order to arrive at all.
- **`set-clipboard`** (`on` / `external` / `off`) — whether an OSC 52 from a pane
  is applied by tmux, forwarded outward, or dropped.
- **DECSET 2026 (synchronized output)** — tmux understands it from **3.7**; below
  that, a frame's framing markers reach a terminal tmux does not coordinate with.
- **zellij** — identified by `ZELLIJ`. Its passthrough behaviour is **not
  verified** for any of the families above, so the profile reports zellij as the
  host and asserts nothing about capabilities beyond the terminal it hides.

The consequence for the client: under a mux the profile reports `Unknown`
capabilities (it saw a mux as the host and a hidden terminal), and a later phase
that wants to gate on a capability has to say so in its own wording rather than
telling the user their terminal cannot do it.

## 6. Hosting workspace managers

These are not session wrappers: they host the program in a pane they own, expose
their own reporting API, and set markers for their children.

- **Herdr** sets `HERDR_ENV` (value `1`), `HERDR_PANE_ID`, and `HERDR_BIN_PATH`.
  Its own UI consumes agent status **only through its own pane-reporting API** —
  it does not read OSC 7501 from the programs in its panes — so a client under
  Herdr gains nothing from emitting program status. It is a host we report, not
  a host we report *to*.
- **Rex** sets `REX_SESSION`, `REX_BLOCK`, and `REX_SERVER`, ships its own
  terminfo entry (`xterm-rex`), and implements OSC 7501. Rex and Herdr's newer
  core both speak the Program Status Protocol, which is why `TerminalKind::Rex`
  asserts exactly that one capability and nothing else.

Reporting *to* any of these is explicitly out of scope for this phase: the client
detects them, logs them, and otherwise behaves as it does under a plain terminal.

## 7. Open questions

1. **Kitty's version is unknown on every path** (kitty exports no version
   variable), so the three version-gated kitty rows resolve to `None`. A reply
   from the terminal is the only way to settle it, which is Phase 0 of
   `terminal-protocols.md`.
2. **Windows Terminal images** are `unknown` because ConPTY swallows the query.
   The picker's own fallback is the whole answer until a console-aware probe
   exists.
3. **zellij passthrough** for graphics/OSC families is unverified (§5).
4. **Ptyxis** is identified but none of its capabilities are asserted — its VTE
   base is not evidence for what its build actually implements.
5. **The macOS Option default** per terminal is unrecorded by design (§4); if a
   later phase wants to warn, it should name the setting and let the user
   check.
