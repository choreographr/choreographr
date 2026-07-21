use crate::db;
use crate::tools::Tool;
use crate::tools::context::ToolContext;
use base64::{Engine as _, engine::general_purpose::STANDARD as BASE64};
use schemars::JsonSchema;
use serde::{Deserialize, Serialize};
use tai_keystore::ServiceCredential;
use tracing::{debug, error};

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

// ── Args structs ────────────────────────────────────────────────────────

#[derive(Debug, Deserialize, JsonSchema)]
pub struct DbSetArgs {
    /// The key to set
    key: String,
    /// The value to store
    value: DbValue,
}

#[derive(Debug, Deserialize, JsonSchema)]
pub struct DbGetArgs {
    /// The key to retrieve
    key: String,
}

#[derive(Debug, Deserialize, JsonSchema)]
pub struct DbDeleteArgs {
    /// The key to delete
    key: String,
}

#[derive(Debug, Deserialize, JsonSchema)]
pub struct DbDeleteRangeArgs {
    /// Start of the key range (inclusive)
    start: String,
    /// End of the key range (exclusive). If omitted, deletes from start to end of session's keys.
    end: Option<String>,
}

#[derive(Debug, Deserialize, JsonSchema)]
pub struct DbGetRangeArgs {
    /// Start of the key range (inclusive)
    start: String,
    /// End of the key range (exclusive). If omitted, retrieves from start to end of session's keys.
    end: Option<String>,
}

#[derive(Debug, Deserialize, JsonSchema)]
pub struct DbListArgs {
    /// Start of the key range (inclusive). If omitted, starts from the beginning.
    start: Option<String>,
    /// End of the key range (exclusive). If omitted, goes to the end.
    end: Option<String>,
}

#[derive(Debug, Deserialize, JsonSchema)]
pub struct DbCountArgs {
    /// Optional prefix to filter keys by. If omitted, counts all keys.
    prefix: Option<String>,
}

#[derive(Debug, Serialize, JsonSchema)]
pub struct DbGetRangeEntry {
    /// The key
    key: String,
    /// The value, base64-encoded
    value_b64: String,
}

// ── db_set ──────────────────────────────────────────────────────────────

pub(crate) struct DbSet;

impl Tool for DbSet {
    type Args = DbSetArgs;
    type Return = String;
    type Error = DbError;

    fn name(&self) -> &'static str {
        "db_set"
    }

    fn group(&self) -> &'static str {
        "db"
    }

    fn description(&self) -> &'static str {
        "Insert or overwrite a key-value pair in the session's database."
    }

    fn describe_invocation(&self, args: &Self::Args) -> String {
        format!(
            "Setting database key `{}` to value ({} bytes).",
            args.key,
            args.value.0.len()
        )
    }

    fn return_string(ret: &Self::Return) -> String {
        ret.clone()
    }

    fn execute(
        &self,
        args: Self::Args,
        _x_credentials: Option<&ServiceCredential>,
        _working_dir: Option<&std::path::Path>,
        ctx: Option<&ToolContext>,
    ) -> Result<Self::Return, Self::Error> {
        let ctx = ctx.ok_or(DbError::NoSessionContext)?;
        let value_len = args.value.0.len();
        db::kv_set(ctx.db.as_ref(), ctx.session_id, &args.key, &args.value.0).map_err(|e| {
            error!(session = ctx.session_id, key = &args.key, error = %e, "db_set failed");
            DbError::Storage(format!("db_set failed: {e}"))
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
    type Return = Option<String>;
    type Error = DbError;

    fn name(&self) -> &'static str {
        "db_get"
    }

    fn group(&self) -> &'static str {
        "db"
    }

    fn description(&self) -> &'static str {
        "Retrieve a value by key from the session's database."
    }

    fn describe_invocation(&self, args: &Self::Args) -> String {
        format!("Getting database key `{}`.", args.key)
    }

    fn return_string(ret: &Self::Return) -> String {
        ret.clone().unwrap_or_default()
    }

    fn output_schema(&self) -> Option<serde_json::Value> {
        // Returns None so the field is omitted from the wire format.
        // Output is a lossy UTF-8 conversion of arbitrary binary data;
        // the nullable-string schema would mislead callers into thinking
        // the value is always valid UTF-8.
        None
    }

    fn execute(
        &self,
        args: Self::Args,
        _x_credentials: Option<&ServiceCredential>,
        _working_dir: Option<&std::path::Path>,
        ctx: Option<&ToolContext>,
    ) -> Result<Self::Return, Self::Error> {
        let ctx = ctx.ok_or(DbError::NoSessionContext)?;
        match db::kv_get(ctx.db.as_ref(), ctx.session_id, &args.key) {
            Ok(value) => {
                let value_len = value.as_ref().map_or(0, Vec::len);
                let value_str = value.map(|v| String::from_utf8_lossy(&v).into_owned());
                debug!(
                    session = ctx.session_id,
                    key = &args.key,
                    value_len,
                    "db_get ok"
                );
                Ok(value_str)
            }
            Err(e) => {
                error!(session = ctx.session_id, key = &args.key, error = %e, "db_get failed");
                Err(DbError::Storage(format!("db_get failed: {e}")))
            }
        }
    }
}

