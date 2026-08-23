use crate::state::{App, apply_selector_left_click};
use choreo_client_core::{ClientError, broken_pipe};
use choreo_proto::ClientMessage;
use crossterm::event::{Event, KeyCode, KeyEventKind, MouseButton, MouseEventKind};

/// Handle events while the model selector overlay is open (Chat page).
///
/// Up/Down move the highlight, Enter selects the highlighted model and
/// closes (sending `SetModel`), Esc dismisses without changing anything,
/// PgUp/PgDn page the highlight, and every other key feeds the filter box.
/// The mouse scroll wheel navigates like the arrows (pin-at-middle); a
/// left-click on a list row selects it exactly like Enter, and a left-click
/// on the filter row positions the input cursor.  Quit is handled via Ctrl+Q
/// at the terminal-event level.
pub(super) fn handle_model_selector_event(
    event: Event,
    app: &mut App,
    client_tx: &std::sync::mpsc::Sender<ClientMessage>,
) -> Result<(), ClientError> {
    match event {
        Event::Key(key) => {
            if key.kind != KeyEventKind::Press {
                return Ok(());
            }
            // Esc dismisses the overlay without changing the model.
            if key.code == KeyCode::Esc {
                tracing::debug!("[choreo-tui] model selector dismissed");
                app.model_selector.close();
                return Ok(());
            }
            // Enter selects the highlighted model (if any) and closes.  An
            // empty filtered list (e.g. a filter with no matches) simply
            // closes.
            if key.code == KeyCode::Enter {
                if let Some(model) = app.model_selector.submit() {
                    tracing::info!(%model, "model selector: selecting model");
                    client_tx
                        .send(ClientMessage::SetModel { model })
                        .map_err(broken_pipe)?;
                }
                return Ok(());
            }
            match key.code {
                KeyCode::Up => app.model_selector.move_up(),
                KeyCode::Down => app.model_selector.move_down(),
                // PgUp/PgDn page the highlight (the render window follows
                // it), matching the wizard's provider picker.
                KeyCode::PageUp => app.model_selector.page_up(),
                KeyCode::PageDown => app.model_selector.page_down(),
                // Everything else goes to the filter input (characters,
                // backspace, word deletes, cursor movement).  `filter_key`
                // returns false for Enter/Esc, which are handled above.
                _ => {
                    app.model_selector.filter_key(key);
                }
            }
            Ok(())
        }
        Event::Mouse(mouse) => {
            // The wheel and click handling are identical to the wizard's
            // provider picker: wheel scrolls the highlight (1 row per notch,
            // same pin-at-middle behavior as the arrows), a click on a
            // visible list row selects it (like Enter), a click on the filter
            // row positions the cursor, and clicks anywhere else (dimmed
            // area, outside the popup) are no-ops.
            match mouse.kind {
                MouseEventKind::ScrollDown => app.model_selector.move_down(),
                MouseEventKind::ScrollUp => app.model_selector.move_up(),
                MouseEventKind::Down(MouseButton::Left) => {
                    // Resolve the click against the RENDERED window start
                    // (`window()`, the same function the renderer draws with),
                    // never the stored `scroll`: a PgUp/PgDn jump moves
                    // `focused` without touching `scroll`, and `picker_window`
                    // then pushes the drawn window to keep the jumped focus
                    // visible — the raw value would map the click onto a
                    // different row than the one drawn.  The popup geometry is
                    // derived from the last known terminal size; before the
                    // first frame it is unknown, so every click is a no-op
                    // (`apply_selector_left_click` returns `None` then).
                    let filtered = app.model_selector.filtered();
                    let (start, _) = app
                        .model_selector
                        .window(&filtered, app.model_selector.viewport_height);
                    // Copy the length out first so the filtered borrow of
                    // `app.model_selector` (the `&str` refs into `all_models`)
                    // ends before the mutable borrow of the filter below.
                    let filtered_len = filtered.len();
                    if let Some(idx) = apply_selector_left_click(
                        app.last_terminal_size,
                        mouse.column,
                        mouse.row,
                        filtered_len,
                        start,
                        &mut app.model_selector.filter,
                    ) {
                        // The popup shows no list while loading or after a
                        // failed refresh (the error text replaces it), so a row
                        // click must not select a stale model from an earlier
                        // load — the clicked row was never drawn.
                        if app.model_selector.loading || app.model_selector.error.is_some() {
                            return Ok(());
                        }
                        // Click in the list body: move the highlight to
                        // the clicked row and select it, exactly like
                        // Enter.
                        app.model_selector.focused = idx;
                        if let Some(model) = app.model_selector.submit() {
                            tracing::info!(%model, "model selector: selecting model");
                            client_tx
                                .send(ClientMessage::SetModel { model })
                                .map_err(broken_pipe)?;
                        }
                    }
                }
                _ => {}
            }
            Ok(())
        }
        _ => Ok(()),
    }
}
