use crate::state::App;
use choreo_client_core::{ClientError, broken_pipe};
use choreo_proto::ClientMessage;
use crossterm::event::{Event, KeyCode, KeyEventKind};

/// Handle events while the model selector overlay is open (Chat page).
///
/// Up/Down move the highlight, Enter selects the highlighted model and
/// closes (sending `SetModel`), Esc dismisses without changing anything,
/// and every other key feeds the filter box.  Non-key events are ignored;
/// quit is handled via Ctrl+Q at the terminal-event level.
pub(super) fn handle_model_selector_event(
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
    // Esc dismisses the overlay without changing the model.
    if key.code == KeyCode::Esc {
        tracing::debug!("[choreo-tui] model selector dismissed");
        app.model_selector.close();
        return Ok(());
    }
    // Enter selects the highlighted model (if any) and closes.  An empty
    // filtered list (e.g. a filter with no matches) simply closes.
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
        // Everything else goes to the filter input (characters, backspace,
        // word deletes, cursor movement).  `filter_key` returns false for
        // Enter/Esc, which are handled above.
        _ => {
            app.model_selector.filter_key(key);
        }
    }
    Ok(())
}
