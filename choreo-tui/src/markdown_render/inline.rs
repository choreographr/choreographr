//! Inline markdown rendering: text, emphasis, links, code, and math into
//! styled spans, with wrapping.

use super::{
    Color, Line, LineChrome, LineJoin, MarkdownInline, Modifier, Span, Style, display_width,
    render_math_pretty, split_word_to_width, wrap_plain_line,
};
pub(crate) fn inlines_to_lines(
    inlines: &[MarkdownInline],
    indent: usize,
    prefix: Option<&str>,
    width: usize,
    modifier: Modifier,
) -> (Vec<Line<'static>>, Vec<LineJoin>, Vec<LineChrome>) {
    let mut lines = Vec::new();
    let mut joins = Vec::new();
    let mut chrome = Vec::new();
    let mut current_spans: Vec<Span<'static>> = Vec::new();
    let mut current_width: usize = 0;
    if indent > 0 {
        current_spans.push(Span::styled(" ".repeat(indent), Style::default()));
        current_width += indent;
    }
    if let Some(prefix) = prefix {
        current_spans.push(Span::styled(prefix.to_string(), Style::default()));
        current_width += display_width(prefix);
    }
    let mut needs_separator = false;
    let final_join = {
        let mut ctx = RenderCtx {
            lines: &mut lines,
            joins: &mut joins,
            chrome: &mut chrome,
            current: &mut current_spans,
            current_width: &mut current_width,
            needs_separator: &mut needs_separator,
            indent,
            width,
            modifier,
            // The very first line of a paragraph/heading is a fresh line.
            current_join: LineJoin::Break,
        };
        render_inlines_to_lines(inlines, &mut ctx);
        // Snapshot the final line's join before `ctx` drops its borrows of
        // the local vectors below.
        ctx.current_join
    };
    if !current_spans.is_empty() || lines.is_empty() {
        lines.push(Line::from(std::mem::take(&mut current_spans)));
        joins.push(final_join);
        chrome.push(LineChrome::default());
    }
    (lines, joins, chrome)
}

/// Bundles all mutable state and parameters needed to render a flat list of
/// [`MarkdownInline`] nodes into Ratatui [`Line`]s, automatically wrapping
/// and applying text styling (bold, italic, colours, etc.).
///
/// The `modifier` field accumulates [`Modifier`] flags as we descend into
/// nested containers (emphasis inside bold, etc.).  Each container saves the
/// current modifier, ORs in its own flag, calls back into the renderer, and
/// then restores the original value — giving us correct modifier stacking
/// with no heap allocation.
pub(crate) struct RenderCtx<'a> {
    /// Output buffer — completed lines are pushed here.
    lines: &'a mut Vec<Line<'static>>,
    /// Per-line [`LineJoin`] copy metadata, pushed in lockstep with `lines`.
    joins: &'a mut Vec<LineJoin>,
    /// Per-line [`LineChrome`] copy metadata, pushed in lockstep with `lines`
    /// (inline content emits no chrome, so every row pushes an empty value).
    chrome: &'a mut Vec<LineChrome>,
    /// Spans being accumulated for the line currently being built.
    current: &'a mut Vec<Span<'static>>,
    /// Display width of `current` (updated alongside every push).
    current_width: &'a mut usize,
    /// The [`LineJoin`] of the line currently being built (how it joins the
    /// previously flushed line).  Set when the line is started by
    /// [`RenderCtx::flush_line_with_next`] and recorded when the line is
    /// finally pushed.
    current_join: LineJoin,
    /// Whether the next word needs a space separator before it (set to
    /// `true` after every word/code chunk, reset to `false` on line break
    /// or flush).
    needs_separator: &'a mut bool,
    /// Left-margin width for blockquote / list nesting.
    indent: usize,
    /// Maximum line width (in columns) before wrapping kicks in.
    width: usize,
    /// Active text modifiers inherited from enclosing containers (e.g.
    /// `BOLD` inside `Strong`, `ITALIC` inside `Emphasis`).  Combined
    /// via bitwise-OR when nesting.
    modifier: Modifier,
}

