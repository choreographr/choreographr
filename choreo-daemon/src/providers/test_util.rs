//! Test-only provider doubles shared by the crate's unit tests.
//!
//! Every item here is compiled only under `#[cfg(test)]` (carried by the
//! `mod test_util;` declaration): the existence-only stub, the failing /
//! recording / scripted clients the agent-loop tests drive, and the tool-loop
//! client that exercises real tool dispatch.

use super::*;

#[derive(Debug)]
pub(crate) struct StubProviderClient;

// Test stub: the panics are intentional misuse detectors, and
// clippy::panic_in_result_fn has no allow-*-in-tests config option.
#[expect(clippy::panic_in_result_fn)]
impl ProviderClient for StubProviderClient {
    // &'static str is the trait's required lifetime, not an over-bound literal.
    #[expect(clippy::unnecessary_literal_bound)]
    fn provider_slug(&self) -> &str {
        "test-stub"
    }

    fn chat_completion_turn(
        &self,
        _params: ChatTurnRequest<'_>,
    ) -> Result<ChatTurnResult, InferenceError> {
        panic!("StubProviderClient is not intended for real use");
    }

    fn chat_completion_turn_streaming(
        &self,
        _params: ChatTurnRequest<'_>,
        _on_event: &mut dyn FnMut(StreamEvent) -> io::Result<()>,
    ) -> Result<ChatTurnResult, InferenceError> {
        panic!("StubProviderClient is not intended for real use");
    }

    fn list_models(&self) -> Result<Vec<String>, InferenceError> {
        panic!("StubProviderClient is not intended for real use");
    }
}

pub(crate) fn make_test_provider() -> InferenceProvider {
    InferenceProvider {
        client: Arc::new(StubProviderClient),
        slug: "test-stub".to_string(),
        image_client: None,
    }
}

/// Provider client that fails every streaming turn with a 4xx client
/// error — used to exercise the agent loop's request-failure path
/// (turn error marking + finalize) without touching the network.
#[derive(Debug)]
pub(crate) struct FailingProviderClient;

impl ProviderClient for FailingProviderClient {
    // &'static str is the trait's required lifetime, not an over-bound literal.
    #[expect(clippy::unnecessary_literal_bound)]
    fn provider_slug(&self) -> &str {
        "test-failing"
    }

    fn chat_completion_turn(
        &self,
        _params: ChatTurnRequest<'_>,
    ) -> Result<ChatTurnResult, InferenceError> {
        Err(InferenceError::ClientError {
            status: 402,
            detail: "Insufficient Balance".to_string(),
        })
    }

    fn chat_completion_turn_streaming(
        &self,
        _params: ChatTurnRequest<'_>,
        _on_event: &mut dyn FnMut(StreamEvent) -> io::Result<()>,
    ) -> Result<ChatTurnResult, InferenceError> {
        Err(InferenceError::ClientError {
            status: 402,
            detail: "Insufficient Balance".to_string(),
        })
    }

    fn list_models(&self) -> Result<Vec<String>, InferenceError> {
        Err(InferenceError::ClientError {
            status: 402,
            detail: "Insufficient Balance".to_string(),
        })
    }
}

pub(crate) fn make_failing_provider() -> InferenceProvider {
    InferenceProvider {
        client: Arc::new(FailingProviderClient),
        slug: "test-failing".to_string(),
        image_client: None,
    }
}

/// Provider client whose streaming turns return `TruncatedToolCall` for the
/// first `truncations` calls (simulating a provider that cuts the response
/// off at its output-token limit mid-tool-call), then a normal final answer.
/// Exercises the agent loop's truncated-tool-call recovery. Also records the
/// `previous_response_id` of every call so a test can assert how the loop
/// chains (or stops chaining) across a recovery.
#[derive(Debug)]
pub(crate) struct TruncatingProviderClient {
    truncations: usize,
    calls: std::sync::atomic::AtomicUsize,
    // Every chain id the loop sent, in call order. Poisoning is irrelevant
    // in a test, so the lock is recovered rather than propagated.
    seen_prev_response_ids: std::sync::Mutex<Vec<Option<String>>>,
}

impl TruncatingProviderClient {
    fn new(truncations: usize) -> Self {
        Self {
            truncations,
            calls: std::sync::atomic::AtomicUsize::new(0),
            seen_prev_response_ids: std::sync::Mutex::new(Vec::new()),
        }
    }

