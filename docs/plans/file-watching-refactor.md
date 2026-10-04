# Plan: unified file watching and hot-reload

**Status:** proposed — design complete; nothing implemented (no source changes in this
change).
**Lifecycle:** this file is **deleted once the plan is fully implemented**. Nothing
written during implementation may reference it — rustdoc, `ARCHITECTURE.md`,
`README.md`, release notes, and commit messages must stand on their own, because a
reference to this plan would go stale the moment it is removed.
**Date:** 2026-10-04
**Targets:** `choreo-daemon` (the `config_watch` transport, its consumers, and the
daemon-core wiring); `ARCHITECTURE.md`/`README.md` for the documented behavior.
**Touches:** `choreo-daemon/src/config_watch.rs` (generalized),
`src/server/core.rs` (wiring), `src/catalog.rs`, `src/accounts/mod.rs`,
`src/server/acl.rs`, `src/mcp/{mod,config}.rs`, `src/daemon.rs` (command loop +
`DaemonCommand`), `src/sessions.rs` (skills invalidation), `tests/it/config_watch.rs`,
`tests/it/acl_hot_reload.rs`, new integration tests, `ARCHITECTURE.md`, `README.md`.

> **TL;DR.** The daemon already has a careful watching transport
> (`config_watch::ConfigWatcher`: one `notify` watcher, synchronous arm, overflow
> rescan, re-arm, non-blocking fan-out) — but its API is **one directory + one
> basename**, so the ACL already needs a *second* watcher instance, the MCP
> project-layer config cannot be watched at all, and skills never refresh. Refactor
> it into a single multi-root `WatchService` (one `notify` instance, per-root
> recursive/non-recursive modes, **opportunistic** roots that are watched when they
> appear rather than created, full-path subscriptions, a shared content-fingerprint
> reload gate), then wire the three consumers that need it: MCP `mcp_servers.json`
> (user + project layers), skills invalidation, and keep context files on their
> deliberate per-iteration re-read. Behavior-preserving migration first; new
> consumers second.

---

## Table of contents

