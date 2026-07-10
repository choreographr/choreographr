use crate::db;
use crate::tools::context::ToolContext;
use crate::tools::{Tool, ToolError};
use base64::{Engine as _, engine::general_purpose::STANDARD as BASE64};
use serde::de::{self, Deserializer};
use serde::{Deserialize, Serialize};
use tai_keystore::ServiceCredential;
use tracing::{debug, error};

fn from_base64<'de, D: Deserializer<'de>>(d: D) -> Result<Vec<u8>, D::Error> {
    let s = String::deserialize(d)?;
    BASE64.decode(&s).map_err(de::Error::custom)
}

// ── Args structs ────────────────────────────────────────────────────────

#[derive(Debug, Deserialize)]
pub struct DbSetArgs {
    key: String,
    #[serde(rename = "value_b64", deserialize_with = "from_base64")]
    value: Vec<u8>,
}

#[derive(Debug, Deserialize)]
pub struct DbGetArgs {
    key: String,
}

#[derive(Debug, Deserialize)]
pub struct DbDeleteArgs {
    key: String,
}

#[derive(Debug, Deserialize)]
pub struct DbDeleteRangeArgs {
    start: String,
    end: Option<String>,
}

#[derive(Debug, Deserialize)]
pub struct DbGetRangeArgs {
    start: String,
    end: Option<String>,
}

#[derive(Debug, Deserialize)]
pub struct DbListArgs {
    start: Option<String>,
    end: Option<String>,
}

#[derive(Debug, Deserialize)]
pub struct DbCountArgs {
    prefix: Option<String>,
}

#[derive(Debug, Serialize)]
pub struct DbGetRangeEntry {
    key: String,
    value_b64: String,
}

// ── db_set ──────────────────────────────────────────────────────────────

pub(crate) struct DbSet;

impl Tool for DbSet {
    type Args = DbSetArgs;
    type Return = String;

    fn name(&self) -> &'static str {
        "db_set"
    }

    fn group(&self) -> &'static str {
        "db"
    }

    fn description(&self) -> &'static str {
        "Insert or overwrite a key-value pair in the session's database. Value must be base64-encoded."
    }

    fn schema(&self) -> serde_json::Value {
        serde_json::json!({
            "type": "object",
            "properties": {
                "key": {
                    "type": "string",
                    "description": "The key to set"
                },
                "value_b64": {
                    "type": "string",
                    "description": "The value, base64-encoded"
                }
            },
            "required": ["key", "value_b64"]
        })
    }

    fn execute(
        &self,
        args: Self::Args,
        _x_credentials: Option<&ServiceCredential>,
        _cwd: Option<&std::path::Path>,
        ctx: Option<&ToolContext>,
    ) -> Result<Self::Return, ToolError> {
        let ctx = ctx.ok_or_else(|| ToolError::Other("no session context".into()))?;
        let value_len = args.value.len();
        db::kv_set(ctx.db.as_ref(), ctx.session_id, &args.key, &args.value).map_err(|e| {
            error!(session = ctx.session_id, key = &args.key, error = %e, "db_set failed");
            ToolError::Other(format!("db_set failed: {e}"))
        })?;
        debug!(
            session = ctx.session_id,
            key = &args.key,
            value_len,
            "db_set ok"
        );
        Ok("ok".to_string())
    }
}

// ── db_get ──────────────────────────────────────────────────────────────

pub(crate) struct DbGet;

impl Tool for DbGet {
    type Args = DbGetArgs;
    type Return = Option<Vec<u8>>;

    fn name(&self) -> &'static str {
        "db_get"
    }

    fn group(&self) -> &'static str {
        "db"
    }

    fn description(&self) -> &'static str {
        "Retrieve a value by key from the session's database."
    }

    fn schema(&self) -> serde_json::Value {
        serde_json::json!({
            "type": "object",
            "properties": {
                "key": {
                    "type": "string",
                    "description": "The key to retrieve"
                }
            },
            "required": ["key"]
        })
    }

    fn execute(
        &self,
        args: Self::Args,
        _x_credentials: Option<&ServiceCredential>,
        _cwd: Option<&std::path::Path>,
        ctx: Option<&ToolContext>,
    ) -> Result<Self::Return, ToolError> {
        let ctx = ctx.ok_or_else(|| ToolError::Other("no session context".into()))?;
        match db::kv_get(ctx.db.as_ref(), ctx.session_id, &args.key) {
            Ok(value) => {
                debug!(
                    session = ctx.session_id,
                    key = &args.key,
                    value_len = value.as_ref().map_or(0, Vec::len),
                    "db_get ok"
                );
                Ok(value)
            }
            Err(e) => {
                error!(session = ctx.session_id, key = &args.key, error = %e, "db_get failed");
                Err(ToolError::Other(format!("db_get failed: {e}")))
            }
        }
    }
}

