// The AI-provider accounts page: the account list plus the credential and
// new-account-wizard modals, extracted from the former monolithic render.rs.
// Shared helpers (popup sizing/centering, cursor positioning, the scrollbar
// widget) live in the parent render/mod.rs and are imported below.
use crate::markdown_render::display_width;
use crate::scrollbar::SmoothScrollbarState;
use crate::state::{AI_PROVIDER_ITEM_LINES, AccountWizardStep, App};
use ratatui::{
    Frame,
    layout::{Constraint, Direction, Layout},
    style::{Color, Modifier, Style},
    text::{Line, Span},
    widgets::{Block, Borders, Clear, Paragraph},
};
use tui_prompts::{Prompt, TextPrompt};

use super::{PopupSize, centered_popup, set_input_cursor, vertical_scrollbar};

// ── AI Provider Accounts ──────────────────────────────────

// Dispatched from the top-level `render()` in render/mod.rs.
pub(super) fn render_ai_providers(frame: &mut Frame<'_>, app: &mut App) {
    render_ai_providers_list(frame, app);
    // The wizard and credential modals overlay the list (drawn last), exactly
    // like the model selector overlays the chat page.  The credential modal
    // wins when both are open — the wizard closes before the credential modal
    // auto-opens after account creation, so this is belt-and-braces.
    if app.ai_providers.credential.is_open() {
        render_credential_modal(frame, app);
    } else if app.ai_providers.wizard.is_open() {
        match app.ai_providers.wizard.step {
            AccountWizardStep::Provider => render_wizard_provider(frame, app),
            AccountWizardStep::Slug => render_wizard_slug(frame, app),
        }
    }
}

fn render_ai_providers_list(frame: &mut Frame<'_>, app: &mut App) {
    let area = frame.area();
    let chunks = Layout::default()
        .direction(Direction::Vertical)
        .constraints([Constraint::Min(1), Constraint::Length(1)])
        .split(area);

    let block = Block::default()
        .title(" AI Provider Accounts ")
        .borders(Borders::ALL);
    let inner = block.inner(chunks[0]);
    frame.render_widget(block, chunks[0]);

    let list_chunks = Layout::default()
        .direction(Direction::Horizontal)
        .constraints([Constraint::Min(1), Constraint::Length(1)])
        .split(inner);

    let scroll = app.ai_providers.scroll;
    let max_rows = list_chunks[0].height as usize;
    let total_items = app.ai_providers.accounts.len();

    if total_items == 0 {
        let msg = Paragraph::new("No AI provider accounts configured. Press 'n' to add one.");
        frame.render_widget(msg, list_chunks[0]);
    } else {
        let mut lines: Vec<Line> = Vec::new();

        for i in scroll..total_items {
            if lines.len() + 3 > max_rows && i != scroll {
                break;
            }
            let account = &app.ai_providers.accounts[i];
            let is_selected = Some(i) == app.ai_providers.selection;

            let sel = if is_selected { ">" } else { " " };

            let style = if is_selected {
                Style::default().bg(Color::Blue).fg(Color::White)
            } else {
                Style::default()
            };

            let name_line = format!("{sel} {} ", account.name);
            let name_spans = vec![ratatui::text::Span::styled(name_line, style)];
            lines.push(Line::from(name_spans));

            let provider_label = format!("   Provider: {}", account.provider);
            let provider_style = if is_selected {
                Style::default().bg(Color::Blue).fg(Color::White)
            } else {
                Style::default().fg(Color::DarkGray)
            };
            lines.push(Line::from(vec![ratatui::text::Span::styled(
                provider_label,
                provider_style,
            )]));

            let cred_label = if account.has_credential {
                "   Credential: yes".to_string()
            } else {
                "   Credential: no".to_string()
            };
            let cred_style = if is_selected {
                Style::default().bg(Color::Blue).fg(Color::White)
            } else if account.has_credential {
                Style::default().fg(Color::Green)
            } else {
                Style::default().fg(Color::DarkGray)
            };
            lines.push(Line::from(vec![ratatui::text::Span::styled(
                cred_label, cred_style,
            )]));

            if lines.len() < max_rows && i + 1 < total_items {
                lines.push(Line::from(Span::styled(String::new(), Style::default())));
            }
        }

        let paragraph = Paragraph::new(lines);
        frame.render_widget(paragraph, list_chunks[0]);
    }

    let items_per_page = (max_rows / AI_PROVIDER_ITEM_LINES).max(1);
    if total_items > items_per_page {
        frame.render_stateful_widget(
            vertical_scrollbar(),
            list_chunks[1],
            &mut SmoothScrollbarState::new(total_items)
                .position(scroll)
                .viewport_content_length(items_per_page),
        );
    }

    let status = if let Some(ref name) = app.ai_providers.confirm_remove {
        Paragraph::new(Line::from(format!(" Remove \"{name}\"? (y/N)  ")))
    } else {
        Paragraph::new(Line::from(format!(
            " <j/k nav>  <r remove>  <c credential>  <n new>  <Esc back>  —  {} accounts",
            total_items
        )))
    };
    frame.render_widget(status, chunks[1]);
}