    /// Record the chain id the loop sent, then advance the call counter and
    /// return either a truncated-tool-call error or a normal final answer.
    fn next_result(
        &self,
        prev_response_id: Option<&str>,
    ) -> Result<ChatTurnResult, InferenceError> {
        self.seen_prev_response_ids
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .push(prev_response_id.map(str::to_string));
        let n = self.calls.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
        if n < self.truncations {
            return Err(InferenceError::TruncatedToolCall {
                discarded: vec![choreo_proto::DiscardedToolCall {
                    name: "write_file".to_string(),
                    arguments_json: "{\"path\":\"x\",".to_string(),
                }],
            });
        }
        Ok(ChatTurnResult::FinalText(
            choreo_ai_protocols::FinalTextResult {
                content: "done".to_string(),
                truncated: false,
                reasoning: None,
                usage: None,
                response_id: None,
                reasoning_artifact: None,
            },
        ))
    }

    /// The chain ids the client was called with so far, in call order.
    pub(crate) fn seen_previous_response_ids(&self) -> Vec<Option<String>> {
        self.seen_prev_response_ids
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .clone()
    }
}

impl ProviderClient for TruncatingProviderClient {
    // &'static str is the trait's required lifetime, not an over-bound literal.
    #[expect(clippy::unnecessary_literal_bound)]
    fn provider_slug(&self) -> &str {
        "test-truncating"
    }

    fn chat_completion_turn(
        &self,
        params: ChatTurnRequest<'_>,
    ) -> Result<ChatTurnResult, InferenceError> {
        self.next_result(params.previous_response_id)
    }

    fn chat_completion_turn_streaming(
        &self,
        params: ChatTurnRequest<'_>,
        _on_event: &mut dyn FnMut(StreamEvent) -> io::Result<()>,
    ) -> Result<ChatTurnResult, InferenceError> {
        self.next_result(params.previous_response_id)
    }

    fn list_models(&self) -> Result<Vec<String>, InferenceError> {
        Ok(Vec::new())
    }
}

/// Build a provider that truncates its first `truncations` streaming turns.
pub(crate) fn make_truncating_provider(truncations: usize) -> InferenceProvider {
    make_truncating_provider_with_slug(truncations, "test-truncating").0
}

/// Like [`make_truncating_provider`], but with a caller-chosen catalog slug
/// (so a test can select a `ResponseId`-policy provider) and handing back the
/// concrete client so the test can inspect the chain ids it was called with.
pub(crate) fn make_truncating_provider_with_slug(
    truncations: usize,
    slug: &str,
) -> (InferenceProvider, Arc<TruncatingProviderClient>) {
    let client = Arc::new(TruncatingProviderClient::new(truncations));
    let provider = InferenceProvider {
        client: client.clone(),
        slug: slug.to_string(),
        image_client: None,
    };
    (provider, client)
}

/// Provider client that counts how many non-streaming turns it served and
/// returns an empty final answer.
///
/// The "recording sink" the cache-warm driver tests arm against: they prove
/// a ping did (or did not) fire by inspecting [`calls`](Self::calls),
/// without binding a socket. The warmer only ever calls the non-streaming
/// path, so the streaming method is a never-reached error stub.
#[derive(Debug, Default)]
pub(crate) struct RecordingProviderClient {
    calls: std::sync::atomic::AtomicUsize,
}

impl RecordingProviderClient {
    /// How many non-streaming turns this client has served.
    pub(crate) fn calls(&self) -> usize {
        self.calls.load(std::sync::atomic::Ordering::SeqCst)
    }
}

impl ProviderClient for RecordingProviderClient {
    // &'static str is the trait's required lifetime, not an over-bound literal.
    #[expect(clippy::unnecessary_literal_bound)]
    fn provider_slug(&self) -> &str {
        "test-recording"
    }

    fn chat_completion_turn(
        &self,
        _params: ChatTurnRequest<'_>,
    ) -> Result<ChatTurnResult, InferenceError> {
        self.calls.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
        Ok(ChatTurnResult::FinalText(
            choreo_ai_protocols::FinalTextResult {
                content: String::new(),
                truncated: false,
                reasoning: None,
                usage: None,
                response_id: None,
                reasoning_artifact: None,
            },
        ))
    }

