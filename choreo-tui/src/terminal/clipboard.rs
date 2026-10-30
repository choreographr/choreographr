//! Terminal-mediated clipboard writes: OSC 52 plus the richer OSC 5522.
//!
//! The TUI copies selected text to the system clipboard through the terminal
//! emulator itself.  The terminal mediates the write, so:
//!
//! - It works over SSH and tmux — the clipboard written is the *local* one,
//!   exactly like terminal-native selection (a system-clipboard crate would
//!   write to the *remote* host's clipboard instead).
//! - It is a no-op on terminals that do not support a sequence: an unknown OSC
//!   is silently ignored, never an error.
//! - The terminal can refuse the write (kitty/iTerm2 permission prompts)
//!   without affecting the TUI at all.
//!
//! # Two writes, no capability probe
//!
//! [`copy_to_clipboard`] emits **OSC 52 first, then OSC 5522**, concatenated
//! into a single `write()` (one stdout lock, one flush).  OSC 52 carries the
//! selection as plain text; the Kitty clipboard protocol (OSC 5522) carries the
//! same text as `text/plain` plus an HTML rendering as `text/html`, so a paste
//! into a rich destination keeps the layout.
//!
//! That order is what makes a capability probe unnecessary: a terminal that
//! implements OSC 5522 applies both writes and ends up holding the richer
//! payload (the later write replaces the clipboard contents), while a terminal
//! that does not implement it ignores the unknown sequence entirely and keeps
//! the plain-text OSC 52 result.  Neither outcome needs the TUI to know which
//! kind of terminal it is talking to — which is only possible because the
//! OSC 5522 *write* half needs no reply.
//!
//! OSC 52 is the one piece that cannot be chunked: its whole payload rides in a
//! single OSC sequence, and many terminals truncate or drop an oversized one
//! (a limit well below the client's own size cap).  So the plain fallback is
//! only emitted for a selection within [`MAX_OSC52_BYTES`]; above it the copy
//! relies solely on the chunked OSC 5522 write, which has no per-sequence limit.
//!
//! # Never expect a reply
//!
//! A hard rule for this module: never emit a sequence that expects a reply — no
//! `type=read`, no `type=walias` follow-up that needs a handshake.  crossterm's
//! input parser parses an OSC *body* as individual keystrokes, so a reply
//! nobody consumed would be typed into the user's prompt instead of the
//! application.  The `type=write:status=DONE` reply (and its error packets) is
//! therefore deliberately not read; that half of the protocol is deferred until
//! the TUI owns the terminal's reply stream.
//!
//! # Why the size cap
//!
//! Selections larger than [`MAX_CLIPBOARD_BYTES`] are refused up front (the
//! caller surfaces a "too large" status).  The cap is **not** a terminal limit
//! any more: Ghostty accepts at least 64 MiB of clipboard data and splits it
//! into 4096-byte packets, which individually sit far below its 8 MiB
//! per-OSC-sequence cap.  It is a **UI-loop latency guard** — 1 MiB of text
//! becomes well over 4 MB of escape bytes (base64 of the plain text twice, once
//! per protocol, plus the escaped HTML rendering), all written between two
//! frames, so a bigger selection would visibly stall the loop.  The tighter
//! [`MAX_OSC52_BYTES`] bounds the un-chunked OSC 52 fallback on top of this.
//!
//! The sequences are framed and written through [`super::osc`] / [`super::write`]
//! exactly like the other OSC families.  A write is only ever triggered by a
//! user-initiated mouse-up over their own selection, so a hostile LLM/tool
//! output can never inject a clipboard write through this path.

use base64::Engine as _;

use super::{osc, write};

/// OSC code of the Kitty clipboard protocol.
const OSC_5522: u16 = 5522;

/// Maximum raw (pre-base64) payload bytes per OSC 5522 `wdata` packet.
///
/// The protocol requires every chunk of one MIME type to be at most 4096 bytes;
/// larger payloads are sent as consecutive packets in order.
const OSC5522_CHUNK_BYTES: usize = 4096;

