use std::fs;
use std::io;
use std::path::PathBuf;

use redb::ReadableDatabase;
use redb::ReadableTable;
use redb::TableDefinition;
use serde::{Deserialize, Serialize};
use tai_proto::SessionMessage;

const SESSIONS: TableDefinition<u64, &[u8]> = TableDefinition::new("sessions");
const SESSION_MESSAGES: TableDefinition<(u64, u32), &[u8]> =
    TableDefinition::new("session_messages");
const META: TableDefinition<&str, u64> = TableDefinition::new("meta");

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SessionRecord {
    pub title: Option<String>,
    pub selected_model: Option<String>,
    pub parent_session_id: Option<u64>,
    pub cwd: Option<String>,
    pub max_turns: Option<u32>,
    pub message_count: u32,
    pub created_at: i64,
    pub active_categories: Vec<String>,
}

pub fn db_path() -> io::Result<PathBuf> {
    if let Ok(override_path) = std::env::var("TAI_DB_PATH") {
        return Ok(PathBuf::from(override_path));
    }
    let data_dir = dirs::data_dir().ok_or_else(|| {
        io::Error::new(
            io::ErrorKind::NotFound,
            "could not determine data directory",
        )
    })?;
    Ok(data_dir.join("tai-daemon").join("state.redb"))
}

pub fn open_db() -> io::Result<redb::Database> {
    let path = db_path()?;
    if let Some(parent) = path.parent() {
        fs::create_dir_all(parent)?;
    }
    redb::Database::create(path).map_err(|e| {
        io::Error::new(
            io::ErrorKind::Other,
            format!("failed to open database: {e}"),
        )
    })
}

pub fn write_session(
    db: &redb::Database,
    session_id: u64,
    record: &SessionRecord,
) -> io::Result<()> {
    let payload =
        bincode::serde::encode_to_vec(record, bincode::config::standard()).map_err(|e| {
            io::Error::new(io::ErrorKind::Other, format!("bincode encode session: {e}"))
        })?;
    let write_txn = db
        .begin_write()
        .map_err(|e| io::Error::new(io::ErrorKind::Other, format!("redb write txn: {e}")))?;
    {
        let mut table = write_txn.open_table(SESSIONS).map_err(|e| {
            io::Error::new(io::ErrorKind::Other, format!("redb open sessions: {e}"))
        })?;
        table.insert(session_id, payload.as_slice()).map_err(|e| {
            io::Error::new(io::ErrorKind::Other, format!("redb insert session: {e}"))
        })?;
    }
    write_txn
        .commit()
        .map_err(|e| io::Error::new(io::ErrorKind::Other, format!("redb commit session: {e}")))?;
    Ok(())
}

pub fn read_session(db: &redb::Database, session_id: u64) -> io::Result<Option<SessionRecord>> {
    let read_txn = db
        .begin_read()
        .map_err(|e| io::Error::new(io::ErrorKind::Other, format!("redb read txn: {e}")))?;
    let table = read_txn
        .open_table(SESSIONS)
        .map_err(|e| io::Error::new(io::ErrorKind::Other, format!("redb open sessions: {e}")))?;
    match table
        .get(session_id)
        .map_err(|e| io::Error::new(io::ErrorKind::Other, format!("redb get session: {e}")))?
    {
        Some(guard) => {
            let record: SessionRecord =
                bincode::serde::decode_from_slice(guard.value(), bincode::config::standard())
                    .map_err(|e| {
                        io::Error::new(io::ErrorKind::Other, format!("bincode decode session: {e}"))
                    })?
                    .0;
            Ok(Some(record))
        }
        None => Ok(None),
    }
}

