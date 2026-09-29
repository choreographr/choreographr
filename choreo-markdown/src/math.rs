//! LaTeX math handling: deciding whether a pulldown-cmark `$…$` / `$$…$$` span
//! is real mathematics or prose, and pretty-printing a real expression to a
//! best-effort Unicode approximation for terminal display.
//!
//! The classifier ([`looks_like_math`]) exists because pulldown-cmark's math
//! rule keys only on whitespace adjacency, so `$`-heavy prose yields spurious
//! spans; [`normalize_math_event`] re-emits those as literal text. The printer
//! ([`render_math_pretty`]) is a total, depth-bounded, table-driven parser that
//! falls back to raw source for anything it cannot map.

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
/// prose — currency, shell variables, and meta-discussion of the math syntax
/// itself — yields spurious spans that the TUI would tint and whitespace-
/// collapse. Recognising *prose* is an open-ended list the reasoning corpus kept
/// defeating (glued pairs such as `$x and y$` or `($HOME) and ($PATH)`), so the
/// default is **inverted: a span is math only when it looks like math**:
///
/// * a span with no interior whitespace is always math — there is nothing for
///   the pretty-printer to collapse, so a wrong call costs at most a tint;
/// * a span with interior whitespace is math only if it carries a positive math
///   signal (see [`contains_math_signal`]); otherwise it is literal text;
/// * a sentence terminator followed by whitespace (`. `, `? `, `! `) is a hard
///   prose veto that overrides even a present signal.
fn looks_like_math(content: &str) -> bool {
    if has_sentence_break(content) {
        return false;
    }
    if !content.contains(char::is_whitespace) {
        return true;
    }
    contains_math_signal(content)
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
/// (`+`/`-`/`*`/`/`) used as a separator.
///
/// The arithmetic operator set only counts when *space-adjacent*, so a hyphen
/// inside a word (`time-sensitive`) is not mistaken for a minus sign. Likewise
/// `^`/`_` only counts as a script when it attaches to an operand on at least
/// one side — a digit, `{`, or TeX command immediately after it, or a digit,
/// `)`, or `}` immediately before it — so an identifier such as `snake_case`
/// is not mistaken for a subscript.
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
            '+' | '-' | '*' | '/' => {
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

// ── LaTeX math → Unicode pretty-printing ─────────────────────────────────
//
// The parser captures `$...$` / `$$...$$` in the AST as `InlineMath` /
// `DisplayMath`, but raw LaTeX source is not pleasant to read in a terminal.
// [`render_math_pretty`] re-renders the inner TeX into a best-effort Unicode
// approximation (Greek letters, operator symbols, sub/superscripts, fractions
// and environments) so chat transcripts display math inline instead of as
// source text.
//
// The renderer is a small, depth-bounded, table-driven parser over the math
// source.  It is a *total* function: every construct it does not understand
// (unknown commands, unbalanced braces, deeply nested arguments) falls through
// to its raw source so output degrades gracefully instead of losing data.

/// Maximum accepted input length (bytes); longer input is returned verbatim to
/// bound the work done on any single expression.
const MAX_MATH_INPUT_LEN: usize = 4096;

/// Maximum nesting depth for grouped arguments and environments.  Deeply
/// nested input (pathological or adversarial) hits the cap and renders the
/// remainder verbatim, bounding recursion and stack use.
const MAX_MATH_DEPTH: u32 = 24;

/// Render a LaTeX math expression — the inner text of `$...$` or `$$...$$`
/// with the delimiters stripped — into a best-effort Unicode string for
/// terminal display.
///
/// The output is intentionally flat: a terminal has no scaled delimiters or
/// stacked fractions, so `\frac{a}{b}` becomes `a/b`, fences collapse to plain
/// brackets, and unknown commands are preserved verbatim.  When nothing can be
/// mapped the source is returned unchanged, so this is safe to call on
/// arbitrary provider-produced math.
#[must_use]
pub fn render_math_pretty(tex: &str) -> String {
    if tex.is_empty() || tex.len() > MAX_MATH_INPUT_LEN {
        return tex.to_string();
    }
    let mut parser = MathPretty::new(tex.chars().collect(), 0);
    let out = parser.render_sequence();
    let trimmed = out.trim();
    if trimmed.is_empty() {
        // The mapping annihilated the input (e.g. only spacing commands);
        // emitting nothing would lose the expression, so fall back to source.
        tex.to_string()
    } else {
        trimmed.to_string()
    }
}

/// Stateful single-pass renderer for one LaTeX math expression.
struct MathPretty {
    chars: Vec<char>,
    pos: usize,
    depth: u32,
}

impl MathPretty {
    fn new(chars: Vec<char>, depth: u32) -> Self {
        Self {
            chars,
            pos: 0,
            depth,
        }
    }

    fn peek(&self) -> Option<char> {
        self.chars.get(self.pos).copied()
    }

    fn bump(&mut self) -> Option<char> {
        let c = self.peek();
        if c.is_some() {
            self.pos += 1;
        }
        c
    }

    /// Render the whole remaining token stream into a single string.
    fn render_sequence(&mut self) -> String {
        if self.depth >= MAX_MATH_DEPTH {
            // Depth cap: emit the rest verbatim rather than recursing deeper.
            let tail = self.chars.get(self.pos..).unwrap_or_default();
            return tail.iter().collect();
        }
        let mut out = String::new();
        while let Some(ch) = self.peek() {
            match ch {
                '\\' => self.parse_command(&mut out),
                '^' => self.parse_script(&mut out, true),
                '_' => self.parse_script(&mut out, false),
                // Prime markers (`f''`) become Unicode prime symbols.
                '\'' => self.parse_primes(&mut out),
                // TeX math collapses inter-token whitespace entirely; the only
                // whitespace that survives is inside `\text{...}`, which is
                // copied verbatim before the main loop resumes.
                c if c.is_whitespace() => {
                    self.bump();
                }
                c => {
                    out.push(c);
                    self.bump();
                }
            }
        }
        out
    }

    /// Skip runs of math-mode whitespace (which carries no meaning).
    fn skip_math_space(&mut self) {
        while self.peek().is_some_and(char::is_whitespace) {
            self.bump();
        }
    }

    /// Render a sub-sequence over a borrowed char span (a group body or an
    /// environment cell), recursing one level deeper so nesting is bounded.
    fn render_inner(&self, inner: &[char]) -> String {
        let mut sub = MathPretty::new(inner.to_vec(), self.depth + 1);
        sub.render_sequence()
    }

    /// Parse one mandatory argument: either a balanced `{...}` group or a
    /// single atom (used for `\frac`, `\sqrt`, `^`/`_` and accents, whose
    /// TeX syntax allows unbraced arguments).  Returns the rendered argument
    /// and whether it came from an explicit braced group.
    fn parse_group_or_single(&mut self) -> (String, bool) {
        self.skip_math_space();
        if self.peek() == Some('{') {
            if let Some(inner) = self.scan_group_content() {
                return (self.render_inner(&inner), true);
            }
            // Unbalanced `{`: keep the brace and render the next atom, then
            // let the main loop continue — lossless, never a panic.
            self.bump();
            let atom = self.render_single_atom();
            return (format!("{{{atom}"), true);
        }
        (self.render_single_atom(), false)
    }

    /// Render exactly one atom: a command (with its own argument handling) or
    /// a single literal character.
    fn render_single_atom(&mut self) -> String {
        match self.peek() {
            Some('\\') => {
                let mut s = String::new();
                self.parse_command(&mut s);
                s
            }
            Some(c) => {
                self.bump();
                c.to_string()
            }
            None => String::new(),
        }
    }

    /// Consume a balanced `{...}` group (if the current char is `{`) and
    /// return its contents without the braces.  On an unbalanced group the
    /// position is rewound to the `{` so callers can emit it literally.
    fn scan_group_content(&mut self) -> Option<Vec<char>> {
        if self.peek() != Some('{') {
            return None;
        }
        self.bump();
        let start = self.pos;
        let mut group_depth = 1usize;
        while let Some(c) = self.peek() {
            match c {
                '{' => {
                    group_depth += 1;
                    self.bump();
                }
                '}' => {
                    group_depth -= 1;
                    // Remember where the `}` sits before consuming it, so the
                    // group slice can end at (not past) the closing brace.
                    let closing_brace = self.pos;
                    self.bump();
                    if group_depth == 0 {
                        // Bounds hold by construction (we just consumed a char
                        // past `start`); `get` keeps the slicing lint quiet and
                        // degrades to an empty group rather than panicking.
                        return Some(
                            self.chars
                                .get(start..closing_brace)
                                .unwrap_or_default()
                                .to_vec(),
                        );
                    }
                }
                _ => {
                    self.bump();
                }
            }
        }
        // No closing brace: restore position to the `{` we consumed.
        self.pos = start - 1;
        None
    }

    /// Parse `^...` (superscript) or `_...` (subscript), mapping the group
    /// onto the Unicode super/subscript alphabet when possible.
    fn parse_script(&mut self, out: &mut String, is_superscript: bool) {
        self.bump(); // consume `^` or `_`
        let (rendered, was_group) = self.parse_group_or_single();
        if rendered.is_empty() {
            return;
        }
        if let Some(mapped) = map_script_run(&rendered, is_superscript) {
            out.push_str(&mapped);
        } else {
            // Unmappable (semantic letters have no Unicode script forms):
            // keep the LaTeX so the formula stays recoverable.
            out.push(if is_superscript { '^' } else { '_' });
            if was_group || rendered.chars().count() > 1 {
                out.push('(');
                out.push_str(&rendered);
                out.push(')');
            } else {
                out.push_str(&rendered);
            }
        }
    }

    /// Map runs of prime markers (`''`) to Unicode prime symbols.
    fn parse_primes(&mut self, out: &mut String) {
        let mut count = 0;
        while self.peek() == Some('\'') {
            self.bump();
            count += 1;
        }
        out.push_str(match count {
            1 => "′",
            2 => "″",
            3 => "‴",
            _ => "′",
        });
    }

    /// Handle a backslash token: a named command, an escaped character, or the
    /// `\ ` / `\\` specials.
    fn parse_command(&mut self, out: &mut String) {
        debug_assert_eq!(self.peek(), Some('\\'));
        self.bump(); // the backslash
        match self.peek() {
            Some(' ') => {
                // `\ ` — an explicit space in math.
                self.bump();
                out.push(' ');
            }
            Some('\\') => {
                // `\\` — an (env) row break; outside environments it carries
                // no displayable meaning.
                self.bump();
            }
            _ => {}
        }
        let name_start = self.pos;
        while let Some(c) = self.peek() {
            if c.is_ascii_alphabetic() {
                self.bump();
            } else {
                break;
            }
        }
        // The letter loop above guarantees `name_start <= self.pos`; `get`
        // still keeps the slicing lint quiet.
        let command_end = self.pos;
        let name: String = self
            .chars
            .get(name_start..command_end)
            .unwrap_or_default()
            .iter()
            .collect();
        if name.is_empty() {
            // Escaped single character: `\{`, `\&`, `\,`, `\alpha` is handled
            // above via the letter loop; here we reach non-letter escapes.
            if let Some(c) = self.bump() {
                out.push(escaped_math_char(c));
            }
            return;
        }
        if self.dispatch_command(&name, out) {
            return;
        }
        // Unknown command: emit it verbatim and do NOT consume a following
        // argument group, so `\foo{x}` renders as source rather than `foo`
        // swallowing the braces.
        out.push('\\');
        out.push_str(&name);
    }

    /// Dispatch a known command to its renderer.  Returns `false` when the
    /// name is unknown so the caller can emit it verbatim.
    fn dispatch_command(&mut self, name: &str, out: &mut String) -> bool {
        match name {
            "begin" | "end" => {
                self.parse_environment(name, out);
                true
            }
            "frac" => {
                let (num, _) = self.parse_group_or_single();
                let (den, _) = self.parse_group_or_single();
                out.push_str(&frac_join(&num, &den));
                true
            }
            "sqrt" => {
                // Optional `\sqrt[n]{...}` index in square brackets.
                let mut index: Vec<char> = Vec::new();
                self.skip_math_space();
                if self.peek() == Some('[') {
                    self.bump();
                    while let Some(c) = self.peek() {
                        if c == ']' {
                            break;
                        }
                        index.push(c);
                        self.bump();
                    }
                    if self.peek() == Some(']') {
                        self.bump();
                    }
                }
                let (radicand, _) = self.parse_group_or_single();
                if radicand.is_empty() {
                    return true;
                }
                if index.as_slice() == ['3'] {
                    out.push('∛');
                } else if !index.is_empty() {
                    if let Some(sup) = map_script_run(&index.iter().collect::<String>(), true) {
                        out.push_str(&sup);
                    }
                    out.push('√');
                } else {
                    out.push('√');
                }
                if needs_frac_parens(&radicand) {
                    out.push('(');
                    out.push_str(&radicand);
                    out.push(')');
                } else {
                    out.push_str(&radicand);
                }
                true
            }
            // Text-style commands: copy their argument verbatim (whitespace
            // preserved), because these carry prose, not math structure.
            "text" | "mathrm" | "textrm" | "operatorname" | "mbox" | "hbox" | "mathbf"
            | "mathit" | "boldsymbol" | "textbf" | "textit" | "mathsf" | "mathtt"
            | "textnormal" | "underline" => {
                self.skip_math_space();
                match self.scan_group_content() {
                    Some(inner) => out.push_str(&inner.iter().collect::<String>()),
                    None => out.push('{'),
                }
                true
            }
            "mathbb" => {
                self.skip_math_space();
                match self.scan_group_content() {
                    Some(inner) => {
                        for c in inner {
                            match blackboard_char(c) {
                                Some(mapped) => out.push(mapped),
                                None => out.push(c),
                            }
                        }
                    }
                    None => out.push('{'),
                }
                true
            }
            "pmod" => {
                out.push_str("(mod");
                self.skip_math_space();
                if let Some(inner) = self.scan_group_content() {
                    out.push(' ');
                    out.push_str(&inner.iter().collect::<String>());
                }
                out.push(')');
                true
            }
            "binom" => {
                let (top, _) = self.parse_group_or_single();
                let (bottom, _) = self.parse_group_or_single();
                out.push('(');
                out.push_str(&top);
                out.push(' ');
                out.push_str(&bottom);
                out.push(')');
                true
            }
            // Combining accents: `\hat{a}` → `â`.  The mark is attached to the
            // first character; multi-character arguments keep the mark on the
            // first glyph, which is the best a flat terminal can do.
            "hat" | "widehat" => {
                out.push_str(&self.accent_combining('\u{0302}'));
                true
            }
            "bar" | "overline" | "overbar" => {
                out.push_str(&self.accent_combining('\u{0304}'));
                true
            }
            "vec" => {
                out.push_str(&self.accent_combining('\u{20D7}'));
                true
            }
            "dot" => {
                out.push_str(&self.accent_combining('\u{0307}'));
                true
            }
            "ddot" => {
                out.push_str(&self.accent_combining('\u{0308}'));
                true
            }
            "acute" => {
                out.push_str(&self.accent_combining('\u{0301}'));
                true
            }
            "grave" => {
                out.push_str(&self.accent_combining('\u{0300}'));
                true
            }
            "tilde" | "widetilde" => {
                out.push_str(&self.accent_combining('\u{0303}'));
                true
            }
            "check" => {
                out.push_str(&self.accent_combining('\u{030C}'));
                true
            }
            "breve" => {
                out.push_str(&self.accent_combining('\u{0306}'));
                true
            }
            "left" | "right" => {
                let fence = self.parse_fence_delim();
                if !fence.is_empty() {
                    out.push_str(&fence);
                }
                true
            }
            _ => math_symbol(name).is_some_and(|sym| {
                out.push_str(sym);
                true
            }),
        }
    }

    /// Attach a combining accent mark to a (single or first) argument char.
    fn accent_combining(&mut self, mark: char) -> String {
        let (arg, _) = self.parse_group_or_single();
        let mut chars = arg.chars();
        let Some(first) = chars.next() else {
            return String::new();
        };
        let mut s = String::new();
        s.push(first);
        s.push(mark);
        for rest in chars {
            s.push(rest);
        }
        s
    }

    /// Consume the delimiter that follows `\left` / `\right` and return its
    /// flattened Unicode equivalent (`.` means "no delimiter").
    fn parse_fence_delim(&mut self) -> String {
        self.skip_math_space();
        match self.peek() {
            Some('.') => {
                self.bump();
                String::new()
            }
            Some('\\') => {
                self.bump();
                let start = self.pos;
                while let Some(c) = self.peek() {
                    if c.is_ascii_alphabetic() {
                        self.bump();
                    } else {
                        break;
                    }
                }
                let name: String = self
                    .chars
                    .get(start..self.pos)
                    .unwrap_or_default()
                    .iter()
                    .collect();
                let mapped = match name.as_str() {
                    "lvert" | "rvert" => "|",
                    "Vert" | "lVert" | "rVert" => "‖",
                    "langle" => "⟨",
                    "rangle" => "⟩",
                    "lfloor" => "⌊",
                    "rfloor" => "⌋",
                    "lceil" => "⌈",
                    "rceil" => "⌉",
                    "uparrow" => "↑",
                    "downarrow" => "↓",
                    "updownarrow" => "↕",
                    _ => "",
                };
                if mapped.is_empty() {
                    // Non-letter escaped delimiter (`\left\{`): fall back to
                    // the escaped-character table.
                    self.bump()
                        .map(|c| escaped_math_char(c).to_string())
                        .unwrap_or_default()
                } else {
                    mapped.to_string()
                }
            }
            Some('{' | '}' | '(' | ')' | '[' | ']' | '|') => {
                // The match guard guarantees a char is present.
                if let Some(c) = self.bump() {
                    c.to_string()
                } else {
                    String::new()
                }
            }
            _ => String::new(),
        }
    }

    /// Handle `\begin{env}...\end{env}`: find the matching end, split the
    /// body into rows/cells (respecting nested environments), and flatten it.
    fn parse_environment(&mut self, name: &str, out: &mut String) {
        if name != "begin" {
            // A stray `\end` is emitted literally; its `{...}` renders as
            // regular characters in the main loop.
            out.push_str("\\end");
            return;
        }
        self.skip_math_space();
        let Some(env) = self.scan_group_content() else {
            out.push_str("\\begin");
            return;
        };
        // `\begin{array}{cc}` carries a column spec after the name — skip it.
        let env_str: String = env.iter().collect();
        self.skip_math_space();
        if matches!(env_str.as_str(), "array" | "tabular") && self.peek() == Some('{') {
            let _ = self.scan_group_content();
        }
        let body_start = self.pos;
        if let Some((end_token_start, after_end)) = self.find_env_end(&env, body_start) {
            // `find_env_end` returns indices within `self.chars`; `.get`
            // with an empty fallback keeps the slicing lint quiet.
            let body: Vec<char> = self
                .chars
                .get(body_start..end_token_start)
                .unwrap_or_default()
                .to_vec();
            self.pos = after_end;
            out.push_str(&self.render_environment_body(&body, &env_str));
        } else {
            // No matching `\end`: emit the `\begin{...}` literally.
            out.push_str("\\begin{");
            out.push_str(&env_str);
            out.push('}');
        }
    }

    /// Scan from `from` to the `\end{...}` that closes the environment opened
    /// at `from-1`, honouring nested `\begin`/`\end` pairs.  Returns the index
    /// of the `\` that starts the matching `\end` (for slicing off the body)
    /// and the index just past its closing `}` (for resuming the main loop).
    fn find_env_end(&self, _env: &[char], from: usize) -> Option<(usize, usize)> {
        let chars = &self.chars;
        let mut i = from;
        let mut depth = 1usize;
        while let Some(&c) = chars.get(i) {
            if c != '\\' {
                i += 1;
                continue;
            }
            let cmd_start = i + 1;
            let mut j = cmd_start;
            while chars.get(j).is_some_and(char::is_ascii_alphabetic) {
                j += 1;
            }
            let cmd: String = chars.get(cmd_start..j).unwrap_or_default().iter().collect();
            if cmd == "begin" || cmd == "end" {
                let mut k = j;
                while chars.get(k).is_some_and(|&c| c != '{') {
                    k += 1;
                }
                if k < chars.len() {
                    let name_start = k + 1;
                    let mut name_end = name_start;
                    while chars.get(name_end).is_some_and(|&c| c != '}') {
                        name_end += 1;
                    }
                    if name_end >= chars.len() {
                        return None; // unbalanced `\end{...`
                    }
                    if cmd == "begin" {
                        depth += 1;
                    } else {
                        depth = depth.saturating_sub(1);
                        if depth == 0 {
                            return Some((i, name_end + 1));
                        }
                    }
                    i = name_end + 1;
                    continue;
                }
            }
            i = j;
        }
        None
    }

    /// Flatten an environment body to a single line: rows on `\\`, cells on
    /// `&`, then per-environment bracket styles.
    fn render_environment_body(&self, body: &[char], env: &str) -> String {
        let rows = split_environment_rows(body);
        let rendered_rows: Vec<String> = rows
            .iter()
            .map(|cells| {
                let rendered: Vec<String> =
                    cells.iter().map(|cell| self.render_inner(cell)).collect();
                // `cases` rows are "value & condition" — joined with a space;
                // matrix-like environments separate columns with commas.
                if matches!(env, "cases" | "dcases") {
                    rendered.join(" ")
                } else {
                    rendered.join(", ")
                }
            })
            .collect();
        let inner = rendered_rows.join("; ");
        match env {
            "cases" | "dcases" => format!("{{ {inner} }}"),
            "pmatrix" => format!("({inner})"),
            "bmatrix" | "Bmatrix" => format!("[{inner}]"),
            "vmatrix" | "Vmatrix" => format!("|{inner}|"),
            // matrix / array / aligned / gathered / split and any future
            // environment flatten to their plain cell content.
            _ => inner,
        }
    }
}

/// Map an escaped single character (from `\{`, `\&`, `\,`, …).
fn escaped_math_char(c: char) -> char {
    match c {
        '{' | '}' | '%' | '$' | '&' | '#' | '_' | '(' | ')' | '[' | ']' => c,
        // Thin/medium/thick/negative spaces and `\ ` collapse to one space.
        ',' | ':' | ';' | '!' => ' ',
        _ => c,
    }
}

/// Whether a rendered fraction component reads as compound (operators, other
/// punctuation) and therefore benefits from explicit parentheses.
fn needs_frac_parens(expr: &str) -> bool {
    expr.chars().any(|c| {
        matches!(
            c,
            '+' | '-'
                | '='
                | '/'
                | '*'
                | '<'
                | '>'
                | '('
                | ')'
                | '['
                | ']'
                | '{'
                | '}'
                | ','
                | ' '
                | ';'
                | '≤'
                | '≥'
                | '≠'
                | '·'
        )
    })
}

/// Render `num/den`, parenthesising either side when it is compound.
fn frac_join(num: &str, den: &str) -> String {
    let mut s = String::new();
    if needs_frac_parens(num) {
        s.push('(');
        s.push_str(num);
        s.push(')');
    } else {
        s.push_str(num);
    }
    s.push('/');
    if needs_frac_parens(den) {
        s.push('(');
        s.push_str(den);
        s.push(')');
    } else {
        s.push_str(den);
    }
    s
}

/// Map a whole rendered argument onto the Unicode super/subscript alphabet.
///
/// Any *letter* that has no script form (e.g. `q`, most capitals) aborts the
/// whole mapping so semantic identifiers keep their literal form; operators,
/// arrows and digits that lack a script form are emitted literally so common
/// phrases like `\lim_{x \to 0}` still read naturally (`limₓ→₀`).
fn map_script_run(expr: &str, is_superscript: bool) -> Option<String> {
    if expr.is_empty() {
        return None;
    }
    let mut mapped = String::new();
    for c in expr.chars() {
        match if is_superscript {
            superscript_char(c)
        } else {
            subscript_char(c)
        } {
            Some(m) => mapped.push(m),
            None if c.is_ascii_alphabetic() => return None,
            None => mapped.push(c),
        }
    }
    Some(mapped)
}

/// Unicode superscript (power) form of a character, when one exists.
fn superscript_char(c: char) -> Option<char> {
    let cp = match c {
        '0' => 0x2070,
        '1' => 0x00B9,
        '2' => 0x00B2,
        '3' => 0x00B3,
        '4'..='9' => 0x2074 + (c as u32 - '4' as u32),
        '+' => 0x207A,
        '-' => 0x207B,
        '=' => 0x207C,
        '(' => 0x207D,
        ')' => 0x207E,
        'n' => 0x207F,
        'i' => 0x2071,
        // Superscript Latin small letters (from Phonetic / Cyrillic-related
        // extensions).  `q` and capitals have no forms and are left to the
        // literal fallback by `map_script_run`.
        'a' => 0x1D43,
        'b' => 0x1D47,
        'c' => 0x1D9C,
        'd' => 0x1D48,
        'e' => 0x1D49,
        'f' => 0x1DA0,
        'g' => 0x1D4D,
        'h' => 0x02B0,
        'j' => 0x02B2,
        'k' => 0x1D4F,
        'l' => 0x02E1,
        'm' => 0x1D50,
        'o' => 0x1D52,
        'p' => 0x1D56,
        'r' => 0x02B3,
        's' => 0x02E2,
        't' => 0x1D57,
        'u' => 0x1D58,
        'v' => 0x1D5B,
        'w' => 0x02B7,
        'x' => 0x02E3,
        'y' => 0x02B8,
        'z' => 0x1DBB,
        _ => return None,
    };
    char::from_u32(cp)
}

/// Unicode subscript form of a character, when one exists.
fn subscript_char(c: char) -> Option<char> {
    let cp = match c {
        '0'..='9' => 0x2080 + (c as u32 - '0' as u32),
        '+' => 0x208A,
        '-' => 0x208B,
        '=' => 0x208C,
        '(' => 0x208D,
        ')' => 0x208E,
        // Latin Subscripts + the Phonetic-Extensions i/r/u/v forms.
        'a' => 0x2090,
        'e' => 0x2091,
        'o' => 0x2092,
        'x' => 0x2093,
        'h' => 0x2095,
        'k' => 0x2096,
        'l' => 0x2097,
        'm' => 0x2098,
        'n' => 0x2099,
        'p' => 0x209A,
        's' => 0x209B,
        't' => 0x209C,
        'i' => 0x1D62,
        'r' => 0x1D63,
        'u' => 0x1D64,
        'v' => 0x1D65,
        _ => return None,
    };
    char::from_u32(cp)
}

/// Double-struck (blackboard bold) form of a character, used by `\mathbb`.
fn blackboard_char(c: char) -> Option<char> {
    // Capitals are not a contiguous run in Unicode (ℂ ℍ ℕ ℙ ℚ ℝ ℤ live in the
    // Letterlike Symbols block), so they need an explicit table.
    const BLACKBOARD_CAPITALS: [u32; 26] = [
        0x1D538, 0x1D539, 0x2102, 0x1D53B, 0x1D53C, 0x1D53D, 0x1D53E, 0x210D, 0x1D540, 0x1D541,
        0x1D542, 0x1D543, 0x1D544, 0x2115, 0x1D546, 0x2119, 0x211A, 0x211D, 0x1D54A, 0x1D54B,
        0x1D54C, 0x1D54D, 0x1D54E, 0x1D54F, 0x1D550, 0x2124,
    ];
    match c {
        'A'..='Z' => BLACKBOARD_CAPITALS
            .get((c as u32 - 'A' as u32) as usize)
            // The `'A'..='Z'` guard pins the table index to 0..26.
            .and_then(|&cp| char::from_u32(cp)),
        // Lowercase and digits are contiguous runs.
        'a'..='z' => char::from_u32(0x1D552 + (c as u32 - 'a' as u32)),
        '0'..='9' => char::from_u32(0x1D7D8 + (c as u32 - '0' as u32)),
        _ => None,
    }
}

/// Split an environment body into `rows[cell][]`, honouring nested
/// environments so `&` / `\\` inside a matrix-in-a-case are not treated as
/// separators of the outer environment.
fn split_environment_rows(body: &[char]) -> Vec<Vec<Vec<char>>> {
    let mut rows: Vec<Vec<Vec<char>>> = Vec::new();
    let mut row: Vec<Vec<char>> = Vec::new();
    let mut cell: Vec<char> = Vec::new();
    let mut depth = 0usize;
    let mut i = 0usize;
    while let Some(&c) = body.get(i) {
        if c == '\\' {
            if body.get(i + 1) == Some(&'\\') {
                if depth == 0 {
                    row.push(std::mem::take(&mut cell));
                    rows.push(std::mem::take(&mut row));
                } else {
                    cell.push('\\');
                    cell.push('\\');
                }
                i += 2;
                continue;
            }
            // A nested `\begin` / `\end` — track depth but keep the command
            // text in the cell so the nested structure survives re-parsing.
            let mut j = i + 1;
            while body.get(j).is_some_and(char::is_ascii_alphabetic) {
                j += 1;
            }
            let cmd: String = body.get(i + 1..j).unwrap_or_default().iter().collect();
            if cmd == "begin" || cmd == "end" {
                let mut end_of_cmd = j;
                let mut k = j;
                while body.get(k).is_some_and(|&c| c != '{') {
                    k += 1;
                }
                let mut name_end = k;
                if k < body.len() {
                    while body.get(name_end).is_some_and(|&c| c != '}') {
                        name_end += 1;
                    }
                    if name_end < body.len() {
                        end_of_cmd = name_end + 1;
                    }
                }
                if cmd == "begin" {
                    depth += 1;
                } else {
                    depth = depth.saturating_sub(1);
                }
                if let Some(span) = body.get(i..end_of_cmd) {
                    for ch in span {
                        cell.push(*ch);
                    }
                }
                i = end_of_cmd;
                continue;
            }
            cell.push(c);
            i += 1;
            continue;
        }
        if c == '&' && depth == 0 {
            row.push(std::mem::take(&mut cell));
            i += 1;
            continue;
        }
        cell.push(c);
        i += 1;
    }
    if !cell.is_empty() || !row.is_empty() {
        row.push(std::mem::take(&mut cell));
        rows.push(row);
    }
    rows
}

/// Look up a single-token math command (Greek letters, operator/relation
/// symbols, named functions) — every name that needs no argument handling.
#[expect(clippy::too_many_lines)]
fn math_symbol(name: &str) -> Option<&'static str> {
    Some(match name {
        // Greek letters.
        "alpha" => "α",
        "beta" => "β",
        "gamma" => "γ",
        "delta" => "δ",
        "epsilon" => "ε",
        "varepsilon" => "ϵ",
        "zeta" => "ζ",
        "eta" => "η",
        "theta" => "θ",
        "vartheta" => "ϑ",
        "iota" => "ι",
        "kappa" => "κ",
        "lambda" => "λ",
        "mu" => "μ",
        "nu" => "ν",
        "xi" => "ξ",
        "omicron" => "ο",
        "pi" => "π",
        "varpi" => "ϖ",
        "rho" => "ρ",
        "varrho" => "ϱ",
        "sigma" => "σ",
        "varsigma" => "ς",
        "tau" => "τ",
        "upsilon" => "υ",
        "phi" => "φ",
        "varphi" => "ϕ",
        "chi" => "χ",
        "psi" => "ψ",
        "omega" => "ω",
        "Gamma" => "Γ",
        "Delta" => "Δ",
        "Theta" => "Θ",
        "Lambda" => "Λ",
        "Xi" => "Ξ",
        "Pi" => "Π",
        "Sigma" => "Σ",
        "Upsilon" => "Υ",
        "Phi" => "Φ",
        "Psi" => "Ψ",
        "Omega" => "Ω",
        // Large operators.
        "sum" => "∑",
        "prod" => "∏",
        "coprod" => "∐",
        "int" => "∫",
        "iint" => "∬",
        "iiint" => "∭",
        "oint" => "∮",
        "bigcap" => "⋂",
        "bigcup" => "⋃",
        "bigvee" => "⋁",
        "bigwedge" => "⋀",
        "bigoplus" => "⊕",
        "bigotimes" => "⊗",
        "bigodot" => "⊙",
        // Binary operators.
        "pm" => "±",
        "mp" => "∓",
        "times" => "×",
        "div" => "÷",
        "cdot" => "·",
        "ast" => "∗",
        "star" => "⋆",
        "circ" => "∘",
        "bullet" => "•",
        "oplus" => "⊕",
        "ominus" => "⊖",
        "otimes" => "⊗",
        "oslash" => "⊘",
        "odot" => "⊙",
        "dagger" => "†",
        "ddagger" => "‡",
        "wedge" => "∧",
        "vee" => "∨",
        "land" => "∧",
        "lor" => "∨",
        "cap" => "∩",
        "cup" => "∪",
        "sqcap" => "⊓",
        "sqcup" => "⊔",
        "uplus" => "⊎",
        "sqsubset" => "⊏",
        "sqsupset" => "⊐",
        "setminus" => "∖",
        // Relations.
        "le" => "≤",
        "leq" => "≤",
        "ge" => "≥",
        "geq" => "≥",
        "ne" => "≠",
        "neq" => "≠",
        "equiv" => "≡",
        "approx" => "≈",
        "approxeq" => "≊",
        "cong" => "≅",
        "sim" => "∼",
        "simeq" => "≃",
        "propto" => "∝",
        "asymp" => "≍",
        "ll" => "≪",
        "gg" => "≫",
        "prec" => "≺",
        "succ" => "≻",
        "preceq" => "⪯",
        "succeq" => "⪰",
        "subset" => "⊂",
        "supset" => "⊃",
        "subseteq" => "⊆",
        "supseteq" => "⊇",
        "in" => "∈",
        "notin" => "∉",
        "ni" => "∋",
        "owns" => "∋",
        "mid" => "∣",
        "nmid" => "∤",
        "parallel" => "∥",
        "perp" => "⊥",
        "top" => "⊤",
        "bot" => "⊥",
        "vdash" => "⊢",
        "dashv" => "⊣",
        "models" => "⊨",
        "vDash" => "⊨",
        "doteq" => "≐",
        "triangleq" => "≜",
        "therefore" => "∴",
        "because" => "∵",
        "neg" => "¬",
        "lnot" => "¬",
        // Arrows.
        "to" => "→",
        "gets" => "←",
        "leftarrow" => "←",
        "rightarrow" => "→",
        "Leftarrow" => "⇐",
        "Rightarrow" => "⇒",
        "leftrightarrow" => "↔",
        "Leftrightarrow" => "⇔",
        "iff" => "⟺",
        "mapsto" => "↦",
        "longmapsto" => "⟼",
        "longrightarrow" => "⟶",
        "longleftarrow" => "⟵",
        "Longrightarrow" => "⟹",
        "implies" => "⟹",
        "impliedby" => "⟸",
        "uparrow" => "↑",
        "downarrow" => "↓",
        "updownarrow" => "↕",
        "nearrow" => "↗",
        "searrow" => "↘",
        "swarrow" => "↙",
        "nwarrow" => "↖",
        "rightleftharpoons" => "⇌",
        "leftrightharpoons" => "⇋",
        // Fences & punctuation.
        "Vert" => "‖",
        "lVert" => "‖",
        "rVert" => "‖",
        "langle" => "⟨",
        "rangle" => "⟩",
        "lfloor" => "⌊",
        "rfloor" => "⌋",
        "lceil" => "⌈",
        "rceil" => "⌉",
        "ulcorner" => "⌜",
        "urcorner" => "⌝",
        "llcorner" => "⌞",
        "lrcorner" => "⌟",
        "lvert" => "|",
        "rvert" => "|",
        "prime" => "′",
        "colon" => ":",
        // Decorations & misc symbols.
        "infty" => "∞",
        "nabla" => "∇",
        "partial" => "∂",
        "hbar" => "ℏ",
        "hslash" => "ℏ",
        "ell" => "ℓ",
        "wp" => "℘",
        "imath" => "ı",
        "jmath" => "ȷ",
        "aleph" => "ℵ",
        "Re" => "ℜ",
        "Im" => "ℑ",
        "emptyset" => "∅",
        "varnothing" => "∅",
        "angle" => "∠",
        "measuredangle" => "∡",
        "sphericalangle" => "∢",
        "degree" => "°",
        "surd" => "√",
        "flat" => "♭",
        "natural" => "♮",
        "sharp" => "♯",
        "dots" => "…",
        "ldots" => "…",
        "cdots" => "⋯",
        "vdots" => "⋮",
        "ddots" => "⋱",
        "checkmark" => "✓",
        "square" => "□",
        "blacksquare" => "■",
        "triangle" => "△",
        "copyright" => "©",
        "pounds" => "£",
        "S" => "§",
        "P" => "¶",
        // Named functions: kept as plain words — a terminal cannot fake the
        // upright-vs-italic distinction, so styling would only add noise.
        "sin" => "sin",
        "cos" => "cos",
        "tan" => "tan",
        "cot" => "cot",
        "sec" => "sec",
        "csc" => "csc",
        "arcsin" => "arcsin",
        "arccos" => "arccos",
        "arctan" => "arctan",
        "sinh" => "sinh",
        "cosh" => "cosh",
        "tanh" => "tanh",
        "coth" => "coth",
        "log" => "log",
        "ln" => "ln",
        "exp" => "exp",
        "lim" => "lim",
        "max" => "max",
        "min" => "min",
        "inf" => "inf",
        "sup" => "sup",
        "arg" => "arg",
        "deg" => "deg",
        "det" => "det",
        "dim" => "dim",
        "gcd" => "gcd",
        "hom" => "hom",
        "ker" => "ker",
        "Pr" => "Pr",
        "mod" => "mod",
        "bmod" => "mod",
        // `{a \over b}` — the binary fraction operator.
        "over" => " / ",
        _ => return None,
    })
}
