//! The rules that decide whether a tool call may be put on the wire.
//!
//! A provider answers one unacceptable call by rejecting the **whole** request
//! (HTTP 400), so every judgement about a call — what its `arguments_json`
//! bytes are, and whether its `function.name` is a name a provider accepts — is
//! made here, by this one owner. Two owners of one such rule drift apart, and
//! the drift costs a turn in either direction: a call the ingest gate accepts
//! and the replay repair rewrites is dispatched as something other than what
//! the model sent, and a call the gate refuses that a later layer would have
//! accepted is a casualty of a rejection nobody actually made.
//!
//! The two **name** rules are deliberately different predicates and deliberately
//! live together, because they are easy to confuse and costly to let drift
//! apart:
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
//!
//! The **arguments** rule is the single classifier [`classify_arguments`]: its
//! three outcomes are consumed by the adapters' ingest gate and by the daemon's
//! replay repair, so what was accepted at ingest and what a later request
//! replays cannot disagree.

use serde_json::Value;

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

/// What a tool call's recorded `arguments_json` bytes are, for both the ingest
/// gate and the replay repair.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Arguments<'a> {
    /// The bytes already parse as a JSON object and may be sent verbatim.
    Object(&'a str),
    /// No arguments were given: empty, whitespace-only, or the JSON literal
    /// `null`. A provider sends this shape for a tool that takes none.
    Absent,
    /// Not a JSON object. `line`/`column` are the parser's own 1-based position
    /// when the failure was a syntax error (typically a cut-off reply), and
    /// `None` for a well-formed value that simply is not an object.
    Invalid {
        /// Parser's 1-based line of the syntax error, when there was one.
        line: Option<usize>,
        /// Parser's 1-based column of the syntax error, when there was one.
        column: Option<usize>,
    },
}

/// Classify a tool call's `arguments_json` bytes in one parse.
///
/// A provider puts one rule on this field — the arguments must be a JSON
/// **object** — so the field is judged once, here, and every layer that has to
/// decide what to do with it reads the same verdict:
///
/// * [`Arguments::Object`] hands back the **original** bytes, never a
///   re-serialization. The request bytes double as the provider's prompt-cache
///   prefix, so reordering or respacing an object would churn a cached prompt
///   for no gain.
/// * [`Arguments::Absent`] is a legal **no-argument call**, not a casualty: an
///   empty or whitespace-only string is what several OpenAI-compatible
///   endpoints send for a tool that declares no parameters, and the JSON
///   literal `null` is the other spelling of the same fact. Both normalize to
///   `{}` at ingest, so the call runs and the turn is never mistaken for a
///   truncation that would spend the daemon's recovery budget on a complete
///   reply.
/// * [`Arguments::Invalid`] is a **casualty**: bytes that are not an object,
///   whether a cut-off fragment (a syntax error, whose position is reported) or
///   a well-formed value of the wrong type (an array, a string, a number, a
///   bool). Keeping the two apart is what lets a caller discard the casualty
///   while letting the legal no-argument call through.
#[must_use]
pub fn classify_arguments(raw: &str) -> Arguments<'_> {
    // An empty or blank string never reaches the parser: `serde_json` would call
    // it a syntax error, and it is not one — no argument list was written.
    if raw.trim().is_empty() {
        return Arguments::Absent;
    }
    // One parse answers both questions (is it an object, and if not, where did
    // the parser stop), so no caller has to parse a second time to read the
    // position off the error.
    match serde_json::from_str::<Value>(raw) {
        Ok(Value::Object(_)) => Arguments::Object(raw),
        Ok(Value::Null) => Arguments::Absent,
        // A well-formed value of the wrong type carries no position: nothing
        // was cut off, the model simply sent a shape a provider will not take.
        Ok(_) => Arguments::Invalid {
            line: None,
            column: None,
        },
        Err(err) => Arguments::Invalid {
            line: Some(err.line()),
            column: Some(err.column()),
        },
    }
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

    /// The corpus the arguments classifier is asserted over: the two shapes a
    /// no-argument tool arrives as, ordinary objects (plain, nested and
    /// whitespace-padded), the cut-off fragments a provider leaves behind, and
    /// the well-formed non-objects a provider will not take.
    fn arguments_corpus() -> Vec<&'static str> {
        vec![
            "{}",
            r#"{"a": 1,"b":[2, 3]}"#,
            "  {\n  \"city\": \"London\"\n}  ",
            r#"{"outer":{"inner":[1,{"x":null}]}}"#,
            "",
            "   ",
            "\t\n",
            "null",
            " null ",
            r#"{"path":"#,
            r#"{"command":"rm -rf /tmp/buil"#,
            "[1]",
            "\"hello\"",
            "42",
            "true",
        ]
    }

    #[test]
    fn classify_arguments_hands_back_the_original_object_bytes() {
        // A kept object is never re-serialized: the same bytes have to come
        // back, because the request bytes double as the prompt-cache prefix.
        for raw in [
            r#"{"a": 1,"b":[2, 3]}"#,
            "  {\n  \"city\": \"London\"\n}  ",
            r#"{"outer":{"inner":[1,{"x":null}]}}"#,
        ] {
            let Arguments::Object(bytes) = classify_arguments(raw) else {
                panic!("{raw:?} is a JSON object");
            };
            assert_eq!(bytes, raw);
            assert!(
                std::ptr::eq(bytes, raw),
                "{raw:?} must be borrowed, not rebuilt",
            );
        }
    }

    #[test]
    fn classify_arguments_calls_a_missing_argument_list_absent() {
        // A legal no-argument call: empty, blank, and the JSON `null` a
        // provider sends for the same thing.
        for raw in ["", "   ", "\t\n", "null", " null "] {
            assert_eq!(classify_arguments(raw), Arguments::Absent, "{raw:?}");
        }
    }

    #[test]
    fn classify_arguments_marks_a_cut_off_fragment_with_the_parser_position() {
        let Arguments::Invalid { line, column } =
            classify_arguments(r#"{"command":"rm -rf /tmp/buil"#)
        else {
            panic!("a fragment is not an object");
        };
        assert_eq!(line, Some(1));
        assert!(column.is_some(), "the parser reports where it stopped");
    }

    #[test]
    fn classify_arguments_marks_a_well_formed_non_object_invalid_without_a_position() {
        // Nothing was cut off, so there is no "where did it stop" to report.
        for raw in ["[1]", "\"hello\"", "42", "true"] {
            assert_eq!(
                classify_arguments(raw),
                Arguments::Invalid {
                    line: None,
                    column: None,
                },
                "{raw:?}",
            );
        }
    }

    #[test]
    fn arguments_corpus_agrees_with_the_literal_definition() {
        // Pin the classifier to its literal definition over the corpus, so a
        // future "tidy-up" (letting a `null` fall through to `Invalid`, or
        // reading an empty string as a parse error) fails here rather than
        // silently shifting the boundary both the ingest gate and the replay
        // repair sit on.
        for raw in arguments_corpus() {
            let expected = if raw.trim().is_empty() {
                Arguments::Absent
            } else {
                match serde_json::from_str::<Value>(raw) {
                    Ok(Value::Object(_)) => Arguments::Object(raw),
                    Ok(Value::Null) => Arguments::Absent,
                    Ok(_) => Arguments::Invalid {
                        line: None,
                        column: None,
                    },
                    Err(err) => Arguments::Invalid {
                        line: Some(err.line()),
                        column: Some(err.column()),
                    },
                }
            };
            assert_eq!(classify_arguments(raw), expected, "{raw:?}");
        }
    }
}
