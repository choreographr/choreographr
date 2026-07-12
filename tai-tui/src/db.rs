use std::fs;
use std::io;
use std::path::PathBuf;

#[cfg(test)]
thread_local! {
    // Thread-local override for test database path.
    // Each test thread sets its own temp directory so parallel tests don't
    // collide on the same database file and don't read/write the real one.

    static TEST_DB_PATH: std::cell::RefCell<Option<PathBuf>> = const { std::cell::RefCell::new(None) };
}

#[cfg(test)]
pub fn set_test_db_path(path: PathBuf) {
    TEST_DB_PATH.with(|cell| {
        *cell.borrow_mut() = Some(path);
    });
}

use redb::{ReadableDatabase, ReadableTable, TableDefinition};
use serde::{Deserialize, Serialize};

const COMMAND_HISTORY: TableDefinition<u64, &[u8]> = TableDefinition::new("command_history");
const META: TableDefinition<&str, u64> = TableDefinition::new("meta");

/// Maximum number of command history entries to keep.
const MAX_HISTORY: u64 = 1000;

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct CommandEntry {
    pub command: String,
    pub timestamp: i64,
}

/// Resolve the database path.
///
/// Uses `TAI_TUI_DB_PATH` env var if set, otherwise
/// `~/.local/share/tai-tui/state.redb`.
pub fn db_path() -> io::Result<PathBuf> {
    // When a test thread has registered an override, use that instead of the
    // real or env-var path.  Thread-local storage keeps parallel tests isolated.
    #[cfg(test)]
    if let Some(path) = TEST_DB_PATH.with(|cell| cell.borrow().clone()) {
        return Ok(path);
    }

    if let Ok(override_path) = std::env::var("TAI_TUI_DB_PATH") {
        return Ok(PathBuf::from(override_path));
    }
    let data_dir = dirs::data_dir().ok_or_else(|| {
        io::Error::new(
            io::ErrorKind::NotFound,
            "could not determine data directory",
        )
    })?;
    Ok(data_dir.join("tai-tui").join("state.redb"))
}

/// Open (or create) the redb database.
pub fn open_db() -> io::Result<redb::Database> {
    let path = db_path()?;
    if let Some(parent) = path.parent() {
        fs::create_dir_all(parent)?;
    }
    // Try open first (fails if file doesn't exist), then fall back to create.
    match redb::Database::open(&path) {
        Ok(db) => return Ok(db),
        Err(redb::DatabaseError::Storage(redb::StorageError::Io(io_err)))
            if io_err.kind() == std::io::ErrorKind::NotFound =>
        {
            // File doesn't exist yet — create below.
        }
        Err(_) => {
            #[cfg(not(test))]
            tracing::info!("[tai-tui] failed to open existing db, recreating");
        }
    }
    redb::Database::create(path)
        .map_err(|e| io::Error::other(format!("failed to open/create database: {e}")))
}

/// Save a command entry and return the assigned ID.
///
/// Trims entries beyond `MAX_HISTORY` so the database doesn't grow unbounded.
pub fn save_command(db: &redb::Database, entry: &CommandEntry) -> io::Result<u64> {
    let write_txn = db
        .begin_write()
        .map_err(|e| io::Error::other(format!("redb write txn: {e}")))?;

    // Allocate the next command ID and bump the counter.
    let cmd_id = {
        let mut table = write_txn
            .open_table(META)
            .map_err(|e| io::Error::other(format!("redb open meta: {e}")))?;
        let current = table
            .get("next_cmd_id")
            .map_err(|e| io::Error::other(format!("redb get meta: {e}")))?
            .map(|g| g.value())
            .unwrap_or(1);
        table
            .insert("next_cmd_id", current.wrapping_add(1))
            .map_err(|e| io::Error::other(format!("redb set meta: {e}")))?;
        current
    };

    // Insert the entry.
    {
        let payload = postcard::to_allocvec(entry)
            .map_err(|e| io::Error::other(format!("postcard encode entry: {e}")))?;
        let mut table = write_txn
            .open_table(COMMAND_HISTORY)
            .map_err(|e| io::Error::other(format!("redb open command_history: {e}")))?;
        table
            .insert(cmd_id, payload.as_slice())
            .map_err(|e| io::Error::other(format!("redb insert entry: {e}")))?;
    }

    // Trim old entries if we've exceeded the cap.
    trim_old_entries(&write_txn, cmd_id)?;

    write_txn
        .commit()
        .map_err(|e| io::Error::other(format!("redb commit: {e}")))?;

    Ok(cmd_id)
}

