//! `session_inspect` — read-only diagnostic tool (group: `debug`).
//!
//! Reproduces the exact request the daemon would build for a session and
//! reports, per assistant turn, whether the provider's reasoning wire field
//! (`reasoning_content` for DeepSeek/Kimi chat) would be echoed back — and
//! which turns would be sent bare, risking a provider rejection like
//! `DeepSeek`'s "The `reasoning_content` in the thinking mode must be passed
//! back to the API".
//!
//! Everything is computed from the persisted session record + turns using the
//! SAME code paths the agent loop uses (`build_chat_request_messages`,
//! `chat_reasoning_echo`, `warn_on_uncaptured_reasoning`,
//! `model_reasoning_passback`), so the report is a faithful dry-run rather than
//! a reimplementation that can drift. The messages are lowered by the chat
//! adapter's own `chat_wire_messages`, so the per-message keys in this report
//! are exactly what the upstream would see. Read-only: only redb read
//! transactions are opened; session state is never mutated.
//!
//! Privacy mirrors the codebase's thinking-content invariant: artifact
//! *metadata* (variant, wire field, byte size) and producer identity are
//! reported for any session, but message-text previews and raw reasoning
//! bytes are only rendered for the calling session (thinking blocks and
//! encrypted signatures otherwise never leave the daemon — same rule as
//! `turn_for_client`), and raw reasoning additionally requires `include_raw`.

use super::ToolExecError;
use crate::daemon::DaemonCommand;
use crate::db::{read_session, read_turns};
use crate::reasoning::{
    build_chat_request_messages, include_reasoning_artifact, warn_on_uncaptured_reasoning,
};
use crate::sessions::SessionState;
use crate::tools::Tool;
use crate::tools::context::ToolContext;
use choreo_ai_protocols::openai::chat_wire_messages;
use choreo_ai_protocols::{
    ReasoningPassback, chat_reasoning_echo, model_reasoning_passback, provider_slug_for_model,
};
use choreo_proto::{ChatReasoningField, ReasoningArtifact, ReasoningProducer, Turn};
use schemars::JsonSchema;
use serde::Deserialize;
use std::collections::BTreeMap;
use std::fmt::Write as _;
use tracing::info;

/// Default cap on the per-turn ledger rows (computation always covers all
/// turns; only the rendered table is capped so a huge session stays a
/// compact report).
const DEFAULT_MAX_TURNS: usize = 512;
/// Truncation width for message-text previews (own session only).
const PREVIEW_LEN: usize = 120;
/// Truncation width for raw reasoning snippets (`include_raw`, own session).
const RAW_LEN: usize = 400;

#[derive(Debug, Deserialize, JsonSchema)]
pub struct SessionInspectArgs {
    /// Session ID to inspect. Defaults to the calling session.
    #[serde(default)]
    pub session_id: Option<u64>,
    /// Provider slug used to resolve the reasoning-passback policy and the
    /// same-model provenance check. Defaults to the most common slug recorded
    /// on the session's own turns, then to the catalog's model→provider
    /// mapping.
    #[serde(default)]
    pub provider: Option<String>,
    /// Model used to resolve the passback policy and the provenance check.
    /// Defaults to the session's selected model.
    #[serde(default)]
    pub model: Option<String>,
    /// Include truncated raw reasoning text per assistant turn. Honored only
    /// for the calling session — thinking content is sensitive and otherwise
    /// never leaves the daemon process.
    #[serde(default)]
    pub include_raw: bool,
    /// Cap the per-turn ledger to this many turns (default 512).
    #[serde(default)]
    pub max_turns: Option<usize>,
}

pub(crate) struct SessionInspect;

impl Tool for SessionInspect {
    type Args = SessionInspectArgs;
    type Return = String;
    type Error = ToolExecError;

    fn name(&self) -> &'static str {
        "session_inspect"
    }

    fn group(&self) -> &'static str {
        "debug"
    }

    fn description(&self) -> &'static str {
        "Read-only reasoning-echo diagnostic: dry-runs the request the daemon \
         would build for a session and reports which assistant turns carry the \
         provider's reasoning field on the wire and which would be sent bare \
         (the DeepSeek/Kimi 400 \"reasoning_content must be passed back\" risk)."
    }

    fn describe_invocation(&self, args: &Self::Args) -> String {
        match args.session_id {
            Some(id) => format!("Inspecting session {id} reasoning-echo status."),
            None => "Inspecting current session reasoning-echo status.".into(),
        }
    }

    fn return_string(ret: &Self::Return) -> String {
        ret.clone()
    }

    fn execute(
        &self,
        args: Self::Args,
        _x_credentials: Option<&crate::tools::ServiceCredential>,
        _working_dir: Option<&std::path::Path>,
        ctx: Option<&ToolContext>,
    ) -> Result<Self::Return, Self::Error> {
        let ctx =
            ctx.ok_or_else(|| ToolExecError("session_inspect requires a session context".into()))?;
        let session_id = args.session_id.unwrap_or(ctx.session_id);
        let report = build_report(ctx, session_id, &args)?;
        info!(
            session_id,
            report_bytes = report.len(),
            "session_inspect completed"
        );
        Ok(report)
    }
}

