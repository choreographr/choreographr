//! Server-sent-events parsing for the `OpenAI` adapters, plus the typed
//! Responses event parser.
//!
//! [`SseReader`] is deliberately format-agnostic: it yields one `data:` payload
//! per event and leaves interpretation to the caller (chat completions and the
//! Responses API read entirely different event vocabularies).

use std::io::{self, BufReader, Read};

/// Hard ceiling on one SSE event's accumulated bytes.
///
/// The reader sits in front of a remote peer that may be a provider, a gateway,
/// or a proxy in between, and an event is only delimited by a blank line. Without
/// a ceiling a peer that never emits one — or one that streams an absurdly long
/// line — would grow the line and event buffers without bound and let a single
/// connection exhaust the process's memory. The limit is far above any
/// legitimate event (the largest is a complete tool-argument string or an
/// encrypted reasoning item, both themselves capped downstream), so it only ever
/// trips on malformed or hostile input.
const MAX_SSE_EVENT_BYTES: usize = 8 * 1024 * 1024;

/// The error returned when one event exceeds [`MAX_SSE_EVENT_BYTES`].
fn event_too_large(observed: usize) -> io::Error {
    io::Error::new(
        io::ErrorKind::InvalidData,
        format!("sse event exceeds {MAX_SSE_EVENT_BYTES} bytes (saw {observed})"),
    )
}

pub(crate) struct SseReader {
    reader: BufReader<Box<dyn Read + Send>>,
    pending: Vec<u8>,
    /// Number of leading `pending` bytes already known to contain no newline.
    /// Resuming the scan here instead of at the start keeps one large event
    /// linear in its length rather than quadratic across 4 KiB reads.
    scanned: usize,
    event_lines: Vec<String>,
    /// Running byte total of `event_lines`, checked against the event ceiling
    /// and reset whenever the event is handed out.
    event_bytes: usize,
    finished: bool,
}

impl SseReader {
    /// Create an SSE reader from any `Read + Send` source (e.g. an HTTP
    /// response body).  Events are yielded incrementally as bytes arrive,
    /// enabling true line-level streaming without preloading the entire
    /// response into memory.
    pub(crate) fn from_reader(read: impl Read + Send + 'static) -> Self {
        Self {
            reader: BufReader::new(Box::new(read)),
            pending: Vec::new(),
            scanned: 0,
            event_lines: Vec::new(),
            event_bytes: 0,
            finished: false,
        }
    }

    pub(crate) fn next_event(&mut self) -> io::Result<Option<String>> {
        if self.finished {
            return Ok(None);
        }

        loop {
            if let Some(event) = self.drain_complete_event()? {
                return Ok(Some(event));
            }

            let mut buf = [0u8; 4096];
            match self.reader.read(&mut buf)? {
                0 => {
                    self.finished = true;
                    if !self.pending.is_empty() {
                        let line = String::from_utf8(std::mem::take(&mut self.pending))
                            .map_err(|error| io::Error::new(io::ErrorKind::InvalidData, error))?;
                        self.event_lines
                            .push(line.trim_end_matches('\r').to_string());
                    }
                    self.scanned = 0;
                    return Ok(self.finish_event());
                }
                n => {
                    self.pending
                        .extend_from_slice(crate::shared::read_slice(&buf, n));
                    // A peer that never terminates its line must not be able to
                    // grow the buffer without bound; `event_lines` is checked
                    // separately as lines are completed below.
                    if self.pending.len() > MAX_SSE_EVENT_BYTES {
                        return Err(event_too_large(self.pending.len()));
                    }
                }
            }
        }
    }