/// Maximum size (in bytes of text) of a selection written to the clipboard.
const MAX_CLIPBOARD_BYTES: usize = 1 << 20; // 1 MiB of text

/// Maximum text bytes emitted as a single OSC 52 sequence.
///
/// OSC 52 has no chunking — one write carries the whole base64 payload as a
/// single OSC sequence — so a very large selection becomes one oversized
/// sequence that many terminals (and multiplexers such as tmux) truncate or drop
/// outright, well below [`MAX_CLIPBOARD_BYTES`].  Above this threshold the plain
/// OSC 52 fallback is skipped and the copy relies solely on the chunked OSC 5522
/// write, which has no per-sequence limit.  The value is a conservative bound on
/// what the common terminals accept in one OSC 52 write while still covering
/// ordinary selections.
const MAX_OSC52_BYTES: usize = 1 << 17; // 128 KiB of text

/// Whether `text` fits under the clipboard size cap.  Split out so the
/// threshold is unit-testable without writing to a real stdout.
fn within_clipboard_limit(text: &str) -> bool {
    text.len() <= MAX_CLIPBOARD_BYTES
}

/// Whether `text` fits the single-sequence OSC 52 fallback cap.
fn within_osc52_limit(text: &str) -> bool {
    text.len() <= MAX_OSC52_BYTES
}

/// Build the OSC 52 sequence that asks the terminal to place `text` on the
/// system clipboard (`c` selection).  Pure function so the byte layout is
/// unit-testable without a terminal.
pub(crate) fn build_osc52(text: &str) -> String {
    let encoded = base64::engine::general_purpose::STANDARD.encode(text.as_bytes());
    osc(52, &format!("c;{encoded}"))
}

/// Escape `text` for use as HTML character data.
///
/// Only the three characters that change meaning in HTML text are escaped;
/// quotes are deliberately left alone because the output is only ever placed
/// inside an element body, never in an attribute value.
fn html_escape(text: &str) -> String {
    let mut out = String::with_capacity(text.len());
    for c in text.chars() {
        match c {
            '&' => out.push_str("&amp;"),
            '<' => out.push_str("&lt;"),
            '>' => out.push_str("&gt;"),
            _ => out.push(c),
        }
    }
    out
}

/// Wrap `text` in a single `<pre>` element as the `text/html` clipboard flavor.
///
/// A rich destination (a document, a chat box, an issue tracker) collapses
/// whitespace in plain text, which destroys exactly what a terminal selection
/// carries: code blocks, table alignment and indentation.  `<pre>` is the
/// conservative encoding that preserves that layout verbatim, which is all the
/// renderer's output actually means.
///
/// Richer semantics — headings, emphasis, fenced code as real tags — would need
/// per-line block-kind metadata that the renderer does not track yet.  That is
/// a follow-up; `<pre>` is the honest ceiling of what a plain text selection can
/// claim.
fn html_pre(text: &str) -> String {
    format!("<pre>{}</pre>", html_escape(text))
}

/// Split one payload into OSC 5522 chunks of at most [`OSC5522_CHUNK_BYTES`].
///
/// An empty payload yields exactly one empty chunk: `chunks()` yields nothing
/// for an empty slice, and the protocol has no other way to attach a
/// zero-length body to a MIME type, so dropping it would leave a declared type
/// unrepresented on the clipboard.
fn chunk_payload(bytes: &[u8]) -> Vec<&[u8]> {
    if bytes.is_empty() {
        vec![bytes]
    } else {
        bytes.chunks(OSC5522_CHUNK_BYTES).collect()
    }
}

