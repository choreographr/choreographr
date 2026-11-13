//! Unit tests for the request builder, the agent loop, and tool dispatch,
//! split by subject into the child modules below.
//!
//! This parent holds the fixtures shared across children (`core_protected`,
//! `make_session_with_turns`, the provider/model consts, the image,
//! reasoning, and agent-loop harnesses, and `build_system_content` fixtures),
//! plus the `mod` declarations. Each child owns one concern's tests and
//! imports exactly what it uses on top of the shared `super::*`.

// NOTE: the #[serial_test::serial(...)] attribute on this test module
// re-emits it without the #[cfg(test)] marker, so clippy's
// allow-*-in-tests config no longer recognizes it as test code —
// the AGENTS.md test allowances are spelled out here instead.
#![expect(clippy::unwrap_used, clippy::expect_used, clippy::indexing_slicing)]
use super::*;
// Test-only handles into the recovery unit: the loop itself reads these
// constants through the recovery module, so they are not re-exported from
// `requests` — the budget tests assert against the exact numbers the recovery
// constants carry, and the agent-loop tests assert on the seeded guidance.
use super::recovery::{
    MAX_TRUNCATION_RECOVERIES, MAX_UNNAMED_TOOL_CALL_RECOVERIES,
    TRUNCATION_MIXED_RECOVERY_INSTRUCTION, TRUNCATION_RECOVERY_INSTRUCTION,
    UNNAMED_TOOL_CALL_RECOVERY_INSTRUCTION,
};
// The turn arms live in their own modules now, so the agent loop's imports no
// longer carry the types the tests build (a reply, a producer, a tool call) or
// the std handles they need. This parent is the shared origin for its children's
// `use super::*`, so they are imported here once.
use crate::context::LoadedSkill;
use crate::daemon::DaemonCommand;
use crate::providers::InferenceProvider;
use crate::providers::test_util::{
    PROSE_WITH_TRUNCATED_CALL, ScriptedTurn, make_failing_provider, make_prose_truncating_provider,
    make_scripted_stream_provider, make_scripted_turn_provider,
    make_scripted_turn_provider_with_slug, make_test_provider, make_tool_loop_provider,
    make_tool_loop_provider_with_tool, make_truncating_provider,
    make_truncating_provider_with_slug,
};
use crate::reasoning::{
    build_chat_request_messages, chains_via_response_id, initial_prev_resp_id,
    warn_on_uncaptured_reasoning,
};
use crate::sessions::AssistantResponse;
use crate::tools::context::ToolContext;
use crate::tools::{Tool, ToolError, ToolExecError, ToolOutput, ToolRegistry};
use choreo_ai_protocols::ChatToolCall;
use choreo_ai_protocols::StreamEvent;
use choreo_ai_protocols::chat_reasoning_echo;
use choreo_ai_protocols::openai::{
    AssistantToolCall, AssistantToolFunction, ChatRequestMessage, ChatToolDefinition,
    chat_wire_messages,
};
use choreo_keystore::ServiceCredential;
use choreo_proto::{AssistantToolCallRecord, ReasoningProducer};
use choreo_proto::{ChatReasoningField, ReasoningArtifact, ToolCallCaller};
use std::collections::HashSet;
use std::path::Path;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::mpsc;
use std::time::{Duration, Instant};

/// Protected set for the mirror tests: just "core" (no platform tools
/// registered in these unit tests).
fn core_protected() -> std::collections::HashSet<String> {
    std::collections::HashSet::from(["core".into()])
}

fn make_session_with_turns() -> SessionState {
    let mut session = SessionState::empty();
    let (tid0, _) = session.start_turn(Some("hello".into()));
    session.set_assistant_response(
        tid0,
        AssistantResponse {
            text: Some("hi".into()),
            ..Default::default()
        },
    );
    session
}

// Neutral provider/model for structure-only tests: the slug is not in the
// catalog, so the passback policy resolves to None and no artifact is ever
// attached — the exact behavior those tests assert.
const TEST_PROVIDER: &str = "test-stub";
const TEST_MODEL: &str = "test-model";

