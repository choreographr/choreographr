use choreo_proto::{AssistantToolCallRecord, OutputStream, ToolResultRecord, Turn};
use choreo_sanitize::{MAX_TOOL_OUTPUT_BYTES, TRUNCATION_SUFFIX};
use std::collections::{BTreeMap, HashMap};

/// Cap on the live accumulated content of a streaming tool result. The
/// daemon's final (authoritative) content is capped at
/// [`choreo_sanitize::MAX_TOOL_OUTPUT_BYTES`] (128 KiB) and replaces this
/// accumulation when the turn completes; the cap here bounds the *in-flight*
/// view so a streaming tool that out-produces the budget (shell/VM/find)
/// cannot balloon client memory before the final record lands. Deliberately
/// mirrors the daemon's cap so the live view and the recorded result agree.
const MAX_STREAMED_TOOL_CONTENT_BYTES: usize = MAX_TOOL_OUTPUT_BYTES;

/// Append `data` to `content`, stopping at [`MAX_STREAMED_TOOL_CONTENT_BYTES`]
/// with the shared `...[truncated]` byte-cap marker once the cap is crossed;
/// later chunks are dropped. Cuts on a char boundary so a multi-byte char is
/// never split. Shared by the append and stub-creation paths of
/// `SessionView::tool_result_chunk`. The marker text is the shared
/// [`choreo_sanitize::TRUNCATION_SUFFIX`], so the live view reads exactly
/// like the daemon's final capped result.
fn push_capped(content: &mut String, data: &str) {
    if content.len() >= MAX_STREAMED_TOOL_CONTENT_BYTES {
        // At or past the cap. When a chunk previously landed *exactly* on
        // the cap, no marker was appended then — but any further chunk
        // proves more output existed, so the stream was truncated: append
        // the shared marker once (after it, content.len() > cap and every
        // later chunk early-returns here). The daemon's ByteBudget fires
        // its marker under the same condition, so the live view stays in
        // lockstep with the recorded result instead of silently dropping
        // the remainder with no truncation signal.
        if content.len() == MAX_STREAMED_TOOL_CONTENT_BYTES && !data.is_empty() {
            content.push_str(TRUNCATION_SUFFIX);
        }
        return;
    }
    let remaining = MAX_STREAMED_TOOL_CONTENT_BYTES - content.len();
    if data.len() <= remaining {
        content.push_str(data);
    } else {
        // Same byte-cap marker the daemon's `truncate_tool_output` appends,
        // so the live view reads exactly like the final capped result.
        let cut = data.floor_char_boundary(remaining);
        content.push_str(&data[..cut]);
        content.push_str(TRUNCATION_SUFFIX);
    }
}

/// Client-side view of a session's turn history.
///
/// Maps `turn_id → Turn` (ordered) and `request_id → turn_id` for
/// routing streaming chunks during an active agent loop.
#[derive(Debug, Clone)]
pub struct SessionView {
    /// turn_id → Turn. Ordered by key (monotonically assigned by daemon).
    pub turns: BTreeMap<u32, Turn>,
    /// request_id → turn_id for streaming chunk routing.
    /// Inserted on `Started`, removed on `Done`/`Failed`/`Cancelled`.
    pub request_to_turn: HashMap<u32, u32>,
    /// call_id → invocation description for tool calls whose start event
    /// (`ToolCallStarted`) arrived before their first streaming chunk created
    /// a stub result.  The description rides on the start event (never on a
    /// chunk — chunks are droppable under load), so a stub created by a later
    /// chunk must be able to recover it here.  Entries are removed when the
    /// authoritative turn replaces the accumulated one (`insert_or_replace`).
    tool_call_descriptions: HashMap<String, String>,
}

impl SessionView {
    pub fn new() -> Self {
        Self {
            turns: BTreeMap::new(),
            request_to_turn: HashMap::new(),
            tool_call_descriptions: HashMap::new(),
        }
    }

    pub fn insert_or_replace(&mut self, turn_id: u32, turn: Turn) {
        // Once the authoritative turn (with the final records) replaces the
        // accumulated one, no more chunks arrive for its calls — drop their
        // description entries so the map stays bounded by in-flight calls.
        for tc in &turn.tool_calls {
            self.tool_call_descriptions.remove(&tc.call_id);
        }
        self.turns.insert(turn_id, turn);
    }

