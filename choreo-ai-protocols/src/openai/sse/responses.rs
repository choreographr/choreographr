//! The Responses API's SSE event vocabulary: one typed event per wire event the
//! adapter reads.
//!
//! [`parse_responses_stream_event`] is the whole mapping — every other item here
//! is a piece of it: the [`CallRef`] an argument event names its call with, the
//! readers for the wrapped `response` object the terminal events carry, and the
//! `function_call` item reader that lifts a call's identity off its output item.

use std::io;

use crate::CallerInfo;

/// All typed events that can appear in a Responses API SSE stream.
///
/// Every variant is consumed in `responses_request_streaming_with_tools`
/// to forward text/reasoning deltas to subscribers and to accumulate tool
/// call arguments across chunks.
#[derive(Debug, Clone)]
pub(crate) enum ResponsesStreamEvent {
    /// Text content delta
    TextDelta(String),
    /// Text output is complete
    TextDone,
    /// A complete opaque reasoning output item (`response.output_item.added`
    /// / `.done` with `type: "reasoning"`), exactly as received. Carries
    /// the item id, summary array and — in stateless mode —
    /// `encrypted_content`, all of which are captured into the round-trip
    /// artifact rather than interpreted here.
    ReasoningItem(serde_json::Value),
    /// A streamed `function_call` output item (`response.output_item.added` /
    /// `.done`) — the only place a streamed call's *name* and *call id* appear.
    ///
    /// The argument events name nothing but the item they belong to, so the
    /// call is assembled from both: the item brings the identity, the name and
    /// the `caller` attribution, and the argument events bring the arguments
    /// (the item carries its own copy, complete on `.done`). `arguments` is
    /// empty on `.added`. `caller` is `None` when the item carries no
    /// attribution (or one this client cannot read), which is legal — the
    /// field is optional on the wire and only meaningful for resuming a
    /// generated program.
    ///
    /// Two fields of the wire item are deliberately not modelled: `namespace`
    /// and `async`. Tools are registered by their bare function name, so a
    /// namespaced call resolves against the bare `name` it carries.
    FunctionCallItem {
        /// The item's own `id`, which every argument event of this call repeats
        /// as `item_id`.
        item_id: Option<String>,
        /// The provider's `call_id`, which the rebuilt request echoes on the
        /// `function_call` and on every `function_call_output` answering it.
        call_id: Option<String>,
        /// The function the model asked for.
        name: Option<String>,
        /// The item's own copy of the call's arguments JSON: empty on `.added`,
        /// complete on `.done`.
        arguments: String,
        /// The provider's attribution for the call (programmatic tool calling).
        caller: Option<CallerInfo>,
    },
    /// A chunk of a function call's arguments
    /// (`response.function_call_arguments.delta`).
    FunctionCallArgumentsDelta {
        /// The call the chunk belongs to (see [`CallRef`]).
        call: CallRef,
        delta: String,
    },
    /// Function call arguments are complete
    /// (`response.function_call_arguments.done`).
    FunctionCallArgumentsDone {
        /// The call the completed arguments belong to (see [`CallRef`]).
        call: CallRef,
        arguments: String,
        /// The function the model asked for, when the gateway put it on this
        /// event.
        ///
        /// The documented schema has no such field — the name belongs to the
        /// call's output item — but a lenient gateway that relays no output
        /// items may still send it, and without it such a call could never be
        /// run. It is a **fallback**: an item's name always wins.
        name: Option<String>,
    },
    /// Reasoning-summary text delta (`response.reasoning_summary_text.delta`).
    ///
    /// The summary is the only reasoning text a Responses model exposes, so
    /// this is what the session displays live. Part boundaries
    /// (`response.reasoning_summary_part.done`) carry no text of their own and
    /// are not modelled: parts concatenate in arrival order, which is the
    /// complete summary the API defines.
    ReasoningSummaryText(String),
    /// Response is complete with optional id and usage
    ResponseCompleted {
        id: Option<String>,
        usage: Option<super::super::Usage>,
    },
    /// Response failed with an error message
    ResponseFailed(String),
    /// The response stopped early, carrying the provider's reason when it sent
    /// one (`max_output_tokens`, `content_filter`, …).
    ///
    /// `incomplete` is a terminal SUCCESS whose output may be partial, not a
    /// transport failure: the id and usage are the same ones `completed`
    /// carries, and the reason decides whether the turn is a truncation (a
    /// length-capped answer, exactly like chat's `finish_reason: length`) or a
    /// terminal error.
    ResponseIncomplete {
        id: Option<String>,
        usage: Option<super::super::Usage>,
        reason: Option<String>,
    },
    /// Program code delta (streaming generated JavaScript)
    ProgramCodeDelta(String),
    /// Program code is complete
    ProgramCodeDone {
        call_id: String,
        fingerprint: Option<String>,
    },
    /// Program output is complete
    ProgramOutputDone {
        call_id: String,
        result: String,
        status: String,
    },
}