/// Centered popup for entering an API key: `c` on an existing account, or
/// auto-opened right after the new-account wizard creates one.  The key is
/// masked on screen; Enter encrypts and sends it (connection.rs), Esc cancels.
fn render_credential_modal(frame: &mut Frame<'_>, app: &mut App) {
    let area = frame.area();
    // A compact popup — there is a single input field, so it only needs to
    // be tall enough for the prompt, the masked input, the error row, and the
    // footer hint.
    let popup = centered_popup(
        area,
        PopupSize {
            w_num: 2,
            w_den: 3,
            h_num: 2,
            h_den: 5,
            min_w: 40,
            min_h: 9,
            max_w: 80,
            max_h: 13,
        },
    );
    frame.render_widget(Clear, popup);

    let account_name = app
        .ai_providers
        .credential
        .target
        .as_deref()
        .unwrap_or("(unknown)");

    let block = Block::default()
        .title(format!(" API Key for \"{account_name}\" "))
        .borders(Borders::ALL)
        .border_style(Style::default().fg(Color::Cyan));
    let inner = block.inner(popup);
    frame.render_widget(block, popup);

    let rows = Layout::default()
        .direction(Direction::Vertical)
        .constraints([
            Constraint::Length(1),
            Constraint::Length(3),
            Constraint::Length(1),
            Constraint::Min(1),
        ])
        .split(inner);

    let dim = Style::default().fg(Color::DarkGray);
    let input_style = Style::default().fg(Color::Cyan);

    frame.render_widget(
        Paragraph::new(Line::from(Span::styled(
            "  Paste the API key for this account:",
            dim,
        ))),
        rows[0],
    );

    let text = &app.ai_providers.credential.input.text;
    let cursor = app.ai_providers.credential.input.cursor;
    // Render ONLY bullets — never any plaintext of the key.  The bullet count
    // mirrors the real character count only up to a fixed cap (24), so a long
    // key renders identically to a 24-char key and an onlooker cannot infer
    // the true length.  Deliberately no byte slicing here: bullets are
    // multi-byte UTF-8 and the real text may be too, so `&text[..4]`-style
    // masks panic on non-char-boundary offsets.
    let masked = "•".repeat(text.chars().count().min(24));
    let display = if text.is_empty() {
        "> ".to_string()
    } else {
        format!("> {masked}")
    };
    frame.render_widget(
        Paragraph::new(Line::from(Span::styled(display, input_style))),
        rows[1],
    );

    // Park the terminal cursor by CHARACTER count, not byte offset.  The
    // InputBuffer cursor is a byte index into the real key that can split a
    // multi-byte char, and the mask is built from multi-byte bullets, so
    // byte-slicing either one panics.  Count the chars of real text before the
    // cursor (`get` fails safely to 0 when the cursor is stale) and mirror
    // that many chars of the mask.
    let masked_before = if text.is_empty() {
        String::new()
    } else {
        let n = text
            .get(..cursor)
            .map_or(0, |s| s.chars().count())
            .min(masked.chars().count());
        masked.chars().take(n).collect::<String>()
    };
    set_input_cursor(frame, rows[1], 0, 2, &masked_before);

    if let Some(ref err) = app.ai_providers.credential.error {
        frame.render_widget(
            Paragraph::new(Line::from(Span::styled(
                format!("  Error: {err}"),
                Style::default().fg(Color::Red),
            ))),
            rows[2],
        );
    }

    let status = Paragraph::new(Line::from(Span::styled(
        " enter save · esc cancel ",
        Style::default().fg(Color::DarkGray),
    )));
    frame.render_widget(status, rows[3]);
}

