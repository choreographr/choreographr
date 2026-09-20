# Plan: Auto-update system

**Status:** proposed — design only (nothing implemented; no source changes in
this change).
**Date:** 2026-09-20
**Touches (when implemented):** new `choreo-update` leaf crate or a
`choreo-shared` module; `choreo-daemon` (`cli.rs`, `config.rs`, a maintenance
thread alongside `catalog.rs`, a new `ClientMessage`/`DaemonMessage` pair);
`choreo-proto` (`frame.rs` — only if a wire message is added);
`choreo-tui` (status banner + `/update`); `scripts/install.sh`
(`--update`/`--check`); new `scripts/manifest.sh` (+ signing);
`.github/workflows/release.yml`; `packaging/README.md`, `README.md`,
`ARCHITECTURE.md`, `RELEASE.md`.

> **TL;DR.** Ship a **signed-manifest, notify-first, opt-in self-updater**.
> The daemon runs a background *check* (mirroring the existing models.dev
> catalog revalidation thread — 25 h cooldown, DB-persisted anchor, etag) and
> broadcasts "update available" to clients; it **never** auto-applies. Applying
> is an explicit action (`choreographr update`, TUI `/update`), performed by
> the **daemon only for install channels the daemon owns** (the curl
> installer / tarball / Termux), and only after verifying a **minisign-signed
> manifest** whose public key is compiled into the binary. Package-managed
> installs (Homebrew, `.deb`/`.rpm`, AUR, `cargo`) are **not** self-replaced —
> the updater prints the right package-manager command instead. Binary swaps
> are atomic (`rename`), both shipped binaries update together, and a running
> daemon is never killed mid-session: the user restarts the service.

---

## Table of contents

