use super::DbError;
use crate::db;
use crate::tools::Tool;
use crate::tools::context::ToolContext;
use base64::{Engine as _, engine::general_purpose::STANDARD as BASE64};
use choreo_keystore::ServiceCredential;
use schemars::JsonSchema;
use serde::{Deserialize, Serialize};
use tracing::{debug, error};

// ── Args structs ────────────────────────────────────────────────────────

#[derive(Debug, Deserialize, JsonSchema)]
pub struct DbGetRangeArgs {
    /// Start of the key range (inclusive)
    start: String,
    /// End of the key range (exclusive). If omitted, retrieves from start to end of session's keys.
    end: Option<String>,
}

#[derive(Debug, Serialize, JsonSchema)]
pub struct DbGetRangeEntry {
    /// The key
    key: String,
    /// The value, base64-encoded
    value_b64: String,
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

#[cfg(test)]
mod tests {
    use super::*;
    use crate::tools::db::DbSet;
    use crate::tools::db::set::set_args;
    use crate::tools::db::tests::test_context;

    #[test]
    fn db_get_range_with_values() {
        let (_dir, ctx) = test_context();
        DbSet
            .execute(set_args("x", b"xxx"), None, None, Some(&ctx))
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

    // ── output_schema tests ──────────────────────────────────────────

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
}
