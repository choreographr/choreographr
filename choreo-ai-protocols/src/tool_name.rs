//! The two rules that decide whether a tool-call name is usable.
//!
//! They are deliberately different predicates and deliberately live together,
//! because they are easy to confuse and costly to let drift apart:
//!
//! - [`is_blank`] is the **stream-delta merge** rule. A provider sends a call's
//!   `function.name` and `id` once, on the first delta for that index, and its
//!   argument-only continuation deltas repeat both keys as present-but-empty
//!   strings. For merging, blank means "absent", so the merge keeps the first
//!   non-blank value it sees.
//! - [`is_provider_safe`] is the **provider validity** rule. A provider accepts
//!   a function name of at most [`MAX_FUNCTION_NAME_LEN`] bytes drawn from
//!   `[A-Za-z0-9_-]`; one illegal name rejects the entire request, so a caller
//!   about to send a name must reject anything that fails this.
//!
//! **Blank is a strict subset of not-provider-safe**: every blank name fails
//! [`is_provider_safe`], and plenty of non-blank names fail it too (a dotted
//! `multi_tool_use.parallel`, a 340-character shell command a weak model put in
//! `name`, a path-style `/`). A caller that rejects only blank names therefore
//! accepts names a provider will refuse — the asymmetry is real, and a caller
//! must pick the predicate that matches its job rather than the stricter-looking
//! one.

/// The provider's function-name byte cap (`OpenAI`'s ceiling).
///
/// `Anthropic` is more generous (128), so 64 is the safe common limit: a name
/// within this cap and drawn from the accepted alphabet is legal on every
/// provider the workspace talks to. The MCP tool-name builder caps at the same
/// number, and a test in the daemon pins the two together.
pub const MAX_FUNCTION_NAME_LEN: usize = 64;

/// Whether `name` is blank — empty or whitespace-only — under the stream-delta
/// merge rule.
///
/// A provider repeats a tool call's `name` and `id` as present-but-empty
/// placeholders on argument-only deltas, so "present but blank" means the same
/// thing as "absent" to a merge and must not overwrite the value the first
/// delta carried. Trimming is part of the rule: a whitespace-only fragment is a
/// placeholder too.
///
/// This says nothing about whether a provider will accept the name — see
/// [`is_provider_safe`].
#[must_use]
pub fn is_blank(name: &str) -> bool {
    name.trim().is_empty()
}

/// Whether `name` is one a provider will accept as a function name.
///
/// The name must be non-empty, at most [`MAX_FUNCTION_NAME_LEN`] **bytes** long,
/// and every byte must be `[A-Za-z0-9_-]`. There is deliberately **no**
/// trimming: a leading or trailing space is an illegal byte, not a blank, so
/// `" sh"` is rejected rather than read as `"sh"`. The rule is what a provider
/// enforces, not a tidier version of it, because the name is compared verbatim
/// against the tool catalog on the way back out.
///
/// Iterates bytes (ASCII classes only), so it needs no regex dependency and no
/// Unicode handling. A blank name always fails this: [`is_blank`] is a strict
/// subset of `!is_provider_safe`.
#[must_use]
pub fn is_provider_safe(name: &str) -> bool {
    !name.is_empty()
        && name.len() <= MAX_FUNCTION_NAME_LEN
        && name
            .bytes()
            .all(|b| b.is_ascii_alphanumeric() || b == b'_' || b == b'-')
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The corpus both predicates are asserted over: the empty and
    /// whitespace-only placeholders a stream sends, ordinary provider names,
    /// the exact cap and one byte past it, a dotted multi-tool name, a
    /// whitespace-prefixed name, a non-ASCII name, and a long shell command a
    /// weak model might emit as a "tool".
    fn corpus() -> Vec<String> {
        vec![
            String::new(),
            "  ".to_string(),
            "sh".to_string(),
            "a-b_c".to_string(),
            "x".repeat(MAX_FUNCTION_NAME_LEN),
            "x".repeat(MAX_FUNCTION_NAME_LEN + 1),
            "multi_tool_use.parallel".to_string(),
            " sh".to_string(),
            "日本語".to_string(),
            format!("rm -rf {} && echo done", "/tmp/some/path ".repeat(20)),
        ]
    }

    #[test]
    fn is_blank_recognizes_empty_and_whitespace_only() {
        assert!(is_blank(""));
        assert!(is_blank("  "));
        assert!(is_blank("\t\n"));
        assert!(!is_blank("sh"));
        assert!(!is_blank(" sh"));
        assert!(!is_blank("日本語"));
    }

    #[test]
    fn is_provider_safe_accepts_only_the_provider_alphabet_within_the_cap() {
        for name in ["sh", "a-b_c", "A", "9", "___"] {
            assert!(is_provider_safe(name), "{name} should be accepted");
        }
        assert!(is_provider_safe(&"x".repeat(MAX_FUNCTION_NAME_LEN)));
        assert!(!is_provider_safe(&"x".repeat(MAX_FUNCTION_NAME_LEN + 1)));
    }

    #[test]
    fn is_provider_safe_rejects_everything_else() {
        // Empty, whitespace-prefixed (no trimming), dotted, non-ASCII, and a
        // long shell command — each is a shape that a blank-only check lets
        // through and a provider rejects.
        assert!(!is_provider_safe(""));
        assert!(!is_provider_safe("  "));
        assert!(!is_provider_safe(" sh"));
        assert!(!is_provider_safe("sh "));
        assert!(!is_provider_safe("multi_tool_use.parallel"));
        assert!(!is_provider_safe("日本語"));
        assert!(!is_provider_safe("mcp/x/y"));
        assert!(!is_provider_safe("has space"));
    }

    #[test]
    fn blank_implies_not_provider_safe_over_the_corpus() {
        // The invariant callers rely on: rejecting only blank names is a
        // strictly weaker gate than rejecting what a provider refuses.
        for name in corpus() {
            let blank = is_blank(&name);
            let safe = is_provider_safe(&name);
            if blank {
                assert!(!safe, "{name:?} is blank, so it must not be provider-safe");
            }
        }
    }

    #[test]
    fn corpus_agrees_with_the_two_hand_written_rules() {
        // Pin the predicates to their literal definitions over the corpus, so a
        // future "tidy-up" of either rule (adding a trim to the validity check,
        // measuring chars instead of bytes) fails here rather than silently
        // shifting the boundary thousands of stored sessions sit on.
        for name in corpus() {
            assert_eq!(is_blank(&name), name.trim().is_empty(), "{name:?}");
            let expected = !name.is_empty()
                && name.len() <= MAX_FUNCTION_NAME_LEN
                && name
                    .bytes()
                    .all(|b| b.is_ascii_alphanumeric() || b == b'_' || b == b'-');
            assert_eq!(is_provider_safe(&name), expected, "{name:?}");
        }
    }
}