/// Write a tiny opaque PNG to a temp file and return the handle plus its path.
fn write_temp_png() -> tempfile::NamedTempFile {
    let buf = image::ImageBuffer::from_fn(3, 2, |x, y| {
        // u8 pixel coordinates (3×2 image): the arithmetic never exceeds u8.
        #[expect(clippy::cast_possible_truncation)]
        image::Rgb([(x * 80) as u8, (y * 90) as u8, 40])
    });
    let mut file = tempfile::NamedTempFile::new().expect("temp png");
    image::DynamicImage::ImageRgb8(buf)
        .write_to(&mut file, image::ImageFormat::Png)
        .expect("write png");
    file
}

/// Build a session whose single turn's `read_image` tool result carries an
/// image reference to a real temp PNG.
fn session_with_image_result() -> (SessionState, tempfile::NamedTempFile) {
    let file = write_temp_png();
    let mut session = SessionState::empty();
    let (tid, _) = session.start_turn(Some("look at this image".into()));
    session.set_assistant_response(
        tid,
        AssistantResponse {
            text: Some("Reading the image.".into()),
            tool_calls: vec![AssistantToolCallRecord {
                call_id: "call_img".into(),
                name: "read_image".into(),
                arguments_json: r#"{"path": "/tmp/x.png"}"#.into(),
                caller: None,
            }],
            ..Default::default()
        },
    );
    let calls = session.turns[&tid].tool_calls.clone();
    session.seed_tool_results(tid, &calls, &[String::new()]);
    let img_ref = choreo_proto::ImageReference {
        path: file.path().display().to_string(),
        mime_type: "image/jpeg".into(),
        width: 3,
        height: 2,
        // Non-empty stored bytes so the vision-attach path attaches the image
        // (an empty `data` would degrade to a placeholder).
        data: vec![0u8; 16],
    };
    session.update_tool_result(
        tid,
        "call_img",
        "read_image".into(),
        &ToolOutput {
            content: "read image: (3x2, image/jpeg)".into(),
            is_error: false,
            invocation_description: String::new(),
            image_ref: Some(img_ref),
            ..Default::default()
        },
    );
    (session, file)
}

/// Append a turn whose `read_image` tool result carries an image reference
/// with NON-empty stored bytes at `image_path`, in the same shape
/// `session_with_image_result` builds.
fn add_image_turn(session: &mut SessionState, user_text: &str, image_path: &str) -> u32 {
    let (tid, _) = session.start_turn(Some(user_text.to_string()));
    session.set_assistant_response(
        tid,
        AssistantResponse {
            text: Some("Reading the image.".into()),
            tool_calls: vec![AssistantToolCallRecord {
                call_id: format!("call_img_{tid}"),
                name: "read_image".into(),
                arguments_json: r#"{"path": "/tmp/x.png"}"#.into(),
                caller: None,
            }],
            ..Default::default()
        },
    );
    let calls = session.turns[&tid].tool_calls.clone();
    session.seed_tool_results(tid, &calls, &[String::new()]);
    let img_ref = choreo_proto::ImageReference {
        path: image_path.to_string(),
        mime_type: "image/jpeg".into(),
        width: 3,
        height: 2,
        data: vec![0u8; 16],
    };
    session.update_tool_result(
        tid,
        &format!("call_img_{tid}"),
        "read_image".into(),
        &ToolOutput {
            content: "read image: (3x2, image/jpeg)".into(),
            is_error: false,
            invocation_description: String::new(),
            image_ref: Some(img_ref),
            ..Default::default()
        },
    );
    tid
}

fn tool_call_record(call_id: &str, name: &str) -> AssistantToolCallRecord {
    AssistantToolCallRecord {
        call_id: call_id.into(),
        name: name.into(),
        arguments_json: "{}".into(),
        caller: None,
    }
}

fn deepseek_producer() -> ReasoningProducer {
    ReasoningProducer {
        provider_slug: "deepseek".into(),
        model: "deepseek-v4-pro".into(),
    }
}

fn anthropic_producer() -> ReasoningProducer {
    ReasoningProducer {
        provider_slug: "anthropic".into(),
        model: "claude-sonnet-5".into(),
    }
}

/// Non-DeepSeek OpenAI-compat chat provider: `ToolLoop` passback but no
/// `reasoning_content` injection — the generalized fallback must cover it.
const GROQ_MODEL: &str = "groq/llama-3.3-70b-versatile";

