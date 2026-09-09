use super::{
    Tool, ToolExecError,
    context::ToolContext,
    shell_util::{format_shell_output, resolve_workdir, run_shell_streaming, spawn_with_watchdog},
};
use base64::Engine as _;
use choreo_keystore::ServiceCredential;
use crossbeam_channel;
use schemars::JsonSchema;
use serde::Deserialize;
use std::path::Path;

/// Which PowerShell binary to spawn. `powershell` is Windows PowerShell 5.1
/// (ships with every Windows 10/11 install, so it is the always-present
/// baseline); `pwsh` is PowerShell 7+ (better UTF-8 and syntax defaults, but
/// a separate install). The tool is only registered on Windows when at least
/// one of the two binaries is on PATH, so the model never sees a tool that
/// cannot spawn.
#[derive(Debug, Clone, Copy, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum PsShell {
    /// Windows PowerShell 5.1 (`powershell.exe` — always present on Windows)
    Powershell,
    /// PowerShell 7+ (`pwsh.exe` — better defaults, separate install)
    Pwsh,
}

impl JsonSchema for PsShell {
    fn schema_name() -> std::borrow::Cow<'static, str> {
        std::borrow::Cow::Borrowed("PsShell")
    }

    fn schema_id() -> std::borrow::Cow<'static, str> {
        std::borrow::Cow::Borrowed(concat!(module_path!(), "::PsShell"))
    }

    fn json_schema(_gen: &mut schemars::SchemaGenerator) -> schemars::Schema {
        // Flat string enum — many OpenAI-compatible providers reject the
        // oneOf/const form schemars would otherwise emit (same rationale as
        // sh.rs's `Shell`).
        schemars::json_schema!({
            "type": "string",
            "enum": ["powershell", "pwsh"]
        })
    }
}

impl PsShell {
    /// The program name to spawn. std's Windows spawn path appends `.exe`
    /// automatically when the program name carries no extension, so the bare
    /// name works on Windows; on other platforms (pwsh is cross-platform)
    /// the bare name is what PATH lookup expects anyway.
    fn binary(self) -> &'static str {
        match self {
            PsShell::Powershell => "powershell",
            PsShell::Pwsh => "pwsh",
        }
    }
}

#[derive(Debug, Deserialize, JsonSchema)]
pub struct PsArgs {
    /// The PowerShell command to execute (runs via `-EncodedCommand`, so quotes and special characters need no escaping)
    pub command: String,
    /// Which PowerShell to use
    pub shell: PsShell,
    /// Working directory for the command (relative to the session working directory, or absolute)
    pub workdir: Option<String>,
    /// Timeout in milliseconds (default 30000; the daemon's outer deadline is raised to cover this when longer)
    pub timeout: Option<u64>,
}

pub(crate) struct PowerShell;

impl Tool for PowerShell {
    type Args = PsArgs;
    type Return = String;
    type Error = ToolExecError;

    fn name(&self) -> &'static str {
        "powershell"
    }

    fn group(&self) -> &'static str {
        "shell"
    }

    fn description(&self) -> &'static str {
        "Execute a PowerShell command using a POSIX-adjacent shell (Windows PowerShell 5.1 or PowerShell 7+). Runs via `-EncodedCommand` (Base64 UTF-16LE), so quotes and special characters need no escaping; output is forced to UTF-8. Non-interactive only — commands that read from stdin will hang. Use `exit N` to set a nonzero exit code. Prefer `exec` when you need to run a single program without shell features."
    }

    fn supports_streaming_output() -> bool {
        true
    }

    fn describe_invocation(&self, args: &Self::Args) -> String {
        let mut parts = vec![format!("Running PowerShell command: `{}`.", args.command)];
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
        execute_ps_tool(&args, working_dir)
    }

    fn execute_streaming(
        &self,
        args: Self::Args,
        _x_credentials: Option<&ServiceCredential>,
        working_dir: Option<&Path>,
        output_tx: crossbeam_channel::Sender<Vec<u8>>,
        _ctx: Option<&ToolContext>,
    ) -> Result<Self::Return, Self::Error> {
        let command = &args.command;
        let timeout_ms = args.timeout.unwrap_or(30000);
        let resolved = resolve_workdir(args.workdir.as_deref(), working_dir);

        let mut cmd = build_ps_command(args.shell, command, &resolved);
        run_shell_streaming(&mut cmd, command, timeout_ms, output_tx)
    }

    fn return_string(ret: &Self::Return) -> String {
        ret.clone()
    }
}

