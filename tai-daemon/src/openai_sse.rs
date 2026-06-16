use bytes::Bytes;
use futures_util::{StreamExt, stream::BoxStream};
use std::io;

pub(crate) struct SseReader {
    stream: BoxStream<'static, Result<Bytes, reqwest::Error>>,
    pending: Vec<u8>,
    event_lines: Vec<String>,
    finished: bool,
}

impl SseReader {
    pub(crate) fn new(response: reqwest::Response) -> Self {
        Self {
            stream: response.bytes_stream().boxed(),
            pending: Vec::new(),
            event_lines: Vec::new(),
            finished: false,
        }
    }

    pub(crate) async fn next_event(&mut self) -> io::Result<Option<String>> {
        if self.finished {
            return Ok(None);
        }

        loop {
            if let Some(event) = self.drain_complete_event()? {
                return Ok(Some(event));
            }

            match self.stream.next().await {
                Some(chunk) => {
                    let chunk = chunk.map_err(io::Error::other)?;
                    self.pending.extend_from_slice(&chunk);
                }
                None => {
                    if !self.pending.is_empty() {
                        let line = String::from_utf8(std::mem::take(&mut self.pending))
                            .map_err(|error| io::Error::new(io::ErrorKind::InvalidData, error))?;
                        self.event_lines
                            .push(line.trim_end_matches('\r').to_string());
                    }
                    self.finished = true;
                    return self.finish_event();
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

pub(crate) fn extract_responses_text_delta(data: &str) -> io::Result<Option<String>> {
    let payload: serde_json::Value = serde_json::from_str(data).map_err(io::Error::other)?;
    let Some(event_type) = payload.get("type").and_then(|value| value.as_str()) else {
        return Ok(None);
    };

    let delta = match event_type {
        "response.output_text.delta" => payload.get("delta").and_then(|value| value.as_str()),
        "response.output_text.done" => None,
        _ => None,
    };

    Ok(delta.map(str::to_string))
}
