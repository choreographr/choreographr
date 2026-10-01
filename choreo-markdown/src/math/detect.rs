use pulldown_cmark::{CowStr, Event};

/// Reclassify a `$…$` / `$$…$$` span that is prose (or currency, or a shell
/// variable) rather than mathematics back into the literal text the author
/// typed.
///
/// pulldown-cmark's math rule is deliberately loose: any `$` not followed by
/// whitespace *opens* a span and any `$` not preceded by whitespace *closes*
/// it. `$`-heavy prose therefore collapses into one giant math span — e.g.
/// `… with a $0 to start? … time-sensitive($) I should …` becomes a single span
/// whose inner text is several whole sentences. The TUI then tints it and
/// routes it through [`render_math_pretty`], which drops inter-token
/// whitespace, rendering the paragraph as a run-together smear. Re-emit such
/// spans as literal `$…$` / `$$…$$` text so the prose survives; genuine
/// equations are untouched (see [`looks_like_math`]).
pub(crate) fn normalize_math_event(event: Event<'_>) -> Event<'_> {
    match event {
        Event::InlineMath(content) if !looks_like_math(content.as_ref()) => {
            Event::Text(CowStr::from(format!("${}$", content.as_ref())))
        }
        // Display math (`$$…$$`) is subject to the same whitespace-adjacency
        // rule and the same failure mode, so it goes through the same guard.
        Event::DisplayMath(content) if !looks_like_math(content.as_ref()) => {
            Event::Text(CowStr::from(format!("$${}$$", content.as_ref())))
        }
        other => other,
    }
}

/// Whether a pulldown-cmark math span (`InlineMath` or `DisplayMath`) really
/// looks like mathematics.
///
/// Because pulldown-cmark matches math purely on whitespace adjacency, `$`-heavy
/// prose — currency, shell variables, arithmetic notes, and meta-discussion of
/// the math syntax itself — yields spurious spans that the TUI would tint and
/// whitespace-collapse. Recognising *prose* is an open-ended list the reasoning
/// corpus kept defeating (glued pairs such as `$x and y$`, `($HOME) and
/// ($PATH)`, or `$revenue = 4×$`), so the default is **inverted: a span is math
/// only when it looks like math**:
///
/// * a whitespace-free span is math only when it carries a variable or TeX
///   command (an ASCII letter or `\` — see [`contains_math_letter`]); a price
///   or range such as `0.60→` is left literal, and because nothing can be
///   collapsed a wrong call here costs at most a tint anyway;
/// * a span carrying a TeX command (`\`) is math — a backslash is an
///   unambiguous math marker;
/// * a span containing a natural-language word (a run of at least
///   [`MIN_WORD_LEN`] ASCII letters — see [`contains_natural_word`]) is prose,
///   even when it also carries an operator signal such as `=`, so an arithmetic
///   note (`5 Story, shown only, now: revenue = 4×`) is not taken for an
///   equation;
/// * otherwise a whitespace-bearing span is math only if it carries a positive
///   math signal (see [`contains_math_signal`]);
/// * a sentence terminator followed by whitespace (`. `, `? `, `! `) is a hard
///   prose veto that overrides everything else.
fn looks_like_math(content: &str) -> bool {
    if has_sentence_break(content) {
        return false;
    }
    if !content.contains(char::is_whitespace) {
        return contains_math_letter(content);
    }
    if content.contains('\\') {
        return true;
    }
    if contains_natural_word(content) {
        return false;
    }
    contains_math_signal(content)
}

/// Whether a whitespace-free span carries a variable or a TeX command — an
/// ASCII letter or a backslash. A span of only digits and symbols (a price, a
/// percentage, a range like `0.60→` or `5%+50¢`) has neither, so it is prose or
/// data rather than an equation and stays literal.
fn contains_math_letter(content: &str) -> bool {
    content
        .chars()
        .any(|c| c.is_ascii_alphabetic() || c == '\\')
}

/// Shortest run of ASCII letters counted as a natural-language word. Two-letter
/// runs are left alone so short math tokens (`dy`, `mv`, `dx`) survive; three
/// catches the common prose words (`and`, `the`, `now`, `revenue`).
const MIN_WORD_LEN: usize = 3;

/// Whether `content` contains a run of at least [`MIN_WORD_LEN`] consecutive
/// ASCII letters — the mark of a natural-language word. Only whitespace-bearing
/// content that carries no TeX command reaches this, so a backslash-named macro
/// (`\lim`, `\mathbb`) is never examined here.
fn contains_natural_word(content: &str) -> bool {
    let mut run = 0usize;
    for c in content.chars() {
        if c.is_ascii_alphabetic() {
            run += 1;
            if run >= MIN_WORD_LEN {
                return true;
            }
        } else {
            run = 0;
        }
    }
    false
}

/// A sentence terminator immediately followed by whitespace (`. `, `? `, `! `)
/// — a sentence boundary that a single equation does not contain. Multibyte
/// UTF-8 is safe here: continuation bytes are `>= 0x80` and so never equal the
/// ASCII punctuation or whitespace bytes being matched.
fn has_sentence_break(content: &str) -> bool {
    content
        .as_bytes()
        .windows(2)
        .any(|pair| matches!(pair, [b'.' | b'?' | b'!', ws] if ws.is_ascii_whitespace()))
}

/// A positive gauge that a whitespace-bearing math span is an equation rather
/// than prose: a TeX command (`\`), a relation (`=`/`<`/`>`), a script that
/// attaches to an operand (`^`/`_` — see below), or an arithmetic operator
/// (`+`/`-`/`/`) used as a separator.
///
/// The arithmetic operator set only counts when *space-adjacent*, so a hyphen
/// inside a word (`time-sensitive`) is not mistaken for a minus sign. `*` is
/// deliberately excluded: it is markdown's emphasis delimiter (`**bold**`,
/// `*italic*`), which clings to words and would otherwise promote prose such as
/// `3k/mo, so a **` to an equation, while multiplication is written `×` or
/// `\times`. Likewise `^`/`_` only counts as a script when it attaches to an
/// operand on at least one side — a digit, `{`, or TeX command immediately
/// after it, or a digit, `)`, or `}` immediately before it — so an identifier
/// such as `snake_case` is not mistaken for a subscript.
fn contains_math_signal(content: &str) -> bool {
    // The character immediately before the one under inspection, tracked so the
    // adjacency tests need no indexing or look-behind.
    let mut prev: Option<char> = None;
    let mut chars = content.chars().peekable();
    while let Some(c) = chars.next() {
        match c {
            '\\' | '=' | '<' | '>' => return true,
            '^' | '_' => {
                let next_is_operand = chars
                    .peek()
                    .is_some_and(|&next| next.is_ascii_digit() || next == '{' || next == '\\');
                let prev_is_base = prev.is_some_and(|p| p.is_ascii_digit() || p == ')' || p == '}');
                if next_is_operand || prev_is_base {
                    return true;
                }
            }
            '+' | '-' | '/' => {
                let next_is_space = chars.peek().is_some_and(|&next| next.is_whitespace());
                if prev.is_some_and(char::is_whitespace) || next_is_space {
                    return true;
                }
            }
            _ => {}
        }
        prev = Some(c);
    }
    false
}