    /// Drop the invocation-description entries for every call of `turn_id`.
    ///
    /// [`Self::insert_or_replace`] cleans the map when the authoritative turn
    /// replaces the accumulated one, but a request that fails mid-tool never
    /// re-broadcasts its turn (`Failed` arrives with no final `TurnAppended`),
    /// and a success whose final broadcast was dropped under load would
    /// otherwise leak entries too.  Callers invoke this from their
    /// request-terminal handlers (`handle_done` / `handle_failed`) so the map
    /// stays bounded by in-flight calls even on those paths.
    pub fn clear_tool_call_descriptions(&mut self, turn_id: u32) {
        if let Some(turn) = self.turns.get(&turn_id) {
            for tc in &turn.tool_calls {
                self.tool_call_descriptions.remove(&tc.call_id);
            }
        }
    }

    pub fn get(&self, turn_id: u32) -> Option<&Turn> {
        self.turns.get(&turn_id)
    }

    pub fn get_mut(&mut self, turn_id: u32) -> Option<&mut Turn> {
        self.turns.get_mut(&turn_id)
    }

    pub fn request_turn(&self, request_id: u32) -> Option<&Turn> {
        let turn_id = self.request_to_turn.get(&request_id)?;
        self.turns.get(turn_id)
    }

    pub fn request_turn_mut(&mut self, request_id: u32) -> Option<&mut Turn> {
        let turn_id = self.request_to_turn.get(&request_id)?;
        self.turns.get_mut(turn_id)
    }

    /// Route streaming output to the current turn for this request.
    pub fn stream_chunk(&mut self, request_id: u32, stream: OutputStream, data: &str) {
        let Some(&turn_id) = self.request_to_turn.get(&request_id) else {
            tracing::warn!(%request_id, "stream_chunk: unknown request");
            return;
        };
        let Some(turn) = self.turns.get_mut(&turn_id) else {
            tracing::warn!(%turn_id, "stream_chunk: unknown turn");
            return;
        };
        match stream {
            OutputStream::Reasoning => {
                if let Some(ref mut text) = turn.assistant_reasoning {
                    text.push_str(data);
                } else {
                    turn.assistant_reasoning = Some(data.to_string());
                }
            }
            OutputStream::Answer => {
                // Reasoning content is retained when the response starts —
                // the TUI shows it behind a collapsible header, so clearing
                // it here would destroy the ability to re-expand it.
                if let Some(ref mut text) = turn.assistant_text {
                    text.push_str(data);
                } else {
                    turn.assistant_text = Some(data.to_string());
                }
            }
            _ => {
                tracing::warn!(?stream, "stream_chunk: unknown stream type");
            }
        }
    }

    /// Route a tool call start notification.
    pub fn tool_call_started(
        &mut self,
        request_id: u32,
        call_id: String,
        name: String,
        args: String,
        invocation_description: String,
    ) {
        let Some(&turn_id) = self.request_to_turn.get(&request_id) else {
            tracing::warn!(%request_id, "tool_call_started: unknown request");
            return;
        };
        let Some(turn) = self.turns.get_mut(&turn_id) else {
            tracing::warn!(%turn_id, "tool_call_started: unknown turn");
            return;
        };
        // Stash the description for a stub that the first chunk may create
        // later.  ToolCallStarted is broadcast before any chunk, so the stub
        // usually does not exist yet; without this, the description would be
        // lost the moment the start event passed (the description never rides
        // on a chunk).
        if !invocation_description.is_empty() {
            self.tool_call_descriptions
                .insert(call_id.clone(), invocation_description.clone());
        }
        // Backfill the tool name AND description onto a stub tool result
        // created out of order (a chunk that arrived before this event).  The
        // stub's name is unresolvable at chunk time if the tool_call hasn't
        // landed yet; filling both in here keeps the TUI's quiet/default-
        // collapse decision and the header rendering correct without waiting
        // for the full turn to replace the record.  Only fills empty fields —
        // a seeded placeholder or an earlier backfill already carries values.
        if let Some(result) = turn.tool_results.iter_mut().find(|r| r.call_id == call_id) {
            if result.name.is_empty() {
                result.name = name.clone();
            }
            if result.invocation_description.is_empty() {
                result.invocation_description = invocation_description.clone();
            }
        }
        // The seeded turn already carries this call in tool_calls (the daemon
        // broadcasts the seeded turn before ToolCallStarted); don't duplicate
        // the record when the start event lands after it.
        if !turn.tool_calls.iter().any(|tc| tc.call_id == call_id) {
            turn.tool_calls.push(AssistantToolCallRecord {
                call_id,
                name,
                arguments_json: args,
            });
        }
    }

