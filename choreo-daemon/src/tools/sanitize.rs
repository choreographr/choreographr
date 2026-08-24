// The shared spoofing predicates come from choreo-sanitize (the leaf crate
// that owns the Unicode-safety policy), and the shared output byte budget /
// truncation primitives are re-exported so every `super::` / `crate::tools::`
// importer in this crate keeps resolving them through this module.
pub(crate) use choreo_sanitize::{MAX_TOOL_OUTPUT_BYTES, finish_tool_output, truncate_tool_output};
use choreo_sanitize::{is_non_joiner_format_char, is_unsafe_unicode};
use std::borrow::Cow;

/// Whether every byte is an ASCII printable (`0x20..=0x7e`), or a TAB when
/// `keep_tabs` — the fast path shared by [`sanitize_text`] and
/// [`sanitize_text_len`] so both stay in sync on when escaping is needed.
/// Multi-byte UTF-8 bytes are all `>= 0x80`, so any non-ASCII text falls
/// through to the slow path, where it may hide a separator or format char.
fn is_plain_ascii(text: &str, keep_tabs: bool) -> bool {
    text.bytes()
        .all(|b| (b == b'\t' && keep_tabs) || (0x20..=0x7e).contains(&b))
}

/// Escape control characters and Unicode line/paragraph separators in a
/// string so a hostile name or content cannot corrupt the line-oriented tool
/// output (every entry must stay on exactly one line) or inject terminal
/// escape sequences.
///
/// - C0/C1 control characters (`char::is_control`) are escaped via
///   `escape_default` (`\n`, `\t`, `\u{1b}`, …).
/// - U+2028 LINE SEPARATOR / U+2029 PARAGRAPH SEPARATOR are **not**
///   `is_control` (categories Zl/Zp), yet terminals render them as line
///   breaks — they must be escaped to preserve the one-line-per-result
///   invariant.
/// - Unicode format characters (category Cf) are invisible but can reorder,
///   hide, or spoof rendered text: bidi marks/overrides/isolates, zero-width
///   space and word joiner, invisible operators, the BOM, and more (see
///   [`is_unsafe_unicode`]). The joiners U+200C/U+200D (ZWNJ/ZWJ) do not
///   reorder or hide text and are legitimate in some scripts, so they pass
///   through.
///
/// `keep_tabs` leaves TAB literal — legitimate in source-line *content* (grep
/// match/context lines) — while names still escape it.
pub(crate) fn sanitize_text(text: &str, keep_tabs: bool) -> String {
    // Fast path: ASCII printables (plus tabs when kept) — nothing to escape.
    // Multi-byte UTF-8 bytes are all >= 0x80, so any non-ASCII text falls
    // through to the slow path (it may hide a separator or bidi char).
    if is_plain_ascii(text, keep_tabs) {
        return text.to_string();
    }
    let mut out = String::with_capacity(text.len());
    for c in text.chars() {
        if sanitize_keeps(c, keep_tabs) {
            out.push(c);
        } else {
            // escape_default renders the special escapes (`\t`, `\r`, `\n`,
            // …) and everything else control-related as `\u{...}` — all inert
            // ASCII text, so nothing terminal-affecting or line-splitting leaks.
            out.extend(c.escape_default());
        }
    }
    out
}

/// Whether `c` passes through [`sanitize_text`] unchanged under `keep_tabs`
/// (tabs are kept in line *content*, escaped in names). Shared by the
/// sanitizer and its allocation-free length estimator so the two can never
/// drift on what counts as "unsafe".
fn sanitize_keeps(c: char, keep_tabs: bool) -> bool {
    if c.is_ascii() {
        // ASCII can never be a Unicode line/paragraph separator or format
        // char, so skip the general-category lookup entirely: keep only the
        // printable range (plus TAB under the content policy). This mirrors
        // the `is_plain_ascii` fast path for the per-char loop, so a
        // mostly-ASCII line with a stray non-ASCII char does not pay a
        // category lookup for every ASCII byte.
        return (c == '\t' && keep_tabs) || (' '..='~').contains(&c);
    }
    !c.is_control() && !is_unsafe_unicode(c)
}