1. [Motivation & goals](#1-motivation--goals)
2. [What exists today (the update landscape)](#2-what-exists-today-the-update-landscape)
3. [Design principles](#3-design-principles)
4. [Trust model: a signed manifest](#4-trust-model-a-signed-manifest)
5. [Channel detection & per-channel strategy](#5-channel-detection--per-channel-strategy)
6. [The update state machine](#6-the-update-state-machine)
7. [Atomic swap, rollback, and restart](#7-atomic-swap-rollback-and-restart)
8. [Versioning & compatibility](#8-versioning--compatibility)
9. [UX surfaces](#9-ux-surfaces)
10. [Scheduling & architecture fit](#10-scheduling--architecture-fit)
11. [Platform specifics](#11-platform-specifics)
12. [Security considerations & threat model](#12-security-considerations--threat-model)
13. [Release & packaging changes](#13-release--packaging-changes)
14. [Phased implementation plan](#14-phased-implementation-plan)
15. [Decisions to confirm](#15-decisions-to-confirm)
16. [Risks & mitigations](#16-risks--mitigations)
17. [Out of scope / future work](#17-out-of-scope--future-work)
18. [Verification / definition of done](#18-verification--definition-of-done)
19. [References](#19-references)
20. [Prior art: how peer agents update](#20-prior-art-how-peer-agents-update)

---

## 1. Motivation & goals

A user asked for an auto-update system. Today a Choreographr install only
changes version when the user reruns an installer, a package manager, or
`cargo install`. This plan defines how the binaries can detect, fetch, verify,
and install new releases themselves.

**Goals**

- **G1 — Detect.** Any installed binary can tell the user a newer release
  exists, cheaply and without surprising traffic.
- **G2 — Apply, safely.** For installs the project owns end-to-end (the curl
  installer / tarball / Termux), update both shipped binaries in place,
  atomically, with rollback.
- **G3 — Delegate, don't fight the package manager.** Homebrew, `.deb`, `.rpm`,
  AUR, and crates.io installs are updated by *their* tool; the updater only
  tells the user which command to run.
- **G4 — Opt-in, never surprising.** Checking is on by default (like the
  existing models.dev refresh); *applying* is always an explicit user action
  unless the user opts into `auto_apply`. No silent daemon kills.
- **G5 — No new trust assumptions.** The current installer pins a version and
  a SHA-256 embedded in `install.sh`; auto-update cannot pin, so it must add a
  **signature** over the version metadata, not merely fetch checksums over TLS.

**Non-goals**

- Updating `choreo-gui` APK/IPA (app-store / `dx` toolchain builds) or the
  embedded in-process daemon on iOS — out of scope here.
- Hot-reloading a running daemon in place (see [§7](#7-atomic-swap-rollback-and-restart)).
- A full TUF repository (see [§4](#4-trust-model-a-signed-manifest) for the
  deliberate simplification).

---

## 2. What exists today (the update landscape)

Choreographr ships **two binaries** — `choreographr` (the daemon) and
`choreo-tui` — across many channels. Each channel has a different owner and a
different correct update mechanism:

| Channel | Where it lands | Who owns updates today |
|---|---|---|
| curl installer (`install.sh`) | `$XDG_BIN_HOME`/`~/.local/bin` (+ systemd user unit / launchd plist) | nothing — rerun the script |
| GitHub release tarball | wherever the user unpacked it | the user, by hand |
| Homebrew (`choreographr/choreographr`) | `/opt/homebrew/bin` (Apple Silicon) | `brew upgrade` |
| `.deb` | `/usr/bin` + `/usr/lib/systemd/user` | `apt`/`dpkg` (root) |
| `.rpm` | `/usr/bin` + systemd unit | `dnf`/`rpm` (root) |
| AUR `choreographr-bin` (deferred) | `/usr/bin` | `paru`/`yay` |
| Termux `.deb` | `$PREFIX/bin` | `pkg upgrade` |
| crates.io (`cargo install` / `cargo binstall`) | `~/.cargo/bin` | `cargo install` again |
| Windows `.zip` (not shipped yet) | user `PATH` | manual |
| Source build (`cargo run`/`build`) | `target/` | `git pull` |

Relevant existing machinery this design reuses:

- **A background maintenance thread with exactly the right shape** already
  exists: `choreo-daemon/src/catalog.rs` runs a detached thread that
  revalidates the models.dev catalog **at most once per 25 h**
  (`REFRESH_ATTEMPT_INTERVAL`), anchored on a **wall-clock attempt timestamp
  persisted in the DB** (`catalog_state`), writes the attempt *before* the
  fetch (crash-safe), uses an **etag conditional GET** (304 → keep), and
  drifts +1 h/day to spread load. An update *check* should be a near-copy of
  this thread.
- **The install script is already environment-overridable** for testing:
  `CHOREOGRAPHR_BASE_URL` points at a mirror/local HTTP server. The updater's
  manifest base should follow the same convention so integration tests can
  serve a fixture.
- **The wire protocol has a strict version gate with no negotiation**
  (`choreo-proto/src/frame.rs`, `PROTOCOL_VERSION = 6`); mixed-version peers
  fail fast with `UnsupportedVersion`. Any update must account for this.
- **`--version` prints the version + release name** via
  `choreo_shared::release_name::version_string(...)`, sourced from
  `[workspace.package] version` and `choreo-shared/release-name.txt`. The
  release name is *metadata* — never an install identifier.
- **A `DaemonConfig` in `~/.config/choreographr/config.toml`** is the natural
  home for an `[updates]` table.
- **The "installed, never auto-enabled" service policy** (`packaging/README.md`)
  is the cultural precedent for `[updates] apply = false` by default.

> **How the peers do it.** A survey of the other agents in `~/agents` (zero,
> jcode, fx, codex, opencode, buzz, deepseek-harness, maka-agent, headlong) is
> in [§20](#20-prior-art-how-peer-agents-update). The short version: CLIs
> self-update from GitHub Releases / a CDN with **SHA-256 over HTTPS** (no
> signature); desktop apps (Tauri/Electron) use **signed feeds**; every
> self-updater with real reach **detects the install method** and delegates
> Homebrew. This plan borrows the detection/delegation and the stage-then-
> reload model, and — consistent with `install.sh` already exceeding the CLI
> norm — keeps the signature (which matches the desktop norm).

---

## 3. Design principles

1. **Check ≠ apply.** Checking is a passive HTTPS GET of a small signed
   document, in the same spirit as the catalog refresh. Applying mutates the
   user's filesystem and possibly restarts a service — that is always
   deliberate.
2. **Own the channel or don't touch it.** A binary self-replaces only when the
   project installed it (curl installer / tarball / Termux). If a package
   manager owns the file, self-replacing desynchronises the package DB — the
   updater must *not* do it.
3. **Reuse the catalog-thread pattern.** A dedicated maintenance thread,
   channel-driven, DB-anchored cooldown, no busy loops, no `Arc<Mutex>` — this
   keeps the change consistent with the daemon's threading rules
   (`AGENTS.md` → Thread Communication).
4. **One writer of the swap.** The daemon owns the update state machine
   (it is the always-running process); clients only *observe* (a broadcast)
   and *request* (a local-only message). Applying from the daemon also means
   the updater and the thing being restarted are the same process tree.
5. **Fail closed.** A missing/invalid signature, an unknown target, a
   downgrade, or a non-regular staged file aborts the swap and leaves the
   install untouched.

---

## 4. Trust model: a signed manifest

### Why a signature is mandatory

The current installer's security note says the version and checksums are
**embedded in the script**, so "a compromised download server cannot silently
swap in different (older or modified) binaries", and "the checksum file itself
is fetched over the same TLS channel as the tarball; the pin protects against
downgrade/swap attacks and TLS protects the pin's transport."

An auto-updater has **no pin** — it must discover the newest version at
runtime. If it trusted a `SHA256SUMS` fetched over TLS alone, then a
compromise of the download host (the very threat the pin defends against)
becomes a code-execution path. Therefore the manifest must be **signed**, and
the verification key **compiled into the binary** — the pin moves from "this
exact version" to "anything this offline key signed."

> **Note on precedent (§20).** This is stricter than the CLI norm: zero,
> jcode, and fx all trust a SHA-256 fetched over HTTPS with no signature.
> Only the desktop apps sign (Tauri's minisign feed; Electron's OS
> code-signing). Since Choreographr's `install.sh` already exceeds the CLI
> norm (it *pins*), the signature is the consistent choice — but a
> checksum-over-HTTPS manifest would match zero/jcode/fx and is a legitimate
> fallback if the signing key/rotation burden is unwanted ([D8](#15-decisions-to-confirm)).

### Manifest format

A small JSON document, `latest.json`, published beside the release:

```json
{
  "schema": 1,
  "version": "0.3.0",
  "release_name": "Lindy",
  "published_at": "2026-10-01T12:00:00Z",
  "channel": "stable",
  "notes_url": "https://github.com/choreographr/choreographr/releases/tag/v0.3.0",
  "assets": [
    {
      "target": "x86_64-unknown-linux-musl",
      "url": "https://choreographr.com/download/0.3.0/choreographr-0.3.0-x86_64-unknown-linux-musl.tar.gz",
      "size": 29123456,
      "sha256": "…"
    }
  ]
}
```

Published as `latest.json` plus a detached `latest.json.minisig`. The
signature is over the raw manifest bytes.

### Verification

- **minisign** (Ed25519) is recommended: a one-line public key, a tiny
  `minisign-verify` dependency (pure Rust), and it matches the project's
  preference for small, auditable deps. Alternatives: raw `ed25519-dalek`
  (adds key-management code) or `sigstore` (heavier, keyless, needs OIDC —
  overkill and a new online dependency).
- The public key is embedded like the release name — a `const` in a
  `choreo-shared` module, or a `choreo-shared/update-key.pub` file compiled in
  with `include_str!`. It is **not** read from a user-writable file.
- Fetch is hardcoded to a stable base URL (default
  `https://choreographr.com/updates/`), overridable only via an explicit env
  var for tests/mirrors (mirroring `CHOREOGRAPHR_BASE_URL`). The *key* is
  never overridable at runtime.
- **Anti-rollback (lite):** compare semver against the running version; refuse
  equal-or-lower unless `--allow-downgrade`. This is not TUF anti-freeze (a
  signed old manifest is still valid), but the manifest is served from a
  controlled host, and an attacker who can serve an old *signed* manifest can
  already serve the current one.

### Deliberate simplification vs. TUF

A full TUF repository (root/timestamp/snapshot/targets keys, expiry,
compromise-rotation) would be the textbook answer. It is rejected here for v1:
it adds four keys, a mirroring story, and a metadata server, for a project
whose only publishing disaster is a **stolen signing key**. That single risk
is handled operationally ([§13](#13-release--packaging-changes)): the key is
offline, the manifest is signed by the release conductor, and a key rotation
is a one-line public-key change shipped in a normal release. Revisit TUF only
if a second publisher ever signs.

---

## 5. Channel detection & per-channel strategy

The updater must know *how it was installed* before doing anything. Detection
should prefer **cheap, read-only path facts** over invoking package managers
(spawning `dpkg`/`rpm`/`brew` from the daemon is extra attack surface and
platform work). Proposed detection, most-specific first:

| Detected channel | Signal | Strategy |
|---|---|---|
| **dev build** | exe under a `target/…` path, or a repo `Cargo.toml` beside it | **refuse** — never touch (`--force` still refuses) |
| **Homebrew** | exe under `/opt/homebrew/` or `/usr/local/Cellar` (or `HOMEBREW_PREFIX` set) | notify: `brew upgrade choreographr` |
| **`.deb`** | exe == `/usr/bin/choreographr` and a `dpkg` marker dir exists | notify: `sudo apt install --only-upgrade choreographr` |
| **`.rpm`** | exe under `/usr/bin` and `/var/lib/rpm` exists | notify: `sudo dnf upgrade choreographr` |
| **AUR** | exe under `/usr/bin` and `/var/lib/pacman` exists | notify: `paru -S choreographr-bin` |
| **Termux** | `$PREFIX` contains `com.termux`, exe under `$PREFIX/bin` | notify: `pkg upgrade`; (optionally self-swap both binaries, user-writable) |
| **cargo** | exe under `~/.cargo/bin` | notify: `cargo binstall choreographr` / `cargo install` |
| **curl-installer / tarball** | exe under `$XDG_BIN_HOME`/`~/.local/bin` (or a generic user-writable dir), no package-manager signal | **self-update** both binaries |
| **unknown** | none of the above | notify-only + print `--version` reconciliation guidance |

Two refinements:

- The classification is a **function of the exe path + a few `stat`s**, kept
  pure and unit-testable (paths injected), exactly as
  `choreo-tui/src/autostart.rs::daemon_binary_path` is factored.
- Prefer **authoritative ownership** (ask `dpkg -S` / `rpm -qf`) only in an
  explicit `--verbose`/diagnostic path if at all; keep the hot path to path
  heuristics so the daemon never forks a package manager implicitly.

A `--channel <name>` override exists for the rare wrong guess and for tests.

This matches the consensus in the peer survey ([§20](#20-prior-art-how-peer-agents-update)):
zero (`DetectInstallMethod`: Homebrew keg = `Cellar/<formula>/<version>` with an
`INSTALL_RECEIPT.json`; npm via a marker file), codex (`InstallContext` →
`InstallMethod`), and opencode (`Installation.method()`) all classify the
install method — and **all three delegate Homebrew to `brew upgrade`** rather
than overwriting the keg. `zero` resolves symlinks first (Homebrew links
`<prefix>/bin` to the keg) so the check and the apply agree on what a
"Homebrew install" is.

---

## 6. The update state machine

```
Idle ──check──▶ Checking ──200/signed──▶ Available ──apply──▶ Downloading
   ▲                │                        │                    │
   │            304/error                    │ (dismiss)          ▼
   └────────────────┘                        └──────────────  Verifying
                                                                   │
                                              ┌──── invalid ────────┤
                                              ▼                     ▼
                                           Failed              Staged (on disk)
                                                                    │
                                                        ┌───────────┴──────────┐
                                                        ▼                      ▼
                                                   (notify-only          Swap (rename)
                                                    channels)                  │
                                                                               ▼
                                                                       RestartRequired
```

States and transitions to encode explicitly (in a `choreo-update` module):

1. **Checking** — GET `latest.json` (with `If-None-Match` etag; 304 → Idle).
2. **Available** — signature verified, semver > current, target has an asset.
3. **Downloading** — stream the tarball to
   `$XDG_DATA_HOME/choreographr/updates/<version>/`, enforce the manifest
   `size` (reject over-large bodies to bound memory/disk).
4. **Verifying** — SHA-256 of the tarball vs. the (now-trusted) manifest;
   optionally run `staged/choreographr --version` as a smoke test and confirm
   it reports the expected version.
5. **Staged** — tarball extracted to a temp dir with an **explicit member
   list** (only `choreographr`, `choreo-tui`, and the service files), each
   member **refused if not a regular file / is a symlink** — the exact
   hardening `install.sh` already applies.
6. **Swap** — see [§7](#7-atomic-swap-rollback-and-restart).
7. **Failed** — every error lands here with the reason; the install is
   untouched and the partial download is cleaned up.

The state machine is **reentrant and lock-guarded** by an advisory file lock
(`$XDG_RUNTIME_DIR/choreographr-update.lock`) so a CLI `choreographr update`
and a daemon-triggered update cannot race.

---

## 7. Atomic swap, rollback, and restart

### Swap (Unix)

1. Extract the staged binaries into a **temp dir on the same filesystem** as
   the target directory (so `rename` is atomic and does not `EXDEV`).
2. For each of `choreographr`, `choreo-tui`:
   - move the current binary aside to `<name>.old-<version>` (or keep a
     single `.<name>.bak`),
   - `rename(staged, target)` — atomic; a running process keeps the old inode,
   - `chmod +x`, and verify the swapped file is the expected mode/size.
3. On any failure mid-sequence, roll back the binaries already swapped from
   their `.bak`, then delete the staged dir.

The daemon can hold an open handle to its own `choreographr` inode — that is
fine on Unix (rename replaces the directory entry; the running process is
undisturbed until it restarts).

### Windows (deferred, but design for it now)

Windows **cannot** rename over a running `.exe`. The standard trick is
rename-aside + `MoveFileEx(.., MOVEFILE_REPLACE_EXISTING | MOVEFILE_DELAY_UNTIL_REBOOT)`,
or a small helper that waits for the daemon to exit. Since Windows is not yet
shipped (`RELEASE.md` — the `windows-msvc` job builds but is not attached),
the updater's Windows arm can be a stub that returns "manual update required"
until the channel ships.

### Restart

- **Never kill a running daemon mid-session.** Applying stages new binaries
  and transitions to **RestartRequired**; the actual restart is explicit.
- **systemd (user):** swapping the file does not restart the unit
  (`Restart=on-failure` fires only on failure, not on a file change). The user
  runs `systemctl --user restart choreographr`; the updater prints this.
  With `auto_apply = true`, the daemon may offer to re-exec after all sessions
  are idle — but that is a v2 nicety, gated behind explicit consent.
- **launchd / Homebrew:** `brew upgrade` + `brew services restart choreographr`.
- **TUI autostart:** the TUI spawns a private `--auto-exit` daemon as a
  **sibling** binary (`choreo-tui/src/autostart.rs`). If a TUI is updated
  while an *older* private daemon is running, the strict protocol gate makes
  the connection fail. The updater must therefore update **both** binaries in
  one operation and tell the user to relaunch the TUI; it must not mutate the
  sibling while the TUI holds it open (Unix rename handles this: the *new*
  TUI start picks up the new binary, the *old* one keeps running until exit).
- **Run the swap out-of-process / after the client exits (precedent, [§20](#20-prior-art-how-peer-agents-update)).**
  codex performs the upgrade *after* the TUI exits (it prints the command and
  runs it once the terminal is restored); fx downloads and stages in the
  background, then waits for the user to press `ctrl+g` and refuses to reload
  while work is in flight (a streaming response, queued prompts, an open
  modal, an unsent draft); Electron apps ask for a separate confirmation and
  check for active tasks before `quitAndInstall`. All three avoid replacing a
  binary that is mid-use. Choreographr's `RestartRequired` state is the same
  idea — and for the TUI autostart case, staging then letting the *next* TUI
  launch pick up the new binary sidesteps self-replacement entirely.

---

## 8. Versioning & compatibility

- **Source of truth:** `[workspace.package] version`; the manifest echoes it.
- **Semver comparison** drives "is this newer"; the release name
  (`choreo-shared/release-name.txt`) is display-only metadata included in the
  notification.
- **Both binaries update together.** A tarball carries both; a partial update
  (new daemon, old TUI) is never produced.
- **Protocol gate.** `PROTOCOL_VERSION` (currently 6) is a hard gate with no
  negotiation. An update that bumps it means a running old daemon + new client
  fail fast. Consequences:
  - The **notification** should warn when the available version's protocol
    differs from the running one ("restart the daemon to update; the new TUI
    requires it").
  - The manifest *could* carry `protocol_version` so the updater can detect a
    breaking bump before download; recommended addition.
- **Data/db migration** is already handled per-release (e.g. the schema-2 zstd
  turn re-encode with a `state.redb.bak-v1` backup), so an update across a
  schema bump is safe by construction — worth restating in user-facing docs.

---

## 9. UX surfaces

1. **CLI subcommand** — `choreographr update [--check] [--channel <c>]
   [--allow-downgrade] [--force]`. `--check` only reports; without it, it
   performs the decision for the detected channel (self-swap or print the
   package-manager command). A `choreo-tui update` twin (or the same shared
   code) so either binary can drive it. Mirrors the existing `acl-add` /
   `fingerprint` utility subcommands in `cli.rs`.
2. **Daemon background check** — logs `update available: X.Y.Z (Name)` and
   broadcasts a message (see [§10](#10-scheduling--architecture-fit)).
3. **TUI status banner** — a non-blocking line, e.g.
   `⬆ 0.3.0 available — /update`, shown from the broadcast; `/update` opens a
   confirm dialog (local connections only) and, on confirm, invokes the apply
   path. Reuses the existing status-line mechanism (like the keystore-locked
   banner).
4. **config.toml `[updates]`:**

   ```toml
   [updates]
   check = true          # background check (default true, like the catalog refresh)
   auto_apply = false    # never apply without explicit consent (default false)
   channel = "stable"
   check_interval_hours = 25
   ```

   `CHOREOGRAPHR_NO_UPDATE_CHECK=1` and `CHOREOGRAPHR_UPDATE_BASE_URL=…` env
   overrides (the latter for tests/mirrors).

5. **Wire messages** (new, and therefore a **protocol bump**): e.g.
   `DaemonMessage::UpdateAvailable { version, release_name, notes_url }` and,
   if remote-triggering is ever allowed, `ClientMessage::CheckForUpdates` /
   `ApplyUpdate` — but **apply/restart must be local-only**, exactly like
   `/acl add` (the approver for a trust decision must be at the machine). A
   remote client may only *observe*.

---

## 10. Scheduling & architecture fit

Clone the catalog maintenance thread's proven shape
(`choreo-daemon/src/catalog.rs`):

- A **detached maintenance thread** owned by the daemon, driven by a
  crossbeam channel with a **`recv_timeout` that doubles as the cadence** —
  no polling, no busy loop.
- **Wall-clock cooldown persisted in the DB** (`update_state`, or reuse the
  `catalog_state` pattern), written **before** the attempt, so a
  restart-every-few-hours daemon checks once per ~day and a crash cannot
  re-trigger immediately.
- **25 h** (not 24) so check times **drift +1 h/day**, spreading load across
  the server's daily cycle — same reasoning as `REFRESH_ATTEMPT_INTERVAL`.
- **Throttle the check and persist the throttle** — peers do exactly this
  ([§20](#20-prior-art-how-peer-agents-update)): jcode detects GitHub's 60/h
  unauthenticated limit and persists a **backoff window** every process on the
  machine reads; codex caches the last check for 20 h in a version file and
  persists a `dismissed_version`; fx checks 30 min after a 10 s first delay;
  maka-agent checks 10 s after launch, then every 4 h, plus a 15 min-throttled
  focus check. Our DB-anchored cooldown is the same pattern.
- **Prefer a self-hosted manifest to the GitHub API.** jcode's rate-limit
  handling exists because `api.github.com` shares a 60 req/h per-IP bucket with
  everything else on the machine/NAT. A static, signed `latest.json` on
  `choreographr.com` has no such limit (and no GitHub JSON-shape coupling) —
  reinforcing [D7](#15-decisions-to-confirm).
- **`/refresh-models`-style manual bypass**: `/update --check` (or the CLI)
  bypasses the cooldown but still records the attempt.
- The check itself is a blocking `ureq` GET on this thread (the daemon is
  thread-only; no tokio) — exactly like the catalog fetch.
- Broadcast: the thread hands a `DaemonCommand` to the command loop over a
  channel (never the thread fanning out to clients directly), matching the
  catalog thread's "the thread owns HTTP; the loop owns broadcast" split.
- **Startup gating:** fetch immediately iff no recorded attempt or the attempt
  is stale (same as catalog), otherwise arm the timer for the remainder.

The `AGENTS.md` threading rules are satisfied: no shared mutable state, no
`Arc<Mutex>`, no lock — the cooldown lives in the DB, the events in channels.

---

## 11. Platform specifics

| Platform | Shipped channel(s) | Auto-update behavior |
|---|---|---|
| **Linux x86_64/aarch64** | static-musl tarball, curl installer, `.deb`, `.rpm`, AUR(deferred) | self-swap only for the tarball/installer dir; notify for packages |
| **macOS arm64/x86_64** | Homebrew (recommended), curl installer, tarball | notify → `brew upgrade`; self-swap for the installer dir. Note Gatekeeper: re-check/clear quarantine on the staged binary if needed (curl-fetched files are not quarantined) |
| **Android/Termux** | Termux `.deb`, tarball | notify → `pkg upgrade`; optional self-swap into `$PREFIX/bin` (user-writable, no root) |
| **Windows x86_64** | `.zip` (not yet attached) | stub: manual update; design the rename-aside path for when it ships |
| **iOS/embedded (`choreo-gui`)** | app bundle / store | out of scope (store-driven) |

Note the **read-only-install** case for system packages: the daemon runs as
the user and cannot write `/usr/bin`, which is a second, independent reason
package channels are notify-only.

---

## 12. Security considerations & threat model

**Threats and defenses**

- **Malicious/compromised download host** → signature over the manifest with
  a compiled-in public key; a host that swaps the tarball or the manifest
  fails the signature check. This is the key improvement over the current
  TLS-only checksum model.
- **MITM / downgrade** → TLS + signature + semver monotonicity
  (`--allow-downgrade` opt-in only).
- **Stolen signing key** → offline key, conductor-signed manifest, and a
  documented rotation procedure (ship a new public key in a normal release).
  Revisit TUF only if there is more than one signer.
- **Malicious archive members** → explicit member list + refuse symlinks /
  non-regular files (copy the exact hardening from `install.sh`).
- **TOCTOU on the swap** → stage on the same filesystem, verify, then
  `rename`; never write the target in place.
- **Symlink attacks on the staging/update dirs** → the updater should reuse
  the `O_NOFOLLOW` + ownership/mode checks the daemon already applies to
  `--log-file` (`cli.rs::open_log_file`): create the update dir `0700`, refuse
  to follow symlinks at predictable paths, and verify regular-file ownership.
- **A lower-privileged user who can write in the install dir** → this is the
  exact threat `zero`'s promotion path defends against: it binds the final
  `rename` to an *open descriptor* on the staging file and its directory
  (not the staging pathname) so a race cannot substitute an attacker's file
  after verification; it also refuses to reuse an unverifiable leftover it
  did not create (`ErrTargetPossiblyTampered`). We can adopt the simpler
  descriptor-bound `rename` and the "never delete a file you did not create"
  rule even without the full machinery.
- **Resource exhaustion** → enforce the manifest `size` cap on the download,
  extract with the same "explicit member list" policy, and clean up partial
  state on every failure.
- **Confused deputy / remote trigger** → applying and restarting are
  **local-connection-only**, like `/acl add`.
- **Supply chain of the updater itself** → the new crate/module keeps the
  workspace's `--locked`, `cargo-deny`, and crates.io-only policy; if the
  manifest is JSON, prefer a minimal, well-audited parser (or a tiny hand
  parser, since the schema is fixed) over a large dependency. `minisign-verify`
  is intentionally small; if its tree is undesirable, vendoring the ~100 lines
  of Ed25519 verification over `ed25519-dalek` is an option.
- **Privacy** → the check is a plain GET of a public static file with no
  install id, no user data, and an `If-None-Match` etag; it can be disabled
  entirely (`check = false` / `CHOREOGRAPHR_NO_UPDATE_CHECK=1`).

**Testing discipline (per `AGENTS.md`)**

- The state machine, channel classification, manifest parsing, signature
  verification, semver comparison, and swap/rollback logic are **pure /
  filesystem-scoped unit tests** (`#[cfg(test)]`) with **no time-based
  waits**.
- The HTTP fetch, the real end-to-end swap, and the daemon maintenance-thread
  cadence are **integration tests** under `tests/it/`, marked `#[ignore]`,
  driving a **local HTTP server** via `CHOREOGRAPHR_UPDATE_BASE_URL` and a
  temporary fixture manifest/minisig. No test ever hits the real network.

---

## 13. Release & packaging changes

1. **Generate + sign the manifest in the release flow.** After the release
   job assembles the assets and the combined `SHA256SUMS`
   (`.github/workflows/release.yml`), add a step that emits `latest.json`
   (version, release name, protocol version, per-target url/size/sha256) and
   produces `latest.json.minisig`.
   - **Preferred:** sign **offline** in the conductor step (Phase 4 of
     `RELEASE.md`), alongside the Homebrew/choreographr.com channel updates —
     consistent with the project's "the conductor drives channel updates"
     model and keeping the private key out of CI secrets.
   - Alternatively, sign in CI with the key in a **GitHub Actions secret**;
     noted as the weaker option (CI compromise ⇒ key compromise).
2. **Publish** `latest.json` + `.minisig` to
   `https://choreographr.com/updates/latest.json` (a stable URL that always
   points at the current release; versioned copies kept for reproducibility),
   and to the GitHub release for transparency.
3. **`install.sh`** gains `--check` and `--update` modes (or a
   `scripts/update.sh` shim) that reuse the same manifest + verification, so
   shell-only users get parity with the in-binary updater.
4. **`scripts/`**: a `scripts/manifest.sh` (generate) and a signing helper;
   `just` recipes `just manifest` / `just sign-manifest`.
5. **Docs**: `README.md` (an "Updating" section), `packaging/README.md`
   (the update policy: own-the-channel vs notify), `ARCHITECTURE.md` (a
   module row for the update maintenance thread + the trust model),
   `RELEASE.md` (the manifest/signing steps in Phase 4), and this plan marked
   done.
6. **`choreo-proto`**: bump `PROTOCOL_VERSION` if a wire message is added, and
   document the bump (the release notes are the commit messages).
7. **`Cargo.toml`**: add any new dep to `[workspace.dependencies]` (e.g.
   `minisign-verify`) per `AGENTS.md`.

---

## 14. Phased implementation plan

Each phase is one commit; overlapping-file phases run as **serial
subsessions** (per `AGENTS.md`).

- **Phase 1 — check-only, no writes (G1, G4).** New leaf crate/module:
  channel classification (pure), manifest fetch + parse, and semver compare.
  `choreographr update --check` (and a TUI banner fed by a daemon broadcast).
  Daemon maintenance thread mirroring `catalog.rs`. No signature yet *only if*
  Phase 1 ships check-only and never downloads (still fetch the fields, but
  refuse to act). **Bump `PROTOCOL_VERSION`** for the broadcast message.
- **Phase 2 — signed manifest + apply for self-owned channels (G2, G5).**
  Add minisign verification + the compiled-in key; implement
  Download→Verify→Stage→Swap→RestartRequired for the installer/tarball dir;
  notify-only for package channels. Full unit + integration coverage.
- **Phase 3 — opts-in automation (G3, G4).** `[updates] auto_apply`, service
  restart integration (systemd/launchd), package-manager hint polish, and the
  Termux self-swap. Windows stub.
- **Phase 4 — release/packaging (G5).** Manifest generation + offline signing
  in the release flow; publish to choreographr.com; `install.sh --update`;
  docs. Mark this plan done.

Dependency: 1 → 2 → 3 → 4. Phases 1–3 touch overlapping daemon/TUI files and
must run serially.

---

## 15. Decisions to confirm

**Recommended / proposed:**

- **D1 — Trust anchor:** minisign-signed manifest, key compiled in. *(Recommend:
  yes — matches the desktop norm and `install.sh`'s existing pin; but note that
  zero/jcode/fx ship checksum-over-HTTPS with **no** signature and it is the
  accepted CLI baseline — see [§4](#4-trust-model-a-signed-manifest) and
  [§20](#20-prior-art-how-peer-agents-update).)*
- **D2 — Check default:** background check **on by default** (mirrors the
  models.dev refresh), apply **off** by default. *(Recommend: yes.)*
- **D3 — Self-update scope:** only installer/tarball/Termux dirs; everything
  else notify-only. *(Recommend: yes.)*
- **D4 — Who applies:** the daemon owns the state machine; apply/restart are
  **local-only**. *(Recommend: yes.)*
- **D5 — Rollout:** ship check-only first (Phase 1) before any self-write.
  *(Recommend: yes.)*

**To confirm before implementing:**

- **D6 — Crate vs. module:** a leaf `choreo-update` crate (shareable by
  daemon, TUI, and `install.sh`'s Rust twin) or a `choreo-shared` module?
  *(Recommend: leaf crate — it has network + filesystem deps that
  `choreo-shared` deliberately avoids.)*
- **D7 — Manifest hosting:** choreographr.com primary + GitHub release
  mirror, or GitHub Releases API as the source? *(Recommend: choreographr.com
  — a controlled domain the project already uses for the installer, avoiding
  GitHub's rate limits and JSON shape.)*
- **D8 — Signer:** offline conductor key vs. CI secret? *(Recommend:
  offline.)*
- **D9 — Auto-restart:** ever? *(Recommend: v2, explicit-consent only.)*
- **D10 — Apply timing:** swap out-of-process / on the next launch, never
  in-place over a running binary (codex after-exit; fx stage-then-`ctrl+g`)?
  *(Recommend: yes.)*
- **D11 — Installer reuse:** should `install.sh` gain a `--latest`/`--update`
  mode and act as the apply path for the curl channel (as codex/opencode do),
  rather than a second in-binary downloader? *(Recommend: yes — one
  downloader/verifier, shared.)*

---

## 16. Risks & mitigations

- **Key management.** A lost/compromised signing key is the worst case.
  Mitigation: offline key, documented rotation, public key change shipped like
  any release; TUF deferred until a second signer exists.
- **Package-manager drift.** Self-replacing a package-owned binary would
  desync the package DB. Mitigation: channel detection defaults to
  notify-only; `--force-self` requires an explicit flag and warns.
- **Mixed-version daemon/client.** Protocol gate fails hard. Mitigation:
  update both binaries atomically; warn on a protocol bump; local-only apply.
- **Surprising the user / breaking "never auto-enabled".** Mitigation:
  apply/restart are explicit; check is quiet and disableable.
- **Windows swap.** No atomic replace of a running exe. Mitigation: stub
  until the channel ships.
- **New dependency / parser surface.** Mitigation: keep the manifest JSON
  minimal and the OS-specific logic small; `cargo-deny` still governs.
- **Disk/space on small installs (Termux).** Staging needs ~2× the tarball
  size momentarily. Mitigation: stream to the data dir, enforce size, clean up
  on every exit path.

---

## 17. Out of scope / future work

- Windows update path (until the `.zip` channel ships).
- `choreo-gui` / iOS / Android-app update (store or app-bundle mechanics).
- Hot upgrade of a running daemon without a restart.
- Delta/binary-diff updates (whole-tarball downloads are small enough).
- Update **channels** beyond `stable` (e.g. `beta`/`nightly`) — the manifest
  schema reserves `channel`, but no extra channel ships initially.
- Signed *per-binary* artifacts and a full TUF metadata repository.

---

## 18. Verification / definition of done

- `choreographr update --check` reports the available version against a signed
  fixture manifest; `choreographr update` self-swaps both binaries for the
  installer channel, atomically, with rollback on an induced failure.
- A tampered tarball, a tampered manifest, a bad signature, an unknown target,
  and a downgrade **all** abort without touching the install (unit + `tests/it`).
- The daemon maintenance thread honours the 25 h cooldown across restarts
  (DB-anchored) and bypasses it on an explicit check.
- A package-channel install prints the correct package-manager command and
  never writes to `/usr/bin`.
- Protocol bump handled: mixed-version peers fail fast; the notification warns.
- `just pre-commit` green; docs updated; commit messages written as release
  notes (this design change is docs-only and needs no gate).

---

## 19. References

- `scripts/install.sh` — the current pinned-version + SHA-256 installer and its
  security rationale (the bar the auto-updater must meet or exceed).
- `choreo-daemon/src/catalog.rs` — the maintenance-thread + 25 h cooldown +
  DB-anchored attempt + etag pattern to mirror.
- `choreo-daemon/src/config.rs`, `~/.config/choreographr/config.toml` — where
  `[updates]` lives.
- `choreo-daemon/src/cli.rs` — existing utility subcommands
  (`acl-add`, `fingerprint`) and the `open_log_file` symlink/ownership
  hardening to reuse for update staging.
- `choreo-tui/src/autostart.rs` — sibling-binary spawn (daemon/TUI coupling on
  update).
- `choreo-proto/src/frame.rs` — `PROTOCOL_VERSION` (strict, no negotiation).
- `choreo-shared/` — `release_name` (`--version`), the natural home for the
  compiled-in signing public key.
- `Cargo.toml` `[package.metadata.binstall]` — the existing asset-URL template
  the manifest can mirror.
- `packaging/README.md` — the "installed, never auto-enabled" policy.
- `.github/workflows/release.yml`, `RELEASE.md` — where manifest generation and
  signing slot in.

---

## 20. Prior art: how peer agents update

Surveyed `~/agents` on 2026-09-20. Four families, one clear pattern.

**A. Native self-update from GitHub Releases / a CDN, SHA-256 over HTTPS (CLIs).**

- **zero** (Go, `internal/update/*`) — GitHub `releases/latest`; per-asset
  `*.sha256`; semver; `DetectInstallMethod` (Homebrew keg = `Cellar/<f>/<v>`
  carrying an `INSTALL_RECEIPT.json`; npm via a `.zero-binary-version` marker /
  `package.json` shape; else standalone); **Homebrew → refusal + `brew upgrade`**;
  npm → `npm install -g …@latest`; standalone → download + verify + extract +
  **descriptor-bound atomic promote** (the rename is bound to an open dir/file
  handle, not a pathname, to defeat a writable-install-dir race) with a
  distinct `ErrTargetPossiblyTampered`, Windows delete-on-close, optional
  helper-binary refresh, `zero upgrade`/`--check`, and a `data:` endpoint for
  deterministic tests.
- **jcode** (Rust, `jcode-update-core` + `jcode-app-core/update*`) — GitHub
  `releases/latest` + `SHA256SUMS`; platform asset naming; atomic install;
  **GitHub 60/h rate-limit detection + a persisted backoff** shared across
  processes; a **dev-build guard** (compare the compiled git hash to the
  release tag via local `git merge-base --is-ancestor`, else the GitHub compare
  API; keep the dev build unless provably behind); a background-vs-foreground
  time estimate; plus a "main source" path (`git pull` + `cargo build`).
- **fx** (Zig, `core/upgrade/*`) — background thread (10 s first delay, 30 min
  interval, interruptible sleep, bounded join on stop); CDN
  `fx-<ver>-<platform>.tar.gz` + `.sha256`; verify → extract → atomic copy over
  self; then **waits for the user to press `ctrl+g` to reload** and refuses to
  reload while work is in flight (streaming, queued prompts, open modal, unsent
  draft); dev-build guard by path (`/zig-out/bin/`); stable/dev channels.

**B. Delegate to the package manager (detect method, no self-write) (CLIs).**

- **codex** (Rust) — `InstallContext` → `InstallMethod`
  (npm/bun/viteplus/pnpm/brew/standalone/other) → `UpdateAction`; a startup
  check throttled to 20 h and cached in a version file; latest version fetched
  from the **method-specific** source (brew cask API; npm registry + GitHub;
  GitHub releases for standalone); a dismissable banner with a persisted
  `dismissed_version`; the actual upgrade runs the package-manager command
  **after the TUI exits** (standalone re-runs the install script).
- **opencode** (TS) — `Installation` service detects method
  (curl/npm/yarn/pnpm/bun/brew/scoop/choco), fetches latest per source,
  `opencode upgrade [target]` runs the method command (pipes the install script
  for the curl method).

**C. Desktop app frameworks with signed feeds.**

- **buzz** (Tauri) — `tauri-plugin-updater`, public key + endpoint embedded at
  build (`BUZZ_UPDATER_PUBLIC_KEY`/`_ENDPOINT` via `build.rs`); Tauri's
  **minisign-signed `latest.json`**.
- **deepseek-harness** (Electron) — `electron-updater`; signed installers
  (Authenticode / notarization); Sha512 + blockmap; fixed feed URL;
  `autoDownload=false`; manual download + install with an explicit restart
  confirmation and an evidence/journal workflow.
- **maka-agent** (Electron) — `electron-updater`; `autoDownload=true`; 10 s
  first check, 4 h interval, 15 min throttled focus check; an **attestation
  verifier** for the downloaded artifact; a prepared install with rollback and
  an active-task guard; `quitAndInstall`.

**D. Server-side `git pull` self-update (supervised restart).**

- **headlong** (Python web) — opt-in (`HEADLONG_WEB_SELF_UPDATE=1`);
  `POST /api/update` → `git pull` → rebuild static → SIGTERM; systemd
  `Restart=always` returns on the new code.

**E. No self-updater:** hermes-agent, turnstone, openwork (Helm/K8s rollout),
  t3code, OpenMinis (app store), rtk (Homebrew / install.sh), pi (install
  locks), langgraph.

### Cross-cutting lessons

1. **Integrity is usually just SHA-256 over HTTPS in CLIs; signatures only in
   desktop frameworks.** zero/jcode/fx use checksums with no signature; only
   Tauri (minisign) and Electron (OS code-signing) sign. Our signed-manifest
   idea is *ahead* of the CLI norm and *matches* the desktop norm.
2. **Install-method detection is non-negotiable** — every self-updater that
   touches a real machine has it, and Homebrew is universally delegate/refuse.
3. **Never self-write where a package manager owns the file.**
4. **Stage, then let the user reload** (fx `ctrl+g`; Electron confirmation;
   codex after-exit).
5. **Run the updater out-of-process / after the client exits** to dodge
   self-replacement races.
6. **Throttle the check and persist the state** (jcode backoff; codex 20 h;
   fx 30 min; maka 4 h + focus).
7. **Guard dev builds** — never overwrite one (jcode ancestry; fx path).
8. **GitHub's unauthenticated 60/h limit is real** (jcode) — argue for a
   self-hosted manifest.
9. **Windows replacement is the hard part** — zero carries ~45 KB of
   Windows-specific stage/replace code (delete-on-close, reboot replacement).
10. **Quiet failures + dismissable banners** (codex persisted dismissal; maka
    holds back transient check errors; deepseek quiet auto-check).
