# Plan: fully utilize redb's feature surface

**Status:** proposed — design complete; nothing implemented (no source changes in this change).
**Lifecycle:** this file is **deleted once the plan is fully implemented**. Nothing written during
implementation may reference it — rustdoc, `ARCHITECTURE.md`, `README.md`, release notes, and commit
messages must stand on their own.
**Date:** 2026-11-05
**Targets:** `choreo-daemon` (the `db` module and its `sessions`/`attachments` children, `daemon/open.rs`,
`metrics.rs`, `cli.rs`); `ARCHITECTURE.md` (the durable-store section); `README.md` only if a CLI
command is documented.
**Touches:** `choreo-daemon/src/db/{mod,sessions,attachments}.rs` (bulk ops, stats helper), a new
`choreo-daemon/src/db/stats.rs`, `choreo-daemon/src/daemon/open.rs` (repair callback, startup
compaction), `choreo-daemon/src/metrics.rs` (DB gauges), `choreo-daemon/src/cli.rs` (a `db`
subcommand group), `ARCHITECTURE.md`.

**Line references are as of commit on `main` when this was written.** They are pointers for the
implementer, not contracts; re-derive them before editing.

> **TL;DR.** The daemon uses redb's *core* correctly — typed tables, single-transaction writes,
> atomic multi-table commits, range scans — but leaves six higher-level capabilities untouched:
> **(1)** bulk in-place row removal (`retain_in`) instead of the collect-into-a-`Vec`-then-`remove`-one-by-one
> loops on every delete path; **(2)** the whole `stats()`/`TableStats`/`DatabaseStats` surface (there is
> **no DB telemetry at all**, despite an existing Prometheus feature); **(3)** `compact()` (the file never
> shrinks after deletes) and `check_integrity()`; **(4)** the `Builder` repair callback for corruption-at-open;
> **(5)** `Durability::None` for the per-iteration turn checkpoint; **(6)** `insert_reserve` for the
> multi-MB image writes. It also documents why some features are deliberately *not* used (multimap
> tables, read-only handles, in-crate backup). Work is ordered so each phase is independently
> shippable, with **Phase 1 (bulk ops)** and **Phase 2 (stats/metrics)** delivering the most value at
> the lowest risk.

---

## Table of contents

