use crate::state::{
    App, selector_list_layout, selector_local_row, selector_position_filter_cursor,
};
use choreo_client_core::{ClientError, broken_pipe};
use choreo_proto::ClientMessage;
use crossterm::event::{Event, KeyCode, KeyEventKind, MouseButton, MouseEventKind};
use ratatui::layout::Rect;

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
                    // The popup geometry is derived from the last known
                    // terminal size; before the first frame it is unknown, so
                    // there is nothing to hit-test against.
                    if let Some((width, height)) = app.last_terminal_size {
                        let layout = selector_list_layout(Rect {
                            x: 0,
                            y: 0,
                            width,
                            height,
                        });
                        if let Some(local) = selector_local_row(&layout, mouse.column, mouse.row) {
                            // Click in the list body: move the highlight to
                            // the clicked row (scroll + local row, clamped to
                            // the filtered-list length) and select it, exactly
                            // like Enter.
                            let filtered_len = app.model_selector.filtered().len();
                            let idx = app.model_selector.scroll.saturating_add(local);
                            if idx < filtered_len {
                                app.model_selector.focused = idx;
                                if let Some(model) = app.model_selector.submit() {
                                    tracing::info!(%model, "model selector: selecting model");
                                    client_tx
                                        .send(ClientMessage::SetModel { model })
                                        .map_err(broken_pipe)?;
                                }
                            }
                        } else if mouse.row == layout.filter_row.y
                            && mouse.column >= layout.filter_row.x
                            && mouse.column < layout.filter_row.x + layout.filter_row.width
                        {
                            // Click in the filter row: position the input
                            // cursor at the clicked column.
                            selector_position_filter_cursor(
                                &mut app.model_selector.filter,
                                &layout,
                                mouse.column,
                            );
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
