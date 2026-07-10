use crate::db;
use crate::tools::context::ToolContext;
use crate::tools::{Tool, ToolExecutionOutput, tool_err, tool_ok};
use base64::{Engine as _, engine::general_purpose::STANDARD as BASE64};
use std::path::Path;
use tai_keystore::ServiceCredential;
use tracing::{debug, error};

// ── db_set ─────────────────────────────────────────────────────────────────────

pub(crate) struct DbSet;

impl Tool for DbSet {
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
        arguments_json: &str,
        x_credentials: Option<&ServiceCredential>,
        cwd: Option<&Path>,
    ) -> ToolExecutionOutput {
        self.execute_with_context(arguments_json, x_credentials, cwd, None)
    }

    fn execute_with_context(
        &self,
        arguments_json: &str,
        _x_credentials: Option<&ServiceCredential>,
        _cwd: Option<&Path>,
        ctx: Option<&ToolContext>,
    ) -> ToolExecutionOutput {
        let ctx = match ctx {
            Some(c) => c,
            None => {
                return ToolExecutionOutput {
                    result: tool_err("no session context"),
                    image: None,
                };
            }
        };
        let args: serde_json::Value = match serde_json::from_str(arguments_json) {
            Ok(v) => v,
            Err(e) => {
                return ToolExecutionOutput {
                    result: tool_err(format!("invalid arguments: {e}")),
                    image: None,
                };
            }
        };
        let key = match args["key"].as_str() {
            Some(k) => k,
            None => {
                return ToolExecutionOutput {
                    result: tool_err("missing required argument: key"),
                    image: None,
                };
            }
        };
        let value_b64 = match args["value_b64"].as_str() {
            Some(v) => v,
            None => {
                return ToolExecutionOutput {
                    result: tool_err("missing required argument: value_b64"),
                    image: None,
                };
            }
        };
        let value = match BASE64.decode(value_b64) {
            Ok(v) => v,
            Err(e) => {
                return ToolExecutionOutput {
                    result: tool_err(format!("base64 decode error: {e}")),
                    image: None,
                };
            }
        };
        let value_len = value.len();
        match db::kv_set(ctx.db.as_ref(), ctx.session_id, key, &value) {
            Ok(()) => {
                debug!(session = ctx.session_id, key, value_len, "db_set ok");
                ToolExecutionOutput {
                    result: tool_ok("ok".to_string()),
                    image: None,
                }
            }
            Err(e) => {
                error!(session = ctx.session_id, key, error = %e, "db_set failed");
                ToolExecutionOutput {
                    result: tool_err(format!("db_set failed: {e}")),
                    image: None,
                }
            }
        }
    }
}

// ── db_get ─────────────────────────────────────────────────────────────────────

pub(crate) struct DbGet;

impl Tool for DbGet {
    fn name(&self) -> &'static str {
        "db_get"
    }

    fn group(&self) -> &'static str {
        "db"
    }

    fn description(&self) -> &'static str {
        "Retrieve a value by key from the session's database. Returns the value base64-encoded."
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
        arguments_json: &str,
        x_credentials: Option<&ServiceCredential>,
        cwd: Option<&Path>,
    ) -> ToolExecutionOutput {
        self.execute_with_context(arguments_json, x_credentials, cwd, None)
    }

    fn execute_with_context(
        &self,
        arguments_json: &str,
        _x_credentials: Option<&ServiceCredential>,
        _cwd: Option<&Path>,
        ctx: Option<&ToolContext>,
    ) -> ToolExecutionOutput {
        let ctx = match ctx {
            Some(c) => c,
            None => {
                return ToolExecutionOutput {
                    result: tool_err("no session context"),
                    image: None,
                };
            }
        };
        let args: serde_json::Value = match serde_json::from_str(arguments_json) {
            Ok(v) => v,
            Err(e) => {
                return ToolExecutionOutput {
                    result: tool_err(format!("invalid arguments: {e}")),
                    image: None,
                };
            }
        };
        let key = match args["key"].as_str() {
            Some(k) => k,
            None => {
                return ToolExecutionOutput {
                    result: tool_err("missing required argument: key"),
                    image: None,
                };
            }
        };
        match db::kv_get(ctx.db.as_ref(), ctx.session_id, key) {
            Ok(Some(value)) => {
                let encoded = BASE64.encode(&value);
                debug!(
                    session = ctx.session_id,
                    key,
                    value_len = value.len(),
                    "db_get ok"
                );
                ToolExecutionOutput {
                    result: tool_ok(encoded),
                    image: None,
                }
            }
            Ok(None) => {
                debug!(session = ctx.session_id, key, "db_get not found");
                ToolExecutionOutput {
                    result: tool_ok("not found".to_string()),
                    image: None,
                }
            }
            Err(e) => {
                error!(session = ctx.session_id, key, error = %e, "db_get failed");
                ToolExecutionOutput {
                    result: tool_err(format!("db_get failed: {e}")),
                    image: None,
                }
            }
        }
    }
}