/// Byte length `text` would occupy after [`sanitize_text`] escaping with the
/// given `keep_tabs` policy — computed *without* allocating. Lets grep's
/// byte-budget pre-check reject an over-budget line before paying for the
/// sanitizing copy. Exact, not an estimate: escaping expands a control or
/// format char to up to 10 bytes (`\u{10ffff}`), so a raw-byte count would both
/// miss ESC-heavy lines and misjudge the budget threshold.
pub(crate) fn sanitize_text_len(text: &str, keep_tabs: bool) -> usize {
    // Fast path mirrors `sanitize_text`: all-ASCII printables (plus tabs when
    // kept) pass through untouched, so the length is just the byte count.
    if is_plain_ascii(text, keep_tabs) {
        return text.len();
    }
    text.chars()
        .map(|c| {
            if sanitize_keeps(c, keep_tabs) {
                c.len_utf8()
            } else {
                c.escape_default().count()
            }
        })
        .sum()
}

// The spoofing predicate — line/paragraph separators plus the non-joiner
// format-char class (bidi marks/overrides/isolates, ZWSP, invisible
// operators, …) — is shared from `choreo-sanitize` (the leaf crate that
// owns the Unicode-safety policy). See `choreo_sanitize::is_unsafe_unicode`
// for the full rationale; the code-space sweep guarding it lives next to it.

/// Escape control characters in a name so a pathological name (e.g. one
/// containing a newline) cannot corrupt the line-oriented tool output — every
/// entry must stay on exactly one line for the LLM to parse the listing.
/// Tabs are escaped too, unlike [`sanitize_content`].
///
/// Shared by the line-oriented tools (`list_files`, `find`, `grep`) so a
/// hostile filename can't break any of them.
pub(crate) fn sanitize_name(name: &str) -> String {
    sanitize_text(name, false)
}

/// Escape control characters in matched line *content*, keeping tabs literal —
/// tabs are ubiquitous in code and harmless, while a hostile line (embedded
/// ESC, backspace, U+2028, …) must not corrupt output or inject terminal
/// escape sequences. Used by `grep` on match/context lines; path labels go
/// through the stricter [`sanitize_name`].
pub(crate) fn sanitize_content(content: &str) -> String {
    sanitize_text(content, true)
}

/// Escape only the Unicode *format* characters (general category Cf) except
/// the joiners — the bidi marks/overrides/isolates, ZWSP, word joiner,
/// invisible operators, BOM, soft hyphen, Mongolian vowel separator, tags,
/// and rarer format controls. These are the chars that can *reorder, hide,
/// or spoof* text in the LLM transcript — the same spoofing threat the
/// line-oriented sanitizers already defend for `grep`/`find` matched lines.
/// Everything else passes through untouched, including ESC/ANSI sequences,
/// newlines, and tabs, so shell/VM output keeps its colors and structure
/// while the model can no longer be shown text that silently reordered
/// itself.
///
/// Applied at the single point where tool results are recorded for the
/// transcript (`record_tool_completion` in requests.rs), so it covers every
/// tool at once; the terminal is defended separately by the TUI's render
/// filter (which keeps SGR color sequences but escapes other controls).
///
/// Returns a [`Cow`]: the fast path (pure ASCII — the common case for tool
/// output) borrows the input with no copy, avoiding a full up-to-128-KiB
/// allocation on every recorded result; only content that actually needs
/// escaping allocates.
pub(crate) fn sanitize_transcript(text: &str) -> Cow<'_, str> {
    // Fast path: pure ASCII (printables plus the whitespace that is always
    // kept) contains no Cf chars, so no escape can be needed — return the
    // input borrowed instead of copying it.
    if text
        .bytes()
        .all(|b| b.is_ascii() && (b >= 0x20 || matches!(b, b'\n' | b'\t' | b'\r')))
    {
        return Cow::Borrowed(text);
    }
    let mut out = String::with_capacity(text.len());
    for c in text.chars() {
        if is_non_joiner_format_char(c) {
            out.extend(c.escape_default());
        } else {
            out.push(c);
        }
    }
    Cow::Owned(out)
}

