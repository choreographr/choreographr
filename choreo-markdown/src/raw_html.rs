//! Raw HTML reduction: keep the text a reader would see, drop the markup.
//!
//! Raw HTML is never interpreted — a terminal renders text, so what a reader
//! would see is kept and the markup that carried it is discarded. `<br>` is the
//! one exception: a GFM table cell can only break a line that way, so the tag
//! becomes a `LineBreak` rather than vanishing. `<script>`/`<style>` lose their
//! body as well as their tags, because what they hold is code rather than prose.
//!
//! These helpers are pure `&str` processing so the parser can call them from the
//! `Event::InlineHtml` (single tag) and `Event::Html` (one line of a block) arms
//! without carrying any parser context of their own.

use crate::MarkdownInline;

/// The element name of a raw tag (`<br>`, `</div>`, `<img src="x">`) and whether
/// it closes.
///
/// Returns `None` when the slice is not an element tag: a comment, a
/// `<!DOCTYPE>`, a processing instruction, or a stray `<`. Callers drop those as
/// markup, exactly like an element they do not recognise.
fn html_tag(text: &str) -> Option<(bool, &str)> {
    let body = text.strip_prefix('<')?;
    let (closing, body) = match body.strip_prefix('/') {
        Some(rest) => (true, rest),
        None => (false, body),
    };
    let name_len = body
        .find(|c: char| !(c.is_ascii_alphanumeric() || c == '-' || c == '_'))
        .unwrap_or(body.len());
    let name = body.get(..name_len)?;
    // A name starts with a letter, which is what separates a tag from `<!` and
    // `<?` without special-casing either.
    name.starts_with(|c: char| c.is_ascii_alphabetic())
        .then_some((closing, name))
}

/// Whether `text` is a `<br>`, `<br/>`, or `<br />` tag — the only raw tag whose
/// meaning is a hard line break.
pub(crate) fn is_hard_break_tag(text: &str) -> bool {
    matches!(html_tag(text), Some((false, name)) if name.eq_ignore_ascii_case("br"))
}

/// Whether `text` opens (`Some(false)`) or closes (`Some(true)`) a
/// `<script>`/`<style>` element, whose body is dropped along with the tags.
pub(crate) fn scripting_tag(text: &str) -> Option<bool> {
    let (closing, name) = html_tag(text)?;
    let scripting = name.eq_ignore_ascii_case("script") || name.eq_ignore_ascii_case("style");
    scripting.then_some(closing)
}

/// Reduce one `Event::Html` run to the inline nodes a terminal can show.
///
/// `dropping_script_body` carries the "inside a `<script>`/`<style>` body" state
/// in and out, because pulldown-cmark streams an HTML block one line per event
/// and an element can span several of them (`Html("<script>\n")`, then its body,
/// then `Html("</script>")`). A `<script>`/`<style>` element therefore goes
/// whole — opening tag, body, closing tag — and one that never closes keeps
/// dropping until the block ends, which is what HTML itself would do.
///
/// Every other tag is removed and the text around it kept, except `<br>`, which
/// becomes a `LineBreak`. Character references are left as written (`&amp;`
/// stays `&amp;`): decoding them would be HTML parsing, which this renderer
/// deliberately avoids. Literal newlines inside the run are kept as text — HTML
/// collapses them to whitespace, and the terminal does the same — so only an
/// explicit `<br>` forces a break.
pub(crate) fn strip_html_block(text: &str, dropping_script_body: &mut bool) -> Vec<MarkdownInline> {
    let mut run: Vec<MarkdownInline> = Vec::with_capacity(1);
    let mut rest = text;
    while let Some(open) = rest.find('<') {
        push_text(
            &mut run,
            rest.get(..open).unwrap_or_default(),
            *dropping_script_body,
        );
        let tail = rest.get(open..).unwrap_or_default();
        // A comment runs to its own terminator, which `>`-scanning would miss.
        if let Some(body) = tail.strip_prefix("<!--") {
            rest = body.split_once("-->").map_or("", |(_, after)| after);
            continue;
        }
        // A `<` with no closing `>` is literal text, not markup.
        let Some(close) = tail.find('>') else {
            push_text(&mut run, "<", *dropping_script_body);
            rest = tail.get(1..).unwrap_or_default();
            continue;
        };
        let tag = tail.get(..=close).unwrap_or_default();
        rest = tail.get(close + 1..).unwrap_or_default();
        // Inside a dropped body only the matching close tag matters; a `<br>` or
        // any other tag there is code, not prose, and stays dropped.
        if *dropping_script_body {
            if let Some(true) = scripting_tag(tag) {
                *dropping_script_body = false;
            }
            continue;
        }
        if is_hard_break_tag(tag) {
            run.push(MarkdownInline::LineBreak);
        } else if let Some(closing) = scripting_tag(tag) {
            *dropping_script_body = !closing;
        }
        // Any other tag is markup a terminal cannot show: dropped, leaving the
        // text around it untouched.
    }
    push_text(&mut run, rest, *dropping_script_body);
    run
}

/// Append `text` to `run`, merging with a neighbouring `Text` node, unless the
/// script body is currently being dropped.
fn push_text(run: &mut Vec<MarkdownInline>, text: &str, dropping: bool) {
    if dropping || text.is_empty() {
        return;
    }
    match run.last_mut() {
        Some(MarkdownInline::Text(last)) => last.push_str(text),
        _ => run.push(MarkdownInline::Text(text.to_string())),
    }
}