/// How a call-related stream event names the call it belongs to.
///
/// The documented `response.function_call_arguments.*` events carry neither the
/// function's name nor its `call_id`: they name the output item the arguments
/// are being streamed into (`item_id`), and the item is where the identity
/// lives. A gateway that relays no output items names the call id instead, so
/// that shape is read too.
#[derive(Debug, Clone)]
pub(crate) enum CallRef {
    /// The output item's id (`item_id`) — the documented join.
    Item(String),
    /// A call id, from a stream that sends no output items.
    Call(String),
}

impl CallRef {
    /// The call two optional wire ids name, preferring the output item (the
    /// documented join) over the call id.
    ///
    /// `None` means neither id was present: the event carries nothing that
    /// could be attributed to a call.
    pub(crate) fn of(item_id: Option<&str>, call_id: Option<&str>) -> Option<Self> {
        let present = |id: Option<&str>| id.filter(|id| !id.is_empty()).map(str::to_owned);
        present(item_id)
            .map(Self::Item)
            .or_else(|| present(call_id).map(Self::Call))
    }
}

/// A `function_call` output item, as the stream event the accumulator reads.
///
/// Every field is optional here on purpose: a nonconforming item must not fail
/// the parse of a stream the provider is already billing for. The accumulator
/// decides what an item with no usable identity means (nothing to attribute).
fn function_call_item_event(item: &serde_json::Value) -> ResponsesStreamEvent {
    let text = |key: &str| {
        item.get(key)
            .and_then(|value| value.as_str())
            .filter(|value| !value.is_empty())
            .map(str::to_owned)
    };
    ResponsesStreamEvent::FunctionCallItem {
        item_id: text("id"),
        call_id: text("call_id"),
        name: text("name"),
        arguments: text("arguments").unwrap_or_default(),
        caller: item.get("caller").and_then(caller_of),
    }
}

/// The attribution a `function_call` item's `caller` field carries.
///
/// `None` only when the value names no `type`. The attribution is never
/// interpreted here: the `type` tag and `caller_id` are lifted off the item so
/// the request can copy them back verbatim. A `direct` caller carries no
/// `caller_id`, which is why the id defaults to empty rather than failing the
/// parse — the API sends the bare `{"type": "direct"}` for a call the model
/// made itself.
fn caller_of(caller: &serde_json::Value) -> Option<CallerInfo> {
    let kind = caller.get("type").and_then(|kind| kind.as_str())?;
    Some(CallerInfo {
        kind: kind.to_string(),
        caller_id: caller
            .get("caller_id")
            .and_then(|id| id.as_str())
            .map_or_default(std::string::ToString::to_string),
    })
}

/// The call a `function_call_arguments.*` event belongs to, or `None` when the
/// event names no call at all (nothing to attribute, so the stream drops it).
fn call_ref(payload: &serde_json::Value) -> Option<CallRef> {
    let id = |key: &str| payload.get(key).and_then(|value| value.as_str());
    CallRef::of(id("item_id"), id("call_id"))
}

/// The `response` object a terminal event wraps.
///
/// `response.completed` / `.incomplete` / `.failed` carry the whole `Response`
/// object under a `response` key (see the streaming-events reference); a
/// handful of gateways flatten those fields onto the event root instead, so
/// every reader of the object goes through here and sees the other case for
/// free.
fn response_object(payload: &serde_json::Value) -> &serde_json::Value {
    payload.get("response").unwrap_or(payload)
}

/// The response id a terminal event reports, when it reports one.
fn response_id(response: &serde_json::Value) -> Option<String> {
    response
        .get("id")
        .and_then(|v| v.as_str())
        .map(String::from)
}