impl RenderCtx<'_> {
    fn base_style(&self) -> Style {
        Style::default().add_modifier(self.modifier)
    }

    fn push_span(&mut self, content: String, style: Style) {
        self.current.push(Span::styled(content, style));
    }

    /// Push the current line into the output and start a fresh line at indent.
    /// Indent padding uses `Style::default()` (not `base_style()`) because
    /// styling modifiers on whitespace are invisible and could confuse tests
    /// or terminal renderers.
    ///
    /// The just-flushed line is recorded with its own `current_join`; the
    /// caller passes the join the fresh line has toward it: word-wrap →
    /// [`LineJoin::Space`], a hard source line break → [`LineJoin::Break`],
    /// a mid-word split → [`LineJoin::Join`].
    fn flush_line_with_next(&mut self, next_join: LineJoin) {
        self.lines.push(Line::from(std::mem::take(self.current)));
        self.joins.push(self.current_join);
        self.chrome.push(LineChrome::default());
        *self.current_width = self.indent;
        if self.indent > 0 {
            self.current
                .push(Span::styled(" ".repeat(self.indent), Style::default()));
        }
        self.current_join = next_join;
    }

    /// Split `word` into grapheme-cluster chunks to fit the available width on the
    /// *current* line. The caller is responsible for flushing before calling this
    /// when the current line has content that won't leave enough room.
    fn render_word_split(&mut self, word: &str, style: Style) {
        *self.needs_separator = false;
        let available = self.width.saturating_sub(*self.current_width);
        let chunked = split_word_to_width(word, available);
        for (ci, chunk) in chunked.iter().enumerate() {
            if ci > 0 {
                // The next row is a mid-word continuation of the split.
                self.flush_line_with_next(LineJoin::Join);
            }
            self.push_span(chunk.clone(), style);
            *self.current_width += display_width(chunk);
        }
        *self.needs_separator = true;
    }
}

pub(crate) fn render_inlines_to_lines(inlines: &[MarkdownInline], ctx: &mut RenderCtx) {
    for inline in inlines {
        match inline {
            MarkdownInline::Text(text) => {
                render_text_inline(text, ctx);
            }
            MarkdownInline::Code(text) => {
                render_code_inline(text, ctx, Color::Cyan);
            }
            MarkdownInline::InlineMath(text) => {
                // Inline math (`$...$`) is pretty-printed to Unicode and then
                // rendered like a normal word (unbreakable, wraps as a unit).
                let pretty = render_math_pretty(text);
                let body: &str = if pretty.is_empty() { text } else { &pretty };
                render_code_inline(body, ctx, Color::Yellow);
            }
            MarkdownInline::DisplayMath(text) => {
                render_display_math(text, ctx);
            }
            MarkdownInline::Strikethrough(content) => {
                render_style_inline(content, ctx, Modifier::CROSSED_OUT);
            }
            MarkdownInline::Emphasis(content) => {
                render_style_inline(content, ctx, Modifier::ITALIC);
            }
            MarkdownInline::Strong(content) => {
                render_style_inline(content, ctx, Modifier::BOLD);
            }
            MarkdownInline::Link {
                content,
                destination,
            } => {
                render_link_inline(content, destination, ctx);
            }
            MarkdownInline::Image { alt, destination } => {
                let prefix_text = "[image: ";
                let prefix_width = display_width(prefix_text);
                let projected = *ctx.current_width + prefix_width;
                if projected > ctx.width && *ctx.current_width > ctx.indent {
                    // Mid-inline wrap: the image continues the sentence.
                    ctx.flush_line_with_next(LineJoin::Space);
                }
                ctx.push_span(prefix_text.to_string(), Style::default());
                *ctx.current_width += prefix_width;

                render_inlines_to_lines(alt, ctx);

                let suffix = if destination.is_empty() {
                    "]".to_string()
                } else {
                    format!("] ({destination})")
                };
                let suffix_width = display_width(&suffix);
                let projected = *ctx.current_width + suffix_width;
                if projected > ctx.width && *ctx.current_width > ctx.indent {
                    ctx.flush_line_with_next(LineJoin::Space);
                }
                ctx.push_span(suffix, Style::default());
                *ctx.current_width += suffix_width;
                *ctx.needs_separator = true;
            }
            MarkdownInline::LineBreak => {
                // A hard source line break: the next line is a fresh line.
                ctx.flush_line_with_next(LineJoin::Break);
                *ctx.needs_separator = false;
            }
        }
    }
}

/// Returns `true` if `text` starts with closing punctuation that should
/// directly follow the preceding word without a space (e.g. "." in "**bold**!").
/// Opening quotes, brackets, and alphanumeric/text keep the space.
pub(crate) fn starts_with_closing_punct(text: &str) -> bool {
    text.chars().next().is_some_and(|c| {
        matches!(
            c,
            '.' | ',' | '!' | '?' | ':' | ';' | ')' | ']' | '}' | '\u{2019}' | '\u{201d}'
        )
    })
}

