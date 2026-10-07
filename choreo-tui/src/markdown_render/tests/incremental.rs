//! Incremental streaming renderer: byte-identity against a whole-document
//! render at every prefix, plus proof that the committed prefix is actually
//! reused.

use super::super::*;
use choreo_proto::TimestampMs;

/// Documents exercised by the prefix sweep: prose, headings (including a
/// non-`#` first heading, which drives the heading shift), fenced code with
/// blank lines *inside* the fence, tables, block quotes, and lists (tight and
/// loose — the latter must take the soft-boundary fallback).
const CORPUS: &[(&str, &str)] = &[
    (
        "paragraphs",
        "First paragraph here.\n\nSecond paragraph with more text.\n\nThird one.\n",
    ),
    (
        "headings",
        "Intro text.\n\n# Title\n\nBody under the title.\n\n## Sub\n\nMore body.\n",
    ),
    (
        "shifted headings",
        "## Two\n\n### Three\n\nBody after the headings.\n",
    ),
    (
        "fenced code with inner blanks",
        "Text before.\n\n```rust\nfn a() {}\n\nfn b() {}\n```\n\nText after.\n",
    ),
    (
        "tilde fence",
        "Lead.\n\n~~~sh\necho one\n\necho two\n~~~\n\nTrail.\n",
    ),
    (
        "table",
        "Before.\n\n| a | b |\n| - | - |\n| 1 | 2 |\n| 3 | 4 |\n\nAfter.\n",
    ),
    (
        "block quote",
        "Before.\n\n> quoted line one\n> quoted line two\n\nAfter.\n",
    ),
    ("tight list", "Before.\n\n- one\n- two\n- three\n\nAfter.\n"),
    (
        "loose list (soft boundary)",
        "Before.\n\n- one\n\n- two\n\nAfter.\n",
    ),
    (
        "heading then list then prose",
        "# Head\n\ntext body here\n\n1. a\n2. b\n3. c\n\ntrailing prose.\n",
    ),
    (
        "mixed everything",
        "Intro.\n\n## Section\n\nProse paragraph.\n\n- item one\n- item two\n\n> a quote\n\n```\ncode\n\nmore code\n```\n\n| x | y |\n| - | - |\n| 1 | 2 |\n\nDone.\n",
    ),
];

/// Render every prefix of `doc` (at char boundaries) through a fresh
/// [`IncrementalMarkdown`] and assert byte-equality with the whole-document
/// render at each step.
///
/// The per-line `LineChrome` table/row identity is compared by *grouping*: the
/// ordinal [`TableRowId`] is allocated from a process-wide monotonic counter
/// (see `next_table_id`), so two renders of the same document never share the
/// same absolute ordinal — only the partition of rows into table groups and
/// each row's index within its table are meaningful.
fn assert_every_prefix_matches(name: &str, doc: &str, width: u16) {
    let mut incremental = IncrementalMarkdown::new();
    for end in prefix_ends(doc) {
        let prefix = doc.get(..end).expect("prefix end is a char boundary");
        let (inc_lines, inc_joins, inc_chrome) = incremental.render(prefix, width);
        let (full_lines, full_joins, full_chrome) = markdown_lines_joined(prefix, width);
        assert_eq!(
            inc_lines, full_lines,
            "[{name}] line mismatch at prefix {end:?} of {doc:?}\n-- incremental --\n{inc_lines:#?}\n-- full --\n{full_lines:#?}"
        );
        assert_eq!(
            inc_joins, full_joins,
            "[{name}] join mismatch at prefix {end:?} of {doc:?}"
        );
        assert_eq!(
            normalize_chrome(&inc_chrome),
            normalize_chrome(&full_chrome),
            "[{name}] chrome mismatch at prefix {end:?} of {doc:?}"
        );
    }
}

/// A `LineChrome` view with table ordinals renumbered to their first-seen order,
/// so two renders that group the same rows into tables compare equal regardless
/// of the absolute process-wide ordinals.
type NormalizedChromeRow = (Vec<(u16, u16)>, Option<(u32, u32)>);

/// A `LineChrome` view with table ordinals renumbered to their first-seen order,
/// so two renders that group the same rows into tables compare equal regardless
/// of the absolute process-wide ordinals.
fn normalize_chrome(chrome: &[LineChrome]) -> Vec<NormalizedChromeRow> {
    let mut ordinals: std::collections::HashMap<u32, u32> = std::collections::HashMap::new();
    let mut next = 0u32;
    chrome
        .iter()
        .map(|c| {
            let table = c.table().map(|id| {
                let ordinal = *ordinals.entry(id.table).or_insert_with(|| {
                    let v = next;
                    next += 1;
                    v
                });
                (ordinal, id.row)
            });
            (c.intervals().to_vec(), table)
        })
        .collect()
}

/// Every char-boundary end offset of `doc`, from 0 to `doc.len()`, in order.
fn prefix_ends(doc: &str) -> Vec<usize> {
    let mut ends: Vec<usize> = doc.char_indices().map(|(i, _)| i).collect();
    ends.push(doc.len());
    ends
}