// ── db_delete ───────────────────────────────────────────────────────────

pub(crate) struct DbDelete;

impl Tool for DbDelete {
    type Args = DbDeleteArgs;
    type Return = String;

    fn name(&self) -> &'static str {
        "db_delete"
    }

    fn group(&self) -> &'static str {
        "db"
    }

    fn description(&self) -> &'static str {
        "Remove a single key from the session's database. Returns 'deleted' or 'not found'."
    }

    fn schema(&self) -> serde_json::Value {
        serde_json::json!({
            "type": "object",
            "properties": {
                "key": {
                    "type": "string",
                    "description": "The key to delete"
                }
            },
            "required": ["key"]
        })
    }

    fn execute(
        &self,
        args: Self::Args,
        _x_credentials: Option<&ServiceCredential>,
        _cwd: Option<&std::path::Path>,
        ctx: Option<&ToolContext>,
    ) -> Result<Self::Return, ToolError> {
        let ctx = ctx.ok_or_else(|| ToolError::Other("no session context".into()))?;
        match db::kv_delete(ctx.db.as_ref(), ctx.session_id, &args.key) {
            Ok(true) => {
                debug!(session = ctx.session_id, key = &args.key, "db_delete ok");
                Ok("deleted".to_string())
            }
            Ok(false) => {
                debug!(
                    session = ctx.session_id,
                    key = &args.key,
                    "db_delete not found"
                );
                Ok("not found".to_string())
            }
            Err(e) => {
                error!(session = ctx.session_id, key = &args.key, error = %e, "db_delete failed");
                Err(ToolError::Other(format!("db_delete failed: {e}")))
            }
        }
    }
}

// ── db_delete_range ─────────────────────────────────────────────────────

pub(crate) struct DbDeleteRange;

impl Tool for DbDeleteRange {
    type Args = DbDeleteRangeArgs;
    type Return = String;

    fn name(&self) -> &'static str {
        "db_delete_range"
    }

    fn group(&self) -> &'static str {
        "db"
    }

    fn description(&self) -> &'static str {
        "Delete all keys in the range [start, end). If end is omitted, deletes from start to the end of the session's keys."
    }

    fn schema(&self) -> serde_json::Value {
        serde_json::json!({
            "type": "object",
            "properties": {
                "start": {
                    "type": "string",
                    "description": "Start of the key range (inclusive)"
                },
                "end": {
                    "type": "string",
                    "description": "End of the key range (exclusive). If omitted, deletes from start to end of session's keys."
                }
            },
            "required": ["start"]
        })
    }

    fn execute(
        &self,
        args: Self::Args,
        _x_credentials: Option<&ServiceCredential>,
        _cwd: Option<&std::path::Path>,
        ctx: Option<&ToolContext>,
    ) -> Result<Self::Return, ToolError> {
        let ctx = ctx.ok_or_else(|| ToolError::Other("no session context".into()))?;
        match db::kv_delete_range(
            ctx.db.as_ref(),
            ctx.session_id,
            &args.start,
            args.end.as_deref(),
        ) {
            Ok(count) => {
                debug!(
                    session = ctx.session_id,
                    start = &args.start,
                    end = args.end.as_deref(),
                    count,
                    "db_delete_range ok"
                );
                Ok(format!("deleted {count} keys"))
            }
            Err(e) => {
                error!(
                    session = ctx.session_id,
                    start = &args.start,
                    end = args.end.as_deref().unwrap_or("(end of session)"),
                    error = %e,
                    "db_delete_range failed"
                );
                Err(ToolError::Other(format!("db_delete_range failed: {e}")))
            }
        }
    }
}