/// Build the full diagnostic report for `session_id`.
///
/// The (provider, model) pair is resolved "explicit arg wins, then recorded
/// session state, then the catalog's model→provider mapping" so the report is
/// reproducible with a known policy even when the session's own data is
/// ambiguous; the report states which source won so a wrong-looking passback
/// is immediately attributable.
fn build_report(
    ctx: &ToolContext,
    session_id: u64,
    args: &SessionInspectArgs,
) -> Result<String, ToolExecError> {
    let record = read_session(&ctx.db, session_id)
        .map_err(|e| ToolExecError(format!("read session {session_id}: {e}")))?;
    let Some(record) = record else {
        return Err(ToolExecError(format!("session {session_id} not found")));
    };
    let turns_raw = read_turns(&ctx.db, session_id)
        .map_err(|e| ToolExecError(format!("read turns for session {session_id}: {e}")))?;
    let turns: BTreeMap<u32, Turn> = turns_raw.into_iter().collect();

    // The most common producer recorded on the session's own turns is the
    // best recorded guess for the live (provider, model) pair; it also makes
    // the provenance check exact for a session whose provider slug is not
    // "deepseek" (e.g. an opencode-style gateway account).
    let dominant_producer = dominant_producer(&turns);

    // Model first; the provider lookup needs it.
    let (model, model_source) = match args.model.clone() {
        Some(m) => (m, "arg".to_string()),
        None => match record.selected_model.clone() {
            Some(m) => (m, "record".to_string()),
            None => match dominant_producer.as_ref().map(|p| p.model.clone()) {
                Some(m) => (m, "dominant-producer".to_string()),
                None => (String::new(), "unresolved".to_string()),
            },
        },
    };
    let (provider, provider_source) = match args.provider.clone() {
        Some(p) => (p, "arg".to_string()),
        None => match dominant_producer.as_ref().map(|p| p.provider_slug.clone()) {
            Some(slug) if !slug.is_empty() => (slug, "dominant-producer".to_string()),
            _ => match provider_slug_for_model(&model) {
                Some(slug) => (slug, "catalog".to_string()),
                // The model is unknown to the catalog; report it as
                // unresolvable rather than guessing a slug that would feed a
                // wrong passback/reasoning_content policy.
                None => ("unknown".to_string(), "unresolved".to_string()),
            },
        },
    };
    if model.is_empty() {
        return Err(ToolExecError(
            "no model resolved: pass `model` or set the session's selected model".into(),
        ));
    }

    let passback = model_reasoning_passback(&provider, &model);
    // The route's chat reasoning-echo policy: the declared field outranks the
    // field a payload was observed under, and a declared field must be present
    // on every assistant message (empty when there is nothing to echo).
    let echo = chat_reasoning_echo(&provider, &model);

    // Reconstruct the state the builder reads. `build_chat_request_messages`
    // only consults `turns` (and the optional system prompt, which we pass as
    // None); setting `selected_model` keeps the reconstruction honest for any
    // future provenance reads that consult config.
    let mut state = SessionState::empty();
    state.turns = turns;
    state.config.selected_model = Some(model.clone());

    let guard_problems = warn_on_uncaptured_reasoning(&state, session_id, &provider, &model);
    // The dry-run has no request in flight, so the builder's image-decay
    // marker is `None`: every historical tool-result image renders as a text
    // placeholder here, exactly as it would on the request a freshly-loaded
    // session would build (only a live agent loop's own turns carry pixels).
    let messages = build_chat_request_messages(&state, None, &provider, &model, None);

    // Wire accounting: lower the messages with the chat adapter's own policy
    // application, so this dry-run cannot invent a different key than the real
    // request — and cannot count a message the lowering drops. The counts are
    // what the upstream would actually see.
    let wire = chat_wire_messages(&messages, echo);
    let dropped = wire.dropped;
    let mut assistant_count = 0usize;
    let mut wire_reasoning_count = 0usize;
    let mut wire_required_missing = 0usize;
    let mut wire_empty: Vec<String> = Vec::new();
    for (msg_index, w) in wire.messages.iter().enumerate() {
        if w.role() != "assistant" {
            continue;
        }
        assistant_count += 1;
        let value = serde_json::to_value(w)
            .map_err(|e| ToolExecError(format!("serialize request message: {e}")))?;
        let keys = [
            "reasoning_content",
            "reasoning",
            "reasoning_text",
            "reasoning_details",
        ];
        let present: Vec<&str> = keys
            .iter()
            .copied()
            .filter(|k| value.get(*k).is_some())
            .collect();
        if !present.is_empty() {
            wire_reasoning_count += 1;
        }
        // The declared field must be on EVERY assistant message of a route that
        // declares one — its absence is the 400 shape this tool exists to catch.
        let declared_present = echo
            .field
            .map(fmt_field)
            .is_some_and(|k| present.contains(&k));
        if echo.require_present && !declared_present {
            wire_required_missing += 1;
        }
        let has_tools = value
            .get("tool_calls")
            .and_then(|tc| tc.as_array())
            .is_some_and(|arr| !arr.is_empty());
        // Empty-message alarm: the lowering drops these, so a survivor with no
        // content, no tool calls, and no non-empty reasoning echo means the drop
        // predicate and the serialization have drifted apart. A non-empty
        // `reasoning_details` array is a payload; the injected `[]` is not.
        let content_empty = value
            .get("content")
            .and_then(|c| c.as_str())
            .is_none_or(|text| text.trim().is_empty());
        let reasoning_non_empty = keys
            .iter()
            .filter_map(|k| value.get(*k).and_then(|v| v.as_str()))
            .any(|s| !s.trim().is_empty())
            || value
                .get("reasoning_details")
                .and_then(|details| details.as_array())
                .is_some_and(|entries| !entries.is_empty());
        if content_empty && !has_tools && !reasoning_non_empty {
            wire_empty.push(format!(
                "  position {msg_index}: empty assistant message survived the projection"
            ));
        }
    }

    // Per-turn ledger, computed with the same gates as the builder so the
    // report can attribute a reason to each turn. The wire cross-check below
    // validates that the two views agree — a MISMATCH means the ledger logic
    // and the real lowering have drifted, which is itself a diagnostic signal.
    let mut ledger = Vec::new();
    let mut capture_losses: Vec<String> = Vec::new();
    let mut infos: Vec<String> = Vec::new();
    let mut raw_lines: Vec<String> = Vec::new();
    let mut ledger_key_count = 0usize;
    let own_session = session_id == ctx.session_id;
    let cap = args.max_turns.unwrap_or(DEFAULT_MAX_TURNS);

    for (idx, (turn_id, turn)) in state.turns.iter().enumerate() {
        let has_assistant_msg = turn.assistant_text.is_some() || !turn.tool_calls.is_empty();
        // A turn with no user text and no assistant contribution emits no
        // messages on the wire; skip it (nothing to echo or omit).
        if turn.user_text.is_none() && !has_assistant_msg {
            continue;
        }

        let producer = turn.reasoning_producer.as_ref();
        let same_model = producer.map(|p| (p.provider_slug.as_str(), p.model.as_str()))
            == Some((provider.as_str(), model.as_str()));
        let tool_involvement = !turn.tool_calls.is_empty() || !turn.tool_results.is_empty();
        // Same helper the builder uses — the empty-message fallback (a
        // content-less, tool-less turn under an echo-capable passback echoes
        // its same-model artifact so the wire message is never empty) is
        // applied here automatically, keeping the ledger and the wire in
        // lockstep.
        let include_artifact = include_reasoning_artifact(turn, &provider, &model, passback);
        let echo_has_artifact = include_artifact && turn.reasoning_artifact.is_some();
        // Whether the turn's assistant message survives the lowering at all: the
        // projection drops an assistant message with no non-blank content and no
        // tool calls (whitespace-only counts as empty). Mirrors
        // `ChatWireMessage::is_empty`, so the ledger and the wire agree.
        let survives = turn
            .assistant_text
            .as_deref()
            .is_some_and(|text| !text.trim().is_empty())
            || !turn.tool_calls.is_empty();
        // Whether this turn's assistant message carries A reasoning key on the
        // wire: the artifact (under the declared field, else the observed one),
        // or the required field as an empty string when there is nothing to
        // echo. A required-but-empty field is not a payload, so it cannot keep a
        // message the projection drops. Mirrors `ChatWireMessage` exactly.
        let wire_present = !turn.undone
            && has_assistant_msg
            && (echo_has_artifact || (echo.require_present && survives));

        let echo_label = if !has_assistant_msg {
            "n/a".to_string()
        } else if echo_has_artifact {
            match echo.field {
                // The structured array is its own destination; name the shape
                // rather than a scalar wire key.
                Some(ChatReasoningField::ReasoningDetails) => "yes(details)".to_string(),
                // The declared field wins over the observation, so say which
                // one the wire will actually carry.
                Some(field) => format!("yes(declared:{})", fmt_field(field)),
                None => "yes(observed)".to_string(),
            }
        } else if wire_present {
            match echo.field {
                Some(ChatReasoningField::ReasoningDetails) => "yes(details:empty)".to_string(),
                field => format!(
                    "yes(empty:{})",
                    fmt_field(field.unwrap_or(ChatReasoningField::ReasoningContent))
                ),
            }
        } else if !same_model {
            "no:producer-mismatch".to_string()
        } else if turn.reasoning_artifact.is_none() {
            if turn.assistant_reasoning.is_some() {
                "no:reasoning-without-artifact".to_string()
            } else {
                "no:no-artifact".to_string()
            }
        } else {
            match passback {
                ReasoningPassback::None => "no:passback-none".to_string(),
                ReasoningPassback::ResponseId => "no:passback-response-id".to_string(),
                ReasoningPassback::ToolLoop if !tool_involvement => "no:tool-loop-skip".to_string(),
                // AllTurns/Signature with a matching artifact should have
                // echoed; anything reaching here is a policy/code drift.
                _ => "no:policy-drift".to_string(),
            }
        };

        let art = turn
            .reasoning_artifact
            .as_ref()
            .map_or_else(|| "-".into(), fmt_artifact);
        // The field the payload was observed under, shown beside the artifact so
        // a provider that streams one field while declaring another is visible
        // at a glance.
        let observed = turn.reasoning_artifact.as_ref().map_or_else(
            || "-".to_string(),
            |artifact| match artifact {
                ReasoningArtifact::ChatReasoning { field, .. } => fmt_field(*field).to_string(),
                _ => "-".to_string(),
            },
        );
        let prod = producer.map_or_else(
            || "-".into(),
            |p| format!("{}/{}", p.provider_slug, p.model),
        );
        let same = if same_model { "y" } else { "n" };
        let user_len = turn.user_text.as_deref().map_or(0, str::len);
        let asst_len = turn.assistant_text.as_deref().map_or(0, str::len);
        let flags = format!(
            "{}{}",
            if turn.undone { "U" } else { "-" },
            if turn.error.is_some() { "E" } else { "-" },
        );

        if idx < cap {
            ledger.push(format!(
                "  t{turn_id:>5} [{flags}] u={user_len} a={asst_len} tc={} r={} art={art} obs={observed} prod={prod} same={same} echo={echo_label}",
                turn.tool_calls.len(),
                turn.tool_results.len(),
            ));
        }

        // Wire-parity counter (vs. the lowered-message dry-run above): the
        // builder skips `undone` turns entirely, so only non-undone turns
        // produce a message on the wire and count toward the parity check.
        if wire_present {
            ledger_key_count += 1;
        }

        // Capture loss: the model reasoned, the session displays it, but no
        // artifact exists to replay — the echo degrades to an empty field. Not
        // a request hazard (the declared field is still present), so it is a
        // note rather than a risk.
        if has_assistant_msg
            && turn.reasoning_artifact.is_none()
            && turn.assistant_reasoning.is_some()
        {
            capture_losses.push(format!(
                "  t{turn_id}: reasoning displayed but not captured (echo degrades to an empty field) user: {}",
                own_preview(turn.user_text.as_deref(), own_session),
            ));
        }
        // Under ToolLoop, a plain-text assistant turn that produced reasoning
        // is deliberately sent bare — informational, matches the policy (and
        // suppressed on models that declare a required field).
        if has_assistant_msg
            && turn.tool_calls.is_empty()
            && !wire_present
            && turn.reasoning_artifact.is_some()
            && same_model
            && matches!(passback, ReasoningPassback::ToolLoop)
        {
            infos.push(format!(
                "  t{turn_id}: short answer with reasoning artifact — ToolLoop echoes only \
                 tool turns, so this turn is sent bare (harmless unless the provider \
                 requires an all-turns echo)"
            ));
        }

        // Raw reasoning snippets: own session only, and only when explicitly
        // requested — thinking text never leaves the daemon otherwise.
        if own_session
            && args.include_raw
            && let Some(ReasoningArtifact::ChatReasoning { field, bytes }) =
                turn.reasoning_artifact.as_ref()
        {
            let text = String::from_utf8_lossy(bytes);
            raw_lines.push(format!(
                "  t{turn_id} [{}]: {}",
                fmt_field(*field),
                truncate(&text, RAW_LEN)
            ));
        }
    }

    let mismatch = if ledger_key_count == wire_reasoning_count {
        "OK"
    } else {
        "MISMATCH"
    };

    let mut out = String::new();
    let _ = writeln!(out, "session_inspect: session {session_id}");
    let _ = writeln!(out, "  model={model} (from: {model_source})");
    let _ = writeln!(
        out,
        "  provider={provider} (from: {provider_source}) passback={passback:?}"
    );
    let _ = writeln!(
        out,
        "  record: title={} working_dir={} account={} groups={:?}",
        record.title.as_deref().unwrap_or("-"),
        record.working_dir.as_deref().unwrap_or("-"),
        record.account_name.as_deref().unwrap_or("-"),
        record.active_tool_groups
    );
    let _ = writeln!(
        out,
        "  turns={} assistant_messages_on_wire={} dropped={} messages_on_wire={}",
        state.turns.len(),
        assistant_count,
        dropped,
        wire.messages.len()
    );
    out.push_str(&mcp_section(ctx));
    out.push('\n');
    let _ = writeln!(
        out,
        "  daemon guard warn_on_uncaptured_reasoning: {guard_problems} problem(s)"
    );
    let _ = writeln!(
        out,
        "  route echo policy: field={} require_present={}",
        echo.field
            .map_or("none".to_string(), |f| fmt_field(f).to_string()),
        echo.require_present
    );
    let _ = writeln!(
        out,
        "  wire dry-run: {wire_reasoning_count}/{assistant_count} assistant messages carry a reasoning field; ledger vs wire: {mismatch}"
    );
    if echo.require_present {
        let _ = writeln!(
            out,
            "  declared-field violations: {wire_required_missing} assistant message(s) omit {} (must be 0)",
            fmt_field(echo.field.unwrap_or(ChatReasoningField::ReasoningContent))
        );
    }
    if dropped > 0 {
        let _ = writeln!(
            out,
            "  projection dropped {dropped} assistant message(s) that would have carried nothing (the model does not see them)"
        );
    }
    if wire_empty.is_empty() {
        out.push_str("  empty assistant message(s) on the wire: none\n");
    } else {
        let _ = write!(
            out,
            "  empty assistant message(s) on the wire (the drop predicate and the serializer have drifted):\n{}\n",
            wire_empty.join("\n")
        );
    }
    out.push('\n');

    // The invariant this tool exists to verify is now structural (the lowering
    // enforces it), so a violation is a regression alarm rather than a
    // classification to interpret.
    if wire_required_missing == 0 {
        out.push_str("RISK (assistant messages missing the route's declared field): none\n");
    } else {
        let _ = writeln!(
            out,
            "RISK (assistant messages missing the route's declared field — a lowering regression, expect a provider 400): {wire_required_missing}"
        );
    }
    if !capture_losses.is_empty() {
        let _ = write!(
            out,
            "NOTE (reasoning displayed but not captured — the echo degrades to an empty field):\n{}\n",
            capture_losses.join("\n")
        );
    }
    if !infos.is_empty() {
        let _ = write!(
            out,
            "INFO (policy-skips — expected under this passback policy):\n{}\n",
            infos.join("\n")
        );
    }
    if !raw_lines.is_empty() {
        let _ = write!(
            out,
            "RAW reasoning (include_raw, own session):\n{}\n",
            raw_lines.join("\n")
        );
    }
    let _ = write!(
        out,
        "\nper-turn ledger{}:\n{}\n",
        if state.turns.len() > cap {
            format!(" (first {cap} of {})", state.turns.len())
        } else {
            String::new()
        },
        ledger.join("\n")
    );
    Ok(out)
}

