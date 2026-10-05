# Plan: key per-daemon keystore unlock keys by daemon identity, not dial address

**Status:** proposed — design complete; nothing implemented (no source changes in this
change).
**Lifecycle:** this file is **deleted once the plan is fully implemented**. Nothing
written during implementation may reference it — rustdoc, `ARCHITECTURE.md`,
`README.md`, release notes, and commit messages must stand on their own, because a
reference to this plan would go stale the moment it is removed.
**Date:** 2026-10-05
**Targets:** `choreo-proto` (a daemon-identity frame); `choreo-daemon` (advertise the
identity, thread the transport public key to the connection threads); `choreo-client-core`
(the keyring keying + startup ordering); `choreo-tui`, `choreo-gui` (consume the identity);
`choreo-transport` (expose the pubkey already loaded at startup); docs for the trust model.
**Touches:** `choreo-proto/src/{types,frame}.rs`, `choreo-daemon/src/{cli.rs,server/core.rs,server/connection.rs}`,
`choreo-client-core/src/{known_servers,credentials,connection}.rs`,
`choreo-tui/src/{state/mod.rs,connection/mod.rs}`,
`choreo-gui/src/client.rs`, `choreo-transport/src/key.rs`,
`choreo-keystore/src/paths.rs` (only if the store moves files),
unit tests in the above, the shared daemon harness `tests/it/`, `ARCHITECTURE.md`,
`README.md`.

> **TL;DR.** The per-daemon keystore unlock key is stored client-side in
> `known_servers.toml` **keyed by the dial address** — and for a unix connection the
> dial address is the socket path. The default socket path just moved from
> `$TMPDIR/choreographr.sock` to `$XDG_RUNTIME_DIR/choreographr.sock`
> (`7839b6d`), which silently orphans the stored key: the client looks it up under
> the new path, misses, and the daemon stays locked. A dial address is a transient
> transport detail, not an identity; keying a durable secret on it is the bug. The
> fix is to key the keystore entry by a **stable daemon identity** — the daemon's
> Noise transport public key, advertised over the socket at connect — and to keep
> the SSH-`known_hosts`-style address→key **pin** (which genuinely is
> address-keyed) as a separate concern. Backwards compatibility is explicitly out
> of scope (beta installs only).

---

## Table of contents

