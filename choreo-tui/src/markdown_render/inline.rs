//! Inline markdown rendering: text, emphasis, links, code, and math into
//! styled spans, with wrapping.

use std::borrow::Cow;

use super::{
    Color, Line, LineChrome, LineJoin, MarkdownInline, Modifier, Span, Style, display_width,
    render_math_pretty, split_word_to_width, wrap_plain_line,
};
pub(crate) fn inlines_to_lines(
    inlines: &[MarkdownInline],
    indent: usize,
    prefix: Vec<Span<'static>>,
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
    // The prefix spans (a heading's decoration, or a tool row's collapse
    // triangle) open the *first* row only: they are charged against this row's
    // width, so the row wraps narrower by the prefix's own width while every
    // row after it — flushed by `flush_line_with_next`, which re-applies only
    // `indent` — gets the full `width`.  A pre-styled run (the triangle, then a
    // heading's own decoration) is passed as separate spans so each keeps its
    // own style.
    for span in prefix {
        current_width += span.width();
        current_spans.push(span);
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
    /// Whether the *source* had whitespace immediately before whatever comes
    /// next, i.e. whether the next inline node is preceded by a space.  Every
    /// node leaves this describing its own right edge: a text node latches the
    /// whitespace it ended with, while a code span, link, image marker, display
    /// equation, or hard break sets it to `false` because none of them carries
    /// trailing whitespace of its own.  A following text node ORs it with its
    /// own leading whitespace — the only other place a gap can come from — and
    /// [`RenderCtx::begin_inline`] is the one place that consumes it.
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

    fn push_span(&mut self, content: impl Into<Cow<'static, str>>, style: Style) {
        self.current.push(Span::styled(content, style));
    }

    /// Whether the current line already ends in whitespace, so a gap before the
    /// next node would duplicate it.  A line with nothing beyond its indent
    /// counts as ending in whitespace: that is what stops a node at the start of
    /// a row from being pushed a space it cannot use.
    fn ends_with_space(&self) -> bool {
        if *self.current_width <= self.indent {
            return true;
        }
        self.current
            .last()
            .is_some_and(|span| span.content.ends_with(char::is_whitespace))
    }

    /// Begin an inline node whose rendered form is `item_width` columns wide:
    /// wrap to a fresh row when it would overflow the current one, otherwise
    /// emit the single-space gap the source asked for in `gap`.
    ///
    /// The gap is emitted only when the line does not already sit on one, so a
    /// marker that ends in its own space (the `[image: ` prefix) or a row that
    /// was just flushed never gets a second one.  The caller owns what follows —
    /// a mid-word split check, or the node's own spans — and is responsible for
    /// setting [`Self::needs_separator`] to describe the node's right edge.
    fn begin_inline(&mut self, item_width: usize, gap: bool) {
        if *self.current_width + usize::from(gap) + item_width > self.width
            && *self.current_width > self.indent
        {
            self.flush_line_with_next(LineJoin::Space);
            return;
        }
        if gap && !self.ends_with_space() {
            self.push_span(" ", self.base_style());
            *self.current_width += 1;
        }
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
    ///
    /// The caller also owns the pending gap: this pushes text and nothing else,
    /// so whatever the next node's separator state is stays the caller's to set.
    fn render_word_split(&mut self, word: &str, style: Style) {
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
                // The source's gap before the image sits on a word boundary like
                // any other inline, so `begin_inline` emits it (or wraps first
                // when it will not fit).
                ctx.begin_inline(prefix_width, *ctx.needs_separator);
                ctx.push_span(prefix_text, Style::default());
                *ctx.current_width += prefix_width;
                // The prefix ends with its own space, so the alt starts straight
                // after it: latching the gap would charge the first alt word a
                // second column it does not get, and — the reason `begin_inline`
                // also checks `ends_with_space` — an alt whose own text begins
                // with whitespace must not double the prefix's space either.
                *ctx.needs_separator = false;

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
                // The rendered image ends with its `]`/`)` marker, so any gap
                // before the next node comes from that node's own whitespace.
                *ctx.needs_separator = false;
            }
            MarkdownInline::LineBreak => {
                // A hard source line break: the next line is a fresh line.
                ctx.flush_line_with_next(LineJoin::Break);
                *ctx.needs_separator = false;
            }
        }
    }
}

