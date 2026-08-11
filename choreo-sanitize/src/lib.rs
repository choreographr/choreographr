//! Shared string-safety primitives for tool output.
//!
//! This leaf crate is the single source of truth for two things every
//! consumer of tool output needs to agree on:
//!
//! - **The Unicode spoofing predicates** — which characters may reorder,
//!   hide, or fake text in rendered output and must therefore be escaped.
//!   [`is_non_joiner_format_char`] covers the invisible format-char class
//!   (bidi marks/overrides/isolates, ZWSP, invisible operators, …) used by
//!   the LLM-transcript sanitizer; [`is_unsafe_unicode`] adds the
//!   line/paragraph separators and is used by the line-oriented sanitizers
//!   and the TUI's terminal sink filter. The set comes from the Unicode
//!   data tables via `unicode-general-category`, so newly-assigned format
//!   characters are escaped automatically on a crate bump instead of
//!   drifting until someone re-reads the tables.
//! - **The tool-output byte budget** — [`MAX_TOOL_OUTPUT_BYTES`] and the
//!   shared `...[truncated]` marker, with [`truncate_tool_output`] /
//!   [`finish_tool_output`] applying the cap and [`ByteBudget`] tracking it
//!   incrementally on streaming paths. Keeping the budget and its marker in
//!   one place means the daemon's final record, the daemon's streamed live
//!   view, and the client's live accumulation all read identically.
//!
//! Consumers: `choreo-daemon` (sanitizers + streaming caps), `choreo-tui`
//! (terminal render filter), `choreo-blockchain` (node-output sanitizer),
//! `choreo-client-core` (live streaming cap).

use unicode_general_category::{GeneralCategory, get_general_category};

/// Shared byte budget for tool output (128 KiB ≈ ~32K tokens for ASCII,
/// ~43K for CJK — far below any modern context window, yet a single call
/// can never flood the conversation). Measured in *bytes* rather than chars
/// so the effective token cost is roughly uniform across scripts: ASCII and
/// CJK both sit at ~3-4 bytes per token, whereas char counts vary 4x.
pub const MAX_TOOL_OUTPUT_BYTES: usize = 128 * 1024;

/// Marker appended when tool output is cut at [`MAX_TOOL_OUTPUT_BYTES`] (or a
/// streaming path hits its byte cap). Shared so the daemon's final record,
/// the daemon's streamed live view, and the client's live accumulation all
/// show the same truncation signal. Does not include the leading newline —
/// use [`TRUNCATION_SUFFIX`] when one is wanted (every current caller
/// appends it on its own line).
pub const TRUNCATION_MARKER: &str = "...[truncated]";

/// [`TRUNCATION_MARKER`] with the leading newline every consumer appends it
/// with (`truncate_tool_output`, the streaming marker chunks, the client's
/// live accumulation) — a single const so the exact bytes never drift. Kept
/// as its own literal (not `concat!`) because `concat!` requires literals,
/// not consts; a test pins the two in agreement.
pub const TRUNCATION_SUFFIX: &str = "\n...[truncated]";

/// Whether `c` is a Unicode *format* character (general category Cf) other
/// than the joiners U+200C/U+200D.
///
/// The Cf class is invisible but spoofing-capable: bidi marks/overrides/
/// isolates, ZWSP and word joiner, invisible operators, the BOM, soft
/// hyphen, Mongolian vowel separator, tags, and the rarer format controls.
/// They can hide text in identifiers, split tokens, or reorder rendered
/// output — the exact threat the LLM-transcript sanitizer (`sanitize_transcript`
/// in choreo-daemon) defends against, so it escapes exactly this class.
///
/// The joiners are deliberately excluded: they affect ligation, not ordering
/// or visibility, and are required by scripts like Persian (ZWNJ) and
/// Devanagari (ZWJ/ZWNJ conjuncts), so escaping them would mangle
/// legitimate text for no safety gain.
pub fn is_non_joiner_format_char(c: char) -> bool {
    get_general_category(c) == GeneralCategory::Format && !matches!(c, '\u{200c}' | '\u{200d}')
}

