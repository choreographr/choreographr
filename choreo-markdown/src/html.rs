//! Markdown → sanitized HTML rendering.

use std::sync::OnceLock;

use ammonia::Builder as HtmlSanitizer;
use pulldown_cmark::{Parser, html};

use crate::MarkdownDocument;
use crate::math::normalize_math_event;
use crate::parse::markdown_options;

/// Parse a markdown string and render it to sanitized HTML in one step.
///
/// This is a convenience function that combines parsing and HTML rendering.
/// It sanitizes the output with [`ammonia`] to prevent XSS attacks.
#[must_use]
pub fn render_markdown_html(input: &str) -> String {
    let mut html_output = String::new();
    html::push_html(
        &mut html_output,
        Parser::new_ext(input, markdown_options()).map(normalize_math_event),
    );
    sanitize_html(&html_output)
}

impl MarkdownDocument {
    /// Convert the AST back to markdown, then render that to sanitized HTML.
    ///
    /// This is useful when you need to modify the AST and then produce HTML output.
    #[must_use]
    pub fn to_html(&self) -> String {
        render_markdown_html(&self.to_markdown())
    }
}

fn sanitize_html(html: &str) -> String {
    static SANITIZER: OnceLock<HtmlSanitizer> = OnceLock::new();
    let sanitizer = SANITIZER.get_or_init(|| {
        let mut s = HtmlSanitizer::default();
        s.add_tags(["table", "thead", "tbody", "tr", "th", "td"]);
        s.add_tag_attributes("th", ["align"]);
        s.add_tag_attributes("td", ["align"]);
        s
    });
    sanitizer.clean(html).to_string()
}
