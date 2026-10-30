//! The format-agnostic SSE frame reader.
//!
//! [`SseReader`] yields one `data:` payload per event and leaves interpretation
//! to the caller: chat completions and the Responses API read entirely
//! different event vocabularies, so nothing here knows any of them.

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
