# Agent Instructions

## Choreographr Coordination Platform (`choreo-content`)

The `choreo-content` crate implements the feature-gated `content` tool group (the
Choreographr Coordination Platform blockchain content registry). The group is
compiled only behind the daemon's `content` cargo feature (off by default;
enable with `--features content`) — a plain build contains no `coord`-era
group at all, and persisted sessions carrying the stale pre-rename `coord`
group name silently ignore it. It owns a
tokio **sidecar runtime** used **only** to drive `subxt` for signed chain
writes; the daemon itself stays thread-only and calls the crate's blocking
`execute_*` functions. IPFS (`ureq`) and the indexer (`tungstenite`) are
synchronous and never touch the sidecar.

The Polkadot account credential (`ServiceCredential::Substrate`) lives in
`choreo-keystore`; the TUI imports a Polkadot-JS keystore export client-side and
sends it over the existing `AddCredential` path.

**Temporary credential plumbing**: the content write tools currently receive the
daemon's single Substrate credential through the `Tool` trait's one
`x_credentials` slot (see `// TEMPORARY` comments in `requests.rs` and
`sessions.rs`). This single-slot reuse is a stopgap until a proper
tool→keystore credential-access system replaces it; do not rely on it
remaining as the permanent mechanism.

## Refactoring

Always try to refactor when implementing new features. Look for opportunities to improve code structure, reduce duplication, and simplify existing code alongside any additions.

## Documentation

When making changes, ensure [ARCHITECTURE.md](./ARCHITECTURE.md) and [README.md](./README.md) are kept up to date. If a change affects the architectural decisions, module structure, data flow, or any other documented aspect, update the files accordingly.

Every non-trivial change (new features, fixes, refactors, dependency updates, behavior changes — anything a reviewer would mention in a commit summary) must also get an entry in [CHANGELOG.md](./CHANGELOG.md) under the `## [Unreleased]` section. Only trivial changes (typo fixes, comment-only edits, test-only tweaks) may skip it.

### CHANGELOG.md

- **One heading per category, at most.** `[Unreleased]` is organized with the Keep a Changelog category headings — `### Added`, `### Changed`, `### Deprecated`, `### Removed`, `### Fixed`, `### Security` — each appearing **once at most**. Append a new bullet under the matching existing heading; never open a second `### Changed` (or any other) block. Include only the categories that apply — do not add an empty one. A section that repeats a category heading is malformed, not just untidy. The `just check-changelog` guard (scripts/check-changelog.sh) — also run by `just pre-commit` and the release workflow before extraction — enforces this: it fails on a repeated or unknown `### ` category heading, or an empty category block, in ANY `## [...]` section.
- **Write it for the release page.** At tag time the CI `release` job copies the entire `## [X.Y.Z]` section (heading stripped) verbatim into the GitHub release body, followed by the auto-generated commit notes (see [RELEASE.md](./RELEASE.md) Phase 1). The file is user-facing prose, not a scratchpad: no TODOs, no internal scaffolding, no "see commit …".
- **Promotion at release.** Phase 1 renames `## [Unreleased]` to `## [X.Y.Z] - YYYY-MM-DD` and starts a fresh, empty `[Unreleased]` above it (moving the compare link so `[Unreleased]` points at `HEAD` again). Once a series is named, the heading always carries that dance-style name in parentheses — `## [X.Y.Z] - YYYY-MM-DD (Name)` — where a **major/minor** release sets a new name and a **patch** keeps the current one (the pre-name 0.1.0 heading has none). The machine source of truth for that name is `choreo-shared/release-name.txt` (a single line; edited on major/minor only), which is compiled into the binaries and read by the CI release job for the release title. The `just check-release-name` guard — also run by the release workflow — fails the release if the file and the heading drift apart. The extraction accepts the dated, undated, and named heading forms.

## Test Discipline