fn groq_producer() -> ReasoningProducer {
    ReasoningProducer {
        provider_slug: "groq".into(),
        model: GROQ_MODEL.into(),
    }
}

/// Pinned to `reasoning_passback = none` by the bundled overlay (the gateway
/// rejects replayed `reasoning_content`) — the fallback must respect it.
fn cerebras_producer() -> ReasoningProducer {
    ReasoningProducer {
        provider_slug: "cerebras".into(),
        model: "gpt-oss-120b".into(),
    }
}

fn artifact(bytes: &[u8]) -> ReasoningArtifact {
    ReasoningArtifact::ChatReasoning {
        field: ChatReasoningField::ReasoningContent,
        bytes: bytes.to_vec(),
    }
}

/// Add a turn with an optional artifact/producer and optional assistant
/// tool calls, returning its `turn_id`.
fn add_turn(
    session: &mut SessionState,
    user_text: &str,
    assistant_text: &str,
    artifact: Option<ReasoningArtifact>,
    producer: Option<ReasoningProducer>,
    tool_calls: Vec<AssistantToolCallRecord>,
) -> u32 {
    let (tid, _) = session.start_turn(Some(user_text.to_string()));
    session.set_assistant_response(
        tid,
        AssistantResponse {
            text: Some(assistant_text.to_string()),
            tool_calls,
            reasoning_artifact: artifact,
            reasoning_producer: producer,
            ..Default::default()
        },
    );
    tid
}

fn assistant_messages(result: &[ChatRequestMessage]) -> Vec<&ChatRequestMessage> {
    result.iter().filter(|m| m.role == "assistant").collect()
}

/// Lower the built messages exactly as the chat adapter would for this route,
/// so a wire assertion cannot drift from the real request.
fn wire_json(
    messages: &[ChatRequestMessage],
    provider: &str,
    model: &str,
) -> Vec<serde_json::Value> {
    chat_wire_messages(messages, chat_reasoning_echo(provider, model))
        .messages
        .iter()
        .map(|m| serde_json::to_value(m).expect("serialize wire message"))
        .collect()
}

/// The preserved Responses reasoning items, with the opaque encrypted payload
/// a stateless request asks for.
fn responses_items() -> ReasoningArtifact {
    ReasoningArtifact::ResponsesItems(
        br#"[{"type":"reasoning","id":"rs_1","encrypted_content":"eJxT_opaque"}]"#.to_vec(),
    )
}

fn config_change_call(name: &str, arguments_json: &str) -> ChatToolCall {
    ChatToolCall {
        id: "call_1".into(),
        name: name.into(),
        arguments_json: arguments_json.into(),
        caller: None,
    }
}

/// A successful tool output with the given structured result (or `None`
/// for tools whose `result_json` wasn't captured).
fn ok_output(result_json: Option<serde_json::Value>) -> ToolOutput {
    ToolOutput {
        content: String::new(),
        is_error: false,
        invocation_description: String::new(),
        image_ref: None,
        result_json,
    }
}

/// Shared setup for an agent-loop test: the request context, the session the
/// loop mutates, and the endpoints the loop broadcasts to.
///
/// The receivers and the temp dir live in the struct rather than being dropped
/// at the call site: a dropped receiver makes every broadcast send fail, which
/// aborts the loop at its first status update, so they must outlive the run.
struct AgentLoopHarness {
    ctx: RequestContext,
    session: SessionState,
    db: Arc<redb::Database>,
    cmd_rx: crossbeam_channel::Receiver<SessionCommand>,
    _daemon_rx: crossbeam_channel::Receiver<DaemonCommand>,
    _dir: tempfile::TempDir,
}

/// Build a harness over `registry` with `max_turns == 0`, so a test's loop is
/// bounded only by the behaviour under test.
fn agent_loop_harness(registry: ToolRegistry) -> AgentLoopHarness {
    let (daemon_tx, daemon_rx) = crossbeam_channel::unbounded::<DaemonCommand>();
    let (cmd_tx, cmd_rx) = crossbeam_channel::unbounded::<SessionCommand>();
    let dir = tempfile::tempdir().unwrap();
    let db = Arc::new(redb::Database::create(dir.path().join("test.redb")).unwrap());
    let ctx = RequestContext {
        cmd_tx,
        session_id: 1,
        db: Arc::clone(&db),
        tool_registry: registry.build().into_shared(),
        daemon_tx,
        max_turns: 0,
        lag_limits: crate::broadcast::LagLimits::default(),
        global_lag: Arc::new(std::sync::atomic::AtomicUsize::new(0)),
        substrate_credential: None,
        warm_policy: crate::cache_warm::WarmPolicy::default(),
    };
    AgentLoopHarness {
        ctx,
        session: SessionState::empty(),
        db,
        cmd_rx,
        _daemon_rx: daemon_rx,
        _dir: dir,
    }
}