1. [Current state](#1-current-state)
2. [What should hot-reload](#2-what-should-hot-reload)
3. [Design](#3-design)
4. [Migration phases](#4-migration-phases)
5. [Testing strategy](#5-testing-strategy)
6. [Risks & mitigations](#6-risks--mitigations)
7. [Out of scope](#7-out-of-scope)
8. [Open questions](#8-open-questions)
9. [Definition of done](#9-definition-of-done)

---

## 1. Current state

### 1.1 Inventory

| File(s) | Mechanism today | Consumer + reload policy |
|---|---|---|
| `models-overlay.toml` (`config_dir`) | `ConfigWatcher` (config dir, non-recursive, basename) | catalog-maintenance thread; re-read + **content fingerprint** gate; forwards `DaemonCommand::CatalogBaseChanged` |
| `accounts.toml` (`config_dir`) | same instance | `spawn_accounts_watcher` thread (drains bursts → one signal); forwards `DaemonCommand::AccountsReload`; **parse-compare** in the command loop |
| `authorized_clients.toml` | **a second `ConfigWatcher` instance** on the file's own parent dir | `spawn_acl_watcher` thread; forwards `DaemonCommand::AclReload`; **parse-compare** inside `SharedAcl` (garbage/removed file keeps current keys) |
| `mcp_servers.json` (user) | **not watched** | explicit reload only: `ClientMessage::McpReload` (commit `850cb4e`), `/mcp reload`, `choreographr mcp reload` |
| `<base_dir or cwd>/.choreographr/mcp_servers.json` (project) | **not watched** and unreachable by the transport (one directory) | same explicit command |
| skills `~/.agents/skills`, `<dir>/.agents/skills` chain | **not watched** | none: session-scoped snapshot (`SessionState.discovered_skills`), invalidated only by `set_working_dir`; `load_skill` reads the body fresh |
| `AGENTS.md` / context files (config-dir, `~/.agents`, `~/.claude`, project chain) | **not watched, by design** | re-read + fingerprinted on **every agent-loop iteration** (`build_system_content` → `discover_context`); new subdirectories inject hints as tools touch them |
| `config.toml` | not watched | loaded once at startup (max turns, cache-warming); changes need a restart |
| catalog cache bin (`data_dir`) | not watched | network refresh thread (models.dev), not a user-edited file |

### 1.2 Transport limitations

`ConfigWatcher` is deliberately transport-only (it never reads files; consumers own
policy) and already handles the hard parts: synchronous initial arm (no startup
race), `IN_Q_OVERFLOW` → rescan-and-replay of divergences, re-arm after the watched
directory is removed, `Access`/`Other` noise filtering, non-blocking `try_send`
delivery. What it cannot do:

1. **One directory per instance.** `ConfigWatcher::new(dir)` + `subscribe(basename)`
   fixes both the directory and the key. Any file outside that directory needs a
   second instance — the ACL is the existing precedent ("the ACL path is a
   parameter, not a fixed config-dir member"), and the module's own docs argue a
   single transport should exist.
2. **No full-path subscriptions.** Two roots can hold the same basename; the
   subscriber map is keyed by basename, so routing cannot distinguish them.
3. **Non-recursive only.** A nested layout (`<root>/.agents/skills/<skill>/SKILL.md`)
   cannot be watched; recursion is required for skill directories.
4. **Roots must exist.** `spawn()` creates the directory (correct for the config
   dir, wrong for `~/.agents` or a project's `.choreographr` — a daemon must not
   create those as a side effect of watching).
5. **No shared reload gate.** The overlay has a content-fingerprint gate; accounts
   and ACL parse-compare; the MCP consumer would be a third variation of the same
   "re-read, decide if it actually changed, then act" pattern.

## 2. What should hot-reload

The refactor settles every user-editable config file explicitly:

| File | After this plan |
|---|---|
| `models-overlay.toml`, `accounts.toml`, `authorized_clients.toml` | Unchanged behavior; migrated onto the shared service (ACL's second instance retired). |
| `mcp_servers.json` (user) | **Auto-reload** — the follow-up from the MCP reload work (`850cb4e`): subscribe the config-dir root; a change forwards the reload to the command loop; malformed files warn and keep the running set. |
| `<base or cwd>/.choreographr/mcp_servers.json` (project) | **Auto-reload** — an opportunistic root (watched when present, never created). This closes the MCP follow-up's project-layer question. |
| skills directories | **Invalidation, not injection**: a change clears each session's cached `discovered_skills`; the next request re-discovers. Global `~/.agents/skills` plus `<base or cwd>/.agents/skills` (both recursive, opportunistic). |
| `AGENTS.md` / context files | **Deliberately not watched.** The per-iteration re-read is cheaper than watcher-driven invalidation for per-session working directories (which are unbounded) and already delivers edits on the next turn. Do not regress it. |
| `config.toml` | **Not watched.** Startup-only by contract (bind addresses, thread counts, cache-warming defaults); changing it needs a restart. Revisit only if a hot-reloadable subset emerges. |

## 3. Design

### 3.1 `WatchService` — the generalized transport

Same module (`config_watch.rs`), same "transport only, no policy" split, generalized
shape:

```rust
pub struct WatchRoot { dir: PathBuf, mode: RecursiveMode, create: bool }

pub struct WatchService { roots: Vec<WatchRoot>, subscribers: ... }

impl WatchService {
    pub fn new() -> Self;
    /// Non-recursive, created-if-missing (the config dir).
    pub fn add_root(&mut self, dir: PathBuf, mode: RootMode);
    /// Recursive/opportunistic roots: never created; watched once present.
    pub fn add_optional_root(&mut self, dir: PathBuf, mode: RootMode);
    /// Subscribe by absolute path; the path's root directory must have been added.
    pub fn subscribe(&mut self, path: PathBuf) -> Receiver<ConfigChange>;
    /// Convenience: `subscribe(dir.join(basename))`.
    pub fn subscribe_in(&mut self, dir: &Path, basename: &str) -> Receiver<ConfigChange>;
    pub fn spawn(self);
}
```

- **One `notify::RecommendedWatcher`**, multiple `watch()` calls (notify supports
  this); one transport thread; one raw-event channel.
- **Routing by full path**: subscribers keyed by absolute path; an event path is
  matched against subscribed paths under the root that produced it (parent-dir
  equality for non-recursive roots, `starts_with` for recursive ones). Overflow
  rescan and the last-known-content view become **per root**.
- **Opportunistic roots**: `create: false` roots are retried on the existing re-arm
  cadence (5 s) until they appear; log at debug while unarmed, once at info when
  armed. This is what lets `<base>/.choreographr` and `~/.agents/skills` join later
  without side-effect creation and without a restart.
- **Preserved semantics**: synchronous initial arm before the thread starts; coarse
  `ChangeKind` (create/modify/remove); `Access`/`Other` stripping; overflow rescan
  with idempotent replay; re-arm when a watched directory is removed; non-blocking
  fan-out. These are pinned by existing tests and must stay pinned.
- **Path resolution** stays in `choreo_shared::paths` (`config_dir`, `base_dir`) and
  the existing test overrides (`set_test_config_root`, the MCP project-root
  override) continue to work, so integration tests remain hermetic.

### 3.2 Shared reload gate

Add a small, pure helper next to the transport:

```rust
/// Content-fingerprint gate: `changed()` re-reads the file and reports whether it
/// differs from the last value this gate observed (absent = `None`).
pub struct ContentGate { last: Option<Option<Vec<u8>>> }
impl ContentGate { pub fn changed(&mut self, path: &Path) -> bool }
```

Consumers the transport wakes use it to collapse editor save storms
deterministically (no timers): the first event of a burst opens the gate, every
later event in the same burst sees identical content and no-ops. The overlay's
existing fingerprint logic becomes this helper; the MCP consumer uses it too.
Accounts and ACL keep their semantic parse-compare (they must distinguish "file
changed" from "meaningfully changed"); the gate is an optional pre-filter there,
not a replacement.

### 3.3 Consumer wiring

- **MCP**: subscribe the user file on the config-dir root and the project file on
  the opportunistic `.choreographr` root. The consumer thread coalesces a burst
  (drain, like the accounts watcher), then sends the reload request to the command
  loop — reusing `McpManager::reload`'s reconcile, so an unchanged config is a
  no-op and live connections are kept. A malformed config logs a warning and keeps
  the current server set (the explicit command remains the path that reports
  errors to the user). The current `DaemonCommand::McpReload { reply }` shape
  needs either a reply-less sibling variant or a consumer that drops/logs the
  reply; decide in P2.
- **Skills**: subscribe the global skills root and the opportunistic project
  skills root, both recursive. On a change, send a new `DaemonCommand::SkillsChanged`;
  the command loop invalidates every session's `discovered_skills` so the next
  request re-discovers. Because a request worker merges its snapshot back on
  `RequestFinished`, carry a small generation counter (or clear on the next request
  boundary) so an invalidation that races an in-flight request is not lost.
  Per-session working directories outside `<base or cwd>` remain snapshot-scoped
  (open question 3).
- **ACL**: replace its dedicated `ConfigWatcher` instance with a root at the ACL
  file's parent directory (still `create: false` — the ACL path is a parameter and
  its directory should be allowed to be absent). `tests/it/acl_hot_reload.rs` is the
  regression.
- **Overlay/accounts**: moved onto the service with no behavior change.

## 4. Migration phases

Each phase is independently shippable, lands with tests + docs, and keeps
`just pre-commit` green.

### P1 — Generalize the transport; migrate existing consumers

- Multi-root `WatchService` (one watcher, per-root modes, opportunistic roots,
  full-path subscriptions) with the preserved semantics above.
- Migrate overlay, accounts, and ACL onto it; delete the ACL's second instance.
- Extend the unit tests (routing per root, recursive matching, opportunistic re-arm
  decision, per-root rescan) and the integration suite (multi-root delivery,
  recursive nested event, root appearing after startup).
- Update the `config_watch.rs` rustdoc and ARCHITECTURE's row.

### P2 — MCP auto-reload (user + project)

- Wire the MCP consumer per §3.3; add the reply-less reload path if needed.
- Tests: edit the user file → a server is added/removed/restarted without a
  restart; edit the project file → same; malformed file → running set unchanged;
  save storm → one reload.
- This completes the MCP configuration follow-up (the watcher subscription and the
  project-layer question) — update the MCP plan's follow-up entry and open question
  when this lands, and note it in the release-note commit.

### P3 — Skills invalidation

- Global + project skills roots (recursive, opportunistic), `SkillsChanged`
  command, session cache invalidation with the generation guard.
- Tests: touch `SKILL.md` → next request sees the new skill; a skill added while a
  session is idle is loadable on the next request; working-dir scoped sessions
  re-discover.

### P4 — Documentation and observability

- ARCHITECTURE: rewrite the `config_watch.rs`/`server/core.rs` rows for the service
  shape; document the reload matrix of §2 (what reloads, when, and what needs a
  restart) in README's config section.
- Log armed/unarmed roots once at startup (info) and expose the watched-root list
  in the daemon log; no new UI.
- Release notes from the commit messages.

## 5. Testing strategy

Per AGENTS.md: unit tests in `src/` are wait-free; filesystem/thread integration
tests live in `tests/it/` with the existing bounded-poll pattern
(`tests/it/config_watch.rs` — 30 s deadline, 10 ms poll, tolerating platform
noise).

- **Unit (wait-free)**: multi-root routing, recursive matching, opportunistic
  re-arm decision, per-root overflow rescan, `ContentGate`, burst coalescing.
- **Integration**: multi-root delivery; a recursive nested change; a root created
  after startup; editor atomic-save (temp + rename) storms; the existing
  `config_watch.rs` and `acl_hot_reload.rs` suites stay green unmodified in P1
  (behavior parity), extended for new roots.
- **Daemon end-to-end**: project `mcp_servers.json` edit adds a server without a
  restart; skills change is visible on the next request; malformed configs keep
  state.
- **Mobile**: verify the embedded daemon (iOS/Android) with `config_watchers: true`
  — notify's kqueue/inotify availability — and that the no-op path (flag false,
  unresolvable dir) degrades exactly as today.

## 6. Risks & mitigations

| Risk | Mitigation |
|---|---|
| Recursive watches on large trees (inotify watch count) | Scope strictly to `.agents/skills` and `.choreographr` — never project roots or home directories wholesale. |
| Editors' non-atomic saves firing bursts | `ContentGate` fingerprint gate + the existing per-consumer coalescing/parse-compare. |
| Watching a root that appears later | Opportunistic roots retried on the re-arm cadence; debug-level while unarmed. |
| `--base-dir` / test overrides changing paths | Resolve every root through `choreo_shared::paths` at wiring time; keep the existing test overrides and hermetic integration tests. |
| Skills invalidation racing an in-flight request | Generation counter (or next-request-boundary clear) so the merge-back cannot un-invalidate a fresh change. |
| Behavior drift while migrating ACL | Keep `acl_hot_reload.rs` as an unmodified regression test through P1. |
| macOS FSEvents extra events; Windows/mobile semantics | The existing integration tests already tolerate platform noise; reuse that harness for new roots. |

## 7. Out of scope

- Watching `config.toml` (startup-only by contract).
- Watching context files/`AGENTS.md` (the per-iteration re-read is the design).
- Per-session project skill directories beyond `<base or cwd>` (open question 3).
- Watching the data directory or the catalog cache bin.
- Any new user-facing watch-status UI.

## 8. Open questions

1. Rename the types (`ConfigWatcher` → `WatchService`, `ConfigChange` →
   `WatchEvent`), or keep the names to minimize churn?
2. Should accounts/ACL adopt the `ContentGate` pre-filter too, or keep parse-compare
   only?
3. Per-session skills: is the `<base or cwd>` scope acceptable, or should sessions
   register their working-dir skills root with the service (unbounded roots — not
   recommended)?
4. Should MCP auto-reload be always-on, or opt-in per server (a config key) given
   that a save can start/restart subprocesses?

## 9. Definition of done

- One watcher instance serves every config file; the ACL's dedicated instance is
  gone; subscriptions are per-path, with per-root recursive/non-recursive modes and
  opportunistic roots.
- The reload matrix of §2 holds end-to-end, with tests: overlay/accounts/ACL
  unchanged, MCP user+project auto-reload, skills invalidation, context files
  unchanged by design.
- Malformed/partial writes never mutate state; save storms collapse to one reload.
- `ARCHITECTURE.md` and `README.md` document the matrix; `just pre-commit` green;
  release notes written from the commit messages.
- **This plan document is deleted** once everything above is implemented, and no
  implementation doc references it (grep for `file-watching-refactor` returns
  nothing).