/// The `session_inspect` MCP section: every configured MCP server's state and
/// tool count (never a secret).
///
/// Queried live from the daemon command loop (the owner of the `McpManager`)
/// over the session's `daemon_tx`, with a short timeout so an unresponsive
/// daemon cannot hang the diagnostic instead of reporting it.
fn mcp_section(ctx: &ToolContext) -> String {
    let (reply_tx, reply_rx) = std::sync::mpsc::channel();
    if ctx
        .daemon_tx
        .send(DaemonCommand::McpStatus {
            // No session scope: report only the daemon tier (this diagnostic
            // runs from a tool thread that may not be the attached session).
            session_id: None,
            reply: reply_tx,
        })
        .is_err()
    {
        return "  MCP: daemon command loop unavailable\n".to_string();
    }
    let Ok(report) = reply_rx.recv_timeout(std::time::Duration::from_secs(2)) else {
        return "  MCP: status query timed out\n".to_string();
    };
    if report.servers.is_empty() {
        return "  MCP: no servers configured\n".to_string();
    }
    let mut out = format!("  MCP servers ({}):\n", report.servers.len());
    for status in report.servers {
        let _ = writeln!(out, "    [{}] {}", status.tier, status.summary());
    }
    out
}

/// Count occurrences of each distinct producer across the session's turns;
/// the most common one is the best recorded guess for the current
/// (provider, model) pair when the caller does not override it.
fn dominant_producer(turns: &BTreeMap<u32, Turn>) -> Option<ReasoningProducer> {
    let mut counts: BTreeMap<(String, String), usize> = BTreeMap::new();
    for turn in turns.values() {
        if let Some(p) = &turn.reasoning_producer {
            *counts
                .entry((p.provider_slug.clone(), p.model.clone()))
                .or_default() += 1;
        }
    }
    counts
        .into_iter()
        .max_by_key(|(_, count)| *count)
        .map(|((provider_slug, model), _)| ReasoningProducer {
            provider_slug,
            model,
        })
}