/// Test tool that cancels the request the moment it runs, then blocks until the
/// test releases it. Sending the cancel from inside `execute_streaming` makes a
/// mid-batch cancel deterministic with no time-based wait: the tool blocks
/// before it can produce a result, so the collector's biased cancel arm always
/// wins and the loop unwinds through the cancelled-turn checkpoint path.
struct CancelThenBlockTool {
    cancel_tx: crossbeam_channel::Sender<()>,
    proceed: std::sync::Mutex<Option<mpsc::Receiver<()>>>,
}

impl Tool for CancelThenBlockTool {
    type Args = serde_json::Value;
    type Return = String;
    type Error = ToolExecError;

    fn name(&self) -> &'static str {
        "_test_cancel_block"
    }
    fn group(&self) -> &'static str {
        "test"
    }
    fn description(&self) -> &'static str {
        "cancels the request on run, then blocks until released"
    }
    fn describe_invocation(&self, _args: &Self::Args) -> String {
        self.description().to_string()
    }
    fn return_string(ret: &Self::Return) -> String {
        ret.clone()
    }
    fn schema(&self) -> serde_json::Value {
        serde_json::json!({})
    }
    fn execute(
        &self,
        _args: Self::Args,
        _xc: Option<&ServiceCredential>,
        _working_dir: Option<&Path>,
        _ctx: Option<&ToolContext>,
    ) -> Result<Self::Return, Self::Error> {
        Ok("ignored".into())
    }
    fn execute_streaming(
        &self,
        _args: Self::Args,
        _xc: Option<&ServiceCredential>,
        _working_dir: Option<&Path>,
        _output_tx: crossbeam_channel::Sender<Vec<u8>>,
        _ctx: Option<&ToolContext>,
    ) -> Result<Self::Return, Self::Error> {
        // Request cancellation first, then block: no result can be produced
        // before the cancel lands, so the collector deterministically unwinds
        // through the cancelled-turn checkpoint instead of a tool result.
        let _ = self.cancel_tx.send(());
        if let Some(rx) = self.proceed.lock().unwrap().take() {
            let _ = rx.recv();
        }
        Ok("released".into())
    }
}

/// Drain every `SessionStatus` the loop broadcast on `cmd_rx`, in order, so a
/// test can assert the status-phase progression one request produced.
fn collect_status_broadcasts(
    cmd_rx: &crossbeam_channel::Receiver<SessionCommand>,
) -> Vec<SessionStatus> {
    let mut statuses = Vec::new();
    while let Ok(cmd) = cmd_rx.try_recv() {
        if let SessionCommand::StatusChanged(status) = cmd {
            statuses.push(status);
        }
    }
    statuses
}

struct FastTestTool;

impl Tool for FastTestTool {
    type Args = serde_json::Value;
    type Return = String;
    type Error = ToolExecError;

    fn name(&self) -> &'static str {
        "_test_fast"
    }
    fn group(&self) -> &'static str {
        "test"
    }
    fn description(&self) -> &'static str {
        "test tool that completes immediately"
    }
    fn describe_invocation(&self, _args: &Self::Args) -> String {
        format!("{}.", self.description())
    }
    fn return_string(ret: &Self::Return) -> String {
        ret.clone()
    }
    fn schema(&self) -> serde_json::Value {
        serde_json::json!({})
    }
    fn execute(
        &self,
        _args: Self::Args,
        _xc: Option<&ServiceCredential>,
        _working_dir: Option<&Path>,
        _ctx: Option<&ToolContext>,
    ) -> Result<Self::Return, Self::Error> {
        Ok("fast result".into())
    }
}

struct BlockingTestTool {
    proceed: std::sync::Mutex<Option<mpsc::Receiver<()>>>,
}