// ── db_delete ───────────────────────────────────────────────────────────

pub(crate) struct DbDelete;

impl Tool for DbDelete {
    type Args = DbDeleteArgs;
    type Return = String;
    type Error = DbError;

    fn name(&self) -> &'static str {
        "db_delete"
    }

    fn group(&self) -> &'static str {
        "db"
    }

    fn description(&self) -> &'static str {
        "Remove a single key from the session's database. Returns 'deleted' or 'not found'."
    }

    fn describe_invocation(&self, args: &Self::Args) -> String {
        format!("Deleting database key `{}`.", args.key)
    }

    fn return_string(ret: &Self::Return) -> String {
        ret.clone()
    }

    fn execute(
        &self,
        args: Self::Args,
        _x_credentials: Option<&ServiceCredential>,
        _working_dir: Option<&std::path::Path>,
        ctx: Option<&ToolContext>,
    ) -> Result<Self::Return, Self::Error> {
        let ctx = ctx.ok_or(DbError::NoSessionContext)?;
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
                Err(DbError::Storage(format!("db_delete failed: {e}")))
            }
        }
    }
}

// ── db_delete_range ─────────────────────────────────────────────────────

pub(crate) struct DbDeleteRange;

impl Tool for DbDeleteRange {
    type Args = DbDeleteRangeArgs;
    type Return = String;
    type Error = DbError;

    fn name(&self) -> &'static str {
        "db_delete_range"
    }

    fn group(&self) -> &'static str {
        "db"
    }

    fn description(&self) -> &'static str {
        "Delete all keys in the range [start, end). If end is omitted, deletes from start to the end of the session's keys."
    }

    fn describe_invocation(&self, args: &Self::Args) -> String {
        format!(
            "Deleting database keys from `{}` to {}.",
            args.start,
            args.end.as_deref().unwrap_or("the end")
        )
    }

    fn return_string(ret: &Self::Return) -> String {
        ret.clone()
    }

    fn execute(
        &self,
        args: Self::Args,
        _x_credentials: Option<&ServiceCredential>,
        _working_dir: Option<&std::path::Path>,
        ctx: Option<&ToolContext>,
    ) -> Result<Self::Return, Self::Error> {
        let ctx = ctx.ok_or(DbError::NoSessionContext)?;
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
                Err(DbError::Storage(format!("db_delete_range failed: {e}")))
            }
        }
    }
}

// ── db_get_range ────────────────────────────────────────────────────────

pub(crate) struct DbGetRange;

impl Tool for DbGetRange {
    type Args = DbGetRangeArgs;
    type Return = Vec<DbGetRangeEntry>;
    type Error = DbError;