    fn chat_completion_turn_streaming(
        &self,
        _params: ChatTurnRequest<'_>,
        _on_event: &mut dyn FnMut(StreamEvent) -> io::Result<()>,
    ) -> Result<ChatTurnResult, InferenceError> {
        // The cache warmer only ever calls the non-streaming path; a
        // streaming call here is a bug, so surface it as an error rather
        // than pretending to work.
        Err(InferenceError::ClientError {
            status: 500,
            detail: "RecordingProviderClient has no streaming path".to_string(),
        })
    }

    fn list_models(&self) -> Result<Vec<String>, InferenceError> {
        Ok(Vec::new())
    }
}

/// Build a provider backed by a [`RecordingProviderClient`], returning the
/// concrete client so a test can inspect [`RecordingProviderClient::calls`].
pub(crate) fn make_recording_provider() -> (InferenceProvider, Arc<RecordingProviderClient>) {
    let client = Arc::new(RecordingProviderClient::default());
    let provider = InferenceProvider {
        client: client.clone(),
        slug: "openai".to_string(),
        image_client: None,
    };
    (provider, client)
}

/// Provider client that answers the first `tool_turns` streaming calls with
/// a `ToolUse` requesting one call to `tool_name`, and every later call with a
/// final answer — the smallest shape that makes the agent loop iterate more
/// than once inside a single request.
///
/// `random` (the default) is a pure computation: it reads no session state,
/// touches no filesystem, and needs no credential, so a test can drive real
/// tool dispatch through the loop without a fixture. The tool name is
/// configurable so a test can instead point at a tool it registers itself
/// (e.g. one that blocks and cancels). The call counter is atomic (the trait
/// hands the client to the loop by shared reference), matching
/// [`RecordingProviderClient`].
#[derive(Debug)]
pub(crate) struct ToolLoopProviderClient {
    tool_turns: usize,
    tool_name: String,
    served: std::sync::atomic::AtomicUsize,
}

impl ProviderClient for ToolLoopProviderClient {
    // &'static str is the trait's required lifetime, not an over-bound literal.
    #[expect(clippy::unnecessary_literal_bound)]
    fn provider_slug(&self) -> &str {
        "openai"
    }

    fn chat_completion_turn(
        &self,
        _params: ChatTurnRequest<'_>,
    ) -> Result<ChatTurnResult, InferenceError> {
        // The agent loop always drives the streaming path; a
        // non-streaming call here is a bug.
        Err(InferenceError::ClientError {
            status: 500,
            detail: "ToolLoopProviderClient is streaming-only".to_string(),
        })
    }

    fn chat_completion_turn_streaming(
        &self,
        params: ChatTurnRequest<'_>,
        _on_event: &mut dyn FnMut(StreamEvent) -> io::Result<()>,
    ) -> Result<ChatTurnResult, InferenceError> {
        // A real provider reports the upload boundary before any output.
        if let Some(cb) = &params.on_uploaded {
            cb();
        }
        let served = self
            .served
            .fetch_add(1, std::sync::atomic::Ordering::SeqCst);
        if served < self.tool_turns {
            return Ok(ChatTurnResult::ToolUse(
                choreo_ai_protocols::ChatAssistantToolUse {
                    content: Some(format!("step {served}")),
                    tool_calls: vec![choreo_ai_protocols::ChatToolCall {
                        id: format!("call_{served}"),
                        name: self.tool_name.clone(),
                        arguments_json: "{}".to_string(),
                        caller: None,
                    }],
                    reasoning: None,
                    usage: None,
                    response_id: None,
                    reasoning_artifact: None,
                },
            ));
        }
        Ok(ChatTurnResult::FinalText(
            choreo_ai_protocols::FinalTextResult {
                content: "done".to_string(),
                truncated: false,
                reasoning: None,
                usage: None,
                response_id: None,
                reasoning_artifact: None,
            },
        ))
    }

    fn list_models(&self) -> Result<Vec<String>, InferenceError> {
        Ok(Vec::new())
    }
}

