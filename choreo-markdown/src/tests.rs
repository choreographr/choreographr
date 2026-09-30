use crate::parse::push_text_content;
use crate::*;

fn item_plain_text(blocks: &[MarkdownBlock]) -> String {
    let mut text = String::new();
    for block in blocks {
        match block {
            MarkdownBlock::Paragraph(content) | MarkdownBlock::Heading { content, .. } => {
                text.push_str(&inline_text(content));
            }
            MarkdownBlock::CodeBlock { code, .. } => text.push_str(code),
            MarkdownBlock::BlockQuote(content) => text.push_str(&item_plain_text(content)),
            MarkdownBlock::List { items, .. } => {
                for item in items {
                    text.push_str(&item_plain_text(item));
                }
            }
            MarkdownBlock::Table { .. } | MarkdownBlock::Rule => {}
        }
    }
    text
}

#[test]
fn markdown_parser_supports_common_llm_output() {
    let document = MarkdownDocument::parse(
        "# Heading\n\nA **bold** [link](https://example.com).\n\n- one\n- two\n\n```rs\nfn main() {}\n```",
    );

    assert!(matches!(document.blocks[0], MarkdownBlock::Heading { .. }));
    assert!(matches!(document.blocks[1], MarkdownBlock::Paragraph(_)));
    assert!(matches!(document.blocks[2], MarkdownBlock::List { .. }));
    assert!(matches!(
        document.blocks[3],
        MarkdownBlock::CodeBlock { .. }
    ));

    let MarkdownBlock::List { items, .. } = &document.blocks[2] else {
        panic!("expected list block");
    };
    assert_eq!(items.len(), 2);
    assert_eq!(item_plain_text(&items[0]), "one");
    assert_eq!(item_plain_text(&items[1]), "two");
}

#[test]
fn markdown_parser_preserves_task_list_item_text() {
    let document = MarkdownDocument::parse("- [x] done\n- [ ] todo");

    let MarkdownBlock::List { items, .. } = &document.blocks[0] else {
        panic!("expected list block");
    };

    assert_eq!(item_plain_text(&items[0]), "[x] done");
    assert_eq!(item_plain_text(&items[1]), "[ ] todo");
}

#[test]
fn markdown_parser_preserves_nested_tight_list_text() {
    let document = MarkdownDocument::parse("- parent\n  - child\n  - child 2");

    let MarkdownBlock::List { items, .. } = &document.blocks[0] else {
        panic!("expected top-level list block");
    };

    assert_eq!(item_plain_text(&items[0]), "parentchildchild 2");

    let nested_list = items[0]
        .iter()
        .find_map(|block| match block {
            MarkdownBlock::List { items, .. } => Some(items),
            _ => None,
        })
        .expect("expected nested list");
    assert_eq!(item_plain_text(&nested_list[0]), "child");
    assert_eq!(item_plain_text(&nested_list[1]), "child 2");
}

#[test]
fn markdown_parser_supports_tables() {
    let document =
        MarkdownDocument::parse("| Name | Role |\n|:--|--:|\n| Ada | Math |\n| Grace | CS |");

    assert!(matches!(document.blocks[0], MarkdownBlock::Table { .. }));
}

#[test]
fn markdown_html_escapes_unsafe_html_and_links() {
    let safe_html = render_markdown_html("[ok](https://example.com)");
    let unsafe_html = render_markdown_html("[x](javascript:alert(1))");

    assert!(safe_html.contains("https://example.com"));
    assert!(!unsafe_html.contains("javascript:alert(1)"));
    assert!(!unsafe_html.contains("href="));
}

#[test]
fn markdown_html_renders_tables() {
    let html = render_markdown_html("| Name | Role |\n|---|---|\n| Ada | Math |\n| Grace | CS |");

    assert!(html.contains("<table>"));
    assert!(html.contains("<td>Ada</td>"));
    assert!(html.contains("<td>Grace</td>"));
}

