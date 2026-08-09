use crate::tools::{ToolExecError, context::ToolContext, truncate_tool_output};
use choreo_keystore::ServiceCredential;
use schemars::JsonSchema;
use serde::Deserialize;
use std::path::Path;

use super::{format_x_api_response, x_api_post};

#[derive(Debug, Deserialize, JsonSchema)]
pub struct XPostArgs {
    /// Text content of the post
    pub text: String,
}

fn execute_x_post_tool(
    args: &XPostArgs,
    x_credentials: Option<&ServiceCredential>,
) -> Result<String, ToolExecError> {
    let text = args.text.trim();
    if text.is_empty() {
        return Err(ToolExecError("text must not be empty".to_string()));
    }

    let body = serde_json::json!({ "text": text }).to_string();

    x_api_post("/2/tweets", &body, x_credentials)
        .map(|response| {
            truncate_tool_output(&format!(
                "tweet posted successfully:\n{}",
                format_x_api_response(&response)
            ))
        })
        .map_err(|e| ToolExecError(truncate_tool_output(&e)))
}

pub(crate) struct XPost;

impl crate::tools::Tool for XPost {
    type Args = XPostArgs;
    type Return = String;
    type Error = ToolExecError;

    fn name(&self) -> &'static str {
        "x_post"
    }

    fn group(&self) -> &'static str {
        "x"
    }

    fn description(&self) -> &'static str {
        "Post a tweet to X (Twitter). Requires X credentials to be configured via the keystore."
    }

    fn describe_invocation(&self, args: &Self::Args) -> String {
        let preview: String = args.text.chars().take(80).collect();
        format!(
            "Posting to X ({} characters): \"{}\".",
            args.text.len(),
            preview
        )
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
        execute_x_post_tool(&args, x_credentials)
    }
}