/// Build the OSC 5522 write packets for an ordered list of `(mime, bytes)`
/// payloads.  Pure function so the byte layout is unit-testable.
///
/// The packet sequence is:
///
/// ```text
/// OSC 5522 ; type=write ST
/// OSC 5522 ; type=wdata:mime=<b64 mime>;<b64 chunk> ST   (one per chunk)
/// …                                                       (next MIME type)
/// OSC 5522 ; type=wdata ST                                (terminator)
/// ```
///
/// Every chunk of one MIME type is emitted before the next type starts, as the
/// protocol requires; the closing `type=wdata` packet marks the end of
/// transmission.  All payloads — chunk and MIME name — use standard base64 with
/// padding.
pub(crate) fn build_osc5522(payloads: &[(&str, &[u8])]) -> String {
    let mut seq = osc(OSC_5522, "type=write");
    for (mime, bytes) in payloads {
        // The MIME type travels base64-encoded too, so a `;` or `:` inside it
        // could never be mistaken for packet punctuation.
        let encoded_mime = base64::engine::general_purpose::STANDARD.encode(mime.as_bytes());
        for chunk in chunk_payload(bytes) {
            let encoded = base64::engine::general_purpose::STANDARD.encode(chunk);
            seq.push_str(&osc(
                OSC_5522,
                &format!("type=wdata:mime={encoded_mime};{encoded}"),
            ));
        }
    }
    seq.push_str(&osc(OSC_5522, "type=wdata"));
    seq
}

/// Build the full clipboard payload set for `text`: the plain text exactly as
/// selected, plus the same text HTML-escaped inside a single `<pre>` element.
///
/// The two flavors ride in one OSC 5522 write so a paste into a rich
/// destination keeps the selection's structure (see [`html_pre`]).
pub(crate) fn rich_payload(text: &str) -> Vec<(&'static str, Vec<u8>)> {
    vec![
        ("text/plain", text.as_bytes().to_vec()),
        ("text/html", html_pre(text).into_bytes()),
    ]
}