/// Prepend the output-encoding preamble to the user's command.
///
/// On Windows, child-process stdout redirected to a pipe is written in the
/// console code page (CP1252/CP437) — Windows PowerShell 5.1 especially —
/// while `format_shell_output` decodes as UTF-8 (lossy). Setting
/// `[Console]::OutputEncoding` to UTF-8 inside the script makes both the
/// shell's own output and native child output UTF-8, so non-ASCII text
/// survives the pipe intact. `$ProgressPreference='SilentlyContinue'`
/// additionally keeps PS 5.1's progress stream from polluting the redirected
/// stdout with its formatted progress records.
fn build_ps_script(command: &str) -> String {
    format!(
        "[Console]::OutputEncoding=[System.Text.Encoding]::UTF8; \
         $ProgressPreference='SilentlyContinue'; \
         {command}"
    )
}

/// Base64-encode the script as UTF-16LE — the exact format `-EncodedCommand`
/// expects. Encoding the whole script (preamble + user command) sidesteps
/// Windows' nested command-line quoting rules entirely: an LLM-generated
/// command containing any mix of single/double quotes, `%VAR%`, `!`, or
/// caret characters arrives byte-exact at the shell.
fn encode_ps_script(script: &str) -> String {
    let utf16le: Vec<u8> = script
        .encode_utf16()
        .flat_map(|unit| unit.to_le_bytes())
        .collect();
    base64::engine::general_purpose::STANDARD.encode(utf16le)
}

/// Build the `powershell`/`pwsh` invocation for `command` in `workdir`.
///
/// `-NoProfile` keeps user profile scripts out of the tool path (startup
/// speed, determinism, and no user script can veto the tool's UTF-8 preamble);
/// `-NonInteractive` fails fast instead of prompting (matching the other shell
/// tools' "commands that read from stdin will hang" contract).
pub(crate) fn build_ps_command(
    shell: PsShell,
    command: &str,
    workdir: &Path,
) -> std::process::Command {
    let encoded = encode_ps_script(&build_ps_script(command));
    let mut cmd = std::process::Command::new(shell.binary());
    cmd.args(["-NoProfile", "-NonInteractive", "-EncodedCommand", &encoded])
        .current_dir(workdir)
        .stdout(std::process::Stdio::piped())
        .stderr(std::process::Stdio::piped());
    cmd
}