/// Compact artifact descriptor for the ledger: variant tag + wire field (for
/// chat) + byte size. Never includes the payload bytes themselves. A structured
/// `reasoning_details` payload also reports its entry count, since the bytes are
/// a JSON array rather than display text.
fn fmt_artifact(artifact: &ReasoningArtifact) -> String {
    match artifact {
        ReasoningArtifact::ChatReasoning {
            field: ChatReasoningField::ReasoningDetails,
            bytes,
        } => {
            let entries = serde_json::from_slice::<serde_json::Value>(bytes)
                .ok()
                .and_then(|value| value.as_array().map(Vec::len))
                .unwrap_or(0);
            format!("chat[reasoning_details;{entries} entries;{}B]", bytes.len())
        }
        ReasoningArtifact::ChatReasoning { field, bytes } => {
            format!("chat[{};{}B]", fmt_field(*field), bytes.len())
        }
        ReasoningArtifact::AnthropicThinking(bytes) => format!("anthropic[{}B]", bytes.len()),
        ReasoningArtifact::GoogleSignatures(bytes) => format!("google-sig[{}B]", bytes.len()),
        ReasoningArtifact::ResponsesItems(bytes) => format!("responses[{}B]", bytes.len()),
    }
}

fn fmt_field(field: ChatReasoningField) -> &'static str {
    match field {
        ChatReasoningField::ReasoningContent => "reasoning_content",
        ChatReasoningField::Reasoning => "reasoning",
        ChatReasoningField::ReasoningText => "reasoning_text",
        ChatReasoningField::ReasoningDetails => "reasoning_details",
    }
}