impl Tool for BlockingTestTool {
    type Args = serde_json::Value;
    type Return = String;
    type Error = ToolExecError;

    fn name(&self) -> &'static str {
        "_test_blocking"
    }
    fn group(&self) -> &'static str {
        "test"
    }
    fn description(&self) -> &'static str {
        "test tool that blocks until proceed"
    }
    fn describe_invocation(&self, _args: &Self::Args) -> String {
        format!("{}.", self.description())
    }
    fn return_string(ret: &Self::Return) -> String {
        ret.clone()
    }
    fn schema(&self) -> serde_json::Value {
        serde_json::json!({})
    }
    fn execute(
        &self,
        _args: Self::Args,
        _xc: Option<&ServiceCredential>,
        _working_dir: Option<&Path>,
        _ctx: Option<&ToolContext>,
    ) -> Result<Self::Return, Self::Error> {
        Ok("ignored".into())
    }
    fn execute_streaming(
        &self,
        _args: Self::Args,
        _xc: Option<&ServiceCredential>,
        _working_dir: Option<&Path>,
        _output_tx: crossbeam_channel::Sender<Vec<u8>>,
        _ctx: Option<&ToolContext>,
    ) -> Result<Self::Return, Self::Error> {
        if let Some(rx) = self.proceed.lock().unwrap().take() {
            let _ = rx.recv();
        }
        Ok("blocked tool done".into())
    }
}

fn run_exec_tool(
    tool: impl Tool + 'static,
    tool_name: &str,
    tool_args: &str,
    timeout_dur: Duration,
    cancel_rx: &crossbeam_channel::Receiver<()>,
) -> (
    ToolOutput,
    bool,
    crossbeam_channel::Receiver<SessionCommand>,
) {
    let (daemon_tx, _daemon_rx) = crossbeam_channel::unbounded::<DaemonCommand>();
    let (cmd_tx, cmd_rx) = crossbeam_channel::unbounded::<SessionCommand>();

    let dir = tempfile::tempdir().expect("tempdir");
    let db = redb::Database::create(dir.path().join("test.redb")).expect("Database");

    let mut session = SessionState::empty();

    let mut registry = ToolRegistry::new();
    registry.register(tool);
    let registry = registry.build().into_shared();

    let tool_call = ChatToolCall {
        id: "call_test".into(),
        name: tool_name.into(),
        arguments_json: tool_args.into(),
        caller: None,
    };

    let ctx = RequestContext {
        cmd_tx,
        session_id: 1,
        db: Arc::new(db),
        tool_registry: registry,
        daemon_tx,
        max_turns: 0,
        lag_limits: crate::broadcast::LagLimits::default(),
        global_lag: Arc::new(std::sync::atomic::AtomicUsize::new(0)),
        substrate_credential: None,
        warm_policy: crate::cache_warm::WarmPolicy::default(),
    };
    let (result, cancelled, _image) = execute_tool_with_timeout(ExecuteToolParams {
        tool_call: &tool_call,
        x_credentials: None,
        working_dir: None,
        timeout_dur,
        stream_id: 1,
        session_id: 1,
        session: &mut session,
        session_tools: Arc::new(crate::mcp::ProjectToolSet::empty()),
        cancel_rx,
        ctx: &ctx,
        invocation_description: "test invocation",
    });
    (result, cancelled, cmd_rx)
}

struct StreamingTestTool;

impl Tool for StreamingTestTool {
    type Args = serde_json::Value;
    type Return = String;
    type Error = ToolExecError;

    fn name(&self) -> &'static str {
        "_test_streaming"
    }
    fn group(&self) -> &'static str {
        "test"
    }
    fn description(&self) -> &'static str {
        "test tool that sends streaming output"
    }
    fn describe_invocation(&self, _args: &Self::Args) -> String {
        format!("{}.", self.description())
    }
    fn return_string(ret: &Self::Return) -> String {
        ret.clone()
    }
    fn schema(&self) -> serde_json::Value {
        serde_json::json!({})
    }
    fn execute(
        &self,
        _args: Self::Args,
        _xc: Option<&ServiceCredential>,
        _working_dir: Option<&Path>,
        _ctx: Option<&ToolContext>,
    ) -> Result<Self::Return, Self::Error> {
        Ok("exec result".into())
    }
    fn supports_streaming_output() -> bool {
        true
    }

    fn execute_streaming(
        &self,
        _args: Self::Args,
        _xc: Option<&ServiceCredential>,
        _working_dir: Option<&Path>,
        output_tx: crossbeam_channel::Sender<Vec<u8>>,
        _ctx: Option<&ToolContext>,
    ) -> Result<Self::Return, Self::Error> {
        let _ = output_tx.send(b"streamed payload".to_vec());
        Ok("streaming done".into())
    }
}

