use crate::tools::{ToolExecError, context::ToolContext, truncate_tool_output};
use choreo_keystore::ServiceCredential;
use schemars::JsonSchema;
use serde::Deserialize;
use std::path::Path;

use super::{format_x_api_response, x_api_get};

#[derive(Debug, Deserialize, JsonSchema)]
pub struct XSearchRecentArgs {
    /// Search query
    pub query: String,
    /// Maximum number of results to return
    pub max_results: Option<u64>,
}

fn execute_x_search_recent_tool(
    args: &XSearchRecentArgs,
    x_credentials: Option<&ServiceCredential>,
) -> Result<String, ToolExecError> {
    let query = args.query.trim();
    if query.is_empty() {
        return Err(ToolExecError("query must not be empty".to_string()));
    }

    let max_results = args.max_results.unwrap_or(10).clamp(10, 100);
    let max_results_str = max_results.to_string();

    let params = vec![
        ("query", query),
        ("max_results", &max_results_str),
        ("tweet.fields", "created_at,author_id,public_metrics"),
    ];

    x_api_get("/2/tweets/search/recent", &params, x_credentials)
        .map(|response| {
            truncate_tool_output(&format!(
                "search results for '{query}':\n{}",
                format_x_api_response(&response)
            ))
        })
        .map_err(|e| ToolExecError(truncate_tool_output(&e)))
}

pub(crate) struct XSearchRecent;

impl crate::tools::Tool for XSearchRecent {
    type Args = XSearchRecentArgs;
    type Return = String;
    type Error = ToolExecError;

    fn name(&self) -> &'static str {
        "x_search_recent"
    }

    fn group(&self) -> &'static str {
        "x"
    }

    fn description(&self) -> &'static str {
        "Search recent tweets on X (Twitter). Requires X credentials to be configured via the keystore."
    }

    fn describe_invocation(&self, args: &Self::Args) -> String {
        let mut parts = vec![format!("Searching X for: `{}`.", args.query)];
        if let Some(max) = args.max_results {
            parts.push(format!(" Max results: {}.", max));
        }
        parts.concat()
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
        execute_x_search_recent_tool(&args, x_credentials)
    }
}