    fn name(&self) -> &'static str {
        "db_get_range"
    }

    fn group(&self) -> &'static str {
        "db"
    }

    fn description(&self) -> &'static str {
        "Retrieve all key-value pairs in the key range [start, end). Returns a JSON array of {key, value_b64} objects."
    }

    fn describe_invocation(&self, args: &Self::Args) -> String {
        format!(
            "Getting database keys from `{}` to {}.",
            args.start,
            args.end.as_deref().unwrap_or("the end")
        )
    }

    fn return_string(ret: &Self::Return) -> String {
        ret.iter()
            .map(|e| format!("{}: {}", e.key, e.value_b64))
            .collect::<Vec<_>>()
            .join("\n")
    }

    fn execute(
        &self,
        args: Self::Args,
        _x_credentials: Option<&ServiceCredential>,
        _working_dir: Option<&std::path::Path>,
        ctx: Option<&ToolContext>,
    ) -> Result<Self::Return, Self::Error> {
        let ctx = ctx.ok_or(DbError::NoSessionContext)?;
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
                Err(DbError::Storage(format!("db_get_range failed: {e}")))
            }
        }
    }
}

// ── db_list ─────────────────────────────────────────────────────────────

pub(crate) struct DbList;

impl Tool for DbList {
    type Args = DbListArgs;
    type Return = Vec<String>;
    type Error = DbError;

    fn name(&self) -> &'static str {
        "db_list"
    }

    fn group(&self) -> &'static str {
        "db"
    }

    fn description(&self) -> &'static str {
        "List key names in the key range [start, end). Both start and end are optional. Returns a JSON array of key strings."
    }

    fn describe_invocation(&self, args: &Self::Args) -> String {
        match (&args.start, &args.end) {
            (Some(s), Some(e)) => format!("Listing database keys from `{}` to `{}`.", s, e),
            (Some(s), None) => format!("Listing database keys starting from `{}`.", s),
            (None, Some(e)) => format!("Listing database keys up to `{}`.", e),
            (None, None) => "Listing all database keys.".to_string(),
        }
    }

    fn return_string(ret: &Self::Return) -> String {
        ret.join("\n")
    }

    fn execute(
        &self,
        args: Self::Args,
        _x_credentials: Option<&ServiceCredential>,
        _working_dir: Option<&std::path::Path>,
        ctx: Option<&ToolContext>,
    ) -> Result<Self::Return, Self::Error> {
        let ctx = ctx.ok_or(DbError::NoSessionContext)?;
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
                Err(DbError::Storage(format!("db_list failed: {e}")))
            }
        }
    }
}

// ── db_count ────────────────────────────────────────────────────────────

pub(crate) struct DbCount;

impl Tool for DbCount {
    type Args = DbCountArgs;
    type Return = u64;
    type Error = DbError;