/// Whether `c` must be escaped in rendered output: the line / paragraph
/// separators U+2028/U+2029 (not `is_control`, yet terminals render them as
/// line breaks, breaking the one-line-per-result invariant) plus every
/// non-joiner format character (see [`is_non_joiner_format_char`]).
///
/// Shared by every sanitizer that must keep untrusted text from reordering,
/// hiding, or faking what the model or the user sees: the daemon's
/// line-oriented sanitizers (`grep`/`find`/`read_file`/`http_request`),
/// the TUI's terminal sink filter, and the blockchain tools' node-output
/// sanitizer.
pub fn is_unsafe_unicode(c: char) -> bool {
    matches!(c, '\u{2028}' | '\u{2029}') || is_non_joiner_format_char(c)
}

/// Cap `content` at `cap` bytes, appending [`TRUNCATION_SUFFIX`] when the cap
/// is hit. Cuts on a char boundary so a multi-byte UTF-8 char is never split.
/// Shared by [`truncate_tool_output`] and [`finish_tool_output`] (the latter
/// caps at a smaller budget to leave room for its tail).
fn truncate_tool_output_at(content: &str, cap: usize) -> String {
    if content.len() <= cap {
        return content.to_string();
    }
    // Cut on a char boundary so we never split a multi-byte UTF-8 char.
    let split = content.floor_char_boundary(cap);
    let mut truncated = content[..split].to_string();
    truncated.push_str(TRUNCATION_SUFFIX);
    truncated
}

/// Cap `content` at [`MAX_TOOL_OUTPUT_BYTES`], appending
/// [`TRUNCATION_SUFFIX`] when the cap is hit. Cuts on a char boundary so a
/// multi-byte UTF-8 char is never split.
pub fn truncate_tool_output(content: &str) -> String {
    truncate_tool_output_at(content, MAX_TOOL_OUTPUT_BYTES)
}

/// Cap `body` at the shared byte budget, then append `marker` (if any).
/// The marker is short and critical ("N of many more"), so room for it is
/// reserved *inside* the budget: `body` is capped at [`MAX_TOOL_OUTPUT_BYTES`]
/// minus the space the tail (and the generic truncation marker, when the
/// body itself must be cut) needs, so `body + tail` never exceeds the
/// budget.
///
/// Keeping the tail within the hard budget — rather than riding *past* it —
/// lets it survive the transcript re-cap in `record_tool_completion`, which
/// re-applies the byte cap after `sanitize_transcript` (escaping expands a
/// Cf char into `\u{202e}`). A tail appended past the cap would be cut off
/// there; a tail kept inside the budget passes through untouched.
pub fn finish_tool_output(body: &str, marker: Option<String>) -> String {
    let Some(marker) = marker else {
        return truncate_tool_output(body);
    };
    let tail = format!("\n{marker}");
    // Reserve room for the tail plus the generic truncation marker (in case
    // the body itself must be cut and the generic marker appears in its
    // place), so the finished string stays within the budget in every case.
    let body_cap = MAX_TOOL_OUTPUT_BYTES.saturating_sub(tail.len() + TRUNCATION_SUFFIX.len());
    let mut out = truncate_tool_output_at(body, body_cap);
    out.push_str(&tail);
    out
}

/// Incremental byte budget for streaming output: keeps the first `limit`
/// bytes, marks itself truncated the moment a chunk does not fully fit, and
/// yields the shared [`TRUNCATION_SUFFIX`] marker exactly once.
///
/// This is the shared engine behind the daemon's streaming paths (shell
/// stdout forwarding, the VM's guest WRITE output) so they all agree on the
/// "first N bytes + one marker" contract — a chunk that would cross the cap
/// is forwarded as a fitting prefix, everything after is dropped, and the
/// marker is emitted once.
pub struct ByteBudget {
    limit: usize,
    used: usize,
    truncated: bool,
    marker_sent: bool,
}

impl ByteBudget {
    pub fn new(limit: usize) -> Self {
        Self {
            limit,
            used: 0,
            truncated: false,
            marker_sent: false,
        }
    }

    /// How many of the first `len` bytes of a chunk may be forwarded: the
    /// whole chunk when it fits under the remaining budget, a fitting prefix
    /// when it would cross the cap (marking the budget truncated), or `0`
    /// once the budget is exhausted.
    pub fn fit(&mut self, len: usize) -> usize {
        if self.used >= self.limit {
            self.truncated = true;
            return 0;
        }
        let remaining = self.limit - self.used;
        if len > remaining {
            self.used = self.limit;
            self.truncated = true;
            remaining
        } else {
            self.used += len;
            len
        }
    }

