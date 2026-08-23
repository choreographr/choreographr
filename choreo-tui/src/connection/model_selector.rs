use crate::state::{App, SelectorClick, selector_click_target, selector_position_filter_cursor};
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
                    // never the stored `scroll`: a filter narrowing or a
                    // PgUp/PgDn jump can leave `scroll` stale (past the new
                    // max_scroll), and the raw value would map the click onto
                    // a different row than the one drawn.  The popup geometry
                    // is derived from the last known terminal size; before
                    // the first frame it is unknown, so every click is a
                    // no-op (`selector_click_target` returns `Noop` then).
                    let filtered = app.model_selector.filtered();
                    let (start, _) = app
                        .model_selector
                        .window(&filtered, app.model_selector.viewport_height);
                    match selector_click_target(
                        app.last_terminal_size,
                        mouse.column,
                        mouse.row,
                        filtered.len(),
                        start,
                    ) {
                        SelectorClick::Row(idx) => {
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
                        SelectorClick::FilterRow {
                            column,
                            filter_row_x,
                        } => {
                            // Click in the filter row: position the input
                            // cursor at the clicked column.
                            selector_position_filter_cursor(
                                &mut app.model_selector.filter,
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
