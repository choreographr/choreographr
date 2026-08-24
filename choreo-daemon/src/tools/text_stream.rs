use super::error::ToolExecError;
use super::sanitize::sanitize_content;
use std::fs::File;
use std::io::{self, BufRead, BufReader};

/// Shared byte budget for tool output — owned by `choreo-sanitize` (see
/// `MAX_TOOL_OUTPUT_BYTES` there) and re-exported here so this crate's
/// modules and tests can refer to it as before.
pub(crate) const BINARY_SNIFF_BYTES: usize = 8 * 1024;

/// Per-line display cap for the file-read tools. Guards against pathological
/// single-line files (minified bundles, base64 blobs, 1 GiB one-liners) that
/// would otherwise force an unbounded line buffer into memory.
pub(crate) const MAX_LINE_DISPLAY_BYTES: usize = 64 * 1024;

/// Open `path` for streaming text reads, rejecting binary files up front.
///
/// The first [`BINARY_SNIFF_BYTES`] are *peeked* via `fill_buf` (not
/// consumed), so the returned reader can continue streaming from the start
/// of the file. A NUL byte in the head marks the file as binary. Invalid
/// UTF-8 in the head is also rejected, unless the invalid sequence is a
/// multi-byte char merely split at the sniff boundary (`error_len() == None`)
/// — the per-line UTF-8 validation in the read tools handles that case.
pub(crate) fn open_text_reader(path: &std::path::Path) -> Result<BufReader<File>, ToolExecError> {
    let file = File::open(path)?;
    let mut reader = BufReader::with_capacity(BINARY_SNIFF_BYTES, file);
    let head = reader.fill_buf()?;
    if let Some(pos) = head.iter().position(|&b| b == 0) {
        return Err(ToolExecError(format!(
            "'{}' appears to be a binary file (NUL byte at offset {pos}); \
             read_file/read_file_range are for UTF-8 text files",
            path.display()
        )));
    }
    if let Err(e) = std::str::from_utf8(head)
        && e.error_len().is_some()
    {
        return Err(ToolExecError(format!(
            "'{}' is not valid UTF-8 text (invalid byte sequence at offset {})",
            path.display(),
            e.valid_up_to()
        )));
    }
    Ok(reader)
}

/// Read one line (up to and including `\n`) into `buf`, stopping early once
/// `buf` reaches `cap` bytes.
///
/// Returns `Ok(true)` when the line is complete (terminated by `\n` or EOF)
/// and `Ok(false)` when the line is longer than `cap` — in that case `buf`
/// holds the first `cap` bytes (no trailing `\n`) and the caller should
/// drain the remainder with [`drain_rest_of_line`] before reading on.
/// Memory stays bounded: `buf` never grows past `cap`.
pub(crate) fn read_line_capped<R: BufRead>(
    reader: &mut R,
    buf: &mut Vec<u8>,
    cap: usize,
) -> io::Result<bool> {
    buf.clear();
    loop {
        // Scope the `fill_buf` borrow so `available` is dropped before
        // `consume` re-borrows the reader (BufRead requires this).
        let (consumed, done) = {
            let available = reader.fill_buf()?;
            if available.is_empty() {
                // EOF: a final partial line (possibly empty) counts as complete.
                return Ok(true);
            }
            let remaining = cap.saturating_sub(buf.len());
            if remaining == 0 {
                // Reached the display cap before finding a newline.
                return Ok(false);
            }
            let take = available.len().min(remaining);
            match available[..take].iter().position(|&b| b == b'\n') {
                Some(idx) => {
                    buf.extend_from_slice(&available[..=idx]);
                    (idx + 1, true)
                }
                None => {
                    buf.extend_from_slice(&available[..take]);
                    (take, false)
                }
            }
        };
        reader.consume(consumed);
        if done {
            return Ok(true);
        }
    }
}

/// Consume the remainder of an over-cap line (up to and including `\n`),
/// returning the number of bytes drained. Keeps line *counting* correct
/// after [`read_line_capped`] bailed out, without ever buffering the whole
/// line — a fixed chunk is reused, so memory stays O(1) in line size.
pub(crate) fn drain_rest_of_line<R: BufRead>(reader: &mut R) -> io::Result<u64> {
    let mut drained: u64 = 0;
    loop {
        let (consumed, done) = {
            let available = reader.fill_buf()?;
            if available.is_empty() {
                // EOF mid-line: no trailing newline to find.
                return Ok(drained);
            }
            match available.iter().position(|&b| b == b'\n') {
                Some(idx) => (idx + 1, true),
                None => (available.len(), false),
            }
        };
        drained += consumed as u64;
        reader.consume(consumed);
        if done {
            return Ok(drained);
        }
    }
}

