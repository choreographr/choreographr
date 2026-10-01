use super::super::*;

// ── math rendering ───────────────────────────────────────────────────

#[test]
fn inline_math_renders_pretty_unicode() {
    let result = markdown_lines("solve $x^2 + 1$ now", 80);
    assert_eq!(result.len(), 1);
    assert_eq!(result[0].to_string(), "solve x²+1 now");
}

#[test]
fn inline_math_limit_keeps_arrow() {
    // The arrow has no subscript glyph, but the partial mapping keeps the
    // common `\lim_{x \to 0}` readable as `limₓ→₀`.
    let result = markdown_lines("The limit $\\lim_{x \\to 0} x = 0$ holds.", 100);
    assert_eq!(result.len(), 1);
    assert_eq!(result[0].to_string(), "The limit limₓ→₀x=0 holds.");
}

#[test]
fn display_math_renders_centered_on_its_own_line() {
    let result = markdown_lines("$$\\sum_{i=1}^{n} i$$", 80);
    assert_eq!(result.len(), 1);
    let line = result[0].to_string();
    let equation = "∑ᵢ₌₁ⁿi";
    assert_eq!(line.trim_start(), equation);
    // (80 − 6) / 2 = 37 columns of leading padding for a 6-wide equation.
    assert_eq!(line.len() - line.trim_start().len(), 37);
}

#[test]
fn display_math_breaks_out_of_flowing_text() {
    let result = markdown_lines("left $$a+b$$ right", 80);
    let rendered: Vec<String> = result.iter().map(ToString::to_string).collect();
    assert_eq!(rendered[0], "left");
    assert_eq!(rendered[2], "right");
    assert_eq!(rendered[1].trim_start(), "a+b");
}

#[test]
fn display_math_wraps_when_too_wide() {
    let result = markdown_lines("$$\\frac{x+1}{x-1} = 2$$", 10);
    let joined: String = result.iter().map(ToString::to_string).collect();
    assert_eq!(joined, "(x+1)/(x-1)=2");
    assert!(result.len() >= 2, "expected the equation to wrap");
    for line in &result {
        assert!(line.width() <= 10, "wrapped line too wide");
    }
}

#[test]
fn dollar_pair_in_prose_does_not_render_as_math() {
    // Regression: two `$` signs in ordinary prose used to be captured as one
    // inline-math span, which the renderer tinted yellow and whitespace-collapsed
    // (`render_math_pretty`), turning the sentence into a run-together smear.
    // It must instead render as literal text with its spacing intact and no
    // math styling.
    let text = "Also: Fly now has a $0 to start? They removed the free tier in \
                Oct 2024; need to verify. This is time-sensitive($) I should flag \
                uncertainty.";
    let result = markdown_lines(text, 200);
    let joined = result
        .iter()
        .map(ToString::to_string)
        .collect::<Vec<_>>()
        .join(" ");
    assert!(
        joined.contains("start? They removed") && joined.contains("time-sensitive($) I should"),
        "prose spacing was corrupted: {joined:?}"
    );
    let tinted = result
        .iter()
        .flat_map(|line| &line.spans)
        .any(|span| span.style.fg == Some(Color::Yellow));
    assert!(!tinted, "prose was tinted as inline math: {joined:?}");
}

#[test]
fn glued_dollar_pair_in_prose_does_not_render_as_math() {
    // Regression follow-up: when both `$` are glued to non-space pulldown still
    // pairs them, and a signal-less heuristic let prose through (`$x and y$`,
    // `($HOME) and ($PATH)`). A later case added arithmetic notes whose text
    // carries an operator signal (`=`, `/`, `×`) but is still prose. All must
    // stay literal text rather than render as a whitespace-collapsed smear.
    for text in [
        "see $x and y$ here",
        "in ($HOME) and ($PATH) now",
        "calc $5 Story, shown only, now: revenue = 4×$ tail",
        "calc $1/take(custom-move floor), shown only, now: revenue = 4×$ tail",
    ] {
        let result = markdown_lines(text, 200);
        let joined = result
            .iter()
            .map(ToString::to_string)
            .collect::<Vec<_>>()
            .join(" ");
        assert_eq!(joined, text, "prose was corrupted");
        let tinted = result
            .iter()
            .flat_map(|line| &line.spans)
            .any(|span| span.style.fg == Some(Color::Yellow));
        assert!(!tinted, "prose was tinted as inline math: {joined:?}");
    }
}

#[test]
fn whitespace_free_number_span_is_not_math_tinted() {
    // A `$…$` span of only digits and symbols (a price range like `0.60→`) has
    // no variable or command, so it must not be tinted as inline math.
    for text in ["768p $0.60→$1.20", "1080p $1.20→$2.40"] {
        let result = markdown_lines(text, 200);
        let joined = result
            .iter()
            .map(ToString::to_string)
            .collect::<Vec<_>>()
            .join(" ");
        assert_eq!(joined, text, "text was altered");
        let tinted = result
            .iter()
            .flat_map(|line| &line.spans)
            .any(|span| span.style.fg == Some(Color::Yellow));
        assert!(
            !tinted,
            "a price range was tinted as inline math: {joined:?}"
        );
    }
}

#[test]
fn markdown_emphasis_span_is_not_math_tinted() {
    // A `**` bold marker swallowed into a `$…$` span must not promote the span
    // to math (which would tint and whitespace-collapse it to `3k/mo,soa**`).
    let text = "founders ~ $3k/mo, so a **$5k–$10k upfront** fee";
    let result = markdown_lines(text, 200);
    let joined = result
        .iter()
        .map(ToString::to_string)
        .collect::<Vec<_>>()
        .join(" ");
    assert_eq!(joined, text, "text was altered");
    let tinted = result
        .iter()
        .flat_map(|line| &line.spans)
        .any(|span| span.style.fg == Some(Color::Yellow));
    assert!(
        !tinted,
        "emphasis prose was tinted as inline math: {joined:?}"
    );
}