/// Borrow a [`rich_payload`] result as the `(&str, &[u8])` list
/// [`build_osc5522`] takes, so no caller repeats the re-mapping.
fn payload_slices<'a>(payload: &'a [(&'static str, Vec<u8>)]) -> Vec<(&'a str, &'a [u8])> {
    payload
        .iter()
        .map(|(mime, bytes)| (*mime, bytes.as_slice()))
        .collect()
}

/// Build the complete clipboard byte sequence for `text`, or `None` when the
/// text exceeds [`MAX_CLIPBOARD_BYTES`].
///
/// Both protocols go out in one string so `copy_to_clipboard` can emit them in a
/// single `write()`: one lock, one flush, so the plain-text and rich writes land
/// back to back rather than straddling a frame boundary.  Order matters — a
/// 5522-capable terminal ends up with the richer payload because the later write
/// replaces the clipboard contents.  The plain OSC 52 fallback is included only
/// up to [`MAX_OSC52_BYTES`], since that sequence cannot be chunked.
fn build_clipboard_sequences(text: &str) -> Option<String> {
    if !within_clipboard_limit(text) {
        return None;
    }
    let rich = rich_payload(text);
    let mut seq = String::new();
    if within_osc52_limit(text) {
        seq.push_str(&build_osc52(text));
    }
    seq.push_str(&build_osc5522(&payload_slices(&rich)));
    Some(seq)
}

/// Copy `text` to the system clipboard: OSC 52 (plain text, when within
/// [`MAX_OSC52_BYTES`]) followed by OSC 5522 (plain text + HTML) in one write.
///
/// Returns `true` when the sequences were written; `false` (without writing)
/// when the text exceeds [`MAX_CLIPBOARD_BYTES`], so the caller can report a
/// "too large to copy" status instead of pretending the copy succeeded.
/// Errors are swallowed on purpose: an unsupported or denying terminal must
/// degrade to "nothing happened", never disturb the UI loop.
pub(crate) fn copy_to_clipboard(text: &str) -> bool {
    let Some(seq) = build_clipboard_sequences(text) else {
        return false;
    };
    // Byte counts only — a selection is user content and never gets logged.  The
    // `osc52` flag records whether the plain fallback rode along (it is skipped
    // above `MAX_OSC52_BYTES`).
    tracing::debug!(
        text_bytes = text.len(),
        sequence_bytes = seq.len(),
        osc52 = within_osc52_limit(text),
        "[choreo-tui] writing clipboard via OSC 52 + OSC 5522"
    );
    write(&seq);
    true
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The OSC 5522 prologue every write sequence opens with.
    const WRITE: &str = "\x1b]5522;type=write\x1b\\";
    /// The `type=wdata` packet that closes the transmission.
    const END: &str = "\x1b]5522;type=wdata\x1b\\";

    /// Base64 of `text/plain` and `text/html`, the two flavors emitted.
    const MIME_PLAIN: &str = "dGV4dC9wbGFpbg==";
    const MIME_HTML: &str = "dGV4dC9odG1s";

    /// One `wdata` packet for `chunk` bytes under the `text/plain` MIME type.
    fn plain_packet(chunk: &[u8]) -> String {
        let encoded = base64::engine::general_purpose::STANDARD.encode(chunk);
        format!("\x1b]5522;type=wdata:mime={MIME_PLAIN};{encoded}\x1b\\")
    }

    #[test]
    fn build_osc52_empty_text() {
        // An empty selection is never copied (the caller gates on non-empty),
        // but the encoder must still produce a well-formed sequence.
        assert_eq!(build_osc52(""), "\x1b]52;c;\x1b\\");
    }

    #[test]
    fn build_osc52_base64_payload() {
        // "hello" base64-encodes to "aGVsbG8="; the sequence wraps it in the
        // OSC 52 clipboard write form with ST (ESC \) terminators.
        assert_eq!(build_osc52("hello"), "\x1b]52;c;aGVsbG8=\x1b\\");
    }

    #[test]
    fn build_osc52_multiline_text() {
        // Embedded newlines pass through the base64 encoding untouched.
        assert_eq!(
            build_osc52("line one\nline two"),
            "\x1b]52;c;bGluZSBvbmUKbGluZSB0d28=\x1b\\"
        );
    }

    #[test]
    fn build_osc52_utf8_round_trips() {
        // Non-ASCII (e.g. code snippets with emoji) survives the encode.
        let seq = build_osc52("→ 😀");
        assert!(seq.starts_with("\x1b]52;c;"));
        assert!(seq.ends_with("\x1b\\"));
        // Prefix is ESC ] 5 2 ; c ; = 7 bytes; suffix ST = 2 bytes.  The
        // sequence was just built by `build_osc52`, so the strip always
        // succeeds; `.get()` keeps the slice total for the fixed indices.
        let b64 = seq.get(7..seq.len().saturating_sub(2)).unwrap_or("");
        let decoded = base64::engine::general_purpose::STANDARD
            .decode(b64)
            .expect("payload must be valid base64");
        assert_eq!(String::from_utf8(decoded).unwrap(), "→ 😀");
    }

    #[test]
    fn osc5522_empty_text_layout() {
        // Empty text: `text/plain` is still represented by one empty chunk
        // (a MIME type with no packets would not arrive at all), while the HTML
        // flavor carries the `<pre></pre>` wrapper around nothing.
        let payload = rich_payload("");
        let payloads = payload_slices(&payload);
        let html = base64::engine::general_purpose::STANDARD.encode(b"<pre></pre>");
        assert_eq!(
            build_osc5522(&payloads),
            format!(
                "{WRITE}{}\x1b]5522;type=wdata:mime={MIME_HTML};{html}\x1b\\{END}",
                plain_packet(b"")
            )
        );
    }

    #[test]
    fn osc5522_small_text_layout() {
        // "hi" as text/plain, and "<pre>hi</pre>" as text/html — every packet
        // exact, in the documented order, ending with the terminator.
        let payload = rich_payload("hi");
        let payloads = payload_slices(&payload);
        let html = base64::engine::general_purpose::STANDARD.encode(b"<pre>hi</pre>");
        assert_eq!(
            build_osc5522(&payloads),
            format!(
                "{WRITE}{}\x1b]5522;type=wdata:mime={MIME_HTML};{html}\x1b\\{END}",
                plain_packet(b"hi")
            )
        );
    }

    #[test]
    fn osc5522_mime_names_are_base64() {
        // The MIME type is base64 of the raw string, so the sequence parser
        // never sees a bare `;` or `=` in that field.
        assert_eq!(
            base64::engine::general_purpose::STANDARD.encode(b"text/plain"),
            MIME_PLAIN
        );
        assert_eq!(
            base64::engine::general_purpose::STANDARD.encode(b"text/html"),
            MIME_HTML
        );
        // A payload set built from arbitrary MIME names encodes each of them.
        let seq = build_osc5522(&[("application/json", b"{}")]);
        let mime = base64::engine::general_purpose::STANDARD.encode(b"application/json");
        assert!(seq.contains(&format!("type=wdata:mime={mime};e30=")));
    }

    #[test]
    fn osc5522_chunk_boundary() {
        // Exactly one chunk's worth of bytes fits in one packet; one byte more
        // spills into a second packet, so the raw size cap is never exceeded.
        let exact = vec![b'x'; OSC5522_CHUNK_BYTES];
        let seq = build_osc5522(&[("text/plain", &exact)]);
        assert_eq!(seq.matches("type=wdata:mime=").count(), 1);

        let over = vec![b'x'; OSC5522_CHUNK_BYTES + 1];
        let seq = build_osc5522(&[("text/plain", &over)]);
        assert_eq!(seq.matches("type=wdata:mime=").count(), 2);
        // The first packet holds the full 4096 raw bytes, the second the
        // remaining single byte.
        assert!(seq.contains(&plain_packet(&vec![b'x'; OSC5522_CHUNK_BYTES])));
        assert!(seq.contains(&plain_packet(b"x")));
    }

    #[test]
    fn osc5522_multi_chunk_ordering() {
        // Every `text/plain` packet precedes the first `text/html` packet: a
        // MIME type's chunks must be contiguous, or the terminal would
        // interleave them into a corrupt payload.
        let text = "y".repeat(OSC5522_CHUNK_BYTES + 10); // two chunks
        let payload = rich_payload(&text);
        let payloads = payload_slices(&payload);
        let seq = build_osc5522(&payloads);

        assert!(seq.starts_with(WRITE));
        assert!(seq.ends_with(END));
        let plain_mime = format!("type=wdata:mime={MIME_PLAIN};");
        let html_mime = format!("type=wdata:mime={MIME_HTML};");
        let last_plain = seq.rfind(&plain_mime).expect("plain packets present");
        let first_html = seq.find(&html_mime).expect("html packets present");
        assert!(
            last_plain < first_html,
            "all text/plain packets must precede the first text/html packet"
        );
        // Two plain chunks (4096 + 10 bytes).  The HTML flavor is the same text
        // plus the `<pre></pre>` wrapper, so it also spills into two chunks.
        assert_eq!(seq.matches(&plain_mime).count(), 2);
        assert_eq!(seq.matches(&html_mime).count(), 2);
    }

    #[test]
    fn html_pre_escapes_markup() {
        // `&`, `<` and `>` are escaped so pasted code cannot be reinterpreted
        // as markup; the wrapper itself stays literal tags.
        assert_eq!(html_pre("a & b"), "<pre>a &amp; b</pre>");
        assert_eq!(html_pre("<div>"), "<pre>&lt;div&gt;</pre>");
        assert_eq!(
            html_pre("if a < b && c > d {"),
            "<pre>if a &lt; b &amp;&amp; c &gt; d {</pre>"
        );
        // Ampersands in an existing entity are escaped too — the input is text,
        // never markup to preserve.
        assert_eq!(html_pre("&amp;"), "<pre>&amp;amp;</pre>");
    }

    #[test]
    fn rich_payload_carries_plain_text_verbatim() {
        // `text/plain` is exactly the selected bytes — the same content OSC 52
        // carries — so a plain destination is never degraded by the richer
        // write.
        let text = "  indented\n\ttabbed\n";
        let payload = rich_payload(text);
        assert_eq!(payload.len(), 2);
        assert_eq!(payload[0].0, "text/plain");
        assert_eq!(payload[0].1, text.as_bytes());
        assert_eq!(payload[1].0, "text/html");
        assert_eq!(payload[1].1, html_pre(text).into_bytes());
    }

    #[test]
    fn clipboard_size_cap_boundary() {
        // Text at the cap is accepted; one byte over is refused (the caller
        // shows a "too large" status instead of stalling the UI loop on
        // megabytes of escape bytes between two frames).
        assert!(within_clipboard_limit(""));
        assert!(within_clipboard_limit(
            "x".repeat(MAX_CLIPBOARD_BYTES).as_str()
        ));
        assert!(!within_clipboard_limit(
            "x".repeat(MAX_CLIPBOARD_BYTES + 1).as_str()
        ));
    }

    #[test]
    fn oversized_text_is_refused_without_writing() {
        // The `false` return is what tells the caller to show the "too large"
        // status; it must happen before any sequence is built or written.
        let oversized = "x".repeat(MAX_CLIPBOARD_BYTES + 1);
        assert!(!copy_to_clipboard(&oversized));
    }

    #[test]
    fn osc52_fallback_cap_boundary() {
        // The OSC 52 fallback has its own, tighter cap than the overall clipboard
        // limit: it is one un-chunked sequence, so it is dropped above this size.
        assert!(within_osc52_limit(""));
        assert!(within_osc52_limit("x".repeat(MAX_OSC52_BYTES).as_str()));
        assert!(!within_osc52_limit(
            "x".repeat(MAX_OSC52_BYTES + 1).as_str()
        ));
        // The fallback cap is strictly inside the overall clipboard cap.
        const { assert!(MAX_OSC52_BYTES < MAX_CLIPBOARD_BYTES) };
    }

    #[test]
    fn clipboard_sequences_include_osc52_within_its_cap() {
        // A small selection gets both writes: the plain OSC 52 and the richer
        // chunked OSC 5522, with OSC 52 first so a 5522 terminal ends up richer.
        let seq = build_clipboard_sequences("hello").expect("within the size cap");
        let osc52 = build_osc52("hello");
        let osc52_at = seq.find(&osc52).expect("OSC 52 present");
        let osc5522_at = seq.find(WRITE).expect("OSC 5522 present");
        assert_eq!(osc52_at, 0, "OSC 52 must be written first");
        assert!(osc52_at < osc5522_at, "OSC 5522 follows OSC 52");
    }

    #[test]
    fn clipboard_sequences_skip_osc52_above_its_cap() {
        // Above the OSC 52 cap the un-chunked fallback is dropped, but the copy
        // still goes out through the chunked OSC 5522 write.
        let text = "x".repeat(MAX_OSC52_BYTES + 1);
        let seq = build_clipboard_sequences(&text).expect("still within the overall cap");
        assert!(
            !seq.contains("\x1b]52;"),
            "OSC 52 must be omitted above its cap"
        );
        assert!(seq.starts_with(WRITE), "the OSC 5522 write still goes out");
    }

    #[test]
    fn clipboard_sequences_none_above_the_overall_cap() {
        // Past the overall cap nothing is built at all, so no sequence is written.
        let oversized = "x".repeat(MAX_CLIPBOARD_BYTES + 1);
        assert!(build_clipboard_sequences(&oversized).is_none());
    }
}