/// Remove entries with IDs below the retention threshold.
///
/// Since IDs are monotonically increasing, any entry with `id <= cmd_id - MAX_HISTORY`
/// is older than the retention window and can be deleted.
fn trim_old_entries(write_txn: &redb::WriteTransaction, last_id: u64) -> io::Result<()> {
    if last_id <= MAX_HISTORY {
        return Ok(());
    }
    let cutoff = last_id.saturating_sub(MAX_HISTORY);
    let mut table = write_txn
        .open_table(COMMAND_HISTORY)
        .map_err(|e| io::Error::other(format!("redb open for trim: {e}")))?;
    let keys_to_remove: Vec<u64> = table
        .iter()
        .map_err(|e| io::Error::other(format!("redb iter for trim: {e}")))?
        .filter_map(|result| {
            result.ok().and_then(|(key, _)| {
                if key.value() <= cutoff {
                    Some(key.value())
                } else {
                    None
                }
            })
        })
        .collect();
    for key in keys_to_remove {
        table
            .remove(key)
            .map_err(|e| io::Error::other(format!("redb remove for trim: {e}")))?;
    }
    Ok(())
}

/// Load the most recent command entries, newest first.
pub fn load_recent_commands(db: &redb::Database, limit: usize) -> io::Result<Vec<CommandEntry>> {
    let read_txn = db
        .begin_read()
        .map_err(|e| io::Error::other(format!("redb read txn: {e}")))?;
    // Table may not exist on first run — return empty in that case.
    let table = match read_txn.open_table(COMMAND_HISTORY) {
        Ok(t) => t,
        Err(_) => return Ok(Vec::new()),
    };

    // Collect all entries, then sort newest-first and take `limit`.
    let mut entries: Vec<(u64, CommandEntry)> = Vec::new();
    for result in table
        .iter()
        .map_err(|e| io::Error::other(format!("redb iter: {e}")))?
    {
        let (key, value) = result.map_err(|e| io::Error::other(format!("redb iter item: {e}")))?;
        match postcard::from_bytes::<CommandEntry>(value.value()) {
            Ok(entry) => entries.push((key.value(), entry)),
            Err(e) => {
                tracing::warn!(
                    "[tai-tui] skipping corrupt history entry {}: {e}",
                    key.value()
                );
            }
        }
    }

    // Sort by ID descending (newest first), take limit.
    entries.sort_by_key(|b| std::cmp::Reverse(b.0));
    Ok(entries.into_iter().take(limit).map(|(_, e)| e).collect())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn test_db() -> redb::Database {
        let dir = tempfile::tempdir().unwrap();
        redb::Database::create(dir.path().join("test.redb")).unwrap()
    }

    #[test]
    fn save_and_load_empty() {
        let db = test_db();
        let loaded = load_recent_commands(&db, 10).unwrap();
        assert!(loaded.is_empty());
    }

    #[test]
    fn save_and_load_single() {
        let db = test_db();
        let entry = CommandEntry {
            command: "hello".into(),
            timestamp: 1000,
        };
        let id = save_command(&db, &entry).unwrap();
        assert_eq!(id, 1);

        let loaded = load_recent_commands(&db, 10).unwrap();
        assert_eq!(loaded.len(), 1);
        assert_eq!(loaded[0].command, "hello");
        assert_eq!(loaded[0].timestamp, 1000);
    }

    #[test]
    fn save_and_load_multiple_newest_first() {
        let db = test_db();
        for i in 0..5 {
            save_command(
                &db,
                &CommandEntry {
                    command: format!("cmd-{i}"),
                    timestamp: i as i64,
                },
            )
            .unwrap();
        }
        let loaded = load_recent_commands(&db, 10).unwrap();
        assert_eq!(loaded.len(), 5);
        // Newest first — cmd-4, cmd-3, cmd-2, cmd-1, cmd-0
        assert_eq!(loaded[0].command, "cmd-4");
        assert_eq!(loaded[4].command, "cmd-0");
    }

    #[test]
    fn respects_limit() {
        let db = test_db();
        for i in 0..10 {
            save_command(
                &db,
                &CommandEntry {
                    command: format!("cmd-{i}"),
                    timestamp: i as i64,
                },
            )
            .unwrap();
        }
        let loaded = load_recent_commands(&db, 3).unwrap();
        assert_eq!(loaded.len(), 3);
        assert_eq!(loaded[0].command, "cmd-9");
        assert_eq!(loaded[2].command, "cmd-7");
    }

    #[test]
    fn trims_beyond_max() {
        let db = test_db();
        // Insert MAX_HISTORY + 10 entries
        for i in 0..MAX_HISTORY + 10 {
            save_command(
                &db,
                &CommandEntry {
                    command: format!("cmd-{i}"),
                    timestamp: i as i64,
                },
            )
            .unwrap();
        }
        // We should have at most MAX_HISTORY entries
        let loaded = load_recent_commands(&db, MAX_HISTORY as usize + 100).unwrap();
        assert!(loaded.len() <= MAX_HISTORY as usize);
        // The first entry should be the newest (largest ID)
        assert_eq!(loaded[0].command, format!("cmd-{}", MAX_HISTORY + 9));
        // The oldest entry should be cmd-9 or older (not cmd-0)
        assert_ne!(loaded.last().unwrap().command, "cmd-0");
    }

    #[test]
    fn db_path_uses_thread_local_override() {
        let dir = tempfile::tempdir().unwrap();
        let custom_path = dir.path().join("custom.redb");
        super::set_test_db_path(custom_path.clone());
        let path = db_path().unwrap();
        assert_eq!(path, custom_path);
    }
}