/// Returns `true` if `text` ends with opening punctuation that the following
/// inline content should directly attach to without a space (e.g. "(" in
/// "(**hi**)" renders as "(hi)", not "( hi)"). This is the mirror image of
/// `starts_with_closing_punct`: that helper keeps trailing punctuation glued
/// to the *preceding* word, this one keeps leading punctuation glued to the
/// *following* inline (bold, emphasis, code, links, …). Only checked when the
/// text itself does not end with whitespace, so explicit source spaces like
/// "( **hi** )" are still preserved.
pub(crate) fn ends_with_opening_punct(text: &str) -> bool {
    text.chars()
        .next_back()
        .is_some_and(|c| matches!(c, '(' | '[' | '{' | '\u{2018}' | '\u{201c}'))
}

pub(crate) fn render_text_inline(text: &str, ctx: &mut RenderCtx) {
    // If the original text does NOT start with whitespace (e.g. "**bold**!"
    // where "!" directly follows the bold), check whether it starts with
    // trailing/closing punctuation that should attach to the preceding word.
    // Opening quotes and brackets should still get a space before them.
    let has_leading_space = text.starts_with(' ') || text.starts_with('\t');
    if !has_leading_space && starts_with_closing_punct(text) {
        *ctx.needs_separator = false;
    }

    let trimmed = text.trim_start();
    let ends_with_space = text.ends_with(' ') || text.ends_with('\t');
    let words: Vec<&str> = if trimmed.is_empty() {
        *ctx.needs_separator = true;
        return;
    } else {
        trimmed.split_whitespace().collect()
    };

    for (i, word) in words.iter().enumerate() {
        let word_width = display_width(word);
        let separator_width = usize::from(*ctx.needs_separator || i > 0);
        let projected = *ctx.current_width + separator_width + word_width;

        if projected > ctx.width && *ctx.current_width > ctx.indent {
            // Word-wrap: the next row continues the sentence.
            ctx.flush_line_with_next(LineJoin::Space);
            *ctx.needs_separator = i > 0;
        }

        if *ctx.current_width + word_width > ctx.width && *ctx.current_width >= ctx.indent {
            ctx.render_word_split(word, ctx.base_style());
            continue;
        }

        if (*ctx.needs_separator || i > 0)
            && !ctx.current.is_empty()
            && *ctx.current_width > ctx.indent
        {
            ctx.push_span(" ".to_string(), ctx.base_style());
            *ctx.current_width += 1;
        }
        ctx.push_span(word.to_string(), ctx.base_style());
        *ctx.current_width += word_width;
        *ctx.needs_separator = true;
    }

    if ends_with_space && !words.is_empty() {
        *ctx.needs_separator = true;
    } else if ends_with_opening_punct(text) {
        // Text ends with an opening bracket/quote and no whitespace, so the
        // next inline (e.g. "**hi**" inside "(**hi**)") must attach directly
        // without a space. Without this the renderer treats "(" as a word and
        // inserts a spurious space after it.
        *ctx.needs_separator = false;
    }
}

pub(crate) fn render_code_inline(text: &str, ctx: &mut RenderCtx, color: Color) {
    let word_width = display_width(text);

    // Flush if projected width exceeds the available line width
    // (but only when the line already has content — don't flush a blank line).
    let projected = *ctx.current_width + usize::from(*ctx.needs_separator) + word_width;
    if projected > ctx.width && *ctx.current_width > ctx.indent {
        ctx.flush_line_with_next(LineJoin::Space);
    } else if *ctx.needs_separator && !ctx.current.is_empty() && *ctx.current_width > ctx.indent {
        ctx.push_span(" ".to_string(), ctx.base_style());
        *ctx.current_width += 1;
    }

    if *ctx.current_width + word_width > ctx.width && *ctx.current_width >= ctx.indent {
        ctx.render_word_split(text, ctx.base_style().fg(color));
        return;
    }

    ctx.push_span(text.to_string(), ctx.base_style().fg(color));
    *ctx.current_width += word_width;
    *ctx.needs_separator = true;
}

