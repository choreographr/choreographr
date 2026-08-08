# MessagePack + Versioned DB Migration Framework

## Context

Choreographr is unreleased. Two cross-cutting decisions need to land *before*
release, because after release both become user-data migrations:

1. **The client-facing wire protocol must move off postcard.** Research
   (2026-08) confirmed postcard has no mature cross-language support: no
   Python binding exists on PyPI (the name is squatted by an unrelated email
   client), the Go/Scala implementations are zero-star WIP parsers, and the
   only real implementation is the Rust crate. The project's stated direction
   (README) is a self-describing binary format with broader language support,
   for future mobile/web/third-party clients.
2. **The DB has no schema versioning.** There is no production `meta` table
   (only a `#[cfg(test)]` one), no migration runner, and `open_db` currently
   *silently destroys* a database whose redb file format it can't open
   (`DatabaseError::UpgradeRequired` falls into the "trying to recreate"
   catch-all and `Database::create` clobbers the file).

**Decision: MessagePack via `rmp-serde` 1.3+ in *named* mode, for both DB
values and the wire payloads; plus a schema-version + migration-runner
framework in `db.rs`.** Verified in the released `rmp-serde` 1.3.1 source
(docs.rs-pinned): `to_vec_named` writes structs as maps *with field names*,
and — unlike older versions — enum variants are written **by name**
(`serialize_unit_variant` → the variant string; newtype/tuple/struct variants
→ `{"variant_name": payload}`). That gives JSON-grade evolution safety
(reorder/remove/mid-insert of struct fields **and** enum variants is safe;
missing fields fall back to `#[serde(default)]`) in a compact binary format
with universal language support. Struct *arrays* are still tolerated on
decode, so a future switch to compact mode stays backwards-readable.

## Goal

- Replace postcard with MessagePack (named) for (a) client↔daemon wire frames
  and (b) the two DB tables whose values are codec-dependent (`sessions`,
  `session_turns`).
- Bump the wire `PROTOCOL_VERSION` so old clients fail cleanly.
- Introduce `schema_version` in a production `meta` table + an ordered,
  transaction-atomic migration runner + a pre-migration backup + a hard error
  for redb `UpgradeRequired`.
- Stamp `schema_version = 1` at DB creation. The migration runner ships as
  empty-but-ready infrastructure: the first migration will be a *future*
  breaking schema change, not a codec rewrite (pre-release data is not worth
  migrating).
- Keep postcard on the Rust-only, language-isolated channels where it costs
  nothing to retain.

## Non-goals

- **No key re-encoding.** redb keys stay typed (`u64`, `(u64, u32)`, `&str`,
  `(u64, String)`); range scans in `kv_get_range`/`kv_delete_range`/`kv_list`/
  `kv_count` depend on redb's native ordering. Key-type changes are handled by
  table-rebuild migrations, never by wrapping keys.
- **No change to the VM or credential channels** (see Scope below).
- **No dual-codec content negotiation** on the wire. One codec, one
  `PROTOCOL_VERSION`.
- **No switch to CBOR/JSON/protobuf** (see `plans/` research in the prior
  discussion; MessagePack is the pick).

## Scope — postcard touch-point inventory

| Location | Path | Verdict |
|---|---|---|
| Wire frame codec | `choreo-proto/src/frame.rs` (`encode_payload`, `encode_frame`, `decode_frame`) | **Move to MessagePack named** |
| Protocol error | `choreo-proto/src/error.rs` (`ProtoError::Postcard`) | **Rename to `Codec`** |
| Proto round-trip tests | `choreo-proto/src/types.rs` `#[cfg(test)]`, `choreo-proto/src/tests.rs` | **Move to MessagePack named** |
| DB values | `choreo-daemon/src/db.rs` (write/read session, read_all_sessions, write/read turns) | **Move to MessagePack named** |
| DB schema versioning | `choreo-daemon/src/db.rs` (new) | **New `meta` table + versioned runner (empty migration chain at release)** |
| VM↔host protocol | `choreo-daemon/src/tools/vm.rs`, `tools/mod.rs` `encode_outer`/`execute_postcard` | **Keep postcard** (Rust-only; documented "VM guest" path) |
| Credential pipeline | `choreo-client-core/src/credentials.rs` (postcard→ECDH→AES-GCM) + `choreo-daemon/src/daemon.rs` decrypt | **Keep postcard** (encrypted, Rust-only, no foreign reader) |
| Tool results to clients | `DaemonMessage` carries strings / raw bytes, not postcard | No change |
| `session_kv`, `credentials`, `deleted_sessions` tables | opaque bytes | No change |

