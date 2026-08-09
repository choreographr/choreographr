use super::DbError;
use crate::db;
use crate::tools::Tool;
use crate::tools::context::ToolContext;
use choreo_keystore::ServiceCredential;
use schemars::JsonSchema;
use serde::Deserialize;
use tracing::{debug, error};

// ── Args structs ────────────────────────────────────────────────────────

#[derive(Debug, Deserialize, JsonSchema)]
pub struct DbGetArgs {
    /// The key to retrieve
    key: String,
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

#[cfg(test)]
pub(crate) fn get_args(key: &str) -> DbGetArgs {
    // Test-only constructor: DbGetArgs fields are private, so sibling tool
    // files' cross-tool tests cannot build the struct with a literal.
    // Kept `#[cfg(test)]` so production builds never see it.
    DbGetArgs {
        key: key.to_string(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::tools::db::tests::test_context;

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

    // ── output_schema tests ──────────────────────────────────────────

    #[test]
    fn db_get_output_schema_is_none() {
        assert!(
            DbGet.output_schema().is_none(),
            "DbGet returns raw bytes converted lossily to string; no structured JSON schema"
        );
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
}