- **Unit tests** (in <code>src/</code> <code>#[cfg(test)]</code> modules) must never use time-based waits (`sleep`, `delay_for`, etc.). Use deterministic patterns only.
- **Integration tests** (tests that bind network sockets, spawn external processes, use `UnixStream::pair()` to exercise the full handler pipeline, or perform filesystem I/O exercising the system boundary) belong in crate-level `tests/` directories, not in `src/`.
- Integration tests are marked <code>#[ignore]</code>. Use the nextest aliases defined in <code>.cargo/config.toml</code>: <code>cargo test-fast</code> (unit tests), <code>cargo test-integration</code> (the <code>#[ignore]</code> suite), and <code>cargo test-all</code> (everything in one pass). Plain <code>cargo test</code> runs libtest (serialized) and is only a fallback when nextest is unavailable.

## Task Execution

When implementing a list of code changes across multiple files, delegate each task to a subsession and run them in series (one at a time), not in parallel. This avoids filesystem conflicts from concurrent edits to overlapping files and keeps each subsession's context focused. (There is no worktree-per-branch support yet, so subsessions share one working tree — hence the serial execution.)

During development a subsession may iterate against just the crates it changed with `cargo nextest run -p <crates>` (the `cargo test-*` aliases bake in `--workspace` and reject `-p`, so call nextest directly — or use `just test-crate <crate>`). That is for fast feedback only. **Before returning its report, a subsession must run the full [Commit Workflow](#commit-workflow) gate and commit its work**; a scoped `-p` run is never sufficient to commit from. A subsession that cannot complete the work must not commit and must leave its changes in the tree for the parent to inspect (see [Commit Workflow → Sub-sessions](#sub-sessions)).

## Dependency Management

Always use the latest stable version of crates where possible. When adding or upgrading a dependency:

1. Use the latest stable semver-compatible release for each crate (check `cargo search <name> --limit 1` for the current version).
2. If a dependency is locked to an older version upstream, accept the duplication rather than patching — upstream issues should resolve naturally over time.
3. If a dependency is used by two or more workspace members, declare it in `[workspace.dependencies]` and reference it with `dep.workspace = true` in member crates. This is not optional — when adding a crate-level dependency that already exists (or is being introduced simultaneously) in another workspace member, promote it to the workspace and update both crates in the same change.
4. Use [`itertools`](https://docs.rs/itertools) where it will genuinely improve code quality — e.g. `.format()` for joining display values, `.sorted()`, `.dedup()`, `.partition_map()`, or `process_results()` replacing awkward manual loops or `Result`-yielding iterator plumbing. Do not adopt it for its own sake; add it (declared in `[workspace.dependencies]`) alongside the concrete change that warrants it.

## Testing New Code

Always write unit and/or integration tests for any new code added to the codebase. Unit tests belong in `src/` `#[cfg(test)]` modules; integration tests belong in crate-level `tests/` directories. Follow the conventions in the **Test Discipline** section above.

## Error Handling

Never use `expect()`, `unwrap()`, or `panic!()` in production code. These create crash surfaces that can take down the daemon. Follow these rules:

1. **Library crates** — define structured error types with `thiserror` and propagate errors with `?`.
2. **Binary crates** — use `anyhow::Context` / `.context()` to attach meaningful context to errors at key boundaries, then propagate with `?`.
3. **Infallible operations** — if an operation truly cannot fail, use `unwrap_or_default()` or `unwrap_or(fallback)` rather than bare `unwrap()`.
4. **Mutex poisoning** — use `.lock().unwrap_or_else(|e| e.into_inner())` to recover from poisoned mutexes instead of panicking.
5. **`unwrap()`/`expect()`/`panic!()` are permitted only in `#[cfg(test)]` modules and `tests/` integration test files.**

## Logging

All crates in the workspace (`choreographr`, `choreo-client-core`, `choreo-keystore`, `choreo-im`, `choreo-gui`, `choreo-markdown`, `choreo-proto`, `choreo-tui`) must log extensively using the `tracing` crate. Every module should emit `tracing` events (`info!`, `warn!`, `error!`, `debug!`, `trace!`) at appropriate levels to provide observability into key operations, state transitions, and error conditions.

In the `choreo-tui` crate specifically, do not use `eprintln!` for diagnostics — output goes to the per-process log file `$TMPDIR/choreo-tui-<pid>.log` (`std::env::temp_dir()`, so it works on Termux/Android too). A failure to create that log file must degrade to no logging, never abort the TUI.

## Thread Communication

Do not share mutable state between threads. Use message passing (`mpsc` channels) for all cross-thread communication. Shared-state patterns (`Arc<RwLock<…>>`, `Arc<Mutex<…>>`) should be avoided in favor of channel-based designs.

Five sanctioned exceptions, each single-purpose, lock-free or minimally scoped, and documented in code and in ARCHITECTURE.md (historical numbering; the list now has seven entries):

1. **Cooperative cancellation flags** (`Arc<AtomicBool>`, e.g. `ToolContext.cancelled`). A blocking tool call cannot be interrupted by a channel message, so a tiny lock-free flag is used as a best-effort stop hint for work that consults it. Keep such flags single-bit (carry no data), document each use in code, and route all control flow — results, cancellation events, kills, streaming — over channels.
2. **The Noise transport state** (`choreo-transport`'s `Arc<Mutex<TransportState>>` on `NoiseStream`, plus its `Arc<AtomicBool>` single-writer guard). An encrypted duplex stream is cloned via `try_clone` across the reader and writer threads of one connection, which must interleave encrypt/decrypt against the same snow `TransportState` — so the state has to be shared, not channeled. The lock is held only per chunk, never across blocking socket I/O (that scope is what prevents the bidirectional large-message deadlock), and the guard is a single-bit flag in the spirit of exception 1. The full rationale lives in ARCHITECTURE.md's `noise.rs` module row.
3. **The daemon's live-connection counter** (`choreo-daemon`'s `Arc<AtomicUsize>` on `server/lifecycle.rs`, held by the RAII `ConnectionSlot`). The concurrent-connection cap (`MAX_CONCURRENT_CONNECTIONS`) is enforced atomically across the two accept paths (Unix main thread + TCP accept thread) and decremented from every connection thread's exit — a channel cannot express that without a dedicated accounting thread, so a single lock-free counter is shared instead. It carries no protocol data: a bookkeeping count whose only role is to bound resource accumulation, and the RAII slot releases it on panic too. The rationale lives in ARCHITECTURE.md's `server/lifecycle.rs` module row.
4. **The provider catalog `ArcSwap`** (`choreo-ai-protocols`'s `PROVIDER_CATALOG`, a `LazyLock<ArcSwap<Vec<ProviderEntry>>>`). Readers are lock-free and the swap is an atomic `store()`, but there is a strict **single-writer invariant** — only the daemon command loop calls `replace_catalog` (after a catalog refresh, overlay change, or `/refresh-models`); every change *request* still travels by channel, and only the atomic store mutates the catalog. It carries no per-message data: a process-wide immutable snapshot that is atomically replaced wholesale. The rationale lives in ARCHITECTURE.md's `catalog/` module row.
5. **The Windows Job Object kill-switch** (`choreo-daemon`'s `Arc<ChildJob>` on `tools/shell_util.rs`, plus the `ProcessIsAlive` process-handle copy). A Windows shell-tool child is assigned to a Job Object so a timeout (or the last handle closing) can terminate the whole process tree. A `HANDLE` is an index into the kernel handle table, not a pointer into our address space: every Job Object operation (Assign/Terminate/Close) is thread-safe kernel-side, so the watchdog and drain threads can share one `Arc<ChildJob>` — the Rust value is immutable and the handle is closed exactly once, by `Drop` on the last owner (`ChildJob` has no `Clone`). `ProcessIsAlive` shares a *copy* of the std `Child`'s process handle so the watchdog can distinguish "still running" from "already exited" at timeout time (the Windows analogue of the Unix pidfd/ESRCH check); it is valid until the `Child` is dropped, and the watchdog is always joined before that. The rationale lives in ARCHITECTURE.md's `tools/shell_util.rs` row.
6. **The delivery-lag byte counters** (`choreo-daemon`'s `broadcast::SubscriberSink.bytes_in_flight` — one `Arc<AtomicUsize>` per subscriber queue — plus the daemon-wide `global_lag` total). The lossless delivery design gives every client an unbounded channel and bounds memory by evicting clients whose in-flight bytes cross a threshold; a queue's byte backlog is inherently shared state, because the producers that increment it on enqueue and the connection writer thread that decrements it on dequeue run on different threads and must both touch the same running total — a channel cannot express that without a dedicated accounting thread. Lock-free, single-purpose, carries no protocol data (a bookkeeping count used only to bound per-client memory). The rationale lives in ARCHITECTURE.md's `broadcast.rs` module row.
7. **The provider-socket registries** (`choreo-sockreg`'s `SocketRegistry` — one per unit of work: a per-session registry inside `SessionState` (cloned into `DaemonState::session_registries` on the command loop) plus one daemon-level registry for non-session-scoped fetches). Each registry's whole purpose is to reach into another thread's blocked provider `read()`: channels cannot interrupt a syscall, but `shutdown(fd, SHUT_RDWR)` makes the blocked read return immediately, so the cancel/suspend decision on the command loop can un-block wedged inference workers — session-scoped (`shutdown_all` on the target session's registry clone only, children included via `cancel_children_of`; other sessions untouched). The mutex is held only for a handful of fd syscalls and carries no message traffic; each clone handle is immutable plumbing (never mutated after creation), not a shared mutable channel replacement. The per-session split keeps cancellation granularity exactly one session: a cancel can no longer disturb unrelated sessions' in-flight connections. The rationale lives in ARCHITECTURE.md's `choreo-sockreg` section.

### Channel selection (crossbeam vs std vs async)

Thread-to-thread messaging must use **`crossbeam_channel`**, not `std::sync::mpsc`, in ALL crates of the workspace — not only those that already depend on it. Crossbeam is a strict superset for our needs: cloneable receivers (true MPMC), `select!`/`select_biased!` (including send arms and timer channels), blocking iterators, and a consistent error taxonomy. std `mpsc` cannot join a `select!` later or add a second receiver without a redesign, so defaulting to crossbeam everywhere keeps future evolution a one-line change no matter which crate the code lives in (a std-`mpsc` channel that later needs crossbeam forces a workspace dependency promotion mid-feature; a crossbeam channel from day one never does). If a crate does not yet declare `crossbeam-channel`, add it (`crossbeam-channel.workspace = true`, promoted to `[workspace.dependencies]` when shared) in the same change that introduces its first cross-thread channel. This is a consistency/future-proofing rule, not a performance rule — our channels are control-plane and human-rate, so channel throughput is never the bottleneck (see also the crossfire evaluation: crossbeam stays; crossfire is rejected codebase-wide).

Apply it as follows:

- **New code** — any crate: always `crossbeam_channel`.
- **Existing std `mpsc`**: convert opportunistically, when the change being made would otherwise strain single-consumer semantics (session event streams, per-request replies that might get tapped by metrics/tracing later), or the moment the channel needs `select!`, a cloned receiver, or a select-with-timeout — regardless of which crate the code lives in. Do not migrate one-shot reply/flag channels or the existing test-site constructions for their own sake — that is churn with no behavioral payoff.
- **Async code**: keep the runtime's own channels (`futures_channel::mpsc` in `choreo-gui`, tokio channels in the daemon's sidecar runtime). Never call a blocking `recv()` (std or crossbeam) inside an async task — bridge across the async/thread boundary with the crossfire-style `From` conversions only if a measured need appears; the default is to keep blocking `recv()` on dedicated threads.

## Inline Comments

Always write inline comments around new code explaining how it works. Focus on the "why" — the reasoning, intent, and non-obvious details — rather than restating what the code literally does.

## Commit Workflow

Finishing an implementation run means the work is **not done until it is committed**. When a run of implementation turns completes, verify and commit it yourself, in the same run. Do not stop to ask the user whether to commit — the commit is part of the task, not a separate approval step.

A run is committed **once, at the end of each unit of work** — not once per turn. When a run delegates to subsessions, each completed subsession commits its own unit before returning (see [Task Execution](#task-execution)); the parent then commits whatever remains when its own run ends.

### The gate

Run **`just pre-commit`**. It is the commit gate, and it is safe to re-run: loop it (fix by hand, re-run) until it passes green, then commit. It runs, in mutation-aware order (the flags themselves live in `.cargo/config.toml`):

1. **`clippy-fix`** — `cargo clippy --fix --allow-dirty --allow-staged --workspace --all-targets --all-features --locked` auto-applies machine-applicable lints. It will not fix everything; whatever remains is hand-fixed in step 2.
2. **`clippy-strict`** — the same invocation plus `-- -D warnings`, which must report **nothing**. Fix pre-existing warnings too, not just the ones the change introduced — the gate is a clean workspace, not a clean diff.
3. **`test-all`** — the full unit + integration suite via nextest, all features and all targets (see [Test Discipline](#test-discipline)). It must pass in full.
4. **`fmt`** — `cargo fmt --all`, applied **last**: fmt is semantics-preserving, so the formatted bytes are behaviourally identical to the tested bytes and need no re-test. (Formatting runs last, not first, precisely because `clippy-fix` mutates the tree — a non-mutating CI check would instead run `fmt --check` first.)
5. **`check-changelog`** — the `[Unreleased]` structure guard (see [Documentation](#documentation)).

If any step fails, fix the cause and re-run `just pre-commit` from the top — a hand fix can introduce a new clippy warning or test failure, so the gate only holds when the whole sequence is clean in one pass. If clippy, formatting, or the tests genuinely cannot be made to pass, **do not commit**; stop and report the failure instead.

### Documentation and changelog (part of the run, before committing)

Update `CHANGELOG.md` under `## [Unreleased]` (see [Documentation](#documentation)) and `ARCHITECTURE.md` / `README.md` if the change touches anything they describe.

### Commit

Stage with `git add`, then commit immediately with `git commit` and a message that meets the same standard as any hand-written commit, following **Conventional Commits**:

    <type>[optional scope][!]: <description>

    [optional body]

    [optional footer(s)]

`<type>` is one of `feat`, `fix`, `docs`, `style`, `refactor`, `perf`, `test`, `build`, `ci`, `chore`, `revert`; a trailing `!` (or a `BREAKING CHANGE:` footer) marks an incompatible change. Set the optional scope to the affected crate name (e.g. `fix(choreo-daemon): …`) and omit it for workspace-wide changes (root `Cargo.toml`, CI, docs). Use the body to explain the "why", not the "what". Only after the commit succeeds, report back with a summary and the commit hash.

### When *not* to commit

- The run produced no file changes (a question, a read-only investigation, a plan).
- Clippy, formatting, or `cargo test-all` cannot be made to pass. Do **not** commit a red tree — a broken commit poisons `git bisect`. Fix it; if you genuinely cannot, stop and report the failure.
- The changes span more than one logically independent unit — split them and commit each instead of bundling unrelated work.
- The tree contains unexpected or sensitive untracked content (credentials, stray files, large blobs) — surface it before staging.

### Sub-sessions

When work is delegated to a subsession, the subsession **runs this gate and commits its work before returning its report** — it does not hand back the report with the changes still uncommitted. If the subsession aborts before completion (an unrecoverable clippy/test failure, a cancelled or timed-out run, any other reason), it must leave its changes **uncommitted** in the working tree and say so in its report, so the parent can inspect the partial state and decide how to proceed. A subsession that cannot finish never commits a red or partial tree.

The `.githooks/pre-commit` hook has been removed; the `just pre-commit` recipe is the gate, and nothing runs it automatically — it is the agent's responsibility on every implementation run.
