//! The `tools/call` path: deadline, cancellation, progress, and the MRTR loop.
//!
//! A tool call can span several JSON-RPC round-trips when the server answers
//! with an `input_required` result, and it must honour both the caller's
//! deadline (reset while progress arrives) and the session's cancellation
//! token. Progress notifications for the call's `progressToken` are coalesced
//! and forwarded to the caller's streaming sink.

use super::convert::{convert_call_result, map_service_error};
use super::handler::ServerEvent;
use crate::error::McpError;
use crate::protocol::CallToolResult;
use crate::session::{CallRequest, EngineCall};
use rmcp::model::{
    CallToolRequest, CallToolRequestParams, CancelledNotificationParam, ClientRequest,
    ElicitResult, ElicitationAction, InputRequest, InputRequests, InputResponses, ServerResult,
};
use rmcp::service::{Peer, PeerRequestOptions, RoleClient};
use std::time::{Duration, Instant};

/// Upper bound on MRTR round-trips for a single `tools/call`.
///
/// A server may answer `tools/call` with an `input_required` result and expect
/// the client to fulfil the named input requests and retry. This client cannot
/// render an elicitation prompt yet, so it answers every request with a
/// `decline` (the MRTR-legal "no") and retries once; the round cap bounds a
/// peer that would otherwise keep asking.
const MAX_MRTR_ROUNDS: usize = 3;

/// Minimum spacing between forwarded progress chunks.
///
/// A chatty server can emit a progress notification per item; relaying every
/// one would flood the chunk sink. Coalescing to at most one chunk per interval
/// keeps the display live without the traffic.
const PROGRESS_MIN_INTERVAL: Duration = Duration::from_millis(100);

/// Drive one `tools/call`, honouring the deadline, the cancellation token, and
/// the server's MRTR `input_required` responses.
///
/// A tool call can span several JSON-RPC round-trips when the server answers
/// with an `input_required` result: this client cannot render an elicitation
/// prompt, so it declines each requested input and retries the original call
/// with the decline responses and the echoed `requestState`. The retry loop is
/// bounded by [`MAX_MRTR_ROUNDS`]; a server that keeps asking is failed with a
/// message naming the input it wanted rather than hanging.
///
/// Progress notifications for this call's `progressToken` are forwarded to the
/// caller's chunk sink, rate-limited to [`PROGRESS_MIN_INTERVAL`].
pub(super) async fn call_tool_impl(
    peer: &Peer<RoleClient>,
    events: &tokio::sync::broadcast::Sender<ServerEvent>,
    call: EngineCall,
    slug: &str,
) -> Result<CallToolResult, McpError> {
    let EngineCall {
        request,
        cancel,
        chunk_tx,
    } = call;
    let CallRequest {
        name,
        arguments,
        timeout,
    } = request;

    let mut input_responses: Option<InputResponses> = None;
    let mut request_state: Option<String> = None;
    let mut rounds = 0usize;
    let mut last_progress: Option<Instant> = None;

    loop {
        let mut params = CallToolRequestParams::new(name.clone());
        if let serde_json::Value::Object(map) = &arguments {
            params.arguments = Some(map.clone());
        }
        if let Some(responses) = input_responses.take() {
            params = params.with_input_responses(responses);
        }
        if let Some(state) = request_state.take() {
            params = params.with_request_state(state);
        }

        // Subscribe BEFORE sending: the broadcast drops a message when no
        // receiver is attached, so a fast server's progress must find this
        // receiver already live.
        let mut events_rx = events.subscribe();

        // Request-scoped options: the deadline resets while progress
        // notifications arrive (a long tool that reports progress is not
        // killed mid-work).
        let options = PeerRequestOptions::with_timeout(timeout).reset_timeout_on_progress();
        let handle = peer
            .send_cancellable_request(
                ClientRequest::CallToolRequest(CallToolRequest::new(params)),
                options,
            )
            .await
            .map_err(|e| map_service_error(e, slug))?;
        let request_id = handle.id.clone();
        let progress_token = handle.progress_token.clone();

        // A fatal transport error maps out of the loop; the response is the
        // only thing that drives a retry.
        let response_fut = handle.await_response();
        tokio::pin!(response_fut);
        let response = loop {
            tokio::select! {
                biased;
                // Cancellation arm first: a session cancel stops the call the
                // instant it is observed, and we tell the server to stop.
                () = cancel.cancelled() => {
                    let _ = peer
                        .notify_cancelled(CancelledNotificationParam::new(
                            Some(request_id.clone()),
                            Some("client cancelled".to_string()),
                        ))
                        .await;
                    return Err(McpError::Cancelled);
                }
                event = events_rx.recv() => {
                    // Any event that is not this call's progress (or a lagged
                    // receiver) is ignored; the loop re-selects.
                    if let Ok(ServerEvent::Progress { token, progress, total, message }) = event
                        && token == progress_token
                    {
                        forward_progress(chunk_tx.as_ref(), &mut last_progress, message.as_deref(), progress, total);
                    }
                }
                response = &mut response_fut => break response,
            }
        };

        match response.map_err(|e| map_service_error(e, slug))? {
            ServerResult::CallToolResult(result) => return Ok(convert_call_result(result)),
            ServerResult::InputRequiredResult(required) => {
                rounds += 1;
                if rounds >= MAX_MRTR_ROUNDS {
                    return Err(McpError::ProtocolError(format!(
                        "server kept requesting client input for tool {name:?} after {rounds} round(s); \
                         this client cannot supply it"
                    )));
                }
                input_responses = Some(decline_responses(required.input_requests.as_ref())?);
                // Echo the opaque state verbatim; it is required on retry and
                // must not be inspected.
                request_state.clone_from(&required.request_state);
            }
            // The tasks extension is not driven; surface it rather than hang.
            other => {
                return Err(McpError::ProtocolError(format!(
                    "tools/call returned a result this client does not handle: {other:?}"
                )));
            }
        }
    }
}

