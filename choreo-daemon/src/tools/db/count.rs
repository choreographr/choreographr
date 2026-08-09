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
pub struct DbCountArgs {
    /// Optional prefix to filter keys by. If omitted, counts all keys.
    prefix: Option<String>,
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
    use crate::tools::db::DbSet;
    use crate::tools::db::set::set_args;
    use crate::tools::db::tests::test_context;

    #[test]
    fn db_count_no_prefix() {
        let (_dir, ctx) = test_context();
        DbSet
            .execute(set_args("a", b"a"), None, None, Some(&ctx))
            .unwrap();
        DbSet
            .execute(set_args("b", b"b"), None, None, Some(&ctx))
            .unwrap();
        let result = DbCount.execute(DbCountArgs { prefix: None }, None, None, Some(&ctx));
        assert_eq!(result.unwrap(), 2);
    }

    #[test]
    fn db_count_with_prefix() {
        let (_dir, ctx) = test_context();
        DbSet
            .execute(set_args("aa", b"aa"), None, None, Some(&ctx))
            .unwrap();
        DbSet
            .execute(set_args("ab", b"ab"), None, None, Some(&ctx))
            .unwrap();
        DbSet
            .execute(set_args("ba", b"ba"), None, None, Some(&ctx))
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

    // ── output_schema tests ──────────────────────────────────────────

    #[test]
    fn db_count_output_schema_is_integer() {
        let schema = DbCount.output_schema().expect("schema");
        assert_eq!(schema["type"], "integer");
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