#[test]
fn markdown_round_trip_paragraph() {
    let input = "Hello **world**.";
    let doc = MarkdownDocument::parse(input);
    assert_eq!(doc.to_markdown(), input);
}

#[test]
fn markdown_round_trip_heading() {
    let input = "# Heading\n\n## Subheading\n\n### Deep";
    let doc = MarkdownDocument::parse(input);
    assert_eq!(doc.to_markdown(), input);
}

#[test]
fn markdown_round_trip_code_block() {
    let input = "```rust\nfn main() {\n    println!(\"hi\");\n}\n```";
    let doc = MarkdownDocument::parse(input);
    assert_eq!(doc.to_markdown(), input);
}

#[test]
fn markdown_round_trip_list() {
    let input = "- one\n- two\n- three";
    let doc = MarkdownDocument::parse(input);
    assert_eq!(doc.to_markdown(), input);
}

#[test]
fn markdown_round_trip_table() {
    let input = "| Name | Role |\n|---|---|\n| Ada | Math |\n| Grace | CS |";
    let doc = MarkdownDocument::parse(input);
    assert_eq!(doc.to_markdown(), input);
}

#[test]
fn markdown_round_trip_strikethrough() {
    let input = "~~struck~~";
    let doc = MarkdownDocument::parse(input);
    assert_eq!(doc.to_markdown(), input);
}

#[test]
fn markdown_parser_supports_strikethrough() {
    let document = MarkdownDocument::parse("~~struck~~");

    let MarkdownBlock::Paragraph(content) = &document.blocks[0] else {
        panic!("expected paragraph");
    };
    assert!(matches!(content[0], MarkdownInline::Strikethrough(_)));
    assert_eq!(inline_text(content), "struck");
}

#[test]
fn markdown_round_trip_math() {
    let input = "$x^2$";
    let doc = MarkdownDocument::parse(input);
    assert_eq!(doc.to_markdown(), input);
}

#[test]
fn markdown_parser_supports_math() {
    let document = MarkdownDocument::parse("$x^2$ and $$\\sum$$");

    let MarkdownBlock::Paragraph(content) = &document.blocks[0] else {
        panic!("expected paragraph");
    };
    assert!(matches!(content[0], MarkdownInline::InlineMath(_)));
    assert!(matches!(content[2], MarkdownInline::DisplayMath(_)));
    assert_eq!(inline_text(content), "x^2 and \\sum");
}

#[test]
fn prose_with_two_dollars_stays_literal_text() {
    // Two `$` signs in running prose must not be captured as one giant
    // inline-math span (which the TUI would tint and whitespace-collapse).
    let input = "Also: Fly now has a $0 to start? They removed the free tier \
                     in Oct 2024; need to verify. This is time-sensitive($) I should \
                     flag uncertainty.";
    let document = MarkdownDocument::parse(input);
    let MarkdownBlock::Paragraph(content) = &document.blocks[0] else {
        panic!("expected paragraph");
    };
    assert!(
        !content
            .iter()
            .any(|node| matches!(node, MarkdownInline::InlineMath(_))),
        "prose was misclassified as inline math: {content:?}"
    );
    assert_eq!(inline_text(content), input.replace('\n', ""));
}

#[test]
fn currency_pair_stays_literal_text() {
    // Currency with interior whitespace carries no math signal, so it stays
    // literal text even without sentence punctuation.
    let document = MarkdownDocument::parse("it costs $5 and $10 today");
    let MarkdownBlock::Paragraph(content) = &document.blocks[0] else {
        panic!("expected paragraph");
    };
    assert!(
        !content
            .iter()
            .any(|node| matches!(node, MarkdownInline::InlineMath(_))),
        "currency was misclassified as inline math: {content:?}"
    );
    assert_eq!(inline_text(content), "it costs $5 and $10 today");
}

