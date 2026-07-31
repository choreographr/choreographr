use super::{DbError, DbValue};
use crate::db;
use crate::tools::Tool;
use crate::tools::context::ToolContext;
use choreo_keystore::ServiceCredential;
use schemars::JsonSchema;
use serde::Deserialize;
use tracing::{debug, error};

// ── Args structs ────────────────────────────────────────────────────────

#[derive(Debug, Deserialize, JsonSchema)]
pub struct DbSetArgs {
    /// The key to set
    key: String,
    /// The value to store
    value: DbValue,
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

#[cfg(test)]
pub(crate) fn set_args(key: &str, value: &[u8]) -> DbSetArgs {
    // Test-only constructor: DbSetArgs fields are private, so sibling tool
    // files' cross-tool tests (which need to seed values via DbSet) cannot
    // build the struct with a literal. Kept `#[cfg(test)]` so production
    // builds never see it.
    DbSetArgs {
        key: key.to_string(),
        value: DbValue(value.to_vec()),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::tools::db::DbGet;
    use crate::tools::db::get::get_args;
    use crate::tools::db::tests::test_context;

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

        let result = DbGet.execute(get_args("greeting"), None, None, Some(&ctx));
        assert_eq!(result.unwrap(), Some("hello".into()));
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
}
