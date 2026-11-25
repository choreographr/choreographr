//! The store's on-disk shape: the `redb` table definitions, the keys addressed
//! inside the string-keyed tables, and the range bound the per-session scans
//! share.
//!
//! One module owns the layout so no child can drift from the shape the
//! migration chain was written against. The constants are `pub(super)` — every
//! `db` child reaches them through this module, and nothing outside `db` sees a
//! table name. A table is created lazily on its first write, so adding one is
//! additive (no schema-version bump and no migration entry) unless it changes
//! the meaning of a row that already exists, which is what
//! [`SCHEMA_VERSION`](super::SCHEMA_VERSION) guards.

use redb::TableDefinition;

pub(super) const SESSIONS: TableDefinition<u64, &[u8]> = TableDefinition::new("sessions");
pub(super) const SESSION_TURNS: TableDefinition<(u64, u32), &[u8]> =
    TableDefinition::new("session_turns");
pub(super) const CREDENTIALS: TableDefinition<&str, &[u8]> = TableDefinition::new("credentials");
/// Production `meta` table: string keys, u64 values. Holds the persisted
/// schema version under [`SCHEMA_VERSION_KEY`]; the test-only
/// `next_session_id` counter shares the same table (test key, shared table).
pub(super) const META: TableDefinition<&str, u64> = TableDefinition::new("meta");
/// Runtime catalog-refresh state (S4): the last models.dev fetch-attempt
/// timestamp and the current etag. One table for both so the refresh state is
/// a single coherent record. A new table is created lazily on first write, so
/// adding it is purely additive — no schema version bump and no migration
/// entry is needed for it.
pub(super) const CATALOG_STATE: TableDefinition<&str, &[u8]> =
    TableDefinition::new("catalog_state");
/// Key for the last models.dev fetch-attempt timestamp (Unix epoch millis, 8
/// bytes little-endian). Written BEFORE every fetch: the cooldown is armed at
/// attempt start, so a daemon that crashes mid-fetch and restarts immediately
/// reads a fresh timestamp and honors the remaining cooldown instead of
/// re-fetching.
pub(super) const CATALOG_LAST_ATTEMPT_KEY: &str = "last_attempt_ms";
/// Key for the models.dev etag (UTF-8 bytes of the raw entity-tag). Written by
/// the daemon command loop AFTER the cache bin is persisted, so the etag
/// always describes content at least as new as what is on disk (bin-first
/// ordering keeps a crash between the two writes paired with the OLD content,
/// which self-heals via a 200 on the next fetch).
pub(super) const CATALOG_ETAG_KEY: &str = "etag";
pub(super) const SESSION_KV: TableDefinition<(u64, String), Vec<u8>> =
    TableDefinition::new("session_kv");
/// Tombstones for deleted sessions whose still-shutting-down thread may
/// re-create the record.  Keyed by session id; present means "deleted — purge
/// any record bearing this id at next startup" (see
/// [`purge_tombstoned_sessions`](super::purge_tombstoned_sessions)).
pub(super) const DELETED_SESSIONS: TableDefinition<u64, ()> =
    TableDefinition::new("deleted_sessions");

/// Iterator type returned by redb range queries on `SESSION_KV`.
pub(super) type KvRangeIter<'a> = Box<
    dyn Iterator<
            Item = Result<
                (
                    redb::AccessGuard<'a, (u64, String)>,
                    redb::AccessGuard<'a, Vec<u8>>,
                ),
                redb::StorageError,
            >,
        > + 'a,
>;

/// Key under which the current schema version is stored in [`META`].
pub(super) const SCHEMA_VERSION_KEY: &str = "schema_version";

/// Per-daemon keystore binding (TOFU design, DESIGN-keystore-unlock.md):
/// key `"binding"` → the 32-byte X25519 public key derived from the daemon's
/// unlock (keystore private) key. Created lazily on first write, so adding
/// the table is purely additive — no schema version bump and no migration.
pub(super) const KEYSTORE: TableDefinition<&str, &[u8]> = TableDefinition::new("keystore");
/// The single key under which the keystore binding is stored in [`KEYSTORE`].
pub(super) const KEYSTORE_BINDING_KEY: &str = "binding";

/// Exclusive upper-bound session id for the range queries that span a single
/// session's keys: `(session_id, …)..(session_range_end(session_id), …)`
/// covers every key whose first tuple element is `session_id`.
///
/// `saturating_add` keeps the bound total even at the theoretical
/// `session_id == u64::MAX` (which the daemon's monotonic id counter can
/// never reach in practice): the range would simply be empty for that id
/// instead of overflowing (debug) or wrapping (release).
///
/// `pub(super)` so every `db` child that spans one session's keys — the
/// `sessions`/`attachments` range scans and the `kv` range helpers — shares
/// this one bound rule; production callers outside `db` never see it.
pub(super) fn session_range_end(session_id: u64) -> u64 {
    session_id.saturating_add(1)
}