#[test]
fn glued_prose_pair_stays_literal_text() {
    // Both `$` glued to non-space (pulldown accepts the pair) and content
    // with interior whitespace but no math signal is prose, not math — the
    // shapes the old prose-detection heuristic missed.
    for input in ["see $x and y$ here", "in ($HOME) and ($PATH) now"] {
        let document = MarkdownDocument::parse(input);
        let MarkdownBlock::Paragraph(content) = &document.blocks[0] else {
            panic!("expected paragraph");
        };
        assert!(
            !content
                .iter()
                .any(|node| matches!(node, MarkdownInline::InlineMath(_))),
            "prose was misclassified as inline math: {input:?} -> {content:?}"
        );
        assert_eq!(inline_text(content), input);
    }
}

#[test]
fn arithmetic_prose_with_a_signal_stays_literal_text() {
    // A glued `$…$` pair whose content is a natural-language note carrying an
    // arithmetic signal (`=` / `/` / `×`) is prose, not an equation — the
    // operator alone must not promote it to math.
    for input in [
        "calc $5 Story, shown only, now: revenue = 4×$ tail",
        "calc $1/take(custom-move floor), shown only, now: revenue = 4×$ tail",
    ] {
        let document = MarkdownDocument::parse(input);
        let MarkdownBlock::Paragraph(content) = &document.blocks[0] else {
            panic!("expected paragraph");
        };
        assert!(
            !content
                .iter()
                .any(|node| matches!(node, MarkdownInline::InlineMath(_))),
            "arithmetic prose was misclassified as math: {input:?} -> {content:?}"
        );
        assert_eq!(inline_text(content), input);
    }
}

#[test]
fn tex_command_keeps_math_despite_words() {
    // A backslash command is an unambiguous math marker, so a `\text{…}` word
    // inside an equation does not trip the natural-language-word rule.
    let document = MarkdownDocument::parse("value $v = \\text{shown only}$ end");
    let MarkdownBlock::Paragraph(content) = &document.blocks[0] else {
        panic!("expected paragraph");
    };
    assert!(
        content
            .iter()
            .any(|node| matches!(node, MarkdownInline::InlineMath(_))),
        "real math was not captured: {content:?}"
    );
}

#[test]
fn whitespace_free_number_span_stays_literal_text() {
    // A whitespace-free `$…$` span of only digits and symbols (`0.60→`, the
    // shape of a price range) carries no variable or TeX command, so it is data
    // rather than an equation — pulldown still pairs the `$`s.
    for input in ["p 768p $0.60→$ q", "p 1080p $1.20→$ q"] {
        let document = MarkdownDocument::parse(input);
        let MarkdownBlock::Paragraph(content) = &document.blocks[0] else {
            panic!("expected paragraph");
        };
        assert!(
            !content
                .iter()
                .any(|node| matches!(node, MarkdownInline::InlineMath(_))),
            "a price range was misclassified as math: {input:?} -> {content:?}"
        );
        assert_eq!(inline_text(content), input);
    }
}

#[test]
fn whitespace_free_letter_span_keeps_math() {
    // A whitespace-free span that carries a variable or a TeX command is still
    // math — the refinement must not drop ordinary short expressions.
    for input in ["$x^2$", "$n_i$", "$v_0$", "$\\alpha$"] {
        let document = MarkdownDocument::parse(input);
        let MarkdownBlock::Paragraph(content) = &document.blocks[0] else {
            panic!("expected paragraph");
        };
        assert!(
            content
                .iter()
                .any(|node| matches!(node, MarkdownInline::InlineMath(_))),
            "short math was dropped: {input:?} -> {content:?}"
        );
    }
}

#[test]
fn math_signal_keeps_inline_math() {
    // A whitespace-bearing span with a positive math signal stays math —
    // including digit-led expressions the old leading-digit rule dropped.
    for input in ["$2x + 1$", "$a - b$", "$x = y$", "$n < m$"] {
        let document = MarkdownDocument::parse(input);
        let MarkdownBlock::Paragraph(content) = &document.blocks[0] else {
            panic!("expected paragraph");
        };
        assert!(
            content
                .iter()
                .any(|node| matches!(node, MarkdownInline::InlineMath(_))),
            "real math was not captured: {input:?} -> {content:?}"
        );
    }
}