// ── db_delete ──────────────────────────────────────────────────────────────────

pub(crate) struct DbDelete;

impl Tool for DbDelete {
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
        arguments_json: &str,
        x_credentials: Option<&ServiceCredential>,
        cwd: Option<&Path>,
    ) -> ToolExecutionOutput {
        self.execute_with_context(arguments_json, x_credentials, cwd, None)
    }

    fn execute_with_context(
        &self,
        arguments_json: &str,
        _x_credentials: Option<&ServiceCredential>,
        _cwd: Option<&Path>,
        ctx: Option<&ToolContext>,
    ) -> ToolExecutionOutput {
        let ctx = match ctx {
            Some(c) => c,
            None => {
                return ToolExecutionOutput {
                    result: tool_err("no session context"),
                    image: None,
                };
            }
        };
        let args: serde_json::Value = match serde_json::from_str(arguments_json) {
            Ok(v) => v,
            Err(e) => {
                return ToolExecutionOutput {
                    result: tool_err(format!("invalid arguments: {e}")),
                    image: None,
                };
            }
        };
        let key = match args["key"].as_str() {
            Some(k) => k,
            None => {
                return ToolExecutionOutput {
                    result: tool_err("missing required argument: key"),
                    image: None,
                };
            }
        };
        match db::kv_delete(ctx.db.as_ref(), ctx.session_id, key) {
            Ok(true) => {
                debug!(session = ctx.session_id, key, "db_delete ok");
                ToolExecutionOutput {
                    result: tool_ok("deleted".to_string()),
                    image: None,
                }
            }
            Ok(false) => {
                debug!(session = ctx.session_id, key, "db_delete not found");
                ToolExecutionOutput {
                    result: tool_ok("not found".to_string()),
                    image: None,
                }
            }
            Err(e) => {
                error!(session = ctx.session_id, key, error = %e, "db_delete failed");
                ToolExecutionOutput {
                    result: tool_err(format!("db_delete failed: {e}")),
                    image: None,
                }
            }
        }
    }
}

// ── db_delete_range ────────────────────────────────────────────────────────────

pub(crate) struct DbDeleteRange;

impl Tool for DbDeleteRange {
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
        arguments_json: &str,
        x_credentials: Option<&ServiceCredential>,
        cwd: Option<&Path>,
    ) -> ToolExecutionOutput {
        self.execute_with_context(arguments_json, x_credentials, cwd, None)
    }

    fn execute_with_context(
        &self,
        arguments_json: &str,
        _x_credentials: Option<&ServiceCredential>,
        _cwd: Option<&Path>,
        ctx: Option<&ToolContext>,
    ) -> ToolExecutionOutput {
        let ctx = match ctx {
            Some(c) => c,
            None => {
                return ToolExecutionOutput {
                    result: tool_err("no session context"),
                    image: None,
                };
            }
        };
        let args: serde_json::Value = match serde_json::from_str(arguments_json) {
            Ok(v) => v,
            Err(e) => {
                return ToolExecutionOutput {
                    result: tool_err(format!("invalid arguments: {e}")),
                    image: None,
                };
            }
        };
        let start = match args["start"].as_str() {
            Some(s) => s,
            None => {
                return ToolExecutionOutput {
                    result: tool_err("missing required argument: start"),
                    image: None,
                };
            }
        };
        let end = args["end"].as_str();
        match db::kv_delete_range(ctx.db.as_ref(), ctx.session_id, start, end) {
            Ok(count) => {
                debug!(
                    session = ctx.session_id,
                    start, end, count, "db_delete_range ok"
                );
                ToolExecutionOutput {
                    result: tool_ok(format!("deleted {count} keys")),
                    image: None,
                }
            }
            Err(e) => {
                error!(
                    session = ctx.session_id,
                    start,
                    end = end.unwrap_or("(end of session)"),
                    error = %e,
                    "db_delete_range failed"
                );
                ToolExecutionOutput {
                    result: tool_err(format!("db_delete_range failed: {e}")),
                    image: None,
                }
            }
        }
    }
}

