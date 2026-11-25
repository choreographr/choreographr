//! The daemon's durable store: a `redb` embedded key-value database holding
//! sessions, turns, credentials, the per-session KV store, deletion tombstones,
//! the keystore binding, and the runtime catalog-refresh state.
//!
//! This module is the facade. Each concern lives in a cohesive child module —
//! `schema` (the table definitions and the key constants every child reaches
//! for), `open` (resolving, opening, hardening and backing up the file),
//! `migrate` (the schema version and the migration runner), `sessions` (session
//! records, turns and deletion tombstones), `attachments` (the raw image byte
//! store), `codec` (the zstd turn-value codec), `credentials` (credential blobs
//! and the keystore binding), `catalog` (the catalog-refresh state), `kv` (the
//! per-session key-value store), `compact` (startup space reclamation), and
//! `store` (the [`Store`] handle every child takes, which owns the `redb`
//! database and the latch escalation that rides it) — and their public items
//! are re-exported here so `db::…` call sites are unchanged.
//!
//! Every child takes a [`Store`] rather than a bare `redb::Database`, and builds
//! its errors with `Store::err`. That is the module's single error conversion,
//! and the one place able to notice a `redb` handle that latched a permanent I/O
//! failure — see `store`. A failure that is *not* storage, e.g. a corrupt row the
//! codec cannot decode, is built with `io::Error::other` instead: a data error
//! says nothing about the handle.
//!
//! All I/O is synchronous `redb` transactions on the calling thread; `redb`'s
//! `Database` is safe for concurrent readers, so one shared handle is threaded
//! to every connection thread for on-demand image reads. Every write commits at
//! `redb`'s default `Durability::Immediate` — the daemon never lowers it — so a
//! committed row has been fsynced and a crash loses only what was never
//! committed.
//!
//! Range queries go through `redb`'s `KeyRange` abstraction, whose key type is
//! fixed by the `TableDefinition` the table was opened with: the call is
//! `table.range(start..end)`, with **no** key-type turbofish (a turbofish does
//! not compile). Every key and value type here is a `redb` built-in whose
//! encoding is unchanged from `redb` 4, so an existing `state.redb` opens as-is
//! — no `Legacy` wrapper and no migration entry — because the only encodings
//! `redb` 5 changed are the `Option<&str>`/`Option<String>`/`Option<bool>`/
//! `Option<char>` ones, and this schema stores every record as opaque bytes
//! (`&[u8]` / `Vec<u8>`) rather than as a `redb`-encoded `Option`.
//!
//! The store holds plaintext transcripts and tool output, so its filesystem
//! permissions are part of the access control: [`open_db_at`] keeps the data
//! directory owner-only and forces the database file (and every pre-migration
//! backup) to mode `0600`.

// Every child module below owns one concern; each is listed here so the
// facade's re-exports are the only names `db::…` call sites depend on.

// The turn attachment byte store (raw image bytes kept out of the compressed
// turn blob) owns the attachment table, the slot naming, and every attachment
// read/write/delete.
mod attachments;
pub use attachments::{read_attachment, write_attachment};

// The runtime catalog-refresh state: the fetch-attempt timestamp and the etag.
mod catalog;
pub use catalog::{
    get_catalog_etag, get_catalog_last_attempt_ms, set_catalog_etag, set_catalog_last_attempt_ms,
};

// The `session_turns` value codec (zstd): constants, encode/decode, and the
// legacy-frame fixtures. Reached directly by `sessions` and `migrate`, which
// are the only callers.
mod codec;

// Startup space reclamation: one unconditional pass with `Maximum` before the
// store is shared, plus the test-only seam the integration suite drives it
// through (see the module docs for why there is no gate).
mod compact;
pub(crate) use compact::compact_at_startup;
#[cfg(any(test, feature = "test-utils"))]
pub use compact::compact_at_startup_for_test;

// The daemon's stored secrets: the encrypted credential blobs and the keystore
// binding adopted on first contact.
mod credentials;
pub use credentials::{
    get_all_credential_blobs, get_keystore_binding, remove_credential_blob, set_credential_blob,
    set_keystore_binding,
};

// The per-session key-value store behind the `db_*` tools.
mod kv;
pub use kv::{kv_count, kv_delete, kv_delete_range, kv_get, kv_get_range, kv_list, kv_set};

// The schema version, the migration chain, and the runner that applies it.
mod migrate;
pub use migrate::{
    INITIAL_SCHEMA_VERSION, SCHEMA_VERSION, run_migrations, run_migrations_at, schema_version,
};

// Opening, hardening and backing up the database file.
mod open;
pub(crate) use open::migration_backup_version;
pub use open::{backup_database, db_path, open_db, open_db_at};

// The durable handle: the `redb` database plus the escalation state of the
// handle it owns. It rides `Store::err` rather than each call site, so every
// storage failure — read or write, on any path — is covered by one hook.
mod store;
// Re-exported at the module root so a `test-utils` build (the integration suite)
// can drive a latched handle through the same backend the unit tests use. Behind
// the feature, so it never reaches the published public API.
pub use store::Store;
#[cfg(any(test, feature = "test-utils"))]
pub use store::test_backend::BreakableBackend;

// The table definitions and key constants every child reaches for.
mod schema;

// The session-record and turn CRUD, including the deletion tombstones.
mod sessions;
pub use sessions::{
    SessionRecord, clear_session_tombstone, delete_session, delete_session_turns,
    mark_session_deleted, purge_tombstoned_sessions, read_all_sessions, read_session, read_turns,
    update_session_flags, write_session, write_turn,
};
