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

### Rustdoc vs ARCHITECTURE.md

Documentation is split by scope, and each layer is canonical for its own kind of content:

- **In-source rustdoc** (`//!` module headers + `///` item docs) **owns the per-module API reference** — a module's purpose, its public items' contracts, local invariants, ownership/lifecycle, and portability notes. Anything that changes when exactly one crate changes belongs next to that crate's code.
- **[ARCHITECTURE.md](./ARCHITECTURE.md) owns the cross-cutting system view** — workspace topology and dependency graphs, the security model, design-decision rationale, the end-to-end data flow, the wire-format/version history, release & packaging, and the sanctioned shared-state exceptions. Anything that would need editing because two or more crates changed belongs in the markdown.

The migration to this split is incremental: a crate is **migrated** once every public item carries docs and its rustdoc is warning-free. Migrated crates carry `#![warn(missing_docs)]` at their crate root (enforced as an error by `clippy-strict`'s `-D warnings`) and are listed in `doc_crates` in the justfile so `doc-check` holds their rustdoc clean. Add a crate to both, in the same change that documents it.

### Release notes via commits

There is **no hand-maintained `CHANGELOG.md`** — the release notes are generated from the commit messages by [git-cliff](https://git-cliff.org) (`cliff.toml`, via `scripts/release-notes.sh`). The CI `release` job renders the pushed tag's section and uses it verbatim as the GitHub release body. **The commit message is therefore the release note**, and writing it is part of every non-trivial change (a new feature, fix, refactor, dependency update, or behavior change — anything a reviewer would mention in a commit summary). Trivial changes (typo fixes, comment-only edits, test-only tweaks) need nothing extra.

- **Write the commit for the release page.** The subject becomes the bullet (its Conventional `type(scope):` prefix is stripped) and the body becomes that bullet's indented paragraph: user-facing prose, not a scratchpad — no TODOs, no internal scaffolding, no "see commit …". One commit per release-note paragraph; avoid nested `- ` sub-bullets (render them as prose).
- **Types that appear on the release page:** `feat` → **Added**, `fix` → **Fixed**, `perf`/`refactor` → **Changed**, plus two deliberate non-standard types that map 1:1 to their Keep a Changelog headings because they cannot be inferred: **`remove`** → Removed and **`security`** → Security. Housekeeping types — `chore` (except `chore(deps)`, which stays under Changed), `docs`, `ci`, `test`, `style`, `build` — are omitted from the notes. The mapping lives in `cliff.toml`; keep this list and that file in sync.
- **Release metadata.** A major/minor release sets a new dance-style **name**, a patch keeps the current one — the machine source of truth is `choreo-shared/release-name.txt` (a single line, edited on major/minor only), compiled into the binaries and read by the CI release job for the release title. See [RELEASE.md](./RELEASE.md) Phase 1.

## Test Discipline

- **Unit tests** (in <code>src/</code> <code>#[cfg(test)]</code> modules) must never use time-based waits (`sleep`, `delay_for`, etc.). Use deterministic patterns only.
- **Integration tests** (tests that bind network sockets, spawn external processes, use `UnixStream::pair()` to exercise the full handler pipeline, or perform filesystem I/O exercising the system boundary) belong in crate-level `tests/` directories, not in `src/`.
- Integration tests are marked <code>#[ignore]</code>. Use the nextest aliases defined in <code>.cargo/config.toml</code>: <code>cargo test-fast</code> (unit tests), <code>cargo test-integration</code> (the <code>#[ignore]</code> suite), and <code>cargo test-all</code> (everything in one pass). Plain <code>cargo test</code> runs libtest (serialized) and is only a fallback when nextest is unavailable.
- **One integration-test binary per crate.** Keep a crate's integration suite in a single <code>tests/it/main.rs</code> target, with each former <code>tests/foo.rs</code> pulled in as a <code>mod foo;</code>. Cargo compiles every top-level <code>tests/*.rs</code> into its own binary that statically links the whole library, so an N-file suite costs N relinks — and N clippy re-checks — on every change to the library or its dependencies; the single <code>it</code> target collapses that fan-out to one compile + link. cargo-nextest still runs every test in its own process, so isolation is unchanged. Add a new integration test as <code>tests/it/&lt;name&gt;.rs</code> plus a <code>mod &lt;name&gt;;</code> line in <code>main.rs</code> — never as a top-level <code>tests/&lt;name&gt;.rs</code>. File-level <code>#![cfg(...)]</code>/<code>#![allow(...)]</code> attributes move with the file and now apply to its module; a module needing the shared harness declares <code>mod common;</code> once at the crate root (<code>main.rs</code>) and reaches it with <code>use crate::common;</code>.

## Task Execution

When implementing a list of code changes across multiple files, delegate each task to a subsession and run them in series (one at a time), not in parallel. This avoids filesystem conflicts from concurrent edits to overlapping files and keeps each subsession's context focused. (There is no worktree-per-branch support yet, so subsessions share one working tree — hence the serial execution.)

During development a subsession may iterate against just the crates it changed with `cargo nextest run -p <crates>` (the `cargo test-*` aliases bake in `--workspace` and reject `-p`, so call nextest directly — or use `just test-crate <crate>`). That is for fast feedback *while still editing* only — never a substitute or precursor gate: when the work is ready to verify, run `just pre-commit` directly (see [Commit Workflow](#commit-workflow)). **Before returning its report, a subsession must run the full [Commit Workflow](#commit-workflow) gate and commit its work**; a scoped `-p` run is never sufficient to commit from. A subsession that cannot complete the work must not commit and must leave its changes in the tree for the parent to inspect (see [Commit Workflow → Sub-sessions](#sub-sessions)).

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

In the `choreo-tui` crate specifically, do not use `eprintln!` for diagnostics — output goes to the per-process log file `$XDG_STATE_HOME/choreographr/tui-<pid>.log` (with a `std::env::temp_dir()` fallback where no XDG state dir exists, so it works on Termux/Android too). A failure to create that log file must degrade to no logging, never abort the TUI.

**Never log message payloads.** A log line must not carry a user prompt, an assistant response, tool arguments or output, a request body, or any secret (a bot token, an unlock key, a credential). Log the *id*, *length*, *kind tag*, or *count* instead. The protocol and event enums (`ClientMessage`, `DaemonMessage`, `SessionEvent`, `ToolCallEvent`, `BridgeEvent`, MCP server log data, …) derive `Debug` and several variants carry exactly this content, so **never `?`-format one of them into a log line or an error/bail string** — project a payload-free name/tag (`MessageKind`, `BridgeEvent::kind`) or a length. The same applies to `bail!`/`anyhow!` messages, which are printed and may be logged.

## Thread Communication

Do not share mutable state between threads. Use message-passing channels for all cross-thread communication. Shared-state patterns (`Arc<RwLock<…>>`, `Arc<Mutex<…>>`) should be avoided in favor of channel-based designs.

The sanctioned exceptions, each single-purpose, lock-free or minimally scoped, and documented in code and in ARCHITECTURE.md (historical numbering; the list now has twelve entries):

1. **Cooperative cancellation flags** (`Arc<AtomicBool>`, e.g. `ToolContext.cancelled`). A blocking tool call cannot be interrupted by a channel message, so a tiny lock-free flag is used as a best-effort stop hint for work that consults it. Keep such flags single-bit (carry no data), document each use in code, and route all control flow — results, cancellation events, kills, streaming — over channels.
2. **The Noise transport state** (`choreo-transport`'s `Arc<Mutex<TransportState>>` on `NoiseStream`, plus its `Arc<AtomicBool>` single-writer guard). An encrypted duplex stream is cloned via `try_clone` across the reader and writer threads of one connection, which must interleave encrypt/decrypt against the same snow `TransportState` — so the state has to be shared, not channeled. The lock is held only per chunk, never across blocking socket I/O (that scope is what prevents the bidirectional large-message deadlock), and the guard is a single-bit flag in the spirit of exception 1. The full rationale lives in ARCHITECTURE.md's `noise.rs` module row.
3. **The daemon's live-connection counter** (`choreo-daemon`'s `Arc<AtomicUsize>` on `server/lifecycle.rs`, held by the RAII `ConnectionSlot`). The concurrent-connection cap (`MAX_CONCURRENT_CONNECTIONS`) is enforced atomically across the two accept paths (Unix main thread + TCP accept thread) and decremented from every connection thread's exit — a channel cannot express that without a dedicated accounting thread, so a single lock-free counter is shared instead. It carries no protocol data: a bookkeeping count whose only role is to bound resource accumulation, and the RAII slot releases it on panic too. The rationale lives in ARCHITECTURE.md's `server/lifecycle.rs` module row.
4. **The single-writer `ArcSwap` snapshots** (the provider catalog — `choreo-ai-protocols`'s `PROVIDER_CATALOG`, a `LazyLock<ArcSwap<Vec<ProviderEntry>>>` — and the daemon's tool registry — `choreo-daemon`'s `DaemonState::tool_registry`, an `Arc<ArcSwap<ToolRegistry>>` shared with every session and request worker). Readers are lock-free and each swap is an atomic `store()`, but there is a strict **single-writer invariant** — only the daemon command loop calls `replace_catalog` (after a catalog refresh, overlay change, or `/refresh-models`) and only the command loop swaps the tool registry (on `DaemonCommand::McpListChanged`, after an MCP server reports a list change over its `subscriptions/listen` stream); every change *request* still travels by channel, and only the atomic store mutates either value. Neither carries per-message data: each is a process-wide immutable snapshot that is atomically replaced wholesale (the registry swap is what lets a live MCP list change reach in-flight sessions without a restart). The rationale lives in ARCHITECTURE.md's `catalog/` and `choreo-mcp` / `mcp/` module rows and its threading section.
5. **The Windows Job Object kill-switch** (`choreo-daemon`'s `Arc<ChildJob>` on `tools/shell_util.rs`, plus the `ProcessIsAlive` process-handle copy). A Windows shell-tool child is assigned to a Job Object so a timeout (or the last handle closing) can terminate the whole process tree. A `HANDLE` is an index into the kernel handle table, not a pointer into our address space: every Job Object operation (Assign/Terminate/Close) is thread-safe kernel-side, so the watchdog and drain threads can share one `Arc<ChildJob>` — the Rust value is immutable and the handle is closed exactly once, by `Drop` on the last owner (`ChildJob` has no `Clone`). `ProcessIsAlive` shares a *copy* of the std `Child`'s process handle so the watchdog can distinguish "still running" from "already exited" at timeout time (the Windows analogue of the Unix pidfd/ESRCH check); it is valid until the `Child` is dropped, and the watchdog is always joined before that. The rationale lives in ARCHITECTURE.md's `tools/shell_util.rs` row.
6. **The delivery-lag byte counters** (`choreo-daemon`'s `broadcast::SubscriberSink.bytes_in_flight` — one `Arc<AtomicUsize>` per subscriber queue — plus the daemon-wide `global_lag` total). The lossless delivery design gives every client an unbounded channel and bounds memory by evicting clients whose in-flight bytes cross a threshold; a queue's byte backlog is inherently shared state, because the producers that increment it on enqueue and the connection writer thread that decrements it on dequeue run on different threads and must both touch the same running total — a channel cannot express that without a dedicated accounting thread. Lock-free, single-purpose, carries no protocol data (a bookkeeping count used only to bound per-client memory). The rationale lives in ARCHITECTURE.md's `broadcast.rs` module row.
7. **The provider-socket registries** (`choreo-sockreg`'s `SocketRegistry` — one per unit of work: a per-session registry inside `SessionState` (cloned into `DaemonState::session_registries` on the command loop) plus one daemon-level registry for non-session-scoped fetches). Each registry's whole purpose is to reach into another thread's blocked provider `read()`: channels cannot interrupt a syscall, but `shutdown(fd, SHUT_RDWR)` makes the blocked read return immediately, so the cancel/suspend decision on the command loop can un-block wedged inference workers — session-scoped (`shutdown_all` on the target session's registry clone only, children included via `cancel_children_of`; other sessions untouched). The mutex is held only for a handful of fd syscalls and carries no message traffic; each clone handle is immutable plumbing (never mutated after creation), not a shared mutable channel replacement. The per-session split keeps cancellation granularity exactly one session: a cancel can no longer disturb unrelated sessions' in-flight connections. The rationale lives in ARCHITECTURE.md's `choreo-sockreg` section.
8. **The MCP engine's connection-lifecycle lock** (`choreo-mcp`'s `RmcpEngine.running`, a `tokio::sync::Mutex<Option<RunningService<..>>>`). The running-service handle must stay alive for the whole connection and be closed exactly once on shutdown, so it is a lifecycle handle — never touched per message. It sits behind a mutex solely because `close` needs `&mut`, and `shutdown(&self)` is its only reader: it `take`s the handle and holds the lock across the bounded `close_with_timeout(..).await`. Single-purpose and not protocol data; it is unrelated to the dispatcher-owned in-flight call registry, which carries no lock. The rationale lives in ARCHITECTURE.md's `engine/mod.rs` module row.
9. **The MCP notification rate-limit counter** (`choreo-mcp`'s `NotificationLimiter.state`, a `std::sync::Mutex<LimiterState>` behind an `Arc`). rmcp delivers notifications to the `ClientHandler` on more than one task, so the fixed-window counter that bounds a connection's server logging notifications is shared across those callbacks through one `Arc`-ed `Mutex`. The lock guards only a handful of integers — no protocol data — and is never held across an `await`. The rationale lives in ARCHITECTURE.md's `engine/handler.rs` module row.
10. **The daemon's client-id counter** (`choreo-daemon`'s `broadcast::ClientId`, a `static NEXT: AtomicU32` read by `ClientId::next`). Each accepted connection must be minted a process-unique id BEFORE its connection thread spawns (the acceptor registers the writer, then the thread sends every later command for that id), and minting happens on whichever accept path took the connection — a channel to a dedicated minting thread would add a synchronous round-trip on the accept path for a token no thread ever reads back. A single lock-free monotonic counter is shared instead; it carries no protocol data (it only mints unique tokens and never leaves the process). The rationale lives in ARCHITECTURE.md's `broadcast.rs` module row.
11. **The log-directory test override** (`choreo-shared`'s `paths::TEST_LOG_DIR`, a `static RwLock<Option<PathBuf>>`). Integration tests must redirect the suite's log directory away from the developer's real `$XDG_STATE_HOME`, but that directory is resolved from many threads (the daemon command loop and each per-server MCP thread), so a thread-local override would let a worker leak its log into the real state dir; one process-global override is shared instead. It carries no protocol data and is a no-op in production (nothing sets it outside tests); cargo-nextest runs every test in its own process, so one global override per process stays isolated between tests. The rationale lives in ARCHITECTURE.md's threading section.
12. **The per-file mutation locks** (`choreo-daemon`'s `tools::file_locks::FILE_LOCKS`, a `LazyLock<FileLocks>` wrapping a `Mutex<HashMap<PathBuf, Arc<Mutex<()>>>>`). A turn's non-config tool calls each run on their own thread (the daemon's concurrent tool dispatch), so two mutations of the same file in one batch — `edit_file`'s read-modify-write, a `write_file` racing it, a `delete_files` racing either — would otherwise interleave and lose an update; the per-path lock serializes same-file mutations while different files stay parallel (pi's `withFileMutationQueue` is the same design). It carries no protocol data, the map lock is held only for the brief reserve/prune steps (never across the mutation), and each per-path mutex only for the mutation itself; the key is the target's canonical path — parent directory included when the file does not yet exist — so symlinks to one target and a create racing an edit share a lock. Only the three `fs` mutation tools take it, and the key is a *single* path: deleting a directory does not serialize against a write of a file inside it, and a tool that writes an arbitrary path outside `tools::fs` (`retrieve_webpage`'s `output_path`) is not covered — route any new mutating tool through `with_file_lock`. The rationale lives in ARCHITECTURE.md's threading section.

### Channel selection (crossbeam vs std vs async)

Thread-to-thread messaging uses the **`crossbeam-channel`** leaf crate (not the `crossbeam` umbrella crate), in ALL crates of the workspace — not only those that already depend on it. Crossbeam is a strict superset for our needs: cloneable receivers (true MPMC), `select!`/`select_biased!` (including send arms and timer channels), blocking iterators, and a consistent error taxonomy. std `mpsc` cannot join a `select!` later or add a second receiver without a redesign, so defaulting to crossbeam everywhere keeps future evolution a one-line change no matter which crate the code lives in (a std-`mpsc` channel that later needs crossbeam forces a workspace dependency promotion mid-feature; a crossbeam channel from day one never does). The daemon's long-lived command channels (`DaemonCommand`, `SessionCommand`) and the `choreo-client-core` daemon-connection seam (`from_ui`/`shutdown`) are crossbeam channels now. If a crate does not yet declare `crossbeam-channel`, add it (`crossbeam-channel.workspace = true`, promoted to `[workspace.dependencies]` when shared) in the same change that introduces its first cross-thread channel. This is a consistency/future-proofing rule, not a performance rule — our channels are control-plane and human-rate, so channel throughput is never the bottleneck (see also the crossfire evaluation: crossbeam stays; crossfire is rejected codebase-wide).

Apply it as follows:

- **New code** — any crate: always `crossbeam_channel` for every **messaging**
  channel. That means command/control channels, event/data channels, fan-outs, and
  stream/drain channels — anything that carries more than a single message, that more
  than one consumer might read, or that any code might later `select!` on or clone.
  The workspace migrated its command channels to crossbeam for exactly this reason;
  do not start a new messaging channel on `std::sync::mpsc`.
- **Waits are event-driven**: use `select!`/`select_biased!`, a blocking `recv`, or `recv_deadline`. Bias the arm that must win deterministically: cancellation/stop arms go first so a cancel is observed the instant it is sent (see `recv_sse_event`, the concurrent tool collector, and the retry backoff). The one deliberate exception is a *drain-before-stop* wait — a thread whose job is to flush a queue of messages or output before it obeys a stop signal — which lists its data arm first so queued work is never dropped (the `choreo-client-core` daemon-connection writer and `choreo-daemon`'s tool-output forwarder do this, each with a comment). Never use `recv_timeout` as a poll interval paired with a flag check, and never a `sleep`-poll loop; a timer that *is* the event uses `crossbeam_channel::after(..)`. The one place a bounded-timeout poll of an exception-#1 flag is correct is a blocking OS call no channel can interrupt — the `tiny_http` metrics accept loop (`metrics.rs::serve_metrics`) is the sole instance.
- **`std::sync::mpsc` is reserved for one-shot reply/flag channels** (and test
  scaffolding). A per-request reply — one request, one response, consumed once by one
  thread, never selected on, never cloned — or a fire-once completion/stop flag, may
  use `std::sync::mpsc`, new or existing. This is a deliberate exception, not legacy
  debt: `std::sync::mpsc::Receiver` is neither `Clone` nor `Sync`, so its type is the
  right compile-time marker for a channel that must have exactly one consumer. The
  `DaemonCommand`/`SessionCommand` reply fields and `request_daemon` are the reference
  pattern. When in doubt, use crossbeam.
- **Async code**: keep the runtime's own channels (`futures_channel::mpsc` in `choreo-gui`, tokio channels in the daemon's sidecar runtime). Never call a blocking `recv()` (std or crossbeam) inside an async task — bridge across the async/thread boundary with the crossfire-style `From` conversions only if a measured need appears; the default is to keep blocking `recv()` on dedicated threads.

## Inline Comments

Always write inline comments around new code explaining how it works. Focus on the "why" — the reasoning, intent, and non-obvious details — rather than restating what the code literally does.

Write comments as if the diff never happened: describe the code as it is now. Rationale is *current* design intent — "why it is this way", never "how it used to be" (no "previously…", "used to…", "the old behavior…", "this fixes the bug where…"). Change history lives in the commit message (which *is* the release note), not in the source. A warning against a tempting wrong approach is still welcome when it is current rationale, but phrase it present-tense and imperative ("Do not fall back to a sleep-poll here, because …") rather than as a narrative of what was changed or removed.

## Commit Workflow

Finishing an implementation run means the work is **not done until it is committed**. When a run of implementation turns completes, verify and commit it yourself, in the same run. Do not stop to ask the user whether to commit — the commit is part of the task, not a separate approval step.

A run is committed **once, at the end of each unit of work** — not once per turn. When a run delegates to subsessions, each completed subsession commits its own unit before returning (see [Task Execution](#task-execution)); the parent then commits whatever remains when its own run ends.

### Run the gate directly — do not invent intermediate gates

When a run is ready to verify, run **`just pre-commit`** as the one and only gate. Do **not** prefix it with a bespoke sequence of checks — a scoped `cargo clippy -p …`/`cargo clippy --all-targets …`, a scoped `cargo nextest run -p …`, a hand-picked lint or format pass, or any ad-hoc command assembled "to be sure." The recipe already runs the full clippy + test + fmt sequence with the exact flags the release workflow expects (they live in `.cargo/config.toml`), so every custom pre-check is wasted work — and, worse, a bespoke check can pass while the real gate fails (different scope/flags), or fail while the real gate passes, sending you to fix a non-problem. An inner-loop `cargo nextest run -p <crate>` while you are still editing is fine (see [Task Execution](#task-execution)); the moment the work is ready, go straight to `just pre-commit`, loop it (fix by hand, re-run) until it is green in one pass, then commit.

### When the gate is not required

Some changes cannot be affected by any step of the gate and may be committed **without** running `just pre-commit`:

- **Documentation-only changes** — Markdown and other non-Rust text (`README.md`, `ARCHITECTURE.md`, `RELEASE.md`, `AGENTS.md`, `docs/`, `packaging/` service/PKGBUILD files, `.github/workflows/*.yml`, …). `clippy`/`test-all`/`fmt` operate on Rust sources and cannot be affected by them, and no other gate step reads non-Rust text — so these need no gate run. (The release notes are generated from commit messages by git-cliff in CI, so there is no changelog to guard locally; see [Release notes via commits](#release-notes-via-commits).)

Anything that touches Rust source, `Cargo.toml`/`Cargo.lock`, build scripts, or the `.cargo` config still requires the full gate. A mixed change (docs **and** code) takes the strictest applicable rule — run the full gate. When in doubt, run it: it is cheap next to a broken commit.

### The gate

Run **`just pre-commit`**. It is the commit gate, and it is safe to re-run: loop it (fix by hand, re-run) until it passes green, then commit. The gate only *verifies*, except for the single tree-mutating step (`fmt`) which runs last; it runs (the flags themselves live in `.cargo/config.toml`):

1. **`clippy-strict`** — `cargo clippy --workspace --all-targets --all-features --locked -- -D warnings`, which must report **nothing**. Fix pre-existing warnings too, not just the ones the change introduced — the gate is a clean workspace, not a clean diff. The gate does not auto-apply lints: run `just clippy-fix` (`cargo clippy --fix --allow-dirty --allow-staged --workspace --all-targets --all-features --locked`) by hand first if you want the machine-applicable ones applied automatically.

   **Do not silence a lint to make the gate pass.** An `#[allow(...)]`/`#[expect(...)]` is a last resort, not a way to satisfy `clippy-strict`: prefer restructuring the code (extract a function, a struct, a helper type, a constant) so the warning cannot arise. Suppress only when there is a justifiable reason the lint is wrong or unavoidable here — the diagnostic is a false positive, or the code is genuinely constrained (a trait/framework signature you do not control, a generated/FFI boundary, a test with a deliberately awkward shape) — and record that reason in a comment on the attribute. Never suppress a whole file or module to hide one site. When you do suppress, **it must be `#[expect(...)]`, not `#[allow(...)]`** — this is enforced by the denied `clippy::allow_attributes` lint (root `Cargo.toml`), which flags every outer `#[allow]` and so surfaces a stale one whose underlying lint no longer fires. Use `#[allow(...)]` only when an `#[expect]` genuinely cannot be satisfied — a file compiled in multiple contexts where the lint fires in one build but not another, or generated/vendored code that must not be hand-edited — and then add a sibling `#[allow(clippy::allow_attributes)]` immediately above it (an inner `#![allow(clippy::allow_attributes)]` on the declaring `mod`, or a file-level inner attribute, for generated/multi-context files) with a comment recording the reason.
2. **`doc-check`** — `RUSTDOCFLAGS="-D warnings" cargo doc --no-deps <doc_crates>`: rustdoc must report **nothing** for every migrated crate — no broken or private intra-doc links, bare URLs, invalid HTML/code blocks, or redundant links. The checked crate list (`doc_crates` in the justfile) grows as crates are migrated (see [Rustdoc vs ARCHITECTURE.md](#rustdoc-vs-architecturemd)).
3. **`test-all`** — the full unit + integration suite via nextest, all features and all targets (see [Test Discipline](#test-discipline)). It must pass in full.
4. **`fmt`** — `cargo fmt --all`, applied **last**: fmt is semantics-preserving, so the formatted bytes are behaviourally identical to the tested bytes and need no re-test.

If any step fails, fix the cause and re-run `just pre-commit` from the top — a hand fix can introduce a new clippy warning or test failure, so the gate only holds when the whole sequence is clean in one pass. If clippy, formatting, or the tests genuinely cannot be made to pass, **do not commit**; stop and report the failure instead.

### Documentation (part of the run, before committing)

Update `ARCHITECTURE.md` / `README.md` if the change touches anything they describe, and write the commit message as the release note (see [Release notes via commits](#release-notes-via-commits)).

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

The `.githooks/pre-commit` hook has been removed; the `just pre-commit` recipe is the gate, and nothing runs it automatically — it is the agent's responsibility on every implementation run that changes Rust source, manifests, build scripts, or the `.cargo` config (documentation-only changes are exempt — see [When the gate is not required](#when-the-gate-is-not-required)).