/// Cap `body` at the shared byte budget with `marker` (if any) reserved
/// *inside* the budget, sanitizing the body for the LLM transcript first.
///
/// `record_tool_completion` re-applies `sanitize_transcript` + the byte cap
/// at the choke point for every tool. Escaping *expands* (a 3-byte bidi char
/// becomes the 7-byte `\u{202e}`), so a raw body that `finish_tool_output`
/// kept ≤ [`MAX_TOOL_OUTPUT_BYTES`] could exceed the budget once escaped and
/// the re-cap would cut the tool's tail — the VM exit footer, the `find`/
/// `grep` "at least N results" marker, or `pdf_to_markdown`'s closing
/// untrusted-content delimiter — off the transcript.
///
/// Sanitizing *before* the cap closes that gap: the escape output is plain
/// ASCII, so the choke point's re-sanitize is a no-op (the function is
/// idempotent on its own output) and its re-cap is a no-op (body + tail
/// already ≤ the budget) — the tail survives end to end. `sanitize_transcript`
/// preserves ESC/ANSI, newlines, and tabs, so shell/VM colors and structure
/// are unaffected. Callers that append a critical tail to raw output (VM,
/// shell exit code, pdf framing) must use this instead of `finish_tool_output`.
pub(crate) fn finish_tool_output_sanitized(body: &str, marker: Option<String>) -> String {
    finish_tool_output(&sanitize_transcript(body), marker)
}

/// Escape control characters and Unicode format chars in a *multi-line*
/// body while preserving structural line breaks.
///
/// [`sanitize_content`] escapes `\n` itself, which is right for a single
/// line (a grep match must stay on one line) but would flatten any text
/// that legitimately spans lines — an HTTP response body, a log excerpt,
/// etc. This variant splits on `\n` *manually* rather than via
/// `str::lines()`, because `lines()` drops a trailing empty segment and
/// would therefore lose a trailing newline; a manual split preserves the
/// exact line structure. Each line has one trailing `\r` stripped (CRLF
/// folding, matching the single-line sanitizers' `str::lines()` semantics)
/// and is then sanitized with the content policy (tabs kept literal).
pub(crate) fn sanitize_multiline(text: &str) -> String {
    let mut out = String::with_capacity(text.len());
    // Split on '\n' manually (NOT str::lines()): lines() omits the empty
    // final segment of a trailing newline, so "a\n" would collapse to "a"
    // and the structural newline would be lost. `enumerate` re-joins with
    // exactly one '\n' between segments, keeping "a\n" intact.
    for (i, line) in text.split('\n').enumerate() {
        if i > 0 {
            out.push('\n');
        }
        // CRLF: fold the bare '\r' (the single-line sanitizers escape it,
        // but here it is just Windows line-ending residue, not hostile
        // content — and escaping it would leak `\r` into every CRLF body).
        let line = line.strip_suffix('\r').unwrap_or(line);
        out.push_str(&sanitize_text(line, true));
    }
    out
}