/// Render a display-math expression (`$$...$$`) as a block on its own line:
/// the pretty-printed equation is centred when it fits, wrapped left-aligned
/// when too wide for the content width.
///
/// Display math always starts and ends its own line — any text before it is
/// flushed with a hard break first, and the in-progress line is reset to a
/// fresh continuation line so following text starts a new row.
pub(crate) fn render_display_math(text: &str, ctx: &mut RenderCtx) {
    let pretty = render_math_pretty(text);
    let body: &str = if pretty.is_empty() { text } else { &pretty };

    // Close out any text already on the current line so the equation starts
    // on a fresh one.
    if *ctx.current_width > ctx.indent {
        ctx.flush_line_with_next(LineJoin::Break);
    }
    *ctx.needs_separator = false;

    let available = ctx.width.saturating_sub(ctx.indent);
    let style = ctx.base_style().fg(Color::Magenta);

    if !body.is_empty() && available > 0 && display_width(body) <= available {
        // A centred equation line.
        let pad = (available - display_width(body)) / 2;
        let mut spans = Vec::with_capacity(3);
        if ctx.indent > 0 {
            spans.push(Span::styled(" ".repeat(ctx.indent), Style::default()));
        }
        if pad > 0 {
            spans.push(Span::styled(" ".repeat(pad), Style::default()));
        }
        spans.push(Span::styled(body.to_string(), style));
        ctx.lines.push(Line::from(spans));
        ctx.joins.push(LineJoin::Break);
        ctx.chrome.push(LineChrome::default());
    } else if !body.is_empty() {
        // Too wide for one line: wrap left-aligned at the content width.  The
        // equation is still one visual block, so every continuation row is a
        // fresh line.
        for chunk in wrap_plain_line(body, available.max(1)) {
            let mut spans = Vec::with_capacity(2);
            if ctx.indent > 0 {
                spans.push(Span::styled(" ".repeat(ctx.indent), Style::default()));
            }
            spans.push(Span::styled(chunk, style));
            ctx.lines.push(Line::from(spans));
            ctx.joins.push(LineJoin::Break);
            ctx.chrome.push(LineChrome::default());
        }
    }

    // Reset the in-progress line to a fresh, empty continuation line (the
    // same invariant `flush_line_with_next` maintains) so any text that
    // follows the equation starts on its own row.
    ctx.current.clear();
    *ctx.current_width = ctx.indent;
    if ctx.indent > 0 {
        ctx.current
            .push(Span::styled(" ".repeat(ctx.indent), Style::default()));
    }
    ctx.current_join = LineJoin::Break;
}

/// Render a styled container (bold, italic, strikethrough) by stacking its
/// [`Modifier`] on top of any modifiers already active from enclosing
/// containers.  The save-OR-restore pattern gives us correct nesting with
/// no heap allocation — e.g. ***bold italic*** becomes
/// `BOLD | ITALIC` for the inner text.
pub(crate) fn render_style_inline(
    content: &[MarkdownInline],
    ctx: &mut RenderCtx,
    modifier: Modifier,
) {
    let prev = ctx.modifier;
    ctx.modifier = prev | modifier;
    render_inlines_to_lines(content, ctx);
    ctx.modifier = prev;
    *ctx.needs_separator = true;
}

/// Render a hyperlink: link text in **bold**, then ` — `, then the URL
/// <u>underlined</u>.  If `destination` is empty the content is rendered
/// without any link-specific styling (bare `[...]()` with no URL).
///
/// Modifier stacking works the same as [`render_style_inline`]: the BOLD
/// flag is `ORed` in for the content, then removed for the separator, then
/// UNDERLINED is `ORed` in for the URL alone, then fully restored.
pub(crate) fn render_link_inline(
    content: &[MarkdownInline],
    destination: &str,
    ctx: &mut RenderCtx,
) {
    if destination.is_empty() {
        render_inlines_to_lines(content, ctx);
        *ctx.needs_separator = true;
        return;
    }

    // Link content in bold (stacked on any parent modifier).
    let prev = ctx.modifier;
    ctx.modifier = prev | Modifier::BOLD;
    render_inlines_to_lines(content, ctx);

    // Separator: " - " with the parent style (no bold).
    ctx.modifier = prev;
    let sep = " - ";
    let sep_width = display_width(sep);
    let url_width = display_width(destination);
    let projected = *ctx.current_width + sep_width + url_width;
    if projected > ctx.width && *ctx.current_width > ctx.indent {
        // Mid-inline wrap: the link separator/URL continue the sentence.
        ctx.flush_line_with_next(LineJoin::Space);
    }
    ctx.push_span(sep.to_string(), ctx.base_style());
    *ctx.current_width += sep_width;

    // URL underlined (stacked on parent but not bold).
    ctx.modifier = prev | Modifier::UNDERLINED;
    ctx.push_span(destination.to_string(), ctx.base_style());
    ctx.modifier = prev;
    *ctx.current_width += url_width;

    *ctx.needs_separator = true;
}