/// Build a provider that requests one `random` tool call for its first
/// `tool_turns` streaming turns, then answers with a final text (see
/// [`ToolLoopProviderClient`]).
pub(crate) fn make_tool_loop_provider(tool_turns: usize) -> InferenceProvider {
    make_tool_loop_provider_with_tool(tool_turns, "random")
}

/// Build a [`ToolLoopProviderClient`] that requests one call to `tool_name` for
/// its first `tool_turns` streaming turns, then answers with a final text.
pub(crate) fn make_tool_loop_provider_with_tool(
    tool_turns: usize,
    tool_name: &str,
) -> InferenceProvider {
    InferenceProvider {
        client: Arc::new(ToolLoopProviderClient {
            tool_turns,
            tool_name: tool_name.to_string(),
            served: std::sync::atomic::AtomicUsize::new(0),
        }),
        slug: "openai".to_string(),
        image_client: None,
    }
}

/// Provider client that replays a scripted sequence of streaming `on_event`
/// deltas and then returns a fixed final answer, so a test can pin exactly
/// which `SessionStatus` phases the daemon broadcasts as the stream flows.
///
/// It fires the upload callback before any delta (a real provider reports
/// the boundary once the request payload is fully uploaded), and when
/// `retry_first` is set it models one retried attempt: the failed attempt's
/// upload boundary fires first, then the retry callback, then the
/// successful attempt's upload boundary — the hook fires per attempt,
/// because the retry loop wraps the request send and the upload completes
/// before the response status is known.  A test can thus assert the phase
/// advances correctly out of the `Retrying` status and across the `Waiting`
/// boundary.
#[derive(Debug)]
pub(crate) struct ScriptedStreamProviderClient {
    events: Vec<StreamEvent>,
    retry_first: bool,
}

impl ProviderClient for ScriptedStreamProviderClient {
    // &'static str is the trait's required lifetime, not an over-bound literal.
    #[expect(clippy::unnecessary_literal_bound)]
    fn provider_slug(&self) -> &str {
        "openai"
    }

    fn chat_completion_turn(
        &self,
        _params: ChatTurnRequest<'_>,
    ) -> Result<ChatTurnResult, InferenceError> {
        // Streaming-only stub: the agent loop always drives the streaming
        // path, so a non-streaming call here is a bug.
        Err(InferenceError::ClientError {
            status: 500,
            detail: "ScriptedStreamProviderClient is streaming-only".to_string(),
        })
    }

    fn chat_completion_turn_streaming(
        &self,
        params: ChatTurnRequest<'_>,
        on_event: &mut dyn FnMut(StreamEvent) -> io::Result<()>,
    ) -> Result<ChatTurnResult, InferenceError> {
        // The upload hook fires per attempt, on the client side, before
        // the response status is known: a failed first attempt reports the
        // `Waiting` boundary too, then the retry callback fires before the
        // successful attempt's own upload boundary.  So with `retry_first`
        // the sequence is upload → retry → upload, matching a real retried
        // provider call.
        if self.retry_first {
            if let Some(cb) = &params.on_uploaded {
                cb();
            }
            if let Some(cb) = params.on_retry.as_mut() {
                cb(1, 3, std::time::Duration::from_millis(1));
            }
        }
        // The provider has received the full request (payload uploaded)
        // before it streams any delta, so the upload boundary always
        // precedes the events — matching a real provider call.
        if let Some(cb) = &params.on_uploaded {
            cb();
        }
        for event in &self.events {
            on_event(event.clone())?;
        }
        Ok(ChatTurnResult::FinalText(
            choreo_ai_protocols::FinalTextResult {
                content: "done".to_string(),
                truncated: false,
                reasoning: None,
                usage: None,
                response_id: None,
                reasoning_artifact: None,
            },
        ))
    }

    fn list_models(&self) -> Result<Vec<String>, InferenceError> {
        Ok(Vec::new())
    }
}

/// Build a provider that replays `events` through the streaming callback,
/// optionally firing the retry callback before any delta (see
/// [`ScriptedStreamProviderClient`]).
pub(crate) fn make_scripted_stream_provider(
    events: Vec<StreamEvent>,
    retry_first: bool,
) -> InferenceProvider {
    InferenceProvider {
        client: Arc::new(ScriptedStreamProviderClient {
            events,
            retry_first,
        }),
        slug: "openai".to_string(),
        image_client: None,
    }
}