// ── db_get_range ───────────────────────────────────────────────────────────────

pub(crate) struct DbGetRange;

impl Tool for DbGetRange {
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
        arguments_json: &str,
        x_credentials: Option<&ServiceCredential>,
        cwd: Option<&Path>,
    ) -> ToolExecutionOutput {
        self.execute_with_context(arguments_json, x_credentials, cwd, None)
    }

    fn execute_with_context(
        &self,
        arguments_json: &str,
        _x_credentials: Option<&ServiceCredential>,
        _cwd: Option<&Path>,
        ctx: Option<&ToolContext>,
    ) -> ToolExecutionOutput {
        let ctx = match ctx {
            Some(c) => c,
            None => {
                return ToolExecutionOutput {
                    result: tool_err("no session context"),
                    image: None,
                };
            }
        };
        let args: serde_json::Value = match serde_json::from_str(arguments_json) {
            Ok(v) => v,
            Err(e) => {
                return ToolExecutionOutput {
                    result: tool_err(format!("invalid arguments: {e}")),
                    image: None,
                };
            }
        };
        let start = match args["start"].as_str() {
            Some(s) => s,
            None => {
                return ToolExecutionOutput {
                    result: tool_err("missing required argument: start"),
                    image: None,
                };
            }
        };
        let end = args["end"].as_str();
        match db::kv_get_range(ctx.db.as_ref(), ctx.session_id, start, end) {
            Ok(entries) => {
                let count = entries.len();
                let result: Vec<serde_json::Value> = entries
                    .into_iter()
                    .map(|(key, value)| {
                        serde_json::json!({
                            "key": key,
                            "value_b64": BASE64.encode(&value)
                        })
                    })
                    .collect();
                let json = serde_json::to_string(&result).unwrap_or_else(|_| "[]".to_string());
                debug!(
                    session = ctx.session_id,
                    start, end, count, "db_get_range ok"
                );
                ToolExecutionOutput {
                    result: tool_ok(json),
                    image: None,
                }
            }
            Err(e) => {
                error!(
                    session = ctx.session_id,
                    start,
                    end = end.unwrap_or("(end of session)"),
                    error = %e,
                    "db_get_range failed"
                );
                ToolExecutionOutput {
                    result: tool_err(format!("db_get_range failed: {e}")),
                    image: None,
                }
            }
        }
    }
}

// ── db_list ────────────────────────────────────────────────────────────────────

pub(crate) struct DbList;

impl Tool for DbList {
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
        arguments_json: &str,
        x_credentials: Option<&ServiceCredential>,
        cwd: Option<&Path>,
    ) -> ToolExecutionOutput {
        self.execute_with_context(arguments_json, x_credentials, cwd, None)
    }

    fn execute_with_context(
        &self,
        arguments_json: &str,
        _x_credentials: Option<&ServiceCredential>,
        _cwd: Option<&Path>,
        ctx: Option<&ToolContext>,
    ) -> ToolExecutionOutput {
        let ctx = match ctx {
            Some(c) => c,
            None => {
                return ToolExecutionOutput {
                    result: tool_err("no session context"),
                    image: None,
                };
            }
        };
        let args: serde_json::Value = match serde_json::from_str(arguments_json) {
            Ok(v) => v,
            Err(e) => {
                return ToolExecutionOutput {
                    result: tool_err(format!("invalid arguments: {e}")),
                    image: None,
                };
            }
        };
        let start = args["start"].as_str();
        let end = args["end"].as_str();
        match db::kv_list(ctx.db.as_ref(), ctx.session_id, start, end) {
            Ok(keys) => {
                let count = keys.len();
                let json = serde_json::to_string(&keys).unwrap_or_else(|_| "[]".to_string());
                debug!(session = ctx.session_id, start, end, count, "db_list ok");
                ToolExecutionOutput {
                    result: tool_ok(json),
                    image: None,
                }
            }
            Err(e) => {
                error!(
                    session = ctx.session_id,
                    start,
                    end,
                    error = %e,
                    "db_list failed"
                );
                ToolExecutionOutput {
                    result: tool_err(format!("db_list failed: {e}")),
                    image: None,
                }
            }
        }
    }
}

