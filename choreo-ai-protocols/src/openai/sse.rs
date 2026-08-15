use std::io::{self, BufReader, Read};

pub(crate) struct SseReader {
    reader: BufReader<Box<dyn Read + Send>>,
    pending: Vec<u8>,
    event_lines: Vec<String>,
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
            event_lines: Vec::new(),
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
                    return self.finish_event();
                }
                n => {
                    self.pending.extend_from_slice(&buf[..n]);
                }
            }
        }
    }

    fn drain_complete_event(&mut self) -> io::Result<Option<String>> {
        while let Some(line_end) = self.pending.iter().position(|byte| *byte == b'\n') {
            let mut line = self.pending.drain(..=line_end).collect::<Vec<_>>();
            if matches!(line.last(), Some(b'\n')) {
                line.pop();
            }
            if matches!(line.last(), Some(b'\r')) {
                line.pop();
            }

            if line.is_empty() {
                if let Some(event) = build_sse_event(&mut self.event_lines) {
                    if event == "[DONE]" {
                        self.finished = true;
                        return Ok(None);
                    }
                    return Ok(Some(event));
                }
                continue;
            }

            let line = String::from_utf8(line)
                .map_err(|error| io::Error::new(io::ErrorKind::InvalidData, error))?;
            self.event_lines.push(line);
        }

        Ok(None)
    }

    fn finish_event(&mut self) -> io::Result<Option<String>> {
        let Some(event) = build_sse_event(&mut self.event_lines) else {
            return Ok(None);
        };
        if event == "[DONE]" {
            return Ok(None);
        }
        Ok(Some(event))
    }
}

pub(crate) fn build_sse_event(event_lines: &mut Vec<String>) -> Option<String> {
    if event_lines.is_empty() {
        return None;
    }

    let data = event_lines
        .iter()
        .filter_map(|line| line.strip_prefix("data:"))
        .map(|value| value.trim_start())
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
    /// Reasoning summary
    ReasoningSummary(Vec<serde_json::Value>),
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
    /// Response is complete with optional id and usage
    ResponseCompleted {
        id: Option<String>,
        usage: Option<super::Usage>,
    },
    /// Response failed with an error message
    ResponseFailed(String),
    /// Response incomplete
    ResponseIncomplete,
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
        "response.reasoning.summary" => {
            let summary = payload
                .get("summary")
                .and_then(|value| value.as_array())
                .cloned()
                .unwrap_or_default();
            Ok(Some(ResponsesStreamEvent::ReasoningSummary(summary)))
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
                .map(|s| s.to_string())
                .unwrap_or_default();
            let delta = payload
                .get("delta")
                .and_then(|value| value.as_str())
                .map(|s| s.to_string())
                .unwrap_or_default();
            Ok(Some(ResponsesStreamEvent::FunctionCallArgumentsDelta {
                call_id,
                delta,
            }))
        }
        "response.function_call_arguments.done" => {
            let call_id = payload
                .get("call_id")
                .and_then(|value| value.as_str())
                .map(|s| s.to_string())
                .unwrap_or_default();
            let name = payload
                .get("name")
                .and_then(|value| value.as_str())
                .map(|s| s.to_string())
                .unwrap_or_default();
            let arguments = payload
                .get("arguments")
                .and_then(|value| value.as_str())
                .map(|s| s.to_string())
                .unwrap_or_default();
            Ok(Some(ResponsesStreamEvent::FunctionCallArgumentsDone {
                call_id,
                name,
                arguments,
            }))
        }
        "response.completed" => {
            let id = payload.get("id").and_then(|v| v.as_str()).map(String::from);
            // If usage is present but unparseable, return None rather than
            // silently zeroing it — the caller can distinguish "no usage data"
            // from invalid data.
            let usage = payload
                .get("usage")
                .and_then(|u| serde_json::from_value::<super::Usage>(u.clone()).ok());
            Ok(Some(ResponsesStreamEvent::ResponseCompleted { id, usage }))
        }
        "response.failed" => {
            // `response.failed.error` is the OpenAI error object
            // `{"code", "message", "param", "type"}` — the
            // human-readable text lives in `message`.  Some providers or
            // emulators emit a plain string instead, so accept both; last
            // resort is the serialized error object so a mid-stream failure
            // is never silently blank.
            let error = payload
                .get("error")
                .map(|value| match value {
                    serde_json::Value::String(s) => s.clone(),
                    other => other
                        .get("message")
                        .and_then(|m| m.as_str())
                        .map(String::from)
                        .unwrap_or_else(|| other.to_string()),
                })
                .unwrap_or_default();
            Ok(Some(ResponsesStreamEvent::ResponseFailed(error)))
        }
        "response.incomplete" => Ok(Some(ResponsesStreamEvent::ResponseIncomplete)),
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
                .map(|s| s.to_string())
                .ok_or_else(|| {
                    io::Error::new(
                        io::ErrorKind::InvalidData,
                        "missing call_id in response.program.code.done",
                    )
                })?;
            let fingerprint = payload
                .get("fingerprint")
                .and_then(|value| value.as_str())
                .map(|s| s.to_string());
            Ok(Some(ResponsesStreamEvent::ProgramCodeDone {
                call_id,
                fingerprint,
            }))
        }
        "response.program_output.done" => {
            let call_id = payload
                .get("call_id")
                .and_then(|value| value.as_str())
                .map(|s| s.to_string())
                .ok_or_else(|| {
                    io::Error::new(
                        io::ErrorKind::InvalidData,
                        "missing call_id in response.program_output.done",
                    )
                })?;
            let result = payload
                .get("result")
                .and_then(|value| value.as_str())
                .map(|s| s.to_string())
                .unwrap_or_default();
            let status = payload
                .get("status")
                .and_then(|value| value.as_str())
                .map(|s| s.to_string())
                .unwrap_or_default();
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