/// Marker appended when a search tool (`find`/`grep`) stops at its
/// `max_results` cap, so the LLM can tell "exactly N results" from
/// "N of many more". `None` when the walk completed naturally.
///
/// Note the marker means **at least** N matches exist: it fires as soon as
/// the cap is hit, so a tree with exactly N matching entries also reports it
/// (proving "more exist" would require walking one extra entry).
pub(crate) fn truncation_marker(truncated: bool, cap: usize, noun: &str) -> Option<String> {
    truncated.then(|| format!("...[truncated at {cap} {noun}]"))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn sanitize_name_escapes_control_chars() {
        assert_eq!(sanitize_name("plain.txt"), "plain.txt");
        assert_eq!(sanitize_name("a\nb"), "a\\nb");
        assert_eq!(sanitize_name("a\tb"), "a\\tb");
    }

    #[test]
    fn sanitize_name_escapes_unicode_separators_and_format_chars() {
        // U+2028/U+2029 are Zl/Zp — not is_control — but terminals render
        // them as line breaks, so they must be escaped to keep the
        // one-line-per-result invariant. Unicode format chars (Cf) are
        // invisible but can reorder, hide, or spoof rendered text: the bidi
        // marks/embeddings/overrides/isolates, zero-width space, word joiner,
        // invisible operators, and BOM must all be escaped.
        assert_eq!(sanitize_name("a\u{2028}b"), "a\\u{2028}b");
        assert_eq!(sanitize_name("a\u{2029}b"), "a\\u{2029}b");
        assert_eq!(sanitize_name("a\u{200e}b"), "a\\u{200e}b");
        assert_eq!(sanitize_name("a\u{200f}b"), "a\\u{200f}b");
        assert_eq!(sanitize_name("a\u{061c}b"), "a\\u{61c}b");
        assert_eq!(sanitize_name("a\u{202e}b"), "a\\u{202e}b");
        assert_eq!(sanitize_name("a\u{2066}b"), "a\\u{2066}b");
        // Zero-width space / word joiner / BOM hide or split text — escaped.
        assert_eq!(sanitize_name("a\u{200b}b"), "a\\u{200b}b");
        assert_eq!(sanitize_name("a\u{2060}b"), "a\\u{2060}b");
        assert_eq!(sanitize_name("a\u{feff}b"), "a\\u{feff}b");
        // Mongolian vowel separator is a Cf too — escaped, not passed through.
        assert_eq!(sanitize_name("a\u{180e}b"), "a\\u{180e}b");
        // Joiners do not reorder or hide text and are legitimate in some
        // scripts (Persian ZWNJ, Indic conjuncts) — they pass through.
        assert_eq!(sanitize_name("a\u{200c}b"), "a\u{200c}b");
        assert_eq!(sanitize_name("a\u{200d}b"), "a\u{200d}b");
        // Non-ASCII but safe chars pass through untouched.
        assert_eq!(sanitize_name("café"), "café");
    }

    #[test]
    fn sanitize_content_keeps_tabs_but_escapes_separators_and_format_chars() {
        // Content keeps tabs literal (legitimate in source) but still escapes
        // every other control/separator and every invisible format char.
        assert_eq!(sanitize_content("a\tb"), "a\tb");
        assert_eq!(sanitize_content("a\nb"), "a\\nb");
        assert_eq!(sanitize_content("a\u{2028}b"), "a\\u{2028}b");
        assert_eq!(sanitize_content("a\u{2029}b"), "a\\u{2029}b");
        assert_eq!(sanitize_content("a\u{200f}b"), "a\\u{200f}b");
        assert_eq!(sanitize_content("a\u{202e}b"), "a\\u{202e}b");
        assert_eq!(sanitize_content("a\u{1b}b"), "a\\u{1b}b");
        assert_eq!(sanitize_content("a\u{200b}b"), "a\\u{200b}b");
        assert_eq!(sanitize_content("a\u{2060}b"), "a\\u{2060}b");
        assert_eq!(sanitize_content("a\u{feff}b"), "a\\u{feff}b");
        assert_eq!(sanitize_content("a\u{180e}b"), "a\\u{180e}b");
        assert_eq!(sanitize_content("a\u{200c}b"), "a\u{200c}b");
    }

    #[test]
    fn sanitize_multiline_preserves_structural_newlines() {
        // Unlike `sanitize_content`, structural newlines must survive: the
        // body is displayed as-is (just with per-line escaping), never
        // flattened onto one line.
        assert_eq!(sanitize_multiline("a\nb\nc"), "a\nb\nc");
        assert_eq!(sanitize_multiline("a\n\nb"), "a\n\nb");
    }

    #[test]
    fn sanitize_multiline_folds_crlf() {
        // CRLF line endings fold to bare '\n' per line — Windows line-ending
        // residue, not hostile content, so it is stripped rather than escaped.
        assert_eq!(sanitize_multiline("a\r\nb"), "a\nb");
        assert_eq!(sanitize_multiline("a\r\nb\r\n"), "a\nb\n");
    }

    #[test]
    fn sanitize_multiline_preserves_trailing_newline() {
        // A manual split on '\n' (not str::lines()) keeps the trailing empty
        // segment, so a body ending in a newline round-trips exactly.
        assert_eq!(sanitize_multiline("a\n"), "a\n");
        assert_eq!(sanitize_multiline("\n"), "\n");
    }

    #[test]
    fn sanitize_multiline_escapes_controls_on_any_line() {
        // ESC and bidi overrides anywhere in the body are escaped per line,
        // so a hostile line cannot inject terminal sequences or spoof text
        // regardless of which line it sits on.
        assert_eq!(
            sanitize_multiline("ok\u{1b}[31m\nplain"),
            "ok\\u{1b}[31m\nplain"
        );
        assert_eq!(sanitize_multiline("a\u{202e}b\nc"), "a\\u{202e}b\nc");
        assert_eq!(sanitize_multiline("a\n\u{1b}b"), "a\n\\u{1b}b");
    }

    #[test]
    fn sanitize_multiline_keeps_tabs_and_ascii() {
        // Tabs are legitimate content and pass through; plain ASCII text is
        // unchanged end to end.
        assert_eq!(sanitize_multiline("a\tb\nc\td"), "a\tb\nc\td");
        assert_eq!(sanitize_multiline("plain text"), "plain text");
        assert_eq!(sanitize_multiline("café\n日本語"), "café\n日本語");
    }

    #[test]
    fn sanitize_transcript_escapes_only_format_chars() {
        // Bidi overrides and other invisible format chars (the spoofing
        // threat) are escaped; joiners, ANSI/ESC, newlines, tabs, CJK, and
        // plain ASCII all pass through untouched.
        assert_eq!(sanitize_transcript("a\u{202e}b"), "a\\u{202e}b");
        assert_eq!(sanitize_transcript("a\u{200b}b"), "a\\u{200b}b");
        assert_eq!(sanitize_transcript("a\u{2066}b"), "a\\u{2066}b");
        // Joiners are legitimate in some scripts and must pass through.
        assert_eq!(sanitize_transcript("a\u{200c}b"), "a\u{200c}b");
        assert_eq!(sanitize_transcript("a\u{200d}b"), "a\u{200d}b");
        // ANSI color sequences are C0 (ESC), not Cf — kept so shell/VM
        // output keeps its colors in the final view.
        assert_eq!(
            sanitize_transcript("\u{1b}[31mred\u{1b}[0m"),
            "\u{1b}[31mred\u{1b}[0m"
        );
        // Structure (newlines, tabs) and safe non-ASCII pass through.
        assert_eq!(sanitize_transcript("a\nb\tc"), "a\nb\tc");
        assert_eq!(sanitize_transcript("café 日本語"), "café 日本語");
        // Pure ASCII hits the fast path and is returned unchanged.
        assert_eq!(
            sanitize_transcript("plain text\nline two"),
            "plain text\nline two"
        );
        // A Cf char past the end is still caught (fast path is per-byte).
        assert_eq!(sanitize_transcript("tail\u{feff}"), "tail\\u{feff}");
    }

    #[test]
    fn sanitize_transcript_then_truncate_stays_within_budget() {
        // Finding 1 guard: escaping *expands* (a Cf char becomes `\u{202e}`),
        // so content that was byte-capped at the source as raw bytes (shell /
        // VM / series) could exceed MAX_TOOL_OUTPUT_BYTES after
        // `sanitize_transcript`. `record_tool_completion` re-applies the cap
        // after escaping; this test pins that composition — a cap-sized,
        // Cf-heavy body must stay within budget + marker, and the marker must
        // survive.
        let raw = "\u{00ad}".repeat(super::MAX_TOOL_OUTPUT_BYTES / 2); // 2-byte Cf char
        let sanitized = sanitize_transcript(&raw);
        assert!(
            sanitized.len() > super::MAX_TOOL_OUTPUT_BYTES,
            "escaping must expand past the raw byte cap: {} > {}",
            sanitized.len(),
            super::MAX_TOOL_OUTPUT_BYTES
        );
        let capped = truncate_tool_output(&sanitized);
        assert!(
            capped.len() <= super::MAX_TOOL_OUTPUT_BYTES + "\n...[truncated]".len(),
            "sanitize-then-truncate must stay within budget + marker: {}",
            capped.len()
        );
        assert!(
            capped.ends_with("...[truncated]"),
            "the truncation marker must survive the composition"
        );
        std::str::from_utf8(capped.as_bytes()).expect("capped output must be valid UTF-8");
    }

    #[test]
    fn finish_tool_output_sanitized_tail_survives_transcript_recap() {
        // The residual gap: escaping *expands* (a 2-byte soft hyphen becomes
        // the 6-byte `\u{ad}`), so a tail reserved inside the budget against
        // the RAW body length gets cut by the transcript choke point's re-cap
        // when the body is Cf-heavy and near the cap. `finish_tool_output_sanitized`
        // escapes BEFORE the cap, making the choke point's re-sanitize (a
        // no-op on ASCII escape output) and re-cap (a no-op on content already
        // ≤ the budget) both inert — the tool's critical tail survives end to
        // end. This pins the composition the VM footer / shell exit code /
        // pdf framing delimiter all rely on.
        let footer = "[VM: exited with code 0 in 100 cycles]";
        // ~128 KiB of 2-byte soft hyphens — expands to ~384 KiB when escaped.
        let body = "\u{00ad}".repeat(super::MAX_TOOL_OUTPUT_BYTES / 2);
        let finished = super::finish_tool_output_sanitized(&body, Some(footer.to_string()));
        assert!(
            finished.len() <= super::MAX_TOOL_OUTPUT_BYTES,
            "body + tail must stay within the budget: {} bytes",
            finished.len()
        );
        // The transcript choke point's composition (sanitize then re-cap)
        // must be a no-op on already-sanitized, within-budget content.
        let recapped = super::truncate_tool_output(&super::sanitize_transcript(&finished));
        assert_eq!(recapped, finished, "re-sanitize + re-cap must be a no-op");
        assert!(
            recapped.ends_with(footer),
            "footer must survive the transcript re-cap"
        );
        // Sanitizing at the source really did escape the Cf chars: the body
        // is inert ASCII in the finished output, not live bidi.
        assert!(
            !recapped.contains('\u{00ad}'),
            "soft hyphens must be escaped, not passed through"
        );
        std::str::from_utf8(recapped.as_bytes()).expect("capped output must be valid UTF-8");
    }

    #[test]
    fn sanitize_text_len_matches_actual_sanitized_length() {
        // The grep byte-budget pre-check must predict the exact rendered
        // length with no allocation: equal to what the sanitizer produces,
        // for both the content policy (tabs kept) and the name policy
        // (tabs escaped), across controls, separators, format chars, and
        // multi-byte text.
        for s in [
            "plain ascii",
            "tab\there",
            "new\nline",
            "esc \u{1b}[31m",
            "sep\u{2028}arator",
            "bidi\u{202e}evil",
            "mongolian\u{180e}vowel",
            "café \u{200b} zwsp",
            "",
        ] {
            assert_eq!(
                sanitize_text_len(s, true),
                sanitize_content(s).len(),
                "{s:?}"
            );
            assert_eq!(sanitize_text_len(s, false), sanitize_name(s).len(), "{s:?}");
        }
    }

    #[test]
    fn sanitize_keeps_matches_policy_for_all_chars() {
        // The keep-predicate must agree with the policy for *every* char:
        // escape C0/C1 controls plus every char the shared spoofing predicate
        // flags; pass through everything else (plus tabs under the content
        // policy). The predicate's own correctness against the Unicode tables
        // is guarded by the code-space sweep in choreo-sanitize; this sweep
        // validates the daemon's keep policy against that shared predicate.
        for c in '\u{0}'..=char::MAX {
            let is_control = c.is_control();
            let is_unsafe = is_unsafe_unicode(c);
            // Name policy (tabs escaped): every control and spoofing char is
            // escaped; nothing else.
            assert_eq!(
                sanitize_keeps(c, false),
                !is_control && !is_unsafe,
                "name-policy keep drift for U+{:04X}",
                c as u32
            );
            // Content policy (tabs literal): identical, but TAB passes through.
            assert_eq!(
                sanitize_keeps(c, true),
                (c == '\t') || (!is_control && !is_unsafe),
                "content-policy keep drift for U+{:04X}",
                c as u32
            );
        }
    }

    #[test]
    fn truncation_marker_only_when_capped() {
        assert_eq!(truncation_marker(false, 50, "results"), None);
        assert_eq!(
            truncation_marker(true, 50, "results").as_deref(),
            Some("...[truncated at 50 results]")
        );
        assert_eq!(
            truncation_marker(true, 200, "matches").as_deref(),
            Some("...[truncated at 200 matches]")
        );
    }
}
