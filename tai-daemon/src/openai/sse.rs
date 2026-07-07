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