// ── db_get_range ────────────────────────────────────────────────────────

pub(crate) struct DbGetRange;

impl Tool for DbGetRange {
    type Args = DbGetRangeArgs;
    type Return = Vec<DbGetRangeEntry>;

    fn name(&self) -> &'static str {
        "db_get_range"
    }

    fn group(&self) -> &'static str {
        "db"
    }

    fn description(&self) -> &'static str {
        "Retrieve all key-value pairs in the key range [start, end). Returns a JSON array of {key, value_b64} objects."
    }

    fn schema(&self) -> serde_json::Value {
        serde_json::json!({
            "type": "object",
            "properties": {
                "start": {
                    "type": "string",
                    "description": "Start of the key range (inclusive)"
                },
                "end": {
                    "type": "string",
                    "description": "End of the key range (exclusive). If omitted, retrieves from start to end of session's keys."
                }
            },
            "required": ["start"]
        })
    }

    fn execute(
        &self,
        args: Self::Args,
        _x_credentials: Option<&ServiceCredential>,
        _cwd: Option<&std::path::Path>,
        ctx: Option<&ToolContext>,
    ) -> Result<Self::Return, ToolError> {
        let ctx = ctx.ok_or_else(|| ToolError::Other("no session context".into()))?;
        match db::kv_get_range(
            ctx.db.as_ref(),
            ctx.session_id,
            &args.start,
            args.end.as_deref(),
        ) {
            Ok(entries) => {
                let count = entries.len();
                let result: Vec<DbGetRangeEntry> = entries
                    .into_iter()
                    .map(|(key, value)| DbGetRangeEntry {
                        key,
                        value_b64: BASE64.encode(&value),
                    })
                    .collect();
                debug!(
                    session = ctx.session_id,
                    start = &args.start,
                    end = args.end.as_deref(),
                    count,
                    "db_get_range ok"
                );
                Ok(result)
            }
            Err(e) => {
                error!(
                    session = ctx.session_id,
                    start = &args.start,
                    end = args.end.as_deref().unwrap_or("(end of session)"),
                    error = %e,
                    "db_get_range failed"
                );
                Err(ToolError::Other(format!("db_get_range failed: {e}")))
            }
        }
    }
}

// ── db_list ─────────────────────────────────────────────────────────────

pub(crate) struct DbList;

impl Tool for DbList {
    type Args = DbListArgs;
    type Return = Vec<String>;

    fn name(&self) -> &'static str {
        "db_list"
    }

    fn group(&self) -> &'static str {
        "db"
    }

    fn description(&self) -> &'static str {
        "List key names in the key range [start, end). Both start and end are optional. Returns a JSON array of key strings."
    }

    fn schema(&self) -> serde_json::Value {
        serde_json::json!({
            "type": "object",
            "properties": {
                "start": {
                    "type": "string",
                    "description": "Start of the key range (inclusive). If omitted, starts from the beginning."
                },
                "end": {
                    "type": "string",
                    "description": "End of the key range (exclusive). If omitted, goes to the end."
                }
            }
        })
    }

    fn execute(
        &self,
        args: Self::Args,
        _x_credentials: Option<&ServiceCredential>,
        _cwd: Option<&std::path::Path>,
        ctx: Option<&ToolContext>,
    ) -> Result<Self::Return, ToolError> {
        let ctx = ctx.ok_or_else(|| ToolError::Other("no session context".into()))?;
        match db::kv_list(
            ctx.db.as_ref(),
            ctx.session_id,
            args.start.as_deref(),
            args.end.as_deref(),
        ) {
            Ok(keys) => {
                let count = keys.len();
                debug!(
                    session = ctx.session_id,
                    start = args.start.as_deref(),
                    end = args.end.as_deref(),
                    count,
                    "db_list ok"
                );
                Ok(keys)
            }
            Err(e) => {
                error!(
                    session = ctx.session_id,
                    start = args.start.as_deref(),
                    end = args.end.as_deref(),
                    error = %e,
                    "db_list failed"
                );
                Err(ToolError::Other(format!("db_list failed: {e}")))
            }
        }
    }
}