/// One line streamed from a text file by [`TextStream`]: the capped byte
/// content plus the byte accounting the read tools need for accurate error
/// offsets and totals.
pub(crate) struct StreamedLine {
    /// 1-based line number within the file.
    pub line_number: u64,
    /// First [`MAX_LINE_DISPLAY_BYTES`] bytes of the line. For over-cap
    /// lines this is a prefix without the trailing `\n` (see `complete`).
    pub content: Vec<u8>,
    /// `true` when the line was fully read (terminated by `\n` or EOF);
    /// `false` when the display cap cut the line short.
    pub complete: bool,
    /// Byte offset of this line's first byte within the file, used to
    /// report NUL / invalid-UTF-8 positions accurately.
    pub start_offset: u64,
}

/// Streaming, memory-bounded line iterator shared by the file-read tools.
///
/// Wraps the [`read_line_capped`] / [`drain_rest_of_line`] helpers so
/// `read_file` and `read_file_range` don't each re-implement the loop:
/// memory stays bounded at one capped line regardless of file size, over-cap
/// lines are drained (counted, never buffered) so byte totals stay exact,
/// and EOF is signalled by `None`.
pub(crate) struct TextStream<R: BufRead> {
    reader: R,
    line_buf: Vec<u8>,
    lines_read: u64,
    total_bytes: u64,
    finished: bool,
}

impl<R: BufRead> TextStream<R> {
    pub(crate) fn new(reader: R) -> Self {
        Self {
            reader,
            // Pre-size the line buffer to the display cap so long-line files
            // don't trigger repeated reallocations while growing.
            line_buf: Vec::with_capacity(MAX_LINE_DISPLAY_BYTES),
            lines_read: 0,
            total_bytes: 0,
            finished: false,
        }
    }

    /// Number of lines read so far (the file total once the iterator is
    /// exhausted). Over-cap lines count as one line each.
    pub(crate) fn total_lines(&self) -> u64 {
        self.lines_read
    }

    /// Total file bytes consumed so far — exact even for over-cap lines,
    /// whose tails are drained rather than buffered.
    pub(crate) fn total_bytes(&self) -> u64 {
        self.total_bytes
    }
}

impl<R: BufRead> Iterator for TextStream<R> {
    type Item = io::Result<StreamedLine>;

    fn next(&mut self) -> Option<Self::Item> {
        if self.finished {
            return None;
        }
        // `read_line_capped` clears the buffer on entry, so the previous
        // line's bytes never linger across iterations.
        let complete =
            match read_line_capped(&mut self.reader, &mut self.line_buf, MAX_LINE_DISPLAY_BYTES) {
                Ok(complete) => complete,
                Err(e) => {
                    self.finished = true;
                    return Some(Err(e));
                }
            };
        if self.line_buf.is_empty() {
            // EOF (empty file, or the trailing newline was already consumed):
            // there is no extra final line to report.
            self.finished = true;
            return None;
        }
        let start_offset = self.total_bytes;
        let line_total = if complete {
            self.line_buf.len() as u64
        } else {
            // Over-cap line: count its full length (draining keeps memory
            // bounded) but hand back only the capped prefix below.
            match drain_rest_of_line(&mut self.reader) {
                Ok(drained) => self.line_buf.len() as u64 + drained,
                Err(e) => {
                    self.finished = true;
                    return Some(Err(e));
                }
            }
        };
        self.total_bytes += line_total;
        self.lines_read += 1;
        Some(Ok(StreamedLine {
            line_number: self.lines_read,
            content: self.line_buf.clone(),
            complete,
            start_offset,
        }))
    }
}

/// Accumulates tool output line-by-line under the shared byte budget so a
/// single tool call can never flood the conversation with more than
/// [`MAX_TOOL_OUTPUT_BYTES`] of returned content.
///
/// Once the budget is exhausted the budget is marked truncated and further
/// pushes are rejected; the caller keeps counting lines/bytes for an honest
/// truncation report but stops validating content it will never return.
pub(crate) struct OutputBudget {
    max_bytes: usize,
    shown_bytes: usize,
    truncated: bool,
}

impl OutputBudget {
    pub(crate) fn new(max_bytes: usize) -> Self {
        Self {
            max_bytes,
            shown_bytes: 0,
            truncated: false,
        }
    }

    /// Bytes of content accepted so far (excluding anything rejected after
    /// truncation and the caller's trailing marker/header).
    pub(crate) fn shown_bytes(&self) -> usize {
        self.shown_bytes
    }

    pub(crate) fn is_truncated(&self) -> bool {
        self.truncated
    }