/// Message-text previews are scope-gated: empty unless the report is for the
/// calling session (thinking/context privacy mirrors `turn_for_client`).
fn own_preview(text: Option<&str>, own_session: bool) -> String {
    match (text, own_session) {
        (Some(t), true) => truncate(t, PREVIEW_LEN),
        _ => "-".into(),
    }
}

fn truncate(text: &str, max: usize) -> String {
    if text.chars().count() <= max {
        text.to_string()
    } else {
        let cut: String = text.chars().take(max).collect();
        format!("{cut}…")
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::db::{SessionRecord, write_session, write_turn};
    use crate::tools::context::ToolContext;
    use choreo_proto::{AssistantToolCallRecord, ReasoningProducer, TimestampMs, Turn};
    use std::sync::Arc;

    // Helper: seed a minimal session record + turns in a temp db and return a
    // ToolContext for the given "owning" session (the TempDir stays alive for
    // the duration of the test via the returned guard).
    fn seed(
        owner: u64,
        target: u64,
        selected_model: &str,
        turns: Vec<(u32, Turn)>,
    ) -> (tempfile::TempDir, ToolContext) {
        let dir = tempfile::tempdir().unwrap();
        let db = Arc::new(redb::Database::create(dir.path().join("test.redb")).unwrap());
        let now = TimestampMs::now().as_millis();
        let record = SessionRecord {
            title: Some("t".into()),
            selected_model: Some(selected_model.into()),
            parent_session_id: None,
            working_dir: None,
            // usize→u32 turn count: test fixtures carry a handful of turns.
            turn_count: {
                let n = turns.len();
                #[expect(clippy::cast_possible_truncation)]
                {
                    n as u32
                }
            },
            created_at: now,
            last_modified: now,
            active_tool_groups: vec!["core".into()],
            context_config: choreo_proto::ContextConfig::default(),
            account_name: None,
            reasoning_effort: None,
            last_response_id: None,
            last_response_id_producer: None,
            pinned: false,
            archived_at: None,
        };
        write_session(&db, target, &record).unwrap();
        for (tid, t) in turns {
            write_turn(&db, target, tid, &t).unwrap();
        }
        let (daemon_tx, _rx) = crossbeam_channel::unbounded();
        let ctx = ToolContext::new(owner, db, daemon_tx);
        (dir, ctx)
    }

    fn chat_artifact(text: &str) -> ReasoningArtifact {
        ReasoningArtifact::ChatReasoning {
            field: ChatReasoningField::ReasoningContent,
            bytes: text.as_bytes().to_vec(),
        }
    }

    fn details_artifact(entries: &serde_json::Value) -> ReasoningArtifact {
        ReasoningArtifact::ChatReasoning {
            field: ChatReasoningField::ReasoningDetails,
            bytes: serde_json::to_vec(entries).unwrap(),
        }
    }

    fn openrouter_producer(model: &str) -> ReasoningProducer {
        ReasoningProducer {
            provider_slug: "openrouter".into(),
            model: model.into(),
        }
    }

    fn tool_turn(
        call_id: &str,
        tool_name: &str,
        artifact: Option<ReasoningArtifact>,
        producer: Option<ReasoningProducer>,
        reasoning: Option<&str>,
    ) -> Turn {
        Turn {
            created_at: TimestampMs::now(),
            undone: false,
            error: None,
            user_text: Some("run it".into()),
            assistant_text: None,
            assistant_reasoning: reasoning.map(String::from),
            tool_calls: vec![AssistantToolCallRecord {
                call_id: call_id.into(),
                name: tool_name.into(),
                arguments_json: "{}".into(),
            }],
            token_usage: None,
            tool_results: Vec::new(),
            displayed_images: Vec::new(),
            reasoning_artifact: artifact,
            reasoning_producer: producer,
        }
    }

    fn text_turn(
        text: &str,
        artifact: Option<ReasoningArtifact>,
        producer: Option<ReasoningProducer>,
    ) -> Turn {
        Turn {
            created_at: TimestampMs::now(),
            undone: false,
            error: None,
            user_text: Some("hi".into()),
            assistant_text: Some(text.into()),
            assistant_reasoning: None,
            tool_calls: Vec::new(),
            token_usage: None,
            tool_results: Vec::new(),
            displayed_images: Vec::new(),
            reasoning_artifact: artifact,
            reasoning_producer: producer,
        }
    }

    fn ds_producer() -> ReasoningProducer {
        ReasoningProducer {
            provider_slug: "deepseek".into(),
            model: "deepseek-v4-flash".into(),
        }
    }

    /// A non-DeepSeek OpenAI-compat chat producer (`ToolLoop` passback but no
    /// `reasoning_content` injection) so the bare-turn classification paths
    /// stay exercisable independent of the `DeepSeek` injection behavior.
    fn groq_producer(model: &str) -> ReasoningProducer {
        ReasoningProducer {
            provider_slug: "groq".into(),
            model: model.into(),
        }
    }

    const GROQ_MODEL: &str = "groq/llama-3.3-70b-versatile";

    fn inspect_args(session: Option<u64>) -> SessionInspectArgs {
        SessionInspectArgs {
            session_id: session,
            provider: None,
            model: None,
            include_raw: false,
            max_turns: None,
        }
    }

    fn run(ctx: &ToolContext, args: SessionInspectArgs) -> String {
        SessionInspect.execute(args, None, None, Some(ctx)).unwrap()
    }

    #[test]
    fn details_artifact_renders_shape_and_details_echo() {
        // An OpenRouter route declares `reasoning_details`; the ledger must
        // report the structured artifact shape and the details echo, with no
        // declared-field violation and ledger/wire parity.
        let model = "minimax/minimax-m2";
        let (_dir, ctx) = seed(
            42,
            42,
            model,
            vec![(
                0,
                tool_turn(
                    "c1",
                    "exec",
                    Some(details_artifact(&serde_json::json!([
                        {"type":"reasoning.text","text":"think","index":0}
                    ]))),
                    Some(openrouter_producer(model)),
                    Some("think"),
                ),
            )],
        );
        let out = run(&ctx, inspect_args(None));
        assert!(out.contains("echo=yes(details)"), "{out}");
        assert!(
            out.contains("art=chat[reasoning_details;1 entries;"),
            "the structured shape (entry count + bytes) is rendered: {out}"
        );
        assert!(
            out.contains("declared-field violations: 0 assistant message(s) omit"),
            "{out}"
        );
        assert!(out.contains("ledger vs wire: OK"), "{out}");
    }

    #[test]
    fn details_route_without_artifact_reports_empty_details() {
        // A tool turn with nothing to echo on a details-declaring route: the
        // injected `reasoning_details: []` is reported as `yes(details:empty)`.
        let model = "minimax/minimax-m2";
        let (_dir, ctx) = seed(
            42,
            42,
            model,
            vec![(
                0,
                tool_turn("c1", "exec", None, Some(openrouter_producer(model)), None),
            )],
        );
        let out = run(&ctx, inspect_args(None));
        assert!(out.contains("echo=yes(details:empty)"), "{out}");
        assert!(
            out.contains("declared-field violations: 0 assistant message(s) omit"),
            "{out}"
        );
        assert!(out.contains("ledger vs wire: OK"), "{out}");
    }

    #[test]
    fn tool_turn_without_artifact_is_flagged_as_risk() {
        // Non-DeepSeek chat (no injection): t0 tool turn with artifact +
        // matching producer → echoed; t1 tool turn that reasoned (display
        // text) but captured no artifact → would be sent bare → the
        // DeepSeek-style 400 candidate.
        let (_dir, ctx) = seed(
            42,
            42,
            GROQ_MODEL,
            vec![
                (
                    0,
                    tool_turn(
                        "c1",
                        "grep",
                        Some(chat_artifact("think one")),
                        Some(groq_producer(GROQ_MODEL)),
                        Some("think one"),
                    ),
                ),
                (
                    1,
                    tool_turn(
                        "c2",
                        "exec",
                        None,
                        Some(groq_producer(GROQ_MODEL)),
                        Some("think two"),
                    ),
                ),
            ],
        );
        let out = run(&ctx, inspect_args(None));
        assert!(out.contains("t    0"), "t0 must appear in ledger: {out}");
        assert!(
            out.contains("echo=yes(observed)"),
            "t0 echoes under the observed field (groq declares none): {out}"
        );
        assert!(out.contains("t    1"), "t1 must appear: {out}");
        assert!(out.contains("no:reasoning-without-artifact"), "{out}");
        assert!(
            out.contains("NOTE (reasoning displayed but not captured"),
            "{out}"
        );
        // groq declares no reasoning field, so no assistant message can omit
        // one: the request-validity alarm stays quiet.
        assert!(
            out.contains("RISK (assistant messages missing the route's declared field): none"),
            "{out}"
        );
        assert!(out.contains("ledger vs wire: OK"), "{out}");
        // The daemon's own guard flags the capture loss.
        assert!(out.contains("1 problem(s)"), "{out}");
    }

    #[test]
    fn deepseek_tool_turn_without_any_reasoning_is_injected_empty() {
        // The opencode-go/deepseek-v4-flash 400 shape: a tool turn with no
        // reasoning at all. After the injection fix the builder still emits
        // `reasoning_content: ""`, so the report must show it as echoing an
        // empty value (not a bare tool turn / not a risk), and the wire
        // dry-run must report 0 bare tool-call messages.
        let (_dir, ctx) = seed(
            42,
            42,
            "deepseek-v4-flash",
            vec![(0, tool_turn("c1", "exec", None, Some(ds_producer()), None))],
        );
        let out = run(&ctx, inspect_args(None));
        assert!(out.contains("echo=yes(empty:reasoning_content)"), "{out}");
        assert!(
            out.contains("RISK (assistant messages missing the route's declared field): none"),
            "{out}"
        );
        assert!(
            !out.contains("NOTE (reasoning displayed but not captured"),
            "{out}"
        );
        assert!(
            out.contains("declared-field violations: 0 assistant message(s) omit"),
            "{out}"
        );
        assert!(
            out.contains("1/1 assistant messages carry a reasoning field"),
            "{out}"
        );
        assert!(out.contains("ledger vs wire: OK"), "{out}");
    }

    #[test]
    fn tool_turn_without_any_reasoning_still_counts_toward_wire_parity() {
        // A non-DeepSeek tool turn with neither an artifact nor displayed
        // reasoning is sent bare, so the ledger's tool-no-echo counter must
        // include it — otherwise the ledger-vs-wire cross-check reports
        // MISMATCH even though the wire dry-run is exact.
        let (_dir, ctx) = seed(
            42,
            42,
            GROQ_MODEL,
            vec![(
                0,
                tool_turn("c1", "exec", None, Some(groq_producer(GROQ_MODEL)), None),
            )],
        );
        let out = run(&ctx, inspect_args(None));
        assert!(out.contains("echo=no:no-artifact"), "{out}");
        // No displayed reasoning, so nothing was lost — just a bare tool turn
        // under a route that declares no field.
        assert!(
            !out.contains("NOTE (reasoning displayed but not captured"),
            "{out}"
        );
        // Both the ledger and the wire must agree that no reasoning key is
        // present → parity OK.
        assert!(out.contains("ledger vs wire: OK"), "{out}");
    }

    #[test]
    fn producer_mismatch_drops_echo_as_risk() {
        // Artifact produced by openai/gpt-4 while the session runs groq
        // (non-DeepSeek, no injection) → the provenance gate drops the echo
        // and the tool-call turn goes bare.
        let (_dir, ctx) = seed(
            42,
            42,
            GROQ_MODEL,
            vec![(
                0,
                tool_turn(
                    "c1",
                    "exec",
                    Some(chat_artifact("think one")),
                    Some(ReasoningProducer {
                        provider_slug: "openai".into(),
                        model: "gpt-4".into(),
                    }),
                    Some("think one"),
                ),
            )],
        );
        let out = run(&ctx, inspect_args(None));
        assert!(out.contains("no:producer-mismatch"), "{out}");
        // A model-bound artifact dropped by the provenance gate is not a
        // request hazard: the turn still has content and tool calls, and groq
        // declares no field to omit.
        assert!(
            out.contains("RISK (assistant messages missing the route's declared field): none"),
            "{out}"
        );
    }

    #[test]
    fn plain_text_turn_artifact_is_info_not_risk() {
        // A non-DeepSeek (no-injection) final-text assistant turn with an
        // artifact is deliberately sent bare under ToolLoop (echo only on
        // tool turns) — INFO, not RISK.
        let (_dir, ctx) = seed(
            42,
            42,
            GROQ_MODEL,
            vec![(
                0,
                text_turn(
                    "answer",
                    Some(chat_artifact("think one")),
                    Some(groq_producer(GROQ_MODEL)),
                ),
            )],
        );
        let out = run(&ctx, inspect_args(None));
        assert!(out.contains("no:tool-loop-skip"), "{out}");
        assert!(out.contains("INFO (policy-skips"), "{out}");
        assert!(!out.contains("no:producer-mismatch"), "{out}");
        // The RISK section must be the "none" variant, not a list.
        assert!(
            out.contains("RISK (assistant messages missing the route's declared field): none"),
            "{out}"
        );
    }

    #[test]
    fn requires_rc_empty_content_turn_echoes_artifact_on_wire() {
        // The reported bug shape (opencode-go deepseek→kimi mid-session
        // switch): a turn recorded as reasoning-only — empty content, no tool
        // calls, but a same-model artifact. ToolLoop alone would skip the
        // echo and the injected empty `reasoning_content` cannot make the
        // message non-empty, so upstream 400s with "the message ... must not
        // be empty". The empty-message fallback must echo the artifact: the
        // report shows echo=yes (real text, not the empty injection), NO
        // empty-message candidates on the wire dry-run, and the daemon guard
        // counts 0 problems.
        let (_dir, ctx) = seed(
            42,
            42,
            "deepseek-v4-flash",
            vec![(
                0,
                text_turn(
                    "",
                    Some(chat_artifact("real reasoning text")),
                    Some(ds_producer()),
                ),
            )],
        );
        let out = run(&ctx, inspect_args(None));
        assert!(
            out.contains("echo=yes(declared:reasoning_content)"),
            "artifact echoed under the declared field: {out}"
        );
        assert!(
            out.contains("empty assistant message(s) on the wire: none"),
            "no empty assistant message on the wire: {out}"
        );
        assert!(
            out.contains("daemon guard warn_on_uncaptured_reasoning: 0 problem(s)"),
            "guard clean: {out}"
        );
        assert!(out.contains("ledger vs wire: OK"), "{out}");
    }

    #[test]
    fn wire_empty_turn_without_artifact_is_dropped_from_the_projection() {
        // Same reasoning-only shape but NO artifact to fill the message: the
        // projection drops the empty assistant message, so it never reaches the
        // wire at all — and the guard still reports the turn, because it left the
        // model's view of the conversation.
        let (_dir, ctx) = seed(
            42,
            42,
            "deepseek-v4-flash",
            vec![(0, text_turn("", None, Some(ds_producer())))],
        );
        let out = run(&ctx, inspect_args(None));
        assert!(
            out.contains("projection dropped 1 assistant message(s)"),
            "the drop is reported: {out}"
        );
        assert!(
            out.contains("empty assistant message(s) on the wire: none"),
            "nothing empty survives to the wire: {out}"
        );
        assert!(
            out.contains("daemon guard warn_on_uncaptured_reasoning: 1 problem(s)"),
            "guard flags the turn the model will not see: {out}"
        );
        assert!(out.contains("ledger vs wire: OK"), "{out}");
    }

    #[test]
    fn wire_empty_turn_echoes_artifact_on_non_requires_rc_provider() {
        // The empty-message fallback is provider-agnostic: a groq (ToolLoop,
        // no `reasoning_content` injection) reasoning-only turn with a
        // same-model artifact echoes it, so the report shows echo=yes and no
        // empty-message candidates — same self-heal as the DeepSeek shape.
        let (_dir, ctx) = seed(
            42,
            42,
            GROQ_MODEL,
            vec![(
                0,
                text_turn(
                    "",
                    Some(chat_artifact("groq reasoning")),
                    Some(groq_producer(GROQ_MODEL)),
                ),
            )],
        );
        let out = run(&ctx, inspect_args(None));
        assert!(out.contains("echo=yes(observed)"), "artifact echoed: {out}");
        assert!(
            out.contains("empty assistant message(s) on the wire: none"),
            "no empty assistant message on the wire: {out}"
        );
        assert!(
            out.contains("daemon guard warn_on_uncaptured_reasoning: 0 problem(s)"),
            "guard clean: {out}"
        );
        assert!(out.contains("ledger vs wire: OK"), "{out}");
    }

    #[test]
    fn wire_empty_turn_without_artifact_dropped_on_non_requires_rc_provider() {
        // Same reasoning-only shape on groq but NO artifact: the projection drops
        // the empty assistant message on any chat route, and the guard flags the
        // turn — the DeepSeek-only behaviour generalized to every chat provider.
        let (_dir, ctx) = seed(
            42,
            42,
            GROQ_MODEL,
            vec![(0, text_turn("", None, Some(groq_producer(GROQ_MODEL))))],
        );
        let out = run(&ctx, inspect_args(None));
        assert!(
            out.contains("projection dropped 1 assistant message(s)"),
            "the drop is reported: {out}"
        );
        assert!(
            out.contains("empty assistant message(s) on the wire: none"),
            "nothing empty survives to the wire: {out}"
        );
        assert!(
            out.contains("daemon guard warn_on_uncaptured_reasoning: 1 problem(s)"),
            "guard flags the turn the model will not see: {out}"
        );
        assert!(out.contains("ledger vs wire: OK"), "{out}");
    }

    #[test]
    fn unknown_session_is_an_error() {
        let (_dir, ctx) = seed(42, 42, "deepseek-v4-flash", vec![]);
        let err = SessionInspect
            .execute(inspect_args(Some(999)), None, None, Some(&ctx))
            .unwrap_err();
        assert!(err.0.contains("not found"), "{err:?}");
    }

    #[test]
    fn raw_reasoning_only_for_own_session() {
        let secret = "SECRET THINKING PAYLOAD";
        // Same-session inspection with include_raw: raw snippets rendered.
        let (_dir, ctx) = seed(
            7,
            7,
            "deepseek-v4-flash",
            vec![(
                0,
                tool_turn(
                    "c1",
                    "exec",
                    Some(chat_artifact(secret)),
                    Some(ds_producer()),
                    Some(secret),
                ),
            )],
        );
        let args = SessionInspectArgs {
            include_raw: true,
            ..inspect_args(None)
        };
        let out = run(&ctx, args);
        assert!(out.contains("RAW reasoning"), "{out}");
        assert!(out.contains(secret), "{out}");

        // Cross-session inspection: metadata only — the raw thinking text
        // must not leak, while the artifact descriptor still shows.
        let (_dir, ctx_other) = seed(
            7,  // caller
            99, // target
            "deepseek-v4-flash",
            vec![(
                0,
                tool_turn(
                    "c1",
                    "exec",
                    Some(chat_artifact(secret)),
                    Some(ds_producer()),
                    Some(secret),
                ),
            )],
        );
        let out = run(&ctx_other, inspect_args(Some(99)));
        assert!(
            !out.contains(secret),
            "raw reasoning leaked across sessions: {out}"
        );
        assert!(out.contains("art=chat[reasoning_content;"), "{out}");
    }

    #[test]
    fn registry_registers_session_inspect_in_debug_group() {
        let registry = crate::tools::ToolRegistry::new().build();
        let active: std::collections::HashSet<String> = ["debug".into()].into_iter().collect();
        let defs = registry.available_definitions(&active);
        let names: Vec<&str> = defs.iter().map(|d| d.function.name.as_str()).collect();
        assert!(names.contains(&"session_inspect"), "{names:?}");
        // The debug group must be advertised so load_tools can enable it.
        let group_names: Vec<String> = registry.group_names();
        assert!(group_names.iter().any(|g| g == "debug"), "{group_names:?}");
    }
}