    /// The one-time truncation marker ([`TRUNCATION_SUFFIX`]) — `Some` on the
    /// first call after the budget was crossed, `None` afterwards (and before
    /// any truncation).
    ///
    /// This is the *only* way to observe truncation: the budget latches (a
    /// chunk that did not fully fit marks it truncated forever), so a bare
    /// `is_truncated()`-style re-check would re-arm on every subsequent
    /// chunk. `take_marker` consumes the signal exactly once — the same
    /// one-shot contract the streaming paths rely on.
    pub fn take_marker(&mut self) -> Option<&'static str> {
        if self.truncated && !self.marker_sent {
            self.marker_sent = true;
            Some(TRUNCATION_SUFFIX)
        } else {
            None
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn spoofing_predicates_match_unicode_tables_for_all_chars() {
        // Sweep the full code space: the predicates must equal exactly
        // (separator) OR (Cf && !joiner), and the Cf-only variant must equal
        // (Cf && !joiner), computed from the raw category tables. This is the
        // guard that keeps the shared policy honest as the
        // `unicode-general-category` tables are updated by a crate bump — a
        // newly-assigned format char is escaped here, never silently passed
        // through.
        for c in '\u{0}'..=char::MAX {
            let is_separator = matches!(c, '\u{2028}' | '\u{2029}');
            let is_cf = get_general_category(c) == GeneralCategory::Format;
            let is_joiner = matches!(c, '\u{200c}' | '\u{200d}');
            let expected_full = is_separator || (is_cf && !is_joiner);
            assert_eq!(
                is_unsafe_unicode(c),
                expected_full,
                "is_unsafe_unicode drift for U+{:04X}",
                c as u32
            );
            let expected_cf = is_cf && !is_joiner;
            assert_eq!(
                is_non_joiner_format_char(c),
                expected_cf,
                "is_non_joiner_format_char drift for U+{:04X}",
                c as u32
            );
        }
    }

    #[test]
    fn spoofing_predicate_spot_checks() {
        // Bidi overrides, ZWSP, invisible operators and separators are unsafe.
        assert!(is_unsafe_unicode('\u{202e}'));
        assert!(is_unsafe_unicode('\u{200b}'));
        assert!(is_unsafe_unicode('\u{2066}'));
        assert!(is_unsafe_unicode('\u{2028}'));
        assert!(is_unsafe_unicode('\u{2029}'));
        assert!(is_unsafe_unicode('\u{feff}'));
        // Joiners are legitimate in some scripts and pass through.
        assert!(!is_unsafe_unicode('\u{200c}'));
        assert!(!is_unsafe_unicode('\u{200d}'));
        // Separators are NOT format chars: the Cf-only predicate excludes them.
        assert!(!is_non_joiner_format_char('\u{2028}'));
        assert!(!is_non_joiner_format_char('\u{2029}'));
        // Safe text passes through.
        assert!(!is_unsafe_unicode('a'));
        assert!(!is_unsafe_unicode('é'));
        assert!(!is_unsafe_unicode('日'));
    }

    #[test]
    fn truncation_marker_and_suffix_agree() {
        // The suffix is the marker with a leading newline; pinned here so the
        // two literals can never drift.
        assert_eq!(TRUNCATION_MARKER, "...[truncated]");
        assert_eq!(TRUNCATION_SUFFIX, "\n...[truncated]");
        assert_eq!(TRUNCATION_SUFFIX, &format!("\n{TRUNCATION_MARKER}"));
    }

    #[test]
    fn truncate_keeps_short_content_untouched() {
        assert_eq!(truncate_tool_output("hello"), "hello");
        assert_eq!(truncate_tool_output(""), "");
    }

    #[test]
    fn truncate_caps_long_content() {
        let big = "x".repeat(MAX_TOOL_OUTPUT_BYTES + 100);
        let out = truncate_tool_output(&big);
        assert!(out.ends_with("...[truncated]"), "{out:?}");
        // body (capped at the budget) + "\n...[truncated]" marker
        assert_eq!(out.len(), MAX_TOOL_OUTPUT_BYTES + TRUNCATION_SUFFIX.len());
    }

    #[test]
    fn truncate_never_splits_utf8() {
        // 3-byte chars where the cap lands mid-char must stay valid UTF-8.
        let big = "€".repeat((MAX_TOOL_OUTPUT_BYTES / 3) + 10);
        let out = truncate_tool_output(&big);
        assert!(out.ends_with("...[truncated]"), "{out:?}");
        std::str::from_utf8(out.as_bytes()).expect("truncated output must be valid UTF-8");
    }

    #[test]
    fn finish_tool_output_keeps_marker_within_budget() {
        // A body larger than the shared byte budget: the byte-cap truncation
        // marker appears, and the caller's marker must survive appended after
        // it — the count signal is the whole point of the marker. The tail
        // stays *within* the budget (room is reserved for it), so it also
        // survives the transcript re-cap in `record_tool_completion`.
        let big = "x".repeat(MAX_TOOL_OUTPUT_BYTES + 100);
        let out = finish_tool_output(&big, Some("...[truncated at 5 results]".to_string()));
        assert!(out.contains("...[truncated]"), "expected byte-cap marker");
        assert!(
            out.ends_with("...[truncated at 5 results]"),
            "marker must survive the cap: …{}",
            &out[out.len().saturating_sub(60)..]
        );
        assert!(
            out.len() <= MAX_TOOL_OUTPUT_BYTES,
            "body + tail must stay within the budget: {} bytes",
            out.len()
        );
    }

    #[test]
    fn finish_tool_output_tail_survives_transcript_recap() {
        // Regression: `record_tool_completion` re-applies the byte cap after
        // `sanitize_transcript` (escaping expands Cf chars). A tool tail
        // (exit footer / count marker) appended by `finish_tool_output` must
        // survive that re-cap — guaranteed by keeping body + tail within the
        // budget, so the re-cap is a no-op. This pins the composition: a body
        // exactly at the cap plus a footer must not have the footer cut off.
        let body = "x".repeat(MAX_TOOL_OUTPUT_BYTES);
        let footer = "[VM: exited with code 0 in 100 cycles]";
        let out = finish_tool_output(&body, Some(footer.to_string()));
        assert!(
            out.len() <= MAX_TOOL_OUTPUT_BYTES,
            "body + footer must fit within the budget: {} bytes",
            out.len()
        );
        // The transcript choke point's truncation is a no-op on content that
        // already fits, so the footer is preserved end to end.
        let recapped = truncate_tool_output(&out);
        assert_eq!(recapped, out, "re-cap must be a no-op so the tail survives");
        assert!(
            recapped.ends_with(footer),
            "footer must survive the transcript re-cap"
        );
    }

    #[test]
    fn finish_tool_output_without_marker_is_plain_cap() {
        let body = "a\nb";
        assert_eq!(finish_tool_output(body, None), body);
    }

    #[test]
    fn byte_budget_keeps_fitting_prefix_and_emits_marker_once() {
        // The "first N bytes + one marker" contract every streaming path
        // shares: a crossing chunk is forwarded as a fitting prefix, the
        // budget reports truncation, the marker is yielded exactly once, and
        // everything after is dropped.
        let mut b = ByteBudget::new(10);
        assert_eq!(b.fit(4), 4, "chunks that fit are forwarded whole");
        assert_eq!(b.take_marker(), None, "no marker before any truncation");
        assert_eq!(b.fit(10), 6, "crossing chunk keeps a fitting prefix");
        assert_eq!(b.take_marker(), Some(TRUNCATION_SUFFIX));
        assert_eq!(b.take_marker(), None, "marker is one-time");
        assert_eq!(b.fit(5), 0, "nothing fits past the cap");
    }

    #[test]
    fn byte_budget_exact_fit_is_not_truncated() {
        // A chunk that lands exactly on the limit is a full fit, not a cut.
        let mut b = ByteBudget::new(8);
        assert_eq!(b.fit(8), 8);
        assert_eq!(b.take_marker(), None, "exact fit is not a cut");
        // The next chunk is what trips the cap.
        assert_eq!(b.fit(1), 0);
        assert_eq!(b.take_marker(), Some(TRUNCATION_SUFFIX));
    }

    #[test]
    fn byte_budget_zero_limit_immediately_truncates() {
        let mut b = ByteBudget::new(0);
        assert_eq!(b.fit(1), 0);
        assert_eq!(b.take_marker(), Some(TRUNCATION_SUFFIX));
    }
}
