use schemars::JsonSchema;
use serde::{Deserialize, Serialize};

mod count;
mod delete;
mod delete_range;
mod get;
mod get_range;
mod list;
mod set;

pub(crate) use count::DbCount;
pub(crate) use delete::DbDelete;
pub(crate) use delete_range::DbDeleteRange;
pub(crate) use get::DbGet;
pub(crate) use get_range::DbGetRange;
pub(crate) use list::DbList;
pub(crate) use set::DbSet;

/// Database tool errors — a structured error type for VM guests to match on.
#[derive(Debug, Serialize, Deserialize, thiserror::Error)]
pub enum DbError {
    #[error("key not found: {0}")]
    NotFound(String),
    #[error("no session context")]
    NoSessionContext,
    #[error("storage error: {0}")]
    Storage(String),
}

// ── DbValue: binary-safe value wrapper ──────────────────────────────────

/// A database value that preserves raw bytes through postcard (VM path)
/// while presenting as a plain string through JSON (LLM path).
#[derive(Debug, Clone)]
pub struct DbValue(pub Vec<u8>);

impl Serialize for DbValue {
    fn serialize<S: serde::Serializer>(&self, serializer: S) -> Result<S::Ok, S::Error> {
        if serializer.is_human_readable() {
            serializer.serialize_str(&String::from_utf8_lossy(&self.0))
        } else {
            serializer.serialize_bytes(&self.0)
        }
    }
}

impl<'de> Deserialize<'de> for DbValue {
    fn deserialize<D: serde::Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
        if deserializer.is_human_readable() {
            let s = String::deserialize(deserializer)?;
            Ok(DbValue(s.into_bytes()))
        } else {
            Vec::<u8>::deserialize(deserializer).map(DbValue)
        }
    }
}

impl JsonSchema for DbValue {
    fn schema_name() -> std::borrow::Cow<'static, str> {
        std::borrow::Cow::Borrowed("DbValue")
    }

    fn schema_id() -> std::borrow::Cow<'static, str> {
        std::borrow::Cow::Borrowed(concat!(module_path!(), "::DbValue"))
    }

    fn json_schema(_gen: &mut schemars::SchemaGenerator) -> schemars::Schema {
        schemars::json_schema!({
            "type": "string",
            "description": "The value to store"
        })
    }
}

#[cfg(test)]
pub(crate) mod tests {
    use super::*;
    use crate::tools::context::ToolContext;
    use std::sync::Arc;

    /// Build a temporary redb Database + ToolContext with a fresh channel.
    /// Shared by all seven db tool files' test modules so they each get an
    /// isolated, pre-warmed session store.
    pub(crate) fn test_context() -> (tempfile::TempDir, ToolContext) {
        let dir = tempfile::tempdir().unwrap();
        let db = Arc::new(redb::Database::create(dir.path().join("test.redb")).unwrap());
        // Note: `crate::db` is the redb KV layer; this module is
        // `crate::tools::db`, so the absolute path avoids any name shadowing
        // between the two `db` modules.
        crate::db::kv_set(&db, 0, "__init__", b"").unwrap();
        crate::db::kv_delete(&db, 0, "__init__").unwrap();
        let (daemon_tx, _daemon_rx) = std::sync::mpsc::channel();
        let ctx = ToolContext::new(42, db, daemon_tx);
        (dir, ctx)
    }

    // ── DbError postcard round trip ────────────────────────────────

    #[test]
    fn db_error_postcard_round_trip() {
        let errors = vec![
            DbError::NotFound("my_key".into()),
            DbError::NoSessionContext,
            DbError::Storage("disk full".into()),
        ];
        for err in &errors {
            let encoded = postcard::to_allocvec(err).unwrap();
            let decoded: DbError = postcard::from_bytes(&encoded).unwrap();
            assert_eq!(err.to_string(), decoded.to_string());
        }
    }
}