#[test]
fn incremental_matches_full_for_every_prefix_at_80() {
    for (name, doc) in CORPUS {
        assert_every_prefix_matches(name, doc, 80);
    }
}

#[test]
fn incremental_matches_full_for_every_prefix_at_narrow_width() {
    // A narrow width forces aggressive wrapping, which stresses the separator
    // placement and the per-line height model.
    for (name, doc) in CORPUS {
        assert_every_prefix_matches(name, doc, 20);
    }
}

#[test]
fn incremental_matches_full_on_single_giant_paragraph() {
    // No blank line anywhere: the committed prefix can never advance, so every
    // frame must fall back to a whole-source render — and still match.
    let doc = "one two three four five six seven eight nine ten eleven twelve";
    assert_every_prefix_matches("giant paragraph", doc, 24);
}

#[test]
fn incremental_reuses_the_committed_prefix() {
    // A paragraph-heavy document with frequent hard boundaries: the committed
    // prefix should absorb most of the bytes, so the *total* bytes parsed
    // across all prefixes stays linear in the document, not quadratic.
    let doc = "# Heading\n\n\
        alpha beta gamma delta epsilon zeta.\n\n\
        eta theta iota kappa lambda mu.\n\n\
        nu xi omicron pi rho sigma.\n\n\
        ```rust\nfn sample() {}\n```\n\n\
        tau upsilon phi chi psi omega.\n";
    let mut incremental = IncrementalMarkdown::new();
    let mut full_bytes = 0usize;
    for end in prefix_ends(doc) {
        let prefix = doc.get(..end).expect("prefix end is a char boundary");
        full_bytes += prefix.len();
        let (a, _, _) = incremental.render(prefix, 80);
        let (b, _, _) = markdown_lines_joined(prefix, 80);
        assert_eq!(a, b, "mismatch at prefix {end}");
    }

    // A whole-source render of every prefix costs O(N²); the incremental
    // renderer must be far below that (it parses each byte only while it is in
    // the still-growing tail).
    assert!(
        incremental.parsed_bytes * 3 < full_bytes,
        "incremental parsed {} bytes vs {} for full renders — prefix was not reused",
        incremental.parsed_bytes,
        full_bytes
    );
    // And it must not have re-rendered the whole source on every frame.
    assert!(
        incremental.full_parses < 20,
        "expected few whole-source fallbacks, got {}",
        incremental.full_parses
    );
}

/// A turn with reasoning and a tool result, so the streaming path exercises the
/// whole assistant block (response ++ reasoning) and the tool section that
/// follows the growing response.
fn streaming_turn(assistant_text: &str) -> Turn {
    Turn {
        created_at: TimestampMs::now(),
        undone: false,
        error: None,
        user_text: Some("please answer".into()),
        assistant_text: Some(assistant_text.to_string()),
        assistant_reasoning: Some("thinking about the answer".into()),
        tool_calls: vec![],
        token_usage: None,
        tool_results: vec![ToolResultRecord {
            call_id: "call-1".into(),
            name: "find".into(),
            content: "one\ntwo\nthree".into(),
            is_error: false,
            invocation_description: "Finding things.".into(),
            image: None,
        }],
        displayed_images: vec![],
        reasoning_artifact: None,
        reasoning_producer: None,
    }
}

#[test]
fn streaming_turn_render_matches_full_for_every_prefix() {
    // The streaming turn renderer must produce byte-identical output to the
    // whole-turn render at every response prefix, including the reasoning
    // header index and the tool-result header ranges that shift as the response
    // grows above them.
    let doc = "Intro paragraph.\n\n```rust\nfn a() {}\n\nfn b() {}\n```\n\n- one\n- two\n\nFinal paragraph.";
    let mut cache = IncrementalMarkdown::new();
    for end in prefix_ends(doc) {
        let turn = streaming_turn(doc.get(..end).expect("prefix end is a char boundary"));
        let streamed = render_turn_lines_streaming(&turn, 60, 65, true, &[false], &mut cache);
        let full = render_turn_lines(&turn, 60, 65, true, &[false]);
        assert_eq!(streamed.lines, full.lines, "lines differ at prefix {end}");
        assert_eq!(streamed.joins, full.joins, "joins differ at prefix {end}");
        assert_eq!(
            streamed.content_ranges, full.content_ranges,
            "content ranges differ at prefix {end}"
        );
        assert_eq!(
            normalize_chrome(&streamed.chrome_ranges),
            normalize_chrome(&full.chrome_ranges),
            "chrome differs at prefix {end}"
        );
        assert_eq!(
            streamed.reasoning_header_idx, full.reasoning_header_idx,
            "reasoning header index differs at prefix {end}"
        );
        assert_eq!(
            streamed.tool_result_header_idxs, full.tool_result_header_idxs,
            "tool header indexes differ at prefix {end}"
        );
    }
}