All existing clients (choreo-tui, choreo-client-core, choreo-acp, choreo-im)
go through `choreo-proto`'s frame functions, so the wire swap is one crate.

---

## Part 1 — MessagePack on the wire (`choreo-proto`)

### Cargo

```toml
# [workspace.dependencies]
rmp-serde = "1.3"   # pin 1.3: name-based enum encoding is new in this line

# choreo-proto/Cargo.toml, choreo-daemon/Cargo.toml
rmp-serde.workspace = true
```

`postcard` stays a workspace dep (VM + credentials still use it).

### `frame.rs`

```rust
/// 1 = postcard era; 2 = MessagePack (named, rmp-serde >= 1.3)
pub const PROTOCOL_VERSION: u8 = 2;

pub fn encode_payload<T: Serialize>(message: &T) -> Result<Vec<u8>, ProtoError> {
    let payload = rmp_serde::to_vec_named(&(PROTOCOL_VERSION, message))
        .map_err(|e| ProtoError::Codec(e.to_string()))?;
    if payload.len() > MAX_FRAME_SIZE {
        return Err(ProtoError::FrameTooLarge);
    }
    Ok(payload)
}
// encode_frame: same body, then 4-byte BE length prefix (unchanged).

pub fn decode_frame<T>(payload: &[u8]) -> Result<T, ProtoError>
where
    T: for<'de> Deserialize<'de>,
{
    let ((version, message), remainder): ((u8, T), &[u8]) =
        rmp_serde::from_slice_ref(payload)
            .map_err(|e| ProtoError::Codec(e.to_string()))?;
    if !remainder.is_empty() {
        return Err(ProtoError::TrailingBytes);
    }
    if version != PROTOCOL_VERSION {
        return Err(ProtoError::UnsupportedVersion { version });
    }
    Ok(message)
}
```

Notes:
- `rmp_serde::from_slice_ref` returns the unused remainder, preserving the
  existing `TrailingBytes` check. **Verify** this function exists in 1.3.1
  during implementation (it does in the 1.x line); fallback is
  `rmp_serde::from_slice` + a manual `Deserializer::new` remainder probe.
- The `(PROTOCOL_VERSION, message)` tuple serializes as a MessagePack array
  of 2 even in named mode — no change to the framing shape.
- `ProtoError::Postcard(String)` → `ProtoError::Codec(String)`. Verified via
  grep: the variant is referenced only inside `choreo-proto` (`error.rs`,
  `frame.rs`). Other crates use `ProtoError` only through `#[from]`/`Io`
  matches and need no changes. `ClientError::Postcard` (client-core) and
  `ToolError::Postcard` (daemon tools) are *separate* enums tied to
  postcard-the-crate (credentials/VM paths) and must stay untouched.

### Tests to update

- `choreo-proto/src/types.rs` `#[cfg(test)]` round-trips (ReasoningArtifact,
  ReasoningProducer, Turn): `postcard::to_allocvec`/`from_bytes` →
  `rmp_serde::to_vec_named`/`from_slice`.
- `choreo-proto/src/tests.rs` version-rejection test:
  `rmp_serde::to_vec_named(&(PROTOCOL_VERSION + 1, ClientMessage::Ping))`.
- Add a named↔array decode-tolerance test (encode named, decode a hand-built
  array blob) to lock in the lenient-decode property.

---

## Part 2 — MessagePack on the DB (`choreo-daemon/src/db.rs`)

Swap the eight call sites; nothing else changes structurally:

| Function | Before | After |
|---|---|---|
| `write_session` | `postcard::to_allocvec(record)` | `rmp_serde::to_vec_named(record)` |
| `read_session` / `read_all_sessions` | `postcard::from_bytes::<SessionRecord>` | `rmp_serde::from_slice::<SessionRecord>` |
| `write_turn` | `postcard::to_allocvec(turn)` | `rmp_serde::to_vec_named(turn)` |
| `read_turns` | `postcard::from_bytes::<Turn>` | `rmp_serde::from_slice::<Turn>` |

Keep the existing warn-and-skip behavior on undecodable entries in
`read_all_sessions`/`read_turns` — it now doubles as post-migration
resilience. Error messages change from "postcard encode/decode" to "codec
encode/decode".

