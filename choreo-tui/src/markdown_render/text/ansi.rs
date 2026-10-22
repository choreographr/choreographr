//! ANSI-escape-coded text into styled ratatui lines: parse the SGR sequences
//! into colours and attributes via `ansi-to-tui`, then wrap the result with the
//! styled wrapper.
//!
//! The input is expected to have passed the ANSI sink
//! ([`crate::text_filter::normalize_ansi_text`]) first, so the only escape
//! sequences it can still carry are the complete SGR ones the parser handles;
//! anything that fails to parse falls back to plain text rather than dropping
//! the content.

use ansi_to_tui::IntoText as _;

use super::super::{Line, LineJoin, Span, Style, warn};
use super::wrap::{plain_text_lines_joined, wrap_styled_line_joined};

/// Render ANSI-escape-coded text as styled ratatui lines, wrapping at `width`.
/// Falls back to [`plain_text_lines`](super::wrap::plain_text_lines) on parse
/// failure. (Test-only: production always needs the aligned copy metadata, so it
/// calls [`ansi_lines_joined`] directly.)
#[cfg(test)]
pub(crate) fn ansi_lines(text: &str, width: u16) -> Vec<Line<'static>> {
    ansi_lines_joined(text, width).0
}

/// [`ansi_lines`] plus the per-line [`LineJoin`] copy metadata (see the enum
/// docs).  Every original `\n` delimited line is a fresh row
/// ([`LineJoin::Break`]); the word-wrap inside an over-long line records
/// [`LineJoin::Space`]/[`LineJoin::Join`] per break via
/// [`wrap_styled_line_joined`].
pub(crate) fn ansi_lines_joined(text: &str, width: u16) -> (Vec<Line<'static>>, Vec<LineJoin>) {
    let width_usize = width as usize;

    match text.as_bytes().into_text() {
        Ok(t) => {
            let mut result: Vec<Line<'static>> = Vec::new();
            let mut joins: Vec<LineJoin> = Vec::new();
            for line in &t.lines {
                if width_usize == 0 || line.width() <= width_usize {
                    let spans: Vec<Span<'static>> = line
                        .spans
                        .iter()
                        .map(|span| Span::styled(span.content.to_string(), span.style))
                        .collect();
                    result.push(Line::from(spans));
                    joins.push(LineJoin::Break);
                } else {
                    // Word-wrap this over-long line at width.
                    wrap_styled_line_joined(line, width_usize, &mut result, &mut joins);
                }
            }
            if result.is_empty() {
                (
                    vec![Line::from(Span::styled(String::new(), Style::default()))],
                    vec![LineJoin::Break],
                )
            } else {
                (result, joins)
            }
        }
        Err(e) => {
            warn!(
                error = %e,
                text_len = text.len(),
                "failed to parse ANSI escape codes, falling back to plain text"
            );
            plain_text_lines_joined(text, width)
        }
    }
}