/// Render a run of source text as words, honouring the whitespace the source
/// actually had around it.
///
/// A gap between two inline nodes is carried by the source text itself: it is
/// either this node's leading whitespace or the previous node's trailing
/// whitespace, which [`RenderCtx::needs_separator`] latches. Either spelling
/// means the rendered pair needs a space, and *only* then — that is what keeps
/// `(+` + `` `num-traits` `` glued as `(+num-traits`, `**bold**!` as `bold!`,
/// `a` + `**b**` + `c` as `abc`, and `( **hi** )` as `( hi )`. Punctuation is
/// not consulted: whether a bracket needs a space is a question about the
/// source's whitespace, and no character-class heuristic can answer it.
///
/// The whitespace tests use [`char::is_whitespace`] — the same class
/// [`str::split_whitespace`] collapses *within* a node — so a non-breaking or
/// ideographic space on a node boundary keeps the pair apart instead of being
/// silently dropped.
pub(crate) fn render_text_inline(text: &str, ctx: &mut RenderCtx) {
    // The gap before this node comes from one of two spellings of the same
    // source whitespace: the previous node's trailing whitespace (latched in
    // `needs_separator`) or this node's own leading whitespace.
    let gap_before = *ctx.needs_separator || text.starts_with(char::is_whitespace);

    // `split_whitespace` both trims and iterates lazily: a text node needs no
    // buffer of its own, and the empty check is a peek rather than a collect.
    let mut words = text.split_whitespace().peekable();
    if words.peek().is_none() {
        // Whitespace-only source: it *is* the gap before the next node, the
        // separator between two adjacent styled spans for instance.
        *ctx.needs_separator = true;
        return;
    }

    for (i, word) in words.enumerate() {
        let word_width = display_width(word);
        // Within one text node every word after the first is separated by the
        // whitespace `split_whitespace` just collapsed; the first word is
        // separated only if the source had a gap before this node at all.
        let separator = i > 0 || gap_before;

        ctx.begin_inline(word_width, separator);

        if *ctx.current_width + word_width > ctx.width && *ctx.current_width >= ctx.indent {
            // Wider than a whole row: split it mid-word across rows.
            ctx.render_word_split(word, ctx.base_style());
            continue;
        }

        ctx.push_span(word.to_string(), ctx.base_style());
        *ctx.current_width += word_width;
    }

    // The gap *after* this node is the whitespace the source had here, nothing
    // more: a text ending in a word leaves the next node glued to it.
    *ctx.needs_separator = text.ends_with(char::is_whitespace);
}

pub(crate) fn render_code_inline(text: &str, ctx: &mut RenderCtx, color: Color) {
    let word_width = display_width(text);

    // A gap, or a wrap when the span will not fit beside what is already here
    // (never flush a blank line — `begin_inline` leaves an empty row alone).
    ctx.begin_inline(word_width, *ctx.needs_separator);

    if *ctx.current_width + word_width > ctx.width && *ctx.current_width >= ctx.indent {
        // Wider than a whole row: split it mid-word across rows, exactly as a
        // word is split.
        ctx.render_word_split(text, ctx.base_style().fg(color));
    } else {
        ctx.push_span(text.to_string(), ctx.base_style().fg(color));
        *ctx.current_width += word_width;
    }

    // A code span has no whitespace of its own on either side: any gap before
    // the next node lives in that node's leading whitespace, never here.
    *ctx.needs_separator = false;
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
///
/// The pending gap is deliberately left as the content left it: a container adds
/// no spacing of its own, which is why `**bold**.` renders `bold.` and `a**b**c`
/// renders `abc`. The one space in `**a** **b**` comes from the whitespace-only
/// text node between the two containers, not from either of them.
pub(crate) fn render_style_inline(
    content: &[MarkdownInline],
    ctx: &mut RenderCtx,
    modifier: Modifier,
) {
    let prev = ctx.modifier;
    ctx.modifier = prev | modifier;
    render_inlines_to_lines(content, ctx);
    ctx.modifier = prev;
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
        // No destination, no URL: the link renders as its own content, so the
        // pending gap after it is whatever that content left behind.
        render_inlines_to_lines(content, ctx);
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
    if *ctx.current_width + url_width > ctx.width && *ctx.current_width >= ctx.indent {
        // The URL alone does not fit what is left of the row. Hard-split it at
        // the row boundary exactly as an over-wide word is split: a destination
        // longer than the row is common (`https://…`), and pushing it whole
        // would overrun the content width — and, in a table cell, desync the
        // frame from its border columns. The separator stays on this row when
        // there is any room for it, so a split URL never leaves a dangling
        // ` - ` alone on a line.
        if *ctx.current_width >= ctx.width {
            ctx.flush_line_with_next(LineJoin::Space);
        }
        ctx.render_word_split(destination, ctx.base_style());
    } else {
        ctx.push_span(destination.to_string(), ctx.base_style());
        *ctx.current_width += url_width;
    }
    ctx.modifier = prev;

    // The link ends at its URL, which carries no trailing whitespace: a gap
    // before the next node belongs to that node's own leading whitespace.
    *ctx.needs_separator = false;
}