pub fn read_all_sessions(db: &redb::Database) -> io::Result<Vec<(u64, SessionRecord)>> {
    let read_txn = db
        .begin_read()
        .map_err(|e| io::Error::new(io::ErrorKind::Other, format!("redb read txn: {e}")))?;
    let table = read_txn
        .open_table(SESSIONS)
        .map_err(|e| io::Error::new(io::ErrorKind::Other, format!("redb open sessions: {e}")))?;
    let mut sessions: Vec<(u64, SessionRecord)> = Vec::new();
    for result in table
        .iter()
        .map_err(|e| io::Error::new(io::ErrorKind::Other, format!("redb iter sessions: {e}")))?
    {
        let (key, value) = result
            .map_err(|e| io::Error::new(io::ErrorKind::Other, format!("redb iter item: {e}")))?;
        let record: SessionRecord =
            bincode::serde::decode_from_slice(value.value(), bincode::config::standard())
                .map_err(|e| {
                    io::Error::new(io::ErrorKind::Other, format!("bincode decode session: {e}"))
                })?
                .0;
        sessions.push((key.value(), record));
    }
    sessions.sort_by_key(|(id, _)| *id);
    Ok(sessions)
}

pub fn delete_session(db: &redb::Database, session_id: u64) -> io::Result<()> {
    let write_txn = db
        .begin_write()
        .map_err(|e| io::Error::new(io::ErrorKind::Other, format!("redb write txn: {e}")))?;
    {
        let mut sessions = write_txn.open_table(SESSIONS).map_err(|e| {
            io::Error::new(io::ErrorKind::Other, format!("redb open sessions: {e}"))
        })?;
        sessions.remove(session_id).map_err(|e| {
            io::Error::new(io::ErrorKind::Other, format!("redb remove session: {e}"))
        })?;
    }
    {
        let mut messages = write_txn.open_table(SESSION_MESSAGES).map_err(|e| {
            io::Error::new(io::ErrorKind::Other, format!("redb open messages: {e}"))
        })?;
        let keys_to_remove: Vec<(u64, u32)> = messages
            .iter()
            .map_err(|e| io::Error::new(io::ErrorKind::Other, format!("redb iter messages: {e}")))?
            .filter_map(|result| {
                result.ok().and_then(|(key, _)| {
                    if key.value().0 == session_id {
                        Some(key.value())
                    } else {
                        None
                    }
                })
            })
            .collect();
        for key in keys_to_remove {
            messages.remove(key).map_err(|e| {
                io::Error::new(io::ErrorKind::Other, format!("redb remove message: {e}"))
            })?;
        }
    }
    write_txn
        .commit()
        .map_err(|e| io::Error::new(io::ErrorKind::Other, format!("redb commit delete: {e}")))?;
    Ok(())
}

pub fn write_message(
    db: &redb::Database,
    session_id: u64,
    index: u32,
    message: &SessionMessage,
) -> io::Result<()> {
    let payload =
        bincode::serde::encode_to_vec(message, bincode::config::standard()).map_err(|e| {
            io::Error::new(io::ErrorKind::Other, format!("bincode encode message: {e}"))
        })?;
    let write_txn = db
        .begin_write()
        .map_err(|e| io::Error::new(io::ErrorKind::Other, format!("redb write txn: {e}")))?;
    {
        let mut table = write_txn.open_table(SESSION_MESSAGES).map_err(|e| {
            io::Error::new(io::ErrorKind::Other, format!("redb open messages: {e}"))
        })?;
        table
            .insert((session_id, index), payload.as_slice())
            .map_err(|e| {
                io::Error::new(io::ErrorKind::Other, format!("redb insert message: {e}"))
            })?;
    }
    write_txn
        .commit()
        .map_err(|e| io::Error::new(io::ErrorKind::Other, format!("redb commit message: {e}")))?;
    Ok(())
}

pub fn read_messages(db: &redb::Database, session_id: u64) -> io::Result<Vec<SessionMessage>> {
    let read_txn = db
        .begin_read()
        .map_err(|e| io::Error::new(io::ErrorKind::Other, format!("redb read txn: {e}")))?;
    let table = read_txn
        .open_table(SESSION_MESSAGES)
        .map_err(|e| io::Error::new(io::ErrorKind::Other, format!("redb open messages: {e}")))?;
    let mut messages: Vec<(u32, SessionMessage)> = Vec::new();
    for result in table
        .iter()
        .map_err(|e| io::Error::new(io::ErrorKind::Other, format!("redb iter messages: {e}")))?
    {
        let (key, value) = result
            .map_err(|e| io::Error::new(io::ErrorKind::Other, format!("redb iter item: {e}")))?;
        let (sid, idx) = key.value();
        if sid == session_id {
            let message: SessionMessage =
                bincode::serde::decode_from_slice(value.value(), bincode::config::standard())
                    .map_err(|e| {
                        io::Error::new(io::ErrorKind::Other, format!("bincode decode message: {e}"))
                    })?
                    .0;
            messages.push((idx, message));
        }
    }
    messages.sort_by_key(|(idx, _)| *idx);
    Ok(messages.into_iter().map(|(_, msg)| msg).collect())
}

