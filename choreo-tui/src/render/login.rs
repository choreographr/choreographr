//! Renders the OAuth login modal: the streamed progress of a `/login` flow.
//!
//! The modal is the topmost overlay (see `render`), covering whatever page is
//! beneath. It shows the provider and account, the latest status line, the
//! authorize URL (so a headless/SSH user can open it elsewhere), a device code
//! when the flow uses one, and the Esc-to-cancel hint. It never shows a token,
//! an `id_token`, or a callback URL carrying a code — none of those ever reach a
//! [`LoginEvent`](choreo_client_core::LoginEvent).

use crate::state::{App, PopupSize, centered_popup};

use super::{display_text, display_text_owned};
use ratatui::{
    Frame,
    style::{Color, Style},
    text::{Line, Text},
    widgets::{Block, Clear, Paragraph, Wrap},
};

/// Draw the login modal over the current frame.
pub(crate) fn render_login_modal(frame: &mut Frame<'_>, app: &App) {
    let Some(modal) = app.login.as_ref() else {
        return;
    };
    let area = centered_popup(frame.area(), PopupSize::LIST);
    // Clear first so the modal reads as opaque over the page beneath.
    frame.render_widget(Clear, area);

    // Everything in this modal arrives from outside the TUI: the provider and
    // account names from the credential, the status line and device code from
    // the authorization server, and the authorize URL.  Normalize each so the
    // popup's lines occupy exactly the columns they draw and no live escape
    // sequence rides along (see `display_text`).
    let mut lines = vec![
        Line::from(format!("Provider: {}", display_text(&modal.provider))),
        Line::from(format!("Account:  {}", display_text(&modal.account))),
        Line::from(""),
        Line::from(display_text_owned(&modal.status)),
    ];
    if let Some(url) = &modal.auth_url {
        lines.push(Line::from(""));
        lines.push(Line::from("Open this URL in a browser to sign in:"));
        lines.push(Line::from(display_text_owned(url)));
    }
    if let Some((code, uri)) = &modal.device_code {
        lines.push(Line::from(""));
        lines.push(Line::from(format!(
            "Enter the code {} at {}",
            display_text(code),
            display_text(uri)
        )));
    }
    lines.push(Line::from(""));
    lines.push(dim_line("Esc: cancel"));

    let block = Block::bordered()
        .title(" Sign in ")
        .border_style(Style::default().fg(Color::Yellow));
    let paragraph = Paragraph::new(Text::from(lines))
        .block(block)
        .wrap(Wrap { trim: false });
    frame.render_widget(paragraph, area);
}

/// A dimmed informational line.
fn dim_line(text: &str) -> Line<'static> {
    Line::from(text.to_string()).style(Style::default().fg(Color::DarkGray))
}