/// Forward one progress notification to the caller's chunk sink, rate-limited.
///
/// A message is preferred; failing that a compact numeric indicator is emitted
/// when the server knows the total. Sends are best-effort (`try_send`): a full
/// or dropped sink never blocks the call.
fn forward_progress(
    chunk_tx: Option<&crossbeam_channel::Sender<Vec<u8>>>,
    last_progress: &mut Option<Instant>,
    message: Option<&str>,
    progress: f64,
    total: Option<f64>,
) {
    let Some(tx) = chunk_tx else {
        return;
    };
    if let Some(last) = *last_progress
        && last.elapsed() < PROGRESS_MIN_INTERVAL
    {
        return;
    }
    let text = match message.filter(|m| !m.is_empty()) {
        Some(message) => format!("{message}\n"),
        None => match total {
            Some(total) if total > 0.0 => format!("[progress {progress:.0}/{total:.0}]\n"),
            _ => return,
        },
    };
    if tx.try_send(text.into_bytes()).is_ok() {
        *last_progress = Some(Instant::now());
    }
}

/// Build a decline response for every server input request in an
/// `input_required` result.
///
/// Only elicitation and roots can be answered with a well-formed decline here;
/// sampling (`sampling/createMessage`) would require this client to invoke a
/// model, so its presence is reported as an unsupported protocol exchange
/// rather than answered with fabricated content.
fn decline_responses(input_requests: Option<&InputRequests>) -> Result<InputResponses, McpError> {
    let mut responses = InputResponses::new();
    let Some(requests) = input_requests else {
        // A state-only `input_required` (load shedding) just needs the state
        // echoed; an empty response map is a valid retry.
        return Ok(responses);
    };
    for (key, request) in requests {
        let value = match request {
            InputRequest::Elicitation(_) => {
                serde_json::to_value(ElicitResult::new(ElicitationAction::Decline))
            }
            // The roots result shape is `{"roots": []}`; built as a literal
            // rather than through the (deprecated) rmcp type only to keep the
            // decline here independent of that deprecation.
            InputRequest::ListRoots(_) => Ok(serde_json::json!({ "roots": [] })),
            InputRequest::CreateMessage(_) => {
                return Err(McpError::ProtocolError(
                    "server requested sampling (`sampling/createMessage`), which this client does \
                     not advertise or serve"
                        .into(),
                ));
            }
            other => {
                return Err(McpError::ProtocolError(format!(
                    "server requested unsupported client input: {other:?}"
                )));
            }
        }
        .map_err(|e| McpError::ProtocolError(format!("failed to encode input response: {e}")))?;
        responses.insert(key.clone(), value);
    }
    Ok(responses)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn decline_responses_declines_elicitation() {
        let elicitation = InputRequest::Elicitation(
            serde_json::from_value(serde_json::json!({
                "method": "elicitation/create",
                "params": {
                    "mode": "form",
                    "message": "Which environment?",
                    "requestedSchema": {"type": "object", "properties": {}}
                }
            }))
            .expect("elicitation request decodes"),
        );
        let mut requests = InputRequests::new();
        requests.insert("q1".to_string(), elicitation);
        let responses = decline_responses(Some(&requests)).expect("declines encode");
        assert_eq!(
            responses.get("q1"),
            Some(&serde_json::json!({"action": "decline"}))
        );
    }

    #[test]
    fn decline_responses_state_only_is_empty_map() {
        let responses = decline_responses(None).expect("no requests is fine");
        assert!(responses.is_empty());
    }

    #[test]
    fn forward_progress_rate_limits_and_formats() {
        let (tx, rx) = crossbeam_channel::unbounded();
        let mut last = None;
        forward_progress(Some(&tx), &mut last, Some("working"), 1.0, Some(2.0));
        // A second message inside the interval is dropped.
        forward_progress(Some(&tx), &mut last, Some("again"), 2.0, Some(2.0));
        let got: Vec<String> = rx
            .try_iter()
            .map(|b| String::from_utf8_lossy(&b).into_owned())
            .collect();
        assert_eq!(got.len(), 1, "rate-limited to one chunk: {got:?}");
        assert!(got[0].contains("working"));
    }

    #[test]
    fn forward_progress_numeric_fallback_and_no_sink() {
        let (tx, rx) = crossbeam_channel::unbounded();
        let mut last = None;
        // No message but a known total: a compact numeric indicator is emitted.
        forward_progress(Some(&tx), &mut last, None, 1.0, Some(4.0));
        let got: Vec<String> = rx
            .try_iter()
            .map(|b| String::from_utf8_lossy(&b).into_owned())
            .collect();
        assert_eq!(got.len(), 1);
        assert!(got[0].contains("1/4"));

        // No sink: nothing is sent and the rate-limit timestamp is untouched.
        let mut untouched = None;
        forward_progress(None, &mut untouched, Some("x"), 1.0, None);
        assert!(untouched.is_none());
    }
}