1. [Problem](#1-problem)
2. [Root cause](#2-root-cause)
3. [Design principle](#3-design-principle)
4. [Design](#4-design)
5. [Phases](#5-phases)
6. [Testing strategy](#6-testing-strategy)
7. [Risks & mitigations](#7-risks--mitigations)
8. [Out of scope](#8-out-of-scope)
9. [Open questions](#9-open-questions)
10. [Definition of done](#10-definition-of-done)

---

## 1. Problem

On a build from `master`, the TUI reports the daemon keystore as **locked** and
cannot unlock it, even though the same install works on the preceding commit. The
daemon is *bound* (the binding lives in `state.redb`, path unchanged), and the
client still holds the correct unlock key — but the client no longer finds it.

Observed store (`~/.config/choreographr/known_servers.toml`):

```toml
[[server]]
addr = "127.0.0.1:9443"
pubkey = "HQCharwXgMYLmo/91PPI4mOo9HXYrs3T3aVRr+1idHI="
unlock_key = "NWqDiVg+dq12wE6Fu9Q9dCtqYsAJI36ggu5L1FOpPF4="

[[server]]
addr = "/tmp/choreographr.sock"
unlock_key = "NWqDiVg+dq12wE6Fu9Q9dCtqYsAJI36ggu5L1FOpPF4="
```

Both entries are the same daemon holding the same `unlock_key`. There is **no**
entry for the new default socket path.

## 2. Root cause

`7839b6d` ("resolve the socket and logs through XDG, and flatten the base-dir
layout") changed the default unix socket path from
`std::env::temp_dir()/choreographr.sock` to
`dirs::runtime_dir()/choreographr.sock` (`choreo-proto/src/io.rs::default_socket_path`).
On a desktop Linux with `XDG_RUNTIME_DIR` set (the normal case) that is
`/tmp/choreographr.sock` → `/run/user/<uid>/choreographr.sock`.

The per-daemon keystore unlock key is stored **keyed by the dial address**, and for
a unix connection the dial address *is* the socket path:

- `choreo-client-core/src/known_servers.rs` — `known_servers.toml` entries are keyed
  by `addr` (`entries.iter().find(|e| e.addr == addr)`); `unlock_key(addr)` /
  `set_unlock_key(addr, key)`.
- `choreo-tui/src/state/mod.rs` — `connection_addr: socket_path()`;
  `choreo-tui/src/connection/mod.rs` — `ConnectionMode::UnixSocket(path) => path.clone()`;
  auto-unlock is `try_auto_unlock_key(&app.connection_addr)`.
- `choreo-gui/src/client.rs::connection_addr()` — the same scheme.

So after the path change: the client resolves `connection_addr` to the new path,
the `known_servers` lookup misses, the `identity.pk` fallback is a legacy file
production code never writes, and the daemon never receives an `Unlock`.

This is **fail-closed but silent**, and it is not just a lock-banner nuisance: the
private half of the unlock keypair exists only client-side
(`decrypt_credential_blobs` merely logs and skips a blob it cannot decrypt), so a
lost entry strands the encrypted credential blobs.

The same latent defect exists in `choreographr migrate`: `migrate.rs` copies
`known_servers.toml` verbatim and moves the socket to `{base}/run/…`, but the
copied entry still names the old default path — so a base-dir migration of a
unix-socket install orphans the key too. Its rustdoc claim ("a client's pinned
server key keeps verifying after the move … and the DB's keystore binding still
holds") is true for TCP only.

Note the design already knows the path is a bad key: the iOS embedded daemon keys
its binding under the stable literal `"embedded"`, explicitly "never under
`socket_path()`". The main path did not get the same treatment.

## 3. Design principle

> The unlock key is a property of the **daemon's keystore**, not of the
> **connection**. It must be keyed by a stable, daemon-owned identity; a dial
> address is data, never the key.

Reaching for the socket path is not a design choice — it is what remains when no
daemon identity is available to the client. The fix is to make the identity
available, then key on it.

## 4. Design

### 4.1 The identity

Use the daemon's **Noise transport public key** (`transport.pub`). It is:

- **always present** — the daemon calls `ensure_transport_keypair()`
  unconditionally at startup (`choreo-daemon/src/cli.rs`), even for unix-only use;
- **stable and unique per instance** — it lives in the instance's config dir, so
  two instances on one host (the `--base-dir` case that motivated the flag) have
  distinct identities;
- **already the system-wide identity** — the ACL key, the TCP pin, and what
  `choreographr fingerprint` renders; keying the keystore on it makes "one daemon =
  one identity = one keyring entry" true everywhere;
- **already in the store** — the `pubkey` field of the TCP entry above *is* this
  identity.

### 4.2 Advertise it on connect

The daemon never tells a client its identity today (the version gate in
`frame.rs` carries no identity, and TCP clients learn the key from the handshake,
not an application frame). Add a first-frame advertisement:

```rust
// choreo-proto/src/types.rs — DaemonMessage
ServerIdentity { transport_pubkey: [u8; 32] },
```

sent by the daemon as the **first frame on every connection**, before it processes
any `ClientMessage` and before the subscribe-time `Keystore` status push. The
daemon already holds the keypair; thread the public half alongside `transport_sk`
into `run_server`/`start_daemon_core` and down to each connection thread (the
writer already sends on connect for the `Keystore` push, so the send site exists).

For TCP the frame is redundant with what the handshake already proved — harmless,
and it keeps one client-side keying rule for every transport.

### 4.3 Key the client store by identity; keep the pin address-keyed

`known_servers` currently answers two different questions with one index, which is
the deeper flaw:

| Question | Correct key | Why |
|---|---|---|
| "Is the server at this address the key I confirmed?" (the SSH `known_hosts` pin) | **address** | The whole point of a pin is "this host must present this key". Keyed by key, the pin is vacuous. **Stays address-keyed.** |
| "Which unlock key belongs to this daemon's keystore?" | **identity** | The key follows the daemon, not the way you happened to dial it. |

Split them. Shape (exact TOML is an open question):

```toml
# known_hosts analogue — address → human-confirmed server identity (TCP)
[[pin]]
addr = "192.168.1.6:9443"
pubkey = "sakdcbVazi0TVRu9TtIOU3zZH7ts8LLRkholwJCw0mY="

# per-daemon keystore key — keyed by the daemon's transport identity
[keystore."HQCharwXgMYLmo/91PPI4mOo9HXYrs3T3aVRr+1idHI="]
unlock_key = "NWqDiVg+dq12wE6Fu9Q9dCtqYsAJI36ggu5L1FOpPF4="
```

`known_servers::unlock_key`/`set_unlock_key` become identity-keyed
(`&[u8; 32]`, not `&str`); `lookup`/`pin`/`remove` stay address-keyed.
`credentials.rs`'s `resolve_private_key` / `try_auto_unlock_key` /
`record_unlock_key` / `build_add_credential_message` take the identity instead of
the address. The dial address is still recorded for display, never for lookup.

### 4.4 Reorder client startup so `Unlock` follows identity

Today the TUI/GUI resolve the key and send `ClientMessage::Unlock` **before reading
any daemon message**. Once the key depends on the advertised identity, the startup
state machine becomes:

1. connect;
2. read `ServerIdentity` (the connection reader's first message);
3. look up the unlock key by identity;
4. if found → send `Unlock`; if not → fall through to the existing
   `Keystore`-status / auto-bind flow (a fresh daemon reports `Unbound` and is
   auto-bound; a bound daemon with no key stays locked and says so).

This is a small move of the existing "resolve on connect" logic into the
identity handler; the "no key available" path already exists.

### 4.5 Edge cases

- **Embedded / in-process (iOS):** no dial address and (currently) no generated
  transport key. Either keep the stable `"embedded"` literal or give the embedded
  daemon its own transport keypair so it fits the uniform scheme. It must never
  fall back to `socket_path()`.
- **SSH-forwarded / remote unix socket:** the identity must come **over the wire**;
  the client must not read it from its own `transport.pub` (a different key).
- **Regenerating `transport.sec`:** changes the identity and orphans the entry —
  acceptable and consistent, because regenerating the transport key already
  invalidates every TCP pin and ACL enrollment.
- **`--base-dir`:** identity-keying removes the migrate orphaning for free — the
  copied store is keyed by identity, which the daemon reports unchanged.

### 4.6 Optional hardening (decide in P4, not required for correctness)

Over a local unix socket the advertised identity is **unauthenticated** — it is a
continuity label, and trust still rests on the socket path's filesystem
permissions (`$XDG_RUNTIME_DIR` being 0700 is what makes the move net-positive).
The current design also sends `Unlock` before reading anything, so an attacker who
binds the path is handed the key. If the keystore warrants it, have the daemon
sign a nonce with its transport secret and let the client pin
`address → identity` (TOFU, the SSH shape) so a changed identity at a known
address is a loud error. This closes the harvest hole and is cheapest to add while
the identity frame is already being introduced.

## 5. Phases

Each phase is independently shippable, lands with tests + docs, and keeps
`just pre-commit` green.

### P1 — Protocol: the identity frame

- Add `DaemonMessage::ServerIdentity { transport_pubkey: [u8; 32] }`; bump
  `PROTOCOL_VERSION` (10) with the `frame.rs` changelog entry.
- Encode/decode round-trip test; the mixed-version gate already fails fast on the
  bump.

### P2 — Daemon: advertise on connect

- Thread the transport **public** key (already loaded in `cli.rs`) into the core
  and each connection thread; send it as the first frame, before request
  dispatch and before the subscribe-time `Keystore` push.
- Unit test: a fresh connection's first received message is `ServerIdentity` with
  the daemon's `transport.pub`, on unix, TCP, and embedded transports.

### P3 — Client: key by identity

- `known_servers`: split the address-keyed pin from the identity-keyed keystore
  map; make `unlock_key`/`set_unlock_key` identity-keyed.
- `credentials.rs`: thread the identity through `resolve_private_key`,
  `try_auto_unlock_key`, `record_unlock_key`, `build_add_credential_message`.
- TUI + GUI: read `ServerIdentity` first, then resolve/send `Unlock`; drop
  `connection_addr` as the keystore key (keep it for the transport dial and
  display).
- Tests: two daemons sharing a host keep disjoint unlock keys; the same daemon
  reached over TCP and unix resolves one key; changing the socket path does not
  change the lookup key; an unknown identity falls through to auto-bind.

### P4 — Docs + optional hardening

- `ARCHITECTURE.md`: rewrite the "Per-daemon unlock key" and `known_servers.rs`
  sections for identity-keying; correct the `migrate.rs` rustdoc claim about unix;
  document the `ServerIdentity` frame in the wire-version history.
- `README.md`: the trust-model wording if it names the address keyring.
- Decide 4.6 (signed hello + address→identity pin); if adopted, land it here.
- Release notes from the commit messages.

## 6. Testing strategy

Per AGENTS.md: unit tests in `src/` are wait-free; socket/thread integration tests
live in `tests/it/` (one `it` binary per crate) and reuse the shared daemon harness.

- **Unit (wait-free):** proto round-trip for `ServerIdentity`; the identity-keyed
  store (`set_unlock_key`/`unlock_key`/`pin` independence); the startup state
  machine's branch table (identity present → `Unlock`; unknown identity → await
  status / auto-bind) driven by synthetic messages.
- **Integration:** a daemon advertises its identity on connect on every transport;
  an auto-unlock with a stored identity key succeeds; an unknown identity
  auto-binds a fresh daemon; two daemons on one host do not collide.
- **Regression:** the disk-path invariant — a test that moves/renames the socket
  path and asserts the unlock key still resolves (the exact bug this fixes); and
  the daemon-smoke/base-dir path if it exercises the keystore.

## 7. Risks & mitigations

| Risk | Mitigation |
|---|---|
| The identity frame reorders the startup message stream (the subscribe-time `Keystore` push already surprised one strict test once) | Send `ServerIdentity` deterministically as the *first* frame; update strict readers in `tests/it/` to expect it, as the `Keystore` push already required. |
| `Unlock` is sent before the identity arrives | Move the send into the identity handler; keep the "no key → await status" branch unchanged. |
| TCP behavior regresses | The identity frame is additive on TCP; the address-keyed pin path is untouched. |
| Embedded daemon has no transport key | Keep the `"embedded"` literal, or generate a keypair for it in P2 (open question 3). |
| Store split breaks the tolerant-load contract | Keep load tolerant (missing/corrupt → empty + warn) and write strict (locked whole-file rewrite). |
| Beta stores mis-keyed after the change | Backwards compat is explicitly out of scope; optionally a one-time best-effort relabel of existing unix entries (see 9.4). |

## 8. Out of scope

- Preserving or migrating existing `known_servers.toml` entries (beta only).
- Authenticating the unix transport (Noise over the local socket).
- Anything in the transport handshake / TCP trust pinning.
- Rotating or expiring unlock keys, or multi-key keystores.

## 9. Open questions

1. **Frame shape:** a dedicated `ServerIdentity` frame, or ride the existing
   subscribe-time `DaemonMessage::Keystore` push with an added identity field?
   (A dedicated frame keeps "status" and "identity" separate; the push is one fewer
   round-trip but conflates concerns.)
2. **Identity source:** transport public key, or a dedicated persisted
   `instance_id` (UUID)? The transport key is free and already the system-wide
   identity, but a dedicated id survives transport-key regeneration.
3. **Embedded daemon:** keep the `"embedded"` literal, or generate a transport
   keypair for it so it keys uniformly?
4. **Beta relabel:** do a one-time best-effort rewrite of existing unix entries
   (`addr = "/…/choreographr.sock"` → the first identity seen at that path), or
   leave beta testers to re-bind?
5. **Store layout:** separate `[[pin]]` + `[keystore]` tables in one file, or two
   files? One file keeps the advisory-lock discipline in one place.

## 10. Definition of done

- A daemon advertises its transport identity as the first frame on every
  connection; the client keys the per-daemon unlock key by that identity and
  keeps the address→key pin address-keyed.
- Moving the default socket path (or migrating under `--base-dir`) no longer
  affects unlock-key resolution — pinned by a regression test.
- `ARCHITECTURE.md` and `README.md` describe identity-keying and the wire frame;
  the misleading `migrate.rs` rustdoc claim is corrected; `just pre-commit` green;
  release notes written from the commit messages.
- **This plan document is deleted** once everything above is implemented, and no
  implementation doc references it (grep for `keystore-identity-keying` returns
  nothing).
