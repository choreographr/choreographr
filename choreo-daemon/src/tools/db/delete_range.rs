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
pub struct DbDeleteRangeArgs {
    /// Start of the key range (inclusive)
    start: String,
    /// End of the key range (exclusive). If omitted, deletes from start to end of session's keys.
    end: Option<String>,
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

#[cfg(test)]
mod tests {
    use super::*;
    use crate::tools::db::DbSet;
    use crate::tools::db::set::set_args;
    use crate::tools::db::tests::test_context;

    #[test]
    fn db_delete_range() {
        let (_dir, ctx) = test_context();
        DbSet
            .execute(set_args("a", b"a"), None, None, Some(&ctx))
            .unwrap();
        DbSet
            .execute(set_args("b", b"b"), None, None, Some(&ctx))
            .unwrap();
        DbSet
            .execute(set_args("c", b"c"), None, None, Some(&ctx))
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
    fn describe_db_delete_range_invocation() {
        let tool = DbDeleteRange;
        let args = DbDeleteRangeArgs {
            start: "a".into(),
            end: Some("z".into()),
        };
        let desc = tool.describe_invocation(&args);
        assert_eq!(desc, "Deleting database keys from `a` to z.");
    }
}