#[test]
fn sentence_break_vetoes_a_math_signal() {
    // A sentence boundary on top of a signal is still prose: the veto wins.
    let document = MarkdownDocument::parse("$x = 5. Then $y");
    let MarkdownBlock::Paragraph(content) = &document.blocks[0] else {
        panic!("expected paragraph");
    };
    assert!(
        !content
            .iter()
            .any(|node| matches!(node, MarkdownInline::InlineMath(_))),
        "prose was misclassified as inline math: {content:?}"
    );
}

#[test]
fn genuine_inline_math_is_preserved() {
    // A real expression — a script signal with no sentence punctuation — is
    // still captured as math and round-trips through `$…$`.
    let document = MarkdownDocument::parse("solve $x^2 + 1$ now");
    let MarkdownBlock::Paragraph(content) = &document.blocks[0] else {
        panic!("expected paragraph");
    };
    assert!(
        content
            .iter()
            .any(|node| matches!(node, MarkdownInline::InlineMath(_))),
        "real math was not captured: {content:?}"
    );
    assert_eq!(document.to_markdown(), "solve $x^2 + 1$ now");
}

#[test]
fn snake_case_identifier_stays_literal_text() {
    // `_` between two letters is an identifier, not a subscript: it must not
    // count as a math signal, so `$`-heavy prose with a snake_case word stays
    // literal text rather than being whitespace-collapsed by the printer.
    let document = MarkdownDocument::parse("rename $my_var and other_var$ now");
    let MarkdownBlock::Paragraph(content) = &document.blocks[0] else {
        panic!("expected paragraph");
    };
    assert!(
        !content
            .iter()
            .any(|node| matches!(node, MarkdownInline::InlineMath(_))),
        "snake_case prose was misclassified as math: {content:?}"
    );
}

#[test]
fn script_with_operand_keeps_inline_math() {
    // A whitespace-bearing span whose only signal is a script *attached to an
    // operand* (`^2`, `_1`, `^{…}`) stays math — the refinement must not drop
    // genuine scripts while excluding `snake_case`.
    for input in ["$x^2 y$", "$a_1 b$", "$x^{n} y$"] {
        let document = MarkdownDocument::parse(input);
        let MarkdownBlock::Paragraph(content) = &document.blocks[0] else {
            panic!("expected paragraph");
        };
        assert!(
            content
                .iter()
                .any(|node| matches!(node, MarkdownInline::InlineMath(_))),
            "script math was dropped: {input:?} -> {content:?}"
        );
    }
}

#[test]
fn display_math_prose_stays_literal_text() {
    // Display math (`$$…$$`) is subject to the same whitespace-adjacency rule,
    // so `$$`-heavy prose must be reclassified too — not just inline math.
    let document = MarkdownDocument::parse("a $$x and y$$ b");
    let MarkdownBlock::Paragraph(content) = &document.blocks[0] else {
        panic!("expected paragraph");
    };
    assert!(
        !content
            .iter()
            .any(|node| matches!(node, MarkdownInline::DisplayMath(_))),
        "prose was misclassified as display math: {content:?}"
    );
}

#[test]
fn genuine_display_math_is_preserved() {
    // A real display equation still round-trips through `$$…$$`.
    let document = MarkdownDocument::parse("a\n\n$$x^2 + 1$$\n\nb");
    let has_display = document.blocks.iter().any(|block| match block {
        MarkdownBlock::Paragraph(content) => content
            .iter()
            .any(|node| matches!(node, MarkdownInline::DisplayMath(_))),
        _ => false,
    });
    assert!(
        has_display,
        "real display math was not captured: {document:?}"
    );
}

#[test]
fn html_prose_with_two_dollars_is_not_math() {
    // The HTML renderer runs the same math guard, so `$`-heavy prose must not
    // become a `math-inline` span there either — the literal text survives.
    let html = render_markdown_html("also $x and y$ here");
    assert!(!html.contains("math-inline"), "prose became math: {html}");
    assert!(html.contains("$x and y$"), "literal text lost: {html}");
}