/// Build a throwaway `ToolContext` and command channel for
/// `spawn_single_tool` tests. Receivers are dropped, which is fine — no
/// assertion inspects the daemon or session command streams here.
fn spawn_test_ctx() -> (ToolContext, crossbeam_channel::Sender<SessionCommand>) {
    let (cmd_tx, _cmd_rx) = crossbeam_channel::unbounded::<SessionCommand>();
    // Receiver is intentionally dropped immediately (see doc comment above);
    // the sender is moved into the ToolContext, so no underscore binding is
    // ever referenced.
    let (daemon_tx, daemon_rx) = crossbeam_channel::unbounded::<DaemonCommand>();
    drop(daemon_rx);
    let dir = tempfile::tempdir().expect("tempdir");
    let db = Arc::new(redb::Database::create(dir.path().join("test.redb")).expect("Database"));
    let ctx = ToolContext {
        session_id: 1,
        db,
        daemon_tx,
        active_tool_groups: std::collections::HashSet::new(),
        reasoning_effort: None,
        selected_model: None,
        working_dir: None,
        cancelled: Arc::new(AtomicBool::new(false)),
        account_name: None,
        discovered_skills: None,
    };
    (ctx, cmd_tx)
}

fn run_spawn_single_tool(
    tool: impl Tool + 'static,
    tool_name: &str,
    tool_args: &str,
    timeout: Option<Duration>,
) -> ToolHandle {
    let (ctx, cmd_tx) = spawn_test_ctx();

    let mut registry = ToolRegistry::new();
    registry.register(tool);
    let registry = registry.build();

    let tool_call = ChatToolCall {
        id: "call_test".into(),
        name: tool_name.into(),
        arguments_json: tool_args.into(),
        caller: None,
    };

    let invocation_description = registry
        .describe_invocation_for(&tool_call.name, &tool_call.arguments_json)
        .unwrap_or_default();

    let (result_tx, result_rx) = crossbeam_channel::unbounded::<ToolHandle>();
    // Hold the kill sender for the duration of this wait — dropping it
    // would disconnect the kill channel and stop the wait-loop early.
    let _kill_tx = spawn_single_tool(SpawnToolArgs {
        tool_call,
        timeout,
        stream_id: 1,
        session_id: 1,
        registry,
        session_tools: Arc::new(crate::mcp::ProjectToolSet::empty()),
        cmd_tx,
        x_credentials: None,
        working_dir: None,
        ctx,
        invocation_description,
        started_at: Instant::now(),
        result_tx,
    });

    result_rx.recv().expect("tool did not deliver a result")
}

fn setup_build_system_content_session() -> (SessionState, Arc<ToolRegistry>, tempfile::TempDir) {
    let dir = tempfile::tempdir().unwrap();
    std::fs::write(dir.path().join("AGENTS.md"), "Project rules").unwrap();

    let mut registry = ToolRegistry::new();
    registry.register(FastTestTool);
    let registry = registry.build();

    let mut session = SessionState::empty();
    session.config.working_dir = Some(dir.path().to_path_buf());
    (session, registry, dir)
}

/// Call `build_system_content` with standard defaults derived from the
/// session state and optional `pending_hints` overrides.
fn test_build_content(
    session: &mut SessionState,
    registry: &ToolRegistry,
    pending_hints: &[String],
) -> String {
    build_system_content(
        &SystemContentParams {
            working_dir: session.config.working_dir.as_deref(),
            context_config: &session.config.context_config,
            skills: &[],
            loaded_skill_bodies: &session.loaded_skill_bodies,
            tool_registry: registry,
            pending_hints,
            session_title: session.config.title.as_deref(),
        },
        &mut session.context_cache,
    )
}

mod agent_loop;
mod config_broadcast;
mod request_builder;
mod skill_persist;
mod system_content;
mod token_estimate;
mod tool_dispatch;