---

## Part 3 — Versioned migration framework (`choreo-daemon/src/db.rs`)

### Design

```rust
/// Persisted schema version. Bump on any *breaking* change to persisted
/// records: codec swap, key-type change, table split/merge, semantic change.
/// Additive fields (with `#[serde(default)]`) do NOT bump it — named
/// MessagePack tolerates those without a migration.
pub const SCHEMA_VERSION: u64 = 1;

/// Production `meta` table (promote the test-only one). Key: &str, value: u64.
const META: TableDefinition<&str, u64> = TableDefinition::new("meta");
const SCHEMA_VERSION_KEY: &str = "schema_version";

type Migration = fn(&redb::Database) -> io::Result<()>;
/// MIGRATIONS[i] upgrades schema version i → i+1.
///
/// Empty at release: version 1 is the *initial* stamped version, reached by
/// initialization, not by a migration. There is no v0 data worth migrating
/// pre-release (see "Legacy / pre-schema databases" below). The first real
/// entry lands with the first future breaking schema change.
const MIGRATIONS: &[Migration] = &[];

fn current_schema_version(db: &redb::Database) -> io::Result<u64> {
    // Open META; TableDoesNotExist (or missing key) ⇒ 0 (unversioned).
}

fn stamp_schema_version(db: &redb::Database, version: u64) -> io::Result<()> {
    // Tiny write txn: META[SCHEMA_VERSION_KEY] = version.
}

pub fn run_migrations(db: &redb::Database) -> io::Result<()> {
    let current = current_schema_version(db)?;
    if current > SCHEMA_VERSION {
        return Err(db_err(format!(
            "database schema version {current} is newer than this binary supports ({SCHEMA_VERSION}); \
             upgrade choreographr before continuing"
        )));
    }
    // An unversioned DB is only ever acceptable as the *initial* state (v1).
    // Once the chain grows, a no-meta DB means pre-release leftovers.
    if current == 0 && SCHEMA_VERSION > 1 {
        return Err(db_err(
            "database has no schema version (pre-release data); recreate it or restore a backup",
        ));
    }
    if current == SCHEMA_VERSION {
        return Ok(()); // idempotent fast path
    }
    // Snapshot only before an actual migration writes. With an empty chain
    // (current release) this never fires — the 0 → 1 transition is pure
    // initialization (stamping), and nothing was rewritten.
    if !MIGRATIONS.is_empty() {
        backup_db_file()?; // state.redb → state.redb.bak-v{SCHEMA_VERSION}
    }
    for (idx, migration) in MIGRATIONS.iter().enumerate().skip(current as usize) {
        info!(from = idx, to = idx + 1, "applying database migration");
        migration(db)?;
    }
    // Final stamping: initializes a fresh/legacy DB (0 → 1) and is a no-op
    // when the last migration already stamped its target.
    stamp_schema_version(db, SCHEMA_VERSION)
}
```

### Legacy / pre-schema databases

A database with no `meta` table reports `current == 0`. Two cases, same
handling at release:

- **Freshly created DB** — `open_db`'s create path runs `run_migrations`,
  which stamps `schema_version = 1`. Nothing else happens.
- **Pre-release dev DB** (postcard-era blobs, no `meta`) — stamped the same
  way. The leftover postcard blobs are *not* migrated (that is the decision
  to skip a v0→v1 migration): `read_all_sessions` / `read_turns` already
  skip undecodable entries with a warning, so legacy sessions drop out
  loudly-but-non-fatally on first read. Acceptable because the project is
  unreleased and dev databases are throwaway. If a stricter posture is ever
  wanted, the `current == 0 && SCHEMA_VERSION > 1` branch in
  `run_migrations` is where a hard refusal would live (it already exists for
  once the chain grows).

### `open_db` hardening

Replace the current "any error → warn + recreate" catch-all:

```rust
match redb::Database::open(&path) {
    Ok(db) => Ok(db),
    Err(redb::DatabaseError::Storage(redb::StorageError::Io(e)))
        if e.kind() == io::ErrorKind::NotFound =>
    {
        // Fresh DB: create, then stamp the schema version via run_migrations.
        let db = redb::Database::create(path)?;
        run_migrations(&db)?;
        Ok(db)
    }
    Err(redb::DatabaseError::UpgradeRequired(actual)) => Err(io::Error::other(format!(
        "database file format version {actual} is not supported by this binary; \
         restore a backup (state.redb.bak-v*) or use the documented dump/restore path"
    ))),
    Err(e) => Err(io::Error::other(format!(
        "failed to open database (refusing to recreate a potentially corrupt file): {e}"
    ))),
}
```

### Startup wiring (`choreo-daemon/src/main.rs`)

```rust
let db = Arc::new(db::open_db().context("failed to open database")?);
db::run_migrations(&db).context("failed to migrate database")?;
// then existing purge_tombstoned_sessions + read_all_sessions
```

### Migration-writing rules (codify in ARCHITECTURE.md)

- **Additive change** (new field + `#[serde(default)]`, new enum variant
  appended): no migration, no version bump.