/// Step 1 of the new-account wizard: a centered, searchable provider picker.
/// Mirrors the model selector — filter row on top, scrollable list of display
/// names, footer hint.  The canonical slug is deliberately NOT shown (it is
/// easily confused with the account slug entered in step 2).  Key handling
/// lives in `handle_account_wizard_event` (connection.rs).
fn render_wizard_provider(frame: &mut Frame<'_>, app: &mut App) {
    let area = frame.area();
    // Same footprint as the model selector: ~60% width, ~2/3 height, so the
    // 200+ provider names get a comfortable list.
    let popup = centered_popup(area, PopupSize::LIST);
    frame.render_widget(Clear, popup);

    let block = Block::default()
        .title(" Select Provider ")
        .borders(Borders::ALL)
        .border_style(Style::default().fg(Color::Cyan));
    let inner = block.inner(popup);
    frame.render_widget(block, popup);

    let chunks = Layout::default()
        .direction(Direction::Vertical)
        .constraints([
            Constraint::Length(1),
            Constraint::Min(1),
            Constraint::Length(1),
        ])
        .split(inner);

    // ── Filter row ──────────────────────────────────────────────
    let filter_row = chunks[0];
    let filter_prefix = "> ";
    let filter_display = format!("{filter_prefix}{}", app.ai_providers.wizard.filter.text);
    frame.render_widget(
        Paragraph::new(Line::from(Span::styled(
            filter_display,
            Style::default().fg(Color::White),
        ))),
        filter_row,
    );
    // Park the terminal cursor right after the filter text.  `cursor` is a
    // byte offset (InputBuffer's convention); clamp the column so a long
    // filter never pushes the cursor off-screen.
    let before_cursor = app
        .ai_providers
        .wizard
        .filter
        .text
        .get(..app.ai_providers.wizard.filter.cursor)
        .unwrap_or(&app.ai_providers.wizard.filter.text);
    let cursor_col =
        filter_row.x + filter_prefix.len() as u16 + display_width(before_cursor) as u16;
    let cursor_col = cursor_col.min(filter_row.x + filter_row.width.saturating_sub(1));
    frame.set_cursor_position((cursor_col, filter_row.y));

    // ── Body: list / empty state ────────────────────────────────
    let body = chunks[1];
    let list_height = body.height as usize;
    // Filter once and reuse the slice for both the window and the row loop —
    // `window` takes the already-filtered list, so the per-frame draw path
    // does not re-scan (and re-lowercase) every provider name.
    let filtered = app.ai_providers.wizard.filtered(&app.providers);
    // Compute the visible window (pure — `window` never mutates state, so
    // drawing the popup cannot disturb scroll/focus state mid-frame).
    let (scroll, count) = app.ai_providers.wizard.window(&filtered, list_height);
    let focused = app.ai_providers.wizard.focused;

    if filtered.is_empty() {
        frame.render_widget(
            Paragraph::new(Line::from(Span::styled(
                " No providers match the filter.",
                Style::default().fg(Color::DarkGray),
            ))),
            body,
        );
        return;
    }

    let mut lines: Vec<Line> = Vec::with_capacity(count);
    for (i, provider) in filtered.iter().enumerate().skip(scroll).take(count) {
        let is_focused = i == focused;
        let prefix = if is_focused { "> " } else { "  " };
        let style = if is_focused {
            Style::default()
                .fg(Color::Yellow)
                .add_modifier(Modifier::BOLD)
        } else {
            Style::default().fg(Color::White)
        };
        lines.push(Line::from(Span::styled(
            format!("{prefix}{}", provider.display_name),
            style,
        )));
    }
    frame.render_widget(Paragraph::new(lines), body);

    // ── Footer hint ─────────────────────────────────────────────
    frame.render_widget(
        Paragraph::new(Line::from(Span::styled(
            " esc cancel · enter select · type to filter",
            Style::default().fg(Color::DarkGray),
        ))),
        chunks[2],
    );
}

/// Step 2 of the new-account wizard: a separate centered modal to enter the
/// account slug (the name the account is stored under, used by `/account`).
/// Shows the provider picked in step 1 for context.  Enter creates the account
/// (connection.rs), Esc returns to the provider picker.
fn render_wizard_slug(frame: &mut Frame<'_>, app: &mut App) {
    let area = frame.area();
    // A compact popup — just the provider context, the slug input, and the
    // error/footer rows.
    let popup = centered_popup(
        area,
        PopupSize {
            w_num: 2,
            w_den: 3,
            h_num: 2,
            h_den: 5,
            min_w: 40,
            min_h: 9,
            max_w: 80,
            max_h: 14,
        },
    );
    frame.render_widget(Clear, popup);

    let block = Block::default()
        .title(" Add Account ")
        .borders(Borders::ALL)
        .border_style(Style::default().fg(Color::Cyan));
    let inner = block.inner(popup);
    frame.render_widget(block, popup);

    let rows = Layout::default()
        .direction(Direction::Vertical)
        .constraints([
            Constraint::Length(2),
            Constraint::Length(3),
            Constraint::Length(1),
            Constraint::Min(1),
        ])
        .split(inner);

    let dim = Style::default().fg(Color::DarkGray);
    let accent = Style::default().fg(Color::Cyan);

    // Provider picked in step 1, shown for context.
    let provider_name = app
        .ai_providers
        .wizard
        .picked_name
        .as_deref()
        .unwrap_or("(none)");
    let lines: Vec<Line> = vec![
        Line::from(Span::styled(format!("  Provider: {provider_name}"), accent)),
        Line::from(Span::styled(
            "  Slug is the account name, e.g. /account <slug>.",
            dim,
        )),
    ];
    frame.render_widget(Paragraph::new(lines), rows[0]);

    let border_style = Style::default().fg(Color::Cyan);
    let slug_prompt = TextPrompt::new(std::borrow::Cow::Borrowed("Slug:"))
        .with_block(Block::bordered().border_style(border_style));
    (&slug_prompt).draw(frame, rows[1], &mut app.ai_providers.wizard.slug);

    if let Some(ref err) = app.ai_providers.wizard.error {
        frame.render_widget(
            Paragraph::new(Line::from(Span::styled(
                format!("  Error: {err}"),
                Style::default().fg(Color::Red),
            ))),
            rows[2],
        );
    }

    let status = Paragraph::new(Line::from(Span::styled(
        " enter create account · esc back to provider ",
        Style::default().fg(Color::DarkGray),
    )));
    frame.render_widget(status, rows[3]);
}