// ── db_count ────────────────────────────────────────────────────────────

pub(crate) struct DbCount;

impl Tool for DbCount {
    type Args = DbCountArgs;
    type Return = u64;

    fn name(&self) -> &'static str {
        "db_count"
    }

    fn group(&self) -> &'static str {
        "db"
    }

    fn description(&self) -> &'static str {
        "Count keys in the session's database, optionally filtered by prefix."
    }

    fn schema(&self) -> serde_json::Value {
        serde_json::json!({
            "type": "object",
            "properties": {
                "prefix": {
                    "type": "string",
                    "description": "Optional prefix to filter keys by. If omitted, counts all keys."
                }
            }
        })
    }

    fn execute(
        &self,
        args: Self::Args,
        _x_credentials: Option<&ServiceCredential>,
        _cwd: Option<&std::path::Path>,
        ctx: Option<&ToolContext>,
    ) -> Result<Self::Return, ToolError> {
        let ctx = ctx.ok_or_else(|| ToolError::Other("no session context".into()))?;
        match db::kv_count(ctx.db.as_ref(), ctx.session_id, args.prefix.as_deref()) {
            Ok(count) => {
                debug!(
                    session = ctx.session_id,
                    prefix = args.prefix.as_deref(),
                    count,
                    "db_count ok"
                );
                Ok(count)
            }
            Err(e) => {
                error!(
                    session = ctx.session_id,
                    prefix = args.prefix.as_deref(),
                    error = %e,
                    "db_count failed"
                );
                Err(ToolError::Other(format!("db_count failed: {e}")))
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::tools::context::ToolContext;
    use std::sync::Arc;

    fn test_context() -> (tempfile::TempDir, ToolContext) {
        let dir = tempfile::tempdir().unwrap();
        let db = Arc::new(redb::Database::create(dir.path().join("test.redb")).unwrap());
        let _ = db::kv_set(&db, 0, "__init__", b"").unwrap();
        let _ = db::kv_delete(&db, 0, "__init__").unwrap();
        let (daemon_tx, _daemon_rx) = std::sync::mpsc::channel();
        let ctx = ToolContext::new(42, db, daemon_tx);
        (dir, ctx)
    }

    #[test]
    fn db_set_and_get_round_trip() {
        let (_dir, ctx) = test_context();
        let result = DbSet.execute(
            DbSetArgs {
                key: "greeting".into(),
                value: b"hello".to_vec(),
            },
            None,
            None,
            Some(&ctx),
        );
        assert_eq!(result.unwrap(), "ok");

        let result = DbGet.execute(
            DbGetArgs {
                key: "greeting".into(),
            },
            None,
            None,
            Some(&ctx),
        );
        assert_eq!(result.unwrap(), Some(b"hello".to_vec()));
    }

    #[test]
    fn db_get_not_found() {
        let (_dir, ctx) = test_context();
        let result = DbGet.execute(
            DbGetArgs {
                key: "nonexistent".into(),
            },
            None,
            None,
            Some(&ctx),
        );
        assert_eq!(result.unwrap(), None);
    }

    #[test]
    fn db_delete_existing() {
        let (_dir, ctx) = test_context();
        DbSet
            .execute(
                DbSetArgs {
                    key: "x".into(),
                    value: b"x".to_vec(),
                },
                None,
                None,
                Some(&ctx),
            )
            .unwrap();
        let result = DbDelete.execute(DbDeleteArgs { key: "x".into() }, None, None, Some(&ctx));
        assert_eq!(result.unwrap(), "deleted");
    }

    #[test]
    fn db_delete_not_found() {
        let (_dir, ctx) = test_context();
        let result = DbDelete.execute(
            DbDeleteArgs {
                key: "nonexistent".into(),
            },
            None,
            None,
            Some(&ctx),
        );
        assert_eq!(result.unwrap(), "not found");
    }

    #[test]
    fn db_count_no_prefix() {
        let (_dir, ctx) = test_context();
        DbSet
            .execute(
                DbSetArgs {
                    key: "a".into(),
                    value: b"a".to_vec(),
                },
                None,
                None,
                Some(&ctx),
            )
            .unwrap();
        DbSet
            .execute(
                DbSetArgs {
                    key: "b".into(),
                    value: b"b".to_vec(),
                },
                None,
                None,
                Some(&ctx),
            )
            .unwrap();
        let result = DbCount.execute(DbCountArgs { prefix: None }, None, None, Some(&ctx));
        assert_eq!(result.unwrap(), 2);
    }

    #[test]
    fn db_count_with_prefix() {
        let (_dir, ctx) = test_context();
        DbSet
            .execute(
                DbSetArgs {
                    key: "aa".into(),
                    value: b"aa".to_vec(),
                },
                None,
                None,
                Some(&ctx),
            )
            .unwrap();
        DbSet
            .execute(
                DbSetArgs {
                    key: "ab".into(),
                    value: b"ab".to_vec(),
                },
                None,
                None,
                Some(&ctx),
            )
            .unwrap();
        DbSet
            .execute(
                DbSetArgs {
                    key: "ba".into(),
                    value: b"ba".to_vec(),
                },
                None,
                None,
                Some(&ctx),
            )
            .unwrap();
        let result = DbCount.execute(
            DbCountArgs {
                prefix: Some("a".into()),
            },
            None,
            None,
            Some(&ctx),
        );
        assert_eq!(result.unwrap(), 2);
    }

    #[test]
    fn db_list_range() {
        let (_dir, ctx) = test_context();
        DbSet
            .execute(
                DbSetArgs {
                    key: "apple".into(),
                    value: b"a".to_vec(),
                },
                None,
                None,
                Some(&ctx),
            )
            .unwrap();
        DbSet
            .execute(
                DbSetArgs {
                    key: "banana".into(),
                    value: b"b".to_vec(),
                },
                None,
                None,
                Some(&ctx),
            )
            .unwrap();
        DbSet
            .execute(
                DbSetArgs {
                    key: "cherry".into(),
                    value: b"c".to_vec(),
                },
                None,
                None,
                Some(&ctx),
            )
            .unwrap();
        let result = DbList.execute(
            DbListArgs {
                start: Some("banana".into()),
                end: Some("cherry".into()),
            },
            None,
            None,
            Some(&ctx),
        );
        assert_eq!(result.unwrap(), vec!["banana".to_string()]);
    }

    #[test]
    fn db_get_range_with_values() {
        let (_dir, ctx) = test_context();
        DbSet
            .execute(
                DbSetArgs {
                    key: "x".into(),
                    value: b"xxx".to_vec(),
                },
                None,
                None,
                Some(&ctx),
            )
            .unwrap();
        let result = DbGetRange.execute(
            DbGetRangeArgs {
                start: "x".into(),
                end: Some("y".into()),
            },
            None,
            None,
            Some(&ctx),
        );
        let entries = result.unwrap();
        assert_eq!(entries.len(), 1);
        assert_eq!(entries[0].key, "x");
        assert_eq!(entries[0].value_b64, "eHh4");
    }

    #[test]
    fn db_delete_range() {
        let (_dir, ctx) = test_context();
        DbSet
            .execute(
                DbSetArgs {
                    key: "a".into(),
                    value: b"a".to_vec(),
                },
                None,
                None,
                Some(&ctx),
            )
            .unwrap();
        DbSet
            .execute(
                DbSetArgs {
                    key: "b".into(),
                    value: b"b".to_vec(),
                },
                None,
                None,
                Some(&ctx),
            )
            .unwrap();
        DbSet
            .execute(
                DbSetArgs {
                    key: "c".into(),
                    value: b"c".to_vec(),
                },
                None,
                None,
                Some(&ctx),
            )
            .unwrap();
        let result = DbDeleteRange.execute(
            DbDeleteRangeArgs {
                start: "b".into(),
                end: None,
            },
            None,
            None,
            Some(&ctx),
        );
        assert_eq!(result.unwrap(), "deleted 2 keys");
    }

    #[test]
    fn db_no_context_returns_error() {
        let result = DbSet.execute(
            DbSetArgs {
                key: "x".into(),
                value: b"x".to_vec(),
            },
            None,
            None,
            None,
        );
        assert!(result.is_err());
        assert_eq!(result.unwrap_err().to_string(), "no session context");
    }
}