    /// Route a tool result chunk — appends to the matching ToolResultRecord.
    /// Creates a stub record if the ToolCallStarted event hasn't arrived yet.
    pub fn tool_result_chunk(&mut self, request_id: u32, call_id: &str, data: &str) {
        let Some(&turn_id) = self.request_to_turn.get(&request_id) else {
            tracing::warn!(%request_id, "tool_result_chunk: unknown request");
            return;
        };
        let Some(turn) = self.turns.get_mut(&turn_id) else {
            tracing::warn!(%turn_id, "tool_result_chunk: unknown turn");
            return;
        };
        // The start event's description is only consulted when a record
        // actually needs it: a placeholder/stub that already carries its
        // description (the common seeded case) skips the map read and the
        // String clone entirely on the per-chunk hot path.
        match turn.tool_results.iter_mut().find(|r| r.call_id == call_id) {
            Some(result) => {
                // A record that predates the start event (e.g. the seeded
                // placeholder when the ToolCallStarted broadcast was dropped)
                // still gets its header from the first chunk onward.
                if result.invocation_description.is_empty()
                    && let Some(desc) = self.tool_call_descriptions.get(call_id)
                {
                    result.invocation_description = desc.clone();
                }
                push_capped(&mut result.content, data);
            }
            None => {
                // Stub created out of order (chunk before ToolCallStarted).
                // Resolve the tool name from the turn's tool_calls so the
                // quiet/derived-default decision (which keys on the name)
                // is correct from the first chunk instead of flipping when
                // the real record lands.  Falls back to empty when the call
                // is unknown, which is never quiet → default expanded.
                let name = turn
                    .tool_calls
                    .iter()
                    .find(|tc| tc.call_id == call_id)
                    .map(|tc| tc.name.clone())
                    .unwrap_or_default();
                let mut content = String::new();
                push_capped(&mut content, data);
                turn.tool_results.push(ToolResultRecord {
                    call_id: call_id.to_string(),
                    name,
                    content,
                    is_error: false,
                    // The stub branch runs once per call, so the lookup +
                    // clone here is off the per-chunk hot path.
                    invocation_description: self
                        .tool_call_descriptions
                        .get(call_id)
                        .cloned()
                        .unwrap_or_default(),
                    image: None,
                });
            }
        }
    }

    pub fn iter(&self) -> impl Iterator<Item = (&u32, &Turn)> {
        self.turns.iter()
    }
}

