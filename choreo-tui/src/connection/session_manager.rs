use crate::state::{App, Page, SessionManagerView, session_list_click_index};
use choreo_client_core::{ClientError, broken_pipe};
use choreo_proto::ClientMessage;
use crossterm::event::{Event, KeyCode, KeyEventKind, MouseButton, MouseEvent, MouseEventKind};

pub(super) fn handle_session_manager_event(
    event: Event,
    app: &mut App,
    client_tx: &std::sync::mpsc::Sender<ClientMessage>,
) -> Result<(), ClientError> {
    match app.session_mgr.view {
        SessionManagerView::List => handle_session_list_event(event, app, client_tx),
        SessionManagerView::Detail => handle_session_detail_event(event, app, client_tx),
    }
}

fn handle_session_list_event(
    event: Event,
    app: &mut App,
    client_tx: &std::sync::mpsc::Sender<ClientMessage>,
) -> Result<(), ClientError> {
    match event {
        Event::Key(key) if key.kind == KeyEventKind::Press => {
            handle_session_list_key(key, app, client_tx)
        }
        Event::Mouse(mouse) => handle_session_list_mouse(mouse, app, client_tx),
        _ => Ok(()),
    }
}

fn handle_session_detail_event(
    event: Event,
    app: &mut App,
    client_tx: &std::sync::mpsc::Sender<ClientMessage>,
) -> Result<(), ClientError> {
    match event {
        Event::Key(key) if key.kind == KeyEventKind::Press => {
            handle_session_detail_key(key, app, client_tx)
        }
        _ => Ok(()),
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

/// Handle mouse events on the session-manager list: a left-click on a visible
/// session row is equivalent to selecting it and pressing Enter (highlight the
/// session and attach to it); the wheel scrolls the highlight.  Clicks outside
/// the list's content rows (block border, status bar, scrollbar column, the
/// table header, or past the last session) and clicks while a deletion
/// confirmation is armed are no-ops.
fn handle_session_list_mouse(
    mouse: MouseEvent,
    app: &mut App,
    client_tx: &std::sync::mpsc::Sender<ClientMessage>,
) -> Result<(), ClientError> {
    if app.session_mgr.confirm_delete.is_some() {
        return Ok(());
    }
    match mouse.kind {
        MouseEventKind::ScrollDown => app.session_mgr.select_down(),
        MouseEventKind::ScrollUp => app.session_mgr.select_up(),
        MouseEventKind::Down(MouseButton::Left) => {
            let total = app.session_mgr.sessions.len();
            // Resolve the click against the RENDERED window start
            // (`window()`, the same function the renderer draws with), never
            // the stored `scroll`: a reorder/resize leaves the stored anchor
            // stale and the renderer clamps it, so the raw value would map
            // the click onto a different row than the one drawn.  The
            // geometry is derived from the last known terminal size; before
            // the first frame it is unknown, so every click is a no-op
            // (`session_list_click_index` returns `None` then).
            let (window_start, _) = app.session_mgr.window(app.session_mgr.viewport_height);
            let Some(idx) = session_list_click_index(
                app.last_terminal_size,
                mouse.column,
                mouse.row,
                total,
                window_start,
            ) else {
                return Ok(());
            };
            // Select the clicked session, then repeat the Enter action on the
            // current selection (the Enter handler reads `selection`).  If the
            // attach send fails (broken pipe), the error propagates and the
            // user stays on the session list with the clicked session
            // highlighted — not stranded on Chat with an un-attached session.
            app.session_mgr.selection = Some(idx);
            if let Some(session) = app.session_mgr.sessions.get(idx) {
                let session_id = session.session_id;
                app.attach_to_session(session_id, client_tx)?;
            }
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
