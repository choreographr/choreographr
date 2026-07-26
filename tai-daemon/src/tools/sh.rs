use super::{
    Tool, ToolExecError,
    context::ToolContext,
    shell_util::{
        format_shell_output, resolve_and_confine, run_shell_streaming, setup_child,
        spawn_with_watchdog,
    },
};
use schemars::JsonSchema;
use serde::Deserialize;
use std::path::Path;
use std::sync::mpsc;
use tai_keystore::ServiceCredential;

#[derive(Debug, Clone, Copy, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Shell {
    /// Bourne Again SHell
    Bash,
    /// Debian Almquist SHell
    Dash,
    /// Z SHell
    Zsh,
}

impl JsonSchema for Shell {
    fn schema_name() -> std::borrow::Cow<'static, str> {
        std::borrow::Cow::Borrowed("Shell")
    }

    fn schema_id() -> std::borrow::Cow<'static, str> {
        std::borrow::Cow::Borrowed(concat!(module_path!(), "::Shell"))
    }

    fn json_schema(_gen: &mut schemars::SchemaGenerator) -> schemars::Schema {
        // Use the simple "enum" format instead of schemars' default
        // "oneOf" with "const" — many OpenAI-compatible providers do
        // not support the const keyword in tool parameter schemas.
        schemars::json_schema!({
            "type": "string",
            "enum": ["bash", "dash", "zsh"]
        })
    }
}

#[derive(Debug, Deserialize, JsonSchema)]
pub struct ShArgs {
    /// The shell command to execute (runs via `<shell> -c`)
    pub command: String,
    /// Which POSIX-compatible shell to use
    pub shell: Shell,
    /// Working directory for the command (relative to the session working directory, or absolute)
    pub workdir: Option<String>,
    /// Timeout in milliseconds (default 30000; capped by outer tool deadline)
    pub timeout: Option<u64>,
}

pub(crate) struct Sh;

impl Tool for Sh {
    type Args = ShArgs;
    type Return = String;
    type Error = ToolExecError;

    fn name(&self) -> &'static str {
        "sh"
    }

    fn group(&self) -> &'static str {
        "shell"
    }

    fn description(&self) -> &'static str {
        "Execute a shell command using a POSIX-compatible shell (bash, dash, or zsh). Supports pipes, redirects, glob expansion, and environment variables. Prefer this over `exec` when you need shell features. Non-interactive only — commands that read from stdin will hang. The `shell` parameter must be explicitly specified (bash, dash, or zsh)."
    }

    fn supports_streaming_output() -> bool {
        true
    }

    fn describe_invocation(&self, args: &Self::Args) -> String {
        let mut parts = vec![format!("Running shell command: `{}`.", args.command)];
        parts.push(format!(" Shell: {:?}.", args.shell));
        if let Some(timeout) = args.timeout {
            parts.push(format!(" Timeout: {}ms.", timeout));
        }
        parts.concat()
    }

    fn execute(
        &self,
        args: Self::Args,
        _x_credentials: Option<&ServiceCredential>,
        working_dir: Option<&Path>,
        _ctx: Option<&ToolContext>,
    ) -> Result<Self::Return, Self::Error> {
        execute_sh_tool(&args, working_dir)
    }

    fn execute_streaming(
        &self,
        args: Self::Args,
        _x_credentials: Option<&ServiceCredential>,
        working_dir: Option<&Path>,
        output_tx: mpsc::Sender<Vec<u8>>,
        _ctx: Option<&ToolContext>,
    ) -> Result<Self::Return, Self::Error> {
        let shell_str = match args.shell {
            Shell::Bash => "bash",
            Shell::Dash => "dash",
            Shell::Zsh => "zsh",
        };
        let command = &args.command;
        let timeout_ms = args.timeout.unwrap_or(30000);
        let resolved = resolve_and_confine(args.workdir.as_deref(), working_dir)?;

        let mut cmd = std::process::Command::new(shell_str);
        cmd.args(["-c", command])
            .current_dir(&resolved)
            .stdout(std::process::Stdio::piped())
            .stderr(std::process::Stdio::piped());

        run_shell_streaming(&mut cmd, command, timeout_ms, output_tx)
    }

    fn return_string(ret: &Self::Return) -> String {
        ret.clone()
    }
}

pub fn execute_sh_tool(args: &ShArgs, working_dir: Option<&Path>) -> Result<String, ToolExecError> {
    let shell_str = match args.shell {
        Shell::Bash => "bash",
        Shell::Dash => "dash",
        Shell::Zsh => "zsh",
    };
    let command = &args.command;
    // The per-tool timeout (default 30s) governs shell execution.
    // The outer deadline in execute_tool_with_timeout (300s) is the
    // absolute safety net — no independent cap needed here.
    let timeout_ms = args.timeout.unwrap_or(30000);

    let resolved = resolve_and_confine(args.workdir.as_deref(), working_dir)?;

    let mut cmd = std::process::Command::new(shell_str);
    cmd.args(["-c", command])
        .current_dir(&resolved)
        .stdout(std::process::Stdio::piped())
        .stderr(std::process::Stdio::piped());

    setup_child(&mut cmd);

    let (output, was_killed) = spawn_with_watchdog(&mut cmd, timeout_ms)?;

    Ok(format_shell_output(
        command, &output, timeout_ms, was_killed,
    ))
}

#[cfg(test)]
mod tests {
    use crate::tools::Tool;
    use schemars::JsonSchema;
    use schemars::SchemaGenerator;

    #[test]
    fn sh_tool_has_valid_metadata() {
        let tool = super::Sh;
        assert!(!tool.name().is_empty());
        assert!(!tool.description().is_empty());
        let schema = tool.schema();
        assert!(schema.is_object());
    }

    #[test]
    fn shell_enum_json_schema_uses_flat_enum_format() {
        let mut generator = SchemaGenerator::default();
        let schema = super::Shell::json_schema(&mut generator);
        let json: serde_json::Value = serde_json::to_value(&schema).unwrap();
        // Should use simple string enum, not oneOf/const
        assert_eq!(json["type"], "string", "Shell should be a string schema");
        let variants: Vec<&str> = json["enum"]
            .as_array()
            .expect("Shell should have an enum array")
            .iter()
            .filter_map(|v| v.as_str())
            .collect();
        assert_eq!(variants, vec!["bash", "dash", "zsh"]);
    }

    #[test]
    fn sh_tool_schema_shell_param_uses_flat_enum() {
        let schema = super::Sh.schema();
        // The shell property should use the flat enum format
        let shell_schema = &schema["properties"]["shell"];
        assert_eq!(shell_schema["type"], "string");
        let variants: Vec<&str> = shell_schema["enum"]
            .as_array()
            .expect("shell parameter should have enum")
            .iter()
            .filter_map(|v| v.as_str())
            .collect();
        assert_eq!(variants, vec!["bash", "dash", "zsh"]);
    }
}
