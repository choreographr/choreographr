use crate::state::{App, Page, SessionManagerView};
use choreo_client_core::{ClientError, broken_pipe};
use choreo_proto::ClientMessage;
use crossterm::event::{Event, KeyCode, KeyEventKind};

pub(super) fn handle_session_manager_event(
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

    match app.session_mgr.view {
        SessionManagerView::List => handle_session_list_key(key, app, client_tx),
        SessionManagerView::Detail => handle_session_detail_key(key, app, client_tx),
    }
}

fn handle_session_list_key(
    key: crossterm::event::KeyEvent,
    app: &mut App,
    client_tx: &std::sync::mpsc::Sender<ClientMessage>,
) -> Result<(), ClientError> {
    // If in delete-confirmation mode, handle y/n/Esc first
    if app.session_mgr.confirm_delete.is_some() {
        match key.code {
            KeyCode::Char('y') | KeyCode::Char('Y') => {
                if let Some((session_id, _title)) = app.session_mgr.confirm_delete.take() {
                    client_tx
                        .send(ClientMessage::DeleteSession { session_id })
                        .map_err(broken_pipe)?;
                }
            }
            KeyCode::Char('n') | KeyCode::Char('N') | KeyCode::Esc => {
                app.session_mgr.confirm_delete = None;
            }
            _ => {}
        }
        return Ok(());
    }

    match key.code {
        KeyCode::Up | KeyCode::Char('k') => app.session_mgr.select_up(),
        KeyCode::Down | KeyCode::Char('j') => app.session_mgr.select_down(),
        KeyCode::PageUp => {
            app.session_mgr.scroll_up_page();
        }
        KeyCode::PageDown => {
            app.session_mgr.scroll_down_page();
        }
        KeyCode::Enter => {
            if let Some(sel) = app.session_mgr.selection
                && let Some(session) = app.session_mgr.sessions.get(sel)
            {
                let session_id = session.session_id;
                // Shared attach sequence (send-before-mutate, so a broken
                // pipe leaves the user on the session list instead of
                // stranding them on Chat with an un-attached session).
                app.attach_to_session(session_id, client_tx)?;
            }
        }
        KeyCode::Char('i') => app.session_mgr.enter_detail(),
        KeyCode::Char('n') => {
            tracing::info!("[choreo-tui] pressing n on session list -> CreateSession");
            client_tx
                .send(ClientMessage::CreateSession {
                    title: None,
                    parent_session_id: None,
                    // Inherit fields from the currently attached session.
                    working_dir: app.active_display_ref().and_then(|d| d.working_dir.clone()),
                    context_config: None,
                    account_name: app
                        .active_display_ref()
                        .and_then(|d| d.account_name.clone()),
                    selected_model: app
                        .active_display_ref()
                        .and_then(|d| d.selected_model.clone()),
                    reasoning_effort: app
                        .active_display_ref()
                        .and_then(|d| d.reasoning_effort.clone()),
                })
                .map_err(broken_pipe)?;
        }
        KeyCode::Char('d') => {
            // Enter delete-confirmation mode for the selected session
            if let Some(sel) = app.session_mgr.selection
                && let Some(session) = app.session_mgr.sessions.get(sel)
            {
                let title = session.title.clone().unwrap_or_else(|| "untitled".into());
                app.session_mgr.confirm_delete = Some((session.session_id, title));
            }
        }
        KeyCode::Esc | KeyCode::Char('q') => {
            app.set_page(Page::Chat);
            let _ = client_tx.send(ClientMessage::UnsubscribeSessionsSummary);
        }
        _ => {}
    }
    Ok(())
}

fn handle_session_detail_key(
    key: crossterm::event::KeyEvent,
    app: &mut App,
    client_tx: &std::sync::mpsc::Sender<ClientMessage>,
) -> Result<(), ClientError> {
    match key.code {
        KeyCode::Char('b') | KeyCode::Esc => {
            app.session_mgr.leave_detail();
        }
        KeyCode::Enter => {
            if let Some(ref detail) = app.session_mgr.detail_data {
                let session_id = detail.session_id;
                // Shared attach sequence — a failed send leaves the user on
                // the detail view, not stranded on Chat with an un-attached
                // session.
                app.attach_to_session(session_id, client_tx)?;
            }
        }
        _ => {}
    }
    Ok(())
}