#[test]
fn markdown_display_from_str() {
    let input = "# Hello\n\nWorld.";
    let doc: MarkdownDocument = input.parse().unwrap();
    assert_eq!(doc.to_string(), input);
}

#[test]
fn push_text_content_merges_adjacent_text() {
    let mut content: Vec<MarkdownInline> = Vec::new();
    push_text_content(&mut content, "I");
    push_text_content(&mut content, "'");
    push_text_content(&mut content, "ll");
    assert_eq!(content.len(), 1);
    assert_eq!(content[0], MarkdownInline::Text("I'll".to_string()));
}

#[test]
fn push_text_content_creates_new_text_when_last_is_not_text() {
    let mut content: Vec<MarkdownInline> = Vec::new();
    content.push(MarkdownInline::Code("x".to_string()));
    push_text_content(&mut content, "hello");
    assert_eq!(content.len(), 2);
    assert_eq!(content[1], MarkdownInline::Text("hello".to_string()));
}

#[test]
fn push_text_content_creates_new_text_when_empty() {
    let mut content: Vec<MarkdownInline> = Vec::new();
    push_text_content(&mut content, "hello");
    assert_eq!(content.len(), 1);
    assert_eq!(content[0], MarkdownInline::Text("hello".to_string()));
}

#[test]
fn smart_punctuation_merges_text_around_inline_boundary() {
    // Smart punctuation triggers text splitting around inline
    // boundaries (e.g. the ` before a code span).  The merging
    // logic should reassemble adjacent text into single nodes.
    let doc = MarkdownDocument::parse("I'll `code` there.");
    let MarkdownBlock::Paragraph(content) = &doc.blocks[0] else {
        panic!("expected paragraph");
    };
    // With smart punctuation "I'll" becomes one text event
    // containing the curly apostrophe.
    assert_eq!(content[0], MarkdownInline::Text("I\u{2019}ll ".to_string()));
    assert_eq!(content[1], MarkdownInline::Code("code".to_string()));
    assert_eq!(content[2], MarkdownInline::Text(" there.".to_string()));
    assert_eq!(content.len(), 3);
}

#[test]
fn smart_punctuation_round_trip() {
    // Smart punctuation transforms straight quotes/apostrophes to
    // their typographic equivalents, so the round-trip output
    // contains curly quotes.
    let input = "I'll be there.";
    let doc = MarkdownDocument::parse(input);
    assert_eq!(doc.to_markdown(), "I\u{2019}ll be there.");
}

#[test]
fn smart_quotes_round_trip() {
    let input = "She said \"hello\" and left.";
    let doc = MarkdownDocument::parse(input);
    assert_eq!(
        doc.to_markdown(),
        "She said \u{201c}hello\u{201d} and left."
    );
}

#[test]
fn text_never_adjacent_across_inline_boundary() {
    // Code nodes should not merge with adjacent text.
    let doc = MarkdownDocument::parse("a `code` b");
    let MarkdownBlock::Paragraph(content) = &doc.blocks[0] else {
        panic!("expected paragraph");
    };
    assert_eq!(content.len(), 3);
    assert_eq!(content[0], MarkdownInline::Text("a ".to_string()));
    assert_eq!(content[1], MarkdownInline::Code("code".to_string()));
    assert_eq!(content[2], MarkdownInline::Text(" b".to_string()));
}

// ── render_math_pretty ────────────────────────────────────────────────

#[test]
fn math_pretty_basic_powers_and_subscripts() {
    assert_eq!(render_math_pretty("x^2 + y_1"), "x²+y₁");
}

#[test]
fn math_pretty_frac() {
    assert_eq!(render_math_pretty("\\frac{1}{2}"), "1/2");
    assert_eq!(render_math_pretty("\\frac{x+1}{x-1}"), "(x+1)/(x-1)");
    assert_eq!(render_math_pretty("\\frac{dy}{dx}"), "dy/dx");
}

