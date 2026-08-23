use crate::state::{
    AccountWizardStep, App, Page, SelectorClick, selector_click_target,
    selector_position_filter_cursor,
};
use choreo_client_core::{ClientError, broken_pipe, is_valid_account_name};
use choreo_proto::ClientMessage;
use crossterm::event::{Event, KeyCode, KeyEventKind, MouseButton, MouseEventKind};
use tui_prompts::State;

pub(super) fn handle_ai_providers_event(
    event: Event,
    app: &mut App,
    client_tx: &std::sync::mpsc::Sender<ClientMessage>,
) -> Result<(), ClientError> {
    // The wizard and credential modals are dispatched from `handle_ui_event`
    // before this function; only the accounts list reaches here.
    handle_ai_providers_list_key(event, app, client_tx)
}

fn handle_ai_providers_list_key(
    event: Event,
    app: &mut App,
    client_tx: &std::sync::mpsc::Sender<ClientMessage>,
) -> Result<(), ClientError> {
    let Event::Key(key) = event else {
        return Ok(());
    };
    if key.kind != KeyEventKind::Press {
        return Ok(());
    }

    // If in delete-confirmation mode, handle y/n/Esc first
    if app.ai_providers.confirm_remove.is_some() {
        match key.code {
            KeyCode::Char('y') | KeyCode::Char('Y') => {
                if let Some(name) = app.ai_providers.confirm_remove.take() {
                    tracing::info!(name, "sending RemoveAccount");
                    client_tx
                        .send(ClientMessage::RemoveAccount { name: name.clone() })
                        .map_err(broken_pipe)?;
                }
            }
            KeyCode::Char('n') | KeyCode::Char('N') | KeyCode::Esc => {
                app.ai_providers.confirm_remove = None;
            }
            _ => {}
        }
        return Ok(());
    }

    match key.code {
        KeyCode::Up | KeyCode::Char('k') => app.ai_providers.select_up(),
        KeyCode::Down | KeyCode::Char('j') => app.ai_providers.select_down(),
        KeyCode::PageUp => {
            app.ai_providers.scroll_up_page();
        }
        KeyCode::PageDown => {
            app.ai_providers.scroll_down_page();
        }

        // Enter selects the highlighted account as the account for the
        // active (attached) session, then returns to the chat page.  The
        // daemon confirms with SessionAccountSet, which refreshes the
        // status-bar provider slug via handle_session_account_set.
        KeyCode::Enter => {
            if let Some(sel) = app.ai_providers.selection
                && let Some(account) = app.ai_providers.accounts.get(sel)
            {
                let name = account.name.clone();
                // Send before flipping pages: if the send fails (broken
                // pipe), the error propagates and the user stays on the
                // accounts page instead of being stranded on Chat with an
                // un-sent selection.
                tracing::debug!(name, "selecting account for the active session");
                client_tx
                    .send(ClientMessage::SetSessionAccount { name })
                    .map_err(broken_pipe)?;
                app.set_page(Page::Chat);
            }
        }

        // Remove account (with confirmation)
        KeyCode::Char('r') => {
            if let Some(sel) = app.ai_providers.selection
                && let Some(account) = app.ai_providers.accounts.get(sel)
            {
                tracing::debug!(name = %account.name, "account removal confirmation armed");
                app.ai_providers.confirm_remove = Some(account.name.clone());
            }
        }
        // New account: open the wizard modal (searchable provider picker,
        // then slug entry, then the credential modal).
        KeyCode::Char('n') => {
            tracing::debug!("opening new-account wizard");
            app.ai_providers.wizard.open();
        }
        // Set credential (API key) for the selected account: open the
        // credential modal.
        KeyCode::Char('c') => {
            if let Some(sel) = app.ai_providers.selection
                && let Some(account) = app.ai_providers.accounts.get(sel)
            {
                tracing::debug!(name = %account.name, "opening credential modal");
                app.ai_providers.credential.open(account.name.clone());
            }
        }
        KeyCode::Esc | KeyCode::Char('q') => {
            app.set_page(Page::Chat);
        }
        _ => {}
    }
    Ok(())
}