/// The response usage a terminal event reports.
///
/// If usage is present but unparseable, `None` rather than zeroed — the caller
/// can then distinguish "no usage data" from invalid data.
fn response_usage(response: &serde_json::Value) -> Option<super::super::Usage> {
    response
        .get("usage")
        .and_then(|u| serde_json::from_value::<super::super::Usage>(u.clone()).ok())
}

/// Parse a single Responses API SSE event data string into a typed event.
///
/// What is required differs by event, because the events differ in what they can
/// be attributed to:
///
/// - A **`function_call` output item** is read leniently: `id`, `call_id` and
///   `name` are all optional, because one nonconforming item must not fail the
///   parse of a stream the provider is already billing for. The accumulator
///   decides what an item with no usable identity means.
/// - The **`function_call_arguments.*`** events are attributed by `item_id`,
///   preferring it over `call_id` (see [`CallRef::of`]); `.done` additionally
///   reads `name` as a gateway fallback. An argument event naming neither id is
///   dropped as `Ok(None)` — there is nothing it could be attributed to.
/// - Some events genuinely cannot be attributed at all without an id, and there
///   the missing id stays a parse error rather than a silent default:
///   `response.program.code.done` and `response.program_output.done` require a
///   `call_id`, which is the only thing that makes the event mean anything.
///
/// Returns `Ok(None)` for unknown event types that should be silently ignored.
pub(crate) fn parse_responses_stream_event(data: &str) -> io::Result<Option<ResponsesStreamEvent>> {
    let payload: serde_json::Value = serde_json::from_str(data).map_err(io::Error::other)?;
    let Some(event_type) = payload.get("type").and_then(|value| value.as_str()) else {
        return Ok(None);
    };

    match event_type {
        "response.output_text.delta" => {
            let delta = payload
                .get("delta")
                .and_then(|value| value.as_str())
                .map(|s| ResponsesStreamEvent::TextDelta(s.to_string()));
            Ok(delta)
        }
        "response.output_text.done" => Ok(Some(ResponsesStreamEvent::TextDone)),
        "response.reasoning_summary_text.delta" => {
            let delta = payload
                .get("delta")
                .and_then(|value| value.as_str())
                .map(|s| ResponsesStreamEvent::ReasoningSummaryText(s.to_string()));
            Ok(delta)
        }
        // `response.reasoning_summary_part.added` / `.done` carry no text (the
        // parts concatenate in arrival order), so they fall through to the
        // unknown-event arm rather than inventing a separator the provider
        // never sent.
        "response.reasoning_summary_part.added" | "response.reasoning_summary_part.done" => {
            Ok(None)
        }
        "response.output_item.added" | "response.output_item.done" => {
            // Two item kinds are read here. A `reasoning` item arrives as an
            // opaque output item; the raw item JSON (id, summary, and
            // encrypted_content in stateless mode) is captured so the
            // round-trip artifact can be assembled verbatim. A
            // `function_call` item is the call itself: it names the function,
            // the `call_id` the request answers, and the provider's `caller`
            // attribution, which the replay copies back onto both the call and
            // the output that answers it (see
            // <https://developers.openai.com/api/docs/guides/tools-programmatic-tool-calling>).
            // The `function_call_arguments.*` events that carry the call's
            // arguments name nothing else, so this item is where a streamed
            // call is identified. Every other item type is ignored — its
            // content flows through dedicated events (output_text, …).
            let Some(item) = payload.get("item") else {
                return Ok(None);
            };
            match item.get("type").and_then(|v| v.as_str()) {
                Some("reasoning") => Ok(Some(ResponsesStreamEvent::ReasoningItem(item.clone()))),
                Some("function_call") => Ok(Some(function_call_item_event(item))),
                _ => Ok(None),
            }
        }
        "response.function_call_arguments.delta" => {
            // An event that names no call at all has nothing to attribute, so
            // it is dropped the same way an unknown event type is.
            let Some(call) = call_ref(&payload) else {
                return Ok(None);
            };
            let delta = payload
                .get("delta")
                .and_then(|value| value.as_str())
                .map_or_default(std::string::ToString::to_string);
            Ok(Some(ResponsesStreamEvent::FunctionCallArgumentsDelta {
                call,
                delta,
            }))
        }
        "response.function_call_arguments.done" => {
            let Some(call) = call_ref(&payload) else {
                return Ok(None);
            };
            // `arguments` is the complete final JSON string, replacing anything
            // the deltas accumulated.
            let arguments = payload
                .get("arguments")
                .and_then(|value| value.as_str())
                .map_or_default(std::string::ToString::to_string);
            Ok(Some(ResponsesStreamEvent::FunctionCallArgumentsDone {
                call,
                arguments,
                // Off-schema, and only a gateway fallback: the accumulator uses
                // it for a call no output item ever named.
                name: payload
                    .get("name")
                    .and_then(|value| value.as_str())
                    .filter(|value| !value.is_empty())
                    .map(str::to_owned),
            }))
        }
        "response.completed" => {
            let response = response_object(&payload);
            Ok(Some(ResponsesStreamEvent::ResponseCompleted {
                id: response_id(response),
                usage: response_usage(response),
            }))
        }
        "response.incomplete" => {
            let response = response_object(&payload);
            Ok(Some(ResponsesStreamEvent::ResponseIncomplete {
                id: response_id(response),
                usage: response_usage(response),
                // `incomplete_details.reason` is the only place the provider
                // says WHY it stopped; absent, the caller treats the partial
                // output as a truncation it cannot classify further.
                reason: response
                    .get("incomplete_details")
                    .and_then(|details| details.get("reason"))
                    .and_then(|value| value.as_str())
                    .map(String::from),
            }))
        }
        "response.failed" => {
            // `response.failed.error` is the OpenAI error object
            // `{"code", "message", "param", "type"}` — the
            // human-readable text lives in `message`.  Some providers or
            // emulators emit a plain string instead, so accept both; last
            // resort is the serialized error object so a mid-stream failure
            // is never silently blank.
            // The error object lives on the wrapped `response`; a flattened
            // event keeps it on the root, so both are read.
            let root = response_object(&payload);
            let error = root
                .get("error")
                .or_else(|| payload.get("error"))
                .map_or_default(|value| match value {
                    serde_json::Value::String(s) => s.clone(),
                    other => other
                        .get("message")
                        .and_then(|m| m.as_str())
                        .map_or_else(|| other.to_string(), String::from),
                });
            Ok(Some(ResponsesStreamEvent::ResponseFailed(error)))
        }
        "response.program.code.delta" => {
            let delta = payload
                .get("delta")
                .and_then(|value| value.as_str())
                .map(|s| ResponsesStreamEvent::ProgramCodeDelta(s.to_string()));
            Ok(delta)
        }
        "response.program.code.done" => {
            let call_id = payload
                .get("call_id")
                .and_then(|value| value.as_str())
                .map(std::string::ToString::to_string)
                .ok_or_else(|| {
                    io::Error::new(
                        io::ErrorKind::InvalidData,
                        "missing call_id in response.program.code.done",
                    )
                })?;
            let fingerprint = payload
                .get("fingerprint")
                .and_then(|value| value.as_str())
                .map(std::string::ToString::to_string);
            Ok(Some(ResponsesStreamEvent::ProgramCodeDone {
                call_id,
                fingerprint,
            }))
        }
        "response.program_output.done" => {
            let call_id = payload
                .get("call_id")
                .and_then(|value| value.as_str())
                .map(std::string::ToString::to_string)
                .ok_or_else(|| {
                    io::Error::new(
                        io::ErrorKind::InvalidData,
                        "missing call_id in response.program_output.done",
                    )
                })?;
            let result = payload
                .get("result")
                .and_then(|value| value.as_str())
                .map_or_default(std::string::ToString::to_string);
            let status = payload
                .get("status")
                .and_then(|value| value.as_str())
                .map_or_default(std::string::ToString::to_string);
            Ok(Some(ResponsesStreamEvent::ProgramOutputDone {
                call_id,
                result,
                status,
            }))
        }
        _ => {
            // Unknown event types are silently ignored
            Ok(None)
        }
    }
}

/// Extract text delta from a Responses API SSE event.
///
/// This is a convenience wrapper around [`parse_responses_stream_event`]
/// that only returns text deltas, retained for backward compatibility
/// in tests.
#[cfg(test)]
pub(crate) fn extract_responses_text_delta(data: &str) -> io::Result<Option<String>> {
    let event = parse_responses_stream_event(data)?;
    match event {
        Some(ResponsesStreamEvent::TextDelta(text)) => Ok(Some(text)),
        _ => Ok(None),
    }
}
