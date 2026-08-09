use crate::tools::{ToolExecError, context::ToolContext, truncate_tool_output};
use choreo_keystore::ServiceCredential;
use schemars::JsonSchema;
use serde::Deserialize;
use std::path::Path;

use super::{format_x_api_response, x_api_get};

#[derive(Debug, Deserialize, JsonSchema)]
pub struct XUserLookupArgs {
    /// X/Twitter username (without @)
    pub username: String,
}

fn execute_x_user_lookup_tool(
    args: &XUserLookupArgs,
    x_credentials: Option<&ServiceCredential>,
) -> Result<String, ToolExecError> {
    let username = args.username.trim();
    if username.is_empty() {
        return Err(ToolExecError("username must not be empty".to_string()));
    }

    let params = vec![("user.fields", "description,public_metrics,created_at")];

    x_api_get(
        &format!("/2/users/by/username/{username}"),
        &params,
        x_credentials,
    )
    .map(|response| {
        truncate_tool_output(&format!(
            "user lookup for @{username}:\n{}",
            format_x_api_response(&response)
        ))
    })
    .map_err(|e| ToolExecError(truncate_tool_output(&e)))
}

pub(crate) struct XUserLookup;

impl crate::tools::Tool for XUserLookup {
    type Args = XUserLookupArgs;
    type Return = String;
    type Error = ToolExecError;

    fn name(&self) -> &'static str {
        "x_user_lookup"
    }

    fn group(&self) -> &'static str {
        "x"
    }

    fn description(&self) -> &'static str {
        "Look up a user on X (Twitter) by username or ID. Requires X credentials to be configured via the keystore."
    }

    fn describe_invocation(&self, args: &Self::Args) -> String {
        format!("Looking up X user `{}`.", args.username)
    }

    fn return_string(ret: &Self::Return) -> String {
        ret.clone()
    }

    fn execute(
        &self,
        args: Self::Args,
        x_credentials: Option<&ServiceCredential>,
        _working_dir: Option<&Path>,
        _ctx: Option<&ToolContext>,
    ) -> Result<Self::Return, Self::Error> {
        execute_x_user_lookup_tool(&args, x_credentials)
    }
}
