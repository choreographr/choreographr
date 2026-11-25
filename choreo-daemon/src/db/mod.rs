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
//! `poison` (escalation from a `redb` handle that latched an I/O failure) —
//! and their public items are re-exported here so `db::…` call sites are
//! unchanged. The one thing that stays put is the error conversion every child
//! funnels its failures through: `db_err`.
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

use std::io;

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

// Startup space reclamation: the decision is a pure function of a measured
// footprint, so it is testable with no database and no clock.
mod compact;
pub(crate) use compact::maybe_compact;

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

// Detection of a `redb` handle that latched a permanent I/O failure, and the
// escalation to a daemon shutdown when one is. It rides `db_err` below rather
// than each call site, so every storage failure — read or write — is covered by
// one hook.
mod poison;
pub(crate) use poison::install as install_poison_escalation;

// The table definitions and key constants every child reaches for.
mod schema;

// The session-record and turn CRUD, including the deletion tombstones.
mod sessions;
pub use sessions::{
    SessionRecord, clear_session_tombstone, delete_session, delete_session_turns,
    mark_session_deleted, purge_tombstoned_sessions, read_all_sessions, read_session, read_turns,
    update_session_flags, write_session, write_turn,
};

fn db_err(msg: String) -> io::Error {
    // Every storage failure in this module is converted here, so this is the one
    // place that can notice a `redb` handle latching itself unusable (see
    // `poison`). Cheap when it has not: the probe is a flag read plus one atomic.
    poison::observe(&msg);
    io::Error::other(msg)
}