impl Default for SessionView {
    fn default() -> Self {
        Self::new()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn turn_with_tool_call(call_id: &str, name: &str) -> Turn {
        Turn {
            created_at: choreo_proto::TimestampMs::now(),
            undone: false,
            error: None,
            user_text: None,
            assistant_text: None,
            assistant_reasoning: None,
            tool_calls: vec![AssistantToolCallRecord {
                call_id: call_id.into(),
                name: name.into(),
                arguments_json: "{}".into(),
            }],
            token_usage: None,
            tool_results: vec![],
            displayed_images: vec![],
            reasoning_artifact: None,
            reasoning_producer: None,
        }
    }

    #[test]
    fn tool_result_chunk_stub_resolves_name_from_tool_calls() {
        // A chunk arriving before the real record (or before
        // ToolCallStarted) creates a stub.  The stub must carry the tool
        // name — resolved from `tool_calls` — so the TUI's quiet/
        // derived-default decision is correct from the first chunk instead
        // of flipping when the real record lands.
        let mut view = SessionView::new();
        view.insert_or_replace(1, turn_with_tool_call("call-1", "read_file"));
        view.request_to_turn.insert(7, 1);

        view.tool_result_chunk(7, "call-1", "line one\n");

        let turn = view.get(1).unwrap();
        assert_eq!(turn.tool_results.len(), 1);
        assert_eq!(turn.tool_results[0].name, "read_file");
        assert_eq!(turn.tool_results[0].call_id, "call-1");
        assert_eq!(turn.tool_results[0].content, "line one\n");
    }

    #[test]
    fn tool_result_chunk_appends_to_existing_record() {
        let mut view = SessionView::new();
        let mut turn = turn_with_tool_call("call-1", "sh");
        turn.tool_results.push(ToolResultRecord {
            call_id: "call-1".into(),
            name: "sh".into(),
            content: "first".into(),
            is_error: false,
            invocation_description: String::new(),
            image: None,
        });
        view.insert_or_replace(1, turn);
        view.request_to_turn.insert(7, 1);

        view.tool_result_chunk(7, "call-1", "second");

        let turn = view.get(1).unwrap();
        assert_eq!(turn.tool_results.len(), 1, "chunks append, never duplicate");
        assert_eq!(turn.tool_results[0].content, "firstsecond");
    }

    #[test]
    fn tool_result_chunk_caps_live_accumulation() {
        // A streaming tool that out-produces the budget must not balloon the
        // in-flight view: the accumulation stops at the daemon's 128 KiB cap
        // with one marker, and later chunks are dropped entirely.
        let mut view = SessionView::new();
        let mut turn = turn_with_tool_call("call-1", "sh");
        let prefix = "x".repeat(MAX_STREAMED_TOOL_CONTENT_BYTES - 10);
        turn.tool_results.push(ToolResultRecord {
            call_id: "call-1".into(),
            name: "sh".into(),
            content: prefix.clone(),
            is_error: false,
            invocation_description: String::new(),
            image: None,
        });
        view.insert_or_replace(1, turn);
        view.request_to_turn.insert(7, 1);

        // A 20-byte chunk only has 10 bytes of headroom: it is cut on a
        // char boundary and the marker appended once.
        view.tool_result_chunk(7, "call-1", "12345678901234567890");
        let content = &view.get(1).unwrap().tool_results[0].content;
        assert!(
            content.starts_with(&prefix),
            "accumulated content must keep the pre-cap prefix"
        );
        assert!(
            content.ends_with("...[truncated]"),
            "cap crossing must append the marker once"
        );
        assert!(
            content.len() <= MAX_STREAMED_TOOL_CONTENT_BYTES + "\n...[truncated]".len(),
            "content must stay within cap + marker"
        );

        // Everything after the cap is dropped.
        let len_before = content.len();
        view.tool_result_chunk(7, "call-1", "more data");
        assert_eq!(
            view.get(1).unwrap().tool_results[0].content.len(),
            len_before,
            "chunks past the cap must be dropped"
        );
    }

    #[test]
    fn tool_result_chunk_caps_giant_stub_chunk() {
        // A single first chunk larger than the cap (pathological) must be
        // capped even on the out-of-order stub-creation path.
        let mut view = SessionView::new();
        view.insert_or_replace(1, turn_with_tool_call("call-1", "sh"));
        view.request_to_turn.insert(7, 1);

        let giant = "y".repeat(MAX_STREAMED_TOOL_CONTENT_BYTES + 1000);
        view.tool_result_chunk(7, "call-1", &giant);

        let content = &view.get(1).unwrap().tool_results[0].content;
        assert!(
            content.ends_with("...[truncated]"),
            "stub must carry the marker"
        );
        assert!(
            content.len() <= MAX_STREAMED_TOOL_CONTENT_BYTES + "\n...[truncated]".len(),
            "stub content must stay within cap + marker"
        );
    }

    #[test]
    fn tool_result_chunk_exact_fit_to_cap_still_marks_truncation() {
        // Regression: a chunk landing exactly on the cap appends no marker
        // at that moment, but any *later* chunk proves more output existed —
        // the live view must then show the truncation marker once, not
        // silently drop the remainder with no signal (the daemon's
        // ByteBudget fires under the same condition, so the views stay in
        // lockstep).
        let mut view = SessionView::new();
        let mut turn = turn_with_tool_call("call-1", "sh");
        let exact = "x".repeat(MAX_STREAMED_TOOL_CONTENT_BYTES);
        turn.tool_results.push(ToolResultRecord {
            call_id: "call-1".into(),
            name: "sh".into(),
            content: exact.clone(),
            is_error: false,
            invocation_description: String::new(),
            image: None,
        });
        view.insert_or_replace(1, turn);
        view.request_to_turn.insert(7, 1);

        // A later chunk proves truncation: the marker appears exactly once.
        view.tool_result_chunk(7, "call-1", "more");
        let len_after_marker = {
            let content = &view.get(1).unwrap().tool_results[0].content;
            assert!(
                content.ends_with("...[truncated]"),
                "post-exact-fit chunk must append the marker: {:?}",
                &content[content.len().saturating_sub(40)..]
            );
            content.len()
        };
        assert_eq!(
            len_after_marker,
            MAX_STREAMED_TOOL_CONTENT_BYTES + TRUNCATION_SUFFIX.len(),
            "cap + one marker"
        );

        // Subsequent chunks are dropped; the marker is not duplicated.
        view.tool_result_chunk(7, "call-1", "even more");
        assert_eq!(
            view.get(1).unwrap().tool_results[0].content.len(),
            len_after_marker,
            "chunks past the cap must be dropped without re-marking"
        );
    }

    #[test]
    fn tool_result_chunk_unknown_call_keeps_empty_name() {
        // A chunk for a call_id with no matching tool_call creates a stub
        // with an empty name — never quiet, so the TUI defaults it to
        // expanded.
        let mut view = SessionView::new();
        view.insert_or_replace(1, turn_with_tool_call("call-1", "sh"));
        view.request_to_turn.insert(7, 1);

        view.tool_result_chunk(7, "nope", "data");

        let turn = view.get(1).unwrap();
        assert_eq!(turn.tool_results.len(), 1);
        assert_eq!(turn.tool_results[0].name, "");
    }

    #[test]
    fn tool_call_started_backfills_stub_name() {
        // A chunk arriving before ToolCallStarted creates a stub whose name
        // is unresolvable at chunk time; the start event must backfill it
        // so the TUI's quiet/default-collapse decision is correct once the
        // name is known — without waiting for the full turn to replace the
        // record.
        let mut view = SessionView::new();
        view.insert_or_replace(1, turn_with_tool_call("call-1", "sh"));
        view.request_to_turn.insert(7, 1);

        // Stub for a call whose tool_call has not landed yet → empty name.
        view.tool_result_chunk(7, "call-2", "data\n");
        assert_eq!(view.get(1).unwrap().tool_results[0].name, "");

        // The start event arrives: the name is backfilled onto the stub.
        view.tool_call_started(
            7,
            "call-2".into(),
            "read_file".into(),
            "{}".into(),
            "".into(),
        );

        let turn = view.get(1).unwrap();
        assert_eq!(turn.tool_results[0].name, "read_file");
        assert_eq!(turn.tool_calls.len(), 2, "the call is recorded as usual");
    }

    #[test]
    fn tool_call_started_stashes_description_for_upcoming_stub() {
        // ToolCallStarted is broadcast before the first chunk, so when the
        // chunk arrives the stub does not exist yet — the description stashed
        // by the start event must be recovered at stub-creation time.
        let mut view = SessionView::new();
        view.insert_or_replace(1, turn_with_tool_call("call-1", "sh"));
        view.request_to_turn.insert(7, 1);

        view.tool_call_started(
            7,
            "call-2".into(),
            "sh".into(),
            r#"{"command":"cargo build"}"#.into(),
            "Running command: `cargo build`.".into(),
        );
        assert!(
            view.get(1).unwrap().tool_results.is_empty(),
            "the start event alone must not create a result record"
        );

        view.tool_result_chunk(7, "call-2", "Compiling…\n");

        let turn = view.get(1).unwrap();
        assert_eq!(turn.tool_results.len(), 1);
        assert_eq!(
            turn.tool_results[0].invocation_description, "Running command: `cargo build`.",
            "stub must carry the description from the start event"
        );
        assert_eq!(turn.tool_results[0].name, "sh");
        assert_eq!(turn.tool_results[0].content, "Compiling…\n");
    }

    #[test]
    fn tool_call_started_backfills_description_onto_out_of_order_stub() {
        // A chunk arriving before the start event creates a stub without a
        // description; the start event must backfill it (mirroring the name
        // backfill) so the live header renders once the event lands.
        let mut view = SessionView::new();
        view.insert_or_replace(1, turn_with_tool_call("call-1", "sh"));
        view.request_to_turn.insert(7, 1);

        view.tool_result_chunk(7, "call-2", "output\n");
        assert_eq!(
            view.get(1).unwrap().tool_results[0].invocation_description,
            ""
        );

        view.tool_call_started(
            7,
            "call-2".into(),
            "sh".into(),
            "{}".into(),
            "Running shell command: `ls`.".into(),
        );

        let turn = view.get(1).unwrap();
        assert_eq!(
            turn.tool_results[0].invocation_description, "Running shell command: `ls`.",
            "start event must backfill the description onto the stub"
        );
        assert_eq!(turn.tool_results[0].name, "sh");
    }

    #[test]
    fn tool_call_started_does_not_duplicate_seeded_tool_call() {
        // The daemon broadcasts the seeded turn (with tool_calls) before
        // ToolCallStarted, so the start event must not push a duplicate call
        // record — duplicates would skew the tool_calls ordering used to
        // resolve stub names.
        let mut view = SessionView::new();
        view.insert_or_replace(1, turn_with_tool_call("call-1", "sh"));
        view.request_to_turn.insert(7, 1);

        view.tool_call_started(7, "call-1".into(), "sh".into(), "{}".into(), "".into());

        let turn = view.get(1).unwrap();
        assert_eq!(
            turn.tool_calls.len(),
            1,
            "a seeded tool_call must not be duplicated by its start event"
        );
    }

    #[test]
    fn insert_or_replace_drops_stale_description_entries() {
        // Description entries are only needed while a call's streaming stub
        // is being assembled; once the authoritative turn replaces the
        // accumulated one, the entries must be cleaned so the map stays
        // bounded by in-flight calls.
        let mut view = SessionView::new();
        view.insert_or_replace(1, turn_with_tool_call("call-1", "sh"));
        view.request_to_turn.insert(7, 1);
        view.tool_call_started(
            7,
            "call-1".into(),
            "sh".into(),
            "{}".into(),
            "Running shell command: `ls`.".into(),
        );
        assert_eq!(view.tool_call_descriptions.len(), 1);

        // The final turn (with the completed record) replaces the stub.
        let mut turn = turn_with_tool_call("call-1", "sh");
        turn.tool_results.push(ToolResultRecord {
            call_id: "call-1".into(),
            name: "sh".into(),
            content: "done".into(),
            is_error: false,
            invocation_description: "Running shell command: `ls`.".into(),
            image: None,
        });
        view.insert_or_replace(1, turn);

        assert!(
            view.tool_call_descriptions.is_empty(),
            "replaced turn's call descriptions must be dropped"
        );
    }

    #[test]
    fn clear_tool_call_descriptions_covers_failed_request_path() {
        // A request that fails mid-tool never re-broadcasts its turn, so
        // `insert_or_replace` never runs for it; the request-terminal
        // handlers call `clear_tool_call_descriptions` instead so the map
        // stays bounded by in-flight calls even then.
        let mut view = SessionView::new();
        view.insert_or_replace(1, turn_with_tool_call("call-1", "sh"));
        view.request_to_turn.insert(7, 1);
        view.tool_call_started(
            7,
            "call-1".into(),
            "sh".into(),
            "{}".into(),
            "Running shell command: `ls`.".into(),
        );
        assert_eq!(view.tool_call_descriptions.len(), 1);

        // No TurnAppended arrives; the failed-request handler clears by
        // turn_id (looked up from request_to_turn before it is removed).
        view.clear_tool_call_descriptions(1);
        assert!(
            view.tool_call_descriptions.is_empty(),
            "failed-request cleanup must drop the stashed descriptions"
        );

        // Idempotent, and a no-op for unknown turns.
        view.clear_tool_call_descriptions(1);
        view.clear_tool_call_descriptions(999);
    }

    #[test]
    fn tool_result_chunk_fills_empty_placeholder_from_stash() {
        // A placeholder whose description is still empty (e.g. the seeded
        // turn arrived but the start event's description was stashed for a
        // record that predates it) must be filled from the stash on the
        // first chunk — the append path, not just stub creation.
        let mut view = SessionView::new();
        let mut turn = turn_with_tool_call("call-1", "sh");
        turn.tool_results.push(ToolResultRecord {
            call_id: "call-1".into(),
            name: "sh".into(),
            content: String::new(),
            is_error: false,
            invocation_description: String::new(),
            image: None,
        });
        view.insert_or_replace(1, turn);
        view.request_to_turn.insert(7, 1);

        // The start event stashes the description (broadcast before chunks).
        view.tool_call_started(
            7,
            "call-1".into(),
            "sh".into(),
            "{}".into(),
            "Running shell command: `ls`.".into(),
        );

        view.tool_result_chunk(7, "call-1", "output\n");

        let result = &view.get(1).unwrap().tool_results[0];
        assert_eq!(
            result.invocation_description, "Running shell command: `ls`.",
            "append path must fill the empty placeholder from the stash"
        );
        assert_eq!(result.content, "output\n");
    }
}