- **Breaking change** (reorder/remove/mid-insert field, reorder/remove enum
  variant, type change, key change, table split, codec change): new
  `migrate_vX_to_vX+1`, bump `SCHEMA_VERSION`.
- Future migrations that rewrite *historical* shapes must define frozen local
  copies of the old structs (the current shapes drift over time; a migration
  must decode what the old binary wrote, not what today's code looks like).
- Every migration gets a unit test that builds a fixture DB **as the old
  version would have written it**, runs the runner, and asserts the new
  version's contents + version stamp + idempotency + backup artifact.

---

## Implementation Order (serial; per AGENTS.md, one subagent at a time)

| # | Step | Crates | Est. |
|---|---|---|---|
| 1 | Add `rmp-serde = "1.3"` to `[workspace.dependencies]`; wire it into `choreo-proto` + `choreo-daemon` Cargo.tomls | root, proto, daemon | 0.5h |
| 2 | Swap `frame.rs` to `to_vec_named`/`from_slice_ref`, bump `PROTOCOL_VERSION`→2, rename `ProtoError::Postcard`→`Codec` (choreo-proto-only; verified by grep — client-core/acp use only `#[from]`/`Io`) | proto | 1.5h |
| 3 | Update proto round-trip tests; add named↔array tolerance + trailing-bytes tests | proto | 1h |
| 4 | Swap the 8 DB call sites to `to_vec_named`/`from_slice` | daemon | 1h |
| 5 | Promote `meta` to production; add `SCHEMA_VERSION`, `MIGRATIONS` (empty), `current_schema_version`, `stamp_schema_version`, `backup_db_file`, `run_migrations` (0→1 initialization, newer-version guard, no-backup-when-empty); harden `open_db` (NotFound vs UpgradeRequired vs other) | daemon | 2h |
| 6 | Wire `run_migrations` into `main.rs` after `open_db` | daemon | 0.5h |
| 7 | Unit tests in `db.rs` `#[cfg(test)]`: fresh DB stamps v1 (0→1 initialization); no-meta DB with legacy postcard blobs stamps v1 and skips undecodable reads with warnings; idempotent second run; newer-version DB errors; no backup written while the chain is empty | daemon | 1h |
| 8 | Integration test (crate-level `tests/`, `#[ignore]`): full `open_db`→`run_migrations`→`purge_tombstoned_sessions`→`read_all_sessions` cycle against a temp `CHOREOGRAPHR_DB_PATH` | daemon | 1h |
| 9 | Docs: ARCHITECTURE.md (wire format section, DB tables + migration contract, dependency table) and README.md (postcard → MessagePack note) | docs | 1h |
| 10 | Full gate: `cargo fmt`, `cargo clippy`, `cargo test-all`; manual smoke: run daemon against a copy of a pre-release dev DB, confirm stamping + legacy blobs skipped with warnings | — | 1h |

Total ≈ 12h (steps 4–8 were trimmed when the v0→v1 migration was dropped).

---

## Execution — subsessions (serial, one at a time)

Each subsession is spawned in order; the next starts only after the previous
diff is reviewed and committed by the parent (per AGENTS.md, subagents run in
series to avoid overlapping-file conflicts). **Resolved Decisions, risk
mitigations, and design alternatives are parent-owned: implement the scope as
written, do not resolve alternatives. Do not modify this plan document.**

| # | Subsession | Files | Verify |
|---|---|---|---|
| 1 | Wire: choreo-proto → MessagePack (steps 1–3) | root `Cargo.toml` (+`rmp-serde = "1.3"`), `choreo-proto/Cargo.toml` (−postcard, +rmp-serde), `src/frame.rs`, `src/error.rs`, proto tests | `cargo nextest run -p choreo-proto` |
| 2 | DB codec swap (step 4) | `choreo-daemon/Cargo.toml` (+rmp-serde), `src/db.rs` call sites | `cargo nextest run -p choreo-daemon` |
| 3 | Migration framework (steps 5–8) | `src/db.rs`, `src/main.rs`, db.rs unit tests, new crate-level `tests/` integration test (`#[ignore]`) | `cargo nextest run -p choreo-daemon` then `cargo nextest run -p choreo-daemon --run-ignored only` |
| 4 | Docs (step 9) | `ARCHITECTURE.md`, `README.md` | read-through |
| 5 | Gate (step 10) | any crate — fixes only | `cargo fmt`, `cargo clippy`, `cargo test-all` |

After the gate, delete this plan in its own commit (repo convention:
`Remove reasoning-roundtrip.md plan (already implemented)`). Its permanent
decisions live in ARCHITECTURE.md.

---

## Test Plan

**Unit (`src/`, `#[cfg(test)]`, no time-based waits):**
- `choreo-proto`: round-trips for all message types; version-rejection
  (`UnsupportedVersion`); `TrailingBytes`; `FrameTooLarge`; named↔array
  decode tolerance; enum-variant-name round-trip (reorder-safe guarantee).
- `db.rs`: fresh-DB stamp (0 → 1 initialization), no-meta legacy DB stamp +
  undecodable-entry skip, idempotency, newer-version rejection, backup
  only-when-migrations-exist.

**Integration (crate-level `tests/`, `#[ignore]`):**
- Full startup sequence against temp DB path.
- Mixed-version client simulation: encode with PROTOCOL_VERSION 1 (postcard),
  assert the daemon rejects with `UnsupportedVersion` (covered by proto unit
  tests; integration asserts the socket path if cheap).

**Command (per AGENTS.md):** `cargo nextest run -p choreo-proto -p choreo-daemon`
for the touched crates; `cargo test-all` before commit.

## Risks & Mitigations

| Risk | Mitigation |
|---|---|
| Old clients break on the wire bump | Expected and correct: `UnsupportedVersion` is already a first-class error; all in-tree clients update in the same change |
| `from_slice_ref` absent/renamed in rmp-serde 1.3.1 | Verify in step 2; fallback = `from_slice` + manual remainder probe |
| Migration rewriting blobs mid-failure leaves a half-migrated DB | Single redb write transaction per migration → atomic; runner aborts on error; `state.redb.bak-v1` exists before any write |
| Legacy postcard-era dev DB after the codec swap | Not migrated by design (unreleased): `read_all_sessions`/`read_turns` skip undecodable blobs with warnings; the hard-refuse option for later is documented in "Legacy / pre-schema databases" |
| Someone adds a JSON-only serde attribute (`skip_serializing_if`, `flatten`) to a shared type | Named MessagePack *tolerates* these on decode (name-based), but `skip_serializing_if` still changes wire bytes for other clients — keep the existing "shared types stay canonical" rule and note it in ARCHITECTURE.md |
| redb file-format bump in future | `UpgradeRequired` now hard-errors with backup/dump guidance instead of destroying data |
| Two codecs (postcard VM/creds + MessagePack wire/db) drift | Both are Rust-only sealed boundaries; document the "postcard = Rust-only internal, MessagePack = anything that crosses a language boundary" rule |

## Resolved Decisions (parent-owned — not open to subsession reinterpretation)

1. **Schema version granularity:** one global `SCHEMA_VERSION`. Per-table
   versions only if a table ever needs independent migration (revisit then).
2. **Backup retention (dormant until the first real migration):** one
   `state.redb.bak-v{n}` per version. Revisit when `MIGRATIONS` gets its
   first entry.
3. **Single-record reads on undecodable data:** keep skipping with a warning
   (same policy as batch reads); never fail the daemon on one bad record.

## Release Checklist

- [ ] `PROTOCOL_VERSION = 2` + MessagePack named shipped in **one** release
      with all in-tree clients.
- [ ] `SCHEMA_VERSION = 1` stamped on fresh DBs; pre-release dev DBs verified
      to start with legacy blobs skipped (warnings, no data migration).
- [ ] ARCHITECTURE.md documents: codec rule (MessagePack = language boundary,
      postcard = Rust-only internal), migration contract, additive vs breaking
      change policy.
- [ ] README.md "currently encoded via Postcard" paragraph updated.
