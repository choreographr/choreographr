# Plan: terminal-protocol follow-ons (replies, images, paste, VT)

**Status:** pending. Phase 0 (§2, owning the terminal reply stream) is the gate for
Phases 1-3; Phases 4-6 are independent of it and of each other.
**Lifecycle:** a pending plan, deleted once implemented. The *current* contract for
what already shipped lives in the in-source rustdoc of `choreo-tui/src/terminal/`,
`choreo-tui/src/backend.rs` and the terminal passages of `ARCHITECTURE.md`.
**Date:** 2026-10-30
**Targets:** `choreo-tui` (Phases 0-3, 5); `choreo-proto`, `choreo-daemon`,
`choreo-ai-protocols` (Phase 4); `choreo-tui` + `choreo-image` (Phase 6).
**Origin:** a review of Ghostty 1.4.0 (released 2026-10-27) for program-facing
features `choreo-tui` could adopt.

> **TL;DR.** The output side is done: the client already emits OSC 9;4 progress,
> OSC 7501 program status (now with `kind`/`progress` and a `login` auth record),
> OSC 52 + OSC 5522 clipboard writes, OSC 2 title, OSC 22 pointer shape, the Kitty
> keyboard protocol, synchronized output (mode 2026) and Kitty graphics. What is
> left splits into two kinds of work. **Phases 0-3** all need one missing
> capability — *reading the terminal's replies* — which crossterm's input parser
> currently swallows; Phase 0 builds that and the rest are small on top of it.
> **Phases 4-6** need no input-path work at all and can land in any order:
> pasted-image prompts (wire + daemon + TUI), Kitty graphics animation and the
> transient image hint, and a real VT model for captured tool output.

---

## Table of contents