    fn name(&self) -> &'static str {
        "db_count"
    }

    fn group(&self) -> &'static str {
        "db"
    }

    fn description(&self) -> &'static str {
        "Count keys in the session's database, optionally filtered by prefix."
    }

    fn describe_invocation(&self, args: &Self::Args) -> String {
        match &args.prefix {
            Some(p) => format!("Counting database entries with prefix `{}`.", p),
            None => "Counting all database entries.".to_string(),
        }
    }

    fn return_string(ret: &Self::Return) -> String {
        ret.to_string()
    }

    fn execute(
        &self,
        args: Self::Args,
        _x_credentials: Option<&ServiceCredential>,
        _working_dir: Option<&std::path::Path>,
        ctx: Option<&ToolContext>,
    ) -> Result<Self::Return, Self::Error> {
        let ctx = ctx.ok_or(DbError::NoSessionContext)?;
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
                Err(DbError::Storage(format!("db_count failed: {e}")))
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
                value: DbValue(b"hello".to_vec()),
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
        assert_eq!(result.unwrap(), Some("hello".into()));
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
                    value: DbValue(b"x".to_vec()),
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
                    value: DbValue(b"a".to_vec()),
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
                    value: DbValue(b"b".to_vec()),
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
                    value: DbValue(b"aa".to_vec()),
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
                    value: DbValue(b"ab".to_vec()),
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
                    value: DbValue(b"ba".to_vec()),
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
                    value: DbValue(b"a".to_vec()),
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
                    value: DbValue(b"b".to_vec()),
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
                    value: DbValue(b"c".to_vec()),
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
                    value: DbValue(b"xxx".to_vec()),
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
                    value: DbValue(b"a".to_vec()),
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
                    value: DbValue(b"b".to_vec()),
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
                    value: DbValue(b"c".to_vec()),
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
                value: DbValue(b"x".to_vec()),
            },
            None,
            None,
            None,
        );
        assert!(result.is_err());
        assert_eq!(result.unwrap_err().to_string(), "no session context");
    }

    // ── output_schema tests ──────────────────────────────────────────

    #[test]
    fn db_get_output_schema_is_none() {
        assert!(
            DbGet.output_schema().is_none(),
            "DbGet returns raw bytes converted lossily to string; no structured JSON schema"
        );
    }

    #[test]
    fn db_get_range_output_schema_is_array_of_objects() {
        let schema = DbGetRange.output_schema().expect("schema");
        assert_eq!(schema["type"], "array");
        assert_eq!(schema["items"]["type"], "object");
        assert!(
            schema["items"]["required"]
                .as_array()
                .unwrap()
                .contains(&serde_json::Value::String("key".into()))
        );
        assert!(
            schema["items"]["required"]
                .as_array()
                .unwrap()
                .contains(&serde_json::Value::String("value_b64".into()))
        );
    }

    #[test]
    fn db_list_output_schema_is_array_of_strings() {
        let schema = DbList.output_schema().expect("schema");
        assert_eq!(schema["type"], "array");
        assert_eq!(schema["items"]["type"], "string");
    }

    #[test]
    fn db_count_output_schema_is_integer() {
        let schema = DbCount.output_schema().expect("schema");
        assert_eq!(schema["type"], "integer");
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

    #[test]
    fn describe_db_set_invocation() {
        let tool = DbSet;
        let args = DbSetArgs {
            key: "my_key".into(),
            value: DbValue(b"hello".to_vec()),
        };
        let desc = tool.describe_invocation(&args);
        assert_eq!(desc, "Setting database key `my_key` to value (5 bytes).");
    }

    #[test]
    fn describe_db_get_invocation() {
        let tool = DbGet;
        let args = DbGetArgs {
            key: "my_key".into(),
        };
        let desc = tool.describe_invocation(&args);
        assert_eq!(desc, "Getting database key `my_key`.");
    }

    #[test]
    fn describe_db_delete_range_invocation() {
        let tool = DbDeleteRange;
        let args = DbDeleteRangeArgs {
            start: "a".into(),
            end: Some("z".into()),
        };
        let desc = tool.describe_invocation(&args);
        assert_eq!(desc, "Deleting database keys from `a` to z.");
    }

    #[test]
    fn describe_db_list_invocation_start_only() {
        let tool = DbList;
        let args = DbListArgs {
            start: Some("b".into()),
            end: None,
        };
        let desc = tool.describe_invocation(&args);
        assert_eq!(desc, "Listing database keys starting from `b`.");
    }

    #[test]
    fn describe_db_count_invocation_with_prefix() {
        let tool = DbCount;
        let args = DbCountArgs {
            prefix: Some("test".into()),
        };
        let desc = tool.describe_invocation(&args);
        assert_eq!(desc, "Counting database entries with prefix `test`.");
    }

    #[test]
    fn describe_db_count_invocation_without_prefix() {
        let tool = DbCount;
        let args = DbCountArgs { prefix: None };
        let desc = tool.describe_invocation(&args);
        assert_eq!(desc, "Counting all database entries.");
    }
}