#[test]
fn math_pretty_nested_frac() {
    assert_eq!(render_math_pretty("\\frac{\\frac{a}{b}}{c}"), "(a/b)/c");
}

#[test]
fn math_pretty_sqrt() {
    assert_eq!(render_math_pretty("\\sqrt{x}"), "√x");
    assert_eq!(render_math_pretty("\\sqrt{x+y}"), "√(x+y)");
    assert_eq!(render_math_pretty("\\sqrt[3]{8}"), "∛8");
}

#[test]
fn math_pretty_greek_and_relations() {
    assert_eq!(render_math_pretty("\\alpha + \\beta \\le \\gamma"), "α+β≤γ");
}

#[test]
fn math_pretty_sum_with_limits() {
    assert_eq!(render_math_pretty("\\sum_{i=1}^{n}"), "∑ᵢ₌₁ⁿ");
}

#[test]
fn math_pretty_limits_flatten_arrows() {
    // `\to` has no subscript glyph, but the surrounding letters/digits do;
    // the partial mapping keeps the common `\lim_{x \to 0}` readable.
    assert_eq!(render_math_pretty("\\lim_{x \\to 0}"), "limₓ→₀");
}

#[test]
fn math_pretty_exponent_group() {
    assert_eq!(render_math_pretty("e^{-x}"), "e⁻ˣ");
    assert_eq!(render_math_pretty("x^{n+1}"), "xⁿ⁺¹");
}

#[test]
fn math_pretty_text_command_keeps_prose() {
    assert_eq!(render_math_pretty("\\text{if } n"), "if n");
}

#[test]
fn math_pretty_mathbb() {
    assert_eq!(render_math_pretty("x \\in \\mathbb{R}"), "x∈ℝ");
    assert_eq!(render_math_pretty("\\mathbb{Z}"), "ℤ");
}

#[test]
fn math_pretty_fences() {
    assert_eq!(render_math_pretty("\\left( x \\right)"), "(x)");
    assert_eq!(render_math_pretty("\\left\\{ x \\right\\}"), "{x}");
}

#[test]
fn math_pretty_escaped_chars() {
    assert_eq!(render_math_pretty("\\{a\\}"), "{a}");
}

#[test]
fn math_pretty_primes() {
    assert_eq!(render_math_pretty("f''(x)"), "f″(x)");
}

#[test]
fn math_pretty_cases_environment() {
    assert_eq!(
        render_math_pretty(
            "\\begin{cases} 1 & \\text{if } x \\\\ 0 & \\text{otherwise} \\end{cases}"
        ),
        "{ 1 if x; 0 otherwise }"
    );
}

#[test]
fn math_pretty_unknown_command_falls_back_to_source() {
    assert_eq!(render_math_pretty("\\foo{x}"), "\\foo{x}");
}

#[test]
fn math_pretty_unknown_script_keeps_literal() {
    // `q` has no subscript glyph, and `b` aborts the `ab` group's mapping;
    // the semantic letters keep their literal form rather than emitting
    // a misleading partial subscript.
    assert_eq!(render_math_pretty("x_q"), "x_q");
    assert_eq!(render_math_pretty("x_{ab}"), "x_(ab)");
    // `k`, `+` and digits all have subscript glyphs, so this stays readable.
    assert_eq!(render_math_pretty("x_{k+1}"), "xₖ₊₁");
}

#[test]
fn math_pretty_empty_and_overlong_input() {
    assert_eq!(render_math_pretty(""), "");
    let long = "a".repeat(5000);
    assert_eq!(render_math_pretty(&long), long);
}

#[test]
fn math_pretty_is_total_on_adversarial_input() {
    // Unbalanced braces, stray delimiters, and truncated environments must
    // never panic and must not collapse a non-empty input to nothing.
    for s in [
        "\\begin{matrix} a & b",
        "\\frac{1}{2",
        "\\left(",
        "\\sqrt[3]{2",
        "{}",
        "\\\\",
    ] {
        let out = render_math_pretty(s);
        assert!(!out.is_empty(), "unexpected collapse: {out:?}");
    }
}