    /// Append `line` plus a trailing newline, honoring the budget. Returns
    /// `true` when the line fit and was appended; `false` — marking the
    /// output truncated — when the budget is exhausted.
    pub(crate) fn push_line(&mut self, out: &mut String, line: &str) -> bool {
        // +1 accounts for the newline re-appended below.
        let display_len = line.len() + 1;
        if self.truncated || self.shown_bytes + display_len > self.max_bytes {
            self.truncated = true;
            return false;
        }
        out.push_str(line);
        out.push('\n');
        self.shown_bytes += display_len;
        true
    }
}

/// Validate and render one streamed line for tool output.
///
/// Shared by `read_file` and `read_file_range`: rejects NUL bytes and
/// invalid UTF-8 in lines that are actually returned (reporting the byte
/// offset into the file), normalizes line endings to `str::lines()`
/// semantics (strip one `\n`, then one `\r`), and appends a
/// `...[line truncated]` marker when the display cap cut the line short.
/// With `numbered`, the line is prefixed with its 1-based file line number
/// (`read_file_range` rendering).
pub(crate) fn render_streamed_line(
    line: &StreamedLine,
    path: &std::path::Path,
    numbered: bool,
) -> Result<String, ToolExecError> {
    if let Some(pos) = line.content.iter().position(|&b| b == 0) {
        return Err(ToolExecError(format!(
            "'{}' appears to be a binary file (NUL byte at offset {})",
            path.display(),
            line.start_offset + pos as u64
        )));
    }
    let line_str = match std::str::from_utf8(&line.content) {
        Ok(s) => s,
        Err(e) if !line.complete && e.error_len().is_none() => {
            // The display cap split a multi-byte char mid-sequence; the
            // prefix before the split is valid and that is all we show.
            std::str::from_utf8(&line.content[..e.valid_up_to()]).unwrap_or_default()
        }
        Err(e) => {
            return Err(ToolExecError(format!(
                "'{}' is not valid UTF-8 text (invalid byte sequence at offset {})",
                path.display(),
                line.start_offset + e.valid_up_to() as u64
            )));
        }
    };

    // Match `str::lines()` display semantics: strip one trailing '\n' and
    // then one trailing '\r', then re-append a single '\n'.
    let mut display = line_str;
    if let Some(stripped) = display.strip_suffix('\n') {
        display = stripped;
    }
    if let Some(stripped) = display.strip_suffix('\r') {
        display = stripped;
    }
    // The line content is file-controlled (potentially hostile): escape
    // C0/C1 controls and Unicode format chars before the content enters the
    // tool transcript / TUI, so a file containing ESC or a bidi override
    // cannot inject terminal escape sequences or spoof the rendered output
    // (same policy as grep on matched lines). Only the *content* is
    // sanitized — the numbered prefix and truncation marker below are ASCII
    // and must pass through untouched.
    let display = sanitize_content(display);
    let mut display_line = String::new();
    if numbered {
        display_line.push_str(&format!("{} | {display}", line.line_number));
    } else {
        display_line.push_str(&display);
    }
    if !line.complete {
        display_line.push_str("\n...[line truncated: exceeds 64 KiB]");
    }
    Ok(display_line)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::path::Path;

    #[test]
    fn text_stream_counts_lines_bytes_and_offsets() {
        use std::io::Cursor;
        let mut stream = TextStream::new(Cursor::new(b"a\nbb\nccc\n".to_vec()));
        let lines: Vec<StreamedLine> = stream.by_ref().map(|l| l.unwrap()).collect();
        assert_eq!(lines.len(), 3);
        assert_eq!(lines[0].line_number, 1);
        assert_eq!(lines[0].content, b"a\n");
        assert_eq!(lines[0].start_offset, 0);
        assert_eq!(lines[1].line_number, 2);
        assert_eq!(lines[1].content, b"bb\n");
        assert_eq!(lines[1].start_offset, 2);
        assert_eq!(lines[2].line_number, 3);
        assert_eq!(lines[2].content, b"ccc\n");
        assert_eq!(lines[2].start_offset, 5);
        assert_eq!(stream.total_lines(), 3);
        assert_eq!(stream.total_bytes(), 9);
    }

    #[test]
    fn text_stream_handles_over_cap_lines() {
        use std::io::Cursor;
        // A single 70 KiB line exceeds the 64 KiB display cap: the iterator
        // hands back the capped prefix yet counts the full length.
        let content = vec![b'x'; 70 * 1024];
        let mut stream = TextStream::new(Cursor::new(content.clone()));
        let line = stream.next().unwrap().unwrap();
        assert!(!line.complete);
        assert_eq!(line.content.len(), MAX_LINE_DISPLAY_BYTES);
        assert_eq!(stream.total_bytes(), content.len() as u64);
        assert!(stream.next().is_none());
    }

    #[test]
    fn output_budget_rejects_lines_past_cap() {
        let mut out = String::new();
        let mut budget = OutputBudget::new(10);
        assert!(budget.push_line(&mut out, "abc")); // 4 bytes
        assert!(budget.push_line(&mut out, "def")); // 8 bytes
        assert!(!budget.push_line(&mut out, "ghi")); // 12 > 10 → truncated
        assert!(budget.is_truncated());
        assert_eq!(budget.shown_bytes(), 8);
        assert_eq!(out, "abc\ndef\n");
        // Pushes after truncation are rejected without growing the output.
        assert!(!budget.push_line(&mut out, "x"));
        assert_eq!(out, "abc\ndef\n");
    }

    #[test]
    fn render_streamed_line_rejects_binary_and_bad_utf8() {
        let path = Path::new("f.txt");
        let nul = StreamedLine {
            line_number: 1,
            content: b"ok\x00no".to_vec(),
            complete: true,
            start_offset: 0,
        };
        let err = render_streamed_line(&nul, path, false)
            .unwrap_err()
            .to_string();
        assert!(err.contains("binary file"), "{err}");

        let bad = StreamedLine {
            line_number: 2,
            content: b"ok\xff".to_vec(),
            complete: true,
            start_offset: 10,
        };
        let err = render_streamed_line(&bad, path, false)
            .unwrap_err()
            .to_string();
        assert!(err.contains("not valid UTF-8"), "{err}");
        // Offsets are reported relative to the file, not the line.
        assert!(err.contains("offset 12"), "{err}");
    }

    #[test]
    fn render_streamed_line_normalizes_endings_and_numbers() {
        let path = Path::new("f.txt");
        let line = StreamedLine {
            line_number: 3,
            content: b"hi\r\n".to_vec(),
            complete: true,
            start_offset: 0,
        };
        assert_eq!(render_streamed_line(&line, path, true).unwrap(), "3 | hi");
        assert_eq!(render_streamed_line(&line, path, false).unwrap(), "hi");
    }

    #[test]
    fn render_streamed_line_handles_mid_char_cap_cut() {
        // 3-byte chars where 65536 % 3 == 1 guarantee the cap cut lands
        // mid-character; the rendered line must stay valid UTF-8.
        let path = Path::new("f.txt");
        let content = "€".repeat(21846); // 65_538 bytes
        let line = StreamedLine {
            line_number: 1,
            content: content.into_bytes(),
            complete: false,
            start_offset: 0,
        };
        let out = render_streamed_line(&line, path, false).unwrap();
        assert!(out.contains("...[line truncated: exceeds 64 KiB]"), "{out}");
        std::str::from_utf8(out.as_bytes()).expect("output must be valid UTF-8");
    }

    #[test]
    fn render_streamed_line_escapes_esc_and_bidi() {
        // File content is hostile input: ESC must render as the inert 7-char
        // `\u{1b}` escape and U+202E (bidi override) as `\u{202e}`, so
        // neither can inject terminal escapes or spoof the transcript.
        let path = Path::new("f.txt");
        let esc = StreamedLine {
            line_number: 1,
            content: b"x\x1b[31mred".to_vec(),
            complete: true,
            start_offset: 0,
        };
        assert_eq!(
            render_streamed_line(&esc, path, false).unwrap(),
            "x\\u{1b}[31mred"
        );
        let bidi = StreamedLine {
            line_number: 1,
            content: "ok\u{202e}evil".as_bytes().to_vec(),
            complete: true,
            start_offset: 0,
        };
        assert_eq!(
            render_streamed_line(&bidi, path, false).unwrap(),
            "ok\\u{202e}evil"
        );
    }

    #[test]
    fn render_streamed_line_keeps_tabs_and_cjk() {
        // Tabs are legitimate source content and CJK passes through untouched
        // (both are safe: they cannot inject terminal escapes or split lines).
        let path = Path::new("f.txt");
        let tabbed = StreamedLine {
            line_number: 1,
            content: b"a\tb".to_vec(),
            complete: true,
            start_offset: 0,
        };
        assert_eq!(render_streamed_line(&tabbed, path, false).unwrap(), "a\tb");
        let cjk = StreamedLine {
            line_number: 1,
            content: "日本語".as_bytes().to_vec(),
            complete: true,
            start_offset: 0,
        };
        assert_eq!(render_streamed_line(&cjk, path, false).unwrap(), "日本語");
    }

    #[test]
    fn render_streamed_line_numbered_path_sanitizes_content_only() {
        // The numbered prefix "N | " must stay raw ASCII while the hostile
        // content after it is escaped.
        let path = Path::new("f.txt");
        let line = StreamedLine {
            line_number: 7,
            content: b"ok\x1b".to_vec(),
            complete: true,
            start_offset: 0,
        };
        assert_eq!(
            render_streamed_line(&line, path, true).unwrap(),
            "7 | ok\\u{1b}"
        );
    }
}