/// Handle keys while the API-key modal is open.  Enter encrypts and sends the
/// credential (auto-unlocking the daemon so it is immediately usable); Esc
/// cancels without saving; every other key edits the masked input buffer.
pub(super) fn handle_credential_modal_event(
    event: Event,
    app: &mut App,
    client_tx: &std::sync::mpsc::Sender<ClientMessage>,
) -> Result<(), ClientError> {
    let Event::Key(key) = event else {
        return Ok(());
    };
    if key.kind != KeyEventKind::Press {
        return Ok(());
    }

    match key.code {
        // Enter saves the credential.
        KeyCode::Enter => {
            let account_name = match app.ai_providers.credential.target.take() {
                Some(name) => name,
                None => return Ok(()),
            };
            let api_key = app.ai_providers.credential.input.text.trim().to_string();
            // Wipe the typed key from the input buffer before it is dropped
            // (the daemon zeroizes its stored `ServiceCredential` copies; this
            // covers the TUI's transient copy).  `api_key` is a trimmed copy
            // moved into the encrypted message below.
            app.ai_providers.credential.wipe_input();

            if api_key.is_empty() {
                tracing::debug!(account_name, "credential save rejected: empty key");
                app.ai_providers.credential.error = Some("API key cannot be empty".to_string());
                app.ai_providers.credential.target = Some(account_name);
                return Ok(());
            }

            app.ai_providers.credential.error = None;

            // Build and send the encrypted credential, auto-unlocking
            // the daemon so the credential is immediately usable.
            match choreo_client_core::build_add_credential_message(
                account_name.clone(),
                "api_key".to_string(),
                vec![api_key],
                true,
            ) {
                Ok(msg) => {
                    tracing::info!(account_name, "credential encrypted and sent");
                    let _ = client_tx.send(msg);
                    app.status = Some(format!(
                        "[daemon] credential stored for account: {account_name}"
                    ));
                }
                Err(e) => {
                    tracing::warn!(account_name, %e, "failed to encrypt API key");
                    app.status = Some(format!(
                        "[warning] failed to encrypt API key for {account_name}: {e}"
                    ));
                }
            }
        }
        // Esc cancels (the account stays, the key is skipped).
        KeyCode::Esc => {
            if let Some(target) = app.ai_providers.credential.target.as_deref() {
                tracing::debug!(target, "credential modal cancelled");
            }
            app.ai_providers.credential.close();
        }
        // All other keys go to the credential input buffer.
        _ => {
            app.ai_providers.credential.input.handle_key(key);
        }
    }
    Ok(())
}