1. [What already shipped](#1-what-already-shipped)
2. [Phase 0 — own the reply stream](#2-phase-0--own-the-reply-stream)
3. [Phase 1 — visibility reports](#3-phase-1--visibility-reports)
4. [Phase 2 — clipboard write status](#4-phase-2--clipboard-write-status)
5. [Phase 3 — OSC 7501 feature detection](#5-phase-3--osc-7501-feature-detection)
6. [Phase 4 — pasted images](#6-phase-4--pasted-images)
7. [Phase 5 — Kitty graphics animation and lifecycle hints](#7-phase-5--kitty-graphics-animation-and-lifecycle-hints)
8. [Phase 6 — libghostty-vt for captured tool output](#8-phase-6--libghostty-vt-for-captured-tool-output)
9. [Deferred polish](#9-deferred-polish)
10. [Limits to design against](#10-limits-to-design-against)
11. [Sequencing and verification](#11-sequencing-and-verification)

---

## 1. What already shipped

`choreo-tui`'s terminal layer (`src/terminal/`) is a leaf module that frames and
writes escape sequences; orchestration that needs `App` lives in
`src/state/terminal_status/` and the connection loop. Two rules it already holds
and every phase below must preserve:

- **The root OSC 7501 record is never written.** OSC 9;4 maps onto it, and a 7501
  report replaces its record wholesale, so a progress update would wipe a status
  report. Every report is a child record (`id=<session>`, or `id=login`).
- **Only output is written.** No phase in this file may write a query until
  Phase 0 lands — see the hazard in §2.1.

| Sequence | Where | Notes |
|---|---|---|
| OSC 9;4 progress | `terminal/progress.rs` | root record; capability probe is env-based (`anstyle-progress`) |
| OSC 7501 status | `terminal/status.rs` | child records; `state`/`app`/`id`/`kind`/`progress`/`title`/`msg` |
| OSC 52 + OSC 5522 clipboard | `terminal/clipboard.rs` | plain + `<pre>` HTML, 4096-byte wdata chunks, 1 MiB cap |
| OSC 2 title, OSC 22 pointer | `terminal/title.rs`, `terminal/pointer.rs` | both deduped on `App` |
| Mode 2026 | `backend.rs` | markers bracket each frame's cell stream |
| Kitty keyboard, bracketed paste, mouse | `connection/` | via crossterm |

---

## 2. Phase 0 — own the reply stream

**This is the blocking piece.** Every deferred reply-consuming feature depends on
it: visibility reports (Phase 1), clipboard write status (Phase 2), and the OSC
7501 `?` feature-detection reply (Phase 3).

### 2.1 Why it is needed, and the hazard it removes

crossterm's unix parser (`crossterm::event::sys::unix::parse`) handles a byte
stream with no notion of "this is a reply, not input":

- `CSI ? 999 ; 1 n` (a visibility report), `CSI ? 2033 ; 1 $ y` (a DECRPM reply)
  and every other `CSI ? …` whose final byte it does not recognise are parsed to
  `Ok(None)` and **silently dropped**. There is no API to see them.
- `ESC ] …` is worse: `]` is not a sequence introducer crossterm knows, so the
  parser produces `Alt+]` followed by **each remaining byte as an individual key
  event**. An unconsumed OSC reply is therefore *typed into the user's prompt*.

That second behaviour is why today's terminal layer is strictly output-only, and
why the OSC 5522 implementation deliberately skips the `status=DONE` reply rather
than ignore it. Any phase that adds a query before Phase 0 is a data-loss bug.

### 2.2 The mechanism

Interpose a filtering reader between the tty and crossterm, and pass everything
that is not a recognised reply through byte-exactly.

Constraints discovered while designing this (each one invalidates an obvious
simpler approach):

1. **crossterm resolves its input fd exactly once** — `tty_fd()` returns fd 0 when
   `isatty(0)`, else opens `/dev/tty` — and its event source then registers that
   fd with epoll. Replacing fd 0 *after* that registration leaves the epoll entry
   pointing at the previous open file description, so readiness and `read(0)`
   disagree. **No `dup2` after crossterm is initialised.**
2. **A pipe fails `isatty`**, so `dup2(pipe, 0)` before crossterm initialises
   makes crossterm open `/dev/tty` instead — the filter is bypassed *and* two
   readers race for the tty, losing keystrokes.
3. Therefore the replacement must be a **pty slave**: `openpty()`, raw-mode the
   slave, `dup2(slave, 0)` before anything touches crossterm. `isatty(0)` is then
   true (crossterm uses fd 0, epoll stays consistent), and the reader thread
   writes the filtered bytes into the master.
4. **The proxy now owns the real terminal's termios.** crossterm's
   `enable_raw_mode`/`disable_raw_mode` act on fd 0, which is the pty slave — so
   the proxy must put the *real* tty into raw mode itself (saving the original
   termios) and restore it on drop, and expose explicit suspend/resume hooks,
   because `connection/resume.rs`'s raw-mode toggle no longer reaches the real
   tty across SIGTSTP/SIGCONT.
5. Window size is unaffected: crossterm and the client both read `/dev/tty`.
6. `io::stdin()` still means fd 0, so the client's existing mio watch and its
   "stdin read-closed → the terminal went away" path keep working: the pipe closes
   when the reader thread exits.

### 2.3 Shape of the change

New `choreo-tui/src/terminal/input.rs` (unix-only; on Windows `install()` returns
`None` and every reply-dependent feature stays off — there is no console byte
stream to filter):

```rust
pub(crate) enum Reply {
    Visibility { visible: bool },              // CSI ? 999 ; Ps n
    ModeReport { mode: u16, status: ModeStatus }, // CSI ? <mode> ; Ps $ y
    ProgramStatusSupport { states: Vec<String>, kinds: Vec<String> }, // OSC 7501 ; ? …
    Clipboard(ClipboardStatus),                // OSC 5522 … status=DONE|EPERM|EFBIG|…
}

/// Consume complete reply sequences, return everything else byte-exactly.
pub(crate) struct ReplyScanner { /* partial-sequence buffer + bracketed-paste state */ }

pub(crate) struct InputProxy { /* owned real-tty fd, termios, thread, shutdown pipe */ }
impl InputProxy {
    pub(crate) fn install(replies: Sender<Reply>) -> Option<Self>;
    pub(crate) fn suspend(&self);   // restore real-tty termios (SIGTSTP)
    pub(crate) fn resume(&self);    // re-apply raw mode (SIGCONT)
}
```

Rules the scanner must obey:

- Pass everything verbatim except the four reply families above. Partial
  sequences spanning reads are buffered, capped, and abandoned (passed through)
  rather than growing unbounded.
- **Inert inside a bracketed paste**: between `ESC[200~` and `ESC[201~` nothing is
  consumed, so a pasted clipboard containing an OSC-looking string is never
  swallowed.
- Replies are delivered on a `crossbeam_channel` (workspace channel convention);
  the UI loop gains one `select!` arm. No protocol payloads cross it — the enums
  above are the whole vocabulary.

### 2.4 The self-check (mandatory)

Ordering mistakes here are silent and catastrophic (no keystrokes at all), so the
install path verifies itself instead of trusting the ordering:

1. Construct the pty pair and `dup2` the slave onto fd 0.
2. Write a sentinel keystroke (`ESC [ 24 ~`, which crossterm parses as `F(12)`)
   into the pty master, then `event::poll`/`event::read` for it.
3. On the expected event: keep the proxy. On anything else or a timeout: roll
   back (restore fd 0 to the real tty, restore termios, drop the thread so the
   leftover sentinel dies with the pty) and return `None` — reply-dependent
   features disable themselves.

A build that fails the self-check must degrade to today's behaviour, never to a
TUI that cannot read keys.

### 2.5 Tests

`ReplyScanner` is a pure function over bytes: unit-test every family, sequences
split across arbitrary read boundaries, a malformed sequence passed through, a
paste containing a forged reply, and long garbage that must not grow the buffer.
The pty plumbing is exercised by an integration test in `choreo-tui/tests/it/`
(a child process on an `openpty` pair: install, feed bytes, assert the filtered
stream and the extracted replies) — `#[ignore]`d, per the workspace rule.

**Verification needs a human at a real terminal**: key input, `Ctrl+Z`/`fg`,
resize, and a program that writes a colour query. There is no automated substitute
for those four.

---

## 3. Phase 1 — visibility reports

Depends on Phase 0.

Ghostty 1.4 implements the visibility-report specification
(https://rockorager.dev/misc/visibility-reports/):

- enable with `CSI ? 2033 h` (the terminal reports the current state immediately
  and on every change), disable with `CSI ? 2033 l`, one-shot query `CSI ? 998 n`;
- the report is `CSI ? 999 ; Ps n` with `Ps` `1` = potentially visible, `2` = not
  visible; unknown values must be treated as visible;
- detection is `CSI ? 2033 $ p` → `CSI ? 2033 ; Ps $ y`, where `Ps` of `0` or `4`
  means unsupported (supported = 1, 2 or 3).

Work:

1. Probe support during the startup window (reuse the probe from Phase 3), then
   enable the mode right after the alternate screen is entered, and disable it on
   exit and suspend.
2. `App` gains `term_visible: bool` (default `true` — the conservative value). The
   UI loop's `Reply::Visibility` arm sets it and marks the frame dirty on a
   hidden → visible transition.
3. Skip `terminal.draw` while hidden. Keep processing events, keep updating state,
   and keep emitting OSC 7501/9;4/title — a hidden tab's indicators are the point.
   No redraw is lost: no other writer touches the screen, and ratatui's
   auto-resize forces a full repaint if the size changed while hidden.
4. `tracing` on the transition; a counter of skipped frames at `trace` level for
   observability.

Value: today the client redraws for every event from every streaming session, even
when the view is a background tab or another workspace. This is also the
prerequisite for any future animation (Phase 5), which must pause when hidden.

---

## 4. Phase 2 — clipboard write status

Depends on Phase 0.

An OSC 5522 write is answered (`type=write:status=DONE`, or `EPERM` / `EFBIG` /
`EIO` / `EINVAL` / `ENOSYS` / `EBUSY`). Reading it turns a fire-and-forget copy into
a confirmed one: "Selection copied to clipboard." becomes a statement of fact, and
a denying terminal surfaces as an explicit message instead of a silent no-op.

Work: map `Reply::Clipboard` onto the status line; when the write fails, say which
failure it was in words a user can act on (permission denied vs too large). Until
this lands, the copy path must **not** claim success — today's message is
optimistic; the post-Phase-2 wording should distinguish "sent" from "confirmed".

---

## 5. Phase 3 — OSC 7501 feature detection

Depends on Phase 0.

The program-status spec defines `OSC 7501 ; ? ST` → `OSC 7501 ; ?:states=…:kinds=…ST`.
A reply without `states` means exactly `idle,working,done,blocked,error`; those
lists are fixed, so a future state is never silently reinterpreted.

Work: probe once at startup, keep the advertised `states`/`kinds`, and suppress a
report whose `state` (or `blocked` + `kind`) the terminal does not list. Today the
client emits unconditionally, which the spec permits ("detection is optional …
terminals MUST ignore unknown OSCs") — so this is correctness insurance for future
states, not a fix.

Note for whoever lands Phase 0 first: this is the *only* place a probe is worth
paying for. Prefer one bundled startup probe that answers all three questions
(modes 2033 and 5522 `DECRQM`, plus this `?`) and ends on a `CSI 5 n` fence — the
same shape `ratatui-image`'s `Picker::from_query_stdio` already uses, including a
watchdog thread so a mute terminal costs a bounded wait, not a hang. Order it
before `build_picker()` so the two drains of stdin do not interleave.

---

## 6. Phase 4 — pasted images

Independent of Phase 0. **The largest item here**: TUI + wire + daemon.

The Kitty clipboard protocol's *paste events* (`CSI ? 5522 h`, kitty ≥ 0.44.1,
Ghostty 1.4) tell the program which MIME types a user's paste carries, with a
one-time `pw` so the read needs no permission prompt; `type=read` then fetches the
chosen type. Today a paste is text-only (`crossterm::event::Event::Paste`), and
`Turn.user_text: Option<String>` (`choreo-proto`) has no attachment slot at all —
vision images exist solely as tool output (`ToolResultRecord::image`).

Work, in dependency order:

1. **Phase 0** first (a `type=read` is a query; without the reply stream its
   answer would be typed into the composer).
2. `choreo-proto`: a user attachment on `RunInput` and a `user_images:
   Vec<ImageReference>` (or a generalised slot) on `Turn`, following the existing
   tool-result precedent exactly — bytes daemon/model-only, emptied on the
   client-facing view by `turn_for_client`, re-attachable on read from the
   `session_attachments` table under a new slot prefix.
3. `choreo-daemon`: normalize the pasted bytes through `choreo-image` (its
   existing pixel-budget and dimension guards are the security boundary for
   untrusted clipboard content), persist, and let the request builder attach them.
4. `choreo-ai-protocols`: project a user-turn image as the same vision part the
   tool-result path already emits (`openai/message.rs`); providers that do not
   accept user images keep text-only behaviour.
5. `choreo-tui`: enable mode 5522, show the pending attachment in the composer,
   fetch on paste, and refuse over-size payloads with an explicit status line.
   Keep the paste path inert when the mode is not granted.

Security notes: a pasted image is untrusted input from another program's clipboard,
so it takes the same decode guards as a tool image; the one-time `pw` must not be
reused outside the paste it arrived with; and nothing from the reply may be logged.

---

## 7. Phase 5 — Kitty graphics animation and lifecycle hints

Independent of Phase 0 (output-only). Moderate size, and partly blocked by the
image crate the client uses.

Ghostty 1.4 completes the Kitty graphics protocol: animation (frame transmission,
composition, playback control), relative placements (`P=`, `Q=`, `H=`, `V=`), and a
transient usage hint (`U=1`, evicted first when image storage is full). It also
made the per-frame cost of **unicode-placeholder** images much lower — the exact
mode `ratatui-image` uses (it transmits once, then renders cells containing
U+10EEEE placeholders).

Work:

1. **Animation** cannot go through `ratatui-image` (11.x has no frame support), so
   it means driving the protocol directly: send `a=f` frames / `a=a` playback
   control, keep a per-image frame cache in the image worker, and tick playback
   from a `crossbeam_channel::after(..)` arm in the UI loop's `select!` — the loop
   deliberately has no poll timer today, and a `sleep`-poll would violate the
   workspace's event-driven rule. Pause while hidden (Phase 1) and while the image
   is scrolled out of view.
2. **Transient hint** for images that belong to scrollback rather than the visible
   transcript: right now an image-heavy session can hit the terminal's
   `image-storage-limit`, and without the hint the terminal evicts whatever it
   likes.
3. **Relative placements** are only worth it if images ever need to move with a
   parent (nested/multi-part figures); skip until a use case appears.
4. Decoding multi-frame sources: `choreo-image` decodes a single still
   (`decode_raster_oriented`); animated GIF/WebP needs a frame iterator plus a
   per-frame budget, under the same decompression-bomb guard.

Prerequisite to decide first: whether animated images are worth the direct-protocol
dependency at all, given they only render on Kitty-graphics terminals. Half-block
and Sixel fallbacks would still show a still frame.

---

## 8. Phase 6 — libghostty-vt for captured tool output

Independent of Phase 0. Largest architectural option here; deliberately last.

The client renders tool output with `ansi-to-tui`, so only SGR survives: every
other control byte is escaped by `text_filter.rs` (by design — it keeps the
measure == draw invariant). A `cargo`, `npm` or `docker build` transcript full of
`\r`, cursor movement and spinners therefore renders as a litter of escaped bytes
instead of its final screen state.

libghostty-vt (shipped as a C API in the 1.4 cycle, with CMake/pkg-config, Nix,
freestanding builds, wasm and an xcframework, targeting Linux/macOS/Windows/
FreeBSD/Android/iOS) is a complete terminal-emulator core and would give the client
a correct model of that output.

Work: bound the change first — feed a tool result's captured bytes through a VT
model and render the resulting screen, behind a per-tool-result decision (a text
transcript is better than a 200×50 screen dump for most results). Then packaging:
a Zig/C dependency in the TUI is a new build-system surface for every release
target, including the static musl tarball and the Android/iOS GUI builds (which do
not use the TUI, so the dependency must stay out of their graph).

Preferable short-term alternative if the interaction matters but the dependency
does not: keep SGR-only rendering and give `sh`-tool output a `\r`-collapse pass,
which fixes the common progress-bar case without a VT model.

---

## 9. Deferred polish

**Overline and styled underlines.** Ghostty 1.4 advertises `Smol`/`Rmol` in
terminfo and reports overline plus curly/dotted/dashed underline in its `DECRQSS`
reply. ratatui's cell model carries only `Modifier::UNDERLINED`, so using them
means emitting SGR outside the cell buffer — which breaks the invariant that what
the client measures is what the terminal draws. Not worth it until a concrete
styling need (a heading style, a diagnostic squiggle) justifies a ratatui-level
change.

**`CSI 8 t` window resize** (`vt-window-resize-allowed`, opt-in in Ghostty and
ignored in splits/tabs/fullscreen): no use case — the client already reflows to
whatever size it is given.

**OSC 99 desktop notifications**: Ghostty 1.4 only *parses* them. If turn-finished
notifications are ever added, use OSC 9 / OSC 777.

**`GHOSTTY_SURFACE_ID`**: could key per-surface client state (remember which
session was attached in *this* terminal) — a nice-to-have, not a protocol gap.

---

## 10. Limits to design against

| Limit | Value | Consequence |
|---|---|---|
| Ghostty OSC payload | 8 MiB per sequence | one OSC 52 write; OSC 5522 chunks stay well under |
| Ghostty Kitty graphics payload | 65 MiB | caps a single animation frame batch |
| OSC 5522 write | ≥64 MiB accepted, 4096-byte packets | cap is a UI-latency choice, not a terminal limit |
| OSC 7501 | whole sequence ≤4096 B; `msg` ≤2048, `title` ≤192 decoded; ≤256 records | already enforced in `terminal/status.rs` |
| Client clipboard cap | 1 MiB text | UI-loop latency guard; revisit with Phase 2's status |

---

## 11. Sequencing and verification

1. **Phase 0** — alone, with its integration test and a hands-on check of key
   input, suspend/resume and resize. Nothing else may start before it lands.
2. **Phases 1-3** — small, independent, each on top of Phase 0's channel.
3. **Phase 4** — wire + daemon first, TUI last.
4. **Phases 5 and 6** — each starts with a "is this worth it" decision, recorded in
   this file before code.

Every phase follows the workspace gate (`just pre-commit`), commits as one
release-note-quality conventional commit, and updates the terminal passages of
`ARCHITECTURE.md` and the terminal bullet in `README.md` in the same change.