pub fn execute_ps_tool(args: &PsArgs, working_dir: Option<&Path>) -> Result<String, ToolExecError> {
    let command = &args.command;
    // The per-tool timeout (default 30s) governs shell execution.
    // The outer deadline in execute_tool_with_timeout (300s) is the
    // absolute safety net — no independent cap needed here.
    let timeout_ms = args.timeout.unwrap_or(30000);

    let resolved = resolve_workdir(args.workdir.as_deref(), working_dir);
    tracing::debug!(
        shell = ?args.shell,
        timeout_ms,
        workdir = %resolved.display(),
        "executing powershell tool"
    );

    let mut cmd = build_ps_command(args.shell, command, &resolved);

    let (output, was_killed) = spawn_with_watchdog(&mut cmd, timeout_ms)?;

    Ok(format_shell_output(
        command, &output, timeout_ms, was_killed,
    ))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::tools::Tool;
    use schemars::SchemaGenerator;

    #[test]
    fn powershell_tool_has_valid_metadata() {
        let tool = super::PowerShell;
        assert!(!tool.name().is_empty());
        assert!(!tool.description().is_empty());
        let schema = tool.schema();
        assert!(schema.is_object());
    }

    #[test]
    fn ps_shell_enum_json_schema_uses_flat_enum_format() {
        // Same provider-compatibility contract as sh.rs's `Shell`: a flat
        // string enum, not oneOf/const.
        let mut generator = SchemaGenerator::default();
        let schema = super::PsShell::json_schema(&mut generator);
        let json: serde_json::Value = serde_json::to_value(&schema).unwrap();
        assert_eq!(json["type"], "string", "PsShell should be a string schema");
        let variants: Vec<&str> = json["enum"]
            .as_array()
            .expect("PsShell should have an enum array")
            .iter()
            .filter_map(|v| v.as_str())
            .collect();
        assert_eq!(variants, vec!["powershell", "pwsh"]);
    }

    #[test]
    fn powershell_tool_schema_shell_param_uses_flat_enum() {
        let schema = super::PowerShell.schema();
        let shell_schema = &schema["properties"]["shell"];
        assert_eq!(shell_schema["type"], "string");
        let variants: Vec<&str> = shell_schema["enum"]
            .as_array()
            .expect("shell property should have an enum array")
            .iter()
            .filter_map(|v| v.as_str())
            .collect();
        assert_eq!(variants, vec!["powershell", "pwsh"]);
    }

    #[test]
    fn build_ps_script_prepends_utf8_preamble() {
        // The preamble must come FIRST so the UTF-8 output encoding is in
        // force before the user's command produces any output, and the user's
        // command must be appended verbatim.
        let script = build_ps_script("Write-Output 'héllo'");
        assert!(script.starts_with("[Console]::OutputEncoding="));
        assert!(script.contains("$ProgressPreference='SilentlyContinue'"));
        assert!(script.ends_with("Write-Output 'héllo'"));
    }

    #[test]
    fn encode_ps_script_round_trips_utf16le() {
        // -EncodedCommand is Base64 of UTF-16LE; decode it back and confirm
        // the exact script survives (this is the quoting-safety property the
        // tool relies on — any mix of quotes/specials must arrive untouched).
        let script = build_ps_script("echo \"a'b\" %PATH% ^! ~");
        let encoded = encode_ps_script(&script);
        let bytes = base64::engine::general_purpose::STANDARD
            .decode(&encoded)
            .expect("valid base64");
        assert_eq!(bytes.len() % 2, 0, "UTF-16LE is whole 2-byte units");
        let units: Vec<u16> = bytes
            .as_chunks::<2>()
            .0
            .iter()
            .map(|c| u16::from_le_bytes(*c))
            .collect();
        let decoded = String::from_utf16(&units).expect("valid UTF-16");
        assert_eq!(decoded, script);
    }

    #[test]
    fn build_ps_command_uses_encodedcommand_and_pipes() {
        // The spawn shape the shell_util watchdog/streaming paths rely on:
        // the four fixed args, the encoded payload, and both stdio pipes.
        let cmd = build_ps_command(
            PsShell::Powershell,
            "Get-ChildItem",
            std::path::Path::new("."),
        );
        assert_eq!(cmd.get_program(), "powershell");
        let args: Vec<String> = cmd
            .get_args()
            .map(|a| a.to_string_lossy().into_owned())
            .collect();
        assert_eq!(
            &args[..3],
            ["-NoProfile", "-NonInteractive", "-EncodedCommand"]
        );
        let encoded = &args[3];
        let decoded = base64::engine::general_purpose::STANDARD
            .decode(encoded)
            .expect("valid base64");
        let units: Vec<u16> = decoded
            .as_chunks::<2>()
            .0
            .iter()
            .map(|c| u16::from_le_bytes(*c))
            .collect();
        let script = String::from_utf16(&units).expect("valid UTF-16");
        assert!(script.ends_with("Get-ChildItem"));
    }

    #[test]
    fn ps_shell_binaries() {
        assert_eq!(PsShell::Powershell.binary(), "powershell");
        assert_eq!(PsShell::Pwsh.binary(), "pwsh");
    }
}