    fn drain_complete_event(&mut self) -> io::Result<Option<String>> {
        loop {
            // Only bytes appended since the last scan can hold the next newline,
            // so the search resumes at `scanned`. Rescanning the whole buffer on
            // every 4 KiB read would make one large event quadratic in its
            // length.
            let Some(offset) = self
                .pending
                .get(self.scanned..)
                .and_then(|unscanned| unscanned.iter().position(|byte| *byte == b'\n'))
            else {
                self.scanned = self.pending.len();
                return Ok(None);
            };
            let line_end = self.scanned + offset;
            let taken = line_end + 1;
            let mut line = self.pending.drain(..taken).collect::<Vec<_>>();
            // The remainder has never been scanned, so the search restarts at
            // its head.
            self.scanned = 0;
            if matches!(line.last(), Some(b'\n')) {
                line.pop();
            }
            if matches!(line.last(), Some(b'\r')) {
                line.pop();
            }

            if line.is_empty() {
                if let Some(event) = build_sse_event(&mut self.event_lines) {
                    self.event_bytes = 0;
                    if event == "[DONE]" {
                        self.finished = true;
                        return Ok(None);
                    }
                    return Ok(Some(event));
                }
                self.event_bytes = 0;
                continue;
            }

            let line = String::from_utf8(line)
                .map_err(|error| io::Error::new(io::ErrorKind::InvalidData, error))?;
            self.event_bytes = self.event_bytes.saturating_add(line.len());
            if self.event_bytes > MAX_SSE_EVENT_BYTES {
                return Err(event_too_large(self.event_bytes));
            }
            self.event_lines.push(line);
        }
    }

    fn finish_event(&mut self) -> Option<String> {
        let event = build_sse_event(&mut self.event_lines)?;
        self.event_bytes = 0;
        if event == "[DONE]" {
            return None;
        }
        Some(event)
    }
}

pub(crate) fn build_sse_event(event_lines: &mut Vec<String>) -> Option<String> {
    if event_lines.is_empty() {
        return None;
    }

    let data = event_lines
        .iter()
        .filter_map(|line| line.strip_prefix("data:"))
        .map(str::trim_start)
        .collect::<Vec<_>>()
        .join("\n");
    event_lines.clear();

    if data.is_empty() { None } else { Some(data) }
}

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
    /// Function call arguments delta
    FunctionCallArgumentsDelta { call_id: String, delta: String },
    /// Function call arguments are complete
    FunctionCallArgumentsDone {
        call_id: String,
        name: String,
        arguments: String,
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
        usage: Option<super::Usage>,
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
        usage: Option<super::Usage>,
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
fn response_usage(response: &serde_json::Value) -> Option<super::Usage> {
    response
        .get("usage")
        .and_then(|u| serde_json::from_value::<super::Usage>(u.clone()).ok())
}

/// Parse a single Responses API SSE event data string into a typed event.
///
/// Fields that the spec guarantees (e.g. `call_id`) are required — missing
/// them is an error rather than a silent default, because a missing
/// identifier would cause confusing downstream failures.
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
            // Reasoning items arrive as opaque output items. Capture the raw
            // item JSON (id, summary, and encrypted_content in stateless
            // mode) so the round-trip artifact can be assembled verbatim.
            // Other item types are ignored — their content flows through
            // dedicated events (output_text, function_call_arguments, ...).
            let item = payload.get("item").cloned();
            if let Some(item) = item {
                if item.get("type").and_then(|v| v.as_str()) == Some("reasoning") {
                    Ok(Some(ResponsesStreamEvent::ReasoningItem(item)))
                } else {
                    Ok(None)
                }
            } else {
                Ok(None)
            }
        }
        "response.function_call_arguments.delta" => {
            let call_id = payload
                .get("call_id")
                .and_then(|value| value.as_str())
                .map_or_default(std::string::ToString::to_string);
            let delta = payload
                .get("delta")
                .and_then(|value| value.as_str())
                .map_or_default(std::string::ToString::to_string);
            Ok(Some(ResponsesStreamEvent::FunctionCallArgumentsDelta {
                call_id,
                delta,
            }))
        }
        "response.function_call_arguments.done" => {
            let call_id = payload
                .get("call_id")
                .and_then(|value| value.as_str())
                .map_or_default(std::string::ToString::to_string);
            let name = payload
                .get("name")
                .and_then(|value| value.as_str())
                .map_or_default(std::string::ToString::to_string);
            let arguments = payload
                .get("arguments")
                .and_then(|value| value.as_str())
                .map_or_default(std::string::ToString::to_string);
            Ok(Some(ResponsesStreamEvent::FunctionCallArgumentsDone {
                call_id,
                name,
                arguments,
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
