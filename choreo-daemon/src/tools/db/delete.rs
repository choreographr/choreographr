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
pub struct DbDeleteArgs {
    /// The key to delete
    key: String,
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

#[cfg(test)]
mod tests {
    use super::*;
    use crate::tools::db::DbSet;
    use crate::tools::db::set::set_args;
    use crate::tools::db::tests::test_context;

    #[test]
    fn db_delete_existing() {
        let (_dir, ctx) = test_context();
        DbSet
            .execute(set_args("x", b"x"), None, None, Some(&ctx))
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
}