1. [Current state](#1-current-state)
2. [Verified redb 4.4.0 capability inventory](#2-verified-redb-440-capability-inventory)
3. [Phase 1 — bulk row ops via `retain_in`](#3-phase-1--bulk-row-ops-via-retain_in)
4. [Phase 2 — database stats and metrics](#4-phase-2--database-stats-and-metrics)
5. [Phase 3 — compaction and integrity check](#5-phase-3--compaction-and-integrity-check)
6. [Phase 4 — open-time repair callback](#6-phase-4--open-time-repair-callback)
7. [Phase 5 — durability tuning for hot writes](#7-phase-5--durability-tuning-for-hot-writes)
8. [Phase 6 — `insert_reserve` for large byte writes](#8-phase-6--insert_reserve-for-large-byte-writes)
9. [Deliberately out of scope](#9-deliberately-out-of-scope)
10. [Testing strategy](#10-testing-strategy)
11. [Documentation](#11-documentation)
12. [Decisions needed](#12-decisions-needed)
13. [Definition of done](#13-definition-of-done)

---

## 1. Current state

redb is pinned at **4.4.0** (`Cargo.toml`), default features (`std` only — no `cache_metrics`,
no `logging`). All storage lives in `choreo-daemon/src/db/`:

- `mod.rs` — schema (table definitions), open+migrate, credentials/catalog/KV plumbing.
- `sessions.rs` — `SessionRecord`/turn CRUD, tombstones, retry wrappers.
- `attachments.rs` — raw image byte store, split out of the compressed turn blob.
- `codec.rs` — zstd turn-value codec.

The `redb::Database` is held as `Arc<redb::Database>` on `DaemonState` (`daemon.rs:330`) and cloned into
every connection thread (concurrent reads); the command loop is the sole writer. **This ownership model
is the key constraint** for the two features that need unique `&mut Database` (compaction, integrity
check): they are only reachable while no `Arc` clone and no live transaction exists.

Startup opens the DB through a Windows-safe open→version→drop→copy→reopen→migrate sequence in
`daemon/open.rs:109-140`, immediately wrapping the handle in `Arc::new(...)` (line 120) **before**
`run_migrations_at` (line 140). That block is the code the startup-compaction variant of Phase 3 must
reshape.

Verified *not* used anywhere in `choreo-daemon` (grep clean): `retain_in`/`retain`/`extract_if`,
`stats()`/`TableStats`/`DatabaseStats`, `compact`, `check_integrity`, `Builder`,
`set_repair_callback`, `set_durability`/`Durability`, `insert_reserve`, savepoints, `ReadOnlyDatabase`,
`list_tables`, multimap tables.

## 2. Verified redb 4.4.0 capability inventory

Facts confirmed against the crate source (`~/.cargo/registry/.../redb-4.4.0`), so the plan does not
promise an API that is absent or feature-gated:

| Capability | Exact signature / note | Usable in production? |
|---|---|---|
| `Table::retain_in(range, pred)` | `impl RangeBounds<KR>, KR: Borrow<K::SelfType<'a>>`; returns `Result<()>` (no count); predicate `FnMut(key,value)->bool` = keep; a panicking predicate **poisons** the txn | **Yes** (no feature flag; the `experimental-api-5` variant is the alternate signature) |
| `Table::retain(pred)` | whole-table variant, same semantics | Yes |
| `ReadableTableMetadata::stats()` / `len()` | implemented by `Table`, `ReadOnlyTable`, multimap tables → `TableStats` + row `len()`; works on a **read** transaction | **Yes** |
| `DatabaseStats` via `WriteTransaction::stats()` | tree_height, allocated_pages, leaf/branch pages, stored_bytes, metadata_bytes, fragmented_bytes, page_size — **only on `WriteTransaction`**, not `ReadTransaction` | Yes (needs a write txn) |
| `Database::compact(&mut self)` | returns `Result<bool, CompactionError>`; refuses with `TransactionInProgress`/`PersistentSavepointExists`/`EphemeralSavepointExists` if any txn/savepoint live; requires unique ownership (`Arc::get_mut`), else `TransactionInProgress` | Yes, but **only with exclusive DB ownership** |
| `Database::check_integrity(&mut self)` | `Result<bool, DatabaseError>`; same unique-ownership + no-live-txn requirement; promotes pending non-durable commits | Yes, same exclusivity |
| `Builder::create/open` + `set_repair_callback(impl Fn(&mut RepairSession) + 'static)` | open-time repair hook, called ≥1× if repair needed | **Yes** |
| `Builder::set_cache_size` | default **1 GiB** already | Yes (optional) |
| `Builder::set_page_size` / `set_region_size` | `#[cfg(any(test, fuzzing))]` | **No — test only** |
| `Builder::set_concurrency_mode` | `#[cfg(feature = "experimental-multiprocess")]` | No (feature off) |
| `WriteTransaction::set_durability(Durability)` | default `Immediate`; `None` = not persisted until a later `Immediate` commit | Yes |
| `Table::insert_reserve(key, len)` | requires `V: MutInPlaceValue`; **`&[u8]` implements it**, so our `&[u8]`-valued tables qualify; returns `AccessGuardMutInPlace<&[u8]>` whose `as_mut()` is `&mut [u8]` to fill in place | Yes |
| Savepoints | `persistent_savepoint`/`ephemeral_savepoint`/`restore_savepoint`/`list_`/`delete_` | Yes (interacts with compact) |
| `ReadOnlyDatabase` / `Builder::open_read_only` | `DatabaseAlreadyOpen` on lock-supporting platforms while a writer holds the file | Marginal (see §9) |
| In-crate backup API | **absent in 4.4.0** | N/A — `fs::copy` is correct |
| Multimap tables | present | N/A to this schema |

## 3. Phase 1 — bulk row ops via `retain_in`

**Goal:** remove the "collect every matching key into a `Vec`, then `remove` each one" pattern from
every delete path. `retain_in(range, |_,_| false)` removes the whole range in a single tree pass with
no temporary key buffer. Pure refactor — identical observable behavior, fewer allocations and one tree
traversal instead of two.

**Sites (collect-then-remove today):**

- `mod.rs` `kv_delete_range` — collects `Vec<(u64,String)>`, removes in a loop. **Returns a count**;
  preserve it by capturing `&mut count` in the predicate (the predicate runs once per entry in range,
  so `|_,_| { *count += 1; false }` counts exactly what is removed).
- `sessions.rs` `delete_session` — the turns block and the kv block each collect-then-remove.
- `sessions.rs` `delete_session_turns` — currently iterates the **whole** `session_turns` table
  (`table.iter()`) and filters `key.0 == session_id`. Replace with a session-scoped
  `retain_in((sid, 0u32)..(session_range_end(sid), 0u32), |_,_| false)` — this **also fixes the
  full-table scan** (an independent efficiency bug), not just the removal loop.
- `attachments.rs` `delete_session_attachments` and `delete_turn_attachments` — both collect-then-remove.

**Approach detail:**

- Reuse the existing `session_range_end` helper for the upper bound; add a small `turn_range(sid, turn)`
  helper if the per-turn bound in `delete_turn_attachments` reads cleaner as a function.
- The predicates only read key fields, never the value or any borrowed-from-the-table state — the
  documented precondition (no reentrancy, must not panic) is satisfied. Keep them `|_, _| false`.
- `purge_tombstoned_sessions` currently reads all tombstone ids then calls `delete_session` per id; that
  is not a collect-then-remove over one table and can stay as-is (it spans multiple tables per id).
- Keep `delete_session`'s multi-table atomicity (sessions + turns + kv + attachments in one
  `write_txn`) — only the in-table removal mechanics change.

**Tests:** the existing `round_trip`, `turn_image_bytes_split_into_attachments_and_reattached`, and
`turn_rewrite_clears_stale_attachment_slots` already assert post-delete emptiness. Add:
- a unit test that `kv_delete_range` returns the correct removed count for a range and leaves
  out-of-range keys intact;
- a unit test that `delete_session_turns` removes only the target session's turns when a second session
  shares the table (guards the range-scoping fix);
- a test that a `retain_in` on an empty range is a no-op.

## 4. Phase 2 — database stats and metrics

**Goal:** make the store observable. Today `metrics.rs` has **zero** database metrics despite a full
Prometheus stack. This is the highest standalone value at low risk, because per-table stats read on a
**read** transaction (no write-lock contention).

**New module `db/stats.rs`:**

```rust
/// Aggregate durable-store statistics, gathered from a single read snapshot.
pub struct DbStats {
    pub file_bytes: u64,                 // fs::metadata(db path)
    pub tables: Vec<(String, TableStats)>, // per known table
    pub sessions: u64,                   // SESSIONS len
    pub turns: u64,                      // SESSION_TURNS len
}
```

- `pub fn database_stats(db: &redb::Database) -> io::Result<DbStats>` opens one `begin_read()`, then for
  each known `TableDefinition` (`SESSIONS`, `SESSION_TURNS`, `SESSION_ATTACHMENTS`, `CREDENTIALS`, `META`,
  `CATALOG_STATE`, `SESSION_KV`, `DELETED_SESSIONS`, `KEYSTORE`) calls `open_table` (treating
  `TableDoesNotExist` as empty) and `stats()`/`len()` via `ReadableTableMetadata`. Sum the per-table
  `TableStats` for aggregate.
- The path for `file_bytes` is available to the caller (startup already knows it); pass it in or store
  it, rather than re-resolving via `db_path()`.
- If an aggregate `DatabaseStats` (single call) is preferred over summing, that requires a
  `WriteTransaction`; expose it as a separate `write_txn.stats()`-based path only for the offline CLI
  (§5), never on the metrics hot path.

**Metrics wiring (`metrics.rs`):**

- Add gauges: `db_file_bytes`, `db_stored_bytes`, `db_fragmented_bytes`, `db_allocated_pages`,
  `db_page_size`, `db_sessions_total`, `db_turns_total`.
- Add `pub fn record_db_stats(stats: &DbStats)` in both the `metrics` backend and the no-op stub
  (identical signature in both, per the crate's existing dual-impl convention).
- Drive it from the existing catalog **maintenance thread** tick (or a dedicated low-frequency timer on
  the command loop) — stats are inherently a poll, not an event. Choose the maintenance tick to avoid
  adding a new thread; refresh at most once per maintenance interval.

**CLI/admin surface:** add a `choreographr db stats` subcommand (§5 introduces the `db` group) that opens
the DB (daemon stopped, unique ownership available), prints the per-table table plus file size, and
exits. This gives operators the same numbers offline.

**Tests:** unit test opens a tiny DB with a couple sessions/turns and asserts `sessions`/`turns` counts
match and `stored_bytes > 0`; a metrics test (feature `metrics`) asserts `record_db_stats` updates the
gauges (follow the existing metrics tests' pattern if present, else assert the stub is a no-op when the
feature is off).

## 5. Phase 3 — compaction and integrity check

**Constraint that shapes this phase:** `compact()` and `check_integrity()` both take `&mut Database` and
fail with `TransactionInProgress` unless the handle is uniquely owned (`Arc::get_mut` succeeds) and no
transaction or savepoint is live. While the daemon runs, the `Arc<redb::Database>` is cloned into every
connection thread, so neither is callable mid-run. Two sanctioned homes:

**A. Offline CLI subcommands (primary).** Add a `db` subcommand group to `cli.rs::Command`:

- `choreographr db stats` — Phase 2 output.
- `choreographr db compact` — open, `compact()`, report whether it ran and bytes reclaimed.
- `choreographr db check` — open, `check_integrity()`, report clean/corrupt; **do not** auto-repair —
  surface the result and let the operator decide (consistent with the existing hard-error-not-recreate
  policy in `open_db_at`).

These run with the daemon stopped, which is exactly when unique ownership is available, and they reuse
`open_db_at` + `run_migrations_at`. The commands must refuse cleanly if the file is locked by a running
daemon (`redb::DatabaseError` surfaces that — map to a clear message).

**B. Optional startup compaction (secondary, gated).** Only worth it if operators won't run the CLI.
Reshape `daemon/open.rs:120-140` so the `Database` stays **owned** (not `Arc`) through open → migrate →
*maybe compact* → `Arc::new`, i.e. move `run_migrations_at` into the pre-`Arc` block and add a
compaction step before wrapping. Gate the step on a fragmentation threshold derived from the per-table
`stats()` (e.g. compact when `fragmented_bytes / (stored_bytes + metadata_bytes)` exceeds a constant) or
an env var `CHOREOGRAPHR_DB_COMPACT=auto|always|never` (default `auto`). Compaction is slow and uses
2-phase commit internally; log start/finish and reclaimed bytes. Keep this conservative and off by
default until measured.

**Tests:** integration test (crate `tests/it/`) that creates a DB, deletes a large fraction of rows,
runs the compaction path, and asserts the file shrank and data round-trips afterward. Integrity: a test
that a healthy DB reports clean; (corruption injection is out of scope — redb's own tests cover the
corrupt path).

## 6. Phase 4 — open-time repair callback

**Goal:** when redb must repair a corrupt database at open, make it **observable** instead of silent.
`open_db_at` currently calls `Database::create`/`open` and hard-errors on unrepairable corruption; a
`Builder::set_repair_callback` lets a repairable file be recovered with a log breadcrumb.

**Approach:**

- Replace `redb::Database::create(path)` / `redb::Database::open(path)` in `open_db_at` with
  `redb::Builder::new().set_repair_callback(...).create(path)` / `.open(path)` (or a shared helper
  `open_with_builder(path, create: bool)`). The builder's other production knob is `set_cache_size`
  (default 1 GiB is already fine — leave it unless a reason to change appears). **Do not** touch
  `set_page_size`/`set_region_size` (test-only) or `set_concurrency_mode` (needs an off feature).
- The callback logs `warn!`/`info!` that a repair session is running and, on completion, that the DB
  recovered. It carries no message payloads (per the logging rule) — counts/paths only.
- This complements, not replaces, the existing `UpgradeRequired`/corruption hard-error arms; a repair
  that cannot complete still surfaces as the same error.

**Tests:** hard to trigger a genuine repair deterministically; unit-test that the builder path still
opens/creates/stamps a fresh and an existing DB identically to today (the callback simply never fires on
a healthy file). Cover the wiring, not redb's repair engine.

## 7. Phase 5 — durability tuning for hot writes

**Goal (needs a decision):** `write_turn` runs once per agent-loop iteration as a checkpoint, and every
commit currently fsyncs (`Durability::Immediate`, the default). The `*_retry` wrappers with 1 ms sleeps
(`sessions.rs`) suggest write contention. The `turn_count` doc already accepts that mid-turn
checkpoints may lag a crash.

**Approach:**

- For the **mid-turn checkpoint** writes, `set_durability(Durability::None)` on the write transaction,
  with an `Immediate` commit at the turn's finalize (and before any process exit / graceful shutdown)
  so the turn is durable once complete. A crash between the last checkpoint and finalize loses only the
  in-flight iteration — the already-documented acceptable window.
- Keep `Immediate` for everything else (sessions, credentials, catalog, keystore) — those are rare and
  must be durable.
- Gate behind a config/env flag initially (`CHOREOGRAPHR_DB_NON_DURABLE_TURNS=1`) and measure; only
  flip the default if the win is clear. `check_integrity`/`compact` already promote/handle pending
  non-durable commits, so the offline tools stay correct.
- This is the **riskiest** phase (crash-durability tradeoff + interaction with finalize ordering); ship
  it last and keep it opt-in.

**Tests:** unit test asserting a non-durable checkpoint followed by an `Immediate` finalize both read
back correctly within one process; a test that a mid-turn checkpoint is visible to a read in the same
process (redb exposes uncommitted-durable state to its own reads). Crash-window behavior is documented,
not unit-tested.

## 8. Phase 6 — `insert_reserve` for large byte writes

**Goal:** avoid the intermediate copy when writing multi-MB image bytes. `insert_reserve(key, len)`
writes straight into reserved pages; `&[u8]` qualifies as `MutInPlaceValue`, and the returned
`AccessGuardMutInPlace<&[u8]>::as_mut()` is a `&mut [u8]` to fill.

**Sites:** the attachment inserts in `attachments.rs` (`write_turn_attachments`, `write_attachment`) and,
optionally, the turn-blob insert in `sessions.rs::write_turn`. Use `insert_reserve(key, data.len())`,
copy `data` into the `&mut [u8]`, drop the guard before commit.

**Caution:** this is a micro-optimization with real complexity cost — only adopt it where the values are
large (image bytes), and only if benchmarked to matter. If it does not measurably help on the emit path,
skip it; the plan tolerates dropping Phase 6.

**Tests:** existing attachment round-trip tests already cover correctness; add one asserting an
`insert_reserve`-written slot reads back byte-for-byte equal to the source.

## 9. Deliberately out of scope

- **Multimap tables** — the schema (tuple-keyed single-value tables) does not need them; adding one
  would complicate the codec for no benefit.
- **`ReadOnlyDatabase` / `open_read_only`** — on lock-supporting platforms it errors with
  `DatabaseAlreadyOpen` while the daemon holds the write lock, so it cannot serve in-daemon admin reads;
  offline reads are already served by the CLI `db` commands.
- **In-crate backup** — redb 4.4.0 ships no backup API; the existing `fs::copy`-before-lock approach is
  the correct implementation of the Windows whole-file-lock constraint and stays.
- **`experimental-multiprocess` / `experimental-api-5` / `cache_metrics` / `logging` features** — none are
  needed; enabling them would pull behavior we do not want (multi-writer coordination, alternate API
  shape, extra deps).
- **Savepoints** — not needed for the migration backup (which must be a filesystem copy) and not
  obviously useful elsewhere; note them as a future option if a "snapshot before a destructive admin op"
  ever appears. (Persistent savepoints also block `compact()`.)

## 10. Testing strategy

Per the workspace's Test Discipline: unit tests in `src/` `#[cfg(test)]` modules (no time-based waits),
integration tests in `choreo-daemon/tests/it/<name>.rs` with a `mod <name>;` line in `tests/it/main.rs`,
marked `#[ignore]`. Compaction/integrity/CLI tests are filesystem-bound → integration. Stats and
`retain_in` are in-memory → unit, except where they exercise `fs::metadata`.

## 11. Documentation

- `ARCHITECTURE.md` durable-store section: describe the stats/metrics path, the offline `db` CLI group,
  and the compaction/integrity constraint (unique-ownership window). Since this is user-visible behavior
  (a CLI group + metrics), it belongs in the markdown, not only rustdoc.
- `db/stats.rs` rustdoc owns the `DbStats` contract; `db/mod.rs` module header gains a line on bulk ops.
- `README.md`: add the `db` subcommands to the CLI section **only if** the README documents CLI commands.

## 12. Decisions needed

1. **Durability (Phase 5):** adopt `Durability::None` for mid-turn checkpoints, or leave all commits
   `Immediate`? (Recommendation: measure first, ship opt-in.)
2. **Compaction home (Phase 3):** offline CLI only, or also a gated startup pass? (Recommendation: CLI
   first; startup pass only if operators need it.)
3. **Stats trigger (Phase 2):** reuse the catalog maintenance tick, or a dedicated low-frequency timer?
   (Recommendation: maintenance tick — no new thread.)
4. **`insert_reserve` (Phase 6):** worth the complexity? (Recommendation: benchmark; drop if not clearly
   faster.)

## 13. Definition of done

- Every delete path in `db/` uses `retain_in` (no collect-then-remove loops remain); counts preserved.
- `db/stats.rs` exists; `database_stats` is unit-tested; DB gauges flow to `/metrics` under the feature
  and are no-ops without it.
- `choreographr db stats|compact|check` exist, are documented, and refuse cleanly when the daemon holds
  the file.
- `open_db_at` uses `Builder` + a logging repair callback.
- (If decided) non-durable turn checkpoints are opt-in and measured.
- `just pre-commit` green; `ARCHITECTURE.md` updated; commits written as release notes (`refactor`/`feat`
  per phase).