/// Handle keys while the new-account wizard modal is open.  Step 1 (Provider):
/// ↑/↓/PgUp/PgDn move the highlight, the mouse scroll wheel navigates like
/// the arrows, typing filters the provider list by display name, Enter (or a
/// left-click on a list row) picks the highlighted provider and advances to
/// step 2, a left-click on the filter row positions the cursor, Esc cancels
/// the whole flow.  Step 2 (Slug): typing edits the slug field, Enter
/// validates and submits `AddAccount` (then auto-opens the credential modal),
/// Esc returns to the provider picker.
pub(super) fn handle_account_wizard_event(
    event: Event,
    app: &mut App,
    client_tx: &std::sync::mpsc::Sender<ClientMessage>,
) -> Result<(), ClientError> {
    match event {
        Event::Key(key) => {
            if key.kind != KeyEventKind::Press {
                return Ok(());
            }

            match app.ai_providers.wizard.step {
                AccountWizardStep::Provider => {
                    // Esc cancels the whole wizard back to the account list.
                    if key.code == KeyCode::Esc {
                        tracing::debug!("account wizard cancelled from provider picker");
                        app.ai_providers.wizard.close();
                        return Ok(());
                    }
                    // Enter picks the highlighted provider and advances to the slug
                    // modal (a no-op when the filtered list is empty).
                    if key.code == KeyCode::Enter {
                        app.ai_providers.wizard.confirm_provider(&app.providers);
                        if let Some(slug) = app.ai_providers.wizard.picked_slug.as_deref() {
                            tracing::debug!(slug, "provider picked in account wizard");
                        }
                        return Ok(());
                    }
                    match key.code {
                        // Only arrow/paging keys navigate here — j/k MUST reach the
                        // filter so the user can search for providers whose names
                        // contain 'j' or 'k' (dozens: "Kimi For Coding", "Jiekou.AI",
                        // "Sakana AI", "Amazon Bedrock", …).  This mirrors the model
                        // selector, which also reserves just Up/Down.
                        KeyCode::Up => app.ai_providers.wizard.move_up(&app.providers),
                        KeyCode::Down => app.ai_providers.wizard.move_down(&app.providers),
                        // PgUp/PgDn page the highlight (the render window follows
                        // it), so browsing ~200 providers takes a few keypresses,
                        // not a row-by-row walk.
                        KeyCode::PageUp => app.ai_providers.wizard.page_up(),
                        KeyCode::PageDown => app.ai_providers.wizard.page_down(&app.providers),
                        // Everything else goes to the filter input (characters,
                        // backspace, word deletes, cursor movement).  `filter_key`
                        // returns false for Enter/Esc, which are handled above.
                        _ => {
                            app.ai_providers.wizard.filter_key(key, &app.providers);
                        }
                    }
                }
                AccountWizardStep::Slug => {
                    // Esc backs out to the provider picker, keeping the pick.
                    if key.code == KeyCode::Esc {
                        tracing::debug!("account wizard backed out of slug step");
                        app.ai_providers.wizard.back_to_provider(&app.providers);
                        return Ok(());
                    }
                    // Enter validates the slug and creates the account.
                    if key.code == KeyCode::Enter {
                        let slug = app.ai_providers.wizard.slug.value().trim().to_string();
                        if slug.is_empty() {
                            tracing::debug!("slug rejected: empty");
                            app.ai_providers.wizard.error =
                                Some("Account slug is required".to_string());
                            return Ok(());
                        }
                        if !is_valid_account_name(&slug) {
                            tracing::debug!(%slug, "slug rejected: invalid characters");
                            app.ai_providers.wizard.error = Some(
                                "slug must be lowercase alphanumeric, hyphens, or underscores"
                                    .to_string(),
                            );
                            return Ok(());
                        }
                        app.ai_providers.wizard.error = None;
                        submit_new_account(app, client_tx)?;
                    } else {
                        // All other keys go to the slug input buffer.
                        app.ai_providers.wizard.slug.handle_key_event(key);
                    }
                }
            }
            Ok(())
        }
        Event::Mouse(mouse) => {
            // Mouse interaction applies only to the provider picker (step 1):
            // the wheel scrolls the highlight (1 row per notch, same
            // pin-at-middle behavior as the arrows), a click on a visible list
            // row selects it (like Enter), a click on the filter row positions
            // the cursor, and clicks anywhere else (dimmed area, outside the
            // popup) are no-ops.  Step 2 (slug entry) has no list to interact
            // with, so its mouse events stay ignored.
            if app.ai_providers.wizard.step != AccountWizardStep::Provider {
                return Ok(());
            }
            match mouse.kind {
                MouseEventKind::ScrollDown => app.ai_providers.wizard.move_down(&app.providers),
                MouseEventKind::ScrollUp => app.ai_providers.wizard.move_up(&app.providers),
                MouseEventKind::Down(MouseButton::Left) => {
                    // Resolve the click against the RENDERED window start
                    // (`window()`, the same function the renderer draws with),
                    // never the stored `scroll`: a filter narrowing or a
                    // PgUp/PgDn jump can leave `scroll` stale (past the new
                    // max_scroll), and the raw value would map the click onto
                    // a different row than the one drawn.  The popup geometry
                    // is derived from the last known terminal size; before
                    // the first frame it is unknown, so every click is a
                    // no-op (`selector_click_target` returns `Noop` then).
                    let filtered = app.ai_providers.wizard.filtered(&app.providers);
                    let (start, _) = app
                        .ai_providers
                        .wizard
                        .window(&filtered, app.ai_providers.wizard.viewport_height);
                    match selector_click_target(
                        app.last_terminal_size,
                        mouse.column,
                        mouse.row,
                        filtered.len(),
                        start,
                    ) {
                        SelectorClick::Row(idx) => {
                            // Click in the list body: move the highlight to
                            // the clicked row and pick it, exactly like Enter.
                            app.ai_providers.wizard.focused = idx;
                            app.ai_providers.wizard.confirm_provider(&app.providers);
                            if let Some(slug) = app.ai_providers.wizard.picked_slug.as_deref() {
                                tracing::debug!(slug, "provider picked in account wizard");
                            }
                        }
                        SelectorClick::FilterRow {
                            column,
                            filter_row_x,
                        } => {
                            // Click in the filter row: position the input
                            // cursor at the clicked column.
                            selector_position_filter_cursor(
                                &mut app.ai_providers.wizard.filter,
                                filter_row_x,
                                column,
                            );
                        }
                        SelectorClick::Noop => {}
                    }
                }
                _ => {}
            }
            Ok(())
        }
        _ => Ok(()),
    }
}

/// Send AddAccount for the slug entered in step 2, then close the wizard and
/// auto-open the credential modal so the user can immediately paste an API
/// key.
fn submit_new_account(
    app: &mut App,
    client_tx: &std::sync::mpsc::Sender<ClientMessage>,
) -> Result<(), ClientError> {
    let slug = app.ai_providers.wizard.slug.value().trim().to_string();
    let provider_str = app
        .ai_providers
        .wizard
        .picked_slug
        .clone()
        // The provider is always chosen before the slug step is reachable; if
        // it somehow is not, fall back to the first option rather than
        // producing an empty provider (the terminal `unwrap_or_default` is
        // unreachable: `app.providers` is initialized from the non-empty
        // compile-time table).
        .or_else(|| app.providers.first().map(|p| p.slug.clone()))
        .unwrap_or_default();

    app.ai_providers.wizard.error = None;

    tracing::info!(%slug, %provider_str, "sending AddAccount");

    // Create the account (no credential yet — the credential modal handles
    // that next).
    client_tx
        .send(ClientMessage::AddAccount {
            name: slug.clone(),
            provider: provider_str,
            base_url: None,
            streaming: None,
            retry_max_attempts: None,
            connect_timeout_secs: None,
            request_timeout_secs: None,
            total_timeout_secs: None,
        })
        .map_err(broken_pipe)?;

    // Close the wizard and immediately open the credential modal for the
    // account just created.
    app.ai_providers.wizard.close();
    app.ai_providers.credential.open(slug);
    Ok(())
}
