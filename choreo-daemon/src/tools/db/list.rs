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
pub struct DbListArgs {
    /// Start of the key range (inclusive). If omitted, starts from the beginning.
    start: Option<String>,
    /// End of the key range (exclusive). If omitted, goes to the end.
    end: Option<String>,
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

#[cfg(test)]
mod tests {
    use super::*;
    use crate::tools::db::DbSet;
    use crate::tools::db::set::set_args;
    use crate::tools::db::tests::test_context;

    #[test]
    fn db_list_range() {
        let (_dir, ctx) = test_context();
        DbSet
            .execute(set_args("apple", b"a"), None, None, Some(&ctx))
            .unwrap();
        DbSet
            .execute(set_args("banana", b"b"), None, None, Some(&ctx))
            .unwrap();
        DbSet
            .execute(set_args("cherry", b"c"), None, None, Some(&ctx))
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

    // ── output_schema tests ──────────────────────────────────────────

    #[test]
    fn db_list_output_schema_is_array_of_strings() {
        let schema = DbList.output_schema().expect("schema");
        assert_eq!(schema["type"], "array");
        assert_eq!(schema["items"]["type"], "string");
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
}