// ── db_count ───────────────────────────────────────────────────────────────────

pub(crate) struct DbCount;

impl Tool for DbCount {
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
        arguments_json: &str,
        x_credentials: Option<&ServiceCredential>,
        cwd: Option<&Path>,
    ) -> ToolExecutionOutput {
        self.execute_with_context(arguments_json, x_credentials, cwd, None)
    }

    fn execute_with_context(
        &self,
        arguments_json: &str,
        _x_credentials: Option<&ServiceCredential>,
        _cwd: Option<&Path>,
        ctx: Option<&ToolContext>,
    ) -> ToolExecutionOutput {
        let ctx = match ctx {
            Some(c) => c,
            None => {
                return ToolExecutionOutput {
                    result: tool_err("no session context"),
                    image: None,
                };
            }
        };
        let args: serde_json::Value = match serde_json::from_str(arguments_json) {
            Ok(v) => v,
            Err(e) => {
                return ToolExecutionOutput {
                    result: tool_err(format!("invalid arguments: {e}")),
                    image: None,
                };
            }
        };
        let prefix = args["prefix"].as_str();
        match db::kv_count(ctx.db.as_ref(), ctx.session_id, prefix) {
            Ok(count) => {
                debug!(session = ctx.session_id, prefix, count, "db_count ok");
                ToolExecutionOutput {
                    result: tool_ok(count.to_string()),
                    image: None,
                }
            }
            Err(e) => {
                error!(
                    session = ctx.session_id,
                    prefix,
                    error = %e,
                    "db_count failed"
                );
                ToolExecutionOutput {
                    result: tool_err(format!("db_count failed: {e}")),
                    image: None,
                }
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
        // Touch the SESSION_KV table so read-only tests (e.g. get on missing key)
        // don't fail with a table-not-found error.
        let _ = db::kv_set(&db, 0, "__init__", b"").unwrap();
        let _ = db::kv_delete(&db, 0, "__init__").unwrap();
        let ctx = ToolContext::new(42, db);
        (dir, ctx)
    }

    #[test]
    fn db_set_and_get_round_trip() {
        let (_dir, ctx) = test_context();
        let set_result = DbSet.execute_with_context(
            r#"{"key": "greeting", "value_b64": "aGVsbG8="}"#,
            None,
            None,
            Some(&ctx),
        );
        assert!(!set_result.result.is_error, "{}", set_result.result.content);
        assert_eq!(set_result.result.content, "ok");

        let get_result =
            DbGet.execute_with_context(r#"{"key": "greeting"}"#, None, None, Some(&ctx));
        assert!(!get_result.result.is_error, "{}", get_result.result.content);
        assert_eq!(get_result.result.content, "aGVsbG8=");
    }

    #[test]
    fn db_get_not_found() {
        let (_dir, ctx) = test_context();
        let result =
            DbGet.execute_with_context(r#"{"key": "nonexistent"}"#, None, None, Some(&ctx));
        assert!(!result.result.is_error);
        assert_eq!(result.result.content, "not found");
    }

    #[test]
    fn db_delete_existing() {
        let (_dir, ctx) = test_context();
        DbSet.execute_with_context(
            r#"{"key": "x", "value_b64": "eA=="}"#,
            None,
            None,
            Some(&ctx),
        );
        let result = DbDelete.execute_with_context(r#"{"key": "x"}"#, None, None, Some(&ctx));
        assert!(!result.result.is_error);
        assert_eq!(result.result.content, "deleted");
    }

    #[test]
    fn db_delete_not_found() {
        let (_dir, ctx) = test_context();
        let result =
            DbDelete.execute_with_context(r#"{"key": "nonexistent"}"#, None, None, Some(&ctx));
        assert!(!result.result.is_error);
        assert_eq!(result.result.content, "not found");
    }

    #[test]
    fn db_count_no_prefix() {
        let (_dir, ctx) = test_context();
        DbSet.execute_with_context(
            r#"{"key": "a", "value_b64": "YQ=="}"#,
            None,
            None,
            Some(&ctx),
        );
        DbSet.execute_with_context(
            r#"{"key": "b", "value_b64": "Yg=="}"#,
            None,
            None,
            Some(&ctx),
        );
        let result = DbCount.execute_with_context(r#"{}"#, None, None, Some(&ctx));
        assert!(!result.result.is_error);
        assert_eq!(result.result.content, "2");
    }

    #[test]
    fn db_count_with_prefix() {
        let (_dir, ctx) = test_context();
        DbSet.execute_with_context(
            r#"{"key": "aa", "value_b64": "YWE="}"#,
            None,
            None,
            Some(&ctx),
        );
        DbSet.execute_with_context(
            r#"{"key": "ab", "value_b64": "YWI="}"#,
            None,
            None,
            Some(&ctx),
        );
        DbSet.execute_with_context(
            r#"{"key": "ba", "value_b64": "YmE="}"#,
            None,
            None,
            Some(&ctx),
        );
        let result = DbCount.execute_with_context(r#"{"prefix": "a"}"#, None, None, Some(&ctx));
        assert!(!result.result.is_error);
        assert_eq!(result.result.content, "2");
    }

    #[test]
    fn db_list_range() {
        let (_dir, ctx) = test_context();
        DbSet.execute_with_context(
            r#"{"key": "apple", "value_b64": "YQ=="}"#,
            None,
            None,
            Some(&ctx),
        );
        DbSet.execute_with_context(
            r#"{"key": "banana", "value_b64": "Yg=="}"#,
            None,
            None,
            Some(&ctx),
        );
        DbSet.execute_with_context(
            r#"{"key": "cherry", "value_b64": "Yw=="}"#,
            None,
            None,
            Some(&ctx),
        );
        let result = DbList.execute_with_context(
            r#"{"start": "banana", "end": "cherry"}"#,
            None,
            None,
            Some(&ctx),
        );
        assert!(!result.result.is_error);
        assert_eq!(result.result.content, r#"["banana"]"#);
    }

    #[test]
    fn db_get_range_with_values() {
        let (_dir, ctx) = test_context();
        DbSet.execute_with_context(
            r#"{"key": "x", "value_b64": "eHh4"}"#,
            None,
            None,
            Some(&ctx),
        );
        let result = DbGetRange.execute_with_context(
            r#"{"start": "x", "end": "y"}"#,
            None,
            None,
            Some(&ctx),
        );
        assert!(!result.result.is_error);
        assert!(result.result.content.contains("x"));
        assert!(result.result.content.contains("eHh4"));
    }

    #[test]
    fn db_delete_range() {
        let (_dir, ctx) = test_context();
        DbSet.execute_with_context(
            r#"{"key": "a", "value_b64": "YQ=="}"#,
            None,
            None,
            Some(&ctx),
        );
        DbSet.execute_with_context(
            r#"{"key": "b", "value_b64": "Yg=="}"#,
            None,
            None,
            Some(&ctx),
        );
        DbSet.execute_with_context(
            r#"{"key": "c", "value_b64": "Yw=="}"#,
            None,
            None,
            Some(&ctx),
        );
        let result =
            DbDeleteRange.execute_with_context(r#"{"start": "b"}"#, None, None, Some(&ctx));
        assert!(!result.result.is_error);
        assert_eq!(result.result.content, "deleted 2 keys");
    }

    #[test]
    fn db_no_context_returns_error() {
        let result =
            DbSet.execute_with_context(r#"{"key": "x", "value_b64": "eA=="}"#, None, None, None);
        assert!(result.result.is_error);
        assert_eq!(result.result.content, "no session context");
    }
}