pub fn next_session_id(db: &redb::Database) -> io::Result<u64> {
    let write_txn = db
        .begin_write()
        .map_err(|e| io::Error::new(io::ErrorKind::Other, format!("redb write txn: {e}")))?;
    let current = {
        let mut table = write_txn
            .open_table(META)
            .map_err(|e| io::Error::new(io::ErrorKind::Other, format!("redb open meta: {e}")))?;
        let current = table
            .get("next_session_id")
            .map_err(|e| io::Error::new(io::ErrorKind::Other, format!("redb get meta: {e}")))?
            .map(|g| g.value())
            .unwrap_or(1);
        table
            .insert("next_session_id", current.wrapping_add(1))
            .map_err(|e| io::Error::new(io::ErrorKind::Other, format!("redb set meta: {e}")))?;
        current
    };
    write_txn
        .commit()
        .map_err(|e| io::Error::new(io::ErrorKind::Other, format!("redb commit meta: {e}")))?;
    Ok(current)
}

pub fn write_message_retry(
    db: &redb::Database,
    session_id: u64,
    index: u32,
    message: &SessionMessage,
) -> io::Result<()> {
    let mut attempts = 0;
    loop {
        match write_message(db, session_id, index, message) {
            Ok(()) => return Ok(()),
            Err(e) => {
                let err_str = e.to_string();
                if err_str.contains("database is busy") && attempts < 3 {
                    attempts += 1;
                    std::thread::sleep(std::time::Duration::from_millis(1));
                    continue;
                }
                return Err(e);
            }
        }
    }
}

pub fn write_session_retry(
    db: &redb::Database,
    session_id: u64,
    record: &SessionRecord,
) -> io::Result<()> {
    let mut attempts = 0;
    loop {
        match write_session(db, session_id, record) {
            Ok(()) => return Ok(()),
            Err(e) => {
                let err_str = e.to_string();
                if err_str.contains("database is busy") && attempts < 3 {
                    attempts += 1;
                    std::thread::sleep(std::time::Duration::from_millis(1));
                    continue;
                }
                return Err(e);
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use tai_proto::SessionMessage;

    #[test]
    fn round_trip() {
        let dir = tempfile::tempdir().unwrap();
        let db = redb::Database::create(dir.path().join("test.redb")).unwrap();

        let id = next_session_id(&db).unwrap();
        assert_eq!(id, 1);

        let record = SessionRecord {
            title: Some("test session".into()),
            selected_model: Some("gpt-4".into()),
            parent_session_id: None,
            cwd: Some("/tmp".into()),
            max_turns: None,
            message_count: 1,
            created_at: 1234567890,
            active_categories: vec!["core".into(), "git".into()],
        };
        write_session(&db, id, &record).unwrap();

        let read = read_session(&db, id).unwrap().unwrap();
        assert_eq!(read.title, record.title);
        assert_eq!(read.message_count, record.message_count);

        let all = read_all_sessions(&db).unwrap();
        assert_eq!(all.len(), 1);
        assert_eq!(all[0].0, id);

        let msg = SessionMessage::UserText {
            content: "hello".into(),
        };
        write_message(&db, id, 0, &msg).unwrap();

        let messages = read_messages(&db, id).unwrap();
        assert_eq!(messages.len(), 1);
        assert_eq!(messages[0], msg);

        let id2 = next_session_id(&db).unwrap();
        assert_eq!(id2, 2);

        delete_session(&db, id).unwrap();
        assert!(read_session(&db, id).unwrap().is_none());
        assert!(read_messages(&db, id).unwrap().is_empty());

        drop(db);
    }
}
